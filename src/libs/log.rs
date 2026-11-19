//! Tracing-based logging setup with stdout and rolling file logging (`no_std` over `ekostd`).
//!
//! # OpenTelemetry
//! There is no OTLP export here any more. The exporter stack this crate used --
//! `opentelemetry-otlp` and the SDK providers behind it -- was the last thing in the
//! graph that pulled tokio in, and it did so for the protobuf *message types* rather
//! than for a runtime: `http-proto` requires `opentelemetry-proto/gen-tonic-messages`,
//! that feature is `["tonic", "tonic-prost", "prost"]`, and tonic 0.14 lists
//! `tokio-stream` as a non-optional dependency, which in turn lists tokio. No transport
//! or encoding choice on `opentelemetry-otlp` 0.31 avoids those types, so the only ways
//! out were to hand-roll an OTLP encoder or to drop the exporter. Nothing in the fleet
//! enabled the `otel` feature, so the exporter went.
//!
//! [`OtelConfig`] stays because several backends construct it inside a [`LoggingConfig`]
//! literal and deleting it would break their builds for no gain. It is inert: setting
//! `enabled: true` now warns at setup and forwards nothing.

use alloc::boxed::Box;
use alloc::collections::BTreeMap as HashMap;
#[cfg(feature = "log_throttling")]
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use chrono::SecondsFormat;
use eko::channel::{SyncSender, sync_channel};
use eko::path::PathBuf;
use eko::thread::JoinHandle;
use parking_lot::RwLock;
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber, level_filters::LevelFilter, span};

use crate::eyre::{self, DefaultHandler, EyreHandler, bail, eyre};

#[deprecated(
    since = "1.3.0",
    note = "This code is not used in current projects and should probably not be used going forward"
)]
pub mod legacy;

#[cfg(feature = "error_aggregation")]
pub mod error_aggregation;
pub mod level_filter;
#[cfg(feature = "log_throttling")]
pub mod throttling;

#[cfg(feature = "error_aggregation")]
pub use error_aggregation::*;
pub use level_filter::*;
#[cfg(feature = "log_throttling")]
pub use throttling::TracingRateLimitLayer;

const NON_BLOCKING_QUEUE_CAPACITY: usize = 4096;

/// Log file rotation schedule (`no_std` replacement for `tracing_appender::rolling::Rotation`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRotation {
    MINUTELY,
    HOURLY,
    DAILY,
    NEVER,
}

impl LogRotation {
    pub const fn minutely() -> Self {
        Self::MINUTELY
    }

    pub const fn hourly() -> Self {
        Self::HOURLY
    }

    pub const fn daily() -> Self {
        Self::DAILY
    }

    pub const fn never() -> Self {
        Self::NEVER
    }

    fn period_key(&self, epoch_secs: i64) -> Option<String> {
        let dt = chrono::DateTime::from_timestamp(epoch_secs, 0)?;
        match self {
            Self::MINUTELY => Some(dt.format("%Y-%m-%d-%H-%M").to_string()),
            Self::HOURLY => Some(dt.format("%Y-%m-%d-%H").to_string()),
            Self::DAILY => Some(dt.format("%Y-%m-%d").to_string()),
            Self::NEVER => None,
        }
    }
}

enum WorkerCommand {
    Write(String),
    Shutdown,
}

/// Guard keeping a background non-blocking log writer alive; flushes queued records on drop.
pub struct WorkerGuard {
    sender: Option<SyncSender<WorkerCommand>>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for WorkerGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerGuard").finish_non_exhaustive()
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(WorkerCommand::Shutdown);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
struct NonBlockingWriter {
    sender: SyncSender<WorkerCommand>,
}

impl NonBlockingWriter {
    fn send_line(&self, line: String) {
        let _ = self.sender.send(WorkerCommand::Write(line));
    }
}

fn spawn_non_blocking_stdout() -> (NonBlockingWriter, WorkerGuard) {
    let (tx, rx) = sync_channel::<WorkerCommand>(NON_BLOCKING_QUEUE_CAPACITY);
    let worker_tx = tx.clone();
    let join = eko::thread::Builder::new()
        .name("elibs-stdout-log")
        .spawn(move || {
            let mut stdout = eko::file::stdout();
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    WorkerCommand::Write(line) => {
                        let _ = stdout.write_all(line.as_bytes());
                    }
                    WorkerCommand::Shutdown => break,
                }
            }
            while let Some(WorkerCommand::Write(line)) = rx.try_recv() {
                let _ = stdout.write_all(line.as_bytes());
            }
            let _ = stdout.flush();
        })
        .ok();

    (
        NonBlockingWriter { sender: worker_tx },
        WorkerGuard {
            sender: Some(tx),
            join,
        },
    )
}

struct RollingFileWriter {
    directory: PathBuf,
    prefix: String,
    suffix: &'static str,
    rotation: LogRotation,
    current_period: Option<String>,
    file: eko::file::File,
}

impl RollingFileWriter {
    fn new(
        directory: PathBuf,
        prefix: String,
        suffix: &'static str,
        rotation: LogRotation,
    ) -> eyre::Result<Self> {
        eko::fs::create_dir_all(&directory)
            .map_err(|err| eyre!("failed to create log directory {}: {err}", directory.display()))?;
        let now_secs = eko::time::epoch_secs();
        let period = rotation.period_key(now_secs);
        let file_path = Self::compose_path(&directory, &prefix, period.as_deref(), suffix);
        let file = eko::file::File::append(&file_path)
            .map_err(|err| eyre!("failed to open log file {}: {err}", file_path.display()))?;
        Ok(Self {
            directory,
            prefix,
            suffix,
            rotation,
            current_period: period,
            file,
        })
    }

    fn compose_path(
        directory: &PathBuf,
        prefix: &str,
        period: Option<&str>,
        suffix: &str,
    ) -> PathBuf {
        let mut filename = String::new();
        if !prefix.is_empty() {
            filename.push_str(prefix);
        }
        if let Some(period) = period {
            if !filename.is_empty() {
                filename.push('.');
            }
            filename.push_str(period);
        }
        if !suffix.is_empty() {
            if !filename.is_empty() {
                filename.push('.');
            }
            filename.push_str(suffix);
        }
        directory.join(filename.as_str())
    }

    fn write_record(&mut self, line: &str) {
        if self.rotation != LogRotation::NEVER {
            let next_period = self.rotation.period_key(eko::time::epoch_secs());
            if next_period != self.current_period {
                let next_path = Self::compose_path(
                    &self.directory,
                    &self.prefix,
                    next_period.as_deref(),
                    self.suffix,
                );
                if let Ok(new_file) = eko::file::File::append(&next_path) {
                    self.file = new_file;
                    self.current_period = next_period;
                }
            }
        }
        let _ = self.file.write_all(line.as_bytes());
    }
}

fn spawn_non_blocking_file(mut appender: RollingFileWriter) -> (NonBlockingWriter, WorkerGuard) {
    let (tx, rx) = sync_channel::<WorkerCommand>(NON_BLOCKING_QUEUE_CAPACITY);
    let worker_tx = tx.clone();
    let join = eko::thread::Builder::new()
        .name("elibs-file-log")
        .spawn(move || {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    WorkerCommand::Write(line) => appender.write_record(&line),
                    WorkerCommand::Shutdown => break,
                }
            }
            while let Some(WorkerCommand::Write(line)) = rx.try_recv() {
                appender.write_record(&line);
            }
            let _ = appender.file.sync_all();
        })
        .ok();

    (
        NonBlockingWriter { sender: worker_tx },
        WorkerGuard {
            sender: Some(tx),
            join,
        },
    )
}

/// Configuration for OpenTelemetry integration (inert compatibility struct).
#[derive(Debug, Clone, Default)]
pub struct OtelConfig {
    /// Whether OTel log/trace forwarding was requested. Requesting it now only warns.
    pub enabled: bool,
    /// Service name that would identify this application to a collector.
    pub service_name: Option<String>,
    /// OTLP collector endpoint (e.g., "http://localhost:4318").
    pub endpoint: Option<String>,
    /// Additional headers that would be sent with OTLP requests (e.g., authentication).
    pub headers: HashMap<String, String>,
}

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub file_config: Option<FileLoggingConfig>,
    /// OpenTelemetry configuration. Accepted and ignored: OTLP export was removed, so
    /// `enabled: true` forwards nothing and logs a warning at setup. The field stays so
    /// existing `LoggingConfig` literals keep compiling. See [`OtelConfig`].
    pub otel_config: OtelConfig,
    #[cfg(feature = "error_aggregation")]
    pub error_aggregation: ErrorAggregationConfig,
    #[cfg(feature = "log_throttling")]
    pub throttling_config: Option<LogThrottlingConfig>,
}

#[derive(Debug, Clone)]
pub struct FileLoggingConfig {
    pub path: PathBuf,
    /// The name of the log file before the '.log', e.g. <file_prefix>.log will be the final log file.
    /// Set to None for the current timestamp
    pub file_prefix: Option<String>,
    // Specifying None means that there will be one log file per program execution
    pub rotation: Option<LogRotation>,
}

#[derive(Debug, Default, Clone)]
pub struct LogThrottlingConfig {
    /// How often to emit throttling summaries as WARN events, set to None to disable entirely
    pub summary_emission_interval: Option<Duration>,
    /// How often to throttling metrics as INFO events, set to None to disable entirely
    pub metrics_emission_interval: Option<Duration>,
    /// Fields to exclude from uniqueness checks, set to None to disable entirely.
    pub excluded_fields: Option<Vec<String>>,
    /// Targets to exempt from any throttling.
    pub exemptions: Option<Vec<String>>,
}

pub struct LogSetupReturn {
    pub reload_handle: LogReloadHandle,
    pub log_guards: (WorkerGuard, Option<WorkerGuard>),
    #[cfg(feature = "error_aggregation")]
    pub errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    /// Call shutdown on this during graceful shutdown
    pub log_throttling_handle: TracingRateLimitLayer,
}

/// Handle for reloading the global [`EnvFilter`] at runtime.
pub type GlobalLogReloadHandle = Arc<RwLock<EnvFilter>>;

/// Handle for reloading the global log level at runtime.
#[derive(Clone)]
pub struct LogReloadHandle(GlobalLogReloadHandle);

impl fmt::Debug for LogReloadHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogReloadHandle")
            .field("inner", &"Arc<RwLock<EnvFilter>>")
            .finish()
    }
}

impl LogReloadHandle {
    /// Sets a new global log level that applies to all sinks (stdout, file, etc.).
    pub fn set_log_level(&self, level: LogLevel) -> eyre::Result<()> {
        match build_env_filter(level) {
            Ok(filter) => {
                *self.0.write() = filter;
                tracing::callsite::rebuild_interest_cache();
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    "Error building new filter from given log level. Ignoring reload attempt"
                );
                bail!(
                    "Error building new filter from given log level: {error}. Ignoring reload attempt"
                )
            }
        }
    }
}

struct EventFieldFormatter {
    message: String,
    fields: String,
}

impl EventFieldFormatter {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
        }
    }
}

impl tracing::field::Visit for EventFieldFormatter {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={:?}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

fn format_timestamp_rfc3339() -> String {
    let micros = crate::libs::utils::get_time_micros();
    let secs = micros.div_euclid(1_000_000);
    let sub_micros = (micros.rem_euclid(1_000_000) as u32) * 1_000;
    if let Some(dt) = chrono::DateTime::from_timestamp(secs, sub_micros) {
        dt.to_rfc3339_opts(SecondsFormat::Micros, true)
    } else {
        String::from("1970-01-01T00:00:00.000000Z")
    }
}

fn format_event_line(event: &Event<'_>) -> String {
    let meta = event.metadata();
    let ts = format_timestamp_rfc3339();
    let level_str = match *meta.level() {
        Level::ERROR => "ERROR",
        Level::WARN => " WARN",
        Level::INFO => " INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    };
    let tid = eko::thread::current_id().as_u64();
    let target = meta.target();
    let line_num = meta.line().unwrap_or(0);

    let mut visitor = EventFieldFormatter::new();
    event.record(&mut visitor);

    let mut out = String::with_capacity(128 + visitor.message.len() + visitor.fields.len());
    let _ = write!(out, "{ts} {level_str} thread-{tid:x} {target}: {line_num}:");
    if !visitor.message.is_empty() {
        out.push(' ');
        out.push_str(&visitor.message);
    }
    if !visitor.fields.is_empty() {
        out.push(' ');
        out.push_str(&visitor.fields);
    }
    out.push('\n');
    out
}

struct FileSink {
    writer: NonBlockingWriter,
    #[cfg(feature = "log_throttling")]
    throttler: TracingRateLimitLayer,
}

struct EkoLoggingSubscriber {
    filter: GlobalLogReloadHandle,
    stdout: NonBlockingWriter,
    file: Option<FileSink>,
    #[cfg(feature = "error_aggregation")]
    error_layer: ErrorAggregationLayer,
    next_span_id: AtomicU64,
}

impl Subscriber for EkoLoggingSubscriber {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        // Runtime reload via `LogReloadHandle` can change target filtering at any time.
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.read().enabled(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(self.filter.read().max_level_hint())
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        if !self.enabled(event.metadata()) {
            return;
        }

        let formatted = format_event_line(event);
        self.stdout.send_line(formatted.clone());

        if let Some(file_sink) = &self.file {
            #[cfg(feature = "log_throttling")]
            {
                let (allowed, summaries) = file_sink.throttler.check_event(event);
                for summary in summaries {
                    let ts = format_timestamp_rfc3339();
                    let tid = eko::thread::current_id().as_u64();
                    file_sink.writer.send_line(format!(
                        "{ts}  WARN thread-{tid:x} tracing_throttle: 0: {summary}\n"
                    ));
                }
                if allowed {
                    file_sink.writer.send_line(formatted);
                }
            }
            #[cfg(not(feature = "log_throttling"))]
            {
                file_sink.writer.send_line(formatted);
            }
        }

        #[cfg(feature = "error_aggregation")]
        self.error_layer.record_event(event);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

struct LoggingSubscriberParts {
    subscriber: Arc<dyn Subscriber + Send + Sync + 'static>,
    reload_handle: LogReloadHandle,
    log_guards: (WorkerGuard, Option<WorkerGuard>),
    otel_warning_endpoint: Option<String>,
    #[cfg(feature = "error_aggregation")]
    errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    log_throttling_handle: TracingRateLimitLayer,
}

fn build_logging_subscriber(config: LoggingConfig) -> eyre::Result<LoggingSubscriberParts> {
    let global_env_filter = build_env_filter(config.level)?;
    let filter_lock = Arc::new(RwLock::new(global_env_filter));
    let reload_handle = LogReloadHandle(Arc::clone(&filter_lock));

    let (stdout_writer, stdout_guard) = spawn_non_blocking_stdout();

    #[cfg(feature = "error_aggregation")]
    let (errors_container, error_layer) = get_error_aggregation(config.error_aggregation.clone());

    #[cfg(feature = "log_throttling")]
    let log_throttling_handle = {
        let throttling_config = config.throttling_config.clone().unwrap_or_default();
        let mut exemptions = alloc::vec!["tracing_throttle::infrastructure::layer".to_string()];
        exemptions.extend(throttling_config.exemptions.unwrap_or_default());

        let layer = TracingRateLimitLayer::builder()
            .with_excluded_fields(throttling_config.excluded_fields.unwrap_or_default())
            .with_exempt_targets(exemptions)
            .with_active_emission(throttling_config.summary_emission_interval.is_some())
            .with_summary_interval(
                throttling_config
                    .summary_emission_interval
                    .unwrap_or(Duration::from_secs(5 * 60)),
            )
            .build()?;

        if let Some(metrics_duration) = throttling_config.metrics_emission_interval {
            let metrics = Arc::clone(layer.metrics());
            nagoya::runtime::background().spawn(async move {
                loop {
                    nagoya::sleep(metrics_duration).await;
                    let snapshot = metrics.snapshot();
                    tracing::info!(
                        events_allowed = snapshot.events_allowed,
                        events_suppressed = snapshot.events_suppressed,
                        suppression_rate = format!("{:.1}%", snapshot.suppression_rate() * 100.0),
                        "Rate limiting metrics"
                    );
                }
            });
        }

        layer
    };

    let (file_sink, file_guard) = match config.file_config {
        None => (None, None),
        Some(file_config) => {
            let default_prefix = || {
                let secs = eko::time::epoch_secs();
                chrono::DateTime::from_timestamp(secs, 0)
                    .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
                    .unwrap_or_else(|| secs.to_string())
            };
            let prefix = file_config.file_prefix.unwrap_or_else(default_prefix);
            let rotation = file_config.rotation.unwrap_or(LogRotation::NEVER);
            let appender = RollingFileWriter::new(file_config.path, prefix, "log", rotation)?;
            let (writer, guard) = spawn_non_blocking_file(appender);
            let sink = FileSink {
                writer,
                #[cfg(feature = "log_throttling")]
                throttler: log_throttling_handle.clone(),
            };
            (Some(sink), Some(guard))
        }
    };

    let otel_warning_endpoint = if config.otel_config.enabled {
        Some(
            config
                .otel_config
                .endpoint
                .unwrap_or_else(|| String::from("unset")),
        )
    } else {
        None
    };

    let subscriber: Arc<dyn Subscriber + Send + Sync + 'static> = Arc::new(EkoLoggingSubscriber {
        filter: filter_lock,
        stdout: stdout_writer,
        file: file_sink,
        #[cfg(feature = "error_aggregation")]
        error_layer,
        next_span_id: AtomicU64::new(1),
    });

    Ok(LoggingSubscriberParts {
        subscriber,
        reload_handle,
        log_guards: (stdout_guard, file_guard),
        otel_warning_endpoint,
        #[cfg(feature = "error_aggregation")]
        errors_container,
        #[cfg(feature = "log_throttling")]
        log_throttling_handle,
    })
}

struct SharedSubscriber(Arc<dyn Subscriber + Send + Sync + 'static>);

impl Subscriber for SharedSubscriber {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.0.register_callsite(metadata)
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0.enabled(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.0.max_level_hint()
    }

    fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
        self.0.new_span(span)
    }

    fn record(&self, span: &span::Id, values: &span::Record<'_>) {
        self.0.record(span, values);
    }

    fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {
        self.0.record_follows_from(span, follows);
    }

    fn event_enabled(&self, event: &Event<'_>) -> bool {
        self.0.event_enabled(event)
    }

    fn event(&self, event: &Event<'_>) {
        self.0.event(event);
    }

    fn enter(&self, span: &span::Id) {
        self.0.enter(span);
    }

    fn exit(&self, span: &span::Id) {
        self.0.exit(span);
    }

    fn clone_span(&self, id: &span::Id) -> span::Id {
        self.0.clone_span(id)
    }

    fn try_close(&self, id: span::Id) -> bool {
        self.0.try_close(id)
    }
}

/// Sets up a filtered `tracing` logging subscriber writing formatted lines to non-blocking
/// stdout and optional rolling log files, with live global level reloading via [`LogReloadHandle`].
pub fn setup_logging(config: LoggingConfig) -> eyre::Result<LogSetupReturn> {
    let parts = build_logging_subscriber(config)?;

    tracing::dispatcher::set_global_default(Dispatch::new(SharedSubscriber(Arc::clone(
        &parts.subscriber,
    ))))
    .map_err(|err| eyre!("failed to install global tracing subscriber: {err}"))?;
    tracing::callsite::rebuild_interest_cache();

    if let Some(endpoint) = parts.otel_warning_endpoint.as_deref() {
        tracing::warn!(
            target: "otel::setup",
            endpoint = endpoint,
            "otel_config.enabled is true but OTLP export has been removed from endpoint-libs - \
             no traces or logs will be forwarded to a collector"
        );
    }

    Ok(LogSetupReturn {
        reload_handle: parts.reload_handle,
        log_guards: parts.log_guards,
        #[cfg(feature = "error_aggregation")]
        errors_container: parts.errors_container,
        #[cfg(feature = "log_throttling")]
        log_throttling_handle: parts.log_throttling_handle,
    })
}

/// Used to wrap the default `EyreHandler` and expose [`CustomEyreHandler::location`] so
/// `internal_error_to_resp` can forward caller location to error aggregation.
pub struct CustomEyreHandler {
    default_handler: Box<dyn EyreHandler>,
    location: Option<&'static core::panic::Location<'static>>,
}

impl CustomEyreHandler {
    pub fn default_with_location_saving(
        error: &(dyn core::error::Error + 'static),
    ) -> Box<dyn EyreHandler> {
        Box::new(Self {
            default_handler: DefaultHandler::default_with(error),
            location: None,
        })
    }

    pub fn get_location(&self) -> &Option<&'static core::panic::Location<'static>> {
        &self.location
    }
}

impl EyreHandler for CustomEyreHandler {
    fn display(
        &self,
        error: &(dyn core::error::Error + 'static),
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.default_handler.display(error, f)
    }

    fn debug(
        &self,
        error: &(dyn core::error::Error + 'static),
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.default_handler.debug(error, f)
    }

    fn track_caller(&mut self, location: &'static core::panic::Location<'static>) {
        self.location = Some(location);
        self.default_handler.track_caller(location);
    }
}

// --- Test-only thread-local subscriber routing over pure no_std `tracing` ---

#[cfg(test)]
struct TestMuxSubscriber {
    fallback: RwLock<Option<Arc<dyn Subscriber + Send + Sync + 'static>>>,
}

#[cfg(test)]
eko::thread_local! {
    static THREAD_TEST_SUBSCRIBER: core::cell::RefCell<Option<Arc<dyn Subscriber + Send + Sync + 'static>>> =
        core::cell::RefCell::new(None);
}

#[cfg(test)]
static TEST_MUX: eko::thread::OnceLock<&'static TestMuxSubscriber> = eko::thread::OnceLock::new();

#[cfg(test)]
fn ensure_test_mux() -> &'static TestMuxSubscriber {
    *TEST_MUX.get_or_init(|| {
        let mux = Box::leak(Box::new(TestMuxSubscriber {
            fallback: RwLock::new(None),
        }));
        struct MuxProxy(&'static TestMuxSubscriber);
        impl Subscriber for MuxProxy {
            fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
                Interest::sometimes()
            }
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                self.0.with_active(|sub| sub.enabled(metadata)).unwrap_or(false)
            }
            fn max_level_hint(&self) -> Option<LevelFilter> {
                Some(LevelFilter::TRACE)
            }
            fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
                self.0
                    .with_active(|sub| sub.new_span(span))
                    .unwrap_or_else(|| span::Id::from_u64(1))
            }
            fn record(&self, span: &span::Id, values: &span::Record<'_>) {
                let _ = self.0.with_active(|sub| sub.record(span, values));
            }
            fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {
                let _ = self.0.with_active(|sub| sub.record_follows_from(span, follows));
            }
            fn event(&self, event: &Event<'_>) {
                let _ = self.0.with_active(|sub| sub.event(event));
            }
            fn enter(&self, span: &span::Id) {
                let _ = self.0.with_active(|sub| sub.enter(span));
            }
            fn exit(&self, span: &span::Id) {
                let _ = self.0.with_active(|sub| sub.exit(span));
            }
        }
        let _ = tracing::dispatcher::set_global_default(Dispatch::new(MuxProxy(mux)));
        tracing::callsite::rebuild_interest_cache();
        mux
    })
}

#[cfg(test)]
impl TestMuxSubscriber {
    fn with_active<R>(
        &self,
        f: impl FnOnce(&dyn Subscriber) -> R,
    ) -> Option<R> {
        let tls_sub = THREAD_TEST_SUBSCRIBER.with_borrow(|slot| slot.clone());
        if let Some(sub) = tls_sub {
            return Some(f(&*sub));
        }
        let guard = self.fallback.read();
        guard.as_ref().map(|sub| f(&**sub))
    }
}

#[cfg(test)]
pub struct DefaultSubscriberGuard {
    prev: Option<Arc<dyn Subscriber + Send + Sync + 'static>>,
}

#[cfg(test)]
impl Drop for DefaultSubscriberGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        THREAD_TEST_SUBSCRIBER.set(prev);
    }
}

#[cfg(test)]
pub(crate) fn set_test_subscriber(
    subscriber: Arc<dyn Subscriber + Send + Sync + 'static>,
) -> DefaultSubscriberGuard {
    let _ = ensure_test_mux();
    let prev = THREAD_TEST_SUBSCRIBER.replace(Some(subscriber));
    tracing::callsite::rebuild_interest_cache();
    DefaultSubscriberGuard { prev }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn with_test_subscriber<R>(
    subscriber: Arc<dyn Subscriber + Send + Sync + 'static>,
    f: impl FnOnce() -> R,
) -> R {
    let _guard = set_test_subscriber(subscriber);
    f()
}

#[cfg(test)]
pub struct LogSetupReturnTest {
    _guard: DefaultSubscriberGuard,
    pub reload_handle: LogReloadHandle,
    #[allow(dead_code)]
    log_guards: (WorkerGuard, Option<WorkerGuard>),
    #[cfg(feature = "error_aggregation")]
    #[allow(dead_code)]
    pub errors_container: Arc<ErrorAggregationContainer>,
}

/// Test-specific logging setup using thread-local dispatch so parallel tests stay isolated.
#[cfg(test)]
pub fn setup_logging_test(config: LoggingConfig) -> eyre::Result<LogSetupReturnTest> {
    let parts = build_logging_subscriber(config)?;
    let guard = set_test_subscriber(parts.subscriber);

    if let Some(endpoint) = parts.otel_warning_endpoint.as_deref() {
        tracing::warn!(
            target: "otel::setup",
            endpoint = endpoint,
            "otel_config.enabled is true but OTLP export has been removed from endpoint-libs - \
             no traces or logs will be forwarded to a collector"
        );
    }

    Ok(LogSetupReturnTest {
        _guard: guard,
        reload_handle: parts.reload_handle,
        log_guards: parts.log_guards,
        #[cfg(feature = "error_aggregation")]
        errors_container: parts.errors_container,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tracing::{debug, error, info};

    #[cfg(feature = "error_aggregation")]
    fn default_error_aggregation_config() -> ErrorAggregationConfig {
        ErrorAggregationConfig {
            limit: 100,
            normalize: true,
        }
    }

    #[test]
    fn test_basic_logging_stdout() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: OtelConfig::default(),
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: Some(LogThrottlingConfig::default()),
        };

        let _guard = setup_logging_test(config);
        assert!(_guard.is_ok(), "Failed to setup logging");

        info!("Test info message");
        error!("Test error message");
    }

    #[test]
    fn test_file_logging_comprehensive() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: eko::compat::path::PathBuf::from(temp_dir.path().to_str().unwrap()),
                file_prefix: Some("test".to_string()),
                rotation: None,
            }),
            otel_config: OtelConfig::default(),
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: Some(LogThrottlingConfig::default()),
        };

        let log_setup = setup_logging_test(config).unwrap();

        info!("info_message_before_reload");
        debug!("debug_message_before_reload");
        error!("error_message");

        eko::sleep_ms(100);

        let result = log_setup.reload_handle.set_log_level(LogLevel::Debug);
        assert!(result.is_ok());

        info!("info_message_after_reload");
        debug!("debug_message_after_reload");

        eko::sleep_ms(100);
        drop(log_setup);

        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(log_files.len(), 1, "Expected exactly one log file");

        let log_file = log_files[0].as_ref().unwrap();
        let log_contents = fs::read_to_string(log_file.path()).unwrap();

        assert!(log_contents.contains("info_message_before_reload"));
        assert!(
            !log_contents.contains("debug_message_before_reload"),
            "Debug should not appear before reload"
        );
        assert!(log_contents.contains("error_message"));
        assert!(log_contents.contains("info_message_after_reload"));
        assert!(
            log_contents.contains("debug_message_after_reload"),
            "Debug should appear after reload to DEBUG level"
        );
    }

    #[test]
    fn enabling_otel_is_accepted_and_changes_nothing() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: OtelConfig {
                enabled: true,
                endpoint: Some("http://nonexistent.invalid:4317".to_string()),
                ..OtelConfig::default()
            },
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        let result = setup_logging_test(config);
        assert!(
            result.is_ok(),
            "an otel_config asking for export must not fail setup"
        );
    }

    #[test]
    fn a_default_otel_config_sets_up_cleanly() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: OtelConfig::default(),
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        assert!(setup_logging_test(config).is_ok());
    }

    #[test]
    fn test_file_logging_with_otel_attempted() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: eko::compat::path::PathBuf::from(temp_dir.path().to_str().unwrap()),
                file_prefix: Some("otel_test".to_string()),
                rotation: None,
            }),
            otel_config: OtelConfig {
                enabled: true,
                endpoint: Some("http://localhost:4317".to_string()),
                ..OtelConfig::default()
            },
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        let log_setup = setup_logging_test(config).unwrap();

        info!("file_log_with_otel_attempted");
        error!("error_log_with_otel_attempted");

        eko::sleep_ms(100);
        drop(log_setup);

        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(log_files.len(), 1, "Expected exactly one log file");

        let log_contents = fs::read_to_string(log_files[0].as_ref().unwrap().path()).unwrap();
        assert!(log_contents.contains("file_log_with_otel_attempted"));
        assert!(log_contents.contains("error_log_with_otel_attempted"));
    }

    #[test]
    fn test_log_level_reload_with_otel_enabled() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: eko::compat::path::PathBuf::from(temp_dir.path().to_str().unwrap()),
                file_prefix: Some("reload_test".to_string()),
                rotation: None,
            }),
            otel_config: OtelConfig {
                enabled: true,
                endpoint: Some("http://localhost:4317".to_string()),
                ..OtelConfig::default()
            },
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        let log_setup = setup_logging_test(config).unwrap();

        info!("info_before_reload");
        debug!("debug_before_reload");

        eko::sleep_ms(100);

        let result = log_setup.reload_handle.set_log_level(LogLevel::Debug);
        assert!(
            result.is_ok(),
            "Log level reload should succeed with OTel enabled"
        );

        info!("info_after_reload");
        debug!("debug_after_reload");

        eko::sleep_ms(100);
        drop(log_setup);

        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        let log_contents = fs::read_to_string(log_files[0].as_ref().unwrap().path()).unwrap();

        assert!(log_contents.contains("info_before_reload"));
        assert!(
            !log_contents.contains("debug_before_reload"),
            "Debug should not appear before reload"
        );
        assert!(log_contents.contains("info_after_reload"));
        assert!(
            log_contents.contains("debug_after_reload"),
            "Debug should appear after reload"
        );
    }

    #[test]
    fn test_otel_config_fields() {
        let config = OtelConfig {
            enabled: true,
            service_name: Some("test-service".to_string()),
            endpoint: Some("http://collector:4317".to_string()),
            headers: {
                let mut h = HashMap::new();
                h.insert("x-api-key".to_string(), "secret-key".to_string());
                h
            },
        };

        assert!(config.enabled);
        assert_eq!(config.service_name, Some("test-service".to_string()));
        assert_eq!(config.endpoint, Some("http://collector:4317".to_string()));
        assert_eq!(
            config.headers.get("x-api-key"),
            Some(&"secret-key".to_string())
        );

        let default_config = OtelConfig::default();
        assert!(!default_config.enabled);
        assert!(default_config.service_name.is_none());
        assert!(default_config.endpoint.is_none());
        assert!(default_config.headers.is_empty());
    }
}

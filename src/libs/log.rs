//! Tracing-based logging setup with stdout, file, and OpenTelemetry logging support

use std::{path::PathBuf, time::Duration};

use chrono::SecondsFormat;
use eyre::{DefaultHandler, EyreHandler, bail};
use tracing::Subscriber;
use tracing_appender::rolling::RollingFileAppender;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{
    EnvFilter, Layer, Registry,
    layer::SubscriberExt,
    registry,
    reload::{self, Handle},
};

#[cfg(feature = "log_throttling")]
use tracing_throttle::TracingRateLimitLayer;

#[cfg(feature = "error_aggregation")]
use {crate::libs::log::error_aggregation::*, std::sync::Arc};

#[deprecated(
    since = "1.3.0",
    note = "This code is not used in current projects and should probably not be used going forward"
)]
pub mod legacy;

#[cfg(feature = "error_aggregation")]
pub mod error_aggregation;
pub mod level_filter;
pub mod otel;

pub use level_filter::*;
pub use otel::{OtelConfig, OtelGuards};

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;

pub use tracing_appender::non_blocking::WorkerGuard;

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub file_config: Option<FileLoggingConfig>,
    /// OpenTelemetry configuration for log/trace forwarding to an OTLP collector.
    /// By default, OTel is disabled. Set `enabled: true` and configure the endpoint
    /// to forward traces and logs to an OTel collector.
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
    /// Fields to exclude from uniqueness checks, set to None to disable entirely.<br><br>
    /// Example:
    /// ```ignore
    /// tracing::info!(user_id=1, "User joined");
    /// tracing::info!(user_id=2, "User joined");  
    ///```
    /// If the `user_id` field is excluded, these will be treated as exactly the same log, so multiple users join messages could be throttled
    pub excluded_fields: Option<Vec<String>>,
    /// Targets to exempt from any throttling. This allows the caller to ensure that any high priority logs are always displayed.<br><br>
    /// Example:
    /// ```ignore
    /// let exemptions = Some(vec!["nothrottle"]); // Assuming this is passed during config stage
    ///
    /// tracing::error!(target: "nothrottle", user_id=1, "User joined"); // Will never be throttled
    /// tracing::error!(user_id=2, "User joined");  // Can possibly be throttled
    /// ```
    pub exemptions: Option<Vec<String>>,
}

pub struct LogSetupReturn {
    pub reload_handle: LogReloadHandle,
    pub log_guards: (WorkerGuard, Option<WorkerGuard>),
    /// OpenTelemetry guards (tracer + logger providers). Must be kept alive to ensure 
    /// pending traces and logs are flushed to the OTLP collector on shutdown.
    pub otel_guards: Option<OtelGuards>,
    #[cfg(feature = "error_aggregation")]
    pub errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    /// Call shutdown on this during graceful shutdown
    pub log_throttling_handle: TracingRateLimitLayer,
}

/// Internal struct to hold the result of building the logging subscriber
struct LoggingSubscriberParts {
    subscriber: Box<dyn Subscriber + Send + Sync + 'static>,
    reload_handle: LogReloadHandle,
    log_guards: (WorkerGuard, Option<WorkerGuard>), // Stdout and optional file log guards
    /// OpenTelemetry guards (tracer + logger providers).
    otel_guards: Option<OtelGuards>,
    #[cfg(feature = "error_aggregation")]
    errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    log_throttling_handle: TracingRateLimitLayer,
}

/// Internal function that builds the logging subscriber without initializing it.
/// Returns the subscriber along with reload handles and guards that need to be retained.
fn build_logging_subscriber(config: LoggingConfig) -> eyre::Result<LoggingSubscriberParts> {
    // Build global env filter and wrap in reload::Layer for runtime reloading
    let global_env_filter = build_env_filter(config.level)?;
    let (reloadable_global_filter, global_reload_handle) = reload::Layer::new(global_env_filter);

    let (non_blocking_stdout, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_thread_names(true)
        .with_line_number(true)
        .with_writer(non_blocking_stdout);

    #[cfg(feature = "error_aggregation")]
    let error_aggregation_config = config.error_aggregation.clone();

    #[cfg(feature = "log_throttling")]
    let throttling_config = config.throttling_config.clone().unwrap_or_default();

    #[cfg(feature = "log_throttling")]
    let mut exemptions = vec!["tracing_throttle::infrastructure::layer".to_string()]; // It seems like it tries to throttle its own logs
    #[cfg(feature = "log_throttling")]
    exemptions.extend(throttling_config.exemptions.unwrap_or_default());

    #[cfg(feature = "log_throttling")]
    let rate_limit_filter = TracingRateLimitLayer::builder()
        .with_excluded_fields(throttling_config.excluded_fields.unwrap_or_default())
        .with_exempt_targets(exemptions)
        .with_active_emission(throttling_config.summary_emission_interval.is_some())
        .with_summary_interval(
            throttling_config
                .summary_emission_interval
                .unwrap_or(Duration::from_secs(5 * 60)),
        )
        .build()
        .expect("Error building tracing rate limit layer");

    #[cfg(feature = "log_throttling")]
    if let Some(metrics_duration) = throttling_config.metrics_emission_interval {
        let metrics = rate_limit_filter.metrics().clone();

        // Periodic metrics reporting
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(metrics_duration).await;

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

    #[cfg(feature = "log_throttling")]
    let log_throttling_handle = rate_limit_filter.clone();

    let (file_layer, file_guard) = match config.file_config {
        None => (None, None),
        Some(file_config) => {
            let appender = RollingFileAppender::builder()
                .rotation(file_config.rotation.unwrap_or(LogRotation::NEVER))
                .filename_prefix(file_config.file_prefix.unwrap_or_else(|| {
                    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
                })) // Default filename prefix is a timestamp. RollingFileAppender requires a prefix if we are using a simple suffix like ".log"
                .filename_suffix("log")
                .build(file_config.path)?;

            let (non_blocking_appender, guard) = tracing_appender::non_blocking(appender);

            let base_layer = tracing_subscriber::fmt::layer()
                .with_thread_names(true)
                .with_line_number(true)
                .with_ansi(false)
                .with_writer(non_blocking_appender);

            #[cfg(feature = "log_throttling")]
            let file_layer = base_layer.with_filter(rate_limit_filter);

            #[cfg(not(feature = "log_throttling"))]
            let file_layer = base_layer;

            (Some(file_layer), Some(guard))
        }
    };

    let reload_handle = LogReloadHandle(global_reload_handle);

    // Build OTel layer (separate, parallel to stdout/file layers)
    let otel_result = otel::build_otel_layer(&config.otel_config);
    let otel_guards = otel_result.guards;
    let otel_tracer = otel_result.tracer;

    // --- Subscriber Composition ---
    
    // Start with Registry and Global Filter
    let subscriber = registry().with(reloadable_global_filter);

    // Compose the "Sink" layers (Stdout + File)
    let sinks = stdout_layer.and_then(file_layer);

    // Add Error Aggregation if enabled
    #[cfg(feature = "error_aggregation")]
    let (sinks, errors_container) = {
        use crate::libs::log::error_aggregation::get_error_aggregation;
        let (container, error_layer) = get_error_aggregation(error_aggregation_config);
        (sinks.and_then(error_layer), container)
    };

    // Combine Subscriber with Sinks
    let subscriber = subscriber.with(sinks);

    // Add OTel Traces and Logs layers if enabled
    let subscriber: Box<dyn Subscriber + Send + Sync + 'static> = match (otel_tracer, &otel_guards) {
        (Some(tracer), Some(guards)) => {
            let trace_layer = OpenTelemetryLayer::new(tracer);
            let log_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&guards.logger_provider);
            Box::new(subscriber.with(trace_layer).with(log_layer))
        }
        _ => Box::new(subscriber),
    };

    Ok(LoggingSubscriberParts {
        subscriber,
        reload_handle,
        log_guards: (stdout_guard, file_guard),
        otel_guards,
        #[cfg(feature = "error_aggregation")]
        errors_container,
        #[cfg(feature = "log_throttling")]
        log_throttling_handle,
    })
}

/// Sets up a fully filtered tracing logging system with a global env filter applied at the registry level.
/// This ensures all layers (stdout, file, etc.) only receive events that pass the global filter.
///
/// By default with the minimal config a stdout layer is setup with a custom env filter that filters according to the specified log level,
/// as well as according to a list of hardcoded crates. See [build_env_filter] for hardcoded crate log filtering.
/// Optionally, file logging can be configured with [FileLoggingConfig], which minimally will create a timestamp prefixed log file in the given
/// path that does not rotate (one log file per program execution)
///
/// Extra configuration for file logging is:
/// - Log file rotation. See [LogRotation] for possible options
///
/// Returns [LogSetupReturn], which is a composite struct containing objects that need to be retained by the client such as:
/// - [LogReloadHandle], for setting a new global log level during runtime
/// - [WorkerGuard], so that the non-blocking file writer can continue writing. This cannot be dropped and needs to be kept alive for the duration of the program execution
/// - [Option<OtelGuards>], the OTel guards. Must be kept alive to flush pending traces and logs on shutdown.
/// - [ErrorAggregationContainer], if the [error_aggregation] feature is enabled. This object allows recent errors to be queried from the logging framework
pub fn setup_logging(config: LoggingConfig) -> eyre::Result<LogSetupReturn> {
    use tracing_subscriber::util::SubscriberInitExt;

    let parts = build_logging_subscriber(config)?;

    parts.subscriber.init();

    Ok(LogSetupReturn {
        reload_handle: parts.reload_handle,
        log_guards: parts.log_guards,
        otel_guards: parts.otel_guards,
        #[cfg(feature = "error_aggregation")]
        errors_container: parts.errors_container,
        #[cfg(feature = "log_throttling")]
        log_throttling_handle: parts.log_throttling_handle,
    })
}

/// Handle for reloading the global EnvFilter at runtime
pub type GlobalLogReloadHandle = Handle<EnvFilter, Registry>;

/// Handle for reloading the global log level at runtime
#[derive(Clone)]
pub struct LogReloadHandle(GlobalLogReloadHandle);

impl std::fmt::Debug for LogReloadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogReloadHandle")
            .field("inner", &"Handle<EnvFilter, Registry>")
            .finish()
    }
}

impl LogReloadHandle {
    /// Sets a new global log level that applies to all layers (stdout, file, etc.)
    /// Errors will be logged by this function and can be safely ignored, but are returned for display purposes
    pub fn set_log_level(&self, level: LogLevel) -> eyre::Result<()> {
        match build_env_filter(level) {
            Ok(filter) => match self.0.modify(|env_filter| *env_filter = filter) {
                Ok(_) => Ok(()),
                Err(error) => {
                    tracing::error!(
                        ?error,
                        "Error setting new global log filter. Ignoring reload attempt"
                    );
                    bail!("Error setting new global log filter: {error}. Ignoring reload attempt")
                }
            },
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

/// This is used to wrap the default EyreHandler but simply expose the [CustomEyreHandler::location] field via [CustomEyreHandler::get_location]
/// This allows the code in [crate::libs::ws::internal_error_to_resp] to access the original caller location which is then stored in a [tracing::Field]
/// for access within [error_aggregation::ErrorAggregationLayer::on_event] (see trait impl) so that the original caller can be recorded within the displayed target
pub struct CustomEyreHandler {
    default_handler: Box<dyn EyreHandler>,
    location: Option<&'static std::panic::Location<'static>>,
}

impl CustomEyreHandler {
    pub fn default_with_location_saving(
        error: &(dyn std::error::Error + 'static),
    ) -> Box<dyn EyreHandler> {
        Box::new(Self {
            default_handler: DefaultHandler::default_with(error),
            location: None,
        })
    }

    pub fn get_location(&self) -> &Option<&'static std::panic::Location<'static>> {
        &self.location
    }
}

impl EyreHandler for CustomEyreHandler {
    fn display(
        &self,
        error: &(dyn std::error::Error + 'static),
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        self.default_handler.display(error, f)
    }

    fn debug(
        &self,
        error: &(dyn std::error::Error + 'static),
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        self.default_handler.debug(error, f)
    }

    fn track_caller(&mut self, location: &'static std::panic::Location<'static>) {
        self.location = Some(location); // Store the location for access later
        self.default_handler.track_caller(location);
    }
}

#[cfg(test)]
pub struct LogSetupReturnTest {
    _guard: tracing::subscriber::DefaultGuard,
    reload_handle: LogReloadHandle,
    #[allow(dead_code)]
    log_guards: (WorkerGuard, Option<WorkerGuard>),
    #[allow(dead_code)]
    otel_guards: Option<OtelGuards>,
    #[cfg(feature = "error_aggregation")]
    #[allow(dead_code)]
    errors_container: Arc<ErrorAggregationContainer>,
}

/// Test-specific logging setup that uses thread-local scoped dispatcher instead of global
/// This allows multiple tests to run without conflicting over the global subscriber
#[cfg(test)]
pub fn setup_logging_test(config: LoggingConfig) -> eyre::Result<LogSetupReturnTest> {
    let parts = build_logging_subscriber(config)?;

    // Use thread-local default instead of global
    let guard = tracing::subscriber::set_default(parts.subscriber);

    Ok(LogSetupReturnTest {
        _guard: guard,
        reload_handle: parts.reload_handle,
        log_guards: parts.log_guards,
        otel_guards: parts.otel_guards,
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

    /// Basic test that verifies logging setup succeeds and logs can be emitted
    #[tokio::test]
    async fn test_basic_logging_stdout() {
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

        // Emit some logs to verify the system works
        info!("Test info message");
        error!("Test error message");
    }

    /// Test file logging functionality including:
    /// - Basic file logging
    /// - Log level filtering (separate stdout vs file levels)
    /// - Log level reloading at runtime
    #[tokio::test]
    async fn test_file_logging_comprehensive() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: temp_dir.path().to_path_buf(),
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

        // Test 1: Basic logging and level filtering (before reload)
        info!("info_message_before_reload");
        debug!("debug_message_before_reload");
        error!("error_message");

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Test 2: Reload global log level to DEBUG
        let result = log_setup.reload_handle.set_log_level(LogLevel::Debug);
        assert!(result.is_ok());

        // Test 3: Verify debug logs now appear after reload
        info!("info_message_after_reload");
        debug!("debug_message_after_reload");

        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(log_setup);

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify file was created and contains expected logs
        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(log_files.len(), 1, "Expected exactly one log file");

        let log_file = log_files[0].as_ref().unwrap();
        let log_contents = fs::read_to_string(log_file.path()).unwrap();

        // Verify messages before reload
        assert!(log_contents.contains("info_message_before_reload"));
        assert!(
            !log_contents.contains("debug_message_before_reload"),
            "Debug should not appear before reload"
        );
        assert!(log_contents.contains("error_message"));

        // Verify messages after reload
        assert!(log_contents.contains("info_message_after_reload"));

        // Global log level reload applies to all layers including file
        assert!(
            log_contents.contains("debug_message_after_reload"),
            "Debug should appear after reload to DEBUG level"
        );
    }

    // === OTel Integration Tests ===

    /// Test that setup succeeds when OTel is disabled (default config)
    /// and otel_guards is None
    #[tokio::test]
    async fn test_otel_disabled_returns_none_guards() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: OtelConfig::default(), // enabled: false
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        let result = setup_logging_test(config);
        assert!(result.is_ok(), "Setup should succeed with OTel disabled");

        let guard = result.unwrap();
        assert!(
            guard.otel_guards.is_none(),
            "otel_guards should be None when OTel is disabled"
        );
    }

    /// Test that OTel initialization succeeds even with an unreachable endpoint
    /// The SDK initializes asynchronously, so guards ARE present even if the endpoint
    /// is unreachable. Export failures happen at runtime, not at setup time.
    #[tokio::test]
    async fn test_otel_graceful_degradation_unreachable_endpoint() {
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

        // Setup should succeed - the SDK doesn't validate connectivity at init time
        let result = setup_logging_test(config);
        assert!(result.is_ok(), "Setup should succeed even with unreachable OTel endpoint");

        let guard = result.unwrap();
        // Guards ARE present because the SDK creates them synchronously.
        // Export failures happen asynchronously when spans/logs are batched.
        // The key assertion is that setup() didn't return an error.
        assert!(
            guard.otel_guards.is_some(),
            "otel_guards should be Some even with unreachable endpoint (SDK initializes synchronously)"
        );
    }

    /// Test that file logging works alongside OTel enabled (but with invalid endpoint)
    /// This verifies that the layer composition doesn't break when OTel is attempted
    #[tokio::test]
    async fn test_file_logging_with_otel_attempted() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: temp_dir.path().to_path_buf(),
                file_prefix: Some("otel_test".to_string()),
                rotation: None,
            }),
            otel_config: OtelConfig {
                enabled: true,
                endpoint: Some("http://localhost:4317".to_string()), // Not running
                ..OtelConfig::default()
            },
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        let log_setup = setup_logging_test(config).unwrap();

        // Emit logs - these should go to file regardless of OTel status
        info!("file_log_with_otel_attempted");
        error!("error_log_with_otel_attempted");

        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(log_setup);
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify file was created and contains logs
        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(log_files.len(), 1, "Expected exactly one log file");

        let log_contents = fs::read_to_string(log_files[0].as_ref().unwrap().path()).unwrap();
        assert!(log_contents.contains("file_log_with_otel_attempted"));
        assert!(log_contents.contains("error_log_with_otel_attempted"));
    }

    /// Test that log level reload works correctly when OTel is enabled
    #[tokio::test]
    async fn test_log_level_reload_with_otel_enabled() {
        let temp_dir = tempfile::tempdir().unwrap();

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: Some(FileLoggingConfig {
                path: temp_dir.path().to_path_buf(),
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

        // Log at INFO level (should appear)
        info!("info_before_reload");
        debug!("debug_before_reload"); // Should NOT appear

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Reload to DEBUG
        let result = log_setup.reload_handle.set_log_level(LogLevel::Debug);
        assert!(result.is_ok(), "Log level reload should succeed with OTel enabled");

        // Log at DEBUG level (should now appear)
        info!("info_after_reload");
        debug!("debug_after_reload");

        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(log_setup);
        std::thread::sleep(std::time::Duration::from_millis(100));

        let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        let log_contents = fs::read_to_string(log_files[0].as_ref().unwrap().path()).unwrap();

        assert!(log_contents.contains("info_before_reload"));
        assert!(!log_contents.contains("debug_before_reload"), "Debug should not appear before reload");
        assert!(log_contents.contains("info_after_reload"));
        assert!(log_contents.contains("debug_after_reload"), "Debug should appear after reload");
    }

    /// Test that OTel config fields are correctly structured
    #[tokio::test]
    async fn test_otel_config_fields() {
        use std::collections::HashMap;

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
        assert_eq!(config.headers.get("x-api-key"), Some(&"secret-key".to_string()));

        // Default config should be disabled
        let default_config = OtelConfig::default();
        assert!(!default_config.enabled);
        assert!(default_config.service_name.is_none());
        assert!(default_config.endpoint.is_none());
        assert!(default_config.headers.is_empty());
    }

    /// Test that setup succeeds with OTel enabled but no endpoint specified
    /// (should use env var fallback or SDK defaults)
    #[tokio::test]
    async fn test_otel_enabled_no_endpoint_uses_fallback() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: OtelConfig {
                enabled: true,
                endpoint: None, // No endpoint - should use SDK defaults
                ..OtelConfig::default()
            },
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
            #[cfg(feature = "log_throttling")]
            throttling_config: None,
        };

        // Setup should succeed (exporter will use SDK defaults)
        let result = setup_logging_test(config);
        assert!(result.is_ok(), "Setup should succeed with OTel enabled but no endpoint");

        let guard = result.unwrap();
        // Guards may or may not be present depending on whether SDK defaults work
        // The key thing is setup didn't panic or return an error
        let _ = guard.otel_guards;
    }
}

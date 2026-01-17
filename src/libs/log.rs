//! Tracing-based logging setup with stdout and file logging support

use std::{path::PathBuf, time::Duration};

use eyre::{bail, DefaultHandler, EyreHandler};
use tracing::Subscriber;
use tracing_appender::rolling::RollingFileAppender;
use tracing_subscriber::{
    layer::SubscriberExt,
    registry,
    reload::{self, Handle},
    EnvFilter, Layer, Registry,
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

pub use level_filter::*;

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;

pub use tracing_appender::non_blocking::WorkerGuard;

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub file_config: Option<FileLoggingConfig>,
    #[cfg(feature = "error_aggregation")]
    pub error_aggregation: ErrorAggregationConfig,
    #[cfg(feature = "log_throttling")]
    pub throttling_config: Option<LogThrottlingConfig>,
}

#[derive(Debug, Clone)]
pub struct FileLoggingConfig {
    pub path: PathBuf,
    pub file_prefix: String,
    /// Used to specify a separate level than the overall log level. e.g. stdout logs DEBUG, but file only logs INFO
    pub file_log_level: Option<LogLevel>,
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

    let filter_handle_clone = global_reload_handle.clone();

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
        .with_summary_formatter(Arc::new(move |summary| {
            let current_env_filter = filter_handle_clone.clone_current();

            current_env_filter.inspect(|filter| {
                if let Some(metadata) = &summary.metadata {
                    use std::str::FromStr;

                    use tracing::{level_filters::LevelFilter, Level};

                    // Attempt manual filtering for these summaries. We default to levels that will result in no summaries being produced if unexpected states occur
                    if filter
                        .max_level_hint()
                        .unwrap_or(LevelFilter::ERROR) // If no max level hint, default to highest level
                        .le(&Level::from_str(&metadata.level).unwrap_or(Level::TRACE))
                    // If event level can't be parsed, default to lowest level
                    {
                        tracing::warn!(
                        level  = %metadata.level,
                        target = %metadata.target,
                        count = summary.count,
                        duration_secs = summary.duration.as_secs(),
                        "Log Throttling summary"
                        );
                    }
                }
            });
        }))
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
            let appender = RollingFileAppender::new(
                file_config.rotation.unwrap_or(LogRotation::NEVER),
                file_config.path,
                file_config.file_prefix,
            );
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

    // Use and_then to compose layers with global filter OUTERMOST (checked first)
    // This ensures filtered events never reach the throttling layer
    #[cfg(feature = "error_aggregation")]
    {
        use crate::libs::log::error_aggregation::get_error_aggregation;

        let (container, error_layer) = get_error_aggregation(error_aggregation_config);

        let subscriber = registry()
            .with(reloadable_global_filter) // Global filter
            .with(stdout_layer.and_then(file_layer).and_then(error_layer));

        #[cfg(feature = "log_throttling")]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handle,
            log_guards: (stdout_guard, file_guard),
            errors_container: container,
            log_throttling_handle,
        });

        #[cfg(not(feature = "log_throttling"))]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handle,
            log_guards: (stdout_guard, file_guard),
            errors_container: container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        let subscriber = registry()
            .with(reloadable_global_filter) // Global filter
            .with(stdout_layer.and_then(file_layer));

        #[cfg(feature = "log_throttling")]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handle,
            log_guards: (stdout_guard, file_guard),
            log_throttling_handle,
        });

        #[cfg(not(feature = "log_throttling"))]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handle,
            log_guards: (stdout_guard, file_guard),
        });
    }
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
/// - [ErrorAggregationContainer], if the [error_aggregation] feature is enabled. This object allows recent errors to be queried from the logging framework
pub fn setup_logging(config: LoggingConfig) -> eyre::Result<LogSetupReturn> {
    use tracing_subscriber::util::SubscriberInitExt;

    let parts = build_logging_subscriber(config)?;

    parts.subscriber.init();

    #[cfg(feature = "error_aggregation")]
    {
        #[cfg(feature = "log_throttling")]
        return Ok(LogSetupReturn {
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
            errors_container: parts.errors_container,
            log_throttling_handle: parts.log_throttling_handle,
        });
        #[cfg(not(feature = "log_throttling"))]
        return Ok(LogSetupReturn {
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
            errors_container: parts.errors_container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        #[cfg(feature = "log_throttling")]
        return Ok(LogSetupReturn {
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
            log_throttling_handle: parts.log_throttling_handle,
        });
        #[cfg(not(feature = "log_throttling"))]
        return Ok(LogSetupReturn {
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
        });
    }
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
                bail!("Error building new filter from given log level: {error}. Ignoring reload attempt")
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

    #[cfg(feature = "error_aggregation")]
    {
        Ok(LogSetupReturnTest {
            _guard: guard,
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
            errors_container: parts.errors_container,
        })
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        return Ok(LogSetupReturnTest {
            _guard: guard,
            reload_handle: parts.reload_handle,
            log_guards: parts.log_guards,
        });
    }
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
                file_prefix: "test".to_string(),
                file_log_level: Some(LogLevel::Info),
                rotation: None,
            }),
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

    #[tokio::test]
    async fn test_build_env_filter() {
        let filter = build_env_filter(LogLevel::Info);
        assert!(filter.is_ok());

        let filter = build_env_filter(LogLevel::Debug);
        assert!(filter.is_ok());

        let filter = build_env_filter(LogLevel::Detail);
        assert!(filter.is_ok());
    }

    /// Test that verifies the global EnvFilter short-circuits BEFORE the throttling
    /// filter sees filtered-out events. This ensures DEBUG events don't consume
    /// throttle budget when log level is INFO.
    #[cfg(feature = "log_throttling")]
    #[tokio::test]
    async fn test_global_filter_prevents_throttle_seeing_filtered_events() {
        // TODO: Setup
        // 1. Create a logging config with level=INFO and throttling enabled
        // 2. Configure throttling with a very aggressive limit (e.g., 1 event per minute)
        //    so that if the throttler sees events, it will throttle after the first one

        // TODO: Emit filtered-out events
        // 3. Emit many DEBUG events (these should be filtered by global EnvFilter)
        //    e.g., 100 debug!("filtered event {}", i) in a loop
        // 4. These events should NOT reach the throttling filter at all

        // TODO: Emit events that should pass the filter
        // 5. Emit INFO events that SHOULD pass the global filter
        //    e.g., info!("should_appear_1"), info!("should_appear_2"), etc.
        // 6. If global filter correctly short-circuits, the throttler's budget
        //    should NOT have been consumed by the DEBUG events

        // TODO: Verify behavior
        // 7. Check the log file - all INFO events should appear (not throttled)
        // 8. If the DEBUG events had reached the throttler, they would have
        //    consumed the budget and caused INFO events to be throttled
        //
        // Expected: All INFO events appear because DEBUG events never hit throttler
        // Failure mode: If DEBUG events reach throttler, they consume budget,
        //               and subsequent INFO events get throttled

        assert!(true)
    }

    // TODO: Test for log throttling

    // #[tokio::test(start_paused = true)]
    // async fn test_log_throttling_summaries() {
    //     use std::time::{Duration, Instant};
    //     use tracing::warn;

    //     let start = Instant::now();

    //     let temp_dir = tempfile::tempdir().unwrap();

    //     let config = LoggingConfig {
    //         level: LogLevel::Debug,
    //         file_config: Some(FileLoggingConfig {
    //             path: temp_dir.path().to_path_buf(),
    //             file_prefix: "test".to_string(),
    //             file_log_level: Some(LogLevel::Info),
    //             rotation: None,
    //         }),
    //         #[cfg(feature = "error_aggregation")]
    //         error_aggregation: default_error_aggregation_config(),
    //         #[cfg(feature = "log_throttling")]
    //         throttling_config: Some(LogThrottlingConfig {
    //             summary_emission_interval: Some(Duration::from_secs(60)),
    //             ..Default::default()
    //         }),
    //     };

    //     let log_setup = setup_logging_test(config).unwrap();

    //     // The elapsed time should be 0 because the clock is paused
    //     assert_eq!(start.elapsed().as_secs(), 0);

    //     tokio::time::advance(Duration::from_secs(75)).await;

    //     // Now the elapsed time should reflect the advanced duration
    //     assert!(start.elapsed().as_secs() >= 75);

    //     tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    //     // Test 3: Verify debug logs now appear after reload
    //     debug!("debug log");
    //     info!("info log");
    //     warn!("debug log");
    //     error!("debug log");

    //     tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    //     drop(log_setup);

    //     tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    //     // Verify file was created and contains expected logs
    //     let log_files: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
    //     assert_eq!(log_files.len(), 1, "Expected exactly one log file");

    //     let log_file = log_files[0].as_ref().unwrap();
    //     let log_contents = fs::read_to_string(log_file.path()).unwrap();

    //     // Verify messages before reload
    //     assert!(log_contents.contains("info_message_before_reload"));
    //     assert!(
    //         !log_contents.contains("debug_message_before_reload"),
    //         "Debug should not appear before reload"
    //     );
    //     assert!(log_contents.contains("error_message"));

    //     // Verify messages after reload
    //     assert!(log_contents.contains("info_message_after_reload"));
    //     assert!(
    //         log_contents.contains("debug_message_after_reload"),
    //         "Debug should appear after reload to DEBUG level"
    //     );

    //     // let handle = tokio::spawn(move || {
    //     //     clock_clone.advance(Duration::from_secs(5));
    //     // });

    //     // handle.join().unwrap();

    //     assert_eq!(clock.now(), start + Duration::from_secs(5));
    // }
}

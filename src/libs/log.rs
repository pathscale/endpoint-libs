//! Tracing-based logging setup with stdout and file logging support

use std::{path::PathBuf, time::Duration};

use eyre::{bail, Context, DefaultHandler, EyreHandler};
use tracing::Subscriber;
use tracing_appender::rolling::RollingFileAppender;
use tracing_subscriber::{
    fmt,
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
pub mod tracing_type_aliases;

pub use level_filter::*;
pub use tracing_type_aliases::*;

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

#[derive(Debug, Default)]
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
    pub reload_handles: LogReloadHandles,
    pub file_log_guard: Option<WorkerGuard>,
    #[cfg(feature = "error_aggregation")]
    pub errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    /// Call shutdown on this during graceful shutdown
    pub log_throttling_handle: TracingRateLimitLayer,
}

/// Internal struct to hold the result of building the logging subscriber
struct LoggingSubscriberParts {
    subscriber: Box<dyn Subscriber + Send + Sync + 'static>,
    reload_handles: LogReloadHandles,
    file_log_guard: Option<WorkerGuard>,
    #[cfg(feature = "error_aggregation")]
    errors_container: Arc<ErrorAggregationContainer>,
    #[cfg(feature = "log_throttling")]
    log_throttling_handle: TracingRateLimitLayer,
}

/// Internal function that builds the logging subscriber without initializing it.
/// Returns the subscriber along with reload handles and guards that need to be retained.
fn build_logging_subscriber(config: LoggingConfig) -> eyre::Result<LoggingSubscriberParts> {
    let stdout_loglevel_filter = build_env_filter(config.level)?;

    let stdout_layer = build_stdout_layer(stdout_loglevel_filter);

    let (stdout_layer, stdout_reload_handle) = reload::Layer::new(stdout_layer);

    let (file_layer, file_reload_handle, worker_guard) = config.file_config.as_ref().map_or_else(
        || (None, None, None),
        |file_config| {
            let (layer, handle, guard) = build_file_layer(&config, file_config.clone());
            (layer, handle, Some(guard))
        },
    );

    let reload_handles = LogReloadHandles {
        stdout: stdout_reload_handle,
        file: file_reload_handle,
    };

    #[cfg(feature = "log_throttling")]
    let throttling_config = config.throttling_config.unwrap_or_default();

    #[cfg(feature = "log_throttling")]
    let rate_limit_filter = TracingRateLimitLayer::builder()
        .with_excluded_fields(throttling_config.excluded_fields.unwrap_or_default())
        .with_exempt_targets(throttling_config.exemptions.unwrap_or_default())
        .with_active_emission(throttling_config.summary_emission_interval.is_some())
        .with_summary_interval(
            throttling_config
                .summary_emission_interval
                .unwrap_or(Duration::from_mins(5)),
        )
        .build()
        .unwrap();

    #[cfg(feature = "log_throttling")]
    let log_throttling_handle = rate_limit_filter.clone();

    #[cfg(feature = "error_aggregation")]
    {
        use crate::libs::log::error_aggregation::get_error_aggregation;

        let (container, error_layer) = get_error_aggregation(config.error_aggregation);

        #[cfg(not(feature = "log_throttling"))]
        let subscriber = registry()
            .with(stdout_layer)
            .with(file_layer)
            .with(error_layer);

        #[cfg(feature = "log_throttling")]
        let subscriber = registry()
            .with(stdout_layer)
            .with(file_layer.with_filter(rate_limit_filter))
            .with(error_layer);

        #[cfg(feature = "log_throttling")]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handles,
            file_log_guard: worker_guard,
            errors_container: container,
            log_throttling_handle,
        });

        #[cfg(not(feature = "log_throttling"))]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handles,
            file_log_guard: worker_guard,
            errors_container: container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        #[cfg(not(feature = "log_throttling"))]
        let subscriber = registry().with(stdout_layer).with(file_layer);

        #[cfg(feature = "log_throttling")]
        let subscriber = registry()
            .with(stdout_layer)
            .with(file_layer.with_filter(rate_limit_filter));

        #[cfg(feature = "log_throttling")]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handles,
            file_log_guard: worker_guard,
            log_throttling_handle,
        });

        #[cfg(not(feature = "log_throttling"))]
        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handles,
            file_log_guard: worker_guard,
        });
    }
}

/// Sets up a fully filtered tracing logging system
/// By default with the minimal config a stdout layer is setup with a custom env filter that filters according to the specified log level,
/// as well as according to a list of hardcoded crates. See [build_env_filter] for hardcoded crate log filtering.
/// Optionally, file logging can be configured with [FileLoggingConfig], which minimally will create a timestamp prefixed log file in the given
/// path that does not rotate (one log file per program execution)
///
/// Extra configuration for file logging is:
/// - A separate log level for the file
/// - Log file rotation. See [LogRotation] for possible options
///
/// Returns [LogSetupReturn], which is a composite struct containing objects that need to be retained by the client such as:
/// - [LogReloadHandles], for setting a new log level during runtime
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
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            errors_container: parts.errors_container,
            log_throttling_handle: parts.log_throttling_handle,
        });
        #[cfg(not(feature = "log_throttling"))]
        return Ok(LogSetupReturn {
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            errors_container: parts.errors_container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        #[cfg(feature = "log_throttling")]
        return Ok(LogSetupReturn {
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            log_throttling_handle: parts.log_throttling_handle,
        });
        #[cfg(not(feature = "log_throttling"))]
        return Ok(LogSetupReturn {
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
        });
    }
}

fn build_file_layer(
    config: &LoggingConfig,
    file_config: FileLoggingConfig,
) -> (
    Option<FileLogReloadableLayer>,
    Option<FileLogReloadHandle>,
    WorkerGuard,
) {
    let file_loglevel_filter = {
        let file_log_level = file_config.file_log_level.unwrap_or(config.level);

        build_env_filter(file_log_level)
            .expect("Error building log level env filter for file layer")
    };

    let appender = RollingFileAppender::new(
        file_config.rotation.unwrap_or(LogRotation::NEVER),
        file_config.path,
        file_config.file_prefix,
    );

    let (non_blocking_appender, guard) = tracing_appender::non_blocking(appender);

    let filtered_layer = tracing_subscriber::fmt::layer()
        .with_thread_names(true)
        .with_line_number(true)
        .with_ansi(false)
        .with_writer(non_blocking_appender)
        .with_filter(file_loglevel_filter);

    let (file_layer, reload_handle) = reload::Layer::new(filtered_layer);

    (Some(file_layer), Some(reload_handle), guard)
}

fn build_stdout_layer(
    stdout_loglevel_filter: EnvFilter,
) -> tracing_subscriber::filter::Filtered<
    fmt::Layer<registry::Registry>,
    EnvFilter,
    registry::Registry,
> {
    tracing_subscriber::fmt::layer()
        .with_thread_names(true)
        .with_line_number(true)
        .with_filter(stdout_loglevel_filter)
}

#[derive(Debug, Clone)]
pub struct LogReloadHandles {
    stdout: Handle<StdoutLayerType, Registry>,
    file: Option<Handle<FileLayerType, RegistryWithStdout>>,
}

// TODO: Find out if the log throttling still works after reloading

impl LogReloadHandles {
    pub fn new(
        stdout: Handle<StdoutLayerType, Registry>,
        file: Option<Handle<FileLayerType, RegistryWithStdout>>,
    ) -> Self {
        Self { stdout, file }
    }

    /// Sets a new log level on the file logging layer and the stdout logging layer
    /// Both layers are optional, since it may be desired to only change the one
    /// If None is set for a layer, this call will do nothing to it
    /// If Some(level) is set for file_level but file logging was not configured, this function will log a warning and return Ok
    /// Errors will be logged by this function and can be safely ignored, but are returned for display purposes
    pub fn set_new_log_level(
        &self,
        stdout_level: Option<LogLevel>,
        file_level: Option<LogLevel>,
    ) -> eyre::Result<()> {
        if let Some(level) = stdout_level {
            match build_env_filter(level) {
                Ok(filter) => match self.stdout.modify(|layer| *layer.filter_mut() = filter) {
                    Ok(_) => (),
                    Err(error) => {
                        tracing::error!(
                            ?error,
                            "Error setting new filter on stdout layer. Ignoring reload attempt"
                        );
                        bail!("Error setting new filter on stdout layer: {error}. Ignoring reload attempt")
                    }
                },
                Err(error) => {
                    tracing::error!(?error, "Error building new filter for stdout logging from given log level. Ignoring reload attempt");
                    bail!("Error building new filter for stdout logging from given log level: {error}. Ignoring reload attempt")
                }
            }
        }

        if let Some(level) = file_level {
            if let Some(file_handle) = &self.file {
                match build_env_filter(level) {
                    Ok(filter) => file_handle
                        .modify(|layer| *layer.filter_mut() = filter)
                        .wrap_err(
                            "Error setting new filter on file layer. Ignoring reload attempt",
                        )?,
                    Err(error) => {
                        tracing::error!(?error, "Error building new filter for file logging from given log level. Ignoring reload attempt");
                        bail!("Error building new filter for file logging from given log level: {error}. Ignoring reload attempt")
                    }
                }
            }
        } else {
            tracing::warn!("Attempted to set a new log level for the file layer, but not file layer was configured on startup. Ignoring");
        }

        Ok(())
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
    reload_handles: LogReloadHandles,
    #[allow(dead_code)]
    file_log_guard: Option<WorkerGuard>,
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
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            errors_container: parts.errors_container,
        })
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        return Ok(LogSetupReturnTest {
            _guard: guard,
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
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

        // Test 2: Reload log level to DEBUG
        let result = log_setup
            .reload_handles
            .set_new_log_level(Some(LogLevel::Debug), Some(LogLevel::Debug));
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

//! Tracing-based logging setup with stdout and file logging support

use std::path::PathBuf;

use eyre::{bail, Context};
use tracing_appender::{non_blocking::WorkerGuard, rolling::RollingFileAppender};
use tracing_subscriber::{
    fmt,
    layer::SubscriberExt,
    registry,
    reload::{self, Handle},
    EnvFilter, Layer, Registry,
};
use tracing::Subscriber;

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

#[derive(Debug)]
pub struct LoggingConfig {
    level: LogLevel,
    file_config: Option<FileLoggingConfig>,
    #[cfg(feature = "error_aggregation")]
    error_aggregation: ErrorAggregationConfig,
}

#[derive(Debug)]
pub struct LogSetupReturn {
    #[allow(dead_code)]
    reload_handles: LogReloadHandles,
    #[allow(dead_code)]
    file_log_guard: Option<WorkerGuard>,
    #[cfg(feature = "error_aggregation")]
    #[allow(dead_code)]
    errors_container: Arc<ErrorAggregationContainer>,
}

#[derive(Debug, Clone)]
pub struct FileLoggingConfig {
    path: PathBuf,
    file_prefix: String,
    /// Used to specify a separate level than the overall log level. e.g. stdout logs DEBUG, but file only logs INFO
    file_log_level: Option<LogLevel>,
    // Specifying None means that there will be one log file per program execution
    rotation: Option<LogRotation>,
}

/// Internal struct to hold the result of building the logging subscriber
struct LoggingSubscriberParts {
    subscriber: Box<dyn Subscriber + Send + Sync + 'static>,
    reload_handles: LogReloadHandles,
    file_log_guard: Option<WorkerGuard>,
    #[cfg(feature = "error_aggregation")]
    errors_container: Arc<ErrorAggregationContainer>,
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

    #[cfg(feature = "error_aggregation")]
    {
        use crate::libs::log::error_aggregation::get_error_aggregation;

        let (container, error_layer) = get_error_aggregation(config.error_aggregation);

        let subscriber = registry()
            .with(stdout_layer)
            .with(file_layer)
            .with(error_layer);

        return Ok(LoggingSubscriberParts {
            subscriber: Box::new(subscriber),
            reload_handles,
            file_log_guard: worker_guard,
            errors_container: container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
        let subscriber = registry().with(stdout_layer).with(file_layer);

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
        return Ok(LogSetupReturn {
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            errors_container: parts.errors_container,
        });
    }

    #[cfg(not(feature = "error_aggregation"))]
    {
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
        return Ok(LogSetupReturnTest {
            _guard: guard,
            reload_handles: parts.reload_handles,
            file_log_guard: parts.file_log_guard,
            errors_container: parts.errors_container,
        });
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
    #[test]
    fn test_basic_logging_stdout() {
        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            #[cfg(feature = "error_aggregation")]
            error_aggregation: default_error_aggregation_config(),
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
    #[test]
    fn test_file_logging_comprehensive() {
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

    #[test]
    fn test_build_env_filter() {
        let filter = build_env_filter(LogLevel::Info);
        assert!(filter.is_ok());

        let filter = build_env_filter(LogLevel::Debug);
        assert!(filter.is_ok());

        let filter = build_env_filter(LogLevel::Detail);
        assert!(filter.is_ok());
    }
}

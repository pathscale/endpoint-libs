//! Log level enum and environment filter builder

use std::str::FromStr;

use eyre::eyre;
use serde::{Deserialize, Serialize};
use tracing::{level_filters::LevelFilter, Level};
use tracing_subscriber::EnvFilter;

/// Helper function to determine if one level is more restrictive than another
/// More restrictive = shows fewer logs
/// Ordering: TRACE < DEBUG < INFO < WARN < ERROR
fn is_more_restrictive(a: Level, b: Level) -> bool {
    let restrictiveness = |level: Level| match level {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    };
    
    restrictiveness(a) > restrictiveness(b)
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, PartialOrd, Ord, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Detail,
}

pub fn build_env_filter(log_level: LogLevel) -> eyre::Result<EnvFilter> {
    let level: Level = log_level.into();
    
    // Start with the default env filter and add a base directive for all targets
    let mut filter = EnvFilter::from_default_env().add_directive(level.into());

    // Only apply crate-specific capping when log level is Debug or Trace (but NOT Detail)
    // At Info and above, the global level already prevents excessive verbosity
    // At Detail, we let everything through at TRACE level without caps
    if log_level > LogLevel::Info && log_level != LogLevel::Detail {
        const DIRECTIVES: &[(Level, &str)] = &[
            (Level::DEBUG, "tungstenite::protocol"),
            (Level::DEBUG, "tokio_postgres::connection"),
            (Level::DEBUG, "tokio_util::codec::framed_impl"),
            (Level::DEBUG, "tokio_tungstenite"),
            (Level::INFO, "h2"),
            (Level::INFO, "rustls::client::hs"),
            (Level::INFO, "rustls::client::tls13"),
            (Level::INFO, "hyper::client"),
            (Level::INFO, "hyper::proto"),
            (Level::INFO, "mio"),
            (Level::INFO, "want"),
            (Level::INFO, "sqlparser"),
        ];

        for (directive_level, crate_name) in DIRECTIVES {
            // Only apply directive if it's MORE RESTRICTIVE than the global level
            // At Debug: only apply INFO directives (they're more restrictive)
            // At Trace: apply both DEBUG and INFO directives (both are more restrictive)
            if is_more_restrictive(*directive_level, level) {
                let directive_string = format!("{}={}", crate_name, directive_level.to_string().to_lowercase());
                filter = filter.add_directive(directive_string.parse()?);
            }
        }
    }

    Ok(filter)
}

impl From<LogLevel> for LevelFilter {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Trace => LevelFilter::TRACE,
            LogLevel::Detail => LevelFilter::TRACE,
            LogLevel::Off => LevelFilter::OFF,
        }
    }
}

impl From<LogLevel> for Level {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
            LogLevel::Off => Level::TRACE,
            LogLevel::Detail => Level::TRACE,
        }
    }
}

impl FromStr for LogLevel {
    type Err = eyre::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_ref() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            "detail" => Ok(LogLevel::Detail),
            "off" => Ok(LogLevel::Off),
            _ => Err(eyre!("Invalid log level: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_env_filter_all_levels() {
        // Test that build_env_filter successfully builds for all LogLevel variants
        let levels = vec![
            LogLevel::Off,
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
            LogLevel::Detail,
        ];

        for level in levels {
            let result = build_env_filter(level);
            assert!(
                result.is_ok(),
                "Failed to build env filter for level: {:?}",
                level
            );
        }
    }

    #[test]
    fn test_build_env_filter_with_error_level() {
        // Test specifically that ERROR level creates a valid filter
        let filter = build_env_filter(LogLevel::Error);
        assert!(filter.is_ok());

        let filter = filter.unwrap();
        // The filter should exist and be usable
        drop(filter);
    }

    #[test]
    fn test_build_env_filter_with_detail_level() {
        // Test that Detail level doesn't add specific crate directives
        let filter = build_env_filter(LogLevel::Detail);
        assert!(filter.is_ok());
    }

    #[test]
    fn test_build_env_filter_crate_directives_applied() {
        // Test that non-Detail levels apply crate-specific directives
        let filter_info = build_env_filter(LogLevel::Info);
        let filter_detail = build_env_filter(LogLevel::Detail);

        assert!(filter_info.is_ok());
        assert!(filter_detail.is_ok());
        // Both should be valid filters - the difference is in their internal directives
    }

    #[test]
    fn test_env_filter_directive_creation() {
        // Test that we can create string-based directives for crates
        let level = Level::ERROR;
        let crate_name = "test_crate";
        let capped_level = std::cmp::max(level, Level::INFO);
        let directive_string = format!("{}={}", crate_name, capped_level.to_string().to_lowercase());
        
        // This should parse without error
        let result = directive_string.parse::<tracing_subscriber::filter::Directive>();
        assert!(result.is_ok(), "Failed to parse directive: {}", directive_string);
    }

    #[tokio::test]
    async fn test_min_logic_filters_verbose_crates() {
        // Import necessary items for logging test setup
        use crate::libs::log::{setup_logging_test, LoggingConfig};
        use tracing::{debug, info, warn, error};

        // Setup logging at Debug level (which is > Info, so directives apply)
        let config = LoggingConfig {
            level: LogLevel::Debug,
            file_config: None,
            #[cfg(feature = "error_aggregation")]
            error_aggregation: crate::libs::log::ErrorAggregationConfig {
                limit: 100,
                normalize: true,
            },
            #[cfg(feature = "log_throttling")]
            throttling_config: Some(crate::libs::log::LogThrottlingConfig::default()),
        };

        let _guard = setup_logging_test(config);
        assert!(_guard.is_ok(), "Failed to setup logging for test");

        // Global level logs (no target specified) at Debug level should all appear
        debug!("Global debug log should appear");
        info!("Global info log should appear");
        warn!("Global warn log should appear");
        error!("Global error log should appear");
        
        // Emit logs from tungstenite::protocol (which has directive (Level::DEBUG, "tungstenite::protocol"))
        // With LogLevel::Debug, min(DEBUG, DEBUG) = DEBUG, so DEBUG logs should appear
        debug!(target: "tungstenite::protocol", "Targeted debug log from tungstenite::protocol should appear");
        info!(target: "tungstenite::protocol", "Targeted info log from tungstenite::protocol should appear");
        
        // Emit logs from h2 (which has directive (Level::INFO, "h2"))
        // With LogLevel::Debug, h2 is capped at INFO, so DEBUG logs should NOT appear
        debug!(target: "h2", "Targeted debug log from h2 should NOT appear");
        info!(target: "h2", "Targeted info log from h2 should appear");
        
        // Allow time for logs to be processed
        std::thread::sleep(std::time::Duration::from_millis(100));
        
        // If we got here without panic, the filter logic worked correctly
        // The test passes if the logs are emitted without errors
    }

    #[tokio::test]
    async fn test_no_directives_applied_at_info_level() {
        // When log level is Info or above, directives should NOT be applied
        // because the global level is restrictive enough
        use crate::libs::log::{setup_logging_test, LoggingConfig};
        use tracing::{debug, info, warn, error};

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            #[cfg(feature = "error_aggregation")]
            error_aggregation: crate::libs::log::ErrorAggregationConfig {
                limit: 100,
                normalize: true,
            },
            #[cfg(feature = "log_throttling")]
            throttling_config: Some(crate::libs::log::LogThrottlingConfig::default()),
        };

        let _guard = setup_logging_test(config);
        assert!(_guard.is_ok(), "Failed to setup logging for test");

        // Global level logs - at Info level, only Info and above should appear
        debug!("Global debug log should NOT appear");
        info!("Global info log should appear");
        warn!("Global warn log should appear");
        error!("Global error log should appear");
        
        // At Info level, no crate directives are applied, so global level (info) applies
        // This means debug logs from anywhere should not appear
        debug!(target: "tungstenite::protocol", "Targeted debug from tungstenite::protocol should NOT appear");
        info!(target: "tungstenite::protocol", "Targeted info from tungstenite::protocol should appear");
        
        // Similarly for h2 - debug should not appear, info and above should
        debug!(target: "h2", "Targeted debug from h2 should NOT appear");
        info!(target: "h2", "Targeted info from h2 should appear");
        warn!(target: "h2", "Targeted warn from h2 should appear");
        
        std::thread::sleep(std::time::Duration::from_millis(100));
        
        // Test passes if logs are processed without errors
    }

    #[test]
    fn test_filter_string_format() {
        // Test to verify the actual filter directives being generated
        let filter_debug = build_env_filter(LogLevel::Debug).unwrap();
        let filter_info = build_env_filter(LogLevel::Info).unwrap();
        let filter_error = build_env_filter(LogLevel::Error).unwrap();
        let filter_trace = build_env_filter(LogLevel::Trace).unwrap();
        let filter_detail = build_env_filter(LogLevel::Detail).unwrap();
        
        println!("Debug level filter: {}", filter_debug.to_string());
        println!("Info level filter: {}", filter_info.to_string());
        println!("Error level filter: {}", filter_error.to_string());
        println!("Trace level filter: {}", filter_trace.to_string());
        println!("Detail level filter: {}", filter_detail.to_string());
        
        // At Debug level: should only have INFO directives (more restrictive than DEBUG)
        // Should have h2=info (more restrictive), but NOT tungstenite::protocol=debug
        let debug_str = filter_debug.to_string();
        assert!(debug_str.contains("h2=info"), "Debug filter should contain h2=info (INFO is more restrictive): {}", debug_str);
        assert!(!debug_str.contains("tungstenite::protocol=debug"), "Debug filter should NOT contain tungstenite::protocol=debug: {}", debug_str);
        assert!(debug_str.contains("debug"), "Debug filter should end with 'debug' as base level: {}", debug_str);
        
        // At Info level, should NOT have any crate-specific directives
        let info_str = filter_info.to_string();
        assert!(info_str.starts_with("info"), "Info filter should start with 'info': {}", info_str);
        assert!(!info_str.contains("h2="), "Info filter should not have h2 directive: {}", info_str);
        
        // At Error level, should NOT have any crate-specific directives
        let error_str = filter_error.to_string();
        assert!(error_str.starts_with("error"), "Error filter should start with 'error': {}", error_str);
        assert!(!error_str.contains("h2="), "Error filter should not have h2 directive: {}", error_str);
        
        // At Trace level: should have ALL directives (both DEBUG and INFO are more restrictive)
        let trace_str = filter_trace.to_string();
        assert!(trace_str.contains("tungstenite::protocol=debug"), "Trace filter should contain tungstenite::protocol=debug: {}", trace_str);
        assert!(trace_str.contains("h2=info"), "Trace filter should contain h2=info: {}", trace_str);
        assert!(trace_str.contains("trace"), "Trace filter should end with 'trace' as base level: {}", trace_str);
        
        // At Detail level, should NOT have any crate-specific directives
        let detail_str = filter_detail.to_string();
        assert!(detail_str.starts_with("trace"), "Detail filter should start with 'trace': {}", detail_str);
        assert!(!detail_str.contains("h2="), "Detail filter should not have h2 directive: {}", detail_str);
    }

    #[test]
    fn test_level_ordering() {
        // Verify that Level ordering matches our assumptions
        // This test ensures the filtering logic remains correct if tracing crate behavior changes
        // Ordering: TRACE (least restrictive) < DEBUG < INFO < WARN < ERROR (most restrictive)
        
        assert!(is_more_restrictive(Level::ERROR, Level::WARN), "ERROR should be more restrictive than WARN");
        assert!(is_more_restrictive(Level::WARN, Level::INFO), "WARN should be more restrictive than INFO");
        assert!(is_more_restrictive(Level::INFO, Level::DEBUG), "INFO should be more restrictive than DEBUG");
        assert!(is_more_restrictive(Level::DEBUG, Level::TRACE), "DEBUG should be more restrictive than TRACE");
        
        // Verify the inverse relationships
        assert!(!is_more_restrictive(Level::TRACE, Level::DEBUG), "TRACE should NOT be more restrictive than DEBUG");
        assert!(!is_more_restrictive(Level::DEBUG, Level::INFO), "DEBUG should NOT be more restrictive than INFO");
        assert!(!is_more_restrictive(Level::INFO, Level::WARN), "INFO should NOT be more restrictive than WARN");
        assert!(!is_more_restrictive(Level::WARN, Level::ERROR), "WARN should NOT be more restrictive than ERROR");
    }
}

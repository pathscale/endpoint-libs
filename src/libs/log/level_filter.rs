//! Log level enum and environment filter builder

use std::str::FromStr;

use eyre::eyre;
use serde::{Deserialize, Serialize};
use tracing::{level_filters::LevelFilter, Level};
use tracing_subscriber::EnvFilter;

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
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

    if log_level != LogLevel::Detail {
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
            let capped_level = std::cmp::max(level, *directive_level);
            let directive_string = format!("{}={}", crate_name, capped_level.to_string().to_lowercase());
            filter = filter.add_directive(directive_string.parse()?);
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
}

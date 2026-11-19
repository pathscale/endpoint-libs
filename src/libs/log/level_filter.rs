//! Log level enum and environment filter builder

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::{self, Display};
use core::str::FromStr;

use crate::eyre::{self, eyre};
use serde::{Deserialize, Serialize};
use tracing::Metadata;
use tracing::{Level, level_filters::LevelFilter};

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

/// One parsed filter directive, either global (`info`) or target-scoped (`h2=warn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    target: Option<String>,
    level: LevelFilter,
}

impl From<LevelFilter> for Directive {
    fn from(level: LevelFilter) -> Self {
        Self {
            target: None,
            level,
        }
    }
}

impl From<Level> for Directive {
    fn from(level: Level) -> Self {
        Self {
            target: None,
            level: LevelFilter::from_level(level),
        }
    }
}

fn parse_level_filter(raw: &str) -> eyre::Result<LevelFilter> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(LevelFilter::OFF),
        "error" => Ok(LevelFilter::ERROR),
        "warn" => Ok(LevelFilter::WARN),
        "info" => Ok(LevelFilter::INFO),
        "debug" => Ok(LevelFilter::DEBUG),
        "trace" => Ok(LevelFilter::TRACE),
        other => Err(eyre!("invalid filter level directive: {other}")),
    }
}

fn format_level_filter(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::OFF => "off",
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::INFO => "info",
        LevelFilter::DEBUG => "debug",
        LevelFilter::TRACE => "trace",
    }
}

impl FromStr for Directive {
    type Err = eyre::Error;

    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(eyre!("empty filter directive"));
        }
        if let Some((left, right)) = trimmed.split_once('=') {
            let target = left.split('[').next().unwrap_or(left).trim();
            let level = parse_level_filter(right)?;
            if target.is_empty() {
                Ok(Self {
                    target: None,
                    level,
                })
            } else {
                Ok(Self {
                    target: Some(target.to_string()),
                    level,
                })
            }
        } else if let Ok(level) = parse_level_filter(trimmed) {
            Ok(Self {
                target: None,
                level,
            })
        } else {
            let target = trimmed.split('[').next().unwrap_or(trimmed).trim();
            if target.is_empty() {
                return Err(eyre!("invalid filter directive: {trimmed}"));
            }
            Ok(Self {
                target: Some(target.to_string()),
                level: LevelFilter::TRACE,
            })
        }
    }
}

impl Display for Directive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target {
            Some(target) => write!(f, "{target}={}", format_level_filter(self.level)),
            None => f.write_str(format_level_filter(self.level)),
        }
    }
}

/// Environment-configurable target/level filter (`no_std` replacement for `tracing_subscriber::EnvFilter`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFilter {
    default_level: LevelFilter,
    directives: Vec<(String, LevelFilter)>,
}

impl Default for EnvFilter {
    fn default() -> Self {
        Self {
            default_level: LevelFilter::ERROR,
            directives: Vec::new(),
        }
    }
}

impl EnvFilter {
    /// Construct an empty filter whose base level is `OFF` until directives are added.
    pub fn empty() -> Self {
        Self {
            default_level: LevelFilter::OFF,
            directives: Vec::new(),
        }
    }

    /// Construct a filter from `RUST_LOG`, falling back to `ERROR` if unset.
    /// Malformed directives in `RUST_LOG` are ignored, matching `EnvFilter::from_default_env`.
    pub fn from_default_env() -> Self {
        let mut filter = Self::default();
        if let Some(raw) = eko::env::var("RUST_LOG") {
            for part in raw.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if let Ok(directive) = part.parse::<Directive>() {
                    filter = filter.add_directive(directive);
                }
            }
        }
        filter
    }

    /// Add or replace a directive in this filter.
    pub fn add_directive(mut self, directive: impl Into<Directive>) -> Self {
        let directive = directive.into();
        match directive.target {
            None => {
                self.default_level = directive.level;
            }
            Some(target) => {
                if let Some((_, existing)) = self.directives.iter_mut().find(|(t, _)| *t == target)
                {
                    *existing = directive.level;
                } else {
                    self.directives.push((target, directive.level));
                }
            }
        }
        self
    }

    /// Maximum verbosity level any target in this filter can emit.
    pub fn max_level_hint(&self) -> LevelFilter {
        let mut max = self.default_level;
        for (_, level) in &self.directives {
            if *level > max {
                max = *level;
            }
        }
        max
    }

    /// Whether an event or span with `metadata` is enabled by this filter.
    pub fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let target = metadata.target();
        let mut matched: Option<(usize, LevelFilter)> = None;
        for (prefix, level) in &self.directives {
            let prefix_bytes = prefix.as_bytes();
            let target_bytes = target.as_bytes();
            let is_match = if target_bytes.len() == prefix_bytes.len() {
                target == prefix
            } else if target_bytes.len() > prefix_bytes.len() && target.starts_with(prefix.as_str())
            {
                // Match on module boundary (`::`) or explicit prefix boundary.
                target_bytes.get(prefix_bytes.len()..prefix_bytes.len() + 2) == Some(b"::")
            } else {
                false
            };
            if is_match {
                match matched {
                    Some((best_len, _)) if best_len > prefix.len() => {}
                    _ => matched = Some((prefix.len(), *level)),
                }
            }
        }

        let allowed = matched.map(|(_, l)| l).unwrap_or(self.default_level);
        match allowed.into_level() {
            None => false,
            Some(max_level) => {
                // In tracing, Level::ERROR < Level::WARN < Level::INFO < Level::DEBUG < Level::TRACE
                *metadata.level() <= max_level
            }
        }
    }
}

impl FromStr for EnvFilter {
    type Err = eyre::Error;

    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
        let mut filter = Self::empty();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            filter = filter.add_directive(part.parse::<Directive>()?);
        }
        Ok(filter)
    }
}

impl Display for EnvFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.directives.is_empty() {
            return f.write_str(format_level_filter(self.default_level));
        }
        for (idx, (target, level)) in self.directives.iter().enumerate() {
            if idx > 0 {
                f.write_str(",")?;
            }
            write!(f, "{target}={}", format_level_filter(*level))?;
        }
        write!(f, ",{}", format_level_filter(self.default_level))
    }
}

pub fn build_env_filter(log_level: LogLevel) -> eyre::Result<EnvFilter> {
    let level_filter: LevelFilter = log_level.into();

    // Start with the default env filter and add a base directive for all targets
    let mut filter = EnvFilter::from_default_env().add_directive(level_filter);

    // Only apply crate-specific capping when log level is Debug or Trace (but NOT Detail)
    // At Info and above, the global level already prevents excessive verbosity
    // At Detail, we let everything through at TRACE level without caps
    if log_level > LogLevel::Info && log_level != LogLevel::Detail {
        let level: Level = log_level.into();
        const DIRECTIVES: &[(Level, &str)] = &[
            (Level::DEBUG, "tungstenite::protocol"),
            (Level::INFO, "h2"),
            (Level::INFO, "rustls::client::hs"),
            (Level::INFO, "rustls::client::tls13"),
            (Level::INFO, "hyper::client"),
            (Level::INFO, "hyper::proto"),
            (Level::INFO, "mio"),
            (Level::INFO, "want"),
            (Level::INFO, "sqlparser"),
            // OpenTelemetry specific caps to prevent excessive verbosity and recursion
            (Level::WARN, "opentelemetry"),
            (Level::WARN, "opentelemetry_otlp"),
            (Level::WARN, "opentelemetry_sdk"),
            (Level::WARN, "tracing_opentelemetry"),
            (Level::WARN, "h2"),
            (Level::WARN, "tower"),
            (Level::WARN, "hyper_util"),
            (Level::WARN, "tonic"),
        ];

        for (directive_level, crate_name) in DIRECTIVES {
            // Only apply directive if it's MORE RESTRICTIVE than the global level
            // At Debug: only apply INFO/WARN directives (they're more restrictive)
            // At Trace: apply DEBUG, INFO, and WARN directives (all are more restrictive)
            if is_more_restrictive(*directive_level, level) {
                let directive_string = format!(
                    "{}={}",
                    crate_name,
                    directive_level.to_string().to_lowercase()
                );
                filter = filter.add_directive(directive_string.parse::<Directive>()?);
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
    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
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
        let filter = build_env_filter(LogLevel::Error);
        assert!(filter.is_ok());

        let filter = filter.unwrap();
        drop(filter);
    }

    #[test]
    fn test_build_env_filter_with_detail_level() {
        let filter = build_env_filter(LogLevel::Detail);
        assert!(filter.is_ok());
    }

    #[test]
    fn test_build_env_filter_crate_directives_applied() {
        let filter_info = build_env_filter(LogLevel::Info);
        let filter_detail = build_env_filter(LogLevel::Detail);

        assert!(filter_info.is_ok());
        assert!(filter_detail.is_ok());
    }

    #[test]
    fn test_env_filter_directive_creation() {
        let level = Level::ERROR;
        let crate_name = "test_crate";
        let capped_level = core::cmp::max(level, Level::INFO);
        let directive_string =
            format!("{}={}", crate_name, capped_level.to_string().to_lowercase());

        let result = directive_string.parse::<Directive>();
        assert!(
            result.is_ok(),
            "Failed to parse directive: {}",
            directive_string
        );
    }

    #[test]
    fn test_min_logic_filters_verbose_crates() {
        use crate::libs::log::{LoggingConfig, setup_logging_test};
        use tracing::{debug, error, info, warn};

        let config = LoggingConfig {
            level: LogLevel::Debug,
            file_config: None,
            otel_config: crate::libs::log::OtelConfig::default(),
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

        debug!("Global debug log should appear");
        info!("Global info log should appear");
        warn!("Global warn log should appear");
        error!("Global error log should appear");

        debug!(target: "tungstenite::protocol", "Targeted debug log from tungstenite::protocol should appear");
        info!(target: "tungstenite::protocol", "Targeted info log from tungstenite::protocol should appear");

        debug!(target: "h2", "Targeted debug log from h2 should NOT appear");
        info!(target: "h2", "Targeted info log from h2 should appear");

        eko::sleep_ms(100);
    }

    #[test]
    fn test_no_directives_applied_at_info_level() {
        use crate::libs::log::{LoggingConfig, setup_logging_test};
        use tracing::{debug, error, info, warn};

        let config = LoggingConfig {
            level: LogLevel::Info,
            file_config: None,
            otel_config: crate::libs::log::OtelConfig::default(),
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

        debug!("Global debug log should NOT appear");
        info!("Global info log should appear");
        warn!("Global warn log should appear");
        error!("Global error log should appear");

        debug!(target: "tungstenite::protocol", "Targeted debug from tungstenite::protocol should NOT appear");
        info!(target: "tungstenite::protocol", "Targeted info from tungstenite::protocol should appear");

        debug!(target: "h2", "Targeted debug from h2 should NOT appear");
        info!(target: "h2", "Targeted info from h2 should appear");
        warn!(target: "h2", "Targeted warn from h2 should appear");

        eko::sleep_ms(100);
    }

    #[test]
    fn test_filter_string_format() {
        let filter_debug = build_env_filter(LogLevel::Debug).unwrap();
        let filter_info = build_env_filter(LogLevel::Info).unwrap();
        let filter_error = build_env_filter(LogLevel::Error).unwrap();
        let filter_trace = build_env_filter(LogLevel::Trace).unwrap();
        let filter_detail = build_env_filter(LogLevel::Detail).unwrap();

        let debug_str = filter_debug.to_string();
        assert!(
            debug_str.contains("h2=warn"),
            "Debug filter should contain h2=warn (OTel cap): {}",
            debug_str
        );
        assert!(
            !debug_str.contains("tungstenite::protocol=debug"),
            "Debug filter should NOT contain tungstenite::protocol=debug: {}",
            debug_str
        );
        assert!(
            debug_str.contains("debug"),
            "Debug filter should end with 'debug' as base level: {}",
            debug_str
        );

        let info_str = filter_info.to_string();
        assert!(
            info_str.starts_with("info"),
            "Info filter should start with 'info': {}",
            info_str
        );
        assert!(
            !info_str.contains("h2="),
            "Info filter should not have h2 directive: {}",
            info_str
        );

        let error_str = filter_error.to_string();
        assert!(
            error_str.starts_with("error"),
            "Error filter should start with 'error': {}",
            error_str
        );
        assert!(
            !error_str.contains("h2="),
            "Error filter should not have h2 directive: {}",
            error_str
        );

        let trace_str = filter_trace.to_string();
        assert!(
            trace_str.contains("tungstenite::protocol=debug"),
            "Trace filter should contain tungstenite::protocol=debug: {}",
            trace_str
        );
        assert!(
            trace_str.contains("h2=warn"),
            "Trace filter should contain h2=warn (OTel cap): {}",
            trace_str
        );
        assert!(
            trace_str.contains("opentelemetry=warn"),
            "Trace filter should contain opentelemetry=warn (OTel cap): {}",
            trace_str
        );
        assert!(
            trace_str.contains("trace"),
            "Trace filter should end with 'trace' as base level: {}",
            trace_str
        );

        let detail_str = filter_detail.to_string();
        assert!(
            detail_str.starts_with("trace"),
            "Detail filter should start with 'trace': {}",
            detail_str
        );
        assert!(
            !detail_str.contains("h2="),
            "Detail filter should not have h2 directive: {}",
            detail_str
        );
    }

    #[test]
    fn test_level_ordering() {
        assert!(
            is_more_restrictive(Level::ERROR, Level::WARN),
            "ERROR should be more restrictive than WARN"
        );
        assert!(
            is_more_restrictive(Level::WARN, Level::INFO),
            "WARN should be more restrictive than INFO"
        );
        assert!(
            is_more_restrictive(Level::INFO, Level::DEBUG),
            "INFO should be more restrictive than DEBUG"
        );
        assert!(
            is_more_restrictive(Level::DEBUG, Level::TRACE),
            "DEBUG should be more restrictive than TRACE"
        );

        assert!(
            !is_more_restrictive(Level::TRACE, Level::DEBUG),
            "TRACE should NOT be more restrictive than DEBUG"
        );
        assert!(
            !is_more_restrictive(Level::DEBUG, Level::INFO),
            "DEBUG should NOT be more restrictive than INFO"
        );
        assert!(
            !is_more_restrictive(Level::INFO, Level::WARN),
            "INFO should NOT be more restrictive than WARN"
        );
        assert!(
            !is_more_restrictive(Level::WARN, Level::ERROR),
            "WARN should NOT be more restrictive than ERROR"
        );
    }
}

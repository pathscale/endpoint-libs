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
            let new_directive =
                format!("{}={}", crate_name, capped_level.to_string().to_lowercase());
            filter = filter.add_directive(new_directive.parse()?);
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

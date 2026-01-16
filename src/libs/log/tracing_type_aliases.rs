//! Verbose tracing type aliases to simplify core logging code

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;
use tracing_subscriber::{
    fmt,
    reload::{self, Handle},
    EnvFilter, Registry,
};

#[cfg(feature = "log_throttling")]
use tracing_throttle::TracingRateLimitLayer;

#[cfg(feature = "log_throttling")]
use tracing_subscriber::filter::combinator::And;

// Type aliases for the concrete layer types used in this module, since generics for client code would be very annoying

/// Reloadable EnvFilter for stdout (wrapped in reload::Layer, implements Filter)
pub type ReloadableStdoutFilter = reload::Layer<EnvFilter, Registry>;

/// Stdout layer with reloadable EnvFilter
pub type StdoutLayerType = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<Registry>,
    ReloadableStdoutFilter,
    Registry,
>;

/// Handle for reloading the stdout EnvFilter at runtime
pub type StdoutLogReloadHandle = Handle<EnvFilter, Registry>;

pub type RegistryWithStdout = tracing_subscriber::layer::Layered<
    StdoutLayerType,
    Registry,
>;

/// Reloadable EnvFilter wrapped in reload::Layer (implements Filter trait)
pub type ReloadableEnvFilter = reload::Layer<EnvFilter, RegistryWithStdout>;

/// Combined filter: Reloadable EnvFilter AND TracingRateLimitLayer
/// EnvFilter is checked first, then TracingRateLimitLayer (only sees events that pass EnvFilter)
#[cfg(feature = "log_throttling")]
pub type CombinedFileFilter = And<ReloadableEnvFilter, TracingRateLimitLayer, RegistryWithStdout>;

/// The file layer type with combined filter (EnvFilter checked first, then throttling)
#[cfg(feature = "log_throttling")]
pub type FileLayerType = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        RegistryWithStdout,
        fmt::format::DefaultFields,
        tracing_subscriber::fmt::format::Format,
        tracing_appender::non_blocking::NonBlocking,
    >,
    CombinedFileFilter,
    RegistryWithStdout,
>;

/// The file layer type without throttling (still uses reloadable EnvFilter)
#[cfg(not(feature = "log_throttling"))]
pub type FileLayerType = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        RegistryWithStdout,
        fmt::format::DefaultFields,
        tracing_subscriber::fmt::format::Format,
        tracing_appender::non_blocking::NonBlocking,
    >,
    ReloadableEnvFilter,
    RegistryWithStdout,
>;

/// Handle for reloading the file EnvFilter at runtime
/// This allows changing the file log level independently of the throttling layer
pub type FileLogReloadHandle = Handle<EnvFilter, RegistryWithStdout>;

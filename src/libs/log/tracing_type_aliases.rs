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

// Type aliases for the concrete layer types used in this module, since generics for client code would be very annoying
pub type StdoutLayerType = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<Registry>,
    EnvFilter,
    Registry,
>;

pub type RegistryWithStdout = tracing_subscriber::layer::Layered<
    tracing_subscriber::reload::Layer<StdoutLayerType, Registry>,
    Registry,
>;

/// The inner file layer filtered by TracingRateLimitLayer (throttling happens after env filtering)
#[cfg(feature = "log_throttling")]
pub type FileLayerThrottled = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        RegistryWithStdout,
        fmt::format::DefaultFields,
        tracing_subscriber::fmt::format::Format,
        tracing_appender::non_blocking::NonBlocking,
    >,
    TracingRateLimitLayer,
    RegistryWithStdout,
>;

/// The complete file layer type: throttled layer wrapped with EnvFilter
#[cfg(feature = "log_throttling")]
pub type FileLayerType = tracing_subscriber::filter::Filtered<
    FileLayerThrottled,
    EnvFilter,
    RegistryWithStdout,
>;

/// The file layer type without throttling
#[cfg(not(feature = "log_throttling"))]
pub type FileLayerType = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        RegistryWithStdout,
        fmt::format::DefaultFields,
        tracing_subscriber::fmt::format::Format,
        tracing_appender::non_blocking::NonBlocking,
    >,
    EnvFilter,
    RegistryWithStdout,
>;

/// The reloadable file logging layer type, wrapping a reload::Layer around FileLayerType
pub type FileLogReloadableLayer = reload::Layer<FileLayerType, RegistryWithStdout>;

/// Handle for reloading the file logging layer at runtime
pub type FileLogReloadHandle = Handle<FileLayerType, RegistryWithStdout>;

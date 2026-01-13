//! Verbose tracing type aliases to simplify core logging code

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;
use tracing_subscriber::{
    fmt, registry,
    reload::{self, Handle},
    EnvFilter, Registry,
};

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

pub type FileLogReloadableLayer = reload::Layer<
    tracing_subscriber::filter::Filtered<FileFilteredTypeT, EnvFilter, FileFilteredTypeS>,
    tracing_subscriber::layer::Layered<
        reload::Layer<
            tracing_subscriber::filter::Filtered<
                fmt::Layer<registry::Registry>,
                EnvFilter,
                registry::Registry,
            >,
            registry::Registry,
        >,
        registry::Registry,
    >,
>;

pub type FileLogReloadHandle = Handle<
    tracing_subscriber::filter::Filtered<FileFilteredTypeT, EnvFilter, FileFilteredTypeS>,
    FileReloadHandleTypeS,
>;

/// Some types to prevent duplication in the above types, and make them a bit easier to read

pub type FileFilteredTypeT = fmt::Layer<
    tracing_subscriber::layer::Layered<
        reload::Layer<
            tracing_subscriber::filter::Filtered<
                fmt::Layer<registry::Registry>,
                EnvFilter,
                registry::Registry,
            >,
            registry::Registry,
        >,
        registry::Registry,
    >,
    fmt::format::DefaultFields,
    fmt::format::Format,
    tracing_appender::non_blocking::NonBlocking,
>;

pub type FileFilteredTypeS = tracing_subscriber::layer::Layered<
    reload::Layer<
        tracing_subscriber::filter::Filtered<
            fmt::Layer<registry::Registry>,
            EnvFilter,
            registry::Registry,
        >,
        registry::Registry,
    >,
    registry::Registry,
>;

pub type FileReloadHandleTypeS = tracing_subscriber::layer::Layered<
    reload::Layer<
        tracing_subscriber::filter::Filtered<
            fmt::Layer<registry::Registry>,
            EnvFilter,
            registry::Registry,
        >,
        registry::Registry,
    >,
    registry::Registry,
>;

// // Alias for extremely verbose type
// type FileLayerType = std::option::Option<
//     tracing_subscriber::filter::Filtered<
//         tracing_subscriber::fmt::Layer<
//             tracing_subscriber::layer::Layered<
//                 tracing_subscriber::filter::Filtered<
//                     tracing_subscriber::fmt::Layer<registry::Registry>,
//                     EnvFilter,
//                     registry::Registry,
//                 >,
//                 registry::Registry,
//             >,
//             fmt::format::DefaultFields,
//             tracing_subscriber::fmt::format::Format,
//             RollingFileAppender,
//         >,
//         EnvFilter,
//         tracing_subscriber::layer::Layered<
//             tracing_subscriber::filter::Filtered<
//                 tracing_subscriber::fmt::Layer<registry::Registry>,
//                 EnvFilter,
//                 registry::Registry,
//             >,
//             registry::Registry,
//         >,
//     >,
// >;

// // for<'writer> std::option::Option<Filtered<tracing_subscriber::fmt::Layer<Layered<Filtered<tracing_subscriber::fmt::Layer<Registry>, EnvFilter, Registry>, Registry>, DefaultFields, tracing_subscriber::fmt::format::Format, RollingFileAppender>, EnvFilter, Layered<Filtered<tracing_subscriber::fmt::Layer<Registry>, EnvFilter, Registry>, Registry>>>: MakeWriter<'writer>

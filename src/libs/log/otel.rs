//! OpenTelemetry log and trace forwarding layer
//!
//! This module provides a separate tracing layer that forwards all log events and span lifecycle
//! to an OpenTelemetry collector via OTLP. It operates independently from the existing stdout
//! and file logging layers.
//!
//! # Configuration
//! OTel can be configured via [`OtelConfig`] or through standard environment variables:
//! - `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` - OTLP collector endpoint for traces
//! - `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` - OTLP collector endpoint for logs
//! - `OTEL_SERVICE_NAME` - Service name for traces
//! - `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` - Protocol (grpc/http)
//! - `OTEL_EXPORTER_OTLP_HEADERS` - Additional headers (e.g., auth tokens)
//!
//! # Graceful Degradation
//! If the OTel layer fails to initialize (e.g., invalid endpoint, network issues), a warning
//! is logged and the application continues with stdout/file logging only.

use std::collections::HashMap;

use opentelemetry::propagation::{TextMapCompositePropagator, TextMapPropagator};
use opentelemetry_otlp::{WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::{SdkTracerProvider, Tracer},
};
use opentelemetry_semantic_conventions::resource::SERVICE_VERSION;

/// Protocol for OTLP exporter
#[derive(Debug, Clone, Default)]
pub enum OtelProtocol {
    /// gRPC protocol (recommended)
    #[default]
    Grpc,
    /// HTTP with protobuf encoding
    HttpProtobuf,
}

/// Configuration for OpenTelemetry integration
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// Whether to enable OTel log/trace forwarding
    pub enabled: bool,
    /// Service name to identify this application
    pub service_name: Option<String>,
    /// OTLP collector endpoint (e.g., "http://localhost:4317")
    pub endpoint: Option<String>,
    /// Protocol to use for OTLP export
    pub protocol: OtelProtocol,
    /// Additional headers to include in OTLP requests (e.g., authentication)
    pub headers: HashMap<String, String>,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service_name: None,
            endpoint: None,
            protocol: OtelProtocol::default(),
            headers: HashMap::new(),
        }
    }
}

/// Guards for OpenTelemetry providers to ensure traces and logs are flushed on drop
pub struct OtelGuards {
    /// The tracer provider guard
    pub tracer_provider: SdkTracerProvider,
    /// The logger provider guard
    pub logger_provider: SdkLoggerProvider,
}

impl Drop for OtelGuards {
    fn drop(&mut self) {
        tracing::info!(target: "otel::setup", "OTel layer shutting down - flushing pending traces and logs");
    }
}

/// Result of building the OTel layer
pub struct OtelLayerResult {
    /// The guards that must be kept alive to ensure traces/logs are flushed on drop
    pub guards: Option<OtelGuards>,
    /// The tracer for OpenTelemetryLayer
    pub tracer: Option<Tracer>,
}

/// Build OpenTelemetry tracer and logger providers for forwarding logs and spans
pub fn build_otel_layer(config: &OtelConfig) -> OtelLayerResult {
    if !config.enabled {
        tracing::debug!(target: "otel::setup", "OTel layer disabled by config");
        return OtelLayerResult {
            guards: None,
            tracer: None,
        };
    }

    match build_otel_layer_inner(config) {
        Ok(result) => {
            tracing::info!(
                target: "otel::setup",
                service_name = result.service_name,
                endpoint = ?config.endpoint,
                "OTel layer initialized successfully (Traces + Logs)"
            );
            tracing::debug!(
                target: "otel::setup",
                protocol = ?config.protocol,
                endpoint = config.endpoint.as_deref().unwrap_or("SDK default"),
                header_keys = ?config.headers.keys().collect::<Vec<_>>(),
                "OTel exporter config"
            );
            OtelLayerResult {
                guards: Some(OtelGuards {
                    tracer_provider: result.tracer_provider,
                    logger_provider: result.logger_provider,
                }),
                tracer: Some(result.tracer),
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "otel::setup",
                error = %e,
                "Failed to initialize OTel layer - continuing without OTel forwarding"
            );
            OtelLayerResult {
                guards: None,
                tracer: None,
            }
        }
    }
}

struct OtelLayerBuild {
    tracer_provider: SdkTracerProvider,
    logger_provider: SdkLoggerProvider,
    tracer: Tracer,
    service_name: String,
}

fn build_otel_layer_inner(config: &OtelConfig) -> Result<OtelLayerBuild, Box<dyn std::error::Error + Send + Sync>> {
    let service_name = config
        .service_name
        .clone()
        .or_else(|| std::env::var("OTEL_SERVICE_NAME").ok())
        .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string());

    let resource = Resource::builder()
        .with_service_name(service_name.clone())
        .with_attribute(opentelemetry::KeyValue::new(
            SERVICE_VERSION,
            env!("CARGO_PKG_VERSION"),
        ))
        .build();

    let tracer_provider = build_tracer_provider(&resource, config)?;
    let logger_provider = build_logger_provider(&resource, config)?;

    init_propagator();

    let tracer = opentelemetry::trace::TracerProvider::tracer(&tracer_provider, service_name.clone());

    Ok(OtelLayerBuild {
        tracer_provider,
        logger_provider,
        tracer,
        service_name,
    })
}

fn build_tracer_provider(
    resource: &Resource,
    config: &OtelConfig,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};

    let endpoint = config
        .endpoint
        .clone()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok());

    let mut builder = SdkTracerProvider::builder().with_resource(resource.clone());

    match config.protocol {
        OtelProtocol::Grpc => {
            tokio::runtime::Handle::try_current()
                .map_err(|_| "gRPC OTel exporter requires a running Tokio runtime; call setup_logging inside a tokio context or use OtelProtocol::HttpProtobuf")?;
            let mut exporter_builder = SpanExporter::builder().with_tonic();
            if let Some(ref ep) = endpoint {
                exporter_builder = exporter_builder.with_endpoint(ep);
            }
            if !config.headers.is_empty() {
                let mut metadata = tonic::metadata::MetadataMap::new();
                for (key, value) in &config.headers {
                    if let (Ok(meta_key), Ok(meta_value)) = (
                        tonic::metadata::MetadataKey::from_bytes(key.as_bytes()),
                        value.parse(),
                    ) {
                        metadata.insert(meta_key, meta_value);
                    }
                }
                exporter_builder = exporter_builder.with_metadata(metadata);
            }
            builder = builder.with_batch_exporter(exporter_builder.build()?);
        }
        OtelProtocol::HttpProtobuf => {
            let mut exporter_builder = SpanExporter::builder().with_http();
            if let Some(ref ep) = endpoint {
                exporter_builder = exporter_builder.with_endpoint(ep);
            }
            if !config.headers.is_empty() {
                exporter_builder = exporter_builder.with_headers(config.headers.clone());
            }
            builder = builder.with_batch_exporter(exporter_builder.build()?);
        }
    }

    Ok(builder.build())
}

fn build_logger_provider(
    resource: &Resource,
    config: &OtelConfig,
) -> Result<SdkLoggerProvider, Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{LogExporter, WithExportConfig};

    let endpoint = config
        .endpoint
        .clone()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").ok())
        .or_else(|| config.endpoint.clone())
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok());

    let mut builder = SdkLoggerProvider::builder().with_resource(resource.clone());

    match config.protocol {
        OtelProtocol::Grpc => {
            tokio::runtime::Handle::try_current()
                .map_err(|_| "gRPC OTel exporter requires a running Tokio runtime; call setup_logging inside a tokio context or use OtelProtocol::HttpProtobuf")?;
            let mut exporter_builder = LogExporter::builder().with_tonic();
            if let Some(ref ep) = endpoint {
                exporter_builder = exporter_builder.with_endpoint(ep);
            }
            if !config.headers.is_empty() {
                let mut metadata = tonic::metadata::MetadataMap::new();
                for (key, value) in &config.headers {
                    if let (Ok(meta_key), Ok(meta_value)) = (
                        tonic::metadata::MetadataKey::from_bytes(key.as_bytes()),
                        value.parse(),
                    ) {
                        metadata.insert(meta_key, meta_value);
                    }
                }
                exporter_builder = exporter_builder.with_metadata(metadata);
            }
            builder = builder.with_batch_exporter(exporter_builder.build()?);
        }
        OtelProtocol::HttpProtobuf => {
            let mut exporter_builder = LogExporter::builder().with_http();
            if let Some(ref ep) = endpoint {
                exporter_builder = exporter_builder.with_endpoint(ep);
            }
            if !config.headers.is_empty() {
                exporter_builder = exporter_builder.with_headers(config.headers.clone());
            }
            builder = builder.with_batch_exporter(exporter_builder.build()?);
        }
    }

    Ok(builder.build())
}

fn init_propagator() {
    let value = std::env::var("OTEL_PROPAGATORS").unwrap_or_else(|_| "tracecontext,baggage".to_string());
    let mut propagators: Vec<(Box<dyn TextMapPropagator + Send + Sync>, String)> = Vec::new();

    for name in value.split(',').map(|s| s.trim().to_lowercase()) {
        match name.as_str() {
            "tracecontext" => propagators.push((Box::new(TraceContextPropagator::new()), name)),
            "baggage" => propagators.push((Box::new(BaggagePropagator::new()), name)),
            _ => {}
        }
    }

    if !propagators.is_empty() {
        let (propagators_impl, _): (Vec<_>, Vec<_>) = propagators.into_iter().unzip();
        opentelemetry::global::set_text_map_propagator(TextMapCompositePropagator::new(propagators_impl));
    }
}

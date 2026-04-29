//! Connect to a WebSocket server using WsClient — the endpoint-libs client.
//! Sends each stdin line as an EchoRequest and prints the JSON response.
//!
//! Usage: cargo run --example ws_echo_ws_client --features ws -- <server_url>
//! e.g.:  cargo run --example ws_echo_ws_client --features ws -- wss://ws-echo.liftmap.pro

use std::io::{self, BufRead};

#[cfg(feature = "error_aggregation")]
use endpoint_libs::libs::log::error_aggregation::ErrorAggregationConfig;
use endpoint_libs::libs::{
    log::{LogLevel, LoggingConfig, OtelConfig, setup_logging},
    ws::WsClientBuilder,
};
use rustls::crypto::ring;
use serde::Serialize;

const METHOD_ECHO: u32 = 1;

#[derive(Serialize)]
struct EchoRequest<'a> {
    message: &'a str,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _log = setup_logging(LoggingConfig {
        level: LogLevel::Debug,
        otel_config: OtelConfig::default(),
        file_config: None,
        #[cfg(feature = "error_aggregation")]
        error_aggregation: ErrorAggregationConfig {
            limit: 100,
            normalize: true,
        },
        #[cfg(feature = "log_throttling")]
        throttling_config: None,
    })?;

    ring::default_provider()
        .install_default()
        .expect("Could not install default crypto provider");

    let server = std::env::args()
        .nth(1)
        .expect("Usage: ws_echo_ws_client <server_url>");

    tracing::info!("Connecting to {server} via WsClient...");
    #[cfg(any(
        feature = "ws-http1",
        all(feature = "ws-wtx", not(feature = "ws-wtx-http2"))
    ))]
    let mode = endpoint_libs::libs::ws::WsVersionMode::Http1Only;
    #[cfg(not(any(
        feature = "ws-http1",
        all(feature = "ws-wtx", not(feature = "ws-wtx-http2"))
    )))]
    let mode = endpoint_libs::libs::ws::WsVersionMode::Http2Only;
    let (mut client, _) = WsClientBuilder::new()
        .mode(mode)
        .danger_accept_invalid_certs()
        .build(&server)
        .await?;
    tracing::info!("Connected. Type a message and press Enter.");

    for line in io::stdin().lock().lines() {
        let message = line?;
        if message.is_empty() {
            continue;
        }
        client
            .send_req(METHOD_ECHO, &EchoRequest { message: &message })
            .await?;

        let raw = client.recv_raw().await?;
        println!("{}", serde_json::to_string(&raw)?);
    }

    Ok(())
}

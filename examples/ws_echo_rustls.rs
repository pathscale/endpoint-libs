//! Connect to a WebSocket echo server using rustls — same TLS stack as the production handlers.
//!
//! Usage: cargo run --example ws_echo_rustls --features ws -- <server_url> [protocol]
//! e.g.:  cargo run --example ws_echo_rustls --features ws -- wss://ws-echo.liftmap.pro

use std::io::{self, BufRead};

use endpoint_libs::libs::log::{LogLevel, LoggingConfig, OtelConfig, setup_logging};
#[cfg(feature = "error_aggregation")]
use endpoint_libs::libs::log::error_aggregation::ErrorAggregationConfig;
use futures::{SinkExt, StreamExt};
use rustls::crypto::ring;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::connect_async;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _log = setup_logging(LoggingConfig {
        level: LogLevel::Info,
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

    let mut args = std::env::args().skip(1);
    let server = args.next().expect("Usage: ws_echo_rustls <server_url> [protocol]");
    let protocol = args.next().unwrap_or_default();

    let mut req = server.as_str().into_client_request()?;
    if !protocol.is_empty() {
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", protocol.parse()?);
    }

    tracing::info!("Connecting to {server} via rustls...");
    let (mut stream, _) = connect_async(req).await?;
    tracing::info!("Connected. Type a message and press Enter.");

    for line in io::stdin().lock().lines() {
        let message = line?;
        if message.is_empty() {
            continue;
        }
        tracing::info!("-> {message}");
        stream.send(Message::Text(message.into())).await?;

        if let Some(msg) = stream.next().await {
            let msg: Message = msg?;
            println!("{}", msg.to_text()?);
        }
    }

    Ok(())
}

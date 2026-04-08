//! Connect to a WebSocket server using WsClient — the endpoint-libs client.
//! Sends each stdin line as an EchoRequest and prints the JSON response.
//!
//! Usage: cargo run --example ws_echo_ws_client --features ws -- <server_url>
//! e.g.:  cargo run --example ws_echo_ws_client --features ws -- wss://ws-echo.liftmap.pro

use std::io::{self, BufRead};

use endpoint_libs::libs::ws::WsClient;
use rustls::crypto::ring;
use serde::Serialize;

const METHOD_ECHO: u32 = 1;

#[derive(Serialize)]
struct EchoRequest<'a> {
    message: &'a str,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    ring::default_provider()
        .install_default()
        .expect("Could not install default crypto provider");

    let server = std::env::args()
        .nth(1)
        .expect("Usage: ws_echo_ws_client <server_url>");

    eprintln!("Connecting to {server} via WsClient...");
    let (mut client, _) = WsClient::new(&server, "", None).await?;
    eprintln!("Connected. Type a message and press Enter.");

    for line in io::stdin().lock().lines() {
        let message = line?;
        if message.is_empty() {
            continue;
        }
        client.send_req(METHOD_ECHO, &EchoRequest { message: &message }).await?;

        let raw = client.recv_raw().await?;
        println!("{}", serde_json::to_string(&raw)?);
    }

    Ok(())
}

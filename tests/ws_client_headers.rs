#![cfg(feature = "ws-client")]

use endpoint_libs::libs::ws::{WsClient, WsClientBuilder, WsTarget};
use nago_wss::proto::message::Limits;
use nago_wss::upgrade::{self, UpgradeOutcome};
use nagoya::reactor::{Addr, Reactor, TcpListener, block_on_with};

#[test]
fn custom_subprotocol_is_sent_once_through_the_upgrade() {
    const PROTOCOL: &str = "0test,  1parameter";
    let reactor = Reactor::local().unwrap();
    let handle = reactor.handle();
    let listener = TcpListener::bind(Addr::localhost(0), &handle).unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("ws://127.0.0.1:{port}/");
    let target = WsTarget::resolve(&url).unwrap();

    block_on_with(&reactor, async {
        for use_constructor in [true, false] {
            let client = async {
                if use_constructor {
                    WsClient::new(
                        &url,
                        "",
                        Some(vec![
                            ("sEc-WebSocket-pRoToCoL", PROTOCOL),
                            ("X-Request-ID", "test"),
                        ]),
                        &handle,
                    )
                    .await
                } else {
                    WsClientBuilder::new()
                        .protocol_header(PROTOCOL)
                        .owned_header("sec-websocket-protocol".into(), PROTOCOL.into())
                        .header("Sec-WebSocket-Protocol", PROTOCOL)
                        .header("X-Request-ID", "test")
                        .connect(&target, &handle)
                        .await
                }
            };
            let peer = async {
                let (socket, _) = listener.accept().await?;
                let outcome = upgrade::accept(socket, Limits::default(), &[], |request| {
                    (request.protocols.first().cloned(), Vec::new())
                })
                .await
                .map_err(|error| eyre::eyre!("peer upgrade failed: {error}"))?;
                let UpgradeOutcome::Upgraded(_, request) = outcome else {
                    eyre::bail!("expected a WebSocket upgrade");
                };
                Ok::<_, eyre::Report>(request)
            };
            let ((_, response), request) = futures::try_join!(client, peer)?;
            let values = |name: &str| {
                request
                    .headers
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>()
            };
            assert_eq!(values("Sec-WebSocket-Protocol"), [PROTOCOL]);
            assert_eq!(values("X-Request-ID"), ["test"]);
            assert_eq!(response.protocol.as_deref(), Some("0test"));
        }
        Ok::<_, eyre::Report>(())
    })
    .unwrap();
}

#[test]
fn conflicting_subprotocol_and_reserved_fields_fail_before_connect() {
    let reactor = Reactor::local().unwrap();
    let handle = reactor.handle();
    let listener = TcpListener::bind(Addr::localhost(0), &handle).unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = WsTarget::resolve(&format!("ws://127.0.0.1:{port}/")).unwrap();
    let invalid_head = "invalid WebSocket request head";
    let conflicting = "conflicting Sec-WebSocket-Protocol values";

    block_on_with(&reactor, async {
        for (builder, expected) in [
            (
                WsClientBuilder::new()
                    .protocol_header("0configured")
                    .owned_header("sEc-WebSocket-Protocol".into(), "0other".into()),
                conflicting,
            ),
            (
                WsClientBuilder::new().headers(vec![
                    ("sec-websocket-protocol", "0first"),
                    ("SEC-WEBSOCKET-PROTOCOL", "0second"),
                ]),
                conflicting,
            ),
            (WsClientBuilder::new().header("hOsT", "other"), invalid_head),
            (
                WsClientBuilder::new().header("Transfer-Encoding", "chunked"),
                invalid_head,
            ),
            (
                WsClientBuilder::new().header("Sec-WebSocket-Extensions", "permessage-deflate"),
                invalid_head,
            ),
            (
                WsClientBuilder::new().header("Sec-WebSocket-Protocol", "0test\r\nHost: other"),
                invalid_head,
            ),
        ] {
            let result = builder
                .pre_upgrade_verifier(|_, _| async {
                    panic!("invalid headers reached a connected socket")
                })
                .connect(&target, &handle)
                .await;
            let error = result.err().expect("invalid request should fail");
            assert_eq!(error.to_string(), expected);
        }
    });
}

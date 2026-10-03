#![cfg(feature = "ws-client-tls")]
use endpoint_libs::libs::ws::WsClientBuilder;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    time::Duration,
};

// A successful TLS handshake reaches the HTTP rejection. Certificate failures
// must stop before the upgrade and must never switch to default trust anchors.
#[test]
fn injected_wss_policy_validates_the_chain_and_host() {
    use nago_wss::tls::rustls;
    let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = identity.cert.der().clone();
    let server = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der())
                    .into(),
            )
            .unwrap(),
    );
    for (host, approved) in [
        ("localhost", true),
        ("127.0.0.1", true),
        ("localhost", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = server.clone();
        let worker = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut tls =
                rustls::StreamOwned::new(rustls::ServerConnection::new(server).unwrap(), socket);
            let mut request = [0u8; 4096];
            if tls.read(&mut request).is_ok() {
                let _ = tls.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
                let _ = tls.flush();
            }
        });
        let mut roots = rustls::RootCertStore::empty();
        if approved {
            roots.add(cert.clone()).unwrap();
        }
        let config = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let reactor = nagoya::reactor::Reactor::local().unwrap();
        let handle = reactor.handle();
        let result = nagoya::reactor::block_on_with(
            &reactor,
            WsClientBuilder::new()
                .tls_config(config)
                .build(&format!("wss://{host}:{}/", address.port()), &handle),
        );
        let error = result
            .err()
            .expect("test server rejects the HTTP upgrade")
            .to_string();
        assert!(
            error.contains(if host == "localhost" && approved {
                "WebSocket upgrade failed"
            } else {
                "TLS handshake failed"
            }),
            "{error}"
        );
        worker.join().unwrap();
    }
}

#[test]
fn explicit_policy_cannot_be_bypassed() {
    use nago_wss::tls::rustls;
    let config = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth(),
    );
    let reactor = nagoya::reactor::Reactor::local().unwrap();
    let handle = reactor.handle();
    let result = nagoya::reactor::block_on_with(
        &reactor,
        WsClientBuilder::new()
            .danger_accept_invalid_certs()
            .tls_config(config)
            .build("wss://localhost:1/", &handle),
    );
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("explicit TLS policy")
    );
}

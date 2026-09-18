//! The framed transport carried over a Nagoya Unix socket, with no tokio anywhere.
//!
//! The claim under test is that `framed_json_neutral` is genuinely runtime-neutral: the
//! same wire format reaches a peer over a socket driven by Nagoya's reactor, on Nagoya's
//! executor, in a build that does not start a tokio runtime. A compile alone would not
//! show that, because the adapter's whole job is readiness, so the message has to travel.
//!
//! If this file fails to compile, the neutral seam has regressed to naming a runtime.

#![cfg(feature = "nagoya-transport")]

use std::os::unix::ffi::OsStrExt;

use endpoint_libs::libs::ws::WireMessage;
use endpoint_libs::libs::ws::transport::framed::framed_json_neutral;
use endpoint_libs::libs::ws::transport::nagoya::NagoyaStream;
use futures::{SinkExt, StreamExt};
use nagoya::reactor::{Addr, Reactor, TaskSet, TcpListener, TcpStream, block_on_with};

/// A socket path that removes itself, so a failed run does not poison the next one.
struct SocketPath(std::path::PathBuf);

impl SocketPath {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("elibs-nagoya-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_wire_message_crosses_a_nagoya_unix_socket_in_both_directions() {
    let path = SocketPath::new("roundtrip");
    let addr = Addr::path(path.0.as_os_str().as_bytes()).expect("the path fits a sun_path");

    let reactor = Reactor::local().expect("a reactor");
    let handle = reactor.handle();
    let listener = TcpListener::bind(addr, &handle).expect("bind");

    // Connected before the executor starts, so neither task has to poll for the other's
    // existence: a Unix-domain connect finishes inside the call rather than by becoming
    // writable later.
    let client_socket =
        nagoya::reactor::socket::TcpSocket::connect(addr).expect("connect to the listener");
    let client = TcpStream::from_socket(client_socket, &handle).expect("register the client");

    let request = WireMessage::Text("what did you index".into());
    let reply = WireMessage::Binary(vec![0, 1, 2, 255].into());

    let expected_request = request.clone();
    let expected_reply = reply.clone();

    let mut tasks = TaskSet::new();
    tasks.push(async move {
        let (server, _) = listener.accept().await.expect("accept");
        let mut framed = framed_json_neutral(NagoyaStream::new(server));
        let received = framed
            .next()
            .await
            .expect("a frame arrives")
            .expect("it decodes");
        assert_eq!(received, expected_request);
        framed.send(expected_reply).await.expect("send the reply");
        framed.close().await.expect("close cleanly");
    });
    tasks.push(async move {
        let mut framed = framed_json_neutral(NagoyaStream::new(client));
        framed.send(request).await.expect("send the request");
        let received = framed
            .next()
            .await
            .expect("a reply arrives")
            .expect("it decodes");
        assert_eq!(received, reply);
    });

    block_on_with(&reactor, tasks);
}

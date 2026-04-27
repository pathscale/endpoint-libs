use std::net::SocketAddr;

use eyre::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;

pub trait ConnectionListener: Send + Sync + Unpin {
    type Channel1: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static;
    type Channel2: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static;

    fn accept(&self) -> BoxFuture<'_, Result<(Self::Channel1, SocketAddr)>>;
    fn handshake(&self, channel: Self::Channel1) -> BoxFuture<'_, Result<Self::Channel2>>;
}

pub struct TcpListener {
    listener: tokio::net::TcpListener,
}
impl TcpListener {
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }
}
impl ConnectionListener for TcpListener {
    type Channel1 = TcpStream;
    type Channel2 = TcpStream;

    fn accept(&self) -> BoxFuture<'_, Result<(Self::Channel1, SocketAddr)>> {
        async {
            let (stream, addr) = self.listener.accept().await?;
            Ok((stream, addr))
        }
        .boxed()
    }
    fn handshake(&self, channel: Self::Channel1) -> BoxFuture<'_, Result<Self::Channel2>> {
        async move { Ok(channel) }.boxed()
    }
}

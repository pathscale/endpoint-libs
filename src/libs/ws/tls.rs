use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::{Context, Result, ensure};
use futures::FutureExt;
use futures::future::BoxFuture;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls_pemfile::certs;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::ConnectionListener;

pub struct TlsListener<T> {
    tcp: T,
    acceptor: TlsAcceptor,
}

impl<T: ConnectionListener> TlsListener<T> {
    pub async fn bind(under: T, pub_certs: Vec<PathBuf>, priv_cert: PathBuf) -> Result<Self> {
        let certs = load_certs(&pub_certs)?;
        ensure!(
            !certs.is_empty(),
            "No certificates found in file: {:?}",
            pub_certs
        );

        let key = load_private_key(&priv_cert)?;

        #[cfg(feature = "ws-tls12")]
        let protocol_versions: &[&rustls::SupportedProtocolVersion] =
            { &[&rustls::version::TLS13, &rustls::version::TLS12] };
        #[cfg(not(feature = "ws-tls12"))]
        let protocol_versions: &[&rustls::SupportedProtocolVersion] =
            { &[&rustls::version::TLS13] };
        #[cfg(feature = "ws-wtx")]
        let protocol_versions: &[&rustls::SupportedProtocolVersion] =
            { &[&rustls::version::TLS12] };

        #[cfg(any(feature = "ws-http1", feature = "ws-wtx"))]
        let alpn_protocols = { vec![b"h2".to_vec(), b"http/1.1".to_vec()] };
        #[cfg(not(any(feature = "ws-http1", feature = "ws-wtx")))]
        let alpn_protocols = { vec![b"h2".to_vec()] };

        let tls_cfg = {
            let mut cfg = rustls::ServerConfig::builder_with_protocol_versions(protocol_versions)
                .with_no_client_auth()
                .with_single_cert(certs, key)?;
            cfg.alpn_protocols = alpn_protocols;
            Arc::new(cfg)
        };
        let acceptor = TlsAcceptor::from(tls_cfg);
        Ok(Self {
            tcp: under,
            acceptor,
        })
    }
}

impl<T: ConnectionListener + 'static> ConnectionListener for TlsListener<T> {
    type Channel1 = T::Channel1;
    type Channel2 = TlsStream<T::Channel2>;
    fn accept(&self) -> BoxFuture<'_, Result<(Self::Channel1, SocketAddr)>> {
        self.tcp.accept()
    }
    fn handshake(&self, channel: Self::Channel1) -> BoxFuture<'_, Result<Self::Channel2>> {
        async {
            let channel = self.tcp.handshake(channel).await?;
            let tls_stream = self.acceptor.accept(channel).await?;
            Ok(tls_stream)
        }
        .boxed()
    }
}

fn load_certs<'a, T: AsRef<Path>>(
    path: impl IntoIterator<Item = T>,
) -> Result<Vec<CertificateDer<'a>>> {
    let mut r_certs = vec![];
    for p in path {
        let p = p.as_ref();
        let f = File::open(p).with_context(|| format!("Failed to open {}", p.display()))?;

        let reader = &mut std::io::BufReader::new(f);
        let certs_results = certs(reader);

        let certs: Vec<CertificateDer> = certs_results.filter_map(|result| result.ok()).collect();

        r_certs.extend(certs);
    }
    Ok(r_certs)
}

fn load_private_key(path: &PathBuf) -> Result<PrivateKeyDer<'static>> {
    let private_key =
        PrivateKeyDer::from_pem_file(path).wrap_err("Error loading private key from file.")?;

    Ok(private_key)
}

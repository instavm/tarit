use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::middleware;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{watch, OwnedSemaphorePermit, Semaphore},
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::TlsAcceptor;

use crate::acme::resolver::CertResolver;

const MAX_CONCURRENT_TLS_CONNECTIONS: usize = 1024;
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TLS_ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
const TLS_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

struct PermittedStream<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
}

impl<S: AsyncRead + Unpin> AsyncRead for PermittedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PermittedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct TlsInfo {
    pub sni: Option<String>,
}

pub fn server_config(resolver: Arc<CertResolver>) -> Arc<rustls::ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

pub fn spawn_tls_server(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    app: axum::Router,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let acceptor = TlsAcceptor::from(config);
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_TLS_CONNECTIONS));
        let mut connections = JoinSet::new();

        if shutdown_rx.borrow().is_some() {
            return;
        }

        loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || shutdown_rx.borrow().is_some() {
                        break;
                    }
                }
                Some(_) = connections.join_next() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let permit = match Arc::clone(&permits).try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tracing::warn!("TLS connection limit reached; dropping connection");
                                continue;
                            }
                        };
                        connections.spawn(serve_tls_connection(
                            acceptor.clone(),
                            stream,
                            app.clone(),
                            shutdown_rx.clone(),
                            permit,
                        ));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to accept TLS connection");
                        tokio::time::sleep(TLS_ACCEPT_ERROR_BACKOFF).await;
                    }
                },
            }
        }

        if tokio::time::timeout(TLS_DRAIN_TIMEOUT, async {
            while connections.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
    })
}

async fn serve_tls_connection(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    app: axum::Router,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
    permit: OwnedSemaphorePermit,
) {
    let tls_stream =
        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
            Ok(Ok(tls_stream)) => tls_stream,
            Ok(Err(error)) => {
                tracing::debug!(%error, "TLS handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!("TLS handshake timed out");
                return;
            }
        };
    let sni = tls_stream.get_ref().1.server_name().map(str::to_owned);
    let app_for_connection = app.layer(middleware::from_fn(
        move |mut request: axum::extract::Request, next: middleware::Next| {
            let sni = sni.clone();
            async move {
                request.extensions_mut().insert(TlsInfo { sni });
                next.run(request).await
            }
        },
    ));
    let io = hyper_util::rt::TokioIo::new(PermittedStream {
        inner: tls_stream,
        _permit: permit,
    });
    let service = hyper_util::service::TowerToHyperService::new(app_for_connection);
    let builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    let connection = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                tracing::debug!(%error, "TLS connection error");
            }
        }
        changed = shutdown_rx.changed() => {
            if changed.is_ok() && shutdown_rx.borrow().is_some() {
                connection.as_mut().graceful_shutdown();
                if let Err(error) = connection.await {
                    tracing::debug!(%error, "TLS connection error during graceful shutdown");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcgen::generate_simple_self_signed;
    use rustls::{
        crypto::ring::sign::any_supported_type,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
        sign::CertifiedKey,
    };

    use crate::acme::resolver::{CertResolver, CertStore};

    use super::{server_config, spawn_tls_server};

    fn self_signed_wildcard() -> Arc<CertifiedKey> {
        let certificate = generate_simple_self_signed(vec![
            "*.shares.example.com".into(),
            "shares.example.com".into(),
        ])
        .unwrap();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certificate.signing_key.serialize_der(),
        ));
        let signing_key = any_supported_type(&private_key).unwrap();

        Arc::new(CertifiedKey::new(
            vec![certificate.cert.der().clone()],
            signing_key,
        ))
    }

    #[tokio::test]
    async fn tls_server_serves_axum_router_over_https() {
        let resolver = CertResolver::new();
        resolver.install(CertStore::from_wildcard(
            "shares.example.com",
            self_signed_wildcard(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let (_tx, rx) = tokio::sync::watch::channel(None);
        let _handle = spawn_tls_server(listener, server_config(resolver), app, rx);
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .resolve("x.shares.example.com", addr)
            .build()
            .unwrap();

        let body = client
            .get("https://x.shares.example.com/")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(body, "ok");
    }
}

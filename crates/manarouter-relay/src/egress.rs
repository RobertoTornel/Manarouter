//! The connector that puts a request on the wire through a proxy.
//!
//! The tunnel code in [`crate::proxy`] knows how to speak HTTP `CONNECT`, SOCKS4 and SOCKS5 to a
//! proxy and how to reach an origin through it. What was missing was the other half: a
//! **connector** hyper can use, so that a proxied request is dialled, tunnelled and — where the
//! origin is `https://` — encrypted to the *origin*, not to the proxy.
//!
//! # Why the connector is per proxy, and not per request
//!
//! A connection pool is keyed by origin. If one pool held both direct and proxied connections to
//! the same provider, a request that was supposed to leave through a proxy could be handed a
//! direct connection that was already open — and the request would leave from the machine
//! instead of from the proxy, which is exactly the leak an egress policy exists to prevent.
//!
//! So each proxy gets **its own client, and therefore its own pool**. A direct request uses the
//! direct client. A proxied request uses the client of its proxy, and there is no path by which a
//! proxied request can end up on a direct connection.
//!
//! # Why the failure is loud
//!
//! When a tunnel fails, the request fails. There is no fallback to a direct connection: an
//! operator who configured a proxy has said where the traffic should go, and a gateway that quietly
//! routes around a broken proxy is not honouring that.
//!
//! # The two TLS hops
//!
//! An `https://` origin through a proxy is two TLS hops, and they are not the same handshake:
//!
//! * the **proxy hop** is TLS when the proxy's own URL is `https://`, and the certificate is
//!   verified against the *proxy's* name;
//! * the **origin hop** is TLS for an `https://` origin, and the certificate is verified against
//!   the *origin's* name, with the provider's ALPN offer.
//!
//! Getting that order wrong is how a proxy ends up seeing the origin's plaintext, so the two are
//! separate variants here rather than one flag.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tower::Service;

use crate::proxy::{open_tunnel, ProxyEndpoint, ProxyError, ProxyTarget, Tunnel, TunnelLimits};

/// A stream that reaches an origin, possibly through a proxy.
///
/// The variants are the four combinations of "is the proxy hop encrypted" and "is the origin hop
/// encrypted". An enum rather than a boxed trait object because hyper's `TokioIo` needs the stream
/// to be `Unpin` and sized, and because a reader of this type should be able to see which hops are
/// encrypted without unwrapping a pointer.
#[derive(Debug)]
pub enum EgressStream {
    /// A plaintext origin over a plaintext proxy hop.
    Plain(TcpStream),
    /// A plaintext origin over an encrypted proxy hop.
    ProxyTls(Box<TlsStream<TcpStream>>),
    /// An encrypted origin over a plaintext proxy hop.
    OriginTls(Box<TlsStream<TcpStream>>),
    /// An encrypted origin over an encrypted proxy hop.
    OriginTlsOverProxyTls(Box<TlsStream<TlsStream<TcpStream>>>),
}

impl EgressStream {
    /// Whether the hop to the origin is encrypted.
    pub const fn origin_is_encrypted(&self) -> bool {
        matches!(self, Self::OriginTls(_) | Self::OriginTlsOverProxyTls(_))
    }
}

impl hyper_util::client::legacy::connect::Connection for EgressStream {
    /// The default metadata: no ALPN was negotiated by this layer, and the connection is a
    /// proxied one.
    ///
    /// `Connected::proxy` is not used because it exists to tell hyper that the *authority* on the
    /// wire is a proxy's rather than the origin's, which is not the case here: the tunnel carries
    /// the origin's own `CONNECT` target, and the request hyper writes is the origin's request.
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

impl AsyncRead for EgressStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Self::OriginTls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Self::OriginTlsOverProxyTls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for EgressStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Self::OriginTls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Self::OriginTlsOverProxyTls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Self::OriginTls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Self::OriginTlsOverProxyTls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            Self::OriginTls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            Self::OriginTlsOverProxyTls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The connector hyper dials through a proxy with.
///
/// It is built per proxy and cloned into the pool, so the pool it serves never holds a connection
/// that belongs to a different proxy.
#[derive(Clone)]
pub struct ProxyConnector {
    /// The proxy to dial through.
    endpoint: Arc<ProxyEndpoint>,
    /// How long the tunnel handshake may take.
    limits: TunnelLimits,
    /// TLS to the origin, with the provider's ALPN offer.
    origin_tls: tokio_rustls::TlsConnector,
    /// TLS to the proxy, for an `https://` proxy.
    ///
    /// It must not offer `h2`: `CONNECT` is HTTP/1.1, and offering `h2` to a proxy and then
    /// speaking HTTP/1.1 over it is a protocol violation some proxies answer by hanging up.
    proxy_tls: Option<tokio_rustls::TlsConnector>,
}

impl std::fmt::Debug for ProxyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The endpoint, but not the credential: a `Debug` that prints a password is a password in
        // whatever log it is printed into.
        f.debug_struct("ProxyConnector")
            .field("endpoint", &self.endpoint.host)
            .field("port", &self.endpoint.port)
            .finish()
    }
}

impl ProxyConnector {
    /// A connector that dials through `endpoint`.
    ///
    /// `origin_tls` is the connector used for the origin hop; it must carry the provider's ALPN
    /// offer, because the origin — not the proxy — is what speaks HTTP/2.
    pub fn new(
        endpoint: ProxyEndpoint,
        limits: TunnelLimits,
        origin_tls: tokio_rustls::TlsConnector,
        proxy_tls: Option<tokio_rustls::TlsConnector>,
    ) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
            limits,
            origin_tls,
            proxy_tls,
        }
    }

    /// The proxy this connector dials.
    pub fn endpoint(&self) -> &ProxyEndpoint {
        &self.endpoint
    }
}

impl Service<Uri> for ProxyConnector {
    type Response = TokioIo<EgressStream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The tunnel is opened per connection, so there is nothing to reserve here.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let endpoint = Arc::clone(&self.endpoint);
        let limits = self.limits;
        let origin_tls = self.origin_tls.clone();
        let proxy_tls = self.proxy_tls.clone();

        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("the request URI {uri} has no host to tunnel to").into()
                })?
                .to_owned();
            let secure = uri.scheme_str() == Some("https");
            let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
            let target = ProxyTarget::new(host.clone(), port);

            let tunnel = open_tunnel(&endpoint, &target, &limits, proxy_tls.as_ref())
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

            let stream = match tunnel {
                Tunnel::Plain(tcp) => {
                    if !secure {
                        EgressStream::Plain(tcp)
                    } else {
                        let name = server_name(&host)?;
                        let tls = origin_tls.connect(name, tcp).await.map_err(
                            |e| -> Box<dyn std::error::Error + Send + Sync> {
                                format!("the TLS handshake with the origin failed: {e}").into()
                            },
                        )?;
                        EgressStream::OriginTls(Box::new(tls))
                    }
                }
                Tunnel::ProxyTls(tls) => {
                    if !secure {
                        EgressStream::ProxyTls(tls)
                    } else {
                        let name = server_name(&host)?;
                        let tls = origin_tls.connect(name, *tls).await.map_err(
                            |e| -> Box<dyn std::error::Error + Send + Sync> {
                                format!("the TLS handshake with the origin failed: {e}").into()
                            },
                        )?;
                        EgressStream::OriginTlsOverProxyTls(Box::new(tls))
                    }
                }
            };
            Ok(TokioIo::new(stream))
        })
    }
}

/// The TLS server name for an origin host.
fn server_name(
    host: &str,
) -> Result<rustls::pki_types::ServerName<'static>, Box<dyn std::error::Error + Send + Sync>> {
    rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("`{host}` is not a valid TLS server name: {e}").into()
        },
    )
}

/// The error a proxy failure becomes on the request path.
///
/// It is a distinct type so that the request path can tell "the proxy failed" from "the provider
/// failed", and report the first without the second being guessed at.
#[derive(Debug)]
pub struct ProxyDialError(pub ProxyError);

impl std::fmt::Display for ProxyDialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the proxy could not be reached: {}", self.0)
    }
}

impl std::error::Error for ProxyDialError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{ProxyScheme, TunnelLimits};

    fn endpoint() -> ProxyEndpoint {
        ProxyEndpoint {
            scheme: ProxyScheme::Http,
            host: "127.0.0.1".to_owned(),
            port: 8080,
            auth: None,
        }
    }

    #[test]
    fn a_connector_does_not_print_its_credential() {
        let mut endpoint = endpoint();
        endpoint.auth = Some(
            crate::proxy::ProxyAuth::new("user", "hunter2").expect("the credential is well formed"),
        );
        let connector = ProxyConnector::new(
            endpoint,
            TunnelLimits::default(),
            connector_without_tls(),
            None,
        );
        let printed = format!("{connector:?}");
        assert!(
            !printed.contains("hunter2"),
            "the debug output leaked: {printed}"
        );
        assert!(printed.contains("127.0.0.1"));
    }

    /// A TLS connector that is never used, because no test in this module dials anything.
    fn connector_without_tls() -> tokio_rustls::TlsConnector {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }
}

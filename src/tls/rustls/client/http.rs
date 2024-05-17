use crate::error::{BoxError, ErrorExt, OpaqueError};
use crate::http::client::{ClientConnection, EstablishedClientConnection};
use crate::http::{Request, RequestContext};
use crate::service::{Context, Service};
use crate::stream::Stream;
use crate::tls::rustls::dep::rustls::RootCertStore;
use crate::tls::rustls::dep::tokio_rustls::{client::TlsStream, TlsConnector};
use crate::tls::rustls::verify::NoServerCertVerifier;
use crate::uri::Scheme;
use crate::{service::Layer, tls::rustls::dep::rustls::ClientConfig};
use pin_project_lite::pin_project;
use private::{ConnectorKindAuto, ConnectorKindSecure};
use std::sync::OnceLock;
use std::{fmt, sync::Arc};
use tokio::io::{AsyncRead, AsyncWrite};

/// A [`Layer`] which wraps the given service with a [`HttpsConnector`].
///
/// See [`HttpsConnector`] for more information.
#[derive(Clone)]
pub struct HttpsConnectorLayer<K = ConnectorKindAuto> {
    config: Option<Arc<ClientConfig>>,
    _kind: std::marker::PhantomData<K>,
}

impl std::fmt::Debug for HttpsConnectorLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsConnectorLayer")
            .field("config", &self.config)
            .finish()
    }
}

impl<K> HttpsConnectorLayer<K> {
    /// Creates a new [`HttpsConnectorLayer`].
    pub fn new() -> Self {
        Self {
            config: None,
            _kind: std::marker::PhantomData,
        }
    }

    /// Attach a client config to this [`HttpsConnectorLayer`],
    /// to be used instead of a globally shared default client config.
    pub fn with_config(mut self, config: Arc<ClientConfig>) -> Self {
        self.config = Some(config);
        self
    }
}

impl HttpsConnectorLayer<ConnectorKindAuto> {
    /// Creates a new [`HttpsConnectorLayer`] which will establish
    /// a secure connection if the request demands it,
    /// otherwise it will forward the pre-established inner connection.
    pub fn auto() -> Self {
        Self {
            config: None,
            _kind: std::marker::PhantomData,
        }
    }
}

impl HttpsConnectorLayer<ConnectorKindSecure> {
    /// Creates a new [`HttpsConnectorLayer`] which will always
    /// establish a secure connection regardless of the request it is for.
    pub fn secure_only() -> Self {
        Self {
            config: None,
            _kind: std::marker::PhantomData,
        }
    }
}

impl<K, S> Layer<S> for HttpsConnectorLayer<K> {
    type Service = HttpsConnector<S, K>;

    fn layer(&self, inner: S) -> Self::Service {
        let connector = HttpsConnector::new(inner);
        match self.config.clone() {
            Some(config) => connector.with_config(config),
            None => connector,
        }
    }
}

impl Default for HttpsConnectorLayer<ConnectorKindAuto> {
    fn default() -> Self {
        Self::auto()
    }
}

/// A connector which can be used to establish a connection to a server.
///
/// By default it will created in auto mode ([`HttpsConnector::auto`]),
/// which will perform the Tls handshake on the underlying stream,
/// only if the request requires a secure connection. You can instead use
/// [`HttpsConnector::secure_only`] to force the connector to always
/// establish a secure connection.
pub struct HttpsConnector<S, K = ConnectorKindAuto> {
    inner: S,
    config: Option<Arc<ClientConfig>>,
    _kind: std::marker::PhantomData<K>,
}

impl<S: fmt::Debug, K> fmt::Debug for HttpsConnector<S, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpsConnector")
            .field("inner", &self.inner)
            .field("config", &self.config)
            .finish()
    }
}

impl<S: Clone, K> Clone for HttpsConnector<S, K> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            config: self.config.clone(),
            _kind: std::marker::PhantomData,
        }
    }
}

impl<S, K> HttpsConnector<S, K> {
    /// Creates a new [`HttpsConnector`].
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            config: None,
            _kind: std::marker::PhantomData,
        }
    }

    /// Attach a client config to this [`HttpsConnector`],
    pub fn with_config(mut self, config: Arc<ClientConfig>) -> Self {
        self.config = Some(config);
        self
    }
}

impl<S> HttpsConnector<S, ConnectorKindAuto> {
    /// Creates a new [`HttpsConnector`] which will establish
    /// a secure connection if the request demands it,
    /// otherwise it will forward the pre-established inner connection.
    pub fn auto(inner: S) -> Self {
        Self::new(inner)
    }
}

impl<S> HttpsConnector<S, ConnectorKindSecure> {
    /// Creates a new [`HttpsConnector`] which will always
    /// establish a secure connection regardless of the request it is for.
    pub fn secure_only(inner: S) -> Self {
        Self::new(inner)
    }
}

impl<S, State, Body, T> Service<State, Request<Body>> for HttpsConnector<S, ConnectorKindAuto>
where
    S: Service<State, Request<Body>, Response = EstablishedClientConnection<T, Body, State>>,
    T: Stream + Unpin,
    S::Error: Into<BoxError>,
    State: Send + Sync + 'static,
    Body: Send + 'static,
{
    type Response = EstablishedClientConnection<AutoTlsStream<T>, Body, State>;
    type Error = OpaqueError;

    async fn serve(
        &self,
        ctx: Context<State>,
        req: Request<Body>,
    ) -> Result<Self::Response, Self::Error> {
        let EstablishedClientConnection { mut ctx, req, conn } =
            self.inner
                .serve(ctx, req)
                .await
                .map_err(|err| OpaqueError::from_boxed(err.into()))?;

        let (addr, stream) = conn.into_parts();
        let request_ctx = ctx.get_or_insert_with(|| RequestContext::new(&req));

        if !request_ctx.scheme.secure() {
            return Ok(EstablishedClientConnection {
                ctx,
                req,
                conn: ClientConnection::new(
                    addr,
                    AutoTlsStream {
                        inner: AutoTlsStreamData::Plain { inner: stream },
                    },
                ),
            });
        }

        let host = match request_ctx.host.as_deref() {
            Some(host) => host,
            None => {
                return Err(OpaqueError::from_display("missing http host"));
            }
        };
        let domain = pki_types::ServerName::try_from(host)
            .map_err(|err| OpaqueError::from_std(err).context("invalid DNS Hostname (tls)"))?
            .to_owned();

        let config = match ctx.get::<Arc<ClientConfig>>() {
            Some(config) => config.clone(),
            None => default_tls_client_config(),
        };
        let connector = TlsConnector::from(config);

        let stream = connector
            .connect(domain, stream)
            .await
            .map_err(OpaqueError::from_std)?;

        Ok(EstablishedClientConnection {
            ctx,
            req,
            conn: ClientConnection::new(
                addr,
                AutoTlsStream {
                    inner: AutoTlsStreamData::Secure { inner: stream },
                },
            ),
        })
    }
}

impl<S, State, Body, T> Service<State, Request<Body>> for HttpsConnector<S, ConnectorKindSecure>
where
    S: Service<State, Request<Body>, Response = EstablishedClientConnection<T, Body, State>>,
    T: Stream + Unpin,
    S::Error: Into<BoxError>,
    State: Send + Sync + 'static,
    Body: Send + 'static,
{
    type Response = EstablishedClientConnection<TlsStream<T>, Body, State>;
    type Error = OpaqueError;

    async fn serve(
        &self,
        ctx: Context<State>,
        req: Request<Body>,
    ) -> Result<Self::Response, Self::Error> {
        let EstablishedClientConnection {
            mut ctx,
            mut req,
            conn,
        } = self
            .inner
            .serve(ctx, req)
            .await
            .map_err(|err| OpaqueError::from_boxed(err.into()))?;

        let (addr, stream) = conn.into_parts();

        let request_ctx = ctx.get_or_insert_with(|| RequestContext::new(&req)).clone();
        if let Some(new_scheme) = match request_ctx.scheme {
            Scheme::Http => Some(crate::http::dep::http::uri::Scheme::HTTPS),
            Scheme::Ws => Some("wss".parse().unwrap()),
            Scheme::Empty | Scheme::Custom(_) | Scheme::Https | Scheme::Wss => None,
        } {
            let (mut parts, body) = req.into_parts();
            let mut uri_parts = parts.uri.into_parts();
            uri_parts.scheme = Some(new_scheme);
            parts.uri = crate::http::dep::http::uri::Uri::from_parts(uri_parts).unwrap();
            req = Request::from_parts(parts, body);
            ctx.insert(RequestContext::new(&req));
        }

        let host = match request_ctx.host.as_deref() {
            Some(host) => host,
            None => {
                return Err(OpaqueError::from_display("missing http host"));
            }
        };
        let domain = pki_types::ServerName::try_from(host)
            .map_err(|err| OpaqueError::from_std(err).context("invalid DNS Hostname (tls)"))?
            .to_owned();

        let config = match ctx.get::<Arc<ClientConfig>>() {
            Some(config) => config.clone(),
            None => default_tls_client_config(),
        };
        let connector = TlsConnector::from(config);

        let stream = connector
            .connect(domain, stream)
            .await
            .map_err(OpaqueError::from_std)?;

        Ok(EstablishedClientConnection {
            ctx,
            req,
            conn: ClientConnection::new(addr, stream),
        })
    }
}

pin_project! {
    /// A stream which can be either a secure or a plain stream.
    pub struct AutoTlsStream<S> {
        #[pin]
        inner: AutoTlsStreamData<S>,
    }
}

impl<S: fmt::Debug> fmt::Debug for AutoTlsStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutoTlsStream")
            .field("inner", &self.inner)
            .finish()
    }
}

pin_project! {
    #[project = AutoTlsStreamDataProj]
    /// A stream which can be either a secure or a plain stream.
    enum AutoTlsStreamData<S> {
        /// A secure stream.
        Secure{ #[pin] inner: TlsStream<S> },
        /// A plain stream.
        Plain { #[pin] inner: S },
    }
}

impl<S: fmt::Debug> fmt::Debug for AutoTlsStreamData<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AutoTlsStreamData::Secure { inner } => f.debug_tuple("Secure").field(inner).finish(),
            AutoTlsStreamData::Plain { inner } => f.debug_tuple("Plain").field(inner).finish(),
        }
    }
}

impl<S> AsyncRead for AutoTlsStream<S>
where
    S: Stream + Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.project().inner.project() {
            AutoTlsStreamDataProj::Secure { inner } => inner.poll_read(cx, buf),
            AutoTlsStreamDataProj::Plain { inner } => inner.poll_read(cx, buf),
        }
    }
}

impl<S> AsyncWrite for AutoTlsStream<S>
where
    S: Stream + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.project().inner.project() {
            AutoTlsStreamDataProj::Secure { inner } => inner.poll_write(cx, buf),
            AutoTlsStreamDataProj::Plain { inner } => inner.poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.project().inner.project() {
            AutoTlsStreamDataProj::Secure { inner } => inner.poll_flush(cx),
            AutoTlsStreamDataProj::Plain { inner } => inner.poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.project().inner.project() {
            AutoTlsStreamDataProj::Secure { inner } => inner.poll_shutdown(cx),
            AutoTlsStreamDataProj::Plain { inner } => inner.poll_shutdown(cx),
        }
    }
}

fn default_tls_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut root_storage = RootCertStore::empty();
            root_storage.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut config = ClientConfig::builder()
                .with_root_certificates(root_storage)
                .with_no_client_auth();
            config
                .dangerous()
                .set_certificate_verifier(Arc::new(NoServerCertVerifier::default()));
            Arc::new(config)
        })
        .clone()
}

mod private {
    #[derive(Debug)]
    /// A connector which can be used to establish a connection to a server
    /// in function of the Request, meaning either it will be a seucre
    /// connector or it will be a plain connector.
    ///
    /// This connector can be handy as it allows to have a single layer
    /// which will work both for plain and secure connections.
    pub struct ConnectorKindAuto;

    #[derive(Debug)]
    /// A connector which can _only_ be used to establish a secure connection,
    /// regardless of the scheme of the request URI.
    pub struct ConnectorKindSecure;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_send() {
        use crate::utils::test_helpers::assert_send;

        assert_send::<HttpsConnectorLayer>();
    }

    #[test]
    fn assert_sync() {
        use crate::utils::test_helpers::assert_sync;

        assert_sync::<HttpsConnectorLayer>();
    }
}

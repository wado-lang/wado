//! Custom `WasiHttpHooks` that uses [`crate::tls_trust`]'s augmented
//! trust store for outbound HTTPS.
//!
//! wasmtime's stock `default_send_request` hardcodes `webpki-roots`, which is
//! not sufficient when the host sits behind a TLS-inspecting proxy that signs
//! outgoing HTTPS with a private CA (e.g. a sandboxed dev environment).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::uri::Scheme;
use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use wasmtime_wasi::TrappableError;
use wasmtime_wasi_http::p3::bindings::http::types::{DnsErrorPayload, ErrorCode};
use wasmtime_wasi_http::p3::{RequestOptions, WasiHttpHooks};

use crate::tls_trust::{build_root_cert_store, install_default_crypto_provider};

macro_rules! warn_log {
    ($($arg:tt)*) => { eprintln!("warning: {}", format_args!($($arg)*)) };
}

const ALPN_H2: &[u8] = b"h2";
const ALPN_HTTP11: &[u8] = b"http/1.1";

/// The HTTP version an outbound connection ended up speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireProtocol {
    Http1,
    Http2,
}

impl WireProtocol {
    /// TLS decides the version through ALPN. A cleartext connection runs no
    /// ALPN and therefore stays on HTTP/1.1: h2c requires the client to know
    /// out of band that the server speaks it, and `wasi:http@0.3.0` gives the
    /// guest no way to say so.
    fn from_alpn(alpn: Option<&[u8]>) -> Self {
        match alpn {
            Some(ALPN_H2) => Self::Http2,
            Some(_) | None => Self::Http1,
        }
    }
}

pub struct WadoHttpHooks {
    client_config: Arc<rustls::ClientConfig>,
}

/// Process-wide `rustls::ClientConfig` shared by every `WadoHttpHooks`.
///
/// `wado serve` builds a fresh `WasiState` per request and used to call
/// `WadoHttpHooks::new()` from the per-request constructor, which in turn
/// ran `build_root_cert_store()` — a function that reads CA bundles from
/// disk via `SSL_CERT_FILE`/`SSL_CERT_DIR`. Doing that I/O on every request
/// is wasted work; the configuration is immutable after process startup,
/// so cache it in a `LazyLock` and clone the `Arc` per request.
fn shared_client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        install_default_crypto_provider();
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(build_root_cert_store())
            .with_no_client_auth();
        // HTTP/2 first: it is the only version that carries trailers to every
        // peer, which protocols layered on `wasi:http` — gRPC puts its
        // `grpc-status` there — depend on. Servers that lack it fall back to
        // `http/1.1` through ALPN.
        config.alpn_protocols = vec![ALPN_H2.to_vec(), ALPN_HTTP11.to_vec()];
        Arc::new(config)
    });
    Arc::clone(&CONFIG)
}

impl WadoHttpHooks {
    pub fn new() -> Self {
        Self {
            client_config: shared_client_config(),
        }
    }
}

impl Default for WadoHttpHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl WasiHttpHooks for WadoHttpHooks {
    fn send_request(
        &mut self,
        request: http::Request<UnsyncBoxBody<Bytes, ErrorCode>>,
        options: Option<RequestOptions>,
        _fut: Box<dyn core::future::Future<Output = Result<(), ErrorCode>> + Send>,
    ) -> Box<
        dyn core::future::Future<
                Output = Result<
                    (
                        http::Response<UnsyncBoxBody<Bytes, ErrorCode>>,
                        Box<dyn core::future::Future<Output = Result<(), ErrorCode>> + Send>,
                    ),
                    TrappableError<ErrorCode>,
                >,
            > + Send,
    > {
        let config = Arc::clone(&self.client_config);
        Box::new(async move {
            let (res, driver) = send_request(config, request, options)
                .await
                .map_err(TrappableError::from)?;
            let driver: Box<dyn core::future::Future<Output = Result<(), ErrorCode>> + Send> =
                Box::new(driver);
            Ok((res.map(BodyExt::boxed_unsync), driver))
        })
    }
}

async fn send_request(
    client_config: Arc<rustls::ClientConfig>,
    mut req: http::Request<UnsyncBoxBody<Bytes, ErrorCode>>,
    options: Option<RequestOptions>,
) -> Result<
    (
        http::Response<impl http_body::Body<Data = Bytes, Error = ErrorCode>>,
        BoxFuture<'static, Result<(), ErrorCode>>,
    ),
    ErrorCode,
> {
    let uri = req.uri();
    let authority = uri.authority().ok_or(ErrorCode::HttpRequestUriInvalid)?;
    let use_tls = uri.scheme() == Some(&Scheme::HTTPS);
    let host = authority.host().to_string();
    let connect_addr = if authority.port().is_some() {
        authority.to_string()
    } else {
        let port = if use_tls { 443 } else { 80 };
        format!("{authority}:{port}")
    };

    let connect_timeout = options
        .as_ref()
        .and_then(|o| o.connect_timeout)
        .unwrap_or(Duration::from_mins(10));
    let first_byte_timeout = options
        .as_ref()
        .and_then(|o| o.first_byte_timeout)
        .unwrap_or(Duration::from_mins(10));
    let between_bytes_timeout = options
        .as_ref()
        .and_then(|o| o.between_bytes_timeout)
        .unwrap_or(Duration::from_mins(10));

    let tcp = match tokio::time::timeout(connect_timeout, TcpStream::connect(&connect_addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::AddrNotAvailable => {
            return Err(dns_error("address not available".to_string(), 0));
        }
        Ok(Err(err))
            if err
                .to_string()
                .starts_with("failed to lookup address information") =>
        {
            return Err(dns_error("address not available".to_string(), 0));
        }
        Ok(Err(err)) => {
            warn_log!("connection refused: {err:?}");
            return Err(ErrorCode::ConnectionRefused);
        }
        Err(_) => return Err(ErrorCode::ConnectionTimeout),
    };

    let (mut sender, conn_driver) = if use_tls {
        let domain = ServerName::try_from(host.as_str())
            .map_err(|e| {
                warn_log!("dns lookup error: {e:?}");
                dns_error("invalid dns name".to_string(), 0)
            })?
            .to_owned();
        let connector = TlsConnector::from(client_config);
        let tls = connector.connect(domain, tcp).await.map_err(|e| {
            warn_log!("tls protocol error: {e:?}");
            ErrorCode::TlsProtocolError
        })?;
        let protocol = WireProtocol::from_alpn(tls.get_ref().1.alpn_protocol());
        handshake(protocol, tls, connect_timeout).await?
    } else {
        handshake(WireProtocol::Http1, tcp, connect_timeout).await?
    };

    if sender.protocol() == WireProtocol::Http1 {
        *req.uri_mut() = origin_form(req.uri());
    }

    let res = tokio::time::timeout(first_byte_timeout, sender.send_request(req))
        .await
        .map_err(|_| ErrorCode::ConnectionReadTimeout)?
        .map_err(ErrorCode::from_hyper_request_error)?;

    let res = res.map(|incoming| IncomingResponseBody {
        incoming,
        timeout: {
            let mut t = tokio::time::interval(between_bytes_timeout);
            t.reset();
            t
        },
    });

    Ok((res, conn_driver))
}

/// HTTP/1 addresses an origin server with just the path and query; scheme and
/// authority belong in the request line only when addressing a proxy. HTTP/2
/// is the opposite — it derives the `:scheme` and `:authority` pseudo-headers
/// from the URI, so that path keeps the URI whole.
fn origin_form(uri: &http::Uri) -> http::Uri {
    http::Uri::builder()
        .path_and_query(
            uri.path_and_query()
                .map(http::uri::PathAndQuery::as_str)
                .unwrap_or("/"),
        )
        .build()
        .expect("comes from valid request")
}

type RequestBody = UnsyncBoxBody<Bytes, ErrorCode>;

enum Sender {
    Http1(hyper::client::conn::http1::SendRequest<RequestBody>),
    Http2(hyper::client::conn::http2::SendRequest<RequestBody>),
}

impl Sender {
    fn protocol(&self) -> WireProtocol {
        match self {
            Self::Http1(_) => WireProtocol::Http1,
            Self::Http2(_) => WireProtocol::Http2,
        }
    }

    async fn send_request(
        &mut self,
        req: http::Request<RequestBody>,
    ) -> hyper::Result<http::Response<hyper::body::Incoming>> {
        match self {
            Self::Http1(sender) => sender.send_request(req).await,
            Self::Http2(sender) => sender.send_request(req).await,
        }
    }
}

async fn handshake<S>(
    protocol: WireProtocol,
    stream: S,
    connect_timeout: Duration,
) -> Result<(Sender, BoxFuture<'static, Result<(), ErrorCode>>), ErrorCode>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    match protocol {
        WireProtocol::Http1 => {
            let (sender, conn) = await_handshake(
                connect_timeout,
                hyper::client::conn::http1::Builder::new().handshake(io),
            )
            .await?;
            Ok((Sender::Http1(sender), spawn_connection(conn)))
        }
        WireProtocol::Http2 => {
            let (sender, conn) = await_handshake(
                connect_timeout,
                hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .handshake(io),
            )
            .await?;
            Ok((Sender::Http2(sender), spawn_connection(conn)))
        }
    }
}

async fn await_handshake<F, T>(connect_timeout: Duration, handshake: F) -> Result<T, ErrorCode>
where
    F: Future<Output = hyper::Result<T>>,
{
    tokio::time::timeout(connect_timeout, handshake)
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(ErrorCode::from_hyper_request_error)
}

/// The hyper connection must be driven concurrently with `sender.send_request`,
/// otherwise the request never reaches the wire. Spawn the connection on the
/// tokio runtime now, and forward its result via a channel so the caller can
/// still observe completion / errors.
fn spawn_connection<C>(conn: C) -> BoxFuture<'static, Result<(), ErrorCode>>
where
    C: Future<Output = hyper::Result<()>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let res = conn.await.map_err(from_hyper_response_error);
        let _ = tx.send(res);
    });
    Box::pin(async move { rx.await.unwrap_or(Ok(())) })
}

fn from_hyper_response_error(err: hyper::Error) -> ErrorCode {
    use core::error::Error as _;
    if err.is_timeout() {
        return ErrorCode::HttpResponseTimeout;
    }
    if let Some(cause) = err.source()
        && let Some(err) = cause.downcast_ref::<ErrorCode>()
    {
        return err.clone();
    }
    warn_log!("hyper response error: {err:?}");
    ErrorCode::HttpProtocolError
}

fn dns_error(rcode: String, info_code: u16) -> ErrorCode {
    ErrorCode::DnsError(DnsErrorPayload {
        rcode: Some(rcode),
        info_code: Some(info_code),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_offers_h2_before_http11() {
        let config = shared_client_config();
        assert_eq!(
            config.alpn_protocols,
            vec![ALPN_H2.to_vec(), ALPN_HTTP11.to_vec()]
        );
    }

    #[test]
    fn only_negotiated_h2_selects_http2() {
        assert_eq!(WireProtocol::from_alpn(Some(ALPN_H2)), WireProtocol::Http2);
        assert_eq!(
            WireProtocol::from_alpn(Some(ALPN_HTTP11)),
            WireProtocol::Http1
        );
        // Cleartext: no ALPN ran, so no h2c.
        assert_eq!(WireProtocol::from_alpn(None), WireProtocol::Http1);
    }

    #[test]
    fn origin_form_drops_scheme_and_authority() {
        let uri = "https://example.com/v1/greeter?a=1".parse().unwrap();
        assert_eq!(origin_form(&uri), "/v1/greeter?a=1");
    }

    #[test]
    fn origin_form_of_empty_path_is_root() {
        let uri = "https://example.com".parse().unwrap();
        assert_eq!(origin_form(&uri), "/");
    }
}

struct IncomingResponseBody {
    incoming: hyper::body::Incoming,
    timeout: tokio::time::Interval,
}

impl http_body::Body for IncomingResponseBody {
    type Data = <hyper::body::Incoming as http_body::Body>::Data;
    type Error = ErrorCode;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use core::task::Poll;
        match std::pin::Pin::new(&mut self.as_mut().incoming).poll_frame(cx) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(from_hyper_response_error(err)))),
            Poll::Ready(Some(Ok(frame))) => {
                self.timeout.reset();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => match self.timeout.poll_tick(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => Poll::Ready(Some(Err(ErrorCode::ConnectionReadTimeout))),
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.incoming.size_hint()
    }
}

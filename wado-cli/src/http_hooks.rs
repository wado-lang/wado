//! Custom `WasiHttpHooks` that uses [`crate::tls_trust`]'s augmented
//! trust store for outbound HTTPS.
//!
//! wasmtime's stock `default_send_request` hardcodes `webpki-roots`, which is
//! not sufficient when the host sits behind a TLS-inspecting proxy that signs
//! outgoing HTTPS with a private CA (e.g. a sandboxed dev environment).

use std::sync::Arc;
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

pub struct WadoHttpHooks {
    client_config: Arc<rustls::ClientConfig>,
}

impl WadoHttpHooks {
    pub fn new() -> Self {
        install_default_crypto_provider();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(build_root_cert_store())
            .with_no_client_auth();
        Self {
            client_config: Arc::new(client_config),
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
        handshake(tls, connect_timeout).await?
    } else {
        handshake(tcp, connect_timeout).await?
    };

    *req.uri_mut() = http::Uri::builder()
        .path_and_query(
            req.uri()
                .path_and_query()
                .map(http::uri::PathAndQuery::as_str)
                .unwrap_or("/"),
        )
        .build()
        .expect("comes from valid request");

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

async fn handshake<S>(
    stream: S,
    connect_timeout: Duration,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<UnsyncBoxBody<Bytes, ErrorCode>>,
        BoxFuture<'static, Result<(), ErrorCode>>,
    ),
    ErrorCode,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (sender, conn) = tokio::time::timeout(
        connect_timeout,
        hyper::client::conn::http1::Builder::new().handshake(hyper_util::rt::TokioIo::new(stream)),
    )
    .await
    .map_err(|_| ErrorCode::ConnectionTimeout)?
    .map_err(ErrorCode::from_hyper_request_error)?;

    // The hyper connection must be driven concurrently with `sender.send_request`,
    // otherwise the request never reaches the wire. Spawn the connection on the
    // tokio runtime now, and forward its result via a channel so the caller can
    // still observe completion / errors.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let res = conn.await.map_err(from_hyper_response_error);
        let _ = tx.send(res);
    });
    let driver: BoxFuture<'static, Result<(), ErrorCode>> =
        Box::pin(async move { rx.await.unwrap_or(Ok(())) });
    Ok((sender, driver))
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

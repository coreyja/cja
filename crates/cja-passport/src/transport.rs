//! One OIDC HTTP boundary for host networking or an injected tailnet dialer.

use std::{future::Future, pin::Pin};

use openidconnect::{AsyncHttpClient, HttpRequest, HttpResponse};

#[cfg(feature = "tailnet")]
use bytes::Bytes;
#[cfg(feature = "tailnet")]
use http_body_util::{BodyExt as _, Full};
#[cfg(feature = "tailnet")]
use hyper_util::rt::TokioIo;
#[cfg(feature = "tailnet")]
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PassportHttpError {
    #[error("host HTTP request failed: {0}")]
    Host(#[from] openidconnect::HttpClientError<reqwest::Error>),
    #[cfg(feature = "tailnet")]
    #[error("invalid OIDC request URL: {0}")]
    Url(#[from] url::ParseError),
    #[cfg(feature = "tailnet")]
    #[error("OIDC endpoint must use HTTPS (loopback HTTP is allowed for tests)")]
    Scheme,
    #[cfg(feature = "tailnet")]
    #[error("OIDC endpoint has no host or port")]
    Authority,
    #[cfg(feature = "tailnet")]
    #[error("OIDC transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "tailnet")]
    #[error("OIDC HTTP connection failed: {0}")]
    Hyper(#[from] hyper::Error),
    #[cfg(feature = "tailnet")]
    #[error("OIDC HTTP request is invalid: {0}")]
    Http(#[from] http::Error),
    #[cfg(feature = "tailnet")]
    #[error("OIDC response exceeds 1 MiB")]
    BodyTooLarge,
}

#[derive(Clone)]
pub(crate) enum PassportHttp {
    Host(reqwest::Client),
    #[cfg(feature = "tailnet")]
    Dialer {
        dialer: Arc<dyn setec::Dialer>,
        tls: tokio_rustls::TlsConnector,
    },
}

impl PassportHttp {
    pub(crate) fn host(client: reqwest::Client) -> Self {
        Self::Host(client)
    }

    #[cfg(feature = "tailnet")]
    pub(crate) fn dialer(dialer: Arc<dyn setec::Dialer>) -> cja::Result<Self> {
        Ok(Self::Dialer {
            dialer,
            tls: tokio_rustls::TlsConnector::from(Arc::new(cja::tls::ring_client_config()?)),
        })
    }
}

impl<'c> AsyncHttpClient<'c> for PassportHttp {
    type Error = PassportHttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            match self {
                Self::Host(client) => client.call(request).await.map_err(Into::into),
                #[cfg(feature = "tailnet")]
                Self::Dialer { dialer, tls } => call_dialer(dialer, tls, request).await,
            }
        })
    }
}

#[cfg(feature = "tailnet")]
struct AbortOnDrop(tokio::task::JoinHandle<Result<(), hyper::Error>>);

#[cfg(feature = "tailnet")]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(feature = "tailnet")]
async fn call_dialer(
    dialer: &Arc<dyn setec::Dialer>,
    tls: &tokio_rustls::TlsConnector,
    request: HttpRequest,
) -> Result<HttpResponse, PassportHttpError> {
    const MAX_BODY: usize = 1024 * 1024;
    let url = url::Url::parse(&request.uri().to_string())?;
    let host = url.host_str().ok_or(PassportHttpError::Authority)?;
    let port = url
        .port_or_known_default()
        .ok_or(PassportHttpError::Authority)?;
    let secure = match url.scheme() {
        "https" => true,
        "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1") => false,
        _ => return Err(PassportHttpError::Scheme),
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(PassportHttpError::Scheme);
    }
    let connection = dialer.dial(host, port).await?;
    let connection: Box<dyn setec::Connection> = if secure {
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(std::io::Error::other)?;
        Box::new(tls.connect(name, connection).await?)
    } else {
        connection
    };
    let (mut sender, driver) =
        hyper::client::conn::http1::handshake(TokioIo::new(connection)).await?;
    let _driver = AbortOnDrop(tokio::spawn(driver));

    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let (parts, body) = request.into_parts();
    let mut outbound = http::Request::builder().method(parts.method).uri(path);
    for (name, value) in &parts.headers {
        if name != http::header::HOST {
            outbound = outbound.header(name, value);
        }
    }
    let outbound = outbound
        .header(http::header::HOST, host_header)
        .body(Full::new(Bytes::from(body)))?;
    let response = sender.send_request(outbound).await?;
    let (parts, mut incoming) = response.into_parts();
    let mut body = Vec::new();
    while let Some(frame) = incoming.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if data.len() > MAX_BODY - body.len() {
                return Err(PassportHttpError::BodyTooLarge);
            }
            body.extend_from_slice(&data);
        }
    }
    Ok(HttpResponse::from_parts(parts, body))
}

#[cfg(all(test, feature = "tailnet"))]
mod tests {
    use super::*;
    use std::{
        io,
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
    };

    struct Forward(SocketAddr, Arc<AtomicUsize>);
    impl setec::Dialer for Forward {
        fn dial<'a>(&'a self, _host: &'a str, _port: u16) -> setec::DialFuture<'a> {
            Box::pin(async move {
                self.1.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(TcpStream::connect(self.0).await?) as Box<dyn setec::Connection>)
            })
        }
    }

    async fn serve_once(
        body: Vec<u8>,
        status: &str,
        location: Option<&str>,
        declared_length: Option<usize>,
    ) -> (
        PassportHttp,
        u16,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let status = status.to_owned();
        let location = location.map(str::to_owned);
        let handle = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(pos) = request.windows(4).position(|x| x == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..header_end]);
            assert!(head.starts_with("POST /path?q=1 HTTP/1.1\r\n"), "{head}");
            assert!(
                head.to_ascii_lowercase()
                    .contains(&format!("host: 127.0.0.1:{port}"))
            );
            let declared: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse().ok())
                })
                .unwrap_or(0);
            while request.len() - header_end < declared {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
            }
            assert_eq!(&request[header_end..], b"request body");
            let location =
                location.map_or_else(String::new, |value| format!("Location: {value}\r\n"));
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{location}\r\n",
                declared_length.unwrap_or(body.len())
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let dials = Arc::new(AtomicUsize::new(0));
        let http = PassportHttp::dialer(Arc::new(Forward(
            SocketAddr::from(([127, 0, 0, 1], port)),
            dials.clone(),
        )))
        .unwrap();
        (http, port, dials, handle)
    }

    fn request(port: u16) -> HttpRequest {
        http::Request::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/path?q=1"))
            .header("content-type", "text/plain")
            .body(b"request body".to_vec())
            .unwrap()
    }

    #[tokio::test]
    async fn redirect_is_returned_without_following_and_body_limit_is_exact() {
        let (http, port, dials, server) =
            serve_once(b"redirect".to_vec(), "302 Found", Some("/elsewhere"), None).await;
        let response = http.call(request(port)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::FOUND);
        assert_eq!(response.headers()[http::header::LOCATION], "/elsewhere");
        assert_eq!(response.body(), b"redirect");
        assert_eq!(dials.load(Ordering::SeqCst), 1);
        server.await.unwrap();

        let (http, port, _, server) =
            serve_once(vec![b'x'; 1024 * 1024], "200 OK", None, None).await;
        assert_eq!(
            http.call(request(port)).await.unwrap().body().len(),
            1024 * 1024
        );
        server.await.unwrap();

        let (http, port, _, server) =
            serve_once(vec![b'x'; 1024 * 1024 + 1], "200 OK", None, None).await;
        assert!(matches!(
            http.call(request(port)).await,
            Err(PassportHttpError::BodyTooLarge)
        ));
        // A too-large response intentionally closes the connection early.
        let _ = server.await;

        let (http, port, _, server) = serve_once(b"short".to_vec(), "200 OK", None, Some(8)).await;
        assert!(matches!(
            http.call(request(port)).await,
            Err(PassportHttpError::Hyper(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_loopback_http_is_rejected_before_dial() {
        struct Refuse;
        impl setec::Dialer for Refuse {
            fn dial<'a>(&'a self, _: &'a str, _: u16) -> setec::DialFuture<'a> {
                panic!("HTTP scheme must be rejected before dialing")
            }
        }
        let client = PassportHttp::dialer(Arc::new(Refuse)).unwrap();
        let request = http::Request::builder()
            .uri("http://passport.example/jwks")
            .body(Vec::new())
            .unwrap();
        assert!(matches!(
            client.call(request).await,
            Err(PassportHttpError::Scheme)
        ));
        let relative = http::Request::builder()
            .uri("/jwks")
            .body(Vec::new())
            .unwrap();
        assert!(matches!(
            client.call(relative).await,
            Err(PassportHttpError::Url(_))
        ));
    }

    #[tokio::test]
    async fn https_rejects_a_plaintext_peer() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = PassportHttp::dialer(Arc::new(Forward(
            server.local_addr().unwrap(),
            Arc::new(AtomicUsize::new(0)),
        )))
        .unwrap();
        let responder = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut hello = [0; 1024];
            assert!(stream.read(&mut hello).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let request = http::Request::builder()
            .uri("https://passport.rooster-mermaid.ts.net/jwks")
            .body(Vec::new())
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), client.call(request))
            .await
            .unwrap();
        assert!(matches!(result, Err(PassportHttpError::Io(_))));
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_closes_the_connection_driver() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let http = PassportHttp::dialer(Arc::new(Forward(
            server.local_addr().unwrap(),
            Arc::new(AtomicUsize::new(0)),
        )))
        .unwrap();
        let task = tokio::spawn(async move { http.call(request(port)).await });
        let (mut stream, _) = server.accept().await.unwrap();
        let mut initial = Vec::new();
        while !initial.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            initial.extend_from_slice(&chunk[..count]);
        }
        task.abort();
        let _ = task.await;
        let mut bytes = [0; 1024];
        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.read(&mut bytes).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => break,
                    Err(error) => panic!("unexpected read failure: {error}"),
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "cancelling the OIDC request must abort its driver"
        );
    }
}

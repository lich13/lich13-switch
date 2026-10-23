use crate::errors::code;
use hyper::Uri;
use hyper_util::{
    client::legacy::connect::{dns::Name, Connected, Connection, HttpConnector},
    rt::TokioIo,
};
use std::{
    convert::Infallible,
    future::{ready, Future, Ready},
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{
    rustls::{self, pki_types::ServerName},
    TlsConnector,
};
use tower_service::Service;
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}
pub struct Transport(TokioIo<Box<dyn Stream>>);
impl hyper::rt::Read for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl hyper::rt::Write for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl Connection for Transport {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnectError {
    TargetConnect,
    TargetTimeout,
    Tls,
    Loop,
}
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TargetConnect => "上游目标连接失败或超时",
            Self::TargetTimeout => "上游连接超时",
            Self::Tls => "上游 TLS 验证失败",
            Self::Loop => "上游指向本网关",
        })
    }
}
impl std::error::Error for ConnectError {}
pub fn classify(error: &(dyn std::error::Error + 'static)) -> Option<ConnectError> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(e) = e.downcast_ref::<ConnectError>() {
            return Some(*e);
        }
        current = e.source();
    }
    None
}
pub fn diagnostic_code(error: &(dyn std::error::Error + 'static)) -> &'static str {
    match classify(error) {
        Some(ConnectError::TargetTimeout) => code::CONNECT_TIMEOUT,
        Some(ConnectError::Tls) => code::TLS_HANDSHAKE_FAILED,
        Some(ConnectError::Loop) => code::GATEWAY_LOOP,
        _ => code::CONNECTION_FAILED,
    }
}
// Resolve once, check every address for a gateway loop, then let hyper-util
// handle address-family fallback without resolving an unchecked second time.
#[derive(Clone)]
struct Resolved(Vec<SocketAddr>);
impl Service<Name> for Resolved {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, _: Name) -> Self::Future {
        ready(Ok(self.0.clone().into_iter()))
    }
}
fn connect_error(error: &(dyn std::error::Error + 'static)) -> ConnectError {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::TimedOut)
        {
            return ConnectError::TargetTimeout;
        }
        current = e.source();
    }
    ConnectError::TargetConnect
}
#[derive(Clone)]
pub struct Connector {
    pub timeout: Duration,
    pub gateway_port: u16,
    tls: TlsConnector,
    ports: Option<Arc<std::sync::Mutex<Vec<u16>>>>,
}
impl Connector {
    pub fn new(timeout: Duration, gateway_port: u16) -> Self {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self {
            ports: None,
            timeout,
            gateway_port,
            tls: TlsConnector::from(Arc::new(tls)),
        }
    }
    pub fn with_ports(mut self, ports: Arc<std::sync::Mutex<Vec<u16>>>) -> Self {
        self.ports = Some(ports);
        self
    }
    fn is_gateway_port(&self, port: u16) -> bool {
        port == self.gateway_port
            || self
                .ports
                .as_ref()
                .is_some_and(|p| super::locks::published(p).contains(&port))
    }
    pub async fn connect(&self, uri: Uri) -> Result<Transport, ConnectError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let host = uri
            .host()
            .ok_or(ConnectError::TargetConnect)?
            .trim_matches(['[', ']']);
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        if self.is_gateway_port(port)
            && (host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()))
        {
            return Err(ConnectError::Loop);
        }
        let addresses = tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| ConnectError::TargetTimeout)?
            .map_err(|_| ConnectError::TargetConnect)?
            .collect();
        self.connect_resolved(uri, addresses, deadline).await
    }
    async fn connect_resolved(
        &self,
        uri: Uri,
        addresses: Vec<SocketAddr>,
        deadline: tokio::time::Instant,
    ) -> Result<Transport, ConnectError> {
        if addresses
            .iter()
            .any(|a| self.is_gateway_port(a.port()) && a.ip().is_loopback())
        {
            return Err(ConnectError::Loop);
        }
        tokio::time::timeout_at(deadline, async {
            let mut http = HttpConnector::new_with_resolver(Resolved(addresses));
            http.enforce_http(false);
            http.set_nodelay(true);
            http.set_happy_eyeballs_timeout(Some(Duration::from_millis(300)));
            // hyper-util divides this remaining budget across same-family addresses
            // and races the other family after the 300ms fallback delay.
            http.set_connect_timeout(Some(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ));
            let tcp = http
                .call(uri.clone())
                .await
                .map_err(|e| connect_error(&e))?
                .into_inner();
            let stream: Box<dyn Stream> = if uri.scheme_str() == Some("https") {
                let host = uri
                    .host()
                    .ok_or(ConnectError::Tls)?
                    .trim_matches(['[', ']']);
                let server =
                    ServerName::try_from(host.to_owned()).map_err(|_| ConnectError::Tls)?;
                Box::new(
                    self.tls
                        .connect(server, tcp)
                        .await
                        .map_err(|_| ConnectError::Tls)?,
                )
            } else {
                Box::new(tcp)
            };
            Ok(Transport(TokioIo::new(stream)))
        })
        .await
        .map_err(|_| ConnectError::TargetTimeout)?
    }
}
impl Service<Uri> for Connector {
    type Response = Transport;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Transport, ConnectError>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.connect(uri).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::Instant,
    };

    #[tokio::test]
    async fn fallback_addresses_deliver_only_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connector = Connector::new(Duration::from_secs(2), 0);
        for first in [
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], address.port())),
            SocketAddr::from(([127, 0, 0, 2], address.port())),
        ] {
            let uri = format!("http://fixture.example.invalid:{}", address.port())
                .parse()
                .unwrap();
            let transport = connector
                .connect_resolved(
                    uri,
                    vec![first, address],
                    Instant::now() + connector.timeout,
                )
                .await
                .unwrap();
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut stream = TokioIo::new(transport);
            stream.write_all(b"fixture").await.unwrap();
            stream.flush().await.unwrap();
            let mut body = [0; 7];
            peer.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"fixture");
            drop(stream);
            assert_eq!(peer.read(&mut body).await.unwrap(), 0);
        }
    }
    #[tokio::test]
    async fn resolved_loop_is_rejected_before_any_connection_and_covers_shared_ports() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connector = Connector::new(Duration::from_secs(1), 0)
            .with_ports(Arc::new(std::sync::Mutex::new(vec![address.port()])));
        let uri = format!("http://fixture.example.invalid:{}", address.port())
            .parse()
            .unwrap();
        let error = connector
            .connect_resolved(uri, vec![address], Instant::now() + connector.timeout)
            .await
            .err()
            .unwrap();
        assert_eq!(error, ConnectError::Loop);
        assert_eq!(diagnostic_code(&error), code::GATEWAY_LOOP);
        let direct = format!("http://127.0.0.1:{}", address.port())
            .parse()
            .unwrap();
        assert_eq!(
            connector.connect(direct).await.err().unwrap(),
            ConnectError::Loop
        );
    }
    #[tokio::test]
    async fn tls_wait_uses_the_remaining_connection_deadline_and_releases_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connector = Connector::new(Duration::from_secs(300), 0);
        let uri = format!("https://fixture.example.invalid:{}", address.port())
            .parse()
            .unwrap();
        // The caller's DNS phase has already consumed almost all its budget.
        let connecting = tokio::spawn(async move {
            connector
                .connect_resolved(
                    uri,
                    vec![address],
                    Instant::now() + Duration::from_millis(100),
                )
                .await
        });
        let (mut peer, _) = listener.accept().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), connecting)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error, ConnectError::TargetTimeout);
        assert_eq!(diagnostic_code(&error), code::CONNECT_TIMEOUT);
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn invalid_tls_is_distinct_from_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            peer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await
                .unwrap();
        });
        let connector = Connector::new(Duration::from_secs(2), 0);
        let error = connector
            .connect(
                format!("https://127.0.0.1:{}", address.port())
                    .parse()
                    .unwrap(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error, ConnectError::Tls);
        assert_eq!(diagnostic_code(&error), code::TLS_HANDSHAKE_FAILED);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancelling_tls_connect_releases_the_tcp_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connector = Connector::new(Duration::from_secs(300), 0);
        let connecting = tokio::spawn(async move {
            connector
                .connect(
                    format!("https://127.0.0.1:{}", address.port())
                        .parse()
                        .unwrap(),
                )
                .await
        });
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut prefix = [0; 1024];
        assert!(peer.read(&mut prefix).await.unwrap() > 0);
        connecting.abort();
        assert!(matches!(connecting.await, Err(e) if e.is_cancelled()));
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut remaining))
            .await
            .unwrap()
            .unwrap();
    }
}

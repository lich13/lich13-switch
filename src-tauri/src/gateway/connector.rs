use hyper::Uri;
use hyper_util::{
    client::legacy::connect::{Connected, Connection},
    rt::TokioIo,
};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
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
        Some(ConnectError::TargetTimeout) => "CONNECT_TIMEOUT",
        Some(ConnectError::Tls) => "TLS_HANDSHAKE_FAILED",
        Some(ConnectError::Loop) => "GATEWAY_LOOP",
        _ => "CONNECTION_FAILED",
    }
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
                .is_some_and(|p| p.lock().unwrap().contains(&port))
    }
    pub async fn connect(&self, uri: Uri) -> Result<Transport, ConnectError> {
        let host = uri
            .host()
            .ok_or(ConnectError::TargetConnect)?
            .trim_matches(['[', ']']);
        let tls = uri.scheme_str() == Some("https");
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        if self.is_gateway_port(port)
            && (host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()))
        {
            return Err(ConnectError::Loop);
        }
        let tcp = {
            tokio::time::timeout(self.timeout, async {
                let addresses = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|_| ConnectError::TargetConnect)?;
                let mut stream = None;
                for address in addresses {
                    if self.is_gateway_port(port) && address.ip().is_loopback() {
                        return Err(ConnectError::Loop);
                    }
                    if let Ok(s) = TcpStream::connect(address).await {
                        stream = Some(s);
                        break;
                    }
                }
                stream.ok_or(ConnectError::TargetConnect)
            })
            .await
            .map_err(|_| ConnectError::TargetTimeout)??
        };
        let _ = tcp.set_nodelay(true);
        let stream: Box<dyn Stream> = if tls {
            let server = ServerName::try_from(host.to_owned()).map_err(|_| ConnectError::Tls)?;
            Box::new(
                tokio::time::timeout(self.timeout, self.tls.connect(server, tcp))
                    .await
                    .map_err(|_| ConnectError::Tls)?
                    .map_err(|_| ConnectError::Tls)?,
            )
        } else {
            Box::new(tcp)
        };
        Ok(Transport(TokioIo::new(stream)))
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

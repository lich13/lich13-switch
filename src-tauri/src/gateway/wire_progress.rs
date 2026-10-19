//! Observe socket progress without changing bytes or treating control heartbeats as generation data.
use std::{
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Instant,
};

// Hyper stops polling reads during a response when half-close is allowed.
// Share the existing read half so a non-consuming waiter can still see a reset.
// Error-only readiness excludes established-connection resets on Windows.
// Read readiness plus MSG_PEEK preserves both request bytes and valid read FINs.
pub struct Tcp {
    read: Arc<tokio::net::tcp::OwnedReadHalf>,
    write: tokio::net::tcp::OwnedWriteHalf,
}
pub fn observed_tcp(
    socket: tokio::net::TcpStream,
    activity: Arc<Activity>,
) -> (Tcp, impl std::future::Future<Output = std::io::Error>) {
    let (read, write) = socket.into_split();
    let read = Arc::new(read);
    let monitor = read.clone();
    let failed = async move {
        let socket = monitor.as_ref().as_ref();
        let mut byte = [0];
        loop {
            // The async readiness waiter is independent of Hyper's poll_read
            // waker. Peeking never removes data that Hyper still needs to read.
            match socket.peek(&mut byte).await {
                Err(error) => return error,
                Ok(0) => activity.read_eof(),
                Ok(_) => {}
            }
            match socket.take_error() {
                Ok(Some(error)) | Err(error) => return error,
                Ok(None) => {}
            }
            // Buffered data and FIN remain readable until Hyper consumes them
            // or the connection ends. Neither establishes response cancellation.
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    };
    (Tcp { read, write }, failed)
}
impl AsyncRead for Tcp {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let socket = self.read.as_ref().as_ref();
        loop {
            std::task::ready!(socket.poll_read_ready(cx))?;
            match socket.try_read(buf.initialize_unfilled()) {
                Ok(size) => {
                    buf.advance(size);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}
impl AsyncWrite for Tcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}
#[derive(Default)]
struct ReadProgress {
    header: Vec<u8>,
    remaining: u64,
    data: bool,
    at: Option<Instant>,
}
impl ReadProgress {
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.remaining > 0 {
                let used = self.remaining.min(bytes.len() as u64) as usize;
                if self.data {
                    self.at = Some(Instant::now());
                }
                self.remaining -= used as u64;
                bytes = &bytes[used..];
                continue;
            }
            let byte = bytes[0];
            bytes = &bytes[1..];
            self.header.push(byte);
            if self.header.len() == 1 {
                self.data = matches!(byte & 0x0f, 0..=2);
            }
            if self.data {
                self.at = Some(Instant::now());
            }
            if self.header.len() < 2 {
                continue;
            }
            let small = self.header[1] & 127;
            let ext = match small {
                126 => 2,
                127 => 8,
                _ => 0,
            };
            let size = 2 + ext + if self.header[1] & 128 != 0 { 4 } else { 0 };
            if self.header.len() < size {
                continue;
            }
            self.remaining = if ext == 0 {
                small as u64
            } else {
                self.header[2..2 + ext]
                    .iter()
                    .fold(0_u64, |n, b| (n << 8) | *b as u64)
            };
            self.header.clear();
        }
    }
}
pub struct Activity {
    read: Mutex<ReadProgress>,
    write: Mutex<Instant>,
    websocket: std::sync::atomic::AtomicBool,
    io_error: std::sync::atomic::AtomicU8,
    evidence: Mutex<crate::usage::model::ConnectionEvidence>,
    sequence: std::sync::atomic::AtomicU64,
    eof: std::sync::atomic::AtomicBool,
    deliveries: Mutex<Vec<Weak<super::protocol::Delivery>>>,
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            read: Mutex::new(ReadProgress::default()),
            write: Mutex::new(Instant::now()),
            websocket: Default::default(),
            io_error: Default::default(),
            evidence: Mutex::new(crate::usage::model::ConnectionEvidence {
                id: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            }),
            sequence: Default::default(),
            eof: Default::default(),
            deliveries: Default::default(),
        }
    }
}
impl Activity {
    fn read_eof(&self) {
        if !self.eof.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.event("peer_read_eof");
        }
    }
    pub fn track(&self, delivery: &Arc<super::protocol::Delivery>) {
        let mut deliveries = self.deliveries.lock().unwrap();
        deliveries.retain(|d| d.upgrade().is_some_and(|d| !d.is_settled()));
        deliveries.push(Arc::downgrade(delivery));
    }
    fn flushed(&self) {
        self.deliveries.lock().unwrap().retain(|delivery| {
            let Some(delivery) = delivery.upgrade() else {
                return false;
            };
            if delivery.is_settled() {
                return false;
            }
            if delivery.finished.load(std::sync::atomic::Ordering::Acquire) {
                self.event("socket_flush_complete");
                delivery.settle();
                return false;
            }
            true
        });
    }
    pub fn connection_failed(&self, code: u8) {
        for delivery in self
            .deliveries
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|d| d.upgrade())
        {
            delivery.failed(code);
        }
    }
    pub fn begin_request(&self) -> u64 {
        let mut evidence = self.evidence.lock().unwrap();
        evidence.request_sequence += 1;
        evidence.request_sequence
    }
    pub fn snapshot(&self, request_sequence: u64) -> crate::usage::model::ConnectionEvidence {
        let mut snapshot = self.evidence.lock().unwrap().clone();
        snapshot.request_sequence = request_sequence;
        snapshot
    }
    pub fn event(&self, kind: &str) {
        let mut evidence = self.evidence.lock().unwrap();
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if evidence.events.len() == 24 {
            evidence.events.remove(0);
        }
        evidence.events.push(crate::usage::model::TransportEvent {
            sequence,
            at: crate::usage::model::now(),
            kind: kind.into(),
        });
    }
    pub fn error_code(&self) -> u8 {
        self.io_error.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn failed(&self, error: &std::io::Error, write: bool) {
        let code = if matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ) {
            6
        } else if write && error.kind() == std::io::ErrorKind::BrokenPipe {
            7
        } else {
            2
        };
        self.io_error
            .store(code, std::sync::atomic::Ordering::Release);
        self.event(if code == 6 {
            "peer_reset"
        } else if write {
            "socket_write_failed"
        } else {
            "socket_read_failed"
        });
        self.connection_failed(code);
    }

    pub fn websocket(&self) {
        self.websocket
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn read_at(&self) -> Option<Instant> {
        self.read.lock().unwrap().at
    }
    pub fn write_at(&self) -> Instant {
        *self.write.lock().unwrap()
    }
}
pub struct Tracked<S> {
    inner: S,
    pub activity: Arc<Activity>,
}
impl<S> Tracked<S> {
    pub fn new(inner: S, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Err(error)) = &result {
            self.activity.failed(error, false);
        }
        if matches!(result, Poll::Ready(Ok(()))) {
            let size = buf.filled().len() - before;
            self.activity.evidence.lock().unwrap().read_bytes += size as u64;
            if size == 0 && buf.remaining() > 0 {
                self.activity.read_eof();
            }
        }
        if matches!(result, Poll::Ready(Ok(())))
            && self
                .activity
                .websocket
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.activity
                .read
                .lock()
                .unwrap()
                .feed(&buf.filled()[before..]);
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Err(error)) = &result {
            self.activity.failed(error, true);
        }
        if let Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                *self.activity.write.lock().unwrap() = Instant::now();
                self.activity.evidence.lock().unwrap().socket_written_bytes += *n as u64;
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.activity.failed(error, true);
        } else if matches!(result, Poll::Ready(Ok(()))) {
            self.activity.flushed();
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(result, Poll::Ready(_)) {
            self.activity.event("socket_shutdown");
        }
        result
    }
}
pub async fn writing<F: std::future::Future>(
    future: F,
    activity: &Activity,
    seconds: u64,
) -> Result<F::Output, ()> {
    let start = Instant::now();
    let mut deadline = start + std::time::Duration::from_secs(seconds);
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = tokio::time::sleep_until(deadline) => {
                let progress = activity.write_at();
                let next = progress + std::time::Duration::from_secs(seconds);
                if progress > start && next > Instant::now() { deadline = next; } else { return Err(()); }
            }
        }
    }
}

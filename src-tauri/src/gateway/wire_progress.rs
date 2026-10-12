//! Observe socket progress without changing bytes or treating control heartbeats as generation data.
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Instant,
};
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
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            read: Mutex::new(ReadProgress::default()),
            write: Mutex::new(Instant::now()),
            websocket: Default::default(),
            io_error: Default::default(),
        }
    }
}
impl Activity {
    pub fn error_code(&self) -> u8 {
        self.io_error.load(std::sync::atomic::Ordering::Acquire)
    }
    fn failed(&self, error: &std::io::Error, write: bool) {
        let code = if matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ) {
            6
        } else if write {
            7
        } else {
            2
        };
        self.io_error
            .store(code, std::sync::atomic::Ordering::Release);
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
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.activity.failed(error, true);
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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

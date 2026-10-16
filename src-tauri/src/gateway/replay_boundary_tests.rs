use super::replay::Replay;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame};
use std::{
    fs, io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tempfile::tempdir;
use tokio::sync::Notify;

const TWO_MIB: usize = 2 * 1024 * 1024;
const LARGE_LEGAL_REQUEST: usize = 65 * 1024 * 1024;

fn fixture_bytes(length: usize) -> Bytes {
    Bytes::from(
        (0..length)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>(),
    )
}

fn entries(path: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

struct ErrorAfterFirst {
    first: Option<Bytes>,
}

impl Body for ErrorAfterFirst {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.first.take() {
            Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            None => Poll::Ready(Some(Err(io::Error::other("fixture body failure")))),
        }
    }
}

struct PendingAfterFirst {
    first: Option<Bytes>,
    pending: Arc<Notify>,
}

impl Body for PendingAfterFirst {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.first.take() {
            Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            None => {
                self.pending.notify_one();
                Poll::Pending
            }
        }
    }
}

#[tokio::test]
async fn replay_uses_memory_at_two_mib_and_spools_one_byte_above() {
    for (length, expected_files) in [(TWO_MIB, 0), (TWO_MIB + 1, 1)] {
        let fixture = tempdir().unwrap();
        let payload = fixture_bytes(length);
        let replay = Replay::capture_progress(Full::new(payload.clone()), fixture.path(), None)
            .await
            .unwrap();

        assert_eq!(replay.length, length as u64);
        assert_eq!(entries(fixture.path()).len(), expected_files);
        let actual = replay.body().collect().await.unwrap().to_bytes();
        assert_eq!(actual, payload);

        drop(replay);
        assert!(entries(fixture.path()).is_empty());
    }
}

#[tokio::test]
async fn legal_sixty_five_mib_request_is_spooled_and_round_trips_exact_bytes() {
    let fixture = tempdir().unwrap();
    let payload = fixture_bytes(LARGE_LEGAL_REQUEST);
    let replay = Replay::capture_progress(Full::new(payload.clone()), fixture.path(), None)
        .await
        .unwrap();

    assert_eq!(replay.length, LARGE_LEGAL_REQUEST as u64);
    let files = entries(fixture.path());
    assert_eq!(
        files.len(),
        1,
        "large request should have one private spool file"
    );
    assert_eq!(
        fs::metadata(&files[0]).unwrap().len(),
        LARGE_LEGAL_REQUEST as u64
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let actual = replay.body().collect().await.unwrap().to_bytes();
    assert_eq!(actual, payload);

    drop(replay);
    assert!(
        entries(fixture.path()).is_empty(),
        "spool file should be removed after replay drop"
    );
}

#[tokio::test]
async fn body_error_after_spooling_cleans_the_partial_spool_file() {
    let fixture = tempdir().unwrap();
    let body = ErrorAfterFirst {
        first: Some(fixture_bytes(TWO_MIB + 1)),
    };

    let result = Replay::capture_progress(body, fixture.path(), None).await;

    assert!(result.is_err());
    assert!(entries(fixture.path()).is_empty());
}

#[tokio::test]
async fn cancelling_capture_while_waiting_after_spooling_cleans_the_spool_file() {
    let fixture = tempdir().unwrap();
    let pending = Arc::new(Notify::new());
    let body = PendingAfterFirst {
        first: Some(fixture_bytes(TWO_MIB + 1)),
        pending: pending.clone(),
    };
    let spool_dir = fixture.path().to_path_buf();
    let capture =
        tokio::spawn(async move { Replay::capture_progress(body, &spool_dir, None).await });

    tokio::time::timeout(Duration::from_secs(5), pending.notified())
        .await
        .expect("capture did not reach the pending body frame");
    assert_eq!(entries(fixture.path()).len(), 1);

    capture.abort();
    match capture.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("capture unexpectedly completed after abort"),
    }
    assert!(entries(fixture.path()).is_empty());
}

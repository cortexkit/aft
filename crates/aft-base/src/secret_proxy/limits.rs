//! Bounded concurrency and activity-based deadlines for command-local sockets.
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

pub(super) const CONNECTIONS: usize = 16;
pub(super) const REQUESTS: usize = 32;
pub(super) const IDLE: Duration = Duration::from_secs(30);
pub(super) const CODINGS: usize = 4;
pub(super) const MATCHERS: usize = 32;
pub(super) const MATERIAL_BYTES: usize = 4096;

const REFUSED_UPLOAD_DRAIN_BYTES: usize = 8 * 1024 * 1024;
const REFUSED_UPLOAD_DRAIN_TIME: Duration = Duration::from_secs(1);

struct RefusedUpload {
    enabled: Arc<AtomicBool>,
    deadline: Option<Pin<Box<Sleep>>>,
    remaining: usize,
}

pub(super) struct IdleIo<T> {
    inner: T,
    timeout: Duration,
    timer: Pin<Box<Sleep>>,
    approval_waits: Option<std::sync::Arc<crate::secret::requests::RequestActivity>>,
    last_activity: Option<Instant>,
    refused_upload: Option<RefusedUpload>,
}

impl<T> IdleIo<T> {
    pub(super) fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            timer: Box::pin(tokio::time::sleep(timeout)),
            approval_waits: None,
            last_activity: None,
            refused_upload: None,
        }
    }
    pub(super) fn with_approval_waits(
        mut self,
        waits: std::sync::Arc<crate::secret::requests::RequestActivity>,
    ) -> Self {
        self.approval_waits = Some(waits);
        self
    }
    pub(super) fn with_refused_upload_drain(mut self, enabled: Arc<AtomicBool>) -> Self {
        self.refused_upload = Some(RefusedUpload {
            enabled,
            deadline: None,
            remaining: REFUSED_UPLOAD_DRAIN_BYTES,
        });
        self
    }
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        // A held approval has its own ten-minute deadline. Quiet sockets must
        // remain usable long enough to return the card's result to the client.
        if let Some(activity) = &self.approval_waits {
            let (waiting, last_activity) = activity.snapshot();
            if last_activity != self.last_activity {
                self.last_activity = last_activity;
                if let Some(at) = last_activity {
                    // Approval completion and a fresh TLS-verified upstream
                    // connection must leave time to send the client's response.
                    // A later socket read/write must retain its newer budget.
                    let deadline = self.timer.deadline().max(at + self.timeout);
                    self.timer.as_mut().reset(deadline);
                }
            }
            if waiting {
                let _ = self.timer.as_mut().poll(cx);
                self.touch();
                return false;
            }
        }
        self.timer.as_mut().poll(cx).is_ready()
    }
    fn touch(&mut self) {
        self.timer.as_mut().reset(Instant::now() + self.timeout);
    }
}

fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, super::PROXY_REFUSAL)
}

impl<T: AsyncRead + Unpin> AsyncRead for IdleIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timeout()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    this.touch();
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for IdleIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timeout()));
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(count)) => {
                if count > 0 {
                    this.touch();
                }
                Poll::Ready(Ok(count))
            }
            other => other,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timeout()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Some(drain) = this
            .refused_upload
            .as_mut()
            .filter(|drain| drain.enabled.load(Ordering::Acquire))
        else {
            return Pin::new(&mut this.inner).poll_shutdown(cx);
        };
        if drain.deadline.is_none() {
            // TLS has flushed the refusal and its close-notify. Half-close the
            // TCP write side before discarding unread ciphertext, so closing
            // an active upload does not reset the connection under the reply.
            std::task::ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
        }
        let deadline = drain
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(REFUSED_UPLOAD_DRAIN_TIME)));
        let mut scratch = [0; 8192];
        while drain.remaining > 0 && deadline.as_mut().poll(cx).is_pending() {
            let len = drain.remaining.min(scratch.len());
            let mut buf = ReadBuf::new(&mut scratch[..len]);
            match std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buf)) {
                Ok(()) if !buf.filled().is_empty() => drain.remaining -= buf.filled().len(),
                _ => break,
            }
        }
        drain.remaining = 0;
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::requests::RequestActivity;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn refused_upload_half_closes_even_if_the_peer_never_finishes() {
        let (socket, mut peer) = tokio::io::duplex(64);
        let mut socket =
            IdleIo::new(socket, IDLE).with_refused_upload_drain(Arc::new(AtomicBool::new(true)));
        let mut shutdown = Box::pin(socket.shutdown());
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        tokio::time::advance(REFUSED_UPLOAD_DRAIN_TIME / 2).await;
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        tokio::time::advance(REFUSED_UPLOAD_DRAIN_TIME).await;
        assert!(futures_util::poll!(&mut shutdown).is_ready());
    }

    #[tokio::test(start_paused = true)]
    async fn refused_upload_drains_no_more_than_its_byte_budget() {
        let (socket, mut peer) = tokio::io::duplex(64);
        let mut socket =
            IdleIo::new(socket, IDLE).with_refused_upload_drain(Arc::new(AtomicBool::new(true)));
        socket.refused_upload.as_mut().unwrap().remaining = 3;
        peer.write_all(b"abcd").await.unwrap();
        // The byte budget ends shutdown without waiting for the read deadline.
        assert!(futures_util::poll!(Box::pin(socket.shutdown())).is_ready());
        assert_eq!(socket.inner.read_u8().await.unwrap(), b'd');
    }

    #[tokio::test(start_paused = true)]
    async fn approval_release_starts_a_fresh_production_idle_budget() {
        let waits = Arc::new(RequestActivity::default());
        waits.begin_wait();
        let (socket, _peer) = tokio::io::duplex(64);
        let mut socket = IdleIo::new(socket, IDLE).with_approval_waits(waits.clone());
        assert!(futures_util::poll!(Box::pin(socket.read_u8())).is_pending());
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        assert!(futures_util::poll!(Box::pin(socket.read_u8())).is_pending());
        // No socket polls accompany this part of the approval hold. Releasing
        // it must revive the idle budget even when the old deadline elapsed.
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        waits.finish_wait();
        socket
            .write_all(b"response")
            .await
            .expect("approval release must leave time to return the response");
        tokio::time::advance(IDLE - Duration::from_secs(1)).await;
        socket.flush().await.unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            socket.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn verified_reconnect_starts_a_fresh_downstream_idle_budget() {
        let activity = Arc::new(RequestActivity::default());
        let (socket, _peer) = tokio::io::duplex(64);
        let mut socket = IdleIo::new(socket, IDLE).with_approval_waits(activity.clone());
        tokio::time::advance(IDLE - Duration::from_secs(1)).await;
        activity.record_activity();
        tokio::time::advance(Duration::from_secs(2)).await;
        socket.write_all(b"response").await.unwrap();
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        assert_eq!(
            socket.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_approval_budget_starts_at_release_not_the_next_socket_poll() {
        let activity = Arc::new(RequestActivity::default());
        activity.begin_wait();
        let (socket, _peer) = tokio::io::duplex(64);
        let mut socket = IdleIo::new(socket, IDLE).with_approval_waits(activity.clone());
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        activity.finish_wait();
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        assert_eq!(
            socket.write_all(b"late").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unheld_sockets_keep_the_production_idle_deadline() {
        let (socket, _peer) = tokio::io::duplex(64);
        let mut socket = IdleIo::new(socket, IDLE);
        tokio::time::advance(IDLE + Duration::from_secs(1)).await;
        assert_eq!(
            socket.write_all(b"late").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}

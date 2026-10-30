//! Bounded concurrency and activity-based deadlines for command-local sockets.
use std::future::Future;
use std::io;
use std::pin::Pin;
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

pub(super) struct IdleIo<T> {
    inner: T,
    timeout: Duration,
    timer: Pin<Box<Sleep>>,
}

impl<T> IdleIo<T> {
    pub(super) fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            timer: Box::pin(tokio::time::sleep(timeout)),
        }
    }
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
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

impl<T: AsyncWrite + Unpin> AsyncWrite for IdleIo<T> {
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
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

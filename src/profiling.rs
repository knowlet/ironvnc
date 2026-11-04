//! Opt-in CPU pipeline observations. These are wall-clock spans, not GPU timings.
//!
//! No samples are retained here: one small metadata event travels with each
//! already credit-bounded update. The diagnostic consumer decides its sample cap.
use std::{
    cell::Cell,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Enable before connecting. Normal connections do not call the clock per read.
pub fn enabled() -> bool {
    std::env::var_os("IRONVNC_PROFILE").is_some_and(|v| v == "1")
}

#[derive(Debug)]
pub struct UpdateProfile {
    pub sequence: u64,
    /// First byte of the server update has been received; excludes server wait.
    pub started: Instant,
    /// Just before the metadata/pixel events enter the FIFO.
    pub queued: Instant,
    /// Negotiated stream read wall time, including pending waits and ready polls.
    /// Buffered reads / decryption may occur here: this is not network CPU time.
    pub receive: Duration,
    /// Update parsing, decoding and framebuffer mutation, INCLUDING receive.
    pub decode_apply: Duration,
    /// Coarse framebuffer mutation spans (blit_rgba / copy_rect only).
    /// Per-pixel set_pixel decoders are intentionally not clocked.
    pub framebuffer: Duration,
    /// False if an encoding uses uninstrumented per-pixel mutation or resize.
    pub framebuffer_complete: bool,
    pub snapshot: Duration,
    pub credit_wait: Duration,
    pub payload_bytes: usize,
}

thread_local! {
    static FRAMEBUFFER_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static FRAMEBUFFER_TIME: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

pub struct FramebufferScope(Option<Instant>);
/// Time coarse mutations only; never place this in a per-pixel loop.
pub fn framebuffer_scope() -> FramebufferScope {
    FramebufferScope(FRAMEBUFFER_ACTIVE.with(|active| active.get().then(Instant::now)))
}
impl Drop for FramebufferScope {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            FRAMEBUFFER_TIME.with(|time| time.set(time.get() + started.elapsed()));
        }
    }
}
pub struct FramebufferMeasurement;
impl Drop for FramebufferMeasurement {
    fn drop(&mut self) {
        FRAMEBUFFER_ACTIVE.with(|active| active.set(false));
    }
}
/// Guard also disables collection when a decoder errors or is cancelled.
pub fn begin_framebuffer(enabled: bool) -> FramebufferMeasurement {
    FRAMEBUFFER_ACTIVE.with(|active| active.set(enabled));
    FRAMEBUFFER_TIME.with(|time| time.set(Duration::ZERO));
    FramebufferMeasurement
}
pub fn finish_framebuffer() -> Duration {
    FRAMEBUFFER_ACTIVE.with(|active| active.set(false));
    FRAMEBUFFER_TIME.with(Cell::get)
}

/// Measures only reads within an explicitly started update. Reads between updates
/// (including the cancellable message-type read) are intentionally not timed.
pub struct TimedStream<S> {
    inner: S,
    active: bool,
    pending_since: Option<Instant>,
    read_time: Duration,
}
impl<S> TimedStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            active: false,
            pending_since: None,
            read_time: Duration::ZERO,
        }
    }
    pub fn begin_update(&mut self, enabled: bool) {
        self.active = enabled;
        self.pending_since = None;
        self.read_time = Duration::ZERO;
    }
    pub fn finish_update(&mut self) -> Duration {
        self.active = false;
        self.pending_since = None;
        self.read_time
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for TimedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.active {
            return Pin::new(&mut self.inner).poll_read(cx, buf);
        }
        let start = self.pending_since.take().unwrap_or_else(Instant::now);
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        match result {
            Poll::Pending => self.pending_since = Some(start),
            Poll::Ready(_) => self.read_time += start.elapsed(),
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for TimedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn framebuffer_scopes_are_opt_in_and_reset() {
        let _measurement = begin_framebuffer(false);
        drop(framebuffer_scope());
        assert_eq!(finish_framebuffer(), Duration::ZERO);
        let _measurement = begin_framebuffer(true);
        {
            let _scope = framebuffer_scope();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(finish_framebuffer() >= Duration::from_millis(1));
        let _measurement = begin_framebuffer(false);
        assert_eq!(finish_framebuffer(), Duration::ZERO);
    }

    #[test]
    fn dropping_measurement_disables_later_scopes() {
        {
            let _measurement = begin_framebuffer(true);
        }
        drop(framebuffer_scope());
        assert_eq!(finish_framebuffer(), Duration::ZERO);
    }

    #[tokio::test]
    async fn failed_read_is_forwarded_and_next_update_resets() {
        let mut stream = TimedStream::new(&b""[..]);
        stream.begin_update(true);
        assert_eq!(
            stream.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        stream.finish_update();
        stream.begin_update(false);
        assert_eq!(stream.finish_update(), Duration::ZERO);
    }

    #[tokio::test]
    async fn disabled_reads_are_unmeasured_and_bytes_preserved() {
        let mut stream = TimedStream::new(&b"abcd"[..]);
        stream.begin_update(false);
        let mut buf = [0; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abcd");
        assert_eq!(stream.finish_update(), Duration::ZERO);
    }

    #[tokio::test]
    async fn pending_read_wait_is_measured_and_reset_between_updates() {
        let (reader, mut writer) = tokio::io::duplex(16);
        let mut stream = TimedStream::new(reader);
        stream.begin_update(true);
        let send = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            writer.write_all(b"x").await.unwrap();
        });
        assert_eq!(stream.read_u8().await.unwrap(), b'x');
        send.await.unwrap();
        assert!(stream.finish_update() >= Duration::from_millis(10));
        stream.begin_update(false);
        assert_eq!(stream.finish_update(), Duration::ZERO);
    }
}

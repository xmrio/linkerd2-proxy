use super::Poll;
use bytes::{Buf, BufMut};
use pin_project::pin_project;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::Context;
use tokio::io::{AsyncRead, AsyncWrite};

#[pin_project]
#[derive(Debug)]
pub struct Instrumented<T> {
    #[pin]
    io: T,
    span: tracing::Span,
}

impl<T> Instrumented<T> {
    pub fn new(io: T, span: tracing::Span) -> Self {
        Self { io, span }
    }
}

impl<T: AsyncRead> AsyncRead for Instrumented<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<usize> {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_read(cx, buf);
        tracing::trace!(poll_read = ?result);
        result
    }

    fn poll_read_buf<B: BufMut>(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut B,
    ) -> Poll<usize> {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_read_buf(cx, buf);
        tracing::trace!(poll_read_buf = ?result);
        result
    }

    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncWrite> AsyncWrite for Instrumented<T> {
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_shutdown(cx);
        tracing::trace!(poll_shutdown = ?result);
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_flush(cx);
        tracing::trace!(poll_flush = ?result);
        result
    }

    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<usize> {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_write(cx, buf);
        tracing::trace!(poll_write = ?result);
        result
    }

    fn poll_write_buf<B: Buf>(self: Pin<&mut Self>, cx: &mut Context, buf: &mut B) -> Poll<usize>
    where
        Self: Sized,
    {
        let this = self.project();
        let _e = this.span.enter();
        let result = this.io.poll_write_buf(cx, buf);
        tracing::trace!(poll_write_buf = ?result);
        result
    }
}

use bytes::{Bytes, BytesMut};
use futures::{executor, SinkExt};
use std::io;
use std::io::Write;
use std::marker::PhantomData;
use std::pin::Pin;
use tokio_postgres::{CopyInSink, Error};

/// The writer returned by the `copy_in` method.
///
/// The copy *must* be explicitly completed via the `finish` method. If it is not, the copy will be aborted.
pub struct CopyInWriter<'a> {
    sink: Pin<Box<CopyInSink<Bytes>>>,
    buf: BytesMut,
    _p: PhantomData<&'a mut ()>,
}

// no-op impl to extend borrow until drop
impl Drop for CopyInWriter<'_> {
    fn drop(&mut self) {}
}

impl<'a> CopyInWriter<'a> {
    pub(crate) fn new(sink: CopyInSink<Bytes>) -> CopyInWriter<'a> {
        CopyInWriter {
            sink: Box::pin(sink),
            buf: BytesMut::new(),
            _p: PhantomData,
        }
    }

    /// Completes the copy, returning the number of rows written.
    ///
    /// If this is not called, the copy will be aborted.
    pub fn finish(mut self) -> Result<u64, Error> {
        self.flush_inner()?;
        executor::block_on(self.sink.as_mut().finish())
    }

    fn flush_inner(&mut self) -> Result<(), Error> {
        if self.buf.is_empty() {
            return Ok(());
        }

        executor::block_on(self.sink.as_mut().send(self.buf.split().freeze()))
    }
}

impl Write for CopyInWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buf.len() > 4096 {
            self.flush()?;
        }

        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_inner()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

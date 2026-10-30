//! Bounded in-memory rolling log for diagnostics.
//!
//! The `tracing` fmt layer writes into a byte-capped ring shared with a
//! [`LogRingHandle`], which snapshots or dumps the recent log on demand
//! (`/bug`, the panic hook) instead of keeping a rolling file.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::fmt::MakeWriter;

/// Default byte capacity of the ring.
pub const DEFAULT_LOG_RING_CAPACITY: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct RingBuffer {
    chunks: VecDeque<String>,
    bytes: usize,
    capacity: usize,
}

impl RingBuffer {
    const fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            capacity,
        }
    }

    fn push(&mut self, text: String) {
        self.bytes += text.len();
        self.chunks.push_back(text);
        while self.bytes > self.capacity && self.chunks.len() > 1 {
            let oldest = self.chunks.pop_front().expect("more than one chunk");
            self.bytes -= oldest.len();
        }
        // A single write larger than the cap keeps its tail: the newest
        // bytes are what a report needs.
        if self.bytes > self.capacity
            && let Some(front) = self.chunks.front_mut()
        {
            let excess = self.bytes - self.capacity;
            let keep_from = front
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= excess)
                .unwrap_or(front.len());
            front.replace_range(..keep_from, "");
            self.bytes -= keep_from;
        }
    }

    fn snapshot(&self) -> String {
        let mut out = String::with_capacity(self.bytes);
        for chunk in &self.chunks {
            out.push_str(chunk);
        }
        out
    }
}

fn lock_ring(inner: &Mutex<RingBuffer>) -> MutexGuard<'_, RingBuffer> {
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`MakeWriter`] target sharing the ring with a [`LogRingHandle`].
#[derive(Debug, Clone)]
pub struct RingMakeWriter {
    inner: Arc<Mutex<RingBuffer>>,
}

impl<'a> MakeWriter<'a> for RingMakeWriter {
    type Writer = RingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RingWriter {
            inner: self.inner.clone(),
        }
    }
}

/// `io::Write` end produced by [`RingMakeWriter`]; each write is one
/// buffered chunk of the ring.
#[derive(Debug)]
pub struct RingWriter {
    inner: Arc<Mutex<RingBuffer>>,
}

impl io::Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock_ring(&self.inner).push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Read side of the log ring; clones share the same buffer.
#[derive(Debug, Clone)]
pub struct LogRingHandle {
    inner: Arc<Mutex<RingBuffer>>,
}

impl LogRingHandle {
    /// Create a writer/handle pair whose ring holds `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> (RingMakeWriter, Self) {
        let inner = Arc::new(Mutex::new(RingBuffer::new(capacity)));
        (
            RingMakeWriter {
                inner: inner.clone(),
            },
            Self { inner },
        )
    }

    /// The buffered log, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> String {
        lock_ring(&self.inner).snapshot()
    }

    /// Write `header` followed by the ring snapshot to `path`, creating the
    /// parent directory when needed. Synchronous: also used from the panic
    /// hook, where no executor may be running.
    ///
    /// # Errors
    /// Returns the underlying IO error when the dump cannot be written.
    pub fn dump_to(&self, path: &Path, header: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{header}\n\n{}", self.snapshot()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::{LogRingHandle, RingBuffer};

    #[test]
    fn ring_keeps_newest_entries_within_cap() {
        let mut ring = RingBuffer::new(16);
        ring.push("first\n".to_string());
        ring.push("second\n".to_string());
        ring.push("third\n".to_string());
        assert_eq!(ring.snapshot(), "second\nthird\n");
    }

    #[test]
    fn ring_writer_pairs_with_handle_snapshot() {
        let (writer, handle) = LogRingHandle::new(64);
        let mut writer = tracing_subscriber::fmt::MakeWriter::make_writer(&writer);
        writer
            .write_all(b"hello ring\n")
            .expect("ring write succeeds");
        assert_eq!(handle.snapshot(), "hello ring\n");
    }

    #[test]
    fn oversized_single_write_keeps_its_tail() {
        let mut ring = RingBuffer::new(8);
        ring.push("0123456789abcdef".to_string());
        assert_eq!(ring.snapshot(), "89abcdef");
    }

    #[test]
    fn snapshot_preserves_write_order() {
        let mut ring = RingBuffer::new(1024);
        for index in 0..5 {
            ring.push(format!("line {index}\n"));
        }
        assert_eq!(ring.snapshot(), "line 0\nline 1\nline 2\nline 3\nline 4\n");
    }
}

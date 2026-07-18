use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use crate::templater::Cache;

use super::{CloseFn, Output, SharedWriter, Wrapped};

/// Buffers all command output and flushes it as a single block on close. When
/// `begin`/`end` are set they wrap the block (after template substitution).
/// When `error_only` is set the block is discarded unless the command failed.
pub struct Group {
    pub begin: String,
    pub end: String,
    pub error_only: bool,
}

/// The shared buffer both wrapped writers append to.
struct GroupBuf {
    dest: SharedWriter,
    buff: Vec<u8>,
    begin: String,
    end: String,
}

impl GroupBuf {
    /// Flushes the buffered output to the destination, wrapping it in the
    /// begin/end markers when either is set. A completely empty buffer emits
    /// nothing.
    fn flush_block(&mut self) -> std::io::Result<()> {
        if self.buff.is_empty() {
            return Ok(());
        }
        let mut dest = self.dest.borrow_mut();
        if self.begin.is_empty() && self.end.is_empty() {
            return dest.write_all(&self.buff);
        }
        dest.write_all(self.begin.as_bytes())?;
        dest.write_all(&self.buff)?;
        dest.write_all(self.end.as_bytes())
    }
}

/// A writer that appends to a shared [`GroupBuf`].
struct GroupWriter(Rc<RefCell<GroupBuf>>);

impl Write for GroupWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().buff.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Output for Group {
    fn wrap_writer(
        &self,
        std_out: SharedWriter,
        _std_err: SharedWriter,
        _prefix: &str,
        mut cache: Option<&mut Cache>,
    ) -> Wrapped {
        let begin = template_marker(&self.begin, &mut cache);
        let end = template_marker(&self.end, &mut cache);

        let group = Rc::new(RefCell::new(GroupBuf {
            dest: std_out,
            buff: Vec::new(),
            begin,
            end,
        }));

        let writer_out: SharedWriter = Rc::new(RefCell::new(GroupWriter(Rc::clone(&group))));
        let writer_err: SharedWriter = Rc::new(RefCell::new(GroupWriter(Rc::clone(&group))));

        let error_only = self.error_only;
        let close: CloseFn = Box::new(move |err| {
            if error_only && err.is_none() {
                return Ok(());
            }
            group.borrow_mut().flush_block()
        });

        Wrapped {
            stdout: writer_out,
            stderr: writer_err,
            close,
        }
    }
}

/// Templates a begin/end marker, appending a trailing newline. An empty marker
/// stays empty.
fn template_marker(marker: &str, cache: &mut Option<&mut Cache>) -> String {
    if marker.is_empty() {
        return String::new();
    }
    let replaced = match cache {
        Some(cache) => cache.replace(marker),
        None => marker.to_string(),
    };
    format!("{replaced}\n")
}

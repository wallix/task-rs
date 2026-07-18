use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;

use crate::logger::{Color, Logger};
use crate::templater::Cache;

use super::{CloseFn, Output, SharedWriter, Wrapped};

/// The color rotation applied to line prefixes; the color for a prefix is
/// chosen by its first-seen index modulo the sequence length.
pub const PREFIX_COLOR_SEQUENCE: [Color; 12] = crate::logger::PREFIX_COLOR_SEQUENCE;

/// Prefixes every output line with the command's label, coloring the label by
/// a stable per-prefix index so concurrent tasks stay visually distinct.
pub struct Prefixed {
    shared: Rc<RefCell<PrefixedState>>,
}

/// State shared across every writer produced by a single [`Prefixed`]: the
/// prefix-to-color-index map, the next index to assign, and the logger used to
/// color prefixes.
struct PrefixedState {
    seen: HashMap<String, usize>,
    counter: usize,
    logger: Rc<RefCell<Logger>>,
}

impl Prefixed {
    /// Creates a prefixed output style that colors prefixes through `logger`.
    pub fn new(logger: Rc<RefCell<Logger>>) -> Self {
        Prefixed {
            shared: Rc::new(RefCell::new(PrefixedState {
                seen: HashMap::new(),
                counter: 0,
                logger,
            })),
        }
    }
}

impl Output for Prefixed {
    fn wrap_writer(
        &self,
        std_out: SharedWriter,
        _std_err: SharedWriter,
        prefix: &str,
        _cache: Option<&mut Cache>,
    ) -> Wrapped {
        let writer = Rc::new(RefCell::new(PrefixWriter {
            dest: std_out,
            shared: Rc::clone(&self.shared),
            prefix: prefix.to_string(),
            buff: Vec::new(),
        }));

        let close_writer = Rc::clone(&writer);
        let close: CloseFn = Box::new(move |_err| close_writer.borrow_mut().write_lines(true));

        Wrapped {
            stdout: Rc::clone(&writer) as SharedWriter,
            stderr: writer as SharedWriter,
            close,
        }
    }
}

/// A writer that buffers partial lines and emits each complete line prefixed
/// with `[prefix] `.
struct PrefixWriter {
    dest: SharedWriter,
    shared: Rc<RefCell<PrefixedState>>,
    prefix: String,
    buff: Vec<u8>,
}

impl PrefixWriter {
    /// Emits buffered complete lines. When `force` is set the trailing partial
    /// line is emitted too (used on close).
    fn write_lines(&mut self, force: bool) -> std::io::Result<()> {
        loop {
            match self.buff.iter().position(|&b| b == b'\n') {
                Some(idx) => {
                    let end = idx.saturating_add(1);
                    let line: Vec<u8> = self.buff.drain(..end).collect();
                    self.write_line(&line)?;
                }
                None => {
                    if force && !self.buff.is_empty() {
                        let line: Vec<u8> = std::mem::take(&mut self.buff);
                        return self.write_line(&line);
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Writes a single line to the destination with a colored prefix. Empty
    /// lines are dropped; a missing trailing newline is added.
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        let mut text = String::from_utf8_lossy(line).into_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }

        let mut shared = self.shared.borrow_mut();
        let idx = match shared.seen.get(&self.prefix) {
            Some(&idx) => idx,
            None => {
                let idx = shared.counter;
                shared.seen.insert(self.prefix.clone(), idx);
                shared.counter = shared.counter.saturating_add(1);
                idx
            }
        };

        let color = PREFIX_COLOR_SEQUENCE
            .get(idx.checked_rem(PREFIX_COLOR_SEQUENCE.len()).unwrap_or(0))
            .copied()
            .unwrap_or(Color::None);

        let mut dest = self.dest.borrow_mut();
        dest.write_all(b"[")?;
        shared
            .logger
            .borrow()
            .f_outf(&mut *dest, color, &self.prefix);
        dest.write_all(b"] ")?;
        dest.write_all(text.as_bytes())
    }
}

impl Write for PrefixWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buff.extend_from_slice(buf);
        self.write_lines(false)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

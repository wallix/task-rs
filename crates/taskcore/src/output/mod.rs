//! Command-output styles.
//!
//! Each style wraps the destination stdout/stderr streams and returns a pair of
//! writers plus a close callback. The [`Interleaved`] style passes output
//! through unchanged; [`Group`] buffers everything and flushes on close (with
//! optional begin/end templates and error-only gating); [`Prefixed`] prefixes
//! every line with the task's label.

mod group;
mod interleaved;
mod prefixed;

pub use group::Group;
pub use interleaved::Interleaved;
pub use prefixed::{PREFIX_COLOR_SEQUENCE, Prefixed};

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use crate::ast;
use crate::logger::Logger;
use crate::templater::Cache;

/// A destination stream shared between the wrapped writer and the close
/// callback. Output styles buffer into their own state and flush here.
pub type SharedWriter = Rc<RefCell<dyn Write>>;

/// Flushes any buffered output once a command has finished. It receives the
/// command's outcome so styles like error-only group can decide whether to
/// emit anything.
pub type CloseFn = Box<dyn FnOnce(Option<&dyn std::error::Error>) -> std::io::Result<()>>;

/// The writers and close callback produced by wrapping a command's streams.
pub struct Wrapped {
    /// The writer the command should send stdout to.
    pub stdout: SharedWriter,
    /// The writer the command should send stderr to.
    pub stderr: SharedWriter,
    /// Flushes buffered output; call once the command completes.
    pub close: CloseFn,
}

/// An output style: wraps a command's stdout/stderr streams.
pub trait Output {
    /// Wraps the destination streams, returning the writers the command should
    /// use plus a close callback. `prefix` labels the command (used by
    /// [`Prefixed`]); `cache` templates begin/end markers (used by [`Group`]).
    fn wrap_writer(
        &self,
        std_out: SharedWriter,
        std_err: SharedWriter,
        prefix: &str,
        cache: Option<&mut Cache>,
    ) -> Wrapped;

    /// Whether this style passes command output straight through to the
    /// destination streams without buffering or rewriting. When true the engine
    /// lets the command inherit the process streams directly, so its output is
    /// seen live rather than captured and replayed. Only [`Interleaved`]
    /// qualifies; [`Group`] must buffer and [`Prefixed`] rewrites each line.
    fn is_passthrough(&self) -> bool {
        false
    }
}

/// The error returned when an output style cannot be built for an
/// [`ast::Output`].
#[derive(Debug)]
pub enum BuildError {
    /// The output style name is not one of the recognized styles.
    Unrecognized(String),
    /// A begin/end group parameter was set on a style that does not support it.
    GroupUnsupported(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Unrecognized(name) => {
                write!(f, "task: output style {name:?} not recognized")
            }
            BuildError::GroupUnsupported(name) => write!(
                f,
                "task: output style {name:?} does not support the group begin/end parameter"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

/// Builds the output style for the requested [`ast::Output`]. The prefixed
/// style needs a shared logger for coloring line prefixes.
pub fn build_for(
    o: &ast::Output,
    logger: Rc<RefCell<Logger>>,
) -> Result<Box<dyn Output>, BuildError> {
    match o.name.as_str() {
        "interleaved" | "" => {
            check_output_group_unset(o)?;
            Ok(Box::new(Interleaved))
        }
        "group" => Ok(Box::new(Group {
            begin: o.group.begin.clone(),
            end: o.group.end.clone(),
            error_only: o.group.error_only,
        })),
        "prefixed" => {
            check_output_group_unset(o)?;
            Ok(Box::new(Prefixed::new(logger)))
        }
        name => Err(BuildError::Unrecognized(name.to_string())),
    }
}

fn check_output_group_unset(o: &ast::Output) -> Result<(), BuildError> {
    if o.group.is_set() {
        return Err(BuildError::GroupUnsupported(o.name.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Var, VarElement, Vars};

    /// A destination buffer whose contents can be inspected after writes.
    #[derive(Clone, Default)]
    struct Buf(Rc<RefCell<Vec<u8>>>);

    impl Buf {
        fn shared(&self) -> SharedWriter {
            Rc::new(RefCell::new(BufWriter(Rc::clone(&self.0)))) as SharedWriter
        }
        fn contents(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
        fn reset(&self) {
            self.0.borrow_mut().clear();
        }
    }

    struct BufWriter(Rc<RefCell<Vec<u8>>>);
    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn writeln(w: &SharedWriter, s: &str) {
        let mut g = w.borrow_mut();
        g.write_all(s.as_bytes()).unwrap();
        g.write_all(b"\n").unwrap();
    }

    #[test]
    fn interleaved() {
        let b = Buf::default();
        let o = Interleaved;
        let w = o.wrap_writer(b.shared(), b.shared(), "", None);
        writeln(&w.stdout, "foo\nbar");
        assert_eq!(b.contents(), "foo\nbar\n");
        writeln(&w.stdout, "baz");
        assert_eq!(b.contents(), "foo\nbar\nbaz\n");
    }

    #[test]
    fn group_flushes_on_close() {
        let b = Buf::default();
        let o = Group {
            begin: String::new(),
            end: String::new(),
            error_only: false,
        };
        let w = o.wrap_writer(b.shared(), b.shared(), "", None);
        writeln(&w.stdout, "out\nout");
        assert_eq!(b.contents(), "");
        writeln(&w.stderr, "err\nerr");
        assert_eq!(b.contents(), "");
        writeln(&w.stdout, "out");
        writeln(&w.stderr, "err");
        assert_eq!(b.contents(), "");
        (w.close)(None).unwrap();
        assert_eq!(b.contents(), "out\nout\nerr\nerr\nout\nerr\n");
    }

    fn cache_with_var1() -> Cache {
        let vars = Vars::from_elements([VarElement {
            key: "VAR1".to_string(),
            value: Var {
                value: Some(serde_yaml_ng::Value::String("example-value".to_string())),
                ..Default::default()
            },
        }]);
        Cache::new(vars)
    }

    #[test]
    fn group_with_begin_end() {
        let mut cache = cache_with_var1();
        let o = Group {
            begin: "::group::{{ .VAR1 }}".to_string(),
            end: "::endgroup::".to_string(),
            error_only: false,
        };
        let b = Buf::default();
        let w = o.wrap_writer(b.shared(), b.shared(), "", Some(&mut cache));
        writeln(&w.stdout, "foo\nbar");
        assert_eq!(b.contents(), "");
        writeln(&w.stdout, "baz");
        (w.close)(None).unwrap();
        assert_eq!(
            b.contents(),
            "::group::example-value\nfoo\nbar\nbaz\n::endgroup::\n"
        );
    }

    #[test]
    fn group_with_begin_end_no_output() {
        let mut cache = cache_with_var1();
        let o = Group {
            begin: "::group::{{ .VAR1 }}".to_string(),
            end: "::endgroup::".to_string(),
            error_only: false,
        };
        let b = Buf::default();
        let w = o.wrap_writer(b.shared(), b.shared(), "", Some(&mut cache));
        (w.close)(None).unwrap();
        assert_eq!(b.contents(), "");
    }

    #[test]
    fn group_error_only_swallows_on_no_error() {
        let b = Buf::default();
        let o = Group {
            begin: String::new(),
            end: String::new(),
            error_only: true,
        };
        let w = o.wrap_writer(b.shared(), b.shared(), "", None);
        writeln(&w.stdout, "std-out");
        writeln(&w.stderr, "std-err");
        (w.close)(None).unwrap();
        assert_eq!(b.contents(), "");
    }

    #[derive(Debug)]
    struct AnyError;
    impl std::fmt::Display for AnyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "any-error")
        }
    }
    impl std::error::Error for AnyError {}

    #[test]
    fn group_error_only_shows_on_error() {
        let b = Buf::default();
        let o = Group {
            begin: String::new(),
            end: String::new(),
            error_only: true,
        };
        let w = o.wrap_writer(b.shared(), b.shared(), "", None);
        writeln(&w.stdout, "std-out");
        writeln(&w.stderr, "std-err");
        let err = AnyError;
        (w.close)(Some(&err)).unwrap();
        assert_eq!(b.contents(), "std-out\nstd-err\n");
    }

    fn test_logger() -> Rc<RefCell<Logger>> {
        Rc::new(RefCell::new(Logger {
            color: false,
            ..Default::default()
        }))
    }

    #[test]
    fn prefixed_simple() {
        let b = Buf::default();
        let o = Prefixed::new(test_logger());
        let w = o.wrap_writer(b.shared(), b.shared(), "prefix", None);
        writeln(&w.stdout, "foo\nbar");
        assert_eq!(b.contents(), "[prefix] foo\n[prefix] bar\n");
        writeln(&w.stdout, "baz");
        assert_eq!(b.contents(), "[prefix] foo\n[prefix] bar\n[prefix] baz\n");
        (w.close)(None).unwrap();
    }

    #[test]
    fn prefixed_multiple_writes_single_line() {
        let b = Buf::default();
        let o = Prefixed::new(test_logger());
        let w = o.wrap_writer(b.shared(), b.shared(), "prefix", None);
        for ch in ["T", "e", "s", "t", "!"] {
            w.stdout.borrow_mut().write_all(ch.as_bytes()).unwrap();
            assert_eq!(b.contents(), "");
        }
        (w.close)(None).unwrap();
        assert_eq!(b.contents(), "[prefix] Test!\n");
    }

    #[test]
    fn prefixed_colors_loop() {
        let logger = Rc::new(RefCell::new(Logger {
            color: true,
            ..Default::default()
        }));
        let o = Prefixed::new(Rc::clone(&logger));

        for i in 0..16usize {
            let b = Buf::default();
            let prefix = format!("prefix-{i}");
            let w = o.wrap_writer(b.shared(), b.shared(), &prefix, None);

            let color = PREFIX_COLOR_SEQUENCE
                .get(i % PREFIX_COLOR_SEQUENCE.len())
                .copied()
                .unwrap();
            let mut colored = Vec::new();
            logger.borrow().f_outf(&mut colored, color, &prefix);
            let colored = String::from_utf8(colored).unwrap();

            b.reset();
            writeln(&w.stdout, "foo\nbar");
            assert_eq!(b.contents(), format!("[{colored}] foo\n[{colored}] bar\n"));
        }
    }

    #[test]
    fn build_for_unrecognized() {
        let o = ast::Output {
            name: "bogus".to_string(),
            ..Default::default()
        };
        let result = build_for(&o, test_logger());
        assert!(matches!(result, Err(BuildError::Unrecognized(_))));
    }

    #[test]
    fn build_for_group_unsupported_on_prefixed() {
        let o = ast::Output {
            name: "prefixed".to_string(),
            group: ast::OutputGroup {
                begin: "x".to_string(),
                ..Default::default()
            },
        };
        let result = build_for(&o, test_logger());
        assert!(matches!(result, Err(BuildError::GroupUnsupported(_))));
    }
}

use crate::templater::Cache;

use super::{CloseFn, Output, SharedWriter, Wrapped};

/// Passes command output straight through to the destination streams with no
/// buffering or transformation.
pub struct Interleaved;

impl Output for Interleaved {
    fn wrap_writer(
        &self,
        std_out: SharedWriter,
        std_err: SharedWriter,
        _prefix: &str,
        _cache: Option<&mut Cache>,
    ) -> Wrapped {
        let close: CloseFn = Box::new(|_err| Ok(()));
        Wrapped {
            stdout: std_out,
            stderr: std_err,
            close,
        }
    }

    fn is_passthrough(&self) -> bool {
        true
    }
}

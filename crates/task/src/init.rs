//! `task --init`: writes a starter Taskfile. Ports Go `init.go`.

use std::path::{Path, PathBuf};

use taskcore::filepathext;
use taskcore::reader::DEFAULT_TASKFILES;

/// The default file name created when `path` is a directory.
const DEFAULT_FILENAME: &str = "Taskfile.yml";

/// The starter Taskfile written by `--init`.
pub const DEFAULT_TASKFILE: &str = include_str!("templates/default.yml");

/// Creates a new Taskfile at `path`.
///
/// `path` may be a file or directory. When it is a directory,
/// `path/Taskfile.yml` is created. The final path is returned, which may differ
/// from the input. Ports Go `InitTaskfile`.
pub fn init_taskfile(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    match std::fs::metadata(path) {
        Ok(info) if !info.is_dir() => Err("task: A Taskfile already exists".to_string()),
        Ok(_) if has_default_taskfile(path) => Err("task: A Taskfile already exists".to_string()),
        Ok(_) => {
            let final_path = path.join(DEFAULT_FILENAME);
            write(&final_path)?;
            Ok(final_path)
        }
        Err(_) => {
            write(path)?;
            Ok(path.to_path_buf())
        }
    }
}

/// Writes the default template to `path`.
fn write(path: &Path) -> Result<(), String> {
    std::fs::write(path, DEFAULT_TASKFILE.as_bytes())
        .map_err(|e| format!("task: unable to write {}: {e}", path.display()))
}

/// Reports whether `dir` already contains one of the default Taskfile names.
fn has_default_taskfile(dir: &Path) -> bool {
    DEFAULT_TASKFILES.iter().any(|name| {
        let candidate = filepathext::smart_join(&dir.to_string_lossy(), name);
        candidate.exists()
    })
}

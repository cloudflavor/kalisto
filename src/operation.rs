//! Cross-process exclusion for mutating operations on one cluster.
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::error::{Error, Result};

pub(crate) fn lock(state_root: &Path) -> Result<File> {
    let path = state_root.with_extension("operation.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(Error::InvalidArgs(
            "cluster operation lock is not a regular file".into(),
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    file.try_lock().map_err(|error| {
        Error::InvalidArgs(format!(
            "another operation is active for this cluster: {error}"
        ))
    })?;
    Ok(file)
}
fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "cluster operation lock".into(),
        source,
    }
}

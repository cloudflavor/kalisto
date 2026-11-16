//! Package shared offline boot assets as a read-only Firecracker block device.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::process::CommandRunner;

pub(crate) const DISK_FILENAME: &str = "firecracker-assets.ext4";

pub(crate) fn prepare(root: &Path, runner: &dyn CommandRunner) -> Result<PathBuf> {
    let source = root.join(".firecracker-assets");
    fs::create_dir(&source).map_err(io_error)?;
    let disk = root.join(DISK_FILENAME);
    let result = (|| {
        let mut bytes = 0u64;
        // No cluster configuration or generated TLS keys enter the shared disk.
        for name in ["k3s", "images.tar"] {
            let asset = root.join(name);
            bytes = bytes
                .checked_add(fs::metadata(&asset).map_err(io_error)?.len())
                .ok_or_else(|| Error::InvalidArgs("Firecracker asset disk is too large".into()))?;
            if fs::hard_link(&asset, source.join(name)).is_err() {
                fs::copy(&asset, source.join(name)).map_err(io_error)?;
            }
        }
        let size = bytes
            .checked_add(bytes / 4)
            .and_then(|size| size.checked_add(64 * 1024 * 1024))
            .ok_or_else(|| Error::InvalidArgs("Firecracker asset disk is too large".into()))?;
        let file = fs::File::create(&disk).map_err(io_error)?;
        file.set_len(size).map_err(io_error)?;
        runner.run(
            "mke2fs",
            &[
                "-q".into(),
                "-F".into(),
                "-t".into(),
                "ext4".into(),
                "-b".into(),
                "4096".into(),
                "-m".into(),
                "0".into(),
                "-L".into(),
                "KALISTO_ASSETS".into(),
                "-d".into(),
                source.display().to_string(),
                disk.display().to_string(),
            ],
        )?;
        Ok(disk.clone())
    })();
    let cleanup = fs::remove_dir_all(&source).map_err(io_error);
    match result {
        Ok(path) => {
            cleanup?;
            Ok(path)
        }
        Err(error) => {
            let _ = fs::remove_file(disk);
            Err(error)
        }
    }
}

fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "Firecracker asset disk".into(),
        source,
    }
}

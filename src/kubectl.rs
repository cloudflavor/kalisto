//! Managed host `kubectl` binary matching the cluster's K3s release.

use std::path::{Path, PathBuf};

use crate::artifact;
use crate::cluster::spec::ImageArch;
use crate::error::{Error, Result};
use crate::process::CommandRunner;

const RELEASE_URL_BASE: &str = "https://dl.k8s.io/release";

/// # Errors
///
/// Returns an error when the operation cannot be completed.
/// Resolve (downloading and checksum-verifying on first use) the managed host
/// kubectl binary for a K3s release version.
pub fn binary(version: &str, runner: &dyn CommandRunner) -> Result<PathBuf> {
    let version = version.trim();
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte))
        || version == "."
        || version == ".."
    {
        return Err(Error::InvalidArgs(
            "invalid K3s version for managed kubectl".to_owned(),
        ));
    }
    let arch = ImageArch::host_default().lima_artifact_suffix();
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => {
            return Err(Error::InvalidArgs(format!(
                "managed kubectl is not supported on {other}"
            )));
        }
    };
    let kubernetes_version = version.split('+').next().unwrap_or(version);
    let binary_name = "kubectl";
    let checksum_name = "kubectl.sha256";
    let release = format!("{RELEASE_URL_BASE}/{kubernetes_version}/bin/{os}/{arch}");
    let root = artifacts_root(version)?;
    let checksum_path = artifact::fetch_into_cache(
        runner,
        &root,
        &format!("{release}/{checksum_name}"),
        None,
        "K3s kubectl checksum",
        checksum_name,
    )?;
    let digest = expected_checksum(&checksum_path)?;
    let path = artifact::fetch_into_cache(
        runner,
        &root,
        &format!("{release}/{binary_name}"),
        Some(&digest),
        "K3s kubectl binary",
        binary_name,
    )?;
    ensure_executable(&path)?;
    Ok(path)
}

fn artifacts_root(version: &str) -> Result<PathBuf> {
    Ok(crate::paths::artifacts_root()?
        .join("downloads")
        .join(version)
        .join("host-kubectl")
        .join(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )))
}

fn expected_checksum(checksum_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(checksum_path).map_err(|source| Error::CommandIo {
        program: "read_to_string".to_owned(),
        source,
    })?;
    let digest = content
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(digest)
    } else {
        Err(Error::InvalidArgs(format!(
            "invalid kubectl checksum content at {}",
            checksum_path.display()
        )))
    }
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::metadata(path)
        .map_err(|source| Error::CommandIo {
            program: "metadata".to_owned(),
            source,
        })?
        .permissions();
    if permissions.mode() & 0o111 != 0o111 {
        let mut executable = permissions;
        executable.set_mode(0o755);
        std::fs::set_permissions(path, executable).map_err(|source| Error::CommandIo {
            program: "set_permissions".to_owned(),
            source,
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_versions() {
        let error = binary("..", &crate::process::SystemRunner)
            .expect_err("relative version must be rejected");
        assert!(matches!(error, Error::InvalidArgs(_)));
        let error =
            binary("", &crate::process::SystemRunner).expect_err("empty version must be rejected");
        assert!(matches!(error, Error::InvalidArgs(_)));
    }

    #[cfg(unix)]
    #[test]
    fn expected_checksum_accepts_plain_digest() {
        use std::io::Write;

        let temp =
            std::env::temp_dir().join(format!("kalisto-kubectl-checksum-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let path = temp.join("kubectl.sha256");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{}", "a".repeat(64)).unwrap();
        assert_eq!(expected_checksum(&path).unwrap(), "a".repeat(64));
    }
}

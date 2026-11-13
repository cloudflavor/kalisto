//! User-scoped, versioned runtime tools; no package manager or host privileges.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::artifact;
use crate::error::{Error, Result};
use crate::paths::artifacts_root;
use crate::process::CommandRunner;

pub(crate) const LIMA_VERSION: &str = "2.2.1";
const HELM_VERSION: &str = "3.19.0";
const CRANE_VERSION: &str = "0.22.1";

struct Tool {
    name: &'static str,
    version: &'static str,
    archive: String,
    url: String,
    checksums: String,
    executable: PathBuf,
}

fn tools() -> Result<Vec<Tool>> {
    if std::env::consts::OS != "macos" {
        return Err(invalid("managed Lima runtime currently requires macOS"));
    }
    let (arch, helm_arch) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "arm64"),
        "x86_64" => ("x86_64", "amd64"),
        other => return Err(invalid(format!("unsupported macOS architecture: {other}"))),
    };
    let lima_archive = format!("lima-{LIMA_VERSION}-Darwin-{arch}.tar.gz");
    let crane_archive = format!("go-containerregistry_Darwin_{arch}.tar.gz");
    let helm_archive = format!("helm-v{HELM_VERSION}-darwin-{helm_arch}.tar.gz");
    Ok(vec![
        Tool {
            name: "limactl",
            version: LIMA_VERSION,
            url: format!(
                "https://github.com/lima-vm/lima/releases/download/v{LIMA_VERSION}/{lima_archive}"
            ),
            checksums: format!(
                "https://github.com/lima-vm/lima/releases/download/v{LIMA_VERSION}/SHA256SUMS"
            ),
            archive: lima_archive,
            executable: "bin/limactl".into(),
        },
        Tool {
            name: "helm",
            version: HELM_VERSION,
            url: format!("https://get.helm.sh/{helm_archive}"),
            checksums: format!("https://get.helm.sh/{helm_archive}.sha256sum"),
            archive: helm_archive,
            executable: format!("darwin-{helm_arch}/helm").into(),
        },
        Tool {
            name: "crane",
            version: CRANE_VERSION,
            url: format!(
                "https://github.com/google/go-containerregistry/releases/download/v{CRANE_VERSION}/{crane_archive}"
            ),
            checksums: format!(
                "https://github.com/google/go-containerregistry/releases/download/v{CRANE_VERSION}/checksums.txt"
            ),
            archive: crane_archive,
            executable: "crane".into(),
        },
    ])
}

fn root() -> Result<PathBuf> {
    Ok(artifacts_root()?.join("runtime-v1"))
}

fn tool_root(tool: &Tool) -> Result<PathBuf> {
    Ok(root()?.join(format!(
        "{}-{}-{}",
        tool.name,
        tool.version,
        std::env::consts::ARCH
    )))
}

/// Resolve installed managed tools without downloading anything during startup.
pub fn program(name: &str) -> Option<PathBuf> {
    tools()
        .ok()?
        .into_iter()
        .find(|tool| tool.name == name)
        .and_then(|tool| {
            let path = tool_root(&tool).ok()?.join(tool.executable);
            path.is_file().then_some(path)
        })
}

pub fn prepare(runner: &dyn CommandRunner, cilium: bool) -> Result<()> {
    let root = root()?;
    fs::create_dir_all(&root).map_err(io_error)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("prepare.lock"))
        .map_err(io_error)?;
    lock.try_lock()
        .map_err(|error| invalid(format!("another runtime preparation is active: {error}")))?;
    for tool in tools()? {
        if tool.name == "helm" && !cilium {
            continue;
        }
        let destination = tool_root(&tool)?;
        if destination.join(&tool.executable).is_file() {
            continue;
        }
        let downloads = root
            .join("downloads")
            .join(format!("{}-{}", tool.name, tool.version));
        let sums = artifact::fetch_into_cache(
            runner,
            &downloads,
            &tool.checksums,
            None,
            "runtime checksums",
            "checksums.txt",
        )?;
        let sums = fs::read_to_string(sums).map_err(io_error)?;
        let checksum = checksum(&sums, &tool.archive)?;
        let archive = artifact::fetch_into_cache(
            runner,
            &downloads,
            &tool.url,
            Some(&checksum),
            tool.name,
            &tool.archive,
        )?;
        let listing = runner.run("tar", &["-tzf".into(), archive.display().to_string()])?;
        if listing
            .stdout
            .lines()
            .any(|entry| !safe_archive_path(entry))
        {
            return Err(invalid("runtime archive contains an unsafe path"));
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| invalid(error.to_string()))?
            .as_nanos();
        let staging = root.join(format!(".{}-{nonce}", tool.name));
        fs::create_dir(&staging).map_err(io_error)?;
        let result = (|| {
            runner.run(
                "tar",
                &[
                    "-xzf".into(),
                    archive.display().to_string(),
                    "-C".into(),
                    staging.display().to_string(),
                ],
            )?;
            if !staging.join(&tool.executable).is_file() {
                return Err(invalid(format!(
                    "{} archive is missing its executable",
                    tool.name
                )));
            }
            fs::rename(&staging, &destination).map_err(io_error)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
    }
    Ok(())
}

/// Built-in public images must not depend on Docker or its credential helpers.
pub fn configure(command: &mut Command, program: &str) -> Result<()> {
    if let Some(executable) = self::program(program)
        && let Some(bin) = executable.parent()
    {
        let mut paths = vec![bin.to_path_buf()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command.env(
            "PATH",
            std::env::join_paths(paths).map_err(|error| invalid(error.to_string()))?,
        );
    }
    if program == "crane" {
        let config = root()?.join("public-registry");
        fs::create_dir_all(&config).map_err(io_error)?;
        let path = config.join("config.json");
        if !path.exists() {
            fs::write(path, b"{\"auths\":{}}\n").map_err(io_error)?;
        }
        command
            .env("DOCKER_CONFIG", config)
            .env_remove("REGISTRY_AUTH_FILE");
    }
    Ok(())
}

fn checksum(content: &str, filename: &str) -> Result<String> {
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(hash), Some(name)) = (fields.next(), fields.next())
            && name.trim_start_matches('*').trim_start_matches("./") == filename
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(hash.to_owned());
        }
    }
    Err(invalid(format!("release checksum missing for {filename}")))
}

fn safe_archive_path(entry: &str) -> bool {
    !entry.is_empty()
        && !Path::new(entry).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}
fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "managed runtime".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_cannot_escape_managed_storage() {
        for path in ["../bin/evil", "/usr/bin/evil", "bin/../../evil"] {
            assert!(!safe_archive_path(path));
        }
        assert!(safe_archive_path("./bin/limactl"));
    }

    #[test]
    fn checksum_requires_exact_release_filename() {
        let hash = "a".repeat(64);
        assert_eq!(
            checksum(&format!("{hash} *tool.tar.gz\n"), "tool.tar.gz").unwrap(),
            hash
        );
        assert!(checksum(&format!("{hash} tool.tar.gz.exe\n"), "tool.tar.gz").is_err());
    }
}

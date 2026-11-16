//! User-scoped, versioned runtime tools; no package manager or host privileges.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::artifact;
use crate::backend::BackendKind;
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

fn macos_arch() -> Result<(&'static str, &'static str)> {
    if std::env::consts::OS != "macos" {
        return Err(invalid("managed runtime helpers currently require macOS"));
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok(("arm64", "arm64")),
        "x86_64" => Ok(("x86_64", "amd64")),
        other => Err(invalid(format!("unsupported macOS architecture: {other}"))),
    }
}

fn lima_tool() -> Result<Tool> {
    let (arch, _) = macos_arch()?;
    let archive = format!("lima-{LIMA_VERSION}-Darwin-{arch}.tar.gz");
    Ok(Tool {
        name: "limactl",
        version: LIMA_VERSION,
        url: format!("https://github.com/lima-vm/lima/releases/download/v{LIMA_VERSION}/{archive}"),
        checksums: format!(
            "https://github.com/lima-vm/lima/releases/download/v{LIMA_VERSION}/SHA256SUMS"
        ),
        archive,
        executable: "bin/limactl".into(),
    })
}

fn crane_tool() -> Result<Tool> {
    let (arch, _) = macos_arch()?;
    let archive = format!("go-containerregistry_Darwin_{arch}.tar.gz");
    Ok(Tool {
        name: "crane",
        version: CRANE_VERSION,
        url: format!(
            "https://github.com/google/go-containerregistry/releases/download/v{CRANE_VERSION}/{archive}"
        ),
        checksums: format!(
            "https://github.com/google/go-containerregistry/releases/download/v{CRANE_VERSION}/checksums.txt"
        ),
        archive,
        executable: "crane".into(),
    })
}

fn helm_tool() -> Result<Tool> {
    let (_, arch) = macos_arch()?;
    let archive = format!("helm-v{HELM_VERSION}-darwin-{arch}.tar.gz");
    Ok(Tool {
        name: "helm",
        version: HELM_VERSION,
        url: format!("https://get.helm.sh/{archive}"),
        checksums: format!("https://get.helm.sh/{archive}.sha256sum"),
        archive,
        executable: format!("darwin-{arch}/helm").into(),
    })
}

fn root() -> Result<PathBuf> {
    Ok(artifacts_root()?.join("runtime-v1"))
}

fn tool_root(root: &Path, tool: &Tool) -> PathBuf {
    root.join(format!(
        "{}-{}-{}",
        tool.name,
        tool.version,
        std::env::consts::ARCH
    ))
}

/// Resolve installed managed tools without downloading anything during startup.
#[must_use]
pub fn program(name: &str) -> Option<PathBuf> {
    if name == "container" {
        return crate::apple_container::runtime::program().ok();
    }
    let tool = match name {
        "limactl" => lima_tool(),
        "crane" => crane_tool(),
        "helm" => helm_tool(),
        _ => return None,
    }
    .ok()?;
    let path = tool_root(&root().ok()?, &tool).join(tool.executable);
    path.is_file().then_some(path)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn prepare(
    runner: &dyn CommandRunner,
    backend: BackendKind,
    cilium: bool,
) -> Result<()> {
    match backend {
        BackendKind::Firecracker => return Ok(()),
        BackendKind::AppleContainer | BackendKind::Lima => {}
    }
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
    if backend == BackendKind::Lima {
        prepare_tool(runner, &root, &lima_tool()?)?;
    }
    prepare_tool(runner, &root, &crane_tool()?)?;
    if cilium {
        prepare_tool(runner, &root, &helm_tool()?)?;
    }
    Ok(())
}

fn prepare_tool(runner: &dyn CommandRunner, root: &Path, tool: &Tool) -> Result<()> {
    let destination = tool_root(root, tool);
    if destination.join(&tool.executable).is_file() {
        return Ok(());
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
    result
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
/// Built-in public images must not depend on Docker or its credential helpers.
pub fn configure(command: &mut Command, program: &str) -> Result<()> {
    if program == "container" {
        crate::apple_container::runtime::configure(command)?;
    }
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

    #[test]
    fn firecracker_never_downloads_macos_helpers() {
        struct NoCommands;

        impl CommandRunner for NoCommands {
            fn run(&self, program: &str, args: &[String]) -> Result<crate::process::Output> {
                panic!("unexpected Firecracker helper command: {program} {args:?}");
            }
        }

        for cilium in [false, true] {
            prepare(&NoCommands, BackendKind::Firecracker, cilium).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    mod preparation {
        use std::cell::RefCell;
        use std::ffi::OsString;
        use std::io::Cursor;

        use flate2::Compression;
        use flate2::write::GzEncoder;
        use sha2::{Digest, Sha256};

        use crate::process::{Output, SystemRunner};
        use crate::test_support::env_lock;

        use super::*;

        struct DataRoot {
            path: PathBuf,
            previous: Option<OsString>,
        }

        impl DataRoot {
            fn new() -> Self {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir()
                    .join(format!("kalisto-runtime-{}-{nonce}", std::process::id()));
                let previous = std::env::var_os("XDG_DATA_HOME");
                // All environment-changing tests hold the shared environment lock.
                unsafe { std::env::set_var("XDG_DATA_HOME", &path) };
                Self { path, previous }
            }
        }

        impl Drop for DataRoot {
            fn drop(&mut self) {
                unsafe {
                    if let Some(previous) = &self.previous {
                        std::env::set_var("XDG_DATA_HOME", previous);
                    } else {
                        std::env::remove_var("XDG_DATA_HOME");
                    }
                }
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        struct DownloadRunner {
            archive: Vec<u8>,
            checksums: String,
            downloads: RefCell<Vec<String>>,
            extractions: RefCell<Vec<String>>,
        }

        impl DownloadRunner {
            fn new(archives: &[String; 3], helm_arch: &str) -> Self {
                let encoder = GzEncoder::new(Vec::new(), Compression::default());
                let mut archive = tar::Builder::new(encoder);
                for executable in ["bin/limactl", "crane", &format!("darwin-{helm_arch}/helm")] {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(4);
                    header.set_mode(0o755);
                    header.set_cksum();
                    archive
                        .append_data(&mut header, executable, Cursor::new(b"tool"))
                        .unwrap();
                }
                let archive = archive.into_inner().unwrap().finish().unwrap();
                let digest = format!("{:x}", Sha256::digest(&archive));
                let checksums = archives
                    .iter()
                    .map(|name| format!("{digest}  {name}\n"))
                    .collect();
                Self {
                    archive,
                    checksums,
                    downloads: RefCell::new(Vec::new()),
                    extractions: RefCell::new(Vec::new()),
                }
            }
        }

        impl CommandRunner for DownloadRunner {
            fn run(&self, program: &str, args: &[String]) -> Result<Output> {
                if program == "curl" {
                    let url = args.last().unwrap();
                    let output = args.windows(2).find(|pair| pair[0] == "-o").unwrap();
                    self.downloads.borrow_mut().push(url.clone());
                    let content = if url.ends_with(".tar.gz") {
                        self.archive.as_slice()
                    } else {
                        self.checksums.as_bytes()
                    };
                    fs::write(&output[1], content).map_err(io_error)?;
                    return Ok(Output {
                        stdout: String::new(),
                        stderr: String::new(),
                    });
                }
                if program == "tar" && args[0] == "-xzf" {
                    self.extractions.borrow_mut().push(
                        Path::new(&args[1])
                            .file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned(),
                    );
                }
                SystemRunner.run(program, args)
            }
        }

        fn assert_prepared_tools(backend: BackendKind, cilium: bool, selected: &[usize]) {
            let _lock = env_lock();
            let _root = DataRoot::new();
            let (arch, helm_arch) = match std::env::consts::ARCH {
                "aarch64" => ("arm64", "arm64"),
                "x86_64" => ("x86_64", "amd64"),
                other => panic!("unsupported test architecture: {other}"),
            };
            let archives = [
                format!("lima-{LIMA_VERSION}-Darwin-{arch}.tar.gz"),
                format!("go-containerregistry_Darwin_{arch}.tar.gz"),
                format!("helm-v{HELM_VERSION}-darwin-{helm_arch}.tar.gz"),
            ];
            let urls = [
                format!("https://github.com/lima-vm/lima/releases/download/v{LIMA_VERSION}"),
                format!(
                    "https://github.com/google/go-containerregistry/releases/download/v{CRANE_VERSION}"
                ),
                "https://get.helm.sh".to_owned(),
            ];
            let checksum_names = [
                "SHA256SUMS".to_owned(),
                "checksums.txt".to_owned(),
                format!("{}.sha256sum", archives[2]),
            ];
            let runner = DownloadRunner::new(&archives, helm_arch);
            prepare(&runner, backend, cilium).unwrap();
            let expected_downloads: Vec<_> = selected
                .iter()
                .flat_map(|&index| {
                    [
                        format!("{}/{}", urls[index], checksum_names[index]),
                        format!("{}/{}", urls[index], archives[index]),
                    ]
                })
                .collect();
            assert_eq!(*runner.downloads.borrow(), expected_downloads);
            let expected_archives: Vec<_> = selected
                .iter()
                .map(|&index| archives[index].clone())
                .collect();
            assert_eq!(*runner.extractions.borrow(), expected_archives);
            for (index, name) in ["limactl", "crane", "helm"].iter().enumerate() {
                assert_eq!(
                    program(name).is_some(),
                    selected.contains(&index),
                    "managed {name} resolution"
                );
            }
            prepare(&runner, backend, cilium).unwrap();
            assert_eq!(*runner.downloads.borrow(), expected_downloads);
            assert_eq!(*runner.extractions.borrow(), expected_archives);
        }

        #[test]
        fn apple_without_cilium_downloads_only_crane() {
            assert_prepared_tools(BackendKind::AppleContainer, false, &[1]);
        }

        #[test]
        fn apple_with_cilium_downloads_crane_and_helm_only() {
            assert_prepared_tools(BackendKind::AppleContainer, true, &[1, 2]);
        }

        #[test]
        fn lima_without_cilium_downloads_limactl_and_crane_only() {
            assert_prepared_tools(BackendKind::Lima, false, &[0, 1]);
        }

        #[test]
        fn lima_with_cilium_downloads_limactl_crane_and_helm() {
            assert_prepared_tools(BackendKind::Lima, true, &[0, 1, 2]);
        }
    }
}

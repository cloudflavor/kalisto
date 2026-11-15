//! Pinned, user-owned Apple runtime; only `prepare` is allowed to fetch assets.
//!
//! Command/layout contracts come from apple/container tag 1.7.1: Makefile,
//! ContainerPlugin/{InstallRoot,ApplicationRoot,ServiceManager,PluginLoader}.swift,
//! ContainerCommands/System/SystemStart.swift and ContainerImagesService/SnapshotStore.swift.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact;
use crate::error::{Error, Result};
use crate::paths::{artifacts_root, data_root};
use crate::process::{CommandRunner, Output};

const VERSION: &str = "1.7.1";
const PACKAGE: &str = "container-1.7.1-installer-signed.pkg";
const PACKAGE_URL: &str = "https://github.com/apple/container/releases/download/1.7.1/container-1.7.1-installer-signed.pkg";
// The signed asset's digest published by GitHub's 1.7.1 release API.
const PACKAGE_SHA256: &str = "b6739caf3d47ca53a78f1a7edf32521420a75c7941838c3cefa75f30b80f5a97";
const TEAM: &str = "UPBK2H6LZM";
const API_LABEL: &str = "com.apple.container.apiserver";
const IMAGES_LABEL: &str = "com.apple.container.core.container-core-images";
// ContainerPersistence/ContainerSystemConfig.swift at tag 1.7.1.
const KERNEL_URL: &str = "https://github.com/kata-containers/kata-containers/releases/download/4.3.1/kata-static-4.3.1-arm64.tar.zst";
const KERNEL_SHA256: &str = "2c5bf7cde334ad5307d482f45cceda496f91c9b6b96f5278c47f4f4eac1bbae6";
const KERNEL_MEMBER: &str = "opt/kata/share/kata-containers/vmlinux-6.18.56-208-debug";
const KERNEL_FILENAME: &str = "vmlinux-6.18.56-208-debug";
const MANIFEST: &str = "prepared.json";
const REQUIRED_BINARIES: &[&str] = &[
    "bin/container",
    "bin/container-apiserver",
    "libexec/container/plugins/container-runtime-linux/bin/container-runtime-linux",
    "libexec/container/plugins/container-network-vmnet/bin/container-network-vmnet",
    "libexec/container/plugins/container-core-images/bin/container-core-images",
    "libexec/container/plugins/container-core-clusters/bin/container-core-clusters",
    "libexec/container/plugins/machine-apiserver/bin/machine-apiserver",
];

#[derive(Deserialize, Serialize)]
struct Prepared {
    version: String,
    package_sha256: String,
    kernel_sha256: String,
    init_image: String,
    init_index: String,
    init_manifest: String,
    snapshot_sha256: String,
    snapshot_info_sha256: String,
    kernel_size: u64,
    snapshot_size: u64,
    snapshot_info_size: u64,
    runtime_sizes: [u64; REQUIRED_BINARIES.len()],
}

pub(crate) fn root() -> Result<PathBuf> {
    std::path::absolute(artifacts_root()?.join("runtime-v1/apple-container-1.7.1-arm64"))
        .map_err(io_error)
}

fn install_root() -> Result<PathBuf> {
    Ok(root()?.join("install"))
}

fn app_root() -> Result<PathBuf> {
    std::path::absolute(data_root()?.join("apple-container")).map_err(io_error)
}

/// This never probes PATH, downloads, or falls back to an older host binary.
pub(crate) fn program() -> Result<PathBuf> {
    Ok(install_root()?.join("bin/container"))
}

pub(crate) fn kernel_path() -> Result<PathBuf> {
    Ok(app_root()?.join("kernels").join(KERNEL_FILENAME))
}

pub(crate) fn init_image() -> Result<String> {
    Ok(load_prepared()?.init_image)
}

pub(crate) fn configure(command: &mut Command) -> Result<()> {
    // Apple forwards every CONTAINER_* variable to its helpers. Do not inherit a
    // user's alternate installation, plugins, debug hooks or default platform.
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("CONTAINER_") || key.starts_with("DYLD_"))
        {
            command.env_remove(key);
        }
    }
    command
        .env("CONTAINER_APP_ROOT", app_root()?)
        .env("CONTAINER_INSTALL_ROOT", install_root()?)
        .env("XDG_CONFIG_HOME", root()?.join("config-home"))
        .env("TMPDIR", root()?.join("tmp"))
        .env_remove("SSH_AUTH_SOCK");
    Ok(())
}

pub(crate) fn run(runner: &dyn CommandRunner, args: &[String]) -> Result<Output> {
    // The main runtime resolver supplies program() and configure() for this name.
    runner.run("container", args)
}

pub(crate) fn prepare(runner: &dyn CommandRunner) -> Result<()> {
    check_platform(runner)?;
    let root = root()?;
    fs::create_dir_all(&root).map_err(io_error)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("prepare.lock"))
        .map_err(io_error)?;
    lock.try_lock().map_err(|error| {
        invalid(format!(
            "another Apple runtime preparation is active: {error}"
        ))
    })?;
    for directory in [root.join("tmp"), root.join("config-home"), app_root()?] {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    install(runner)?;
    verify_version(runner)?;
    let domain = launchd_domain(runner)?;
    let registered = check_services(runner, &domain)?;
    if registered {
        verify_status(runner)?;
    } else {
        // XDG_CONFIG_HOME prevents SystemStart from copying the user's unrelated
        // ~/.config/container/config.toml into this installation's app root.
        run(
            runner,
            &[
                "system".into(),
                "start".into(),
                "--app-root".into(),
                app_root()?.display().to_string(),
                "--install-root".into(),
                install_root()?.display().to_string(),
                "--disable-kernel-install".into(),
            ],
        )?;
        verify_status(runner)?;
    }
    confine_image_temporaries(runner, &domain)?;
    prepare_kernel(runner)?;

    let properties = run(
        runner,
        &strings(&["system", "property", "list", "--format", "json"]),
    )?;
    let properties = parse_json(&properties.stdout, "Apple runtime properties")?;
    let source = properties
        .pointer("/vminit/image")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Apple runtime properties omit vminit.image"))?;
    run(
        runner,
        &strings(&["image", "pull", "--platform", "linux/arm64", source]),
    )?;
    // Infra images are deliberately hidden by Apple's image inspect command.
    // A separate local alias exposes the descriptor and platform manifest.
    let probe = "kalisto.invalid/vminit:apple-1.7.1";
    run(runner, &strings(&["image", "tag", source, probe]))?;
    let (index, manifest) = inspect_init(runner, probe)?;
    let reference = format!("kalisto.invalid/vminit:sha256-{}", digest_hex(&index)?);
    run(runner, &strings(&["image", "tag", probe, &reference]))?;
    let snapshot = snapshot_root(&manifest)?;
    let mut runtime_sizes = [0; REQUIRED_BINARIES.len()];
    for (size, relative) in runtime_sizes.iter_mut().zip(REQUIRED_BINARIES) {
        *size = file_size(&install_root()?.join(relative))?;
    }
    let prepared = Prepared {
        version: VERSION.into(),
        package_sha256: PACKAGE_SHA256.into(),
        kernel_sha256: artifact::file_sha256(runner, &kernel_path()?)?,
        init_image: reference,
        init_index: index,
        init_manifest: manifest,
        snapshot_sha256: artifact::file_sha256(runner, &snapshot.join("snapshot"))?,
        snapshot_info_sha256: artifact::file_sha256(runner, &snapshot.join("snapshot-info"))?,
        kernel_size: file_size(&kernel_path()?)?,
        snapshot_size: file_size(&snapshot.join("snapshot"))?,
        snapshot_info_size: file_size(&snapshot.join("snapshot-info"))?,
        runtime_sizes,
    };
    publish_json(&root.join(MANIFEST), &prepared)?;
    Ok(())
}

/// Offline preflight, including restarting only Kalisto's own launchd service.
pub(crate) fn check_available(runner: &dyn CommandRunner) -> Result<()> {
    check_platform(runner)?;
    let prepared = load_prepared()?;
    for (relative, expected) in REQUIRED_BINARIES.iter().zip(prepared.runtime_sizes) {
        verify_file(&install_root()?.join(relative), expected)?;
    }
    verify_version(runner)?;
    verify_file(&kernel_path()?, prepared.kernel_size)?;
    let snapshot = snapshot_root(&prepared.init_manifest)?;
    verify_file(&snapshot.join("snapshot"), prepared.snapshot_size)?;
    verify_file(&snapshot.join("snapshot-info"), prepared.snapshot_info_size)?;
    let domain = launchd_domain(runner)?;
    if !check_services(runner, &domain)? {
        let plist = app_root()?.join("apiserver/apiserver.plist");
        let value = read_plist(runner, &plist)?;
        validate_plist(
            &value,
            API_LABEL,
            &install_root()?.join("bin/container-apiserver"),
        )?;
        // SystemStart can download init even with --disable-kernel-install.
        // Bootstrap its cached plist instead: ServiceManager.register uses this
        // exact launchctl operation; APIServer.Start itself does not fetch assets.
        runner.run(
            "/bin/launchctl",
            &strings(&["bootstrap", &domain, path_str(&plist)?]),
        )?;
    }
    verify_status(runner)?;
    confine_image_temporaries(runner, &domain)?;
    let (index, manifest) = inspect_init(runner, &prepared.init_image)?;
    if index != prepared.init_index || manifest != prepared.init_manifest {
        return Err(invalid(
            "cached Apple init image changed; run `kalisto pull` again",
        ));
    }
    Ok(())
}

fn check_platform(runner: &dyn CommandRunner) -> Result<()> {
    if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
        return Err(invalid(
            "Apple containers require native Apple silicon and macOS 26 or later",
        ));
    }
    let version = runner.run("/usr/bin/sw_vers", &strings(&["-productVersion"]))?;
    let major = version
        .stdout
        .trim()
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok());
    if major.is_none_or(|major| major < 26) {
        return Err(invalid("Apple containers require macOS 26 or later"));
    }
    Ok(())
}

fn install(runner: &dyn CommandRunner) -> Result<()> {
    let destination = install_root()?;
    if destination.exists() {
        verify_signatures(runner, &destination)?;
        return Ok(());
    }
    let package = artifact::fetch_into_cache(
        runner,
        &root()?.join("downloads"),
        PACKAGE_URL,
        Some(PACKAGE_SHA256),
        "signed Apple container runtime",
        PACKAGE,
    )?;
    let signature = runner.run(
        "/usr/bin/env",
        &strings(&[
            "LC_ALL=C",
            "/usr/sbin/pkgutil",
            "--check-signature",
            path_str(&package)?,
        ]),
    )?;
    validate_package_signature(&signature.stdout)?;
    let listing = runner.run("/usr/bin/xar", &strings(&["-tf", path_str(&package)?]))?;
    if !listing
        .stdout
        .lines()
        .all(|entry| matches!(entry, "Bom" | "Payload" | "PackageInfo"))
    {
        return Err(invalid(
            "signed Apple runtime package contains unexpected members",
        ));
    }
    let staging = Scratch::new(&root()?, "install")?;
    let payload = staging.0.join("payload");
    let extracted = staging.0.join("install");
    fs::create_dir(&payload).map_err(io_error)?;
    fs::create_dir(&extracted).map_err(io_error)?;
    runner.run(
        "/usr/bin/xar",
        &strings(&[
            "-xf",
            path_str(&package)?,
            "-C",
            path_str(&payload)?,
            "Payload",
            "PackageInfo",
        ]),
    )?;
    for (attribute, expected) in [
        ("identifier", "com.apple.container-installer"),
        ("version", VERSION),
        ("install-location", "/usr/local"),
    ] {
        let query = format!("string(/pkg-info/@{attribute})");
        let value = runner.run(
            "/usr/bin/xmllint",
            &strings(&[
                "--nonet",
                "--xpath",
                &query,
                path_str(&payload.join("PackageInfo"))?,
            ]),
        )?;
        if value.stdout.trim() != expected {
            return Err(invalid(format!(
                "unexpected Apple package {attribute}: {}",
                value.stdout.trim()
            )));
        }
    }
    let archive = payload.join("Payload");
    let paths = runner.run("/bin/pax", &strings(&["-zf", path_str(&archive)?]))?;
    if paths.stdout.lines().any(|entry| !safe_archive_path(entry)) || paths.stdout.is_empty() {
        return Err(invalid("Apple runtime payload contains unsafe paths"));
    }
    // This is Apple's Makefile SUDO= installation method. Keep the entire signed
    // layout, permissions and entitlements; never copy or re-sign executables.
    // Positional shell arguments avoid interpolating host paths into shell code.
    runner.run(
        "/bin/sh",
        &strings(&[
            "-c",
            "cd \"$1\" && exec /bin/pax -rz -f \"$2\"",
            "kalisto-pkg",
            path_str(&extracted)?,
            path_str(&archive)?,
        ]),
    )?;
    validate_tree(&extracted, &extracted.canonicalize().map_err(io_error)?)?;
    verify_signatures(runner, &extracted)?;
    fs::rename(&extracted, &destination).map_err(io_error)
}

fn validate_package_signature(output: &str) -> Result<()> {
    if !output
        .contains("Status: signed by a developer certificate issued by Apple for distribution")
        || !output.contains("1. Developer ID Installer: Apple Inc. - Containerization (UPBK2H6LZM)")
        || !output.contains("Apple Root CA")
    {
        return Err(invalid(
            "Apple runtime package is not trusted and signed by Apple Inc. - Containerization (UPBK2H6LZM)",
        ));
    }
    Ok(())
}

fn verify_version(runner: &dyn CommandRunner) -> Result<()> {
    if !program()?.is_file() {
        return Err(invalid(
            "managed Apple container 1.7.1 is missing; run `kalisto pull` first",
        ));
    }
    let version = run(runner, &strings(&["--version"]))?;
    if !valid_cli_version(version.stdout.trim()) {
        return Err(invalid(format!(
            "expected managed Apple container {VERSION}, found {}",
            version.stdout.trim()
        )));
    }
    Ok(())
}

fn valid_cli_version(value: &str) -> bool {
    value
        .strip_prefix("container CLI version ")
        .is_some_and(|version| {
            version.split_once(" (").is_some_and(|(number, extra)| {
                number == VERSION && extra.starts_with("build: release,") && extra.ends_with(')')
            })
        })
}

fn verify_signatures(runner: &dyn CommandRunner, install: &Path) -> Result<()> {
    if fs::symlink_metadata(install)
        .map_err(io_error)?
        .file_type()
        .is_symlink()
    {
        return Err(invalid(
            "managed Apple runtime installation must not be a symlink",
        ));
    }
    let canonical = install.canonicalize().map_err(io_error)?;
    let requirement =
        format!("=anchor apple generic and certificate leaf[subject.OU] = \"{TEAM}\"");
    for executable in REQUIRED_BINARIES {
        let executable = install.join(executable);
        if !executable
            .canonicalize()
            .map_err(io_error)?
            .starts_with(&canonical)
        {
            return Err(invalid(
                "managed Apple runtime executable escapes its installation",
            ));
        }
        runner.run(
            "/usr/bin/codesign",
            &strings(&[
                "--verify",
                "--strict",
                "-R",
                &requirement,
                path_str(&executable)?,
            ]),
        )?;
    }
    Ok(())
}

fn file_size(path: &Path) -> Result<u64> {
    let metadata = fs::metadata(path).map_err(|error| {
        invalid(format!(
            "cached Apple asset {} is unavailable: {error}; run `kalisto pull`",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(invalid(format!(
            "cached Apple asset {} is not a nonempty file; run `kalisto pull`",
            path.display()
        )));
    }
    Ok(metadata.len())
}

fn verify_file(path: &Path, expected: u64) -> Result<()> {
    let actual = file_size(path)?;
    if expected == 0 || actual != expected {
        return Err(invalid(format!(
            "cached Apple asset {} changed size (expected {expected}, found {actual}); run `kalisto pull`",
            path.display()
        )));
    }
    Ok(())
}

fn prepare_kernel(runner: &dyn CommandRunner) -> Result<()> {
    // Download and extract on our volume, not KernelService's Foundation temp
    // directory. Its local --binary operation only copies into appRoot/kernels.
    let archive = artifact::fetch_into_cache(
        runner,
        &root()?.join("downloads"),
        KERNEL_URL,
        Some(KERNEL_SHA256),
        "Apple recommended kernel",
        "kata-static-4.3.1-arm64.tar.zst",
    )?;
    let scratch = Scratch::new(&root()?, "kernel")?;
    runner.run(
        "/usr/bin/tar",
        &strings(&[
            "-xf",
            path_str(&archive)?,
            "-C",
            path_str(&scratch.0)?,
            KERNEL_MEMBER,
        ]),
    )?;
    let binary = scratch.0.join(KERNEL_MEMBER);
    let resolved = binary.canonicalize().map_err(io_error)?;
    if !resolved.starts_with(scratch.0.canonicalize().map_err(io_error)?) || !resolved.is_file() {
        return Err(invalid(
            "recommended Apple kernel escapes its extraction directory",
        ));
    }
    run(
        runner,
        &strings(&[
            "system",
            "kernel",
            "set",
            "--binary",
            path_str(&binary)?,
            "--arch",
            "arm64",
            "--force",
        ]),
    )?;
    Ok(())
}

fn launchd_domain(runner: &dyn CommandRunner) -> Result<String> {
    let manager = runner.run("/bin/launchctl", &strings(&["managername"]))?;
    let uid = runner.run("/usr/bin/id", &strings(&["-u"]))?;
    let uid = uid.stdout.trim();
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) || uid == "0" {
        return Err(invalid(
            "Apple containers must run in an unprivileged login session, without sudo",
        ));
    }
    match manager.stdout.trim() {
        "Aqua" => Ok(format!("gui/{uid}")),
        "Background" => Ok(format!("user/{uid}")),
        other => Err(invalid(format!(
            "unsupported Apple container launchd session: {other}; use a normal macOS login session"
        ))),
    }
}

fn check_services(runner: &dyn CommandRunner, domain: &str) -> Result<bool> {
    let services = runner.run("/bin/launchctl", &strings(&["list"]))?;
    let mut api_registered = false;
    let app = app_root()?;
    let install = install_root()?;
    for label in services
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
    {
        if !label.starts_with("com.apple.container.") {
            continue;
        }
        let target = format!("{domain}/{label}");
        let service = runner.run("/bin/launchctl", &strings(&["print", &target]))?;
        if !owned_service(&service.stdout, &app, &install) {
            return Err(service_conflict(label));
        }
        api_registered |= label == API_LABEL;
    }
    Ok(api_registered)
}

fn owned_service(output: &str, app: &Path, install: &Path) -> bool {
    launchctl_value(output, "CONTAINER_APP_ROOT", "=>") == app.to_str()
        && launchctl_value(output, "CONTAINER_INSTALL_ROOT", "=>") == install.to_str()
        && launchctl_value(output, "program", "=")
            .is_some_and(|program| Path::new(program).starts_with(install))
}

fn launchctl_value<'a>(output: &'a str, key: &str, separator: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(separator)?;
        (name.trim() == key).then_some(value.trim())
    })
}

fn service_conflict(label: &str) -> Error {
    invalid(format!(
        "Apple's login-session service `{label}` belongs to another installation or is incompatible. Kalisto will not stop or replace it. Stop that installation yourself with its `container system stop`, then run `kalisto pull` (or use the Lima backend)."
    ))
}

fn verify_status(runner: &dyn CommandRunner) -> Result<()> {
    let status = run(runner, &strings(&["system", "status", "--format", "json"]))?;
    let status = parse_json(&status.stdout, "Apple service status")?;
    validate_status(&status, &app_root()?, &install_root()?)
}

fn validate_status(status: &Value, app: &Path, install: &Path) -> Result<()> {
    if status["status"] != "running"
        || status["server"]["version"] != VERSION
        || status["server"]["build"] != "release"
        || status["server"]["appName"] != "container-apiserver"
        || status["paths"]["appRoot"].as_str() != app.to_str()
        || status["paths"]["installRoot"].as_str() != install.to_str()
    {
        return Err(service_conflict(API_LABEL));
    }
    Ok(())
}

fn confine_image_temporaries(runner: &dyn CommandRunner, domain: &str) -> Result<()> {
    // PluginLoader only forwards CONTAINER_* and proxy variables, NOT TMPDIR.
    // Image load/save/export use Foundation temporaryDirectory. Amend only the
    // verified Kalisto-owned image helper's plist, never the user's environment.
    let plist = app_root()?.join("plugin-state/container-core-images/service.plist");
    let executable = install_root()?
        .join("libexec/container/plugins/container-core-images/bin/container-core-images");
    let mut value = read_plist(runner, &plist)?;
    validate_plist(&value, IMAGES_LABEL, &executable)?;
    let target = format!("{domain}/{IMAGES_LABEL}");
    let service = runner.run("/bin/launchctl", &strings(&["print", &target]))?;
    if !owned_service(&service.stdout, &app_root()?, &install_root()?) {
        return Err(service_conflict(IMAGES_LABEL));
    }
    let temporary = root()?.join("tmp");
    if launchctl_value(&service.stdout, "TMPDIR", "=>") == temporary.to_str() {
        return Ok(());
    }
    value["EnvironmentVariables"]["TMPDIR"] = Value::String(path_str(&temporary)?.into());
    let parent = plist
        .parent()
        .ok_or_else(|| invalid("Apple service plist has no parent"))?;
    let scratch = Scratch::new(parent, "plist")?;
    let updated = scratch.0.join("service.plist");
    fs::write(
        &updated,
        serde_json::to_vec(&value).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)?;
    runner.run(
        "/usr/bin/plutil",
        &strings(&["-convert", "xml1", path_str(&updated)?]),
    )?;
    fs::rename(updated, &plist).map_err(io_error)?;
    runner.run("/bin/launchctl", &strings(&["bootout", &target]))?;
    runner.run(
        "/bin/launchctl",
        &strings(&["bootstrap", domain, path_str(&plist)?]),
    )?;
    Ok(())
}

fn read_plist(runner: &dyn CommandRunner, path: &Path) -> Result<Value> {
    let value = runner.run(
        "/usr/bin/plutil",
        &strings(&["-convert", "json", "-o", "-", path_str(path)?]),
    )?;
    parse_json(&value.stdout, "Apple launchd plist")
}

fn validate_plist(value: &Value, label: &str, executable: &Path) -> Result<()> {
    if value["Label"] != label
        || value["ProgramArguments"].get(0).and_then(Value::as_str) != executable.to_str()
        || value["ProgramArguments"].get(1).and_then(Value::as_str) != Some("start")
        || value["EnvironmentVariables"]["CONTAINER_APP_ROOT"].as_str() != app_root()?.to_str()
        || value["EnvironmentVariables"]["CONTAINER_INSTALL_ROOT"].as_str()
            != install_root()?.to_str()
        || value["MachServices"][label] != true
    {
        return Err(service_conflict(label));
    }
    Ok(())
}

fn inspect_init(runner: &dyn CommandRunner, reference: &str) -> Result<(String, String)> {
    let output = run(runner, &strings(&["image", "inspect", reference]))?;
    let images = parse_json(&output.stdout, "Apple init image")?;
    let image = images
        .as_array()
        .filter(|images| images.len() == 1)
        .and_then(|images| images.first())
        .ok_or_else(|| invalid("cached Apple init image is missing; run `kalisto pull`"))?;
    let index = image
        .pointer("/configuration/descriptor/digest")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Apple init image omits its OCI index digest"))?;
    let manifest = image["variants"]
        .as_array()
        .and_then(|variants| {
            variants.iter().find(|variant| {
                variant["platform"]["os"] == "linux"
                    && variant["platform"]["architecture"] == "arm64"
            })
        })
        .and_then(|variant| variant["digest"].as_str())
        .ok_or_else(|| invalid("Apple init image has no cached linux/arm64 manifest"))?;
    digest_hex(index)?;
    digest_hex(manifest)?;
    Ok((index.into(), manifest.into()))
}

fn snapshot_root(manifest: &str) -> Result<PathBuf> {
    Ok(app_root()?.join("snapshots").join(digest_hex(manifest)?))
}

fn load_prepared() -> Result<Prepared> {
    let path = root()?.join(MANIFEST);
    let bytes = fs::read(&path).map_err(|error| {
        invalid(format!(
            "Apple runtime is not prepared ({error}); run `kalisto pull` first"
        ))
    })?;
    let prepared: Prepared = serde_json::from_slice(&bytes).map_err(|error| {
        invalid(format!(
            "invalid Apple runtime manifest: {error}; run `kalisto pull`"
        ))
    })?;
    if prepared.version != VERSION
        || prepared.package_sha256 != PACKAGE_SHA256
        || prepared.init_image
            != format!(
                "kalisto.invalid/vminit:sha256-{}",
                digest_hex(&prepared.init_index)?
            )
    {
        return Err(invalid(
            "incompatible Apple runtime manifest; run `kalisto pull`",
        ));
    }
    digest_hex(&prepared.init_manifest)?;
    Ok(prepared)
}

fn digest_hex(digest: &str) -> Result<&str> {
    let hex = digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| invalid(format!("invalid Apple OCI digest: {digest}")))?;
    Ok(hex)
}

fn publish_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("runtime manifest has no parent"))?;
    let scratch = Scratch::new(parent, "manifest")?;
    let temporary = scratch.0.join("manifest");
    fs::write(
        &temporary,
        serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)?;
    fs::rename(temporary, path).map_err(io_error)
}

fn safe_archive_path(entry: &str) -> bool {
    !entry.is_empty()
        && !entry.contains(['\0', '\n', '\r'])
        && !Path::new(entry).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn validate_tree(directory: &Path, root: &Path) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let kind = entry.file_type().map_err(io_error)?;
        let path = entry.path();
        if kind.is_symlink() {
            if !path.canonicalize().map_err(io_error)?.starts_with(root) {
                return Err(invalid(
                    "Apple runtime payload contains an escaping symlink",
                ));
            }
        } else if kind.is_dir() {
            validate_tree(&path, root)?;
        } else if !kind.is_file() {
            return Err(invalid("Apple runtime payload contains a special file"));
        }
    }
    Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(root: &Path, label: &str) -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| invalid(error.to_string()))?
            .as_nanos();
        let path = root.join(format!(".{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).map_err(io_error)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse_json(content: &str, label: &str) -> Result<Value> {
    serde_json::from_str(content).map_err(|error| invalid(format!("invalid {label}: {error}")))
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("Apple runtime requires UTF-8 host paths"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}

fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "managed Apple container runtime".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cached_asset_requires_matching_nonzero_file_size() {
        let directory = Scratch::new(&std::env::temp_dir(), "apple-asset-size").unwrap();
        let file = directory.0.join("asset");
        fs::write(&file, b"cached").unwrap();
        assert!(verify_file(&file, 6).is_ok());
        assert!(verify_file(&file, 5).is_err());
        assert!(verify_file(&file, 0).is_err());
        assert!(verify_file(&directory.0, 6).is_err());
    }

    #[test]
    fn package_signature_rejects_other_developers() {
        assert!(validate_package_signature(
            "Status: signed by a developer certificate issued by Apple for distribution\n1. Developer ID Installer: Other Inc. (OTHERTEAM1)\nApple Root CA"
        ).is_err());
    }

    #[test]
    fn version_requires_exact_release_not_prefix_or_debug() {
        assert!(valid_cli_version(
            "container CLI version 1.7.1 (build: release, commit: a15b460)"
        ));
        assert!(!valid_cli_version(
            "container CLI version 1.7.10 (build: release, commit: a15b460)"
        ));
        assert!(!valid_cli_version(
            "container CLI version 1.7.1 (build: debug, commit: a15b460)"
        ));
    }

    #[test]
    fn payload_paths_reject_traversal_and_absolute_paths() {
        for path in [
            "../bin/container",
            "./bin/../../container",
            "/usr/local/bin/container",
            "",
            "bin/container\r",
        ] {
            assert!(!safe_archive_path(path), "{path:?}");
        }
        assert!(safe_archive_path(
            "./libexec/container/plugins/container-runtime-linux/bin/container-runtime-linux"
        ));
    }

    #[test]
    fn digest_cannot_be_used_for_snapshot_path_traversal() {
        assert!(digest_hex("sha256:../../snapshots").is_err());
        assert!(digest_hex(&format!("sha256:{}", "a".repeat(64))).is_ok());
    }

    #[test]
    fn service_ownership_requires_exact_roots_and_managed_program() {
        let output = "program = /kalisto/install/bin/container-apiserver\nenvironment = {\n CONTAINER_APP_ROOT => /kalisto/service\n CONTAINER_INSTALL_ROOT => /kalisto/install\n}";
        assert!(owned_service(
            output,
            Path::new("/kalisto/service"),
            Path::new("/kalisto/install")
        ));
        assert!(!owned_service(
            output,
            Path::new("/kalisto/service-other"),
            Path::new("/kalisto/install")
        ));
        assert!(!owned_service(
            &output.replace(
                "program = /kalisto/install/bin",
                "program = /kalisto/install-other/bin"
            ),
            Path::new("/kalisto/service"),
            Path::new("/kalisto/install")
        ));
    }

    #[test]
    fn service_status_rejects_same_version_with_foreign_data_root() {
        let status = json!({
            "status": "running",
            "server": {"version": "1.7.1", "build": "release", "appName": "container-apiserver"},
            "paths": {"appRoot": "/user/container", "installRoot": "/kalisto/install"}
        });
        assert!(
            validate_status(
                &status,
                Path::new("/kalisto/service"),
                Path::new("/kalisto/install")
            )
            .is_err()
        );
    }
}

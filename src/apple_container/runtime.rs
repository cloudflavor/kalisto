//! Compatible installed Apple runtimes, with a signed private fallback.
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
#[cfg(test)]
const IMAGES_LABEL: &str = "com.apple.container.container-core-images";
// ContainerPersistence/ContainerSystemConfig.swift at tag 1.7.1.
const KERNEL_URL: &str = "https://github.com/kata-containers/kata-containers/releases/download/4.3.1/kata-static-4.3.1-arm64.tar.zst";
const KERNEL_SHA256: &str = "2c5bf7cde334ad5307d482f45cceda496f91c9b6b96f5278c47f4f4eac1bbae6";
const KERNEL_MEMBER: &str = "opt/kata/share/kata-containers/vmlinux-6.18.56-208-debug";
const KERNEL_FILENAME: &str = "vmlinux-6.18.56-208-debug";
const MANIFEST: &str = "prepared.json";
const SELECTION: &str = "selection.json";
const REQUIRED_BINARIES: &[&str] = &[
    "bin/container",
    "bin/container-apiserver",
    "libexec/container/plugins/container-runtime-linux/bin/container-runtime-linux",
    "libexec/container/plugins/container-network-vmnet/bin/container-network-vmnet",
    "libexec/container/plugins/container-core-images/bin/container-core-images",
    "libexec/container/plugins/container-core-clusters/bin/container-core-clusters",
    "libexec/container/plugins/machine-apiserver/bin/machine-apiserver",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Selection {
    version: String,
    managed: bool,
    executable: PathBuf,
    api_program: PathBuf,
    install_root: PathBuf,
    app_root: PathBuf,
}

impl Selection {
    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(invalid(
                "incompatible Apple runtime selection; run `kalisto pull`",
            ));
        }
        for path in [
            &self.executable,
            &self.api_program,
            &self.install_root,
            &self.app_root,
        ] {
            validate_absolute(path)?;
        }
        let parent = self
            .executable
            .parent()
            .ok_or_else(|| invalid("Apple CLI has no parent"))?;
        if self
            .executable
            .file_name()
            .is_none_or(|name| name != "container")
            || self.api_program != parent.join("container-apiserver")
            || !matches!(
                parent.file_name().and_then(|name| name.to_str()),
                Some("bin" | "cli")
            )
            || parent.parent() != Some(self.install_root.as_path())
        {
            return Err(invalid(
                "Apple runtime executable does not match its installation layout",
            ));
        }
        if self.managed
            && (self.install_root != root()?.join("install")
                || self.app_root != state_root()?
                || parent.file_name().is_none_or(|name| name != "bin"))
        {
            return Err(invalid("managed Apple runtime selection has foreign roots"));
        }
        Ok(())
    }

    fn runtime_files(&self) -> [PathBuf; REQUIRED_BINARIES.len()] {
        std::array::from_fn(|index| match index {
            0 => self.executable.clone(),
            1 => self.api_program.clone(),
            _ => self.install_root.join(REQUIRED_BINARIES[index]),
        })
    }
}

#[derive(Deserialize, Serialize)]
struct Prepared {
    version: String,
    selection: Selection,
    package_sha256: String,
    kernel_sha256: String,
    init_image: String,
    init_index: String,
    init_manifest: String,
    kernel_size: u64,
    snapshot_size: u64,
    snapshot_info_size: u64,
    runtime_sizes: [u64; REQUIRED_BINARIES.len()],
}

pub(crate) fn root() -> Result<PathBuf> {
    let artifacts = artifacts_root()?;
    let artifacts = match artifacts.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::path::absolute(artifacts).map_err(io_error)?
        }
        Err(error) => return Err(io_error(error)),
    };
    Ok(artifacts.join("runtime-v1/apple-container-1.7.1-arm64"))
}

fn state_root() -> Result<PathBuf> {
    std::path::absolute(data_root()?.join("apple-container")).map_err(io_error)
}

fn load_selection_at(path: &Path) -> Result<Option<Selection>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let selected: Selection = serde_json::from_slice(&bytes).map_err(|error| {
        invalid(format!(
            "invalid Apple runtime selection: {error}; run `kalisto pull`"
        ))
    })?;
    selected.validate()?;
    Ok(Some(selected))
}

fn selection() -> Result<Selection> {
    load_selection_at(&state_root()?.join(SELECTION))?
        .ok_or_else(|| invalid("Apple runtime is not selected; run `kalisto pull` first"))
}

fn install_root() -> Result<PathBuf> {
    Ok(selection()?.install_root)
}

fn app_root() -> Result<PathBuf> {
    Ok(selection()?.app_root)
}

/// Resolve the user-data selection without probing PATH or downloading.
pub(crate) fn program() -> Result<PathBuf> {
    Ok(selection()?.executable)
}

pub(crate) fn kernel_path() -> Result<PathBuf> {
    Ok(root()?.join("kernels").join(KERNEL_FILENAME))
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
    configure_selection(command, &selection()?)?;
    Ok(())
}

fn configure_selection(command: &mut Command, selected: &Selection) -> Result<()> {
    command
        .env("CONTAINER_APP_ROOT", &selected.app_root)
        .env("CONTAINER_INSTALL_ROOT", &selected.install_root)
        .env("TMPDIR", root()?.join("tmp"))
        .env_remove("SSH_AUTH_SOCK");
    if selected.managed {
        command.env("XDG_CONFIG_HOME", root()?.join("config-home"));
    }
    Ok(())
}

fn validate_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(format!(
            "Apple runtime requires an absolute normalized path: {}",
            path.display()
        )));
    }
    path_str(path)?;
    Ok(())
}

fn managed_selection() -> Result<Selection> {
    let install_root = root()?.join("install");
    Ok(Selection {
        version: VERSION.into(),
        managed: true,
        executable: install_root.join("bin/container"),
        api_program: install_root.join("bin/container-apiserver"),
        install_root,
        app_root: state_root()?,
    })
}

fn service_selection(output: &str) -> Result<Selection> {
    let field = |key, separator| {
        launchctl_value(output, key, separator)
            .map(PathBuf::from)
            .ok_or_else(|| service_conflict(API_LABEL))
    };
    let api_program = field("program", "=")?;
    let install_root = field("CONTAINER_INSTALL_ROOT", "=>")?;
    let app_root = field("CONTAINER_APP_ROOT", "=>")?;
    let executable = api_program.with_file_name("container");
    let mut selected = Selection {
        version: VERSION.into(),
        managed: false,
        executable,
        api_program,
        install_root,
        app_root,
    };
    let managed = managed_selection()?;
    if selected.executable == managed.executable
        && selected.api_program == managed.api_program
        && selected.install_root == managed.install_root
        && selected.app_root == managed.app_root
    {
        selected.managed = true;
    }
    selected.validate()?;
    Ok(selected)
}

fn validate_installation(selected: &Selection) -> Result<()> {
    let canonical = selected.install_root.canonicalize().map_err(io_error)?;
    for file in selected.runtime_files() {
        if !file
            .canonicalize()
            .map_err(io_error)?
            .starts_with(&canonical)
        {
            return Err(invalid(
                "Apple runtime executable escapes its selected installation",
            ));
        }
        file_size(&file)?;
    }
    Ok(())
}

fn candidate_run(
    runner: &dyn CommandRunner,
    selected: &Selection,
    args: &[String],
) -> Result<Output> {
    // Discovery runs before persistence, so the global `container` resolver
    // cannot configure this executable yet. Preserve its environment isolation.
    let mut sanitized = Vec::new();
    for (key, _) in std::env::vars_os() {
        if let Some(key) = key.to_str()
            && (key.starts_with("CONTAINER_") || key.starts_with("DYLD_"))
            && !matches!(key, "CONTAINER_APP_ROOT" | "CONTAINER_INSTALL_ROOT")
        {
            sanitized.push("-u".into());
            sanitized.push(key.to_owned());
        }
    }
    let executable = path_str(&selected.executable)?;
    let (program, args) = if sanitized.is_empty() {
        (executable, args)
    } else {
        sanitized.push(executable.into());
        sanitized.extend_from_slice(args);
        ("/usr/bin/env", sanitized.as_slice())
    };
    runner.run_with_env(
        program,
        args,
        &[
            ("CONTAINER_APP_ROOT", &selected.app_root),
            ("CONTAINER_INSTALL_ROOT", &selected.install_root),
        ],
    )
}

fn registered_services(runner: &dyn CommandRunner) -> Result<Vec<String>> {
    let services = runner.run("/bin/launchctl", &strings(&["list"]))?;
    Ok(services
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|label| label.starts_with("com.apple.container."))
        .map(str::to_owned)
        .collect())
}

fn verify_borrowed_helper(
    runner: &dyn CommandRunner,
    label: &str,
    output: &str,
    selected: &Selection,
) -> Result<()> {
    let install_root = launchctl_value(output, "CONTAINER_INSTALL_ROOT", "=>")
        .map(PathBuf::from)
        .ok_or_else(|| service_conflict(label))?;
    validate_absolute(&install_root)?;
    let name = label.rsplit('.').next().unwrap_or_default();
    let relative = REQUIRED_BINARIES[2..]
        .iter()
        .find(|path| Path::new(path).file_name().is_some_and(|file| file == name))
        .ok_or_else(|| service_conflict(label))?;
    let program = launchctl_value(output, "program", "=")
        .map(Path::new)
        .ok_or_else(|| service_conflict(label))?;
    if !owned_service(output, &selected.app_root, &install_root)
        || program.canonicalize().map_err(io_error)?
            != install_root
                .join(relative)
                .canonicalize()
                .map_err(io_error)?
    {
        return Err(service_conflict(label));
    }
    let native_cli = install_root.join("cli/container");
    let executable = if native_cli.is_file() {
        native_cli
    } else {
        install_root.join("bin/container")
    };
    let helper_installation = Selection {
        version: VERSION.into(),
        managed: false,
        api_program: executable.with_file_name("container-apiserver"),
        executable,
        install_root,
        app_root: selected.app_root.clone(),
    };
    helper_installation.validate()?;
    verify_cli(runner, &helper_installation)?;
    validate_installation(&helper_installation)
}

fn verify_selected_services(
    runner: &dyn CommandRunner,
    domain: &str,
    labels: &[String],
    selected: &Selection,
) -> Result<()> {
    for label in labels {
        let service = runner.run(
            "/bin/launchctl",
            &strings(&["print", &format!("{domain}/{label}")]),
        )?;
        if !owned_service(&service.stdout, &selected.app_root, &selected.install_root) {
            if selected.managed || label == API_LABEL {
                return Err(service_conflict(label));
            }
            // A compatible user-owned helper may outlive a Homebrew upgrade.
            // Validate its own installation without re-registering or replacing it.
            verify_borrowed_helper(runner, label, &service.stdout, selected)?;
        } else if label == API_LABEL
            && launchctl_value(&service.stdout, "program", "=").map(Path::new)
                != Some(selected.api_program.as_path())
        {
            return Err(service_conflict(label));
        }
    }
    Ok(())
}

fn select_runtime(
    runner: &dyn CommandRunner,
    domain: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<Selection> {
    let labels = registered_services(runner)?;
    if labels.iter().any(|label| label == API_LABEL) {
        let service = runner.run(
            "/bin/launchctl",
            &strings(&["print", &format!("{domain}/{API_LABEL}")]),
        )?;
        let selected = service_selection(&service.stdout)?;
        let version = candidate_run(runner, &selected, &strings(&["--version"]))?;
        if !valid_cli_version(version.stdout.trim()) {
            return Err(service_conflict(API_LABEL));
        }
        let status = candidate_run(
            runner,
            &selected,
            &strings(&["system", "status", "--format", "json"]),
        )?;
        validate_status(
            &parse_json(&status.stdout, "Apple service status")?,
            &selected.app_root,
            &selected.install_root,
        )?;
        validate_installation(&selected)?;
        verify_selected_services(runner, domain, &labels, &selected)?;
        return Ok(selected);
    }
    // Orphaned helpers also own global Mach service names. Do not install over
    // them merely because the API service is absent.
    if let Some(label) = labels.first() {
        let managed = managed_selection()?;
        let service = runner.run(
            "/bin/launchctl",
            &strings(&["print", &format!("{domain}/{label}")]),
        )?;
        if !owned_service(&service.stdout, &managed.app_root, &managed.install_root) {
            return Err(service_conflict(label));
        }
        verify_selected_services(runner, domain, &labels, &managed)?;
        validate_installation(&managed)?;
        verify_cli(runner, &managed)?;
        return Ok(managed);
    }
    if let Some(path) = path {
        for directory in std::env::split_paths(path).filter(|directory| directory.is_absolute()) {
            let candidate = directory.join("container");
            let executable = match candidate.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
            };
            let parent = executable
                .parent()
                .ok_or_else(|| invalid("Apple CLI has no parent"))?;
            let install_root = parent
                .parent()
                .ok_or_else(|| invalid("Apple CLI has no install root"))?
                .to_path_buf();
            // Homebrew's bin/container is a shell wrapper exporting an opt/
            // install root. Use the actual cli/ siblings, not that wrapper.
            let executable = if parent.file_name().is_some_and(|name| name == "bin")
                && install_root.join("cli/container").is_file()
            {
                install_root.join("cli/container")
            } else {
                executable
            };
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .ok_or_else(|| invalid("HOME is required for an installed Apple runtime"))?;
            let selected = Selection {
                version: VERSION.into(),
                managed: false,
                api_program: executable.with_file_name("container-apiserver"),
                executable,
                install_root,
                app_root: PathBuf::from(home)
                    .join("Library/Application Support/com.apple.container"),
            };
            let version = candidate_run(runner, &selected, &strings(&["--version"]))?;
            if !valid_cli_version(version.stdout.trim()) {
                continue;
            }
            selected.validate()?;
            match validate_installation(&selected) {
                Ok(()) => return Ok(selected),
                Err(Error::CommandIo { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    managed_selection()
}

fn start_service(runner: &dyn CommandRunner, domain: &str, selected: &Selection) -> Result<()> {
    if selected.managed {
        run(
            runner,
            &[
                "system".into(),
                "start".into(),
                "--app-root".into(),
                selected.app_root.display().to_string(),
                "--install-root".into(),
                selected.install_root.display().to_string(),
                "--disable-kernel-install".into(),
            ],
        )?;
    } else {
        // SystemStart copies user configuration and overwrites the shared API
        // plist. Register a Kalisto-local plist instead; never edit either.
        let plist = state_root()?.join("borrowed-apiserver.plist");
        let value = serde_json::json!({
            "Label": API_LABEL,
            "ProgramArguments": [selected.api_program, "start"],
            "EnvironmentVariables": {
                "CONTAINER_APP_ROOT": selected.app_root,
                "CONTAINER_INSTALL_ROOT": selected.install_root,
            },
            "LimitLoadToSessionType": ["Aqua", "Background"],
            "RunAtLoad": true,
            "MachServices": { (API_LABEL): true },
        });
        publish_json(&plist, &value)?;
        runner.run(
            "/usr/bin/plutil",
            &strings(&["-convert", "xml1", path_str(&plist)?]),
        )?;
        runner.run(
            "/bin/launchctl",
            &strings(&["bootstrap", domain, path_str(&plist)?]),
        )?;
    }
    Ok(())
}

pub(crate) fn run(runner: &dyn CommandRunner, args: &[String]) -> Result<Output> {
    // The main runtime resolver supplies program() and configure() for this name.
    runner.run("container", args)
}

fn prepare_service(runner: &dyn CommandRunner, domain: &str, selected: &Selection) -> Result<()> {
    if selected.managed {
        for directory in [root()?.join("config-home"), selected.app_root.clone()] {
            fs::create_dir_all(directory).map_err(io_error)?;
        }
        install(runner)?;
    }
    verify_cli(runner, selected)?;
    let labels = registered_services(runner)?;
    verify_selected_services(runner, domain, &labels, selected)?;
    if !labels.iter().any(|label| label == API_LABEL) {
        start_service(runner, domain, selected)?;
    }
    let status = candidate_run(
        runner,
        selected,
        &strings(&["system", "status", "--format", "json"]),
    )?;
    validate_status(
        &parse_json(&status.stdout, "Apple service status")?,
        &selected.app_root,
        &selected.install_root,
    )
}

pub(crate) fn prepare(runner: &dyn CommandRunner) -> Result<()> {
    check_platform(runner)?;
    let state = state_root()?;
    fs::create_dir_all(&state).map_err(io_error)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("prepare.lock"))
        .map_err(io_error)?;
    lock.try_lock().map_err(|error| {
        invalid(format!(
            "another Apple runtime preparation is active: {error}"
        ))
    })?;
    // Resolve the shared login-session service before any private installation
    // or download. A foreign or incompatible daemon must never be replaced.
    let domain = launchd_domain(runner)?;
    let selected = select_runtime(runner, &domain, std::env::var_os("PATH").as_deref())?;
    let root = root()?;
    fs::create_dir_all(root.join("tmp")).map_err(io_error)?;
    publish_json(&state.join(SELECTION), &selected)?;
    prepare_service(runner, &domain, &selected)?;
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
    let source = super::registry::prepare_remote_image(runner, source)?;
    // Infra images are deliberately hidden by Apple's image inspect command.
    // A separate local alias exposes the descriptor and platform manifest.
    let probe = "kalisto.invalid/vminit:apple-1.7.1";
    run(runner, &strings(&["image", "tag", &source, probe]))?;
    let (index, manifest) = inspect_init(runner, probe)?;
    let reference = format!("kalisto.invalid/vminit:sha256-{}", digest_hex(&index)?);
    run(runner, &strings(&["image", "tag", probe, &reference]))?;
    let snapshot = snapshot_root(&manifest)?;
    let mut runtime_sizes = [0; REQUIRED_BINARIES.len()];
    for (size, path) in runtime_sizes.iter_mut().zip(selected.runtime_files()) {
        *size = file_size(&path)?;
    }
    // Runtime-owned snapshots can be 512 GiB sparse disks. Bind their metadata
    // to the selected service and OCI descriptors without scanning sparse extents.
    let prepared = Prepared {
        version: VERSION.into(),
        selection: selected,
        package_sha256: PACKAGE_SHA256.into(),
        kernel_sha256: artifact::file_sha256(runner, &kernel_path()?)?,
        init_image: reference,
        init_index: index,
        init_manifest: manifest,
        kernel_size: file_size(&kernel_path()?)?,
        snapshot_size: file_size(&snapshot.join("snapshot"))?,
        snapshot_info_size: file_size(&snapshot.join("snapshot-info"))?,
        runtime_sizes,
    };
    publish_json(&state.join(MANIFEST), &prepared)?;
    Ok(())
}

/// Offline preflight, including restarting only Kalisto's own launchd service.
pub(crate) fn check_available(runner: &dyn CommandRunner) -> Result<()> {
    check_platform(runner)?;
    let prepared = load_prepared()?;
    for (path, expected) in prepared
        .selection
        .runtime_files()
        .iter()
        .zip(&prepared.runtime_sizes)
    {
        verify_file(path, *expected)?;
    }
    verify_version(runner)?;
    verify_file(&kernel_path()?, prepared.kernel_size)?;
    let snapshot = snapshot_root(&prepared.init_manifest)?;
    verify_file(&snapshot.join("snapshot"), prepared.snapshot_size)?;
    verify_file(&snapshot.join("snapshot-info"), prepared.snapshot_info_size)?;
    let domain = launchd_domain(runner)?;
    if !check_services(runner, &domain)? {
        if !prepared.selection.managed {
            return Err(invalid(
                "selected borrowed Apple service is not running; start it with its `container system start` before offline `kalisto up`",
            ));
        }
        let plist = app_root()?.join("apiserver/apiserver.plist");
        let value = read_plist(runner, &plist)?;
        validate_plist(
            &value,
            API_LABEL,
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
    verify_cli(runner, &selection()?)
}

fn verify_cli(runner: &dyn CommandRunner, selected: &Selection) -> Result<()> {
    if !selected.executable.is_file() {
        return Err(invalid(
            "selected Apple container 1.7.1 is missing; run `kalisto pull` first",
        ));
    }
    let version = candidate_run(runner, selected, &strings(&["--version"]))?;
    if !valid_cli_version(version.stdout.trim()) {
        return Err(invalid(format!(
            "expected Apple container {VERSION} release build, found {}",
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
    // Keep the exact pinned kernel on Kalisto's cache volume. Nodes pass this
    // binary explicitly; never change a shared service's default kernel.
    let archive = artifact::fetch_into_cache(
        runner,
        &root()?.join("downloads"),
        KERNEL_URL,
        Some(KERNEL_SHA256),
        "Apple recommended kernel",
        "kata-static-4.3.1-arm64.tar.zst",
    )?;
    let scratch = Scratch::new(&root()?, "kernel")?;
    let compressed = fs::File::open(&archive).map_err(io_error)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(compressed)
        .map_err(|error| invalid(format!("invalid compressed Apple kernel archive: {error}")))?;
    let mut archive = tar::Archive::new(decoder);
    let binary = scratch.0.join(KERNEL_FILENAME);
    for entry in archive.entries().map_err(io_error)? {
        let mut entry = entry.map_err(io_error)?;
        let path = entry.path().map_err(io_error)?;
        let path = path.strip_prefix("./").unwrap_or(&path);
        if path != Path::new(KERNEL_MEMBER) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid(
                "Apple kernel archive member must be a regular file",
            ));
        }
        let mut output = fs::File::create_new(&binary).map_err(io_error)?;
        std::io::copy(&mut entry, &mut output).map_err(io_error)?;
        break;
    }
    file_size(&binary)?;
    let destination = kernel_path()?;
    fs::create_dir_all(
        destination
            .parent()
            .ok_or_else(|| invalid("kernel has no parent"))?,
    )
    .map_err(io_error)?;
    fs::rename(binary, destination).map_err(io_error)?;
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
    let labels = registered_services(runner)?;
    verify_selected_services(runner, domain, &labels, &selection()?)?;
    Ok(labels.iter().any(|label| label == API_LABEL))
}

fn owned_service(output: &str, app: &Path, install: &Path) -> bool {
    launchctl_value(output, "CONTAINER_APP_ROOT", "=>").map(Path::new) == Some(app)
        && launchctl_value(output, "CONTAINER_INSTALL_ROOT", "=>").map(Path::new) == Some(install)
        && launchctl_value(output, "program", "=").is_some_and(|program| {
            let program = Path::new(program);
            validate_absolute(program).is_ok() && program.starts_with(install)
        })
}

fn launchctl_value<'a>(output: &'a str, key: &str, separator: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(separator)?;
        (name.trim() == key).then_some(value.trim())
    })
}

fn service_conflict(label: &str) -> Error {
    invalid(format!(
        "Apple's login-session service `{label}` belongs to another installation or is incompatible with container {VERSION} release. Kalisto will not stop, replace, or download over it. Use a compatible service or the Lima backend."
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
        || status["paths"]["appRoot"].as_str().map(Path::new) != Some(app)
        || status["paths"]["installRoot"].as_str().map(Path::new) != Some(install)
    {
        return Err(service_conflict(API_LABEL));
    }
    Ok(())
}

fn read_plist(runner: &dyn CommandRunner, path: &Path) -> Result<Value> {
    let value = runner.run(
        "/usr/bin/plutil",
        &strings(&["-convert", "json", "-o", "-", path_str(path)?]),
    )?;
    parse_json(&value.stdout, "Apple launchd plist")
}

fn validate_plist(value: &Value, label: &str, mach_service: &str, executable: &Path) -> Result<()> {
    if value["Label"] != label
        || value["ProgramArguments"]
            .get(0)
            .and_then(Value::as_str)
            .map(Path::new)
            != Some(executable)
        || value["ProgramArguments"].get(1).and_then(Value::as_str) != Some("start")
        || value["EnvironmentVariables"]["CONTAINER_APP_ROOT"]
            .as_str()
            .map(Path::new)
            != Some(app_root()?.as_path())
        || value["EnvironmentVariables"]["CONTAINER_INSTALL_ROOT"]
            .as_str()
            .map(Path::new)
            != Some(install_root()?.as_path())
        || value["MachServices"][mach_service] != true
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
    let path = state_root()?.join(MANIFEST);
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
    validate_prepared(&prepared, &selection()?)?;
    Ok(prepared)
}

fn validate_prepared(prepared: &Prepared, selected: &Selection) -> Result<()> {
    if prepared.version != VERSION
        || &prepared.selection != selected
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
    Ok(())
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
        program: "Apple container runtime".into(),
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

    #[test]
    fn service_ownership_accepts_owned_directory_urls_with_trailing_separators() {
        let output = "program = /kalisto/install/bin/container-apiserver\nCONTAINER_APP_ROOT => /kalisto/service/\nCONTAINER_INSTALL_ROOT => /kalisto/install/";
        assert!(owned_service(
            output,
            Path::new("/kalisto/service"),
            Path::new("/kalisto/install")
        ));
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::*;
    use std::cell::RefCell;

    struct Fixture {
        _directory: Scratch,
        selected: Selection,
    }

    impl Fixture {
        fn new(layout: &str) -> Self {
            let directory = Scratch::new(&std::env::temp_dir(), "apple-runtime-reuse").unwrap();
            let base = directory.0.canonicalize().unwrap();
            let install_root = base.join("Cellar/container/1.7.1_2");
            let selected = Selection {
                version: VERSION.into(),
                managed: false,
                executable: install_root.join(layout).join("container"),
                api_program: install_root.join(layout).join("container-apiserver"),
                install_root,
                app_root: base.join("Application Support/com.apple.container"),
            };
            for file in selected.runtime_files() {
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, b"existing runtime").unwrap();
            }
            fs::create_dir_all(&selected.app_root).unwrap();
            Self {
                _directory: directory,
                selected,
            }
        }
    }

    struct DiscoveryRunner {
        selected: Selection,
        helper: Option<Selection>,
        helper_version: Option<&'static str>,
        running: bool,
        cli_version: &'static str,
        status: Value,
        fail_version: bool,
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl DiscoveryRunner {
        fn new(selected: &Selection, running: bool) -> Self {
            Self {
                selected: selected.clone(),
                helper: None,
                helper_version: None,
                running,
                cli_version: "container CLI version 1.7.1 (build: release, commit: a15b460)",
                status: serde_json::json!({
                    "status": "running",
                    "server": {"version": VERSION, "build": "release", "appName": "container-apiserver"},
                    "paths": {"appRoot": selected.app_root, "installRoot": selected.install_root},
                }),
                fail_version: false,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn service(selected: &Selection, program: &Path) -> String {
            format!(
                "program = {}\nCONTAINER_APP_ROOT => {}\nCONTAINER_INSTALL_ROOT => {}\n",
                program.display(),
                selected.app_root.display(),
                selected.install_root.display()
            )
        }
    }

    impl CommandRunner for DiscoveryRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            self.calls
                .borrow_mut()
                .push((program.into(), args.to_vec()));
            let stdout = if program == "/bin/launchctl" && args == ["list"] {
                if self.running {
                    format!("123\t0\t{API_LABEL}\n124\t0\t{IMAGES_LABEL}\n")
                } else {
                    String::new()
                }
            } else if program == "/bin/launchctl"
                && args == ["print", &format!("gui/501/{API_LABEL}")]
            {
                Self::service(&self.selected, &self.selected.api_program)
            } else if program == "/bin/launchctl"
                && args == ["print", &format!("gui/501/{IMAGES_LABEL}")]
            {
                let helper = self.helper.as_ref().unwrap_or(&self.selected);
                Self::service(helper, &helper.install_root.join(REQUIRED_BINARIES[4]))
            } else if Path::new(program) == self.selected.executable && args == ["--version"] {
                if self.fail_version {
                    return Err(invalid("injected CLI execution failure"));
                }
                self.cli_version.into()
            } else if self
                .helper
                .as_ref()
                .is_some_and(|helper| Path::new(program) == helper.executable)
                && args == ["--version"]
            {
                self.helper_version.unwrap_or(self.cli_version).into()
            } else if Path::new(program) == self.selected.executable
                && args == ["system", "status", "--format", "json"]
            {
                self.status.to_string()
            } else {
                panic!("unexpected command (installation or service mutation): {program} {args:?}");
            };
            Ok(Output {
                stdout,
                stderr: String::new(),
            })
        }

        fn run_with_env(
            &self,
            program: &str,
            args: &[String],
            env: &[(&str, &Path)],
        ) -> Result<Output> {
            let selected = self
                .helper
                .as_ref()
                .filter(|helper| env[1].1 == helper.install_root)
                .unwrap_or(&self.selected);
            assert_eq!(
                env[1],
                ("CONTAINER_INSTALL_ROOT", selected.install_root.as_path())
            );
            assert_eq!(env[0].0, "CONTAINER_APP_ROOT");
            assert!(env[0].1.is_absolute());
            if self.running {
                assert_eq!(env[0].1, selected.app_root);
            }
            if program == "/usr/bin/env" {
                let executable = args
                    .iter()
                    .position(|arg| Path::new(arg) == selected.executable)
                    .unwrap();
                self.run(&args[executable], &args[executable + 1..])
            } else {
                self.run(program, args)
            }
        }
    }

    #[test]
    fn running_homebrew_release_is_reused_without_install_or_shared_helper_mutation() {
        let fixture = Fixture::new("cli");
        let runner = DiscoveryRunner::new(&fixture.selected, true);
        let plist = fixture.selected.app_root.join("service.plist");
        fs::write(&plist, b"untouched shared service").unwrap();
        let selected = select_runtime(
            &runner,
            "gui/501",
            Some(std::ffi::OsStr::new("/irrelevant")),
        )
        .unwrap();
        prepare_service(&runner, "gui/501", &selected).unwrap();
        assert_eq!(selected, fixture.selected);
        assert_eq!(fs::read(plist).unwrap(), b"untouched shared service");
        assert_eq!(runner.calls.borrow().len(), 11);
    }

    #[test]
    fn running_official_layout_is_also_borrowed() {
        let fixture = Fixture::new("bin");
        let runner = DiscoveryRunner::new(&fixture.selected, true);
        assert_eq!(
            select_runtime(&runner, "gui/501", None).unwrap(),
            fixture.selected
        );
    }

    #[test]
    fn compatible_helper_from_another_installation_is_borrowed_without_mutation() {
        let fixture = Fixture::new("cli");
        let mut helper = Fixture::new("bin");
        helper
            .selected
            .app_root
            .clone_from(&fixture.selected.app_root);
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.helper = Some(helper.selected.clone());
        let plist = helper.selected.install_root.join("service.plist");
        fs::write(&plist, b"existing image helper").unwrap();

        let selected = select_runtime(&runner, "gui/501", None).unwrap();
        prepare_service(&runner, "gui/501", &selected).unwrap();
        assert_eq!(selected, fixture.selected);
        assert_eq!(fs::read(plist).unwrap(), b"existing image helper");
    }

    #[test]
    fn incompatible_helper_reports_version_before_missing_newer_files() {
        let fixture = Fixture::new("cli");
        let mut helper = Fixture::new("bin");
        helper
            .selected
            .app_root
            .clone_from(&fixture.selected.app_root);
        fs::remove_file(
            helper
                .selected
                .install_root
                .join("libexec/container/plugins/machine-apiserver/bin/machine-apiserver"),
        )
        .unwrap();
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.helper = Some(helper.selected);
        runner.helper_version = Some("container CLI version 0.10.0 (build: release, commit: old)");

        let error = select_runtime(&runner, "gui/501", None).unwrap_err();
        assert!(error.to_string().contains("expected Apple container 1.7.1"));
    }

    #[test]
    fn helper_with_another_application_root_remains_a_conflict() {
        let fixture = Fixture::new("cli");
        let helper = Fixture::new("bin");
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.helper = Some(helper.selected);

        let error = select_runtime(&runner, "gui/501", None).unwrap_err();
        assert!(error.to_string().contains(IMAGES_LABEL));
    }

    #[test]
    fn borrowed_helper_must_run_its_expected_packaged_executable() {
        let fixture = Fixture::new("cli");
        let mut helper = Fixture::new("bin");
        helper
            .selected
            .app_root
            .clone_from(&fixture.selected.app_root);
        let runner = DiscoveryRunner::new(&fixture.selected, true);
        let output = DiscoveryRunner::service(&helper.selected, &helper.selected.executable);

        let error =
            verify_borrowed_helper(&runner, IMAGES_LABEL, &output, &fixture.selected).unwrap_err();
        assert!(error.to_string().contains(IMAGES_LABEL));
    }

    #[test]
    fn running_incompatible_version_never_reaches_download_or_replacement() {
        let fixture = Fixture::new("cli");
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.status["server"]["version"] = Value::String("1.7.0".into());
        let error = select_runtime(&runner, "gui/501", None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("will not stop, replace, or download")
        );
        assert_eq!(runner.calls.borrow().len(), 4);
    }

    #[test]
    fn running_same_version_with_mismatched_roots_is_rejected() {
        let fixture = Fixture::new("cli");
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.status["paths"]["appRoot"] = Value::String("/another/users/runtime".into());
        assert!(select_runtime(&runner, "gui/501", None).is_err());
    }

    #[test]
    fn discovery_does_not_hide_unrelated_cli_execution_errors() {
        let fixture = Fixture::new("cli");
        let mut runner = DiscoveryRunner::new(&fixture.selected, true);
        runner.fail_version = true;
        let error = select_runtime(&runner, "gui/501", None).unwrap_err();
        assert_eq!(error.to_string(), "injected CLI execution failure");
    }

    #[test]
    fn installed_homebrew_wrapper_resolves_native_cli_and_real_install_root() {
        let fixture = Fixture::new("cli");
        let bin = fixture.selected.install_root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(
            bin.join("container"),
            b"#!/bin/sh\nexec ../cli/container \"$@\"\n",
        )
        .unwrap();
        let runner = DiscoveryRunner::new(&fixture.selected, false);
        let selected = select_runtime(&runner, "gui/501", Some(bin.as_os_str())).unwrap();
        assert_eq!(selected.executable, fixture.selected.executable);
        assert_eq!(selected.install_root, fixture.selected.install_root);
        assert!(!selected.managed);
    }

    #[test]
    fn no_compatible_installation_selects_signed_private_fallback() {
        let fixture = Fixture::new("bin");
        let mut runner = DiscoveryRunner::new(&fixture.selected, false);
        runner.cli_version = "container CLI version 1.8.0 (build: release, commit: other)";
        let bin = fixture.selected.executable.parent().unwrap();
        let selected = select_runtime(&runner, "gui/501", Some(bin.as_os_str())).unwrap();
        assert_eq!(selected, managed_selection().unwrap());
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    #[test]
    fn persisted_selection_restores_borrowed_executable_and_roots_without_discovery() {
        let fixture = Fixture::new("cli");
        let path = fixture.selected.app_root.join(SELECTION);
        publish_json(&path, &fixture.selected).unwrap();
        assert_eq!(load_selection_at(&path).unwrap(), Some(fixture.selected));
    }

    #[test]
    fn selection_rejects_a_daemon_outside_the_cli_installation_layout() {
        let fixture = Fixture::new("cli");
        let mut selected = fixture.selected;
        selected.api_program = PathBuf::from("/another/runtime/container-apiserver");
        assert!(selected.validate().is_err());
    }

    #[test]
    fn helper_program_cannot_escape_matching_installation_roots_with_parent_components() {
        let output = "program = /runtime/../foreign/container-core-images\nCONTAINER_APP_ROOT => /state\nCONTAINER_INSTALL_ROOT => /runtime\n";
        assert!(!owned_service(
            output,
            Path::new("/state"),
            Path::new("/runtime")
        ));
    }

    #[test]
    fn incomplete_installed_runtime_uses_private_fallback_without_hiding_other_failures() {
        let fixture = Fixture::new("bin");
        fs::remove_file(fixture.selected.install_root.join(REQUIRED_BINARIES[4])).unwrap();
        let runner = DiscoveryRunner::new(&fixture.selected, false);
        let bin = fixture.selected.executable.parent().unwrap();
        let selected = select_runtime(&runner, "gui/501", Some(bin.as_os_str())).unwrap();
        assert_eq!(selected, managed_selection().unwrap());
    }

    #[test]
    fn borrowed_cli_configuration_never_overrides_shared_user_configuration() {
        let fixture = Fixture::new("cli");
        let mut command = Command::new(&fixture.selected.executable);
        configure_selection(&mut command, &fixture.selected).unwrap();
        let changes = command.get_envs().collect::<Vec<_>>();
        assert!(!changes.iter().any(|(key, _)| *key == "XDG_CONFIG_HOME"));
        let temporary = root().unwrap().join("tmp");
        assert!(
            changes
                .iter()
                .any(|(key, value)| { *key == "TMPDIR" && *value == Some(temporary.as_os_str()) })
        );
    }
}

#[cfg(test)]
mod prepared_selection_tests {
    use super::*;

    #[test]
    fn prepared_assets_from_another_service_cannot_validate_selected_runtime() {
        let selected = Selection {
            version: VERSION.into(),
            managed: false,
            executable: PathBuf::from("/brew/container/cli/container"),
            api_program: PathBuf::from("/brew/container/cli/container-apiserver"),
            install_root: PathBuf::from("/brew/container"),
            app_root: PathBuf::from("/users/alice/container"),
        };
        let index = format!("sha256:{}", "a".repeat(64));
        let prepared = Prepared {
            version: VERSION.into(),
            selection: selected.clone(),
            package_sha256: PACKAGE_SHA256.into(),
            kernel_sha256: "b".repeat(64),
            init_image: format!("kalisto.invalid/vminit:sha256-{}", "a".repeat(64)),
            init_index: index,
            init_manifest: format!("sha256:{}", "c".repeat(64)),
            kernel_size: 100,
            snapshot_size: 100,
            snapshot_info_size: 100,
            runtime_sizes: [100; REQUIRED_BINARIES.len()],
        };
        validate_prepared(&prepared, &selected).unwrap();
        let mut foreign = selected;
        foreign.app_root = PathBuf::from("/users/bob/container");
        assert!(validate_prepared(&prepared, &foreign).is_err());
    }
}

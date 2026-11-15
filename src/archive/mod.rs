//! Cold whole-cluster archives. VM disks contain credentials and application data.
//! Only trusted archives should be imported; import never starts their guest code.

mod crypto;
#[cfg(test)]
mod crypto_tests;
mod format;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use self::format::{Manifest, Scratch, invalid, io_error, private_file};
use crate::backend::BackendKind;
use crate::cluster::spec::ClusterSpec;
use crate::cluster::state::ClusterState;
use crate::error::Result;
use crate::k3s::NodeRole;
use crate::paths::data_root;
use crate::process::{CommandRunner, SystemRunner};

pub use self::crypto::{ExportEncryption, ImportDecryption, generate_identity};
pub use self::format::DEFAULT_MAX_BYTES;

#[derive(Debug)]
pub struct ExportOutcome {
    pub name: String,
    pub path: PathBuf,
    pub nodes: usize,
    pub bytes: u64,
    pub encrypted: bool,
}
#[derive(Debug)]
pub struct ImportOutcome {
    pub name: String,
    pub nodes: usize,
}

/// Export stopped native VZ nodes. `stop` authorizes clean OS shutdown, not force.
/// Applications must be quiesced by their owner when application consistency matters.
pub fn export(name: &str, output: &Path, stop: bool) -> Result<ExportOutcome> {
    require_macos()?;
    export_encrypted(name, output, stop, &ExportEncryption::Plaintext)
}

/// Stream gzip directly into an age envelope when protection is selected.
pub fn export_encrypted(
    name: &str,
    output: &Path,
    stop: bool,
    protection: &ExportEncryption,
) -> Result<ExportOutcome> {
    require_macos()?;
    export_protected_with_runner(name, output, stop, &SystemRunner, protection)
}

pub fn export_with_runner(
    name: &str,
    output: &Path,
    stop: bool,
    runner: &dyn CommandRunner,
) -> Result<ExportOutcome> {
    export_protected_with_runner(name, output, stop, runner, &ExportEncryption::Plaintext)
}

pub(crate) fn export_protected_with_runner(
    name: &str,
    output: &Path,
    stop: bool,
    runner: &dyn CommandRunner,
    protection: &ExportEncryption,
) -> Result<ExportOutcome> {
    if !protection.is_encrypted() && output.extension().is_some_and(|ext| ext == "age") {
        return Err(invalid(
            ".age output requires --recipient, --passphrase, or --passphrase-file",
        ));
    }
    validate_name(name)?;
    if fs::symlink_metadata(output).is_ok() {
        return Err(invalid("archive output already exists"));
    }
    let root = ClusterSpec::state_root_for(name)?;
    let _operation = crate::operation::lock(&root)?;
    let state = ClusterState::load(&root)?;
    validate_state(&state)?;
    let spec = spec_for(&state)?;
    let assets = crate::pull::load(&spec)?;
    let image = base_image(&state.template)?;
    let mut sources = vec![("base-image".into(), image)];
    sources.push((
        "assets/manifest.json".into(),
        assets.root.join("manifest.json"),
    ));
    for file in assets.filenames() {
        sources.push((format!("assets/{file}"), assets.root.join(file)));
    }
    for (index, node) in state.nodes.iter().enumerate() {
        let info = inspect(runner, &node.name)?;
        validate_instance(&info, &state, &assets.root, node)?;
        if info["status"] != "Stopped" && !stop {
            return Err(invalid(
                "cluster must be stopped before export; use --stop to authorize clean shutdown",
            ));
        }
        let dir = instance_dir(&info, &node.name)?;
        sources.push((format!("nodes/{index}/disk"), dir.join("disk")));
        if dir.join("vz-efi").is_file() {
            sources.push((format!("nodes/{index}/vz-efi"), dir.join("vz-efi")));
        }
        for entry in fs::read_dir(&node.artifacts_root).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if matches!(file, "kubeconfig.yaml" | "node-token")
                || (file.starts_with("cilium-") && file.ends_with(".json"))
            {
                sources.push((format!("runtime/{index}/{file}"), path));
            } else if !matches!(file, "k3s.stdout.log" | "k3s.stderr.log" | "k3s.pid") {
                return Err(invalid(
                    "runtime contains extra host-mounted files; export refuses to silently omit their data",
                ));
            }
        }
    }
    for (_, source) in &sources {
        if !fs::symlink_metadata(source).map_err(io_error)?.is_file() {
            return Err(invalid(
                "archive source is not a regular file; legacy/backing disks are unsupported",
            ));
        }
    }
    // Preflight every node and output before stopping anything. Never force stop.
    if stop {
        for node in state.nodes.iter().rev() {
            if inspect(runner, &node.name)?["status"] != "Stopped" {
                runner.run(
                    "limactl",
                    &["stop".into(), "--tty=false".into(), node.name.clone()],
                )?;
            }
        }
    }
    ensure_stopped(&state, runner)?;
    let mut portable = state.clone();
    portable.workspace_root = PathBuf::new();
    portable.template = "archive:base-image".into();
    portable.kubeconfig_path = PathBuf::new();
    portable.token_path = PathBuf::new();
    for node in &mut portable.nodes {
        node.artifacts_root = PathBuf::new();
    }
    let manifest = Manifest {
        schema: 1,
        architecture: std::env::consts::ARCH.into(),
        lima_version: crate::runtime::LIMA_VERSION.into(),
        contains_secrets: true,
        state: serde_json::from_slice(&portable.to_bytes()?)
            .map_err(|error| invalid(error.to_string()))?,
        files: Vec::new(),
    };
    // Write into task-owned staging, then recheck stopped status before publishing.
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let scratch = Scratch::new(parent)?;
    let staged = scratch.0.join("snapshot.kalisto");
    format::write_protected(&staged, manifest, &sources, protection)?;
    if !protection.is_encrypted() {
        format::verify(&staged, DEFAULT_MAX_BYTES)?;
    }
    ensure_stopped(&state, runner)?;
    fs::hard_link(&staged, output).map_err(io_error)?;
    let bytes = fs::metadata(output).map_err(io_error)?.len();
    if protection.is_encrypted() {
        tracing::warn!(
            "Encrypted archive contains credentials and application data; keep recovery keys separate and safe"
        );
    } else {
        tracing::warn!(
            "UNENCRYPTED archive contains credentials and application data; use --recipient or --passphrase before sharing"
        );
    }
    Ok(ExportOutcome {
        name: name.into(),
        path: output.to_path_buf(),
        nodes: state.nodes.len(),
        bytes,
        encrypted: protection.is_encrypted(),
    })
}

/// Verify and restore a trusted archive. Does not overwrite, rename, or start nodes.
/// Verified managed host tools may be downloaded if the target has not prepared them.
pub fn import(input: &Path, api_port: Option<u16>, max_bytes: u64) -> Result<ImportOutcome> {
    require_macos()?;
    import_encrypted(input, api_port, max_bytes, &ImportDecryption::None)
}

/// Fully authenticate/decompress/check all payloads before publishing any VM.
pub fn import_encrypted(
    input: &Path,
    api_port: Option<u16>,
    max_bytes: u64,
    credentials: &ImportDecryption,
) -> Result<ImportOutcome> {
    require_macos()?;
    import_protected_with_runner(input, api_port, max_bytes, &SystemRunner, true, credentials)
}

#[cfg(test)]
pub(crate) fn import_with_runner(
    input: &Path,
    api_port: Option<u16>,
    max_bytes: u64,
    runner: &dyn CommandRunner,
    prepare_tools: bool,
) -> Result<ImportOutcome> {
    import_protected_with_runner(
        input,
        api_port,
        max_bytes,
        runner,
        prepare_tools,
        &ImportDecryption::None,
    )
}

pub(crate) fn import_protected_with_runner(
    input: &Path,
    api_port: Option<u16>,
    max_bytes: u64,
    runner: &dyn CommandRunner,
    prepare_tools: bool,
    credentials: &ImportDecryption,
) -> Result<ImportOutcome> {
    if max_bytes == 0 {
        return Err(invalid("archive size limit must be positive"));
    }
    if api_port == Some(0) {
        return Err(invalid("API port must be between 1 and 65535"));
    }
    let scratch = Scratch::new(&data_root()?.join("archive-staging"))?;
    let manifest = format::read_protected(input, &scratch.0, max_bytes, credentials)?;
    if manifest.architecture != std::env::consts::ARCH
        || manifest.lima_version != crate::runtime::LIMA_VERSION
    {
        return Err(invalid(
            "archive architecture/Lima version is not compatible with this installation",
        ));
    }
    let mut state = ClusterState::from_bytes(
        &serde_json::to_vec(&manifest.state).map_err(|error| invalid(error.to_string()))?,
    )?;
    validate_state(&state)?;
    let root = ClusterSpec::state_root_for(&state.name)?;
    let _operation = crate::operation::lock(&root)?;
    if fs::symlink_metadata(&root).is_ok() {
        return Err(invalid(
            "cluster state destination already exists; import never overwrites",
        ));
    }
    let home = lima_home()?;
    let expected = expected_payloads(&state, &scratch.0)?;
    if manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(invalid(
            "archive payload does not match cluster node inventory",
        ));
    }
    for node in &state.nodes {
        let path = home.join(&node.name);
        if fs::symlink_metadata(&path).is_ok() {
            return Err(invalid(format!(
                "Lima instance {} already exists",
                node.name
            )));
        }
        if path.join("ssh.sock.1234567890123456").as_os_str().len() >= 104 {
            return Err(invalid(
                "Lima instance path exceeds macOS socket path limit",
            ));
        }
    }
    let spec = spec_for(&state)?;
    crate::pull::load_from_root(&spec, &scratch.0.join("assets"))?;
    if prepare_tools {
        crate::runtime::prepare(
            runner,
            state.cni_plugin == crate::cluster::addon::CniPlugin::Cilium,
        )?;
    }
    state.api_port = api_port.unwrap_or(state.api_port);
    state.workspace_root = std::env::current_dir()
        .map_err(io_error)?
        .canonicalize()
        .map_err(io_error)?;
    state.template = root.join("archive/node.yaml").display().to_string();
    state.kubeconfig_path = root.join("kubeconfig.yaml");
    state.token_path = root.join("node-token");
    state.firecracker_kernel = None;
    state.firecracker_rootfs = None;
    let mut owned = OwnedDestinations(Vec::new());
    reserve(&root, &mut owned)?;
    private_file(&root.join("archive-import-incomplete"))?
        .sync_all()
        .map_err(io_error)?;
    let archive_root = root.join("archive");
    fs::create_dir(&archive_root).map_err(io_error)?;
    copy_payload(
        &scratch.0.join("base-image"),
        &archive_root.join("base-image"),
    )?;
    fs::rename(scratch.0.join("assets"), archive_root.join("assets")).map_err(io_error)?;
    let template = json!({"vmType":"vz","mountType":"virtiofs","mounts":[],"containerd":{"system":false,"user":false},
        "images":[{"location":archive_root.join("base-image"),"arch":crate::cluster::spec::ImageArch::host_default().lima_template_value()}]});
    write_json(&root.join("archive/node.yaml"), &template)?;
    fs::create_dir_all(&home).map_err(io_error)?;
    for (index, node) in state.nodes.iter_mut().enumerate() {
        let dir = home.join(&node.name);
        reserve(&dir, &mut owned)?;
        copy_payload(
            &scratch.0.join(format!("nodes/{index}/disk")),
            &dir.join("disk"),
        )?;
        let firmware = scratch.0.join(format!("nodes/{index}/vz-efi"));
        if firmware.is_file() {
            copy_payload(&firmware, &dir.join("vz-efi"))?;
        }
        // Lima itself nullifies this on clone: regenerate a host-local VZ identity.
        private_file(&dir.join("vz-identifier"))?
            .sync_all()
            .map_err(io_error)?;
        node.artifacts_root = root
            .join("runtime")
            .join(if node.role == NodeRole::ControlPlane {
                "control-plane"
            } else {
                &node.name
            });
        fs::create_dir_all(&node.artifacts_root).map_err(io_error)?;
        let runtime = scratch.0.join(format!("runtime/{index}"));
        if runtime.is_dir() {
            for entry in fs::read_dir(runtime).map_err(io_error)? {
                let file = entry.map_err(io_error)?;
                copy_payload(&file.path(), &node.artifacts_root.join(file.file_name()))?;
            }
        }
        let resources = if node.role == NodeRole::ControlPlane {
            state.control_plane_resources
        } else {
            state.worker_resources
        };
        let mut config = json!({
            "vmType":"vz","os":"Linux","arch":crate::cluster::spec::ImageArch::host_default().lima_template_value(),
            "mountType":"virtiofs","cpus":resources.cpus,"memory":format!("{}GiB",resources.memory_gib),
            "disk":format!("{}B",fs::metadata(dir.join("disk")).map_err(io_error)?.len()),
            "images":[{"location":archive_root.join("base-image"),"arch":crate::cluster::spec::ImageArch::host_default().lima_template_value()}],
            "containerd":{"system":false,"user":false},"provision":[],
            "rosetta":{"enabled":state.enable_rosetta,"binfmt":state.enable_rosetta},
            "mounts":[{"location":node.artifacts_root.canonicalize().map_err(io_error)?,"writable":true},{"location":archive_root.join("assets").canonicalize().map_err(io_error)?,"writable":false}],
            "networks":if state.network=="vzNAT" {json!([{"vzNAT":true}])} else {json!([{"lima":"user-v2"}])},
            "portForwards":[]
        });
        if node.role == NodeRole::ControlPlane {
            config["portForwards"] = json!([{"guestIP":"127.0.0.1","guestPort":6443,"hostIP":"127.0.0.1","hostPort":state.api_port}]);
        }
        // Never restore archived host commands, mounts, sockets, PIDs, SSH keys,
        // cloud-init media or absolute host paths. Lima regenerates these at up.
        write_json(&dir.join("lima.yaml"), &config)?;
    }
    // Publish the definition last. The operation lock excludes concurrent up,
    // create, destroy, scale and addon mutation until the transaction commits.
    let pending = root.join("cluster.state.partial");
    let mut definition = private_file(&pending)?;
    definition.write_all(&state.to_bytes()?).map_err(io_error)?;
    definition.sync_all().map_err(io_error)?;
    fs::hard_link(&pending, ClusterState::state_file(&root)).map_err(io_error)?;
    owned.0.clear();
    let _ = fs::remove_file(pending);
    let _ = fs::remove_file(root.join("archive-import-incomplete"));
    Ok(ImportOutcome {
        name: state.name,
        nodes: state.nodes.len(),
    })
}

/// Archived Node Ready conditions are historical. Require each kubelet to report
/// the boot ID of its newly started guest before declaring a restore ready.
pub(crate) fn wait_for_fresh_nodes(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &crate::cluster::node::Instance,
    nodes: &[crate::cluster::node::Instance],
    guest_kubeconfig: &Path,
) -> Result<()> {
    for node in nodes {
        let output = backend.guest_shell(
            runner,
            &node.name,
            &node.artifacts.root,
            "cat /proc/sys/kernel/random/boot_id",
        )?;
        let boot = output.stdout.trim();
        if boot.len() != 36
            || !boot
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        {
            return Err(invalid("cannot verify restored guest boot identity"));
        }
        let command = format!(
            "test \"$(KUBECONFIG={} /usr/local/bin/k3s kubectl --request-timeout=5s get node {} -o jsonpath='{{.status.nodeInfo.bootID}}')\" = {}",
            crate::shell::shell_escape(&guest_kubeconfig.display().to_string()),
            crate::shell::shell_escape(&node.name),
            crate::shell::shell_escape(boot)
        );
        crate::cluster::wait::wait_for_stable_guest_success(
            backend,
            runner,
            crate::cluster::wait::GuestWait {
                instance_name: &control_plane.name,
                runtime_root: &control_plane.artifacts.root,
                command: &command,
                timeout: std::time::Duration::from_secs(90),
                consecutive_successes: 1,
                interval: std::time::Duration::from_secs(1),
            },
        )?;
    }
    Ok(())
}

fn expected_payloads(state: &ClusterState, directory: &Path) -> Result<BTreeSet<String>> {
    let mut expected = BTreeSet::from(["base-image".into(), "assets/manifest.json".into()]);
    let spec = spec_for(state)?;
    let assets = crate::pull::load_from_root(&spec, &directory.join("assets"))?;
    expected.extend(assets.filenames().map(|file| format!("assets/{file}")));
    for (index, _) in state.nodes.iter().enumerate() {
        expected.insert(format!("nodes/{index}/disk"));
        if directory.join(format!("nodes/{index}/vz-efi")).is_file() {
            expected.insert(format!("nodes/{index}/vz-efi"));
        }
        let runtime = directory.join(format!("runtime/{index}"));
        if runtime.is_dir() {
            for file in fs::read_dir(runtime).map_err(io_error)? {
                let file = file.map_err(io_error)?;
                expected.insert(format!(
                    "runtime/{index}/{}",
                    file.file_name().to_string_lossy()
                ));
            }
        }
    }
    Ok(expected)
}
fn spec_for(state: &ClusterState) -> Result<ClusterSpec> {
    let mut spec = ClusterSpec::new(
        state.name.clone(),
        std::env::current_dir().map_err(io_error)?,
    );
    spec.backend = state.backend;
    spec.k3s_version = state.k3s_version.clone();
    spec.cni_plugin = state.cni_plugin;
    spec.cilium_version = state.cilium_version.clone();
    spec.addons = state.addons.clone();
    spec.component_toggles = state.component_toggles;
    Ok(spec)
}
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(invalid(
            "archive cluster name must be lowercase letters/digits/hyphens, at most 48 characters",
        ));
    }
    Ok(())
}
fn validate_state(state: &ClusterState) -> Result<()> {
    validate_name(&state.name)?;
    if state.backend != BackendKind::Lima
        || state.worker_count >= 64
        || state.nodes.len() != state.worker_count + 1
        || state.api_port == 0
        || !matches!(state.network.as_str(), "vzNAT" | "lima:user-v2")
    {
        return Err(invalid(
            "only complete native Lima/VZ clusters on vzNAT or lima:user-v2 can be archived",
        ));
    }
    if state.control_plane_resources.cpus == 0
        || state.control_plane_resources.memory_gib == 0
        || state.worker_resources.cpus == 0
        || state.worker_resources.memory_gib == 0
    {
        return Err(invalid("archive node resources must be positive"));
    }
    let mut actual = BTreeSet::new();
    for node in &state.nodes {
        if !actual.insert(node.name.clone()) {
            return Err(invalid("duplicate archive node identity"));
        }
    }
    let control_plane = format!("kalisto-{}-cp", state.name);
    let mut expected = BTreeSet::from([control_plane.clone()]);
    expected
        .extend((0..state.worker_count).map(|index| format!("kalisto-{}-w{index}", state.name)));
    if actual != expected
        || state
            .nodes
            .iter()
            .filter(|node| node.role == NodeRole::ControlPlane)
            .count()
            != 1
        || !state
            .nodes
            .iter()
            .any(|node| node.role == NodeRole::ControlPlane && node.name == control_plane)
    {
        return Err(invalid(
            "archive node identities do not match cluster definition",
        ));
    }
    Ok(())
}
fn inspect(runner: &dyn CommandRunner, name: &str) -> Result<serde_json::Value> {
    let output = runner.run("limactl", &["list".into(), "--json".into(), name.into()])?;
    serde_json::from_str(&output.stdout)
        .map_err(|error| invalid(format!("invalid Lima instance metadata: {error}")))
}
fn ensure_stopped(state: &ClusterState, runner: &dyn CommandRunner) -> Result<()> {
    for node in &state.nodes {
        if inspect(runner, &node.name)?["status"] != "Stopped" {
            return Err(invalid("node did not stop cleanly; export aborted"));
        }
    }
    Ok(())
}
fn instance_dir(info: &serde_json::Value, name: &str) -> Result<PathBuf> {
    let dir = lima_home()?.join(name).canonicalize().map_err(io_error)?;
    if let Some(reported) = info.get("dir").and_then(serde_json::Value::as_str)
        && Path::new(reported).canonicalize().map_err(io_error)? != dir
    {
        return Err(invalid("Lima instance directory mismatch"));
    }
    Ok(dir)
}
fn validate_instance(
    info: &serde_json::Value,
    state: &ClusterState,
    assets: &Path,
    node: &crate::cluster::state::NodeState,
) -> Result<()> {
    if info["config"]["vmType"] != "vz" {
        return Err(invalid("archive requires native VZ instances"));
    }
    for key in ["additionalDisks", "provision"] {
        if info["config"][key]
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
        {
            return Err(invalid(
                "custom provisioning/additional VM disks require separate backup and are not supported",
            ));
        }
    }
    let own_runtime = node.artifacts_root.canonicalize().map_err(io_error)?;
    if let Some(mounts) = info["config"]["mounts"].as_array() {
        for mount in mounts {
            let path = mount
                .get("location")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid("invalid Lima mount"))?;
            let path = Path::new(path).canonicalize().map_err(io_error)?;
            if path != own_runtime && path != assets {
                return Err(invalid(format!(
                    "cluster {} has external host mounts; export will not silently omit their data",
                    state.name
                )));
            }
        }
    }
    Ok(())
}
fn base_image(template: &str) -> Result<PathBuf> {
    crate::pull::validate_lima_template(template)?;
    let value: serde_yaml::Value = serde_yaml::from_slice(&fs::read(template).map_err(io_error)?)
        .map_err(|error| invalid(error.to_string()))?;
    let images = value
        .get("images")
        .and_then(serde_yaml::Value::as_sequence)
        .ok_or_else(|| invalid("node image missing"))?;
    if images.len() != 1 {
        return Err(invalid("archive requires one native base image"));
    }
    let path = images[0]
        .get("location")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| invalid("node image location missing"))?;
    Path::new(path.strip_prefix("file://").unwrap_or(path))
        .canonicalize()
        .map_err(io_error)
}
fn lima_home() -> Result<PathBuf> {
    let home = std::env::var_os("LIMA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".lima")))
        .ok_or_else(|| invalid("HOME or LIMA_HOME must be configured"))?;
    if home.is_absolute() {
        Ok(home)
    } else {
        std::env::current_dir()
            .map(|root| root.join(home))
            .map_err(io_error)
    }
}
fn require_macos() -> Result<()> {
    if std::env::consts::OS != "macos" {
        return Err(invalid(
            "cluster archives currently support macOS Lima/VZ only",
        ));
    }
    Ok(())
}
struct OwnedDestinations(Vec<PathBuf>);
impl Drop for OwnedDestinations {
    fn drop(&mut self) {
        for path in self.0.iter().rev() {
            let _ = fs::remove_dir_all(path);
        }
    }
}
fn reserve(path: &Path, owned: &mut OwnedDestinations) -> Result<()> {
    fs::create_dir(path).map_err(io_error)?;
    owned.0.push(path.to_path_buf());
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)
}
fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut file = private_file(path)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|error| invalid(error.to_string()))?)
        .map_err(io_error)?;
    file.sync_all().map_err(io_error)
}
fn copy_payload(source: &Path, destination: &Path) -> Result<()> {
    let mut input = fs::File::open(source).map_err(io_error)?;
    let mut output = private_file(destination)?;
    // The destination may be on another filesystem; preserve holes explicitly.
    let size = input.metadata().map_err(io_error)?.len();
    output.set_len(size).map_err(io_error)?;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            output.write_all(&buffer[..count]).map_err(io_error)?;
        } else {
            output
                .seek(SeekFrom::Current(count as i64))
                .map_err(io_error)?;
        }
    }
    output.sync_all().map_err(io_error)
}

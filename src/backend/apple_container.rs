//! Ordinary Apple containers; CLI and inspect schemas are pinned to 1.7.1.
//! See <https://github.com/apple/container/blob/1.7.1/docs/command-reference.md>
//! and `ContainerResource/Container/ManagedContainer.swift` in the same release.

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apple_container::runtime;
use crate::backend::runtime::{BackendConfig, BackendKind, ClusterBackend, InstanceConfig};
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};

const MANAGER_LABEL: &str = "io.kalisto.manager";
const CLUSTER_LABEL: &str = "io.kalisto.cluster";
const ROOT_LABEL: &str = "io.kalisto.runtime-root";
const WORKSPACE_LABEL: &str = "io.kalisto.workspace-root";
const METADATA_FILE: &str = "apple-container.json";

#[derive(Deserialize)]
struct ListedResource {
    id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct AppleContainerBackend;

pub const BACKEND: AppleContainerBackend = AppleContainerBackend;

#[derive(Debug, Deserialize, Serialize)]
struct NodeMetadata {
    name: String,
    runtime_root: PathBuf,
    network: String,
    guest_ip: String,
    host_port: Option<u16>,
}

pub(crate) struct NodeMetrics {
    pub pid: Option<u32>,
    pub guest_ip: Option<String>,
    pub rss_bytes: Option<u64>,
    pub vsz_bytes: Option<u64>,
    pub rootfs_bytes: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
}

impl ClusterBackend for AppleContainerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleContainer
    }

    fn check_available(&self, runner: &dyn CommandRunner) -> Result<()> {
        runtime::check_available(runner)
    }

    fn start_instance(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance: &InstanceConfig,
    ) -> Result<Output> {
        validate_config(config)?;
        let assets = config.boot_assets_root.as_ref().ok_or_else(|| {
            Error::InvalidArgs(
                "Apple container nodes require prepared boot assets; run `kalisto pull` first"
                    .into(),
            )
        })?;
        fs::create_dir_all(&instance.runtime_root)
            .map_err(io_error("create node runtime directory"))?;
        let root = absolute_directory(&instance.runtime_root)?;
        let assets_root = absolute_directory(&assets.root)?;
        if root == assets_root || root.starts_with(&assets_root) || assets_root.starts_with(&root) {
            return Err(Error::InvalidArgs(
                "Apple node runtime and boot asset directories must not overlap".into(),
            ));
        }
        let network = network_name(&instance.name)?;
        let network_info = inspect_network(runner, &network)?.ok_or_else(|| {
            Error::InvalidArgs(format!(
                "managed Apple network `{network}` is missing; recreate the cluster definition"
            ))
        })?;
        verify_network(&network_info, &network, config)?;
        let image = assets.apple_node_image()?;
        // Apple 1.7.1's create fetches on a local miss. Inspect before calling it,
        // including the arm64 config, so an absent platform cannot trigger a pull.
        check_local_image(runner, &image)?;
        let existing = inspect_node(runner, &instance.name)?;
        let mut already_running = false;
        if let Some(node) = &existing {
            verify_node(node, &instance.name, &root)?;
            verify_boot_configuration(
                node,
                &root,
                &assets_root,
                &image,
                instance.host_port_forward,
            )?;
            let current_state = state(node)?;
            if current_state != "running" && current_state != "stopped" {
                return Err(Error::InvalidArgs(format!(
                    "Apple node `{}` is in state `{current_state}`; refusing to restart it",
                    instance.name
                )));
            }
            already_running = current_state == "running";
        } else {
            create_node(runner, instance, &root, &assets_root, &network, &image)?;
        }
        let output = if already_running {
            Output {
                stdout: String::new(),
                stderr: String::new(),
            }
        } else {
            runtime::run(runner, &["start".into(), instance.name.clone()])?
        };
        let node = inspect_node(runner, &instance.name)?.ok_or_else(|| {
            Error::InvalidArgs(format!(
                "Apple node `{}` disappeared after start",
                instance.name
            ))
        })?;
        verify_node(&node, &instance.name, &root)?;
        if state(&node)? != "running" {
            return Err(Error::InvalidArgs(format!(
                "Apple node `{}` did not enter running state",
                instance.name
            )));
        }
        let metadata = NodeMetadata {
            name: instance.name.clone(),
            runtime_root: root.clone(),
            guest_ip: node_ip(&node, &network)?,
            network,
            host_port: instance.host_port_forward,
        };
        write_metadata(&root, &metadata)?;
        exec_root(
            runner,
            &instance.name,
            concat!(
                "n=0; until test -f /run/kalisto-node-ready && ",
                "test \"$(cat /run/kalisto-node-ready)\" = ",
                "\"$(cat /proc/sys/kernel/random/boot_id)\"; do ",
                "n=$((n+1)); if [ \"$n\" -ge 200 ]; then ",
                "echo 'Kalisto node initialization did not become ready' >&2; exit 1; ",
                "fi; sleep 0.1; done"
            ),
        )?;
        Ok(output)
    }

    fn stop_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        force: bool,
    ) -> Result<()> {
        let Some(node) = inspect_node(runner, instance_name)? else {
            return Ok(());
        };
        verify_node(&node, instance_name, &absolute_directory(runtime_root)?)?;
        match state(&node)? {
            "stopped" => return Ok(()),
            "running" | "stopping" => {}
            other => {
                return Err(Error::InvalidArgs(format!(
                    "Apple node `{instance_name}` has unknown state `{other}`"
                )));
            }
        }
        let mut args = vec!["stop".into()];
        if force {
            args.extend(["--time".into(), "0".into()]);
        }
        args.push(instance_name.into());
        runtime::run(runner, &args)?;
        Ok(())
    }

    fn delete_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
    ) -> Result<()> {
        let Some(node) = inspect_node(runner, instance_name)? else {
            return Ok(());
        };
        verify_node(&node, instance_name, &absolute_directory(runtime_root)?)?;
        runtime::run(
            runner,
            &["delete".into(), "--force".into(), instance_name.into()],
        )?;
        match fs::remove_file(runtime_root.join(METADATA_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove Apple node metadata")(error)),
        }
    }

    fn query_instance_status(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
    ) -> Result<String> {
        let Some(node) = inspect_node(runner, instance_name)? else {
            return Ok(format!("{instance_name} Stopped apple-container"));
        };
        verify_node(&node, instance_name, &absolute_directory(runtime_root)?)?;
        let display = match state(&node)? {
            "running" => "Running",
            "stopped" => "Stopped",
            "stopping" => "Stopping",
            "unknown" => "Unknown",
            other => {
                return Err(Error::InvalidArgs(format!(
                    "unrecognized Apple node state `{other}`"
                )));
            }
        };
        Ok(format!("{instance_name} {display} apple-container"))
    }

    fn guest_artifact_root(&self, _instance_name: &str, runtime_root: &Path) -> Result<PathBuf> {
        absolute_directory(runtime_root)
    }

    fn guest_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        let node = inspect_node(runner, instance_name)?.ok_or_else(|| {
            Error::InvalidArgs(format!("Apple node `{instance_name}` does not exist"))
        })?;
        verify_node(&node, instance_name, &absolute_directory(runtime_root)?)?;
        exec_root(runner, instance_name, command)
    }

    fn guest_sudo_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        self.guest_shell(runner, instance_name, runtime_root, command)
    }

    fn control_plane_host(&self, instance_name: &str, runtime_root: &Path) -> Result<String> {
        Ok(read_metadata(instance_name, runtime_root)?.guest_ip)
    }

    fn host_api_endpoint(
        &self,
        instance_name: &str,
        runtime_root: &Path,
        requested_port: u16,
    ) -> Result<String> {
        let metadata = read_metadata(instance_name, runtime_root)?;
        if metadata.host_port != Some(requested_port) {
            return Err(Error::InvalidArgs(format!(
                "Apple node `{instance_name}` does not publish requested API port {requested_port}; recreate it with the requested port"
            )));
        }
        Ok(format!("127.0.0.1:{requested_port}"))
    }

    fn prepare_cluster_network(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance_names: &[String],
    ) -> Result<()> {
        validate_config(config)?;
        let network = cluster_network(instance_names)?;
        if let Some(info) = inspect_network(runner, &network)? {
            return verify_network(&info, &network, config);
        }
        runtime::run(
            runner,
            &[
                "network".into(),
                "create".into(),
                "--label".into(),
                format!("{MANAGER_LABEL}=kalisto"),
                "--label".into(),
                format!("{CLUSTER_LABEL}={network}"),
                "--label".into(),
                format!(
                    "{WORKSPACE_LABEL}={}",
                    path_text(&absolute_directory(&config.workspace_root)?)?
                ),
                network.clone(),
            ],
        )?;
        let info = inspect_network(runner, &network)?.ok_or_else(|| {
            Error::InvalidArgs(format!(
                "Apple network `{network}` disappeared after creation"
            ))
        })?;
        verify_network(&info, &network, config)
    }

    fn cleanup_cluster_network(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance_names: &[String],
    ) -> Result<()> {
        validate_config(config)?;
        let network = cluster_network(instance_names)?;
        let Some(info) = inspect_network(runner, &network)? else {
            return Ok(());
        };
        verify_network(&info, &network, config)?;
        // Apple refuses removal while any (even stopped) container references it.
        runtime::run(runner, &["network".into(), "delete".into(), network])?;
        Ok(())
    }
}

fn create_node(
    runner: &dyn CommandRunner,
    instance: &InstanceConfig,
    root: &Path,
    assets_root: &Path,
    network: &str,
    image: &str,
) -> Result<()> {
    let mut args = vec![
        "create".into(),
        "--name".into(),
        instance.name.clone(),
        "--platform".into(),
        "linux/arm64".into(),
        "--cpus".into(),
        instance.cpus.to_string(),
        "--memory".into(),
        format!("{}G", instance.memory_gib),
        "--uid".into(),
        "0".into(),
        "--gid".into(),
        "0".into(),
        "--cap-add".into(),
        "ALL".into(),
        "--masked-path".into(),
        "NONE".into(),
        "--read-only-path".into(),
        "NONE".into(),
        "--kernel".into(),
        path_text(&runtime::kernel_path()?)?.to_owned(),
        "--init-image".into(),
        runtime::init_image()?,
        "--network".into(),
        network.into(),
        "--label".into(),
        format!("{MANAGER_LABEL}=kalisto"),
        "--label".into(),
        format!("{CLUSTER_LABEL}={network}"),
        "--label".into(),
        format!("{ROOT_LABEL}={}", path_text(root)?),
        "--mount".into(),
        bind_mount(assets_root, true)?,
        "--mount".into(),
        bind_mount(root, false)?,
    ];
    if let Some(port) = instance.host_port_forward {
        if port == 0 {
            return Err(Error::InvalidArgs(
                "Apple API host port must not be zero".into(),
            ));
        }
        args.extend(["--publish".into(), format!("127.0.0.1:{port}:6443/tcp")]);
    }
    // The image owns PID 1 (tini) and initializes shared mounts/cgroup2/BPF.
    args.push(image.into());
    runtime::run(runner, &args)?;
    Ok(())
}

fn validate_config(config: &BackendConfig) -> Result<()> {
    if config.network != "apple-container" {
        return Err(Error::InvalidArgs("Apple container backend requires --network apple-container (a private, Kalisto-managed cluster network)".into()));
    }
    if config.enable_rosetta
        || config.firecracker_kernel.is_some()
        || config.firecracker_rootfs.is_some()
    {
        return Err(Error::InvalidArgs(
            "Apple container backend does not support Rosetta or Firecracker boot overrides".into(),
        ));
    }
    Ok(())
}

fn network_name(instance_name: &str) -> Result<String> {
    let cluster = instance_name
        .strip_suffix("-cp")
        .or_else(|| {
            let (prefix, suffix) = instance_name.rsplit_once("-w")?;
            (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
                .then_some(prefix)
        })
        .ok_or_else(|| {
            Error::InvalidArgs(format!("invalid Kalisto node name `{instance_name}`"))
        })?;
    if !cluster.starts_with("kalisto-")
        || cluster.len() > 63
        || !cluster.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(Error::InvalidArgs(format!(
            "invalid Apple cluster network name `{cluster}`"
        )));
    }
    Ok(cluster.into())
}

fn cluster_network(names: &[String]) -> Result<String> {
    let first = names.first().ok_or_else(|| {
        Error::InvalidArgs("Apple cluster network requires at least one node".into())
    })?;
    let network = network_name(first)?;
    for name in &names[1..] {
        if network_name(name)? != network {
            return Err(Error::InvalidArgs(
                "Apple cluster nodes must belong to the same cluster".into(),
            ));
        }
    }
    Ok(network)
}

// Use a successful list as the only evidence of absence. In particular an XPC,
// service, permission or malformed inspect failure must never mean "not found".
fn resource_exists(runner: &dyn CommandRunner, name: &str, network: bool) -> Result<bool> {
    let args = if network {
        vec![
            "network".into(),
            "list".into(),
            "--format".into(),
            "json".into(),
        ]
    } else {
        vec![
            "list".into(),
            "--all".into(),
            "--format".into(),
            "json".into(),
        ]
    };
    let output = runtime::run(runner, &args)?;
    let values: Vec<ListedResource> = serde_json::from_str(&output.stdout).map_err(|error| {
        Error::InvalidArgs(format!("invalid Apple resource list JSON: {error}"))
    })?;
    Ok(values.iter().any(|value| value.id == name))
}

fn inspect_node(runner: &dyn CommandRunner, name: &str) -> Result<Option<Value>> {
    if !resource_exists(runner, name, false)? {
        return Ok(None);
    }
    let output = runtime::run(runner, &["inspect".into(), name.into()])?;
    Ok(Some(single_resource(&output.stdout, name)?))
}

fn inspect_network(runner: &dyn CommandRunner, name: &str) -> Result<Option<Value>> {
    if !resource_exists(runner, name, true)? {
        return Ok(None);
    }
    let output = runtime::run(runner, &["network".into(), "inspect".into(), name.into()])?;
    Ok(Some(single_resource(&output.stdout, name)?))
}

fn json_array(text: &str, context: &str) -> Result<Vec<Value>> {
    serde_json::from_str(text)
        .map_err(|error| Error::InvalidArgs(format!("invalid {context} JSON: {error}")))
}

fn single_resource(text: &str, name: &str) -> Result<Value> {
    let mut values = json_array(text, "Apple inspect")?;
    if values.len() != 1 || values[0].get("id").and_then(Value::as_str) != Some(name) {
        return Err(Error::InvalidArgs(format!(
            "Apple inspect did not return exactly resource `{name}`"
        )));
    }
    Ok(values.remove(0))
}

fn string_field<'a>(value: &'a Value, pointer: &str, field: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgs(format!("Apple inspect is missing {field}")))
}

fn verify_label(value: &Value, key: &str, expected: &str, resource: &str) -> Result<()> {
    if value
        .pointer("/configuration/labels")
        .and_then(|labels| labels.get(key))
        .and_then(Value::as_str)
        != Some(expected)
    {
        return Err(Error::InvalidArgs(format!(
            "refusing to use or modify Apple resource `{resource}`: ownership label `{key}` does not match this Kalisto cluster"
        )));
    }
    Ok(())
}

fn verify_node(value: &Value, name: &str, root: &Path) -> Result<()> {
    if value.get("id").and_then(Value::as_str) != Some(name)
        || value.pointer("/configuration/id").and_then(Value::as_str) != Some(name)
    {
        return Err(Error::InvalidArgs(format!(
            "Apple inspect returned the wrong node for `{name}`"
        )));
    }
    verify_label(value, MANAGER_LABEL, "kalisto", name)?;
    verify_label(value, CLUSTER_LABEL, &network_name(name)?, name)?;
    verify_label(value, ROOT_LABEL, path_text(root)?, name)
}

fn verify_network(value: &Value, name: &str, config: &BackendConfig) -> Result<()> {
    verify_label(value, MANAGER_LABEL, "kalisto", name)?;
    verify_label(value, CLUSTER_LABEL, name, name)?;
    verify_label(
        value,
        WORKSPACE_LABEL,
        path_text(&absolute_directory(&config.workspace_root)?)?,
        name,
    )?;
    if value.pointer("/configuration/name").and_then(Value::as_str) != Some(name)
        || value.pointer("/configuration/mode").and_then(Value::as_str) != Some("nat")
    {
        return Err(Error::InvalidArgs(format!(
            "Apple network `{name}` does not match the managed NAT configuration"
        )));
    }
    Ok(())
}

fn state(node: &Value) -> Result<&str> {
    // ManagedContainer in 1.7.1 wraps RuntimeStatus under status.state.
    string_field(node, "/status/state", "node runtime state")
}

fn node_ip(node: &Value, network: &str) -> Result<String> {
    let networks = node
        .pointer("/status/networks")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::InvalidArgs("Apple inspect is missing node network attachments".into())
        })?;
    let attachment = networks
        .iter()
        .find(|attachment| attachment.get("network").and_then(Value::as_str) == Some(network))
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "Apple node is not attached to managed network `{network}`"
            ))
        })?;
    let cidr = string_field(attachment, "/ipv4Address", "node IPv4 address")?;
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| Error::InvalidArgs(format!("invalid Apple node CIDR `{cidr}`")))?;
    let ip = address.parse::<Ipv4Addr>().map_err(|error| {
        Error::InvalidArgs(format!("invalid Apple node IP `{address}`: {error}"))
    })?;
    if !prefix.parse::<u8>().is_ok_and(|prefix| prefix <= 32)
        || ip.is_unspecified()
        || ip.is_loopback()
    {
        return Err(Error::InvalidArgs(format!(
            "unusable Apple node CIDR `{cidr}`"
        )));
    }
    Ok(ip.to_string())
}

fn verify_boot_configuration(
    node: &Value,
    root: &Path,
    assets: &Path,
    image: &str,
    host_port: Option<u16>,
) -> Result<()> {
    let configuration = &node["configuration"];
    if configuration
        .pointer("/image/reference")
        .and_then(Value::as_str)
        != Some(image)
    {
        return Err(Error::InvalidArgs("existing Apple node uses different prepared image; recreate the cluster to change boot assets".into()));
    }
    let network = network_name(string_field(node, "/id", "node ID")?)?;
    let attachments = configuration
        .get("networks")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgs("Apple node is missing network configuration".into()))?;
    if attachments.len() != 1
        || attachments[0].get("network").and_then(Value::as_str) != Some(network.as_str())
    {
        return Err(Error::InvalidArgs(
            "existing Apple node is not exclusively attached to its managed cluster network".into(),
        ));
    }
    let mounts = configuration
        .get("mounts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgs("Apple node is missing its configured mounts".into()))?;
    if mounts.len() != 2
        || !mounts.iter().any(|mount| mount_matches(mount, root, false))
        || !mounts
            .iter()
            .any(|mount| mount_matches(mount, assets, true))
        || configuration.get("ssh").and_then(Value::as_bool) != Some(false)
    {
        return Err(Error::InvalidArgs("existing Apple node does not have the exact Kalisto-only mount configuration; recreate the cluster".into()));
    }
    let ports = configuration
        .get("publishedPorts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::InvalidArgs("Apple node is missing published port configuration".into())
        })?;
    let valid = match host_port {
        None => ports.is_empty(),
        Some(port) => {
            ports.len() == 1
                && ports[0].get("hostAddress").and_then(Value::as_str) == Some("127.0.0.1")
                && ports[0].get("hostPort").and_then(Value::as_u64) == Some(u64::from(port))
                && ports[0].get("containerPort").and_then(Value::as_u64) == Some(6443)
                && ports[0].get("proto").and_then(Value::as_str) == Some("tcp")
                && ports[0].get("count").and_then(Value::as_u64).unwrap_or(1) == 1
        }
    };
    if !valid {
        return Err(Error::InvalidArgs("existing Apple node API publication differs from the requested localhost port; recreate the cluster".into()));
    }
    Ok(())
}

fn mount_matches(mount: &Value, path: &Path, readonly: bool) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    mount.get("source").and_then(Value::as_str) == Some(path)
        && mount.get("destination").and_then(Value::as_str) == Some(path)
        && mount
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options.iter().any(|option| option.as_str() == Some("ro")) == readonly
            })
}

fn check_local_image(runner: &dyn CommandRunner, image: &str) -> Result<()> {
    let digest = image
        .rsplit_once("@sha256:")
        .map(|(_, digest)| digest)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "prepared Apple node image `{image}` must be pinned to a SHA-256 manifest digest"
            ))
        })?;
    let expected_digest = format!("sha256:{digest}");
    let output = runtime::run(runner, &["image".into(), "inspect".into(), image.into()])?;
    let values = json_array(&output.stdout, "Apple image inspect")?;
    let valid = values.len() == 1
        && values[0]
            .get("variants")
            .and_then(Value::as_array)
            .is_some_and(|variants| {
                variants.iter().any(|variant| {
                    variant.pointer("/platform/os").and_then(Value::as_str) == Some("linux")
                        && variant
                            .pointer("/platform/architecture")
                            .and_then(Value::as_str)
                            == Some("arm64")
                        && variant.get("digest").and_then(Value::as_str)
                            == Some(expected_digest.as_str())
                        && variant.get("config").is_some_and(Value::is_object)
                })
            });
    if !valid {
        return Err(Error::InvalidArgs(format!(
            "prepared Apple image `{image}` has no matching complete local linux/arm64 configuration; run `kalisto pull`"
        )));
    }
    Ok(())
}

fn exec_root(runner: &dyn CommandRunner, name: &str, command: &str) -> Result<Output> {
    runtime::run(
        runner,
        &[
            "exec".into(),
            "--uid".into(),
            "0".into(),
            "--gid".into(),
            "0".into(),
            name.into(),
            "/bin/sh".into(),
            "-c".into(),
            command.into(),
        ],
    )
}

fn absolute_directory(path: &Path) -> Result<PathBuf> {
    let absolute = path
        .canonicalize()
        .map_err(io_error("resolve Apple node directory"))?;
    if !absolute.is_dir() {
        return Err(Error::InvalidArgs(format!(
            "Apple node mount `{}` is not a directory",
            absolute.display()
        )));
    }
    Ok(absolute)
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::InvalidArgs(format!(
            "Apple runtime requires a UTF-8 path: {}",
            path.display()
        ))
    })
}

fn bind_mount(path: &Path, readonly: bool) -> Result<String> {
    let text = path_text(path)?;
    if text.contains(',') || text.contains('\n') || text.contains('\r') {
        return Err(Error::InvalidArgs(format!(
            "Apple mount path cannot contain commas or line breaks: {text}"
        )));
    }
    Ok(format!(
        "type=bind,source={text},target={text}{}",
        if readonly { ",readonly" } else { "" }
    ))
}

fn write_metadata(root: &Path, metadata: &NodeMetadata) -> Result<()> {
    let contents = serde_json::to_vec(metadata).map_err(|error| {
        Error::InvalidArgs(format!("cannot serialize Apple node metadata: {error}"))
    })?;
    let temporary = root.join("apple-container.json.tmp");
    fs::write(&temporary, contents).map_err(io_error("write Apple node metadata"))?;
    fs::rename(&temporary, root.join(METADATA_FILE))
        .map_err(io_error("persist Apple node metadata"))
}

fn read_metadata(name: &str, runtime_root: &Path) -> Result<NodeMetadata> {
    let root = absolute_directory(runtime_root)?;
    let contents =
        fs::read(root.join(METADATA_FILE)).map_err(io_error("read Apple node metadata"))?;
    let metadata: NodeMetadata = serde_json::from_slice(&contents)
        .map_err(|error| Error::InvalidArgs(format!("invalid Apple node metadata: {error}")))?;
    if metadata.name != name
        || metadata.runtime_root != root
        || metadata.network != network_name(name)?
        || metadata.guest_ip.parse::<Ipv4Addr>().is_err()
    {
        return Err(Error::InvalidArgs(format!(
            "Apple node metadata does not belong to `{name}` at {}",
            root.display()
        )));
    }
    Ok(metadata)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::CommandIo {
        program: operation.into(),
        source,
    }
}

pub(crate) fn metrics(
    runner: &dyn CommandRunner,
    name: &str,
    runtime_root: &Path,
) -> Result<NodeMetrics> {
    let Some(node) = inspect_node(runner, name)? else {
        return Ok(NodeMetrics {
            pid: None,
            guest_ip: None,
            rss_bytes: None,
            vsz_bytes: None,
            rootfs_bytes: None,
            rx_bytes: None,
            tx_bytes: None,
        });
    };
    verify_node(&node, name, &absolute_directory(runtime_root)?)?;
    let running = state(&node)? == "running";
    let guest_ip = if running {
        Some(node_ip(&node, &network_name(name)?)?)
    } else {
        None
    };
    let mut result = NodeMetrics {
        pid: None,
        guest_ip,
        rss_bytes: None,
        vsz_bytes: None,
        rootfs_bytes: None,
        rx_bytes: None,
        tx_bytes: None,
    };
    if running {
        let output = runtime::run(
            runner,
            &[
                "stats".into(),
                "--no-stream".into(),
                "--format".into(),
                "json".into(),
                name.into(),
            ],
        )?;
        let sample = single_resource(&output.stdout, name)?;
        result.rss_bytes = sample.get("memoryUsageBytes").and_then(Value::as_u64);
        result.rx_bytes = sample.get("networkRxBytes").and_then(Value::as_u64);
        result.tx_bytes = sample.get("networkTxBytes").and_then(Value::as_u64);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    struct Runner(RefCell<VecDeque<Result<Output>>>);

    impl CommandRunner for Runner {
        fn run(&self, _program: &str, _args: &[String]) -> Result<Output> {
            self.0.borrow_mut().pop_front().expect("unexpected command")
        }
    }

    fn runner(responses: Vec<Result<Output>>) -> Runner {
        Runner(RefCell::new(responses.into()))
    }

    fn output(value: Value) -> Result<Output> {
        Ok(Output {
            stdout: value.to_string(),
            stderr: String::new(),
        })
    }

    fn failed() -> Error {
        Error::CommandFailed {
            program: "container".into(),
            args: vec!["inspect".into()],
            code: Some(1),
            stdout: String::new(),
            stderr: "XPC service unavailable".into(),
        }
    }

    fn owned_node() -> Value {
        json!({
            "id": "kalisto-demo-cp",
            "configuration": {
                "id": "kalisto-demo-cp",
                "labels": {
                    "io.kalisto.manager": "kalisto",
                    "io.kalisto.cluster": "kalisto-demo",
                    "io.kalisto.runtime-root": "/exact/node"
                }
            },
            "status": {
                "state": "running",
                "networks": [{"network": "kalisto-demo", "ipv4Address": "192.168.64.3/24"}]
            }
        })
    }

    #[test]
    fn successful_empty_list_is_missing_without_inspect() {
        let runner = runner(vec![output(json!([]))]);
        assert!(inspect_node(&runner, "kalisto-demo-cp").unwrap().is_none());
    }

    #[test]
    fn failed_inspect_is_not_treated_as_missing() {
        let runner = runner(vec![
            output(json!([{"id": "kalisto-demo-cp"}])),
            Err(failed()),
        ]);
        assert!(matches!(
            inspect_node(&runner, "kalisto-demo-cp"),
            Err(Error::CommandFailed { .. })
        ));
    }

    #[test]
    fn malformed_list_is_not_treated_as_missing() {
        let runner = runner(vec![output(json!([{"name": "unrelated"}]))]);
        assert!(inspect_node(&runner, "kalisto-demo-cp").is_err());
    }

    #[test]
    fn same_named_unowned_container_cannot_be_deleted() {
        let mut node = owned_node();
        node["configuration"]["labels"] = json!({});
        let runner = runner(vec![
            output(json!([{"id": "kalisto-demo-cp"}])),
            output(json!([node])),
        ]);
        assert!(
            BACKEND
                .delete_instance(&runner, "kalisto-demo-cp", Path::new("/"))
                .is_err()
        );
    }

    #[test]
    fn another_runtime_directory_cannot_adopt_owned_node() {
        assert!(
            verify_node(
                &owned_node(),
                "kalisto-demo-cp",
                Path::new("/different/node")
            )
            .is_err()
        );
    }

    #[test]
    fn reads_current_managed_container_status_and_cidr() {
        assert_eq!(
            node_ip(&owned_node(), "kalisto-demo").unwrap(),
            "192.168.64.3"
        );
    }

    #[test]
    fn worker_scaling_keeps_the_control_plane_network() {
        assert_eq!(
            network_name("kalisto-demo-w42").unwrap(),
            network_name("kalisto-demo-cp").unwrap()
        );
    }

    #[test]
    fn nested_mounts_do_not_substitute_for_exact_asset_share() {
        let mount = json!({"source": "/exact", "destination": "/exact", "options": ["ro"]});
        assert!(!mount_matches(&mount, Path::new("/exact/assets"), true));
    }

    #[test]
    fn wrong_manifest_cannot_trigger_image_fetch_during_start() {
        let image = format!("docker.io/kalisto/node@sha256:{}", "a".repeat(64));
        let runner = runner(vec![output(json!([{
            "id": "indirect-index",
            "variants": [{
                "platform": {"os": "linux", "architecture": "arm64"},
                "digest": format!("sha256:{}", "b".repeat(64)),
                "config": {}
            }]
        }]))]);
        assert!(check_local_image(&runner, &image).is_err());
    }

    #[test]
    fn metrics_use_actual_memory_and_network_samples() {
        let mut node = owned_node();
        node["configuration"]["labels"][ROOT_LABEL] = json!("/");
        let runner = runner(vec![
            output(json!([{"id": "kalisto-demo-cp"}])),
            output(json!([node])),
            output(json!([{
                "id": "kalisto-demo-cp",
                "memoryUsageBytes": 123_456,
                "networkRxBytes": 222,
                "networkTxBytes": 333
            }])),
        ]);
        let result = metrics(&runner, "kalisto-demo-cp", Path::new("/")).unwrap();
        assert_eq!(
            (result.rss_bytes, result.rx_bytes, result.tx_bytes),
            (Some(123_456), Some(222), Some(333))
        );
    }
}

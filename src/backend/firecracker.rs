use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::debug;

use crate::backend::runtime::{BackendConfig, BackendKind, ClusterBackend, InstanceConfig};
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};
use crate::telemetry::raw_streams_enabled;

const BOOT_ARGS: &str =
    "console=ttyS0 reboot=k panic=1 ds=nocloud;s=file:///var/lib/cloud/seed/nocloud/";
const GUEST_ARTIFACT_PARENT: &str = "/var/lib/kalisto";
const GUEST_NOCLOUD_SEED_DIR: &str = "/var/lib/cloud/seed/nocloud";
const SSH_USER: &str = "ubuntu";
const BRIDGE_NAME: &str = "kalistofc0";
const BRIDGE_HOST_IP: &str = "172.31.0.1";
const BRIDGE_CIDR_BITS: u8 = 16;
const ROOTLESS_SLIRP_TAP_NAME: &str = "kfcslip0";
const ROOTLESS_SLIRP_GATEWAY_IP: &str = "10.0.2.2";
const ROOTLESS_SLIRP_CIDR_BITS: u8 = 24;
const ROOTLESS_NETWORK_MTU: u16 = 1500;
const ROOTLESS_SSH_HOST: &str = "127.0.0.1";
const ROOTLESS_NETWORK_DIR: &str = "firecracker-rootless-net";
const ROOTLESS_SLIRP_SOCKET_FILE: &str = "slirp4netns.sock";
const ROOTLESS_SLIRP_SOCKET_PATH_FILE: &str = "slirp4netns.socket-path";
const ROOTLESS_SLIRP_SHORT_SOCKET_DIR: &str = "/tmp/kalisto-slirp4netns";
const UNIX_SOCKET_PATH_MAX: usize = 107;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirecrackerNetworkMode {
    Bridge,
    RootlessSlirp,
}

impl FirecrackerNetworkMode {
    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "" | "bridge" => Ok(Self::Bridge),
            "slirp" | "slirp4netns" => Ok(Self::RootlessSlirp),
            other => Err(Error::InvalidArgs(format!(
                "unsupported firecracker network mode `{other}`; expected one of: bridge, slirp4netns"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::RootlessSlirp => "slirp4netns",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FirecrackerBackend;

pub const BACKEND: FirecrackerBackend = FirecrackerBackend;

impl ClusterBackend for FirecrackerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Firecracker
    }

    fn check_available(&self, runner: &dyn CommandRunner) -> Result<()> {
        if !cfg!(target_os = "linux") {
            return Err(Error::InvalidArgs(
                "firecracker backend requires a Linux host".to_owned(),
            ));
        }

        ensure_path_exists(Path::new("/dev/kvm"), "/dev/kvm")?;
        runner.run("firecracker", &["--version".into()])?;
        runner.run("debugfs", &["-V".into()])?;
        runner.run("ip", &["-V".into()])?;
        runner.run("iptables", &["--version".into()])?;
        runner.run("sysctl", &["--version".into()])?;
        let _ = runner.run("ssh", &["-V".into()]);
        let _ = runner.run("ssh-keygen", &["-V".into()]);
        Ok(())
    }

    fn start_instance(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance: &InstanceConfig,
    ) -> Result<Output> {
        let kernel = config
            .firecracker_kernel
            .as_deref()
            .ok_or_else(|| Error::InvalidArgs(missing_artifact_message("kernel")))?;
        let rootfs = config
            .firecracker_rootfs
            .as_deref()
            .ok_or_else(|| Error::InvalidArgs(missing_artifact_message("rootfs")))?;

        ensure_path_exists(kernel, "firecracker kernel")?;
        ensure_path_exists(rootfs, "firecracker rootfs")?;
        let assets_disk = config
            .boot_assets_root
            .as_ref()
            .map(|assets| assets.root.join(crate::firecracker_assets::DISK_FILENAME));
        if let Some(disk) = &assets_disk {
            ensure_path_exists(disk, "firecracker prepared assets disk")?;
        }
        fs::create_dir_all(&instance.runtime_root).map_err(io_error("create_dir_all"))?;

        let runtime = RuntimeFiles::new(&instance.runtime_root);
        let network_mode = FirecrackerNetworkMode::parse(&config.network)?;
        ensure_firecracker_network_tools(runner, network_mode)?;
        let net = match network_mode {
            FirecrackerNetworkMode::Bridge => {
                let net = NetworkConfig::for_bridge_instance(&instance.name);
                ensure_prepared_network(&net)?;
                net
            }
            FirecrackerNetworkMode::RootlessSlirp => {
                prepare_rootless_network(runner, instance, &runtime)?
            }
        };
        ensure_ssh_key(runner, &runtime.ssh_key)?;
        write_cloud_init_files(&runtime, &instance.name, &net)?;
        prepare_instance_rootfs(runner, rootfs, &runtime)?;
        write_file(
            &runtime.config,
            &build_config_json(kernel, instance, &runtime, &net, assets_disk.as_deref()),
        )?;

        let stdout_file = fs::File::create(&runtime.stdout_log).map_err(io_error("create"))?;
        let stderr_file = fs::File::create(&runtime.stderr_log).map_err(io_error("create"))?;

        debug!(
            instance = %instance.name,
            runtime_root = %instance.runtime_root.display(),
            kernel = %kernel.display(),
            rootfs = %runtime.instance_rootfs.display(),
            tap = %net.tap_name,
            guest_ip = %net.guest_ip,
            config = %runtime.config.display(),
            stdout_log = %runtime.stdout_log.display(),
            stderr_log = %runtime.stderr_log.display(),
            meta_data = %runtime.meta_data.display(),
            user_data = %runtime.user_data.display(),
            network_config = %runtime.network_config.display(),
            nocloud_seed_dir = GUEST_NOCLOUD_SEED_DIR,
            "starting firecracker microvm"
        );

        let mut child = firecracker_command(instance, &runtime, &net);
        child.stdout(Stdio::from(stdout_file));
        child.stderr(Stdio::from(stderr_file));

        let mut child = spawn_background_process(&mut child, "firecracker")?;

        write_file(&runtime.pid, &format!("{}\n", child.id()))?;
        write_file(&runtime.guest_ip, &format!("{}\n", net.guest_ip))?;
        write_file(&runtime.host_ip, &format!("{}\n", net.host_ip))?;
        write_file(&runtime.tap_name, &format!("{}\n", net.tap_name))?;

        let log_streamer = VmLogStreamer::start_if_enabled(&runtime, config.stream_vm_logs);
        let wait_result = wait_for_ssh(runner, &runtime, &net, &mut child).and_then(|()| {
            if let Some(assets) = &config.boot_assets_root {
                mount_prepared_assets(runner, &runtime, &net, &assets.root)?;
            }
            Ok(())
        });
        if let Some(log_streamer) = log_streamer {
            log_streamer.stop();
        }
        if let Err(error) = wait_result {
            let _ = stop_process(runner, &runtime, &net, true);
            return Err(error);
        }

        Ok(Output {
            stdout: format!(
                "firecracker started: pid={} guest_ip={} config={}",
                child.id(),
                net.guest_ip,
                runtime.config.display()
            ),
            stderr: String::new(),
        })
    }

    fn stop_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        force: bool,
    ) -> Result<()> {
        let runtime = RuntimeFiles::new(runtime_root);
        let net = load_network_config(&runtime, instance_name)?;
        stop_process(runner, &runtime, &net, force)
    }

    fn delete_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
    ) -> Result<()> {
        self.stop_instance(runner, instance_name, runtime_root, true)
    }

    fn query_instance_status(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
    ) -> Result<String> {
        let runtime = RuntimeFiles::new(runtime_root);
        let pid = read_pid(&runtime.pid)?;
        let net = load_network_config(&runtime, instance_name)?;

        let Some(pid) = pid else {
            return Ok(format!("{instance_name} Stopped firecracker"));
        };

        if process_exists(runner, pid) {
            return Ok(format!(
                "{instance_name} Running firecracker pid={pid} guest_ip={}",
                net.guest_ip
            ));
        }

        if run_ssh_command(runner, &runtime.ssh_key, &net, "true").is_ok() {
            Ok(format!(
                "{instance_name} Running firecracker guest_ip={}",
                net.guest_ip
            ))
        } else {
            Ok(format!("{instance_name} Exited firecracker pid={pid}"))
        }
    }

    fn guest_artifact_root(&self, instance_name: &str, _runtime_root: &Path) -> Result<PathBuf> {
        Ok(Path::new(GUEST_ARTIFACT_PARENT).join(instance_name))
    }

    fn guest_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        run_ssh(runner, runtime_root, instance_name, command)
    }

    fn guest_sudo_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        run_ssh(
            runner,
            runtime_root,
            instance_name,
            &format!("sudo sh -lc {}", shell_quote(command)),
        )
    }

    fn copy_file_to_guest(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &Path,
        source: &Path,
        destination: &Path,
    ) -> Result<()> {
        let runtime = RuntimeFiles::new(runtime_root);
        let net = load_network_config(&runtime, instance_name)?;
        let source = source
            .canonicalize()
            .map_err(io_error("resolve transfer source"))?;
        let temporary = run_ssh_command(
            runner,
            &runtime.ssh_key,
            &net,
            "umask 077; mktemp /tmp/kalisto-transfer.XXXXXXXXXX",
        )?
        .stdout
        .trim()
        .to_owned();
        if !temporary.starts_with("/tmp/kalisto-transfer.")
            || temporary.contains(char::is_whitespace)
        {
            return Err(Error::InvalidArgs(
                "invalid guest transfer temporary path".to_owned(),
            ));
        }
        let result: Result<()> = (|| {
            let (mut args, host) = ssh_connection_args(&runtime.ssh_key, &net, "-P")?;
            args.push("-O".into());
            args.push("--".into());
            args.push(source.display().to_string());
            args.push(format!("{SSH_USER}@{host}:{}", shell_quote(&temporary)));
            runner.run("scp", &args)?;
            self.guest_sudo_shell(
                runner,
                instance_name,
                runtime_root,
                &format!(
                    "install -D -m 0600 -- {} {}",
                    shell_quote(&temporary),
                    shell_quote(&destination.to_string_lossy())
                ),
            )?;
            Ok(())
        })();
        let cleanup = run_ssh_command(
            runner,
            &runtime.ssh_key,
            &net,
            &format!("rm -f -- {}", shell_quote(&temporary)),
        );
        result?;
        cleanup?;
        Ok(())
    }

    fn control_plane_host(&self, instance_name: &str, runtime_root: &Path) -> Result<String> {
        Ok(load_network_config(&RuntimeFiles::new(runtime_root), instance_name)?.guest_ip)
    }

    fn host_api_endpoint(
        &self,
        instance_name: &str,
        runtime_root: &Path,
        requested_port: u16,
    ) -> Result<String> {
        let net = load_network_config(&RuntimeFiles::new(runtime_root), instance_name)?;
        Ok(match net.mode {
            FirecrackerNetworkMode::Bridge => format!("{}:6443", net.guest_ip),
            FirecrackerNetworkMode::RootlessSlirp => format!(
                "{ROOTLESS_SSH_HOST}:{}",
                net.api_port_forward.unwrap_or(requested_port)
            ),
        })
    }

    fn prepare_cluster_network(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance_names: &[String],
    ) -> Result<()> {
        if FirecrackerNetworkMode::parse(&config.network)? == FirecrackerNetworkMode::RootlessSlirp
        {
            return Ok(());
        }
        let bridge_net = instance_names.first().map_or_else(
            || NetworkConfig::for_bridge_instance("kalisto"),
            |name| NetworkConfig::for_bridge_instance(name),
        );
        setup_bridge(runner, &bridge_net)?;
        for instance_name in instance_names {
            setup_tap(runner, &NetworkConfig::for_bridge_instance(instance_name))?;
        }
        Ok(())
    }

    fn cleanup_cluster_network(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance_names: &[String],
    ) -> Result<()> {
        if FirecrackerNetworkMode::parse(&config.network)? == FirecrackerNetworkMode::RootlessSlirp
        {
            return Ok(());
        }
        for instance_name in instance_names {
            let net = NetworkConfig::for_bridge_instance(instance_name);
            let _ = runner.run("ip", &["link".into(), "del".into(), net.tap_name]);
        }

        let bridge_net = instance_names.first().map_or_else(
            || NetworkConfig::for_bridge_instance("kalisto"),
            |name| NetworkConfig::for_bridge_instance(name),
        );
        if runner
            .run(
                "ip",
                &[
                    "link".into(),
                    "show".into(),
                    "dev".into(),
                    bridge_net.bridge_name.clone(),
                ],
            )
            .is_err()
        {
            return Ok(());
        }

        if let Ok(output) = runner.run(
            "ip",
            &[
                "-o".into(),
                "link".into(),
                "show".into(),
                "master".into(),
                bridge_net.bridge_name.clone(),
            ],
        ) && output.stdout.trim().is_empty()
        {
            let _ = teardown_host_egress(runner, &bridge_net);
            let _ = runner.run(
                "ip",
                &[
                    "link".into(),
                    "set".into(),
                    "dev".into(),
                    bridge_net.bridge_name.clone(),
                    "down".into(),
                ],
            );
            let _ = runner.run(
                "ip",
                &[
                    "link".into(),
                    "del".into(),
                    "dev".into(),
                    bridge_net.bridge_name,
                    "type".into(),
                    "bridge".into(),
                ],
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RuntimeFiles {
    config: PathBuf,
    pid: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    user_data: PathBuf,
    meta_data: PathBuf,
    network_config: PathBuf,
    instance_rootfs: PathBuf,
    ssh_key: PathBuf,
    guest_ip: PathBuf,
    host_ip: PathBuf,
    tap_name: PathBuf,
    network_mode: PathBuf,
    ssh_port: PathBuf,
    api_port_forward: PathBuf,
}

#[derive(Debug)]
struct VmLogStreamer {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl VmLogStreamer {
    fn start_if_enabled(runtime: &RuntimeFiles, stream_vm_logs: bool) -> Option<Self> {
        if !(stream_vm_logs && raw_streams_enabled()) {
            return None;
        }

        let stop = Arc::new(AtomicBool::new(false));
        let handles = vec![
            spawn_log_stream_thread(
                runtime.stdout_log.clone(),
                format!("{}:serial", runtime.stdout_log.display()),
                false,
                Arc::clone(&stop),
            ),
            spawn_log_stream_thread(
                runtime.stderr_log.clone(),
                format!("{}:firecracker", runtime.stderr_log.display()),
                true,
                Arc::clone(&stop),
            ),
        ];

        Some(Self { stop, handles })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
struct StreamedLogTail {
    path: PathBuf,
    label: String,
    offset: u64,
    to_stderr: bool,
}

impl StreamedLogTail {
    fn new(path: PathBuf, label: String, to_stderr: bool) -> Self {
        Self {
            path,
            label,
            offset: 0,
            to_stderr,
        }
    }

    fn emit_pending(&mut self) -> Result<()> {
        if !self.path.is_file() {
            return Ok(());
        }

        let mut file = fs::File::open(&self.path).map_err(io_error("open"))?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(io_error("seek"))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(io_error("read_to_end"))?;
        if bytes.is_empty() {
            return Ok(());
        }

        self.offset += bytes.len() as u64;
        let text = String::from_utf8(bytes)?;
        if self.to_stderr {
            let mut stderr = std::io::stderr();
            for line in text.lines() {
                writeln!(stderr, "[{}] {}", self.label, line).map_err(io_error("write"))?;
            }
            stderr.flush().map_err(io_error("flush"))?;
        } else {
            let mut stdout = std::io::stdout();
            for line in text.lines() {
                writeln!(stdout, "[{}] {}", self.label, line).map_err(io_error("write"))?;
            }
            stdout.flush().map_err(io_error("flush"))?;
        }
        Ok(())
    }
}

fn spawn_log_stream_thread(
    path: PathBuf,
    label: String,
    to_stderr: bool,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut tail = StreamedLogTail::new(path, label, to_stderr);
        while !stop.load(Ordering::Relaxed) {
            if let Err(error) = tail.emit_pending() {
                eprintln!("failed to stream VM logs: {error}");
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        let _ = tail.emit_pending();
    })
}

impl RuntimeFiles {
    fn new(runtime_root: &Path) -> Self {
        Self {
            config: runtime_root.join("firecracker-config.json"),
            pid: runtime_root.join("firecracker.pid"),
            stdout_log: runtime_root.join("firecracker.stdout.log"),
            stderr_log: runtime_root.join("firecracker.stderr.log"),
            user_data: runtime_root.join("user-data"),
            meta_data: runtime_root.join("meta-data"),
            network_config: runtime_root.join("network-config"),
            instance_rootfs: runtime_root.join("rootfs.raw"),
            ssh_key: runtime_root.join("id_ed25519"),
            guest_ip: runtime_root.join("guest-ip"),
            host_ip: runtime_root.join("host-ip"),
            tap_name: runtime_root.join("tap-name"),
            network_mode: runtime_root.join("network-mode"),
            ssh_port: runtime_root.join("ssh-port"),
            api_port_forward: runtime_root.join("api-port-forward"),
        }
    }
}

#[derive(Debug, Clone)]
struct NetworkConfig {
    mode: FirecrackerNetworkMode,
    bridge_name: String,
    tap_name: String,
    host_ip: String,
    guest_ip: String,
    guest_mac: String,
    cidr_bits: u8,
    ssh_port: Option<u16>,
    api_port_forward: Option<u16>,
}

impl NetworkConfig {
    // Tap name and MAC construction intentionally truncates the 32-bit stable hash
    // into octets; the value is derived, not round-tripped.
    #[allow(clippy::cast_possible_truncation)]
    fn for_bridge_instance(name: &str) -> Self {
        let hash = stable_hash(name);
        let third = 1 + ((hash >> 8) % 254) as u8;
        let fourth = 2 + (hash % 253) as u8;
        let host_ip = BRIDGE_HOST_IP.to_owned();
        // TODO: make this configurable, do not hardcode
        let guest_ip = format!("172.31.{third}.{fourth}");
        let guest_mac = format!(
            "06:00:{:02x}:{:02x}:{:02x}:{:02x}",
            (hash >> 24) as u8,
            (hash >> 16) as u8,
            (hash >> 8) as u8,
            hash as u8
        );
        let tap_name = format!("ktap{hash:08x}");
        let tap_name = tap_name.chars().take(15).collect();

        Self {
            mode: FirecrackerNetworkMode::Bridge,
            bridge_name: BRIDGE_NAME.to_owned(),
            tap_name,
            host_ip,
            guest_ip,
            guest_mac,
            cidr_bits: BRIDGE_CIDR_BITS,
            ssh_port: None,
            api_port_forward: None,
        }
    }

    // Same intentional hash truncation as for_bridge_instance.
    #[allow(clippy::cast_possible_truncation)]
    fn for_rootless_instance(name: &str, ssh_port: u16, api_port_forward: Option<u16>) -> Self {
        let hash = stable_hash(name);
        let host_octet = 100 + (hash % 100) as u8;
        let guest_ip = format!("10.0.2.{host_octet}");
        let guest_mac = format!(
            "06:00:{:02x}:{:02x}:{:02x}:{:02x}",
            (hash >> 24) as u8,
            (hash >> 16) as u8,
            (hash >> 8) as u8,
            hash as u8
        );
        let tap_name = format!("ktap{hash:08x}");
        let tap_name = tap_name.chars().take(15).collect();

        Self {
            mode: FirecrackerNetworkMode::RootlessSlirp,
            bridge_name: BRIDGE_NAME.to_owned(),
            tap_name,
            host_ip: ROOTLESS_SLIRP_GATEWAY_IP.to_owned(),
            guest_ip,
            guest_mac,
            cidr_bits: ROOTLESS_SLIRP_CIDR_BITS,
            ssh_port: Some(ssh_port),
            api_port_forward,
        }
    }
}

#[derive(Debug, Clone)]
struct RootlessClusterNetwork {
    anchor_pid: u32,
    api_socket: PathBuf,
}

impl RootlessClusterNetwork {
    fn from_runtime_root(runtime_root: &Path) -> Result<Self> {
        let root = cluster_runtime_root(runtime_root)?.join(ROOTLESS_NETWORK_DIR);
        let anchor_pid = read_required_pid(&root.join("anchor.pid"), "rootless firecracker netns")?;
        let _ = read_required_pid(&root.join("slirp4netns.pid"), "slirp4netns")?;
        let api_socket = load_slirp_api_socket_path(&root)?;
        Ok(Self {
            anchor_pid,
            api_socket,
        })
    }
}

fn cluster_runtime_root(runtime_root: &Path) -> Result<PathBuf> {
    runtime_root.parent().map(PathBuf::from).ok_or_else(|| {
        Error::InvalidArgs(format!(
            "failed to derive cluster runtime root from {}",
            runtime_root.display()
        ))
    })
}

fn ensure_firecracker_network_tools(
    runner: &dyn CommandRunner,
    mode: FirecrackerNetworkMode,
) -> Result<()> {
    if mode == FirecrackerNetworkMode::RootlessSlirp {
        runner.run("unshare", &["--version".into()])?;
        runner.run("nsenter", &["--version".into()])?;
        runner.run("slirp4netns", &["--version".into()])?;
    }
    Ok(())
}

fn prepare_rootless_network(
    runner: &dyn CommandRunner,
    instance: &InstanceConfig,
    runtime: &RuntimeFiles,
) -> Result<NetworkConfig> {
    let cluster_network = ensure_rootless_cluster_network(runner, &instance.runtime_root)?;
    let ssh_port = reserve_host_port()?;
    let api_port_forward = instance.host_port_forward;
    let net = NetworkConfig::for_rootless_instance(&instance.name, ssh_port, api_port_forward);

    ensure_rootless_tap(runner, cluster_network.anchor_pid, &net)?;
    ensure_slirp_host_forward(&cluster_network.api_socket, ssh_port, &net.guest_ip, 22)?;
    if let Some(host_port) = api_port_forward {
        ensure_slirp_host_forward(&cluster_network.api_socket, host_port, &net.guest_ip, 6443)?;
    }

    write_file(&runtime.network_mode, &format!("{}\n", net.mode.as_str()))?;
    write_file(&runtime.ssh_port, &format!("{ssh_port}\n"))?;
    if let Some(host_port) = api_port_forward {
        write_file(&runtime.api_port_forward, &format!("{host_port}\n"))?;
    } else {
        let _ = fs::remove_file(&runtime.api_port_forward);
    }
    Ok(net)
}

fn ensure_rootless_cluster_network(
    runner: &dyn CommandRunner,
    runtime_root: &Path,
) -> Result<RootlessClusterNetwork> {
    let root = cluster_runtime_root(runtime_root)?.join(ROOTLESS_NETWORK_DIR);
    fs::create_dir_all(&root).map_err(io_error("create_dir_all"))?;
    // Concurrent node starts must share one fully initialized namespace.
    let network_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("initialization.lock"))
        .map_err(io_error("open rootless network lock"))?;
    network_lock
        .lock()
        .map_err(io_error("lock rootless network"))?;

    let anchor_path = root.join("anchor.pid");
    let slirp_path = root.join("slirp4netns.pid");
    let socket_path_file = root.join(ROOTLESS_SLIRP_SOCKET_PATH_FILE);
    let socket_path = load_slirp_api_socket_path(&root)?;
    if let (Some(anchor_pid), Some(slirp_pid)) = (read_pid(&anchor_path)?, read_pid(&slirp_path)?)
        && process_exists(runner, anchor_pid)
        && process_exists(runner, slirp_pid)
        && socket_path.exists()
    {
        return Ok(RootlessClusterNetwork {
            anchor_pid,
            api_socket: socket_path,
        });
    }

    let _ = terminate_stale_slirp_processes_for_socket(runner, &socket_path);
    let _ = fs::remove_file(&anchor_path);
    let _ = fs::remove_file(&slirp_path);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&socket_path_file);

    let mut anchor = Command::new("unshare");
    anchor
        .args(["--user", "--map-root-user", "--net", "--fork", "sh", "-lc"])
        .arg("trap : TERM INT; while :; do sleep 3600; done")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let anchor_child = spawn_background_process(&mut anchor, "unshare")?;
    let anchor_pid = anchor_child.id();
    write_file(&anchor_path, &format!("{anchor_pid}\n"))?;
    wait_for_rootless_anchor_ready(runner, anchor_pid)?;

    let stderr_log = root.join("slirp4netns.stderr.log");
    let socket_path = choose_slirp_api_socket_path(&root)?;
    write_file(&socket_path_file, &format!("{}\n", socket_path.display()))?;
    let netns_path = format!("/proc/{anchor_pid}/ns/net");
    let userns_path = format!("/proc/{anchor_pid}/ns/user");
    let mtu_arg = format!("--mtu={ROOTLESS_NETWORK_MTU}");
    let stderr_file = fs::File::create(&stderr_log).map_err(io_error("create"))?;
    let mut slirp = Command::new("slirp4netns");
    slirp
        .arg("--configure")
        .arg(&mtu_arg)
        .arg("--disable-host-loopback")
        .arg("--api-socket")
        .arg(&socket_path)
        .arg("--netns-type=path")
        .arg(&netns_path)
        .arg("--userns-path")
        .arg(&userns_path)
        .arg(ROOTLESS_SLIRP_TAP_NAME)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    let mut slirp_child = spawn_background_process(&mut slirp, "slirp4netns")?;
    wait_for_slirp_ready(&mut slirp_child, &socket_path, &stderr_log)?;
    write_file(&slirp_path, &format!("{}\n", slirp_child.id()))?;

    let cluster = RootlessClusterNetwork {
        anchor_pid,
        api_socket: socket_path,
    };
    ensure_rootless_bridge(runner, cluster.anchor_pid)?;
    Ok(cluster)
}

fn load_slirp_api_socket_path(root: &Path) -> Result<PathBuf> {
    let socket_path_file = root.join(ROOTLESS_SLIRP_SOCKET_PATH_FILE);
    if let Some(path) = read_trimmed(&socket_path_file)? {
        return Ok(PathBuf::from(path));
    }
    Ok(root.join(ROOTLESS_SLIRP_SOCKET_FILE))
}

fn choose_slirp_api_socket_path(root: &Path) -> Result<PathBuf> {
    let default = root.join(ROOTLESS_SLIRP_SOCKET_FILE);
    if unix_socket_path_fits(&default) {
        return Ok(default);
    }

    let short_root = Path::new(ROOTLESS_SLIRP_SHORT_SOCKET_DIR);
    fs::create_dir_all(short_root).map_err(io_error("create_dir_all"))?;
    let socket_name = format!("{:08x}.sock", stable_hash(&root.display().to_string()));
    let short_path = short_root.join(socket_name);
    if !unix_socket_path_fits(&short_path) {
        return Err(Error::InvalidArgs(format!(
            "resolved slirp4netns API socket path is too long: {}",
            short_path.display()
        )));
    }
    Ok(short_path)
}

fn unix_socket_path_fits(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX
}

fn wait_for_rootless_anchor_ready(runner: &dyn CommandRunner, anchor_pid: u32) -> Result<()> {
    for _ in 0..100 {
        if !process_exists(runner, anchor_pid) {
            return Err(Error::InvalidArgs(format!(
                "rootless network anchor process {anchor_pid} exited before becoming ready"
            )));
        }
        if run_in_rootless_namespace(runner, anchor_pid, &["true"]).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(Error::InvalidArgs(format!(
        "timed out waiting for rootless network anchor process {anchor_pid} readiness"
    )))
}

fn wait_for_slirp_ready(
    child: &mut std::process::Child,
    api_socket: &Path,
    stderr_log: &Path,
) -> Result<()> {
    for _ in 0..100 {
        if let Some(status) = child.try_wait().map_err(|source| Error::CommandIo {
            program: "slirp4netns".to_owned(),
            source,
        })? {
            let stderr = fs::read_to_string(stderr_log).unwrap_or_default();
            return Err(Error::InvalidArgs(format!(
                "slirp4netns exited early with status {:?}\nstderr:\n{}",
                status.code(),
                truncate(&stderr, 4000)
            )));
        }
        if api_socket.exists()
            && slirp_api_request(api_socket, &json!({ "execute": "list_hostfwd" })).is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let stderr = fs::read_to_string(stderr_log).unwrap_or_default();
    Err(Error::InvalidArgs(format!(
        "timed out waiting for slirp4netns readiness at {}\nstderr:\n{}",
        api_socket.display(),
        truncate(&stderr, 4000)
    )))
}

fn ensure_rootless_bridge(runner: &dyn CommandRunner, anchor_pid: u32) -> Result<()> {
    let mtu = ROOTLESS_NETWORK_MTU.to_string();
    if run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "show", "dev", BRIDGE_NAME],
    )
    .is_err()
    {
        run_in_rootless_namespace(
            runner,
            anchor_pid,
            &["ip", "link", "add", "name", BRIDGE_NAME, "type", "bridge"],
        )?;
    }
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "set", "dev", BRIDGE_NAME, "mtu", mtu.as_str()],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &[
            "ip",
            "link",
            "set",
            "dev",
            ROOTLESS_SLIRP_TAP_NAME,
            "mtu",
            mtu.as_str(),
        ],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &[
            "ip",
            "link",
            "set",
            "dev",
            ROOTLESS_SLIRP_TAP_NAME,
            "master",
            BRIDGE_NAME,
        ],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "set", "dev", ROOTLESS_SLIRP_TAP_NAME, "up"],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "set", "dev", BRIDGE_NAME, "up"],
    )?;
    Ok(())
}

fn ensure_rootless_tap(
    runner: &dyn CommandRunner,
    anchor_pid: u32,
    net: &NetworkConfig,
) -> Result<()> {
    let mtu = ROOTLESS_NETWORK_MTU.to_string();
    let _ = run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "del", net.tap_name.as_str()],
    );
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &[
            "ip",
            "tuntap",
            "add",
            "dev",
            net.tap_name.as_str(),
            "mode",
            "tap",
        ],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &[
            "ip",
            "link",
            "set",
            "dev",
            net.tap_name.as_str(),
            "mtu",
            mtu.as_str(),
        ],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &[
            "ip",
            "link",
            "set",
            "dev",
            net.tap_name.as_str(),
            "master",
            BRIDGE_NAME,
        ],
    )?;
    run_in_rootless_namespace(
        runner,
        anchor_pid,
        &["ip", "link", "set", "dev", net.tap_name.as_str(), "up"],
    )?;
    Ok(())
}

fn run_in_rootless_namespace(
    runner: &dyn CommandRunner,
    anchor_pid: u32,
    command: &[&str],
) -> Result<crate::process::Output> {
    let mut args = vec![
        "-t".into(),
        anchor_pid.to_string(),
        "--user".into(),
        "--preserve-credentials".into(),
        "--keep-caps".into(),
        "--net".into(),
    ];
    args.extend(command.iter().map(|value| (*value).to_owned()));
    runner.run("nsenter", &args)
}

fn ensure_slirp_host_forward(
    api_socket: &Path,
    host_port: u16,
    guest_addr: &str,
    guest_port: u16,
) -> Result<()> {
    if slirp_host_forward_exists(api_socket, host_port, guest_addr, guest_port)? {
        return Ok(());
    }

    let response = slirp_api_request(
        api_socket,
        &json!({
            "execute": "add_hostfwd",
            "arguments": {
                "proto": "tcp",
                "host_addr": ROOTLESS_SSH_HOST,
                "host_port": host_port,
                "guest_addr": guest_addr,
                "guest_port": guest_port
            }
        }),
    )?;
    if response.get("error").is_some() {
        return Err(Error::InvalidArgs(format!(
            "failed to add slirp4netns host forward {ROOTLESS_SSH_HOST}:{host_port} -> {guest_addr}:{guest_port}: {response}"
        )));
    }
    Ok(())
}

fn slirp_host_forward_exists(
    api_socket: &Path,
    host_port: u16,
    guest_addr: &str,
    guest_port: u16,
) -> Result<bool> {
    let response = slirp_api_request(api_socket, &json!({ "execute": "list_hostfwd" }))?;
    let entries = response
        .get("return")
        .and_then(|value| value.get("entries"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(entries.iter().any(|entry| {
        entry.get("proto").and_then(Value::as_str) == Some("tcp")
            && entry.get("host_addr").and_then(Value::as_str) == Some(ROOTLESS_SSH_HOST)
            && entry.get("host_port").and_then(Value::as_u64) == Some(u64::from(host_port))
            && entry.get("guest_addr").and_then(Value::as_str) == Some(guest_addr)
            && entry.get("guest_port").and_then(Value::as_u64) == Some(u64::from(guest_port))
    }))
}

fn remove_slirp_host_forward(
    api_socket: &Path,
    host_port: u16,
    guest_addr: &str,
    guest_port: u16,
) -> Result<()> {
    let response = slirp_api_request(api_socket, &json!({ "execute": "list_hostfwd" }))?;
    let entries = response
        .get("return")
        .and_then(|value| value.get("entries"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        let matches = entry.get("proto").and_then(Value::as_str) == Some("tcp")
            && entry.get("host_addr").and_then(Value::as_str) == Some(ROOTLESS_SSH_HOST)
            && entry.get("host_port").and_then(Value::as_u64) == Some(u64::from(host_port))
            && entry.get("guest_addr").and_then(Value::as_str) == Some(guest_addr)
            && entry.get("guest_port").and_then(Value::as_u64) == Some(u64::from(guest_port));
        if matches && let Some(id) = entry.get("id").and_then(Value::as_u64) {
            let _ = slirp_api_request(
                api_socket,
                &json!({
                    "execute": "remove_hostfwd",
                    "arguments": { "id": id }
                }),
            )?;
        }
    }
    Ok(())
}

fn slirp_api_request(api_socket: &Path, request: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(api_socket).map_err(io_error("unix_connect"))?;
    let payload = serde_json::to_vec(request).map_err(|error| {
        Error::InvalidArgs(format!(
            "failed to serialize slirp4netns API request for {}: {error}",
            api_socket.display()
        ))
    })?;
    stream.write_all(&payload).map_err(io_error("write_all"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_error("shutdown"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(io_error("read_to_string"))?;
    serde_json::from_str(&response).map_err(|error| {
        Error::InvalidArgs(format!(
            "failed to parse slirp4netns API response from {}: {error}",
            api_socket.display()
        ))
    })
}

fn reserve_host_port() -> Result<u16> {
    let listener = TcpListener::bind((ROOTLESS_SSH_HOST, 0)).map_err(io_error("tcp_bind"))?;
    listener
        .local_addr()
        .map_err(io_error("local_addr"))
        .map(|addr| addr.port())
}

fn read_required_pid(path: &Path, label: &str) -> Result<u32> {
    read_pid(path)?
        .ok_or_else(|| Error::InvalidArgs(format!("missing {label} pid at {}", path.display())))
}

fn build_config_json(
    kernel: &Path,
    instance: &InstanceConfig,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
    assets_disk: Option<&Path>,
) -> String {
    let mut drives = vec![json!({
        "drive_id": "rootfs",
        "path_on_host": runtime.instance_rootfs,
        "is_root_device": true,
        "is_read_only": false,
    })];
    if let Some(disk) = assets_disk {
        drives.push(json!({
            "drive_id": "assets",
            "path_on_host": disk,
            "is_root_device": false,
            "is_read_only": true,
        }));
    }
    json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "boot_args": BOOT_ARGS,
        },
        "drives": drives,
        "machine-config": {
            "vcpu_count": instance.cpus,
            "mem_size_mib": u32::from(instance.memory_gib) * 1024,
            "smt": false,
        },
        "network-interfaces": [{
            "iface_id": "eth0",
            "host_dev_name": net.tap_name,
            "guest_mac": net.guest_mac,
        }],
    })
    .to_string()
}

/// Cluster processes outlive the CLI and its terminal, including recorder PTYs.
fn spawn_background_process(command: &mut Command, program: &'static str) -> Result<Child> {
    command.stdin(Stdio::null());
    #[cfg(target_os = "linux")]
    // SAFETY: setsid is async-signal-safe and the hook accesses no shared state
    // or allocator between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(io_error(program))
}

fn firecracker_command(
    instance: &InstanceConfig,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
) -> Command {
    let mut command = match net.mode {
        FirecrackerNetworkMode::Bridge => Command::new("firecracker"),
        FirecrackerNetworkMode::RootlessSlirp => {
            let cluster_network = RootlessClusterNetwork::from_runtime_root(&instance.runtime_root)
                .expect("rootless cluster network should exist before firecracker spawn");
            let mut command = Command::new("nsenter");
            command.args([
                "-t".to_owned(),
                cluster_network.anchor_pid.to_string(),
                "--user".to_owned(),
                "--preserve-credentials".to_owned(),
                "--keep-caps".to_owned(),
                "--net".to_owned(),
                "firecracker".to_owned(),
            ]);
            command
        }
    };

    command
        .arg("--id")
        .arg(&instance.name)
        .arg("--no-api")
        .arg("--config-file")
        .arg(&runtime.config);
    command
}

fn write_cloud_init_files(
    runtime: &RuntimeFiles,
    instance_name: &str,
    net: &NetworkConfig,
) -> Result<()> {
    let pub_key_path = runtime.ssh_key.with_extension("pub");
    let pub_key = fs::read_to_string(&pub_key_path).map_err(io_error("read_to_string"))?;

    write_file(
        &runtime.user_data,
        // TODO: change this garbage to a struct and yaml serializer
        &format!(
            concat!(
                "#cloud-config\n",
                "users:\n",
                "  - default\n",
                "  - name: ubuntu\n",
                "    sudo: ALL=(ALL) NOPASSWD:ALL\n",
                "    groups: [sudo]\n",
                "    shell: /bin/bash\n",
                "    ssh_authorized_keys:\n",
                "      - {}\n",
                "ssh_pwauth: false\n",
                "disable_root: true\n",
                "runcmd:\n",
                "  - mkdir -p {}/{}\n",
                "  - systemctl enable ssh || systemctl enable sshd || true\n",
                "  - systemctl start ssh || systemctl start sshd || true\n"
            ),
            pub_key.trim(),
            GUEST_ARTIFACT_PARENT,
            instance_name,
        ),
    )?;
    write_file(
        &runtime.meta_data,
        &format!("instance-id: {instance_name}\nlocal-hostname: {instance_name}\n"),
    )?;
    write_file(
        &runtime.network_config,
        &format!(
            concat!(
                "version: 2\n",
                "ethernets:\n",
                "  eth0:\n",
                "    dhcp4: false\n",
                "    addresses:\n",
                "      - {}/{}\n",
                "    routes:\n",
                "      - to: 0.0.0.0/0\n",
                "        via: {}\n",
                "    nameservers:\n",
                "      addresses: [1.1.1.1, 8.8.8.8]\n"
            ),
            net.guest_ip, net.cidr_bits, net.host_ip
        ),
    )?;
    Ok(())
}

fn prepare_instance_rootfs(
    runner: &dyn CommandRunner,
    base_rootfs: &Path,
    runtime: &RuntimeFiles,
) -> Result<()> {
    if runtime.instance_rootfs.is_file() {
        return Ok(());
    }
    runner.run(
        "cp",
        &[
            "--reflink=auto".into(),
            "--sparse=always".into(),
            base_rootfs.display().to_string(),
            runtime.instance_rootfs.display().to_string(),
        ],
    )?;
    inject_nocloud_seed(runner, &runtime.instance_rootfs, runtime)
}

fn inject_nocloud_seed(
    runner: &dyn CommandRunner,
    rootfs: &Path,
    runtime: &RuntimeFiles,
) -> Result<()> {
    let command_file = runtime.config.with_file_name("debugfs-seed.commands");
    write_file(
        &command_file,
        &format!(
            // TODO: why is this garbage not a dedicated function that executes commands on
            // the gues
            concat!(
                "mkdir /var/lib/cloud\n",
                "mkdir /var/lib/cloud/seed\n",
                "mkdir {seed_dir}\n",
                "write {user_data} {seed_dir}/user-data\n",
                "write {meta_data} {seed_dir}/meta-data\n",
                "write {network_config} {seed_dir}/network-config\n"
            ),
            seed_dir = GUEST_NOCLOUD_SEED_DIR,
            user_data = runtime.user_data.display(),
            meta_data = runtime.meta_data.display(),
            network_config = runtime.network_config.display(),
        ),
    )?;

    runner.run(
        "debugfs",
        &[
            "-w".into(),
            "-f".into(),
            command_file.display().to_string(),
            rootfs.display().to_string(),
        ],
    )?;
    Ok(())
}

fn ensure_ssh_key(runner: &dyn CommandRunner, ssh_key: &Path) -> Result<()> {
    // A resumed disk already trusts this key. Never replace it merely because
    // its derived public-key file was removed.
    if ssh_key.is_file() && !ssh_key.with_extension("pub").is_file() {
        let public_key = runner.run(
            "ssh-keygen",
            &["-y".into(), "-f".into(), ssh_key.display().to_string()],
        )?;
        return write_file(&ssh_key.with_extension("pub"), &public_key.stdout);
    }
    if ssh_key.is_file() && ssh_key.with_extension("pub").is_file() {
        return Ok(());
    }

    let _ = fs::remove_file(ssh_key);
    let _ = fs::remove_file(ssh_key.with_extension("pub"));
    runner.run(
        "ssh-keygen",
        &[
            "-q".into(),
            "-t".into(),
            "ed25519".into(),
            "-N".into(),
            String::new(),
            "-f".into(),
            ssh_key.display().to_string(),
        ],
    )?;
    Ok(())
}

fn setup_bridge(runner: &dyn CommandRunner, net: &NetworkConfig) -> Result<()> {
    if runner
        .run(
            "ip",
            &[
                "link".into(),
                "show".into(),
                "dev".into(),
                net.bridge_name.clone(),
            ],
        )
        .is_err()
    {
        runner.run(
            "ip",
            &[
                "link".into(),
                "add".into(),
                "name".into(),
                net.bridge_name.clone(),
                "type".into(),
                "bridge".into(),
            ],
        )?;
    }

    runner.run(
        "ip",
        &[
            "addr".into(),
            "replace".into(),
            format!("{}/{}", net.host_ip, net.cidr_bits),
            "dev".into(),
            net.bridge_name.clone(),
        ],
    )?;
    runner.run(
        "ip",
        &[
            "link".into(),
            "set".into(),
            "dev".into(),
            net.bridge_name.clone(),
            "up".into(),
        ],
    )?;
    ensure_host_egress(runner, net)?;
    Ok(())
}

fn setup_tap(runner: &dyn CommandRunner, net: &NetworkConfig) -> Result<()> {
    let _ = runner.run("ip", &["link".into(), "del".into(), net.tap_name.clone()]);
    runner.run(
        "ip",
        &[
            "tuntap".into(),
            "add".into(),
            "dev".into(),
            net.tap_name.clone(),
            "mode".into(),
            "tap".into(),
        ],
    )?;
    runner.run(
        "ip",
        &[
            "link".into(),
            "set".into(),
            "dev".into(),
            net.tap_name.clone(),
            "master".into(),
            net.bridge_name.clone(),
        ],
    )?;
    runner.run(
        "ip",
        &[
            "link".into(),
            "set".into(),
            "dev".into(),
            net.tap_name.clone(),
            "up".into(),
        ],
    )?;
    Ok(())
}

fn ensure_prepared_network(net: &NetworkConfig) -> Result<()> {
    let bridge_path = Path::new("/sys/class/net").join(&net.bridge_name);
    if !bridge_path.exists() {
        return Err(Error::InvalidArgs(format!(
            "firecracker bridge `{}` is not configured; run `kalisto create` with sudo before `kalisto up`",
            net.bridge_name
        )));
    }

    let tap_path = Path::new("/sys/class/net").join(&net.tap_name);
    if !tap_path.exists() {
        return Err(Error::InvalidArgs(format!(
            "firecracker tap `{}` is not configured; run `kalisto create` with sudo before `kalisto up`",
            net.tap_name
        )));
    }

    Ok(())
}

fn load_network_config(runtime: &RuntimeFiles, instance_name: &str) -> Result<NetworkConfig> {
    let network_mode = read_trimmed(&runtime.network_mode)?
        .map(|value| FirecrackerNetworkMode::parse(&value))
        .transpose()?
        .unwrap_or(FirecrackerNetworkMode::Bridge);
    let default = match network_mode {
        FirecrackerNetworkMode::Bridge => NetworkConfig::for_bridge_instance(instance_name),
        FirecrackerNetworkMode::RootlessSlirp => NetworkConfig::for_rootless_instance(
            instance_name,
            read_trimmed(&runtime.ssh_port)?
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(22),
            read_trimmed(&runtime.api_port_forward)?.and_then(|value| value.parse::<u16>().ok()),
        ),
    };
    let tap_name = read_trimmed(&runtime.tap_name)?.unwrap_or_else(|| default.tap_name.clone());
    let host_ip = read_trimmed(&runtime.host_ip)?.unwrap_or_else(|| default.host_ip.clone());
    let guest_ip = read_trimmed(&runtime.guest_ip)?.unwrap_or_else(|| default.guest_ip.clone());
    let mut net = default;
    net.tap_name = tap_name;
    net.host_ip = host_ip;
    net.guest_ip = guest_ip;
    Ok(net)
}

fn wait_for_ssh(
    runner: &dyn CommandRunner,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
    child: &mut std::process::Child,
) -> Result<()> {
    for _ in 0..90 {
        if let Some(status) = child.try_wait().map_err(|source| Error::CommandIo {
            program: "firecracker".to_owned(),
            source,
        })? {
            let stderr = fs::read_to_string(&runtime.stderr_log).unwrap_or_default();
            let stdout = fs::read_to_string(&runtime.stdout_log).unwrap_or_default();
            debug_firecracker_runtime(runtime, net, &stdout, &stderr, "firecracker exited early");
            let _ = fs::remove_file(&runtime.pid);
            let hint = firecracker_startup_hint(net, &stdout, &stderr);
            return Err(Error::InvalidArgs(format!(
                "firecracker exited early with status {:?}\nstdout:\n{}\nstderr:\n{}{}",
                status.code(),
                truncate(&stdout, 2000),
                truncate(&stderr, 2000),
                hint
            )));
        }

        if run_ssh_command(runner, &runtime.ssh_key, net, "true").is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }

    let stderr = fs::read_to_string(&runtime.stderr_log).unwrap_or_default();
    let stdout = fs::read_to_string(&runtime.stdout_log).unwrap_or_default();
    debug_firecracker_runtime(
        runtime,
        net,
        &stdout,
        &stderr,
        "timed out waiting for firecracker ssh readiness",
    );
    Err(Error::InvalidArgs(format!(
        "timed out waiting for firecracker ssh readiness at {}\nstderr:\n{}",
        net.guest_ip,
        truncate(&stderr, 2000)
    )))
}

fn firecracker_startup_hint(net: &NetworkConfig, stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    if !combined.contains("Invalid TUN/TAP Backend")
        || !combined.contains("Device or resource busy")
    {
        return String::new();
    }

    format!(
        "\n\nhost TAP device `{}` is busy.\nRun a privileged network reset before `kalisto up`:\n  sudo ip link del {} || true\n  sudo ip link del dev {} type bridge || true\n  sudo kalisto create --name <cluster>\n",
        net.tap_name, net.tap_name, BRIDGE_NAME
    )
}

fn debug_firecracker_runtime(
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
    stdout: &str,
    stderr: &str,
    message: &str,
) {
    debug!(
        guest_ip = %net.guest_ip,
        host_ip = %net.host_ip,
        tap = %net.tap_name,
        config = %runtime.config.display(),
        stdout_log = %runtime.stdout_log.display(),
        stderr_log = %runtime.stderr_log.display(),
        meta_data = %runtime.meta_data.display(),
        user_data = %runtime.user_data.display(),
        network_config = %runtime.network_config.display(),
        instance_rootfs = %runtime.instance_rootfs.display(),
        nocloud_seed_dir = GUEST_NOCLOUD_SEED_DIR,
        stdout_tail = %tail(stdout, 4000),
        stderr_tail = %tail(stderr, 4000),
        "{message}"
    );
}

fn run_ssh(
    runner: &dyn CommandRunner,
    runtime_root: &Path,
    instance_name: &str,
    command: &str,
) -> Result<Output> {
    let runtime = RuntimeFiles::new(runtime_root);
    let net = load_network_config(&runtime, instance_name)?;
    run_ssh_command(runner, &runtime.ssh_key, &net, command)
}

fn mount_prepared_assets(
    runner: &dyn CommandRunner,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
    assets_root: &Path,
) -> Result<()> {
    if !assets_root.is_absolute() {
        return Err(Error::InvalidArgs(
            "firecracker prepared assets root must be absolute".to_owned(),
        ));
    }
    let root = shell_quote(&assets_root.to_string_lossy());
    let command = format!(
        "set -eu; test \"$(blkid -s LABEL -o value /dev/vdb)\" = KALISTO_ASSETS; \
         mkdir -p -- {root}; \
         if ! mountpoint -q -- {root}; then mount -t ext4 -o ro,noload /dev/vdb {root}; fi; \
         findmnt -rn -S /dev/vdb -M {root} -o OPTIONS | grep -Eq '(^|,)ro(,|$)'; \
         test -s {root}/k3s; test -s {root}/images.tar"
    );
    run_ssh_command(
        runner,
        &runtime.ssh_key,
        net,
        &format!("sudo sh -lc {}", shell_quote(&command)),
    )?;
    Ok(())
}

fn run_ssh_command(
    runner: &dyn CommandRunner,
    ssh_key: &Path,
    net: &NetworkConfig,
    command: &str,
) -> Result<Output> {
    let (mut args, host) = ssh_connection_args(ssh_key, net, "-p")?;
    args.push(format!("{SSH_USER}@{host}"));
    args.push(command.to_owned());
    runner.run("ssh", &args)
}

/// Host SSH agent keys are forwarded to guests when an agent is available.
///
/// Kalisto's own guest key stays the primary identity; forwarded host keys act
/// as fallbacks so guest-side git/ssh operations can use the host agent.
fn ssh_connection_args<'a>(
    ssh_key: &Path,
    net: &'a NetworkConfig,
    port_flag: &str,
) -> Result<(Vec<String>, &'a str)> {
    let (host, port) = match net.mode {
        FirecrackerNetworkMode::Bridge => (net.guest_ip.as_str(), None),
        FirecrackerNetworkMode::RootlessSlirp => (
            ROOTLESS_SSH_HOST,
            Some(net.ssh_port.ok_or_else(|| {
                Error::InvalidArgs("missing rootless firecracker SSH host port".to_owned())
            })?),
        ),
    };
    let mut args = vec![
        "-F".into(),
        "/dev/null".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-i".into(),
        ssh_key.display().to_string(),
    ];
    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        args.extend(["-o".to_owned(), "ForwardAgent=yes".to_owned()]);
    }
    if let Some(port) = port {
        args.push(port_flag.into());
        args.push(port.to_string());
    }
    Ok((args, host))
}

fn stop_process(
    runner: &dyn CommandRunner,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
    force: bool,
) -> Result<()> {
    if !force {
        let _ = run_ssh_command(
            runner,
            &runtime.ssh_key,
            net,
            "sudo shutdown -h now || sudo poweroff || true",
        );
        for _ in 0..30 {
            if run_ssh_command(runner, &runtime.ssh_key, net, "true").is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    if let Some(pid) = read_pid(&runtime.pid)?
        && process_exists(runner, pid)
    {
        let _ = signal(runner, "-TERM", pid);
        for _ in 0..20 {
            if !process_exists(runner, pid) {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        if process_exists(runner, pid) {
            let _ = signal(runner, "-KILL", pid);
        }
    }

    let _ = fs::remove_file(&runtime.pid);
    if net.mode == FirecrackerNetworkMode::RootlessSlirp {
        cleanup_rootless_instance_network(runner, runtime, net)?;
    }
    Ok(())
}

fn cleanup_rootless_instance_network(
    runner: &dyn CommandRunner,
    runtime: &RuntimeFiles,
    net: &NetworkConfig,
) -> Result<()> {
    let cluster =
        RootlessClusterNetwork::from_runtime_root(runtime.pid.parent().ok_or_else(|| {
            Error::InvalidArgs("missing runtime root for rootless cleanup".to_owned())
        })?)?;
    if let Some(ssh_port) = net.ssh_port {
        let _ = remove_slirp_host_forward(&cluster.api_socket, ssh_port, &net.guest_ip, 22);
    }
    if let Some(api_port_forward) = net.api_port_forward {
        let _ =
            remove_slirp_host_forward(&cluster.api_socket, api_port_forward, &net.guest_ip, 6443);
    }
    let _ = run_in_rootless_namespace(
        runner,
        cluster.anchor_pid,
        &["ip", "link", "del", net.tap_name.as_str()],
    );
    let _ = fs::remove_file(&runtime.ssh_port);
    let _ = fs::remove_file(&runtime.api_port_forward);
    maybe_cleanup_rootless_cluster_network(runner, runtime)?;
    Ok(())
}

fn maybe_cleanup_rootless_cluster_network(
    runner: &dyn CommandRunner,
    runtime: &RuntimeFiles,
) -> Result<()> {
    let cluster_root = cluster_runtime_root(runtime.pid.parent().ok_or_else(|| {
        Error::InvalidArgs("missing runtime root for rootless cleanup".to_owned())
    })?)?;
    let mut active = false;
    for entry in fs::read_dir(&cluster_root).map_err(io_error("read_dir"))? {
        let entry = entry.map_err(io_error("read_dir"))?;
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(ROOTLESS_NETWORK_DIR)
            || !path.is_dir()
        {
            continue;
        }
        if read_pid(&path.join("firecracker.pid"))?.is_some() {
            active = true;
            break;
        }
    }
    if active {
        return Ok(());
    }

    let network_root = cluster_root.join(ROOTLESS_NETWORK_DIR);
    if let Some(slirp_pid) = read_pid(&network_root.join("slirp4netns.pid"))?
        && process_exists(runner, slirp_pid)
    {
        let _ = signal(runner, "-TERM", slirp_pid);
    }
    if let Some(anchor_pid) = read_pid(&network_root.join("anchor.pid"))?
        && process_exists(runner, anchor_pid)
    {
        let _ = signal(runner, "-TERM", anchor_pid);
    }
    let socket_path = load_slirp_api_socket_path(&network_root)?;
    let _ = terminate_stale_slirp_processes_for_socket(runner, &socket_path);
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(network_root.join(ROOTLESS_SLIRP_SOCKET_PATH_FILE));
    let _ = fs::remove_dir_all(network_root);
    Ok(())
}

fn ensure_host_egress(runner: &dyn CommandRunner, net: &NetworkConfig) -> Result<()> {
    let uplink = detect_uplink_interface(runner)?;
    runner.run("sysctl", &["-w".into(), "net.ipv4.ip_forward=1".into()])?;
    ensure_iptables_rule(
        runner,
        None,
        &forward_from_bridge_rule_args(net),
        &forward_from_bridge_rule_args(net),
    )?;
    ensure_iptables_rule(
        runner,
        None,
        &forward_to_bridge_rule_args(net),
        &forward_to_bridge_rule_args(net),
    )?;
    let nat_args = nat_postrouting_rule_args(net, &uplink);
    ensure_iptables_rule(runner, Some("nat"), &nat_args, &nat_args)?;
    Ok(())
}

fn teardown_host_egress(runner: &dyn CommandRunner, net: &NetworkConfig) -> Result<()> {
    let uplink = detect_uplink_interface(runner)?;
    remove_iptables_rule(runner, None, &forward_from_bridge_rule_args(net))?;
    remove_iptables_rule(runner, None, &forward_to_bridge_rule_args(net))?;
    remove_iptables_rule(
        runner,
        Some("nat"),
        &nat_postrouting_rule_args(net, &uplink),
    )?;
    Ok(())
}

fn detect_uplink_interface(runner: &dyn CommandRunner) -> Result<String> {
    let output = runner.run("ip", &["route".into(), "show".into(), "default".into()])?;
    parse_default_route_interface(&output.stdout)
}

fn parse_default_route_interface(output: &str) -> Result<String> {
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        while let Some(part) = parts.next() {
            if part == "dev" {
                let interface = parts.next().ok_or_else(|| {
                    Error::InvalidArgs(format!(
                        "failed to parse uplink interface from default route output: {output}"
                    ))
                })?;
                if !interface.is_empty() {
                    return Ok(interface.to_owned());
                }
            }
        }
    }

    Err(Error::InvalidArgs(format!(
        "failed to detect host uplink interface from default route output: {output}"
    )))
}

fn ensure_iptables_rule(
    runner: &dyn CommandRunner,
    table: Option<&str>,
    check_args: &[String],
    add_args: &[String],
) -> Result<()> {
    let mut check = iptables_base_args(table);
    check.push("-C".into());
    check.extend_from_slice(check_args);
    if runner.run("iptables", &check).is_ok() {
        return Ok(());
    }

    let mut add = iptables_base_args(table);
    add.push("-A".into());
    add.extend_from_slice(add_args);
    runner.run("iptables", &add)?;
    Ok(())
}

fn remove_iptables_rule(
    runner: &dyn CommandRunner,
    table: Option<&str>,
    delete_args: &[String],
) -> Result<()> {
    let mut check = iptables_base_args(table);
    check.push("-C".into());
    check.extend_from_slice(delete_args);
    if runner.run("iptables", &check).is_err() {
        return Ok(());
    }

    let mut delete = iptables_base_args(table);
    delete.push("-D".into());
    delete.extend_from_slice(delete_args);
    runner.run("iptables", &delete)?;
    Ok(())
}

fn iptables_base_args(table: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(table) = table {
        args.push("-t".into());
        args.push(table.to_owned());
    }
    args
}

fn forward_from_bridge_rule_args(net: &NetworkConfig) -> Vec<String> {
    vec![
        "FORWARD".into(),
        "-i".into(),
        net.bridge_name.clone(),
        "-j".into(),
        "ACCEPT".into(),
    ]
}

fn forward_to_bridge_rule_args(net: &NetworkConfig) -> Vec<String> {
    vec![
        "FORWARD".into(),
        "-o".into(),
        net.bridge_name.clone(),
        "-m".into(),
        "conntrack".into(),
        "--ctstate".into(),
        "RELATED,ESTABLISHED".into(),
        "-j".into(),
        "ACCEPT".into(),
    ]
}

fn nat_postrouting_rule_args(net: &NetworkConfig, uplink: &str) -> Vec<String> {
    vec![
        "POSTROUTING".into(),
        "-s".into(),
        firecracker_subnet(net),
        "!".into(),
        "-d".into(),
        firecracker_subnet(net),
        "-o".into(),
        uplink.to_owned(),
        "-j".into(),
        "MASQUERADE".into(),
    ]
}

fn firecracker_subnet(net: &NetworkConfig) -> String {
    let mut octets = [0_u8; 4];
    for (index, part) in net.host_ip.split('.').take(4).enumerate() {
        octets[index] = part.parse::<u8>().unwrap_or(0);
    }

    let prefix_bytes = usize::from(net.cidr_bits / 8);
    for octet in octets.iter_mut().skip(prefix_bytes) {
        *octet = 0;
    }

    format!(
        "{}.{}.{}.{}/{}",
        octets[0], octets[1], octets[2], octets[3], net.cidr_bits
    )
}

fn read_trimmed(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(
        fs::read_to_string(path)
            .map_err(io_error("read_to_string"))?
            .trim()
            .to_owned(),
    ))
}

fn read_pid(path: &Path) -> Result<Option<u32>> {
    let Some(content) = read_trimmed(path)? else {
        return Ok(None);
    };
    let pid = content
        .parse::<u32>()
        .map_err(|error| Error::InvalidArgs(format!("invalid firecracker pid file: {error}")))?;
    Ok(Some(pid))
}

fn process_exists(runner: &dyn CommandRunner, pid: u32) -> bool {
    signal(runner, "-0", pid).is_ok()
}

fn terminate_stale_slirp_processes_for_socket(
    runner: &dyn CommandRunner,
    socket_path: &Path,
) -> Result<()> {
    let marker = socket_path.display().to_string();
    let output = runner.run("ps", &["-eo".into(), "pid=,args=".into()])?;
    for line in output.stdout.lines() {
        let Some((pid, args)) = parse_pid_and_args_line(line) else {
            continue;
        };
        if !args.contains("slirp4netns") || !args.contains(&marker) || !process_exists(runner, pid)
        {
            continue;
        }

        let _ = signal(runner, "-TERM", pid);
        for _ in 0..20 {
            if !process_exists(runner, pid) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if process_exists(runner, pid) {
            let _ = signal(runner, "-KILL", pid);
        }
    }
    Ok(())
}

fn parse_pid_and_args_line(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut fields = trimmed.splitn(2, char::is_whitespace);
    let pid = fields.next()?.parse::<u32>().ok()?;
    let args = fields.next().unwrap_or_default().trim_start();
    Some((pid, args))
}

fn signal(runner: &dyn CommandRunner, signal: &str, pid: u32) -> Result<Output> {
    runner.run("kill", &[signal.to_owned(), pid.to_string()])
}

fn stable_hash(input: &str) -> u32 {
    let mut value = 0x811c_9dc5_u32;
    for byte in input.bytes() {
        value ^= u32::from(byte);
        value = value.wrapping_mul(0x0100_0193);
    }
    value
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn ensure_path_exists(path: &Path, label: &str) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(Error::InvalidArgs(format!(
            "{label} path does not exist: {}",
            path.display()
        )))
    }
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).map_err(io_error("write"))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

fn tail(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_owned();
    }

    let skipped = char_count - max_chars;
    let tailed: String = text.chars().skip(skipped).collect();
    format!("...{tailed}")
}

fn missing_artifact_message(label: &str) -> String {
    format!(
        "firecracker backend requires a configured {label} path; pass --firecracker-{label} or configure artifact discovery"
    )
}

fn io_error(program: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::CommandIo {
        program: program.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Mutex;

    use super::{
        BACKEND, FirecrackerNetworkMode, NetworkConfig, ROOTLESS_SLIRP_SHORT_SOCKET_DIR,
        UNIX_SOCKET_PATH_MAX, build_config_json, choose_slirp_api_socket_path, firecracker_subnet,
        load_slirp_api_socket_path, nat_postrouting_rule_args, parse_default_route_interface,
        parse_pid_and_args_line, stable_hash,
    };
    use crate::backend::InstanceConfig;
    use crate::backend::runtime::ClusterBackend;
    use crate::error::Result;
    use crate::process::{CommandRunner, Output};

    #[derive(Default)]
    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            panic!("unexpected command invocation: {program} {args:?}");
        }
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "linux")]
    mod process_lifetime {
        use std::fs;
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::os::unix::process::CommandExt;
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        struct OwnedChild(Child);

        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        struct Workspace(PathBuf);

        impl Drop for Workspace {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn deadline_stream(stream: &UnixStream) {
            let timeout = Some(Duration::from_secs(5));
            stream.set_read_timeout(timeout).unwrap();
            stream.set_write_timeout(timeout).unwrap();
        }

        fn launcher(socket: &Path) {
            let mut parent = UnixStream::connect(socket).unwrap();
            deadline_stream(&parent);
            let (mut channel, child_channel) = UnixStream::pair().unwrap();
            deadline_stream(&channel);
            let descriptor = child_channel.as_raw_fd();
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "printf ready >&3; read -r token <&3; printf survived >&3",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: these async-signal-safe calls only prepare this child's
            // IPC descriptor and restore the default hangup disposition.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(descriptor, 3) == -1
                        || libc::fcntl(3, libc::F_SETFD, 0) == -1
                        || libc::signal(libc::SIGHUP, libc::SIG_DFL) == libc::SIG_ERR
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = OwnedChild(
                super::super::spawn_background_process(&mut command, "/bin/sh").unwrap(),
            );
            drop(child_channel);
            let mut ready = [0; 5];
            channel.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            parent.write_all(&ready).unwrap();
            let mut closed = [0; 1];
            parent.read_exact(&mut closed).unwrap();
            channel.write_all(b"go\n").unwrap();
            let mut survived = [0; 8];
            channel.read_exact(&mut survived).unwrap();
            assert_eq!(&survived, b"survived");
            assert!(child.0.wait().unwrap().success());
        }

        #[test]
        fn background_process_survives_launching_pty_close() {
            const SOCKET_ENV: &str = "KALISTO_FIRECRACKER_PTY_TEST_SOCKET";
            if let Some(socket) = std::env::var_os(SOCKET_ENV) {
                launcher(Path::new(&socket));
                return;
            }
            let root = std::env::temp_dir().join(format!(
                "kalisto-fc-pty-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let root = Workspace(root);
            let socket = root.0.join("ipc.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: openpty initializes two owned descriptors on success;
            // the optional name, termios and window-size pointers are null.
            let result = unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
            // SAFETY: each descriptor is valid and transferred exactly once.
            let master = unsafe { fs::File::from_raw_fd(master) };
            // SAFETY: the slave descriptor is distinct from the master.
            let slave = unsafe { fs::File::from_raw_fd(slave) };
            // The launcher must not inherit another master descriptor, or the
            // parent's close would not actually hang up the terminal.
            for descriptor in [master.as_raw_fd(), slave.as_raw_fd()] {
                // SAFETY: both descriptors are live and owned in this scope.
                assert_ne!(
                    unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
                    -1,
                    "{}",
                    std::io::Error::last_os_error()
                );
            }
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "backend::firecracker::tests::process_lifetime::background_process_survives_launching_pty_close",
                    "--nocapture",
                ])
                .env(SOCKET_ENV, &socket)
                .stdin(Stdio::from(slave));
            // SAFETY: only async-signal-safe session, terminal and signal
            // operations run after fork. The launcher ignores HUP so it can
            // observe its child after the parent closes the PTY.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1
                        || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1
                        || libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = OwnedChild(command.spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut channel = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(child.0.try_wait().unwrap().is_none(), "launcher exited");
                        assert!(Instant::now() < deadline, "launcher did not connect");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept launcher IPC: {error}"),
                }
            };
            deadline_stream(&channel);
            let mut ready = [0; 5];
            channel.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            drop(master);
            channel.write_all(b"x").unwrap();
            assert!(child.0.wait().unwrap().success());
        }
    }

    #[test]
    fn firecracker_config_contains_required_sections() {
        let runtime_root = Path::new("/tmp/runtime");
        let runtime = super::RuntimeFiles::new(runtime_root);
        let net = NetworkConfig::for_bridge_instance("vm");
        let disk = Path::new("/tmp/assets/firecracker-assets.ext4");
        let json = build_config_json(
            Path::new("/tmp/vmlinux"),
            &InstanceConfig {
                name: "vm".to_owned(),
                runtime_root: runtime_root.to_path_buf(),
                host_port_forward: None,
                cpus: 2,
                memory_gib: 4,
            },
            &runtime,
            &net,
            Some(disk),
        );
        let config: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(config["machine-config"]["mem_size_mib"], 4096);
        assert_eq!(config["network-interfaces"][0]["iface_id"], "eth0");
        assert_eq!(config["drives"].as_array().unwrap().len(), 2);
        assert_eq!(config["drives"][0]["drive_id"], "rootfs");
        assert_eq!(
            config["drives"][0]["path_on_host"],
            runtime.instance_rootfs.to_str().unwrap()
        );
        assert_eq!(config["drives"][0]["is_root_device"], true);
        assert_eq!(config["drives"][0]["is_read_only"], false);
        assert_eq!(config["drives"][1]["path_on_host"], disk.to_str().unwrap());
        assert_eq!(config["drives"][1]["is_root_device"], false);
        assert_eq!(config["drives"][1]["is_read_only"], true);
        assert!(
            config["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .contains("ds=nocloud;s=file:///var/lib/cloud/seed/nocloud/")
        );
    }

    #[test]
    fn existing_rootfs_is_preserved_without_copying_or_reseeding() {
        let _guard = TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "kalisto-firecracker-preserve-rootfs-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let runtime = super::RuntimeFiles::new(&root);
        fs::write(&runtime.instance_rootfs, b"existing kubernetes state").unwrap();
        fs::write(&runtime.ssh_key, b"existing private key").unwrap();
        fs::write(
            runtime.ssh_key.with_extension("pub"),
            b"existing public key",
        )
        .unwrap();

        super::prepare_instance_rootfs(&PanicRunner, Path::new("/unused/base"), &runtime).unwrap();
        super::ensure_ssh_key(&PanicRunner, &runtime.ssh_key).unwrap();
        assert_eq!(
            fs::read(&runtime.instance_rootfs).unwrap(),
            b"existing kubernetes state"
        );
        assert_eq!(fs::read(&runtime.ssh_key).unwrap(), b"existing private key");
        fs::remove_dir_all(root).unwrap();
    }

    #[derive(Default)]
    struct RecordingRunner {
        commands: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            self.commands
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            Ok(Output {
                stdout: if args
                    .last()
                    .is_some_and(|arg| arg.starts_with("umask 077; mktemp"))
                {
                    "/tmp/kalisto-transfer.unit\n".to_owned()
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn ssh_connection_args_forwards_host_agent_when_present() {
        let _guard = TEST_LOCK.lock().unwrap();
        let net = NetworkConfig::for_bridge_instance("vm");
        let previous = std::env::var_os("SSH_AUTH_SOCK");

        // SAFETY: TEST_LOCK serialises this test against other tests that read
        // or mutate the process environment.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", "/tmp/kalisto-agent-test.sock") };
        let (forwarded, _) = super::ssh_connection_args(Path::new("/k/id_ed25519"), &net, "-p")
            .expect("bridge ssh args");
        // SAFETY: TEST_LOCK is still held while restoring the environment.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SSH_AUTH_SOCK", value),
                None => std::env::remove_var("SSH_AUTH_SOCK"),
            }
        };
        let (plain, _) = super::ssh_connection_args(Path::new("/k/id_ed25519"), &net, "-p")
            .expect("bridge ssh args");

        assert!(
            forwarded
                .windows(2)
                .any(|pair| pair == ["-o", "ForwardAgent=yes"])
        );
        assert!(
            !plain
                .windows(2)
                .any(|pair| pair == ["-o", "ForwardAgent=yes"])
        );
        for args in [&forwarded, &plain] {
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-o", "IdentitiesOnly=yes"])
            );
            assert!(args.windows(2).any(|pair| pair == ["-i", "/k/id_ed25519"]));
        }
    }

    #[test]
    fn prepared_disk_is_mounted_readonly_at_host_assets_path_and_verified() {
        let runner = RecordingRunner::default();
        let runtime = super::RuntimeFiles::new(Path::new("/tmp/runtime"));
        let net = NetworkConfig::for_bridge_instance("vm");
        super::mount_prepared_assets(&runner, &runtime, &net, Path::new("/cache/assets")).unwrap();
        let commands = runner.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        let (program, args) = &commands[0];
        assert_eq!(program, "ssh");
        let command = args.last().unwrap();
        assert!(command.starts_with("sudo sh -lc "));
        assert!(command.contains("mount -t ext4 -o ro,noload /dev/vdb"));
        assert!(command.contains("/cache/assets"));
        assert!(command.contains("KALISTO_ASSETS"));
        assert!(command.contains("findmnt"));
        assert!(command.contains("/k3s"));
        assert!(command.contains("/images.tar"));
    }

    #[test]
    fn manifest_transfer_uses_scp_and_restrictive_guest_install() {
        let _guard = TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "kalisto-firecracker-transfer-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("cilium.yaml");
        fs::write(&source, "manifest contents\n".repeat(20_000)).unwrap();
        fs::write(root.join("network-mode"), "slirp4netns\n").unwrap();
        fs::write(root.join("ssh-port"), "12345\n").unwrap();
        let runner = RecordingRunner::default();
        BACKEND
            .copy_file_to_guest(
                &runner,
                "vm",
                &root,
                &source,
                Path::new("/var/lib/kalisto/vm/cilium.yaml"),
            )
            .unwrap();
        let commands = runner.commands.lock().unwrap();
        let (_, scp) = commands
            .iter()
            .find(|(program, _)| program == "scp")
            .unwrap();
        assert!(scp.iter().any(|arg| arg == "-O"));
        assert!(scp.windows(2).any(|args| args == ["-P", "12345"]));
        assert!(
            scp.iter()
                .any(|arg| arg == &root.join("id_ed25519").display().to_string())
        );
        assert!(scp.last().unwrap().starts_with("ubuntu@127.0.0.1:"));
        assert!(commands.iter().any(|(program, args)| {
            program == "ssh"
                && args.last().is_some_and(|arg| {
                    arg.contains("install -D -m 0600")
                        && arg.contains("/var/lib/kalisto/vm/cilium.yaml")
                })
        }));
        assert!(
            commands
                .iter()
                .all(|(_, args)| { args.iter().all(|arg| !arg.contains("manifest contents")) })
        );
        assert!(
            commands
                .last()
                .unwrap()
                .1
                .last()
                .unwrap()
                .starts_with("rm -f -- ")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(stable_hash("kalisto-cp"), stable_hash("kalisto-cp"));
        assert_ne!(stable_hash("kalisto-cp"), stable_hash("kalisto-w0"));
    }

    #[test]
    fn parses_default_route_uplink_interface() {
        let output = "default via 192.168.1.1 dev eno1 proto dhcp src 192.168.1.20 metric 100\n";
        assert_eq!(parse_default_route_interface(output).unwrap(), "eno1");
    }

    #[test]
    fn firecracker_subnet_matches_bridge_network() {
        let net = NetworkConfig::for_bridge_instance("vm");
        assert_eq!(firecracker_subnet(&net), "172.31.0.0/16");
    }

    #[test]
    fn nat_rule_targets_uplink_and_bridge_subnet() {
        let net = NetworkConfig::for_bridge_instance("vm");
        let args = nat_postrouting_rule_args(&net, "eno1");

        assert_eq!(
            args,
            vec![
                "POSTROUTING",
                "-s",
                "172.31.0.0/16",
                "!",
                "-d",
                "172.31.0.0/16",
                "-o",
                "eno1",
                "-j",
                "MASQUERADE"
            ]
        );
    }

    #[test]
    fn parses_rootless_firecracker_network_mode_aliases() {
        assert_eq!(
            FirecrackerNetworkMode::parse("slirp").unwrap(),
            FirecrackerNetworkMode::RootlessSlirp
        );
        assert_eq!(
            FirecrackerNetworkMode::parse("slirp4netns").unwrap(),
            FirecrackerNetworkMode::RootlessSlirp
        );
        assert_eq!(
            FirecrackerNetworkMode::parse("").unwrap(),
            FirecrackerNetworkMode::Bridge
        );
    }

    #[test]
    fn query_status_without_pid_reports_stopped_without_shelling_out() {
        let _guard = TEST_LOCK.lock().unwrap();
        let runtime_root = std::env::temp_dir().join(format!(
            "kalisto-firecracker-status-no-pid-{}",
            std::process::id()
        ));
        fs::create_dir_all(&runtime_root).unwrap();
        let runner = PanicRunner;

        let status = BACKEND
            .query_instance_status(&runner, "kalisto-test-cp", &runtime_root)
            .unwrap();
        assert_eq!(status, "kalisto-test-cp Stopped firecracker");

        let _ = fs::remove_dir_all(runtime_root);
    }

    #[test]
    fn choose_slirp_api_socket_path_falls_back_to_short_tmp_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        let long_root = std::env::temp_dir().join(format!(
            "kalisto-firecracker-rootless-network-{}-{}-{}",
            "x".repeat(70),
            std::process::id(),
            stable_hash("fallback")
        ));
        let socket_path = choose_slirp_api_socket_path(&long_root).unwrap();
        assert!(socket_path.starts_with(Path::new(ROOTLESS_SLIRP_SHORT_SOCKET_DIR)));
        assert!(socket_path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX);
    }

    #[test]
    fn load_slirp_api_socket_path_prefers_persisted_override() {
        let _guard = TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "kalisto-firecracker-socket-path-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("slirp4netns.socket-path"),
            "/tmp/kalisto-slirp4netns/unit.sock\n",
        )
        .unwrap();

        let socket_path = load_slirp_api_socket_path(&root).unwrap();
        assert_eq!(socket_path, Path::new("/tmp/kalisto-slirp4netns/unit.sock"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_pid_and_args_line_extracts_pid_and_args() {
        let parsed = parse_pid_and_args_line("  1234 slirp4netns --api-socket /tmp/a.sock")
            .expect("line should parse");
        assert_eq!(parsed.0, 1234);
        assert_eq!(parsed.1, "slirp4netns --api-socket /tmp/a.sock");
    }

    #[test]
    fn parse_pid_and_args_line_rejects_invalid_rows() {
        assert!(parse_pid_and_args_line("").is_none());
        assert!(parse_pid_and_args_line("not-a-pid slirp4netns --help").is_none());
    }
}

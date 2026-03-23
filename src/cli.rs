use std::path::PathBuf;

use structopt::StructOpt;
use structopt::clap::Shell;
use tracing::Level;

use crate::backend::BackendKind;
use crate::cluster::addon::CniPlugin;
use crate::cluster::spec::ClusterSpec;
use crate::k3s::ServerMode;
use crate::paths::{default_cluster_name, default_project_root, selected_cluster_name};

#[derive(Debug, StructOpt)]
#[structopt(name = "kalisto")]
pub struct Cli {
    #[structopt(long, parse(try_from_str = parse_log_level))]
    pub log_level: Option<Level>,
    #[structopt(subcommand)]
    pub command: Command,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub enum Command {
    Create(UpArgs),
    Up(UpArgs),
    Down(ClusterLookupArgs),
    Destroy(ClusterLookupArgs),
    Logs(NodeLogsArgs),
    Addon(AddonArgs),
    Use(NamedClusterArgs),
    Scale(ScaleArgs),
    Completions(CompletionsArgs),
    Ls,
    Stats(StatsArgs),
    Status(ClusterLookupArgs),
    Kubeconfig(ClusterLookupArgs),
    Bench(BenchArgs),
    ProbeMacosLima(MacosLimaArgs),
    ProbeMacosLimaK3s(MacosLimaK3sArgs),
}

#[derive(Debug, StructOpt)]
pub struct AddonArgs {
    #[structopt(subcommand)]
    pub command: AddonCommand,
}

#[derive(Debug, StructOpt)]
pub struct NodeLogsArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub node: String,
    #[structopt(short = "f", long)]
    pub follow: bool,
}

impl NodeLogsArgs {
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub enum AddonCommand {
    Install(AddonInstallArgs),
}

#[derive(Debug, StructOpt)]
pub struct AddonInstallArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(name = "addon")]
    pub addon: String,
}

impl AddonInstallArgs {
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct UpArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long, parse(try_from_str = parse_backend_kind))]
    pub backend: Option<BackendKind>,
    #[structopt(long)]
    pub template: Option<String>,
    #[structopt(long, parse(from_os_str))]
    pub firecracker_kernel: Option<PathBuf>,
    #[structopt(long, parse(from_os_str))]
    pub firecracker_rootfs: Option<PathBuf>,
    #[structopt(long)]
    pub workers: Option<usize>,
    #[structopt(long)]
    pub control_plane_cpus: Option<u8>,
    #[structopt(long)]
    pub control_plane_memory_gib: Option<u8>,
    #[structopt(long)]
    pub worker_cpus: Option<u8>,
    #[structopt(long)]
    pub worker_memory_gib: Option<u8>,
    #[structopt(long)]
    pub network: Option<String>,
    #[structopt(long)]
    pub api_port: Option<u16>,
    #[structopt(long)]
    pub k3s_version: Option<String>,
    #[structopt(long, parse(try_from_str = parse_cni_plugin))]
    pub cni: Option<CniPlugin>,
    #[structopt(long)]
    pub cilium_version: Option<String>,
    #[structopt(long)]
    pub keep_failed_runtime: bool,
    #[structopt(long)]
    pub stream_vm_logs: bool,
    #[structopt(long)]
    pub agentless: bool,
    #[structopt(long = "no-rosetta")]
    pub no_rosetta: bool,
}

impl UpArgs {
    pub fn into_cluster_spec(self) -> crate::error::Result<ClusterSpec> {
        let project_root = default_project_root(self.project_root)?;
        let name = self
            .name
            .unwrap_or(default_cluster_name(Some(&project_root))?);
        let mut spec = ClusterSpec::new(name, project_root);
        if let Some(backend) = self.backend {
            spec.backend = backend;
        }
        if let Some(template) = self.template {
            spec.template = template;
        }
        spec.firecracker_kernel = self.firecracker_kernel;
        spec.firecracker_rootfs = self.firecracker_rootfs;
        if let Some(workers) = self.workers {
            spec.worker_count = workers;
            if spec.backend == BackendKind::Lima && workers > 0 && spec.network == "vzNAT" {
                spec.network = "lima:user-v2".to_owned();
            }
        }
        if let Some(cpus) = self.control_plane_cpus {
            spec.control_plane_resources.cpus = cpus;
        }
        if let Some(memory_gib) = self.control_plane_memory_gib {
            spec.control_plane_resources.memory_gib = memory_gib;
        }
        if let Some(cpus) = self.worker_cpus {
            spec.worker_resources.cpus = cpus;
        }
        if let Some(memory_gib) = self.worker_memory_gib {
            spec.worker_resources.memory_gib = memory_gib;
        }
        if let Some(network) = self.network {
            spec.network = network;
        }
        if let Some(api_port) = self.api_port {
            spec.api_port = api_port;
        }
        if let Some(k3s_version) = self.k3s_version {
            spec.k3s_version = k3s_version;
        }
        if let Some(cni) = self.cni {
            spec.cni_plugin = cni;
            spec.cni_plugin
                .apply_component_defaults(&mut spec.component_toggles);
        }
        if let Some(cilium_version) = self.cilium_version {
            spec.cilium_version = cilium_version;
        }
        spec.keep_failed_runtime = self.keep_failed_runtime;
        spec.stream_vm_logs = self.stream_vm_logs;
        if self.agentless {
            spec.server_mode = ServerMode::Agentless;
            spec.worker_count = 0;
        }
        spec.enable_rosetta = !self.no_rosetta;
        Ok(spec)
    }
}

fn parse_cni_plugin(value: &str) -> std::result::Result<CniPlugin, String> {
    CniPlugin::parse(value)
        .ok_or_else(|| format!("unsupported cni `{value}`; expected one of: none, flannel, cilium"))
}

fn parse_backend_kind(value: &str) -> std::result::Result<BackendKind, String> {
    BackendKind::parse(value)
        .ok_or_else(|| format!("unsupported backend `{value}`; expected one of: firecracker, lima"))
}

fn parse_log_level(value: &str) -> std::result::Result<Level, String> {
    match value.to_ascii_lowercase().as_str() {
        "trace" => Ok(Level::TRACE),
        "debug" => Ok(Level::DEBUG),
        "info" => Ok(Level::INFO),
        "warn" => Ok(Level::WARN),
        "error" => Ok(Level::ERROR),
        _ => Err(format!(
            "unsupported log level `{value}`; expected one of: trace, debug, info, warn, error"
        )),
    }
}

#[derive(Debug, StructOpt)]
pub struct ClusterLookupArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub force: bool,
}

impl ClusterLookupArgs {
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct NamedClusterArgs {
    #[structopt(name = "name")]
    pub name: String,
}

#[derive(Debug, StructOpt)]
pub struct ScaleArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub workers: usize,
}

impl ScaleArgs {
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct CompletionsArgs {
    #[structopt(possible_values = &["bash", "elvish", "fish", "powershell", "zsh"])]
    pub shell: Shell,
}

#[derive(Debug, StructOpt)]
pub struct StatsArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub watch: bool,
    #[structopt(long, default_value = "2")]
    pub interval_secs: u64,
}

impl StatsArgs {
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct BenchArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long, parse(try_from_str = parse_backend_kind))]
    pub backend: Option<BackendKind>,
    #[structopt(long)]
    pub template: Option<String>,
    #[structopt(long, parse(from_os_str))]
    pub firecracker_kernel: Option<PathBuf>,
    #[structopt(long, parse(from_os_str))]
    pub firecracker_rootfs: Option<PathBuf>,
    #[structopt(long)]
    pub workers: Option<usize>,
    #[structopt(long)]
    pub control_plane_cpus: Option<u8>,
    #[structopt(long)]
    pub control_plane_memory_gib: Option<u8>,
    #[structopt(long)]
    pub worker_cpus: Option<u8>,
    #[structopt(long)]
    pub worker_memory_gib: Option<u8>,
    #[structopt(long)]
    pub network: Option<String>,
    #[structopt(long)]
    pub api_port: Option<u16>,
    #[structopt(long)]
    pub k3s_version: Option<String>,
    #[structopt(long, parse(try_from_str = parse_cni_plugin))]
    pub cni: Option<CniPlugin>,
    #[structopt(long)]
    pub cilium_version: Option<String>,
    #[structopt(long)]
    pub keep_failed_runtime: bool,
    #[structopt(long)]
    pub stream_vm_logs: bool,
    #[structopt(long)]
    pub agentless: bool,
    #[structopt(long = "no-rosetta")]
    pub no_rosetta: bool,
    #[structopt(long, default_value = "3")]
    pub runs: usize,
}

impl BenchArgs {
    pub fn into_cluster_spec(self) -> crate::error::Result<(ClusterSpec, usize)> {
        let runs = self.runs;
        let up = UpArgs {
            project_root: self.project_root,
            name: self.name,
            backend: self.backend,
            template: self.template,
            firecracker_kernel: self.firecracker_kernel,
            firecracker_rootfs: self.firecracker_rootfs,
            workers: self.workers,
            control_plane_cpus: self.control_plane_cpus,
            control_plane_memory_gib: self.control_plane_memory_gib,
            worker_cpus: self.worker_cpus,
            worker_memory_gib: self.worker_memory_gib,
            network: self.network,
            api_port: self.api_port,
            k3s_version: self.k3s_version,
            cni: self.cni,
            cilium_version: self.cilium_version,
            keep_failed_runtime: self.keep_failed_runtime,
            stream_vm_logs: self.stream_vm_logs,
            agentless: self.agentless,
            no_rosetta: self.no_rosetta,
        };
        Ok((up.into_cluster_spec()?, runs))
    }
}

#[derive(Debug, StructOpt)]
pub struct MacosLimaArgs {
    #[structopt(long, parse(from_os_str))]
    pub workspace_root: PathBuf,
    #[structopt(long)]
    pub template: Option<String>,
    #[structopt(long)]
    pub network: Option<String>,
    #[structopt(long = "no-rosetta")]
    pub no_rosetta: bool,
    #[structopt(long)]
    pub keep_resources: bool,
}

#[derive(Debug, StructOpt)]
pub struct MacosLimaK3sArgs {
    #[structopt(long, parse(from_os_str))]
    pub workspace_root: PathBuf,
    #[structopt(long)]
    pub template: Option<String>,
    #[structopt(long)]
    pub workers: Option<usize>,
    #[structopt(long)]
    pub network: Option<String>,
    #[structopt(long)]
    pub api_port: Option<u16>,
    #[structopt(long)]
    pub k3s_version: Option<String>,
    #[structopt(long)]
    pub agentless: bool,
    #[structopt(long = "no-rosetta")]
    pub no_rosetta: bool,
    #[structopt(long)]
    pub keep_resources: bool,
}

#[cfg(test)]
mod tests {
    use structopt::StructOpt;

    use crate::cluster::addon::CniPlugin;

    use super::{Cli, Command};

    #[test]
    fn up_flags_parse_runtime_debug_options() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "--log-level",
            "debug",
            "up",
            "--keep-failed-runtime",
            "--stream-vm-logs",
            "--control-plane-cpus",
            "8",
            "--control-plane-memory-gib",
            "32",
            "--worker-cpus",
            "4",
            "--worker-memory-gib",
            "12",
        ])
        .expect("cli should parse");

        assert_eq!(cli.log_level, Some(tracing::Level::DEBUG));
        match cli.command {
            Command::Up(args) => {
                assert!(args.keep_failed_runtime);
                assert!(args.stream_vm_logs);
                assert_eq!(args.control_plane_cpus, Some(8));
                assert_eq!(args.control_plane_memory_gib, Some(32));
                assert_eq!(args.worker_cpus, Some(4));
                assert_eq!(args.worker_memory_gib, Some(12));
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn use_command_parses_cluster_name() {
        let cli = Cli::from_iter_safe(["kalisto", "use", "dev"]).expect("cli should parse");

        match cli.command {
            Command::Use(args) => assert_eq!(args.name, "dev"),
            other => panic!("expected use command, got {other:?}"),
        }
    }

    #[test]
    fn completions_command_parses_shell() {
        let cli = Cli::from_iter_safe(["kalisto", "completions", "zsh"]).expect("cli should parse");

        match cli.command {
            Command::Completions(args) => {
                assert_eq!(format!("{:?}", args.shell).to_ascii_lowercase(), "zsh")
            }
            other => panic!("expected completions command, got {other:?}"),
        }
    }

    #[test]
    fn scale_command_parses_worker_target() {
        let cli =
            Cli::from_iter_safe(["kalisto", "scale", "--workers", "3"]).expect("cli should parse");

        match cli.command {
            Command::Scale(args) => {
                assert!(args.name.is_none());
                assert_eq!(args.workers, 3);
            }
            other => panic!("expected scale command, got {other:?}"),
        }
    }

    #[test]
    fn stats_command_parses_default_lookup_args() {
        let cli = Cli::from_iter_safe(["kalisto", "stats", "--watch", "--interval-secs", "5"])
            .expect("cli should parse");

        match cli.command {
            Command::Stats(args) => {
                assert!(args.name.is_none());
                assert!(args.watch);
                assert_eq!(args.interval_secs, 5);
            }
            other => panic!("expected stats command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_parses_cni_flags() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--cni",
            "cilium",
            "--cilium-version",
            "1.19.2",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                assert_eq!(args.cni, Some(CniPlugin::Cilium));
                assert_eq!(args.cilium_version.as_deref(), Some("1.19.2"));
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn addon_install_command_parses_catalog_id() {
        let cli =
            Cli::from_iter_safe(["kalisto", "addon", "install", "lb/metallb"]).expect("parse");

        match cli.command {
            Command::Addon(args) => match args.command {
                super::AddonCommand::Install(args) => {
                    assert!(args.name.is_none());
                    assert_eq!(args.addon, "lb/metallb");
                }
            },
            other => panic!("expected addon command, got {other:?}"),
        }
    }

    #[test]
    fn logs_command_parses_node_name() {
        let cli = Cli::from_iter_safe(["kalisto", "logs", "--node", "kalisto-dev-cp", "-f"])
            .expect("cli should parse");

        match cli.command {
            Command::Logs(args) => {
                assert!(args.name.is_none());
                assert_eq!(args.node, "kalisto-dev-cp");
                assert!(args.follow);
            }
            other => panic!("expected logs command, got {other:?}"),
        }
    }

    #[test]
    fn create_command_parses_cluster_flags() {
        let cli = Cli::from_iter_safe(["kalisto", "create", "--name", "dev"]).expect("parse");
        match cli.command {
            Command::Create(args) => assert_eq!(args.name.as_deref(), Some("dev")),
            other => panic!("expected create command, got {other:?}"),
        }
    }

    #[test]
    fn destroy_command_parses_lookup_args() {
        let cli = Cli::from_iter_safe(["kalisto", "destroy", "--name", "dev"]).expect("parse");
        match cli.command {
            Command::Destroy(args) => assert_eq!(args.name.as_deref(), Some("dev")),
            other => panic!("expected destroy command, got {other:?}"),
        }
    }

    #[test]
    fn remove_command_is_rejected() {
        assert!(Cli::from_iter_safe(["kalisto", "remove", "--name", "dev"]).is_err());
    }
}

use std::collections::HashSet;
use std::path::PathBuf;

use structopt::StructOpt;
use structopt::clap::Shell;
use tracing::Level;

use crate::backend::BackendKind;
use crate::cluster::addon::CniPlugin;
use crate::cluster::spec::{ClusterSpec, ImageArch, parse_image_arch};
use crate::config::load_project_config;
use crate::k3s::ServerMode;
use crate::paths::{default_cluster_name, default_project_root, selected_cluster_name};

#[derive(Debug, StructOpt)]
#[structopt(name = "kalisto")]
pub struct Cli {
    #[structopt(long, parse(try_from_str = parse_log_level))]
    pub log_level: Option<Level>,
    /// Emit machine-readable JSON from read-only inspection commands
    /// (list, status, stats, kubeconfig).
    #[structopt(long, global = true)]
    pub json: bool,
    #[structopt(subcommand)]
    pub command: Command,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub enum Command {
    Create(UpArgs),
    /// Cache assets for the selected backend before starting fresh nodes.
    Pull(UpArgs),
    /// Export stopped native VZ nodes, including credentials and VM-local data.
    Export(ExportArgs),
    /// Restore a trusted cold archive without overwriting or starting nodes.
    Import(ImportArgs),
    /// Generate a private native age identity; prints only its public recipient.
    ArchiveKeygen(ArchiveKeygenArgs),
    Up(UpArgs),
    Test(TestArgs),
    Mcp(McpArgs),
    #[structopt(name = "mcp-task-runner")]
    McpTaskRunner(McpTaskRunnerArgs),
    /// Apply a Kubernetes workload manifest to a running cluster.
    Apply(ApplyArgs),
    Down(ClusterLookupArgs),
    Destroy(DestroyArgs),
    Logs(NodeLogsArgs),
    Addon(AddonArgs),
    Plugin(AddonArgs),
    Use(NamedClusterArgs),
    Scale(ScaleArgs),
    Completions(CompletionsArgs),
    #[structopt(name = "list", alias = "ls")]
    List,
    Stats(StatsArgs),
    Status(ClusterLookupArgs),
    Kubeconfig(KubeconfigArgs),
    /// Run the managed kubectl binary against a cluster with its kubeconfig.
    Kubectl(KubectlArgs),
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
pub struct TestArgs {
    #[structopt(subcommand)]
    pub command: TestCommand,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub enum TestCommand {
    Run(TestRunArgs),
    Ls(TestListArgs),
}

#[derive(Debug, Clone, StructOpt)]
pub struct TestRunArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub scenario: String,
}

impl TestRunArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn into_cluster_spec(&self) -> crate::error::Result<ClusterSpec> {
        resolve_base_cluster_spec(self.project_root.clone(), self.name.clone())
    }
}

#[derive(Debug, StructOpt)]
pub struct TestListArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct ApplyArgs {
    #[structopt(long)]
    pub name: Option<String>,
    /// Path to a Kubernetes manifest file to apply on the cluster.
    #[structopt(long, parse(from_os_str))]
    pub manifest: PathBuf,
}

impl ApplyArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(setting = structopt::clap::AppSettings::TrailingVarArg)]
pub struct KubectlArgs {
    #[structopt(long)]
    pub name: Option<String>,
    /// Print the managed kubectl binary path and exit.
    #[structopt(long)]
    pub path: bool,
    /// Arguments passed through to the managed kubectl binary.
    #[structopt(allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl KubectlArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
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
    Ls(AddonListArgs),
}

#[derive(Debug, StructOpt)]
pub struct AddonInstallArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(name = "addon")]
    pub addon: String,
}

impl AddonInstallArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct AddonListArgs {
    #[structopt(long)]
    pub name: Option<String>,
}

impl AddonListArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // CLI arguments / persisted config surface; bool-per-flag is the design
#[derive(Debug, StructOpt)]
pub struct UpArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
    /// Backend: apple-container by default on macOS, firecracker on Linux.
    #[structopt(long, parse(try_from_str = parse_backend_kind))]
    pub backend: Option<BackendKind>,
    #[structopt(long)]
    pub template: Option<String>,
    #[structopt(long = "arch", parse(try_from_str = parse_image_arch))]
    pub image_arch: Option<ImageArch>,
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
    #[structopt(long)]
    pub cilium_version: Option<String>,
    #[structopt(long = "addons", alias = "addon", parse(try_from_str = parse_up_addon_id))]
    pub addons: Vec<String>,
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn into_cluster_spec(self) -> crate::error::Result<ClusterSpec> {
        let mut spec = resolve_base_cluster_spec(self.project_root.clone(), self.name.clone())?;
        if let Some(backend) = self.backend {
            if spec.backend != backend {
                spec.network = crate::cluster::spec::default_network(backend);
            }
            spec.backend = backend;
        }
        if let Some(template) = self.template {
            spec.template = template;
        }
        spec.image_arch = self.image_arch;
        spec.firecracker_kernel = self.firecracker_kernel;
        spec.firecracker_rootfs = self.firecracker_rootfs;
        if let Some(workers) = self.workers {
            spec.worker_count = workers;
            spec.requested_worker_count = Some(workers);
            if spec.backend == BackendKind::Lima && workers > 0 && spec.network == "vzNAT" {
                "lima:user-v2".clone_into(&mut spec.network);
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
        if let Some(cilium_version) = self.cilium_version {
            spec.cilium_version = cilium_version;
        }
        apply_requested_addons(&mut spec, &self.addons)?;
        if spec.addons.iter().any(|addon| addon == "lb/metallb") || !spec.metallb.is_empty() {
            spec.metallb.validate()?;
        }
        spec.keep_failed_runtime = self.keep_failed_runtime;
        spec.stream_vm_logs = self.stream_vm_logs;
        if self.agentless {
            spec.server_mode = ServerMode::Agentless;
            spec.worker_count = 0;
        }
        if self.no_rosetta {
            spec.enable_rosetta = false;
        }
        Ok(spec)
    }
}

fn resolve_base_cluster_spec(
    project_root: Option<PathBuf>,
    cli_name: Option<String>,
) -> crate::error::Result<ClusterSpec> {
    let project_root = default_project_root(project_root)?;
    let config = load_project_config(&project_root)?;
    let name = cli_name
        .clone()
        .or_else(|| {
            config
                .as_ref()
                .and_then(|config| config.cluster.name.clone())
        })
        .unwrap_or(default_cluster_name(Some(&project_root))?);
    let mut spec = ClusterSpec::new(name, project_root);
    if let Some(config) = &config {
        config.apply_to_cluster_spec(&mut spec)?;
    }
    if let Some(name) = cli_name {
        spec.name = name;
    }
    Ok(spec)
}

#[derive(Debug, StructOpt)]
pub struct ExportArgs {
    #[structopt(long)]
    pub name: String,
    #[structopt(long, parse(from_os_str))]
    pub output: PathBuf,
    /// Authorize graceful shutdown; no forced power-off is used for export.
    #[structopt(long)]
    pub stop: bool,
    /// Encrypt to a native age1... public key. Repeat for multiple recipients.
    #[structopt(long = "recipient", conflicts_with_all = &["passphrase", "passphrase-file"])]
    pub recipients: Vec<String>,
    /// Prompt twice on the terminal; passphrase is never accepted as an argument.
    #[structopt(long, conflicts_with = "passphrase-file")]
    pub passphrase: bool,
    /// Read a passphrase from a private mode-0600 file (automation/MCP).
    #[structopt(long, parse(from_os_str))]
    pub passphrase_file: Option<PathBuf>,
}

impl ExportArgs {
    /// # Errors
    ///
    /// Returns an error if prompting fails, passphrases do not match, or encryption
    /// options, recipients, or passphrase files are invalid.
    pub fn encryption(&self) -> crate::error::Result<crate::archive::ExportEncryption> {
        if self.passphrase {
            crate::archive::ExportEncryption::with_passphrase(prompt_archive_passphrase(true)?)
        } else {
            crate::archive::ExportEncryption::from_files(
                &self.recipients,
                self.passphrase_file.as_deref(),
            )
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct ArchiveKeygenArgs {
    #[structopt(long, parse(from_os_str))]
    pub output: PathBuf,
}

#[derive(Debug, StructOpt)]
pub struct ImportArgs {
    #[structopt(parse(from_os_str))]
    pub input: PathBuf,
    #[structopt(long)]
    pub api_port: Option<u16>,
    /// Maximum total uncompressed payload size (default: 1 TiB).
    #[structopt(long, default_value = "1099511627776")]
    pub max_bytes: u64,
    /// Private native age identity file. Repeat for multiple files.
    #[structopt(long = "identity", parse(from_os_str), conflicts_with_all = &["passphrase", "passphrase-file"])]
    pub identity_files: Vec<PathBuf>,
    #[structopt(long, conflicts_with = "passphrase-file")]
    pub passphrase: bool,
    #[structopt(long, parse(from_os_str))]
    pub passphrase_file: Option<PathBuf>,
}

impl ImportArgs {
    /// # Errors
    ///
    /// Returns an error if prompting fails or credential options or files are invalid.
    pub fn decryption(&self) -> crate::error::Result<crate::archive::ImportDecryption> {
        if self.passphrase {
            Ok(crate::archive::ImportDecryption::Passphrase(
                prompt_archive_passphrase(false)?,
            ))
        } else {
            crate::archive::ImportDecryption::from_files(
                &self.identity_files,
                self.passphrase_file.as_deref(),
            )
        }
    }
}

fn prompt_archive_passphrase(confirm: bool) -> crate::error::Result<age::secrecy::SecretString> {
    use age::secrecy::ExposeSecret;
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(crate::error::Error::InvalidArgs(
            "passphrase prompt requires a terminal; use a private --passphrase-file for automation"
                .into(),
        ));
    }
    let secret = age::secrecy::SecretString::from(
        rpassword::prompt_password("Archive passphrase: ").map_err(|_| {
            crate::error::Error::InvalidArgs("cannot read archive passphrase from terminal".into())
        })?,
    );
    if confirm {
        let repeated = age::secrecy::SecretString::from(
            rpassword::prompt_password("Confirm archive passphrase: ").map_err(|_| {
                crate::error::Error::InvalidArgs(
                    "cannot read archive passphrase confirmation".into(),
                )
            })?,
        );
        if secret.expose_secret() != repeated.expose_secret() {
            return Err(crate::error::Error::InvalidArgs(
                "archive passphrases do not match".into(),
            ));
        }
    }
    Ok(secret)
}

#[derive(Debug, StructOpt)]
pub struct McpArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, StructOpt)]
pub struct McpTaskRunnerArgs {
    #[structopt(long, parse(from_os_str))]
    pub manifest: PathBuf,
    #[structopt(long, parse(from_os_str))]
    pub exit_path: PathBuf,
}

fn parse_up_addon_id(value: &str) -> std::result::Result<String, String> {
    match value {
        "cni/flannel" | "cni/cilium" | "ingress/traefik" | "ingress/kong" | "lb/servicelb"
        | "lb/metallb" | "storage/longhorn" | "storage/local-path" => Ok(value.to_owned()),
        _ => Err(format!(
            "unsupported addon `{value}`; expected one of: cni/flannel, cni/cilium, ingress/traefik, ingress/kong, lb/servicelb, lb/metallb, storage/longhorn, storage/local-path"
        )),
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn apply_requested_addons(
    spec: &mut ClusterSpec,
    addons: &[String],
) -> crate::error::Result<()> {
    let mut requested_cni = None;
    for addon in addons {
        let addon = parse_up_addon_id(addon).map_err(crate::error::Error::InvalidArgs)?;
        match addon.as_str() {
            "cni/flannel" => {
                requested_cni = merge_requested_cni(requested_cni, CniPlugin::Flannel)?;
            }
            "cni/cilium" => requested_cni = merge_requested_cni(requested_cni, CniPlugin::Cilium)?,
            _ if !spec.addons.iter().any(|installed| installed == &addon) => {
                spec.addons.push(addon);
            }
            _ => {}
        }
    }
    if let Some(cni) = requested_cni {
        spec.cni_plugin = cni;
        spec.cni_plugin
            .apply_component_defaults(&mut spec.component_toggles);
    }
    Ok(())
}

fn merge_requested_cni(
    existing: Option<CniPlugin>,
    requested: CniPlugin,
) -> crate::error::Result<Option<CniPlugin>> {
    match existing {
        Some(current) if current != requested => Err(crate::error::Error::InvalidArgs(format!(
            "conflicting CNI add-ons requested: `{}` and `{}` are mutually exclusive",
            current.as_str(),
            requested.as_str()
        ))),
        _ => Ok(Some(requested)),
    }
}

fn parse_backend_kind(value: &str) -> std::result::Result<BackendKind, String> {
    BackendKind::parse(value).ok_or_else(|| {
        format!(
            "unsupported backend `{value}`; expected one of: firecracker, lima, apple-container"
        )
    })
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
pub struct DestroyArgs {
    /// Cluster names, separated by commas; defaults to the selected cluster.
    #[structopt(long, conflicts_with = "all")]
    pub name: Option<String>,
    /// Destroy every saved cluster definition.
    #[structopt(short = "a", long, conflicts_with = "name")]
    pub all: bool,
    #[structopt(long)]
    pub force: bool,
}

impl DestroyArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_names(&self) -> crate::error::Result<Option<Vec<String>>> {
        let Some(value) = &self.name else {
            return Ok(selected_cluster_name()?.map(|name| vec![name]));
        };
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for name in value.split(',').map(str::trim) {
            if name.is_empty() {
                return Err(crate::error::Error::InvalidArgs(
                    "cluster names must not be empty in --name".to_owned(),
                ));
            }
            if seen.insert(name) {
                names.push(name.to_owned());
            }
        }
        Ok(Some(names))
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct KubeconfigArgs {
    #[structopt(long)]
    pub name: Option<String>,
    #[structopt(long)]
    pub path: bool,
}

impl KubeconfigArgs {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn cluster_name(&self) -> crate::error::Result<Option<String>> {
        match &self.name {
            Some(name) => Ok(Some(name.clone())),
            None => selected_cluster_name(),
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // CLI arguments / persisted config surface; bool-per-flag is the design
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
    #[structopt(long = "arch", parse(try_from_str = parse_image_arch))]
    pub image_arch: Option<ImageArch>,
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
    #[structopt(long)]
    pub cilium_version: Option<String>,
    #[structopt(long = "addons", alias = "addon", parse(try_from_str = parse_up_addon_id))]
    pub addons: Vec<String>,
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
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn into_cluster_spec(self) -> crate::error::Result<(ClusterSpec, usize)> {
        let runs = self.runs;
        let up = UpArgs {
            project_root: self.project_root,
            name: self.name,
            backend: self.backend,
            template: self.template,
            image_arch: self.image_arch,
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
            cilium_version: self.cilium_version,
            addons: self.addons,
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
    use std::fs;

    use structopt::StructOpt;

    use crate::cluster::addon::CniPlugin;
    use crate::cluster::spec::ImageArch;

    use super::{Cli, Command};

    #[test]
    fn archive_encryption_flags_parse_and_refuse_conflicting_or_inline_secrets() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "export",
            "--name",
            "dev",
            "--output",
            "backup.age",
            "--recipient",
            "age1first",
            "--recipient",
            "age1second",
        ])
        .unwrap();
        match cli.command {
            Command::Export(args) => assert_eq!(args.recipients, vec!["age1first", "age1second"]),
            _ => panic!("expected export"),
        }
        assert!(
            Cli::from_iter_safe([
                "kalisto",
                "export",
                "--name",
                "dev",
                "--output",
                "backup.age",
                "--recipient",
                "age1first",
                "--passphrase"
            ])
            .is_err()
        );
        assert!(
            Cli::from_iter_safe([
                "kalisto",
                "export",
                "--name",
                "dev",
                "--output",
                "backup.age",
                "--passphrase",
                "--passphrase-file",
                "secret-file"
            ])
            .is_err()
        );
        assert!(
            Cli::from_iter_safe([
                "kalisto",
                "import",
                "backup.age",
                "--identity",
                "dev.agekey",
                "--passphrase-file",
                "secret-file"
            ])
            .is_err()
        );
        assert!(
            Cli::from_iter_safe([
                "kalisto",
                "import",
                "backup.age",
                "--password",
                "forbidden-inline-secret"
            ])
            .is_err()
        );
        let cli = Cli::from_iter_safe([
            "kalisto",
            "import",
            "backup.age",
            "--identity",
            "first.agekey",
            "--identity",
            "second.agekey",
        ])
        .unwrap();
        match cli.command {
            Command::Import(args) => assert_eq!(args.identity_files.len(), 2),
            _ => panic!("expected import"),
        }
        assert!(matches!(
            Cli::from_iter_safe(["kalisto", "archive-keygen", "--output", "dev.agekey"])
                .unwrap()
                .command,
            Command::ArchiveKeygen(_)
        ));
    }

    #[test]
    fn archive_commands_parse_safe_defaults_and_explicit_shutdown_consent() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "export",
            "--name",
            "dev",
            "--output",
            "dev.kalisto",
        ])
        .unwrap();
        match cli.command {
            Command::Export(args) => assert!(!args.stop),
            _ => panic!("expected export"),
        }
        let cli = Cli::from_iter_safe([
            "kalisto",
            "export",
            "--name",
            "dev",
            "--output",
            "dev.kalisto",
            "--stop",
        ])
        .unwrap();
        match cli.command {
            Command::Export(args) => assert!(args.stop),
            _ => panic!("expected export"),
        }
        let cli = Cli::from_iter_safe(["kalisto", "import", "dev.kalisto", "--api-port", "16444"])
            .unwrap();
        match cli.command {
            Command::Import(args) => {
                assert_eq!(args.api_port, Some(16444));
                assert_eq!(args.max_bytes, crate::archive::DEFAULT_MAX_BYTES);
            }
            _ => panic!("expected import"),
        }
        assert!(Cli::from_iter_safe(["kalisto", "import", "dev.kalisto", "--force"]).is_err());
    }

    #[test]
    fn pull_accepts_the_same_cluster_asset_selection_as_up() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "pull",
            "--backend",
            "lima",
            "--arch",
            "arm64",
            "--addons",
            "cni/cilium",
        ])
        .unwrap();
        match cli.command {
            Command::Pull(args) => {
                assert_eq!(args.image_arch, Some(ImageArch::Aarch64));
                assert_eq!(args.addons, vec!["cni/cilium"]);
            }
            other => panic!("expected pull command, got {other:?}"),
        }
    }

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
                assert_eq!(format!("{:?}", args.shell).to_ascii_lowercase(), "zsh");
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
    fn test_run_command_parses_scenario_and_name() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "test",
            "run",
            "--scenario",
            "smoke",
            "--name",
            "dev",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Test(args) => match args.command {
                super::TestCommand::Run(args) => {
                    assert_eq!(args.scenario, "smoke");
                    assert_eq!(args.name.as_deref(), Some("dev"));
                }
                super::TestCommand::Ls(_) => {
                    panic!("expected test run command, got ls")
                }
            },
            other => panic!("expected test command, got {other:?}"),
        }
    }

    #[test]
    fn test_ls_command_parses_project_root() {
        let cli = Cli::from_iter_safe(["kalisto", "test", "ls", "--project-root", "/tmp/work"])
            .expect("cli should parse");

        match cli.command {
            Command::Test(args) => match args.command {
                super::TestCommand::Ls(args) => {
                    assert_eq!(
                        args.project_root,
                        Some(std::path::PathBuf::from("/tmp/work"))
                    );
                }
                super::TestCommand::Run(_) => {
                    panic!("expected test ls command, got run")
                }
            },
            other => panic!("expected test command, got {other:?}"),
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
    fn up_command_parses_cni_addon_flags() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--addons",
            "cni/cilium",
            "--cilium-version",
            "1.19.2",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                assert_eq!(args.addons, vec!["cni/cilium".to_owned()]);
                assert_eq!(args.cilium_version.as_deref(), Some("1.19.2"));
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_parses_image_arch_flag() {
        let cli = Cli::from_iter_safe(["kalisto", "up", "--arch", "amd64"]).expect("parse");

        match cli.command {
            Command::Up(args) => assert_eq!(args.image_arch, Some(ImageArch::X86_64)),
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_parses_addon_flags() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--addons",
            "cni/flannel",
            "--addons",
            "lb/metallb",
            "--addons",
            "ingress/traefik",
            "--addons",
            "ingress/kong",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                assert_eq!(
                    args.addons,
                    vec![
                        "cni/flannel".to_owned(),
                        "lb/metallb".to_owned(),
                        "ingress/traefik".to_owned(),
                        "ingress/kong".to_owned()
                    ]
                );
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_cni_addon_sets_cluster_cni_without_tracking_it_as_post_boot_addon() {
        let cli = Cli::from_iter_safe(["kalisto", "up", "--addons", "cni/flannel"]).expect("parse");

        match cli.command {
            Command::Up(args) => {
                let spec = args.into_cluster_spec().unwrap();
                assert_eq!(spec.cni_plugin, CniPlugin::Flannel);
                assert!(spec.component_toggles.flannel_backend_none);
                assert!(!spec.addons.iter().any(|addon| addon == "cni/flannel"));
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_rejects_conflicting_cni_addons() {
        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--addons",
            "cni/flannel",
            "--addons",
            "cni/cilium",
        ])
        .expect("parse");

        match cli.command {
            Command::Up(args) => {
                let error = args.into_cluster_spec().unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("conflicting CNI add-ons requested")
                );
            }
            other => panic!("expected up command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_rejects_unsupported_addon_flag() {
        let error =
            Cli::from_iter_safe(["kalisto", "up", "--addons", "ingress/nginx"]).unwrap_err();
        assert!(format!("{error}").contains("unsupported addon `ingress/nginx`"));
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
                super::AddonCommand::Ls(_) => {
                    panic!("expected addon install command, got ls")
                }
            },
            other => panic!("expected addon command, got {other:?}"),
        }
    }

    #[test]
    fn addon_ls_command_parses_optional_cluster_name() {
        let cli = Cli::from_iter_safe(["kalisto", "addon", "ls", "--name", "dev"]).expect("parse");

        match cli.command {
            Command::Addon(args) => match args.command {
                super::AddonCommand::Ls(args) => assert_eq!(args.name.as_deref(), Some("dev")),
                super::AddonCommand::Install(_) => {
                    panic!("expected addon ls command, got install")
                }
            },
            other => panic!("expected addon command, got {other:?}"),
        }
    }

    #[test]
    fn plugin_alias_command_parses_ls_subcommand() {
        let cli = Cli::from_iter_safe(["kalisto", "plugin", "ls"]).expect("parse");

        match cli.command {
            Command::Plugin(args) => match args.command {
                super::AddonCommand::Ls(args) => assert!(args.name.is_none()),
                super::AddonCommand::Install(_) => {
                    panic!("expected plugin ls command, got install")
                }
            },
            other => panic!("expected plugin command, got {other:?}"),
        }
    }

    #[test]
    fn up_command_loads_kalisto_toml_into_cluster_spec() {
        let root = std::env::temp_dir().join(format!("kalisto-cli-config-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("kalisto.toml"),
            r#"
[cluster]
name = "from-config"
backend = "lima"
api_port = 17443

[plugins.cilium]
enabled = true
version = "1.19.2"
"#,
        )
        .unwrap();

        let cli = Cli::from_iter_safe(["kalisto", "up", "--project-root", root.to_str().unwrap()])
            .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                let spec = args.into_cluster_spec().unwrap();
                assert_eq!(spec.name, "from-config");
                assert_eq!(spec.backend, crate::backend::BackendKind::Lima);
                assert_eq!(spec.network, "lima:user-v2");
                assert_eq!(spec.api_port, 17443);
                assert_eq!(spec.cni_plugin, CniPlugin::Cilium);
                assert_eq!(spec.cilium_version, "1.19.2");
            }
            other => panic!("expected up command, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn up_command_cli_flags_override_kalisto_toml() {
        let root =
            std::env::temp_dir().join(format!("kalisto-cli-override-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("kalisto.toml"),
            r#"
[cluster]
name = "from-config"
backend = "apple-container"
network = "apple-container"
api_port = 17443

[plugins.cilium]
enabled = true
"#,
        )
        .unwrap();

        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--project-root",
            root.to_str().unwrap(),
            "--backend",
            "lima",
            "--name",
            "from-cli",
            "--api-port",
            "18443",
            "--addons",
            "cni/flannel",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                let spec = args.into_cluster_spec().unwrap();
                assert_eq!(spec.name, "from-cli");
                assert_eq!(spec.backend, crate::backend::BackendKind::Lima);
                assert_eq!(spec.network, "lima:user-v2");
                assert_eq!(spec.api_port, 18443);
                assert_eq!(spec.cni_plugin, CniPlugin::Flannel);
                assert!(spec.component_toggles.flannel_backend_none);
            }
            other => panic!("expected up command, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn up_command_cli_addons_extend_kalisto_toml_plugins() {
        let root =
            std::env::temp_dir().join(format!("kalisto-cli-addon-merge-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("kalisto.toml"),
            r"
[plugins.traefik]
enabled = true
",
        )
        .unwrap();

        let cli = Cli::from_iter_safe([
            "kalisto",
            "up",
            "--project-root",
            root.to_str().unwrap(),
            "--addons",
            "lb/metallb",
            "--addons",
            "ingress/traefik",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Up(args) => {
                let spec = args.into_cluster_spec().unwrap();
                assert!(spec.addons.iter().any(|addon| addon == "ingress/traefik"));
                assert!(spec.addons.iter().any(|addon| addon == "lb/metallb"));
                assert_eq!(
                    spec.addons
                        .iter()
                        .filter(|addon| addon.as_str() == "ingress/traefik")
                        .count(),
                    1
                );
            }
            other => panic!("expected up command, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_run_uses_project_cluster_config() {
        let root =
            std::env::temp_dir().join(format!("kalisto-cli-test-config-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("kalisto.toml"),
            r#"
[cluster]
name = "from-config"
backend = "lima"

[[harness.scenarios]]
name = "smoke"

[[harness.scenarios.probes]]
type = "command"
name = "pods"
command = "kubectl get pods"
"#,
        )
        .unwrap();

        let cli = Cli::from_iter_safe([
            "kalisto",
            "test",
            "run",
            "--project-root",
            root.to_str().unwrap(),
            "--scenario",
            "smoke",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Test(args) => match args.command {
                super::TestCommand::Run(args) => {
                    let spec = args.into_cluster_spec().unwrap();
                    assert_eq!(spec.name, "from-config");
                    assert_eq!(spec.backend, crate::backend::BackendKind::Lima);
                }
                super::TestCommand::Ls(_) => {
                    panic!("expected test run command, got ls")
                }
            },
            other => panic!("expected test command, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
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
    fn destroy_accepts_all_short_and_long_flags() {
        for flag in ["-a", "--all"] {
            let cli = Cli::from_iter_safe(["kalisto", "destroy", flag]).unwrap();
            match cli.command {
                Command::Destroy(args) => assert!(args.all),
                other => panic!("expected destroy command, got {other:?}"),
            }
        }
    }

    #[test]
    fn destroy_all_conflicts_with_named_clusters() {
        assert!(Cli::from_iter_safe(["kalisto", "destroy", "-a", "--name", "one,two"]).is_err());
    }

    #[test]
    fn destroy_splits_trims_and_deduplicates_names() {
        let cli = Cli::from_iter_safe(["kalisto", "destroy", "--name", "one, two,one"]).unwrap();
        match cli.command {
            Command::Destroy(args) => assert_eq!(
                args.cluster_names().unwrap(),
                Some(vec!["one".to_owned(), "two".to_owned()]),
            ),
            other => panic!("expected destroy command, got {other:?}"),
        }
    }

    #[test]
    fn destroy_rejects_empty_names_before_any_cleanup() {
        for value in ["", "one,", ",one", "one,,two", "one,   ,two"] {
            let args = super::DestroyArgs {
                name: Some(value.to_owned()),
                all: false,
                force: false,
            };
            assert!(args.cluster_names().is_err());
        }
    }

    #[test]
    fn remove_command_is_rejected() {
        assert!(Cli::from_iter_safe(["kalisto", "remove", "--name", "dev"]).is_err());
    }

    #[test]
    fn list_command_accepts_ls_alias() {
        let cli = Cli::from_iter_safe(["kalisto", "ls"]).expect("parse");
        match cli.command {
            Command::List => {}
            other => panic!("expected list command, got {other:?}"),
        }
    }

    #[test]
    fn kubeconfig_command_defaults_to_export_snippet() {
        let cli = Cli::from_iter_safe(["kalisto", "kubeconfig"]).expect("parse");
        match cli.command {
            Command::Kubeconfig(args) => {
                assert!(args.name.is_none());
                assert!(!args.path);
            }
            other => panic!("expected kubeconfig command, got {other:?}"),
        }
    }

    #[test]
    fn kubeconfig_command_accepts_path_mode_and_name() {
        let cli = Cli::from_iter_safe(["kalisto", "kubeconfig", "--name", "dev", "--path"])
            .expect("parse");
        match cli.command {
            Command::Kubeconfig(args) => {
                assert_eq!(args.name.as_deref(), Some("dev"));
                assert!(args.path);
            }
            other => panic!("expected kubeconfig command, got {other:?}"),
        }
    }
}

use std::path::PathBuf;

use structopt::StructOpt;

use crate::cluster::spec::ClusterSpec;
use crate::k3s::ServerMode;
use crate::paths::{default_cluster_name, default_project_root};

#[derive(Debug, StructOpt)]
#[structopt(name = "kalisto")]
pub struct Cli {
    #[structopt(subcommand)]
    pub command: Command,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub enum Command {
    Up(UpArgs),
    Down(ClusterLookupArgs),
    Remove(ClusterLookupArgs),
    Ls,
    Status(ClusterLookupArgs),
    Kubeconfig(ClusterLookupArgs),
    Bench(BenchArgs),
    ProbeMacosLima(MacosLimaArgs),
    ProbeMacosLimaK3s(MacosLimaK3sArgs),
}

#[derive(Debug, StructOpt)]
pub struct UpArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
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
}

impl UpArgs {
    pub fn into_cluster_spec(self) -> crate::error::Result<ClusterSpec> {
        let project_root = default_project_root(self.project_root)?;
        let name = self
            .name
            .unwrap_or(default_cluster_name(Some(&project_root))?);
        let mut spec = ClusterSpec::new(name, project_root);
        if let Some(template) = self.template {
            spec.template = template;
        }
        if let Some(workers) = self.workers {
            spec.worker_count = workers;
            if workers > 0 && spec.network == "vzNAT" {
                spec.network = "lima:user-v2".to_owned();
            }
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
        if self.agentless {
            spec.server_mode = ServerMode::Agentless;
            spec.worker_count = 0;
        }
        spec.enable_rosetta = !self.no_rosetta;
        Ok(spec)
    }
}

#[derive(Debug, StructOpt)]
pub struct ClusterLookupArgs {
    #[structopt(long)]
    pub name: Option<String>,
}

impl ClusterLookupArgs {
    pub fn cluster_name(&self) -> crate::error::Result<String> {
        match &self.name {
            Some(name) => Ok(name.clone()),
            None => default_cluster_name(None),
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct BenchArgs {
    #[structopt(long, parse(from_os_str))]
    pub project_root: Option<PathBuf>,
    #[structopt(long)]
    pub name: Option<String>,
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
    #[structopt(long, default_value = "3")]
    pub runs: usize,
}

impl BenchArgs {
    pub fn into_cluster_spec(self) -> crate::error::Result<(ClusterSpec, usize)> {
        let runs = self.runs;
        let up = UpArgs {
            project_root: self.project_root,
            name: self.name,
            template: self.template,
            workers: self.workers,
            network: self.network,
            api_port: self.api_port,
            k3s_version: self.k3s_version,
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

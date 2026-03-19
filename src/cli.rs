use std::path::PathBuf;

use structopt::StructOpt;

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
    Status(ClusterLookupArgs),
    Kubeconfig(ClusterLookupArgs),
    ProbeMacosLima(MacosLimaArgs),
    ProbeMacosLimaK3s(MacosLimaK3sArgs),
}

#[derive(Debug, StructOpt)]
pub struct UpArgs {
    #[structopt(long, parse(from_os_str))]
    pub workspace_root: PathBuf,
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

#[derive(Debug, StructOpt)]
pub struct ClusterLookupArgs {
    #[structopt(long, parse(from_os_str))]
    pub workspace_root: PathBuf,
    #[structopt(long)]
    pub name: Option<String>,
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

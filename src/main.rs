use std::process::ExitCode;

use structopt::StructOpt;

use kalisto::cli::{
    BenchArgs, Cli, ClusterLookupArgs, Command, MacosLimaArgs, MacosLimaK3sArgs, UpArgs,
};
use kalisto::cluster::manager::{
    benchmark as cluster_benchmark, down as cluster_down,
    kubeconfig_path as cluster_kubeconfig_path, status as cluster_status, up as cluster_up,
};
use kalisto::error::{Error, Result};
use kalisto::k3s::ServerMode as K3sServerMode;
use kalisto::probe::macos_lima::{
    ProbeConfig as MacosLimaProbeConfig, run as run_macos_lima_probe,
};
use kalisto::probe::macos_lima_k3s::{
    ProbeConfig as MacosLimaK3sProbeConfig, run as run_macos_lima_k3s_probe,
};
use kalisto::telemetry;

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    telemetry::init()?;
    match Cli::from_args().command {
        Command::Up(args) => run_up(args),
        Command::Down(args) => run_down(args),
        Command::Status(args) => run_status(args),
        Command::Kubeconfig(args) => run_kubeconfig(args),
        Command::Bench(args) => run_bench(args),
        Command::ProbeMacosLima(args) => run_lima_probe(args),
        Command::ProbeMacosLimaK3s(args) => run_lima_k3s_probe(args),
    }
}

fn run_up(args: UpArgs) -> Result<()> {
    let spec = args.into_cluster_spec();
    let outcome = cluster_up(&spec)?;
    println!("cluster: {}", outcome.state.name);
    println!("kubeconfig: {}", outcome.state.kubeconfig_path.display());
    println!("control-plane ready: {}", outcome.control_plane_ready);
    println!("cluster ready: {}", outcome.cluster_ready);
    Ok(())
}

fn run_down(args: ClusterLookupArgs) -> Result<()> {
    let name = args.cluster_name();
    cluster_down(&args.workspace_root, &name)?;
    println!("removed cluster `{name}`");
    Ok(())
}

fn run_status(args: ClusterLookupArgs) -> Result<()> {
    let name = args.cluster_name();
    let status = cluster_status(&args.workspace_root, &name)?;
    println!("cluster: {}", status.name);
    println!("state root: {}", status.state_root.display());
    println!("kubeconfig: {}", status.kubeconfig_path.display());
    for node in status.nodes {
        println!("{} ({:?}): {}", node.name, node.role, node.lima_status);
    }
    Ok(())
}

fn run_kubeconfig(args: ClusterLookupArgs) -> Result<()> {
    let name = args.cluster_name();
    println!(
        "{}",
        cluster_kubeconfig_path(&args.workspace_root, &name)?.display()
    );
    Ok(())
}

fn run_bench(args: BenchArgs) -> Result<()> {
    let (spec, runs) = args.into_cluster_spec();
    let samples = cluster_benchmark(&spec, runs)?;
    for sample in samples {
        println!(
            "run {}: cluster={} control-plane-ready={} cluster-ready={}",
            sample.run, sample.cluster_name, sample.control_plane_ready, sample.cluster_ready
        );
    }
    Ok(())
}

fn run_lima_probe(args: MacosLimaArgs) -> Result<()> {
    let mut config = MacosLimaProbeConfig::new(args.workspace_root);
    config.keep_resources = args.keep_resources;
    config.enable_rosetta = !args.no_rosetta;
    if let Some(template) = args.template {
        config.template = template;
    }
    if let Some(network) = args.network {
        config.network = network;
    }

    let report = run_macos_lima_probe(&config);
    for result in &report.results {
        println!("[{}] {}: {}", result.status, result.name, result.detail);
    }
    println!("{}", report.summary());

    if report.has_failures() {
        Err(Error::InvalidArgs(
            "macOS Lima probe reported one or more failures".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn run_lima_k3s_probe(args: MacosLimaK3sArgs) -> Result<()> {
    let mut config = MacosLimaK3sProbeConfig::new(args.workspace_root);
    config.keep_resources = args.keep_resources;
    config.server_mode = if args.agentless {
        K3sServerMode::Agentless
    } else {
        K3sServerMode::Standard
    };
    config.enable_rosetta = !args.no_rosetta;
    if let Some(api_port) = args.api_port {
        config.api_port = api_port;
    }
    if let Some(k3s_version) = args.k3s_version {
        config.k3s_version = k3s_version;
    }
    if let Some(template) = args.template {
        config.template = template;
    }
    if let Some(worker_count) = args.workers {
        config.worker_count = worker_count;
    }
    if let Some(network) = args.network {
        config.network = Some(network);
    }

    let report = run_macos_lima_k3s_probe(&config);
    for result in &report.results {
        println!("[{}] {}: {}", result.status, result.name, result.detail);
    }
    println!("{}", report.summary());

    if report.has_failures() {
        Err(Error::InvalidArgs(
            "macOS Lima k3s probe reported one or more failures".to_owned(),
        ))
    } else {
        Ok(())
    }
}

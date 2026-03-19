use std::process::ExitCode;

use structopt::StructOpt;

use kalisto::cli::{Cli, Command, MacosLimaArgs, MacosLimaK3sArgs};
use kalisto::error::{Error, Result};
use kalisto::k3s::ServerMode as K3sServerMode;
use kalisto::probe::macos_lima::{
    ProbeConfig as MacosLimaProbeConfig, run as run_macos_lima_probe,
};
use kalisto::probe::macos_lima_k3s::{
    ProbeConfig as MacosLimaK3sProbeConfig, run as run_macos_lima_k3s_probe,
};

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
    match Cli::from_args().command {
        Command::ProbeMacosLima(args) => run_lima_probe(args),
        Command::ProbeMacosLimaK3s(args) => run_lima_k3s_probe(args),
    }
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

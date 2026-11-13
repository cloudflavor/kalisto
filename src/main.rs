use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use serde_json::json;
use structopt::StructOpt;

use kalisto::cli::{
    AddonArgs, AddonCommand, AddonInstallArgs, AddonListArgs, ApplyArgs, BenchArgs, Cli,
    ClusterLookupArgs, Command, CompletionsArgs, DestroyArgs, KubeconfigArgs, KubectlArgs,
    MacosLimaArgs, MacosLimaK3sArgs, McpArgs, McpTaskRunnerArgs, NamedClusterArgs, NodeLogsArgs,
    ScaleArgs, StatsArgs, TestArgs, TestCommand, TestListArgs, TestRunArgs, UpArgs,
    apply_requested_addons,
};
use kalisto::cluster::addon::CniPlugin;
use kalisto::cluster::manager::{
    benchmark as cluster_benchmark, create as cluster_create, defined_cluster_names,
    destroy as cluster_destroy, down as cluster_down, install_addon as cluster_install_addon,
    kubeconfig_path as cluster_kubeconfig_path, list as cluster_list, logs as cluster_logs,
    scale as cluster_scale, stats as cluster_stats, status as cluster_status, up as cluster_up,
};
use kalisto::cluster::state::ClusterState;
use kalisto::config::load_project_config;
use kalisto::error::{Error, Result};
use kalisto::harness::scenario::ScenarioClusterLifecycle;
use kalisto::harness::{ScenarioConfig, run_scenario};
use kalisto::k3s::ServerMode as K3sServerMode;
use kalisto::mcp::{run as run_mcp_server, run_task_runner as run_mcp_task_runner};
use kalisto::paths::{
    clear_default_cluster_name_if_matches, handoff_default_cluster_file_to_invoking_user,
    read_default_cluster_name, write_default_cluster_name,
};
use kalisto::plugin::{PluginSpec, built_in_catalog};
use kalisto::probe::macos_lima::{
    ProbeConfig as MacosLimaProbeConfig, run as run_macos_lima_probe,
};
use kalisto::probe::macos_lima_k3s::{
    ProbeConfig as MacosLimaK3sProbeConfig, run as run_macos_lima_k3s_probe,
};
use kalisto::process::{CommandRunner, SystemRunner};
use kalisto::shell::shell_escape;
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
    let matches = Cli::clap()
        .version(kalisto::version::build_version())
        .get_matches();
    let cli = Cli::from_clap(&matches);
    telemetry::init(cli.log_level)?;
    match cli.command {
        Command::Create(args) => run_create(args),
        Command::Pull(args) => run_pull(args),
        Command::Export(args) => {
            let encryption = args.encryption()?;
            let result = kalisto::archive::export_encrypted(
                &args.name,
                &args.output,
                args.stop,
                &encryption,
            )?;
            println!(
                "cluster: {}\narchive: {}\nnodes: {}\nbytes: {}\nencrypted: {}\ncontains secrets: true",
                result.name,
                result.path.display(),
                result.nodes,
                result.bytes,
                result.encrypted
            );
            Ok(())
        }
        Command::Import(args) => {
            let credentials = args.decryption()?;
            let result = kalisto::archive::import_encrypted(
                &args.input,
                args.api_port,
                args.max_bytes,
                &credentials,
            )?;
            println!(
                "cluster: {}\nnodes: {}\nstate: stopped\nRun kalisto up --name {} to boot restored nodes",
                result.name, result.nodes, result.name
            );
            Ok(())
        }
        Command::ArchiveKeygen(args) => {
            let public = kalisto::archive::generate_identity(&args.output)?;
            println!(
                "recipient: {public}\nprivate key file: {}",
                args.output.display()
            );
            Ok(())
        }
        Command::Up(args) => run_up(args),
        Command::Test(args) => run_test(args),
        Command::Mcp(args) => run_mcp(args),
        Command::McpTaskRunner(args) => run_hidden_mcp_task_runner(args),
        Command::Apply(args) => run_apply(args),
        Command::Down(args) => run_down(args),
        Command::Destroy(args) => run_destroy(args),
        Command::Logs(args) => run_logs(args),
        Command::Addon(args) => run_addon(args),
        Command::Plugin(args) => run_addon(args),
        Command::Use(args) => run_use(args),
        Command::Scale(args) => run_scale(args),
        Command::Completions(args) => run_completions(args),
        Command::Kubectl(args) => run_kubectl(args),
        Command::List => run_list(cli.json),
        Command::Stats(args) => run_stats(args, cli.json),
        Command::Status(args) => run_status(args, cli.json),
        Command::Kubeconfig(args) => run_kubeconfig(args, cli.json),
        Command::Bench(args) => run_bench(args),
        Command::ProbeMacosLima(args) => run_lima_probe(args),
        Command::ProbeMacosLimaK3s(args) => run_lima_k3s_probe(args),
    }
}

fn run_test(args: TestArgs) -> Result<()> {
    match args.command {
        TestCommand::Run(args) => run_test_run(args),
        TestCommand::Ls(args) => run_test_list(args),
    }
}

fn run_mcp(args: McpArgs) -> Result<()> {
    run_mcp_server(args.project_root)
}

fn run_hidden_mcp_task_runner(args: McpTaskRunnerArgs) -> Result<()> {
    run_mcp_task_runner(args.manifest, args.exit_path)
}

fn run_create(args: UpArgs) -> Result<()> {
    let spec = args.into_cluster_spec()?;
    let outcome = cluster_create(&spec)?;
    if read_default_cluster_name()?.is_none() {
        write_default_cluster_name(&outcome.state.name)?;
        handoff_default_cluster_file_to_invoking_user(&SystemRunner)?;
    }
    let state_root = kalisto::cluster::spec::ClusterSpec::state_root_for(&outcome.state.name)?;
    println!("cluster: {}", outcome.state.name);
    println!("state root: {}", state_root.display());
    println!("runtime root: {}", spec.runtime_root().display());
    println!("created cluster definition");
    Ok(())
}

fn run_pull(args: UpArgs) -> Result<()> {
    let spec = args.into_cluster_spec()?;
    let assets = kalisto::pull::prepare(&spec)?;
    println!("prepared assets: {}", assets.root.display());
    println!("probe image: {}", assets.probe_image());
    Ok(())
}

fn run_up(args: UpArgs) -> Result<()> {
    let spec = args.into_cluster_spec()?;
    let outcome = cluster_up(&spec)?;
    if read_default_cluster_name()?.is_none() {
        write_default_cluster_name(&outcome.state.name)?;
        handoff_default_cluster_file_to_invoking_user(&SystemRunner)?;
    }
    println!("cluster: {}", outcome.state.name);
    println!("kubeconfig: {}", outcome.state.kubeconfig_path.display());
    println!("control-plane ready: {}", outcome.control_plane_ready);
    println!("cluster ready: {}", outcome.cluster_ready);
    Ok(())
}

fn run_test_list(args: TestListArgs) -> Result<()> {
    let project_root = kalisto::paths::default_project_root(args.project_root)?;
    let config = load_project_config(&project_root)?.ok_or_else(|| {
        Error::InvalidArgs(format!(
            "no kalisto.toml found under {}",
            project_root.display()
        ))
    })?;
    if config.harness.scenarios.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "{} does not define any harness scenarios",
            project_root.join("kalisto.toml").display()
        )));
    }

    for scenario in &config.harness.scenarios {
        println!(
            "{}\t{}\t{}\t{}",
            scenario.name,
            scenario.namespace,
            match scenario.cluster_lifecycle {
                ScenarioClusterLifecycle::Ephemeral => "ephemeral",
                ScenarioClusterLifecycle::Persistent => "persistent",
            },
            describe_scenario(scenario)
        );
    }
    Ok(())
}

fn run_test_run(args: TestRunArgs) -> Result<()> {
    let mut spec = args.into_cluster_spec()?;
    let project_root = spec.workspace_root.clone();
    let config = load_project_config(&project_root)?.ok_or_else(|| {
        Error::InvalidArgs(format!(
            "no kalisto.toml found under {}",
            project_root.display()
        ))
    })?;
    let scenario = config
        .harness
        .scenario(&args.scenario)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "harness scenario `{}` was not found in {}",
                args.scenario,
                project_root.join("kalisto.toml").display()
            ))
        })?;

    apply_harness_cluster_settings(&mut spec, &scenario)?;
    let lifecycle = scenario.cluster_lifecycle;
    let cluster_name = spec.name.clone();
    let outcome = cluster_up(&spec)?;
    let run_result = run_scenario(&outcome.state, &scenario, &SystemRunner);
    let report = match run_result {
        Ok(report) => report,
        Err(error) => {
            if should_preserve_cluster_after_scenario_failure(lifecycle) {
                eprintln!(
                    "scenario failed; preserving cluster `{}` for inspection",
                    cluster_name
                );
            }
            return Err(error);
        }
    };
    if should_destroy_cluster_after_scenario_success(lifecycle) {
        cluster_destroy(&cluster_name)?;
    }

    println!("scenario: {}", report.scenario);
    println!("cluster: {}", report.cluster);
    println!("namespace: {}", report.namespace);
    println!("manifests applied: {}", report.applied_manifests.len());
    println!("waits passed: {}", report.waits.len());
    println!("probes passed: {}", report.probes.len());
    println!(
        "cluster lifecycle: {}",
        match lifecycle {
            ScenarioClusterLifecycle::Ephemeral => "ephemeral",
            ScenarioClusterLifecycle::Persistent => "persistent",
        }
    );
    Ok(())
}

fn should_destroy_cluster_after_scenario_success(lifecycle: ScenarioClusterLifecycle) -> bool {
    lifecycle == ScenarioClusterLifecycle::Ephemeral
}

fn should_preserve_cluster_after_scenario_failure(lifecycle: ScenarioClusterLifecycle) -> bool {
    lifecycle == ScenarioClusterLifecycle::Ephemeral
}

fn apply_harness_cluster_settings(
    spec: &mut kalisto::cluster::spec::ClusterSpec,
    scenario: &ScenarioConfig,
) -> Result<()> {
    apply_requested_addons(spec, &scenario.cluster.addons)?;
    spec.worker_count = scenario.cluster.workers;
    spec.requested_worker_count = Some(scenario.cluster.workers);
    spec.server_mode = if scenario.cluster.agentless {
        K3sServerMode::Agentless
    } else {
        K3sServerMode::Standard
    };
    if scenario.cluster.agentless {
        spec.worker_count = 0;
        spec.requested_worker_count = Some(0);
    }
    Ok(())
}

fn run_down(args: ClusterLookupArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    cluster_down(&name, args.force)?;
    println!("stopped cluster `{name}`");
    Ok(())
}

fn run_destroy(args: DestroyArgs) -> Result<()> {
    let names = if args.all {
        defined_cluster_names()?
    } else {
        match args.cluster_names()? {
            Some(names) => names,
            None => vec![require_selected_cluster_name(None)?],
        }
    };
    if names.is_empty() {
        println!("no clusters");
        return Ok(());
    }
    destroy_cluster_batch(&names, |name| {
        cluster_destroy(name)?;
        clear_default_cluster_name_if_matches(name)
    })
}

fn destroy_cluster_batch(
    names: &[String],
    mut destroy: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut failures = Vec::new();
    for name in names {
        match destroy(name) {
            Ok(()) => println!("destroyed cluster `{name}`"),
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidArgs(format!(
            "failed to destroy {} cluster(s): {}",
            failures.len(),
            failures.join("; "),
        )))
    }
}

fn describe_scenario(scenario: &ScenarioConfig) -> String {
    format!(
        "{} manifest(s), {} wait(s), {} probe(s)",
        scenario.manifests.len(),
        scenario.waits.len(),
        scenario.probes.len()
    )
}

fn run_addon(args: AddonArgs) -> Result<()> {
    match args.command {
        AddonCommand::Install(args) => run_addon_install(args),
        AddonCommand::Ls(args) => run_addon_list(args),
    }
}

fn run_addon_install(args: AddonInstallArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let outcome = cluster_install_addon(&name, &args.addon)?;
    println!("cluster: {}", outcome.cluster);
    println!("installed add-on: {}", outcome.addon);
    Ok(())
}

fn run_addon_list(args: AddonListArgs) -> Result<()> {
    let selected_cluster = args.cluster_name()?;
    let cluster_state = selected_cluster
        .as_deref()
        .map(load_cluster_state_if_present)
        .transpose()?;
    let catalog = built_in_catalog()?;

    if let Some(state) = &cluster_state {
        println!("cluster: {}", state.name);
        println!();
    }

    let mut rows = vec![vec![
        "PLUGIN".to_owned(),
        "CATEGORY".to_owned(),
        "INSTALL".to_owned(),
        "STATUS".to_owned(),
        "CONFLICTS".to_owned(),
        "DESCRIPTION".to_owned(),
    ]];
    for plugin in catalog {
        rows.push(vec![
            plugin.id.clone(),
            plugin.category.clone(),
            plugin.install_kind.clone(),
            plugin_status(cluster_state.as_ref(), &plugin),
            render_conflicts(&plugin.conflicts),
            plugin.description.clone(),
        ]);
    }

    print_table(&rows);
    Ok(())
}

fn run_logs(args: NodeLogsArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let logs = cluster_logs(&name, &args.node)?;
    println!("cluster: {}", logs.cluster);
    println!("node: {}", logs.node);
    println!();
    for (index, section) in logs.sections.iter().enumerate() {
        println!("== {} ({}) ==", section.label, section.path.display());
        print!("{}", section.content);
        if !section.content.ends_with('\n') {
            println!();
        }
        if index + 1 != logs.sections.len() {
            println!();
        }
    }
    if args.follow {
        follow_log_sections(&logs.sections)?;
    }
    Ok(())
}

fn run_use(args: NamedClusterArgs) -> Result<()> {
    let clusters = cluster_list()?;
    if !clusters.iter().any(|cluster| cluster.name == args.name) {
        return Err(Error::InvalidArgs(format!(
            "cluster `{}` is not defined",
            args.name
        )));
    }
    write_default_cluster_name(&args.name)?;
    handoff_default_cluster_file_to_invoking_user(&SystemRunner)?;
    println!("default cluster: {}", args.name);
    Ok(())
}

fn run_scale(args: ScaleArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let outcome = cluster_scale(&name, args.workers)?;
    println!("cluster: {}", outcome.name);
    println!(
        "workers: {} -> {}",
        outcome.previous_workers, outcome.current_workers
    );
    if outcome.added_workers.is_empty() {
        println!("added nodes: none");
    } else {
        println!("added nodes: {}", outcome.added_workers.join(", "));
    }
    Ok(())
}

fn run_completions(args: CompletionsArgs) -> Result<()> {
    Cli::clap().gen_completions_to("kalisto", args.shell, &mut std::io::stdout());
    Ok(())
}

fn json_error(error: serde_json::Error) -> Error {
    Error::InvalidArgs(format!("failed to serialize JSON output: {error}"))
}

fn run_list(json: bool) -> Result<()> {
    let clusters = cluster_list()?;
    let default_cluster = read_default_cluster_name()?;
    if json {
        let value = json!({
            "defaultCluster": default_cluster,
            "clusters": clusters.iter().map(|cluster| json!({
                "name": cluster.name,
                "isDefault": default_cluster.as_deref() == Some(cluster.name.as_str()),
                "state": cluster_state_label(cluster),
                "nodeCount": cluster.nodes.len(),
                "kubeconfigPath": cluster.kubeconfig_path.display().to_string(),
                "nodes": cluster.nodes.iter().map(|node| json!({
                    "name": node.name,
                    "role": role_label(node.role),
                    "state": render_node_state(&node.backend_status),
                    "detail": node.backend_status,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(json_error)?
        );
        return Ok(());
    }
    if clusters.is_empty() {
        println!("no clusters");
        return Ok(());
    }

    let mut cluster_rows = Vec::with_capacity(clusters.len() + 1);
    cluster_rows.push(vec![
        "CLUSTER".to_owned(),
        "DEFAULT".to_owned(),
        "STATE".to_owned(),
        "NODES".to_owned(),
        "KUBECONFIG".to_owned(),
    ]);

    let mut node_rows = vec![vec![
        "CLUSTER".to_owned(),
        "NODE".to_owned(),
        "ROLE".to_owned(),
        "STATE".to_owned(),
        "DETAIL".to_owned(),
    ]];

    for cluster in &clusters {
        cluster_rows.push(vec![
            cluster.name.clone(),
            if default_cluster.as_deref() == Some(cluster.name.as_str()) {
                "yes".to_owned()
            } else {
                String::new()
            },
            cluster_state_label(cluster),
            cluster.nodes.len().to_string(),
            cluster.kubeconfig_path.display().to_string(),
        ]);
        for node in &cluster.nodes {
            node_rows.push(vec![
                cluster.name.clone(),
                node.name.clone(),
                role_label(node.role).to_owned(),
                render_node_state(&node.backend_status),
                node.backend_status.clone(),
            ]);
        }
    }

    print_table(&cluster_rows);
    println!();
    print_table(&node_rows);
    Ok(())
}

fn run_status(args: ClusterLookupArgs, json: bool) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let status = cluster_status(&name)?;
    if json {
        let value = json!({
            "name": status.name,
            "stateRoot": status.state_root.display().to_string(),
            "kubeconfigPath": status.kubeconfig_path.display().to_string(),
            "nodes": status.nodes.iter().map(|node| json!({
                "name": node.name,
                "role": role_label(node.role),
                "state": render_node_state(&node.backend_status),
                "detail": node.backend_status,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(json_error)?
        );
        return Ok(());
    }
    println!("cluster: {}", status.name);
    println!("state root: {}", status.state_root.display());
    println!("kubeconfig: {}", status.kubeconfig_path.display());
    println!();
    let mut rows = vec![vec![
        "NODE".to_owned(),
        "ROLE".to_owned(),
        "STATE".to_owned(),
        "DETAIL".to_owned(),
    ]];
    for node in status.nodes {
        rows.push(vec![
            node.name,
            role_label(node.role).to_owned(),
            render_node_state(&node.backend_status),
            node.backend_status,
        ]);
    }
    print_table(&rows);
    Ok(())
}

fn run_stats(args: StatsArgs, json: bool) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    if json && args.watch {
        return Err(Error::InvalidArgs(
            "--json does not support --watch; run stats repeatedly instead".to_owned(),
        ));
    }
    let interval = Duration::from_secs(args.interval_secs.max(1));
    if json {
        let stats = cluster_stats(&name)?;
        let value = json!({
            "name": stats.name,
            "stateRoot": stats.state_root.display().to_string(),
            "kubeconfigPath": stats.kubeconfig_path.display().to_string(),
            "nodes": stats.nodes.iter().map(|node| json!({
                "name": node.name,
                "role": role_label(node.role),
                "state": node_state_label(&node.backend_status),
                "backendStatus": node.backend_status,
                "configuredCpus": node.configured_cpus,
                "configuredMemoryGib": node.configured_memory_gib,
                "pid": node.pid,
                "guestIp": node.guest_ip,
                "rssBytes": node.rss_bytes,
                "vszBytes": node.vsz_bytes,
                "rootfsBytes": node.rootfs_bytes,
                "rxBytes": node.rx_bytes,
                "txBytes": node.tx_bytes,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(json_error)?
        );
        return Ok(());
    }

    if args.watch {
        loop {
            let stats = cluster_stats(&name)?;
            print!("\x1b[2J\x1b[H");
            println!("{}", render_stats(&stats));
            thread::sleep(interval);
        }
    } else {
        let stats = cluster_stats(&name)?;
        print!("{}", render_stats(&stats));
        Ok(())
    }
}

fn run_kubeconfig(args: KubeconfigArgs, json: bool) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let path = cluster_kubeconfig_path(&name)?;
    if json {
        let value = json!({
            "name": name,
            "path": path.display().to_string(),
            "exportCommand": kubeconfig_export_command(&path),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(json_error)?
        );
        return Ok(());
    }
    if args.path {
        println!("{}", path.display());
    } else {
        println!("{}", kubeconfig_export_command(&path));
    }
    Ok(())
}

fn run_apply(args: ApplyArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let manifest = std::fs::read_to_string(&args.manifest).map_err(|source| Error::CommandIo {
        program: "read_to_string".to_owned(),
        source,
    })?;
    let outcome = kalisto::cluster::workload::apply_manifest(&name, &manifest, &SystemRunner)?;
    println!("cluster: {}", outcome.cluster);
    println!("node: {}", outcome.node);
    println!("applied: {}", args.manifest.display());
    println!("{}", outcome.output);
    Ok(())
}

fn run_kubectl(args: KubectlArgs) -> Result<()> {
    let name = require_selected_cluster_name(args.cluster_name()?)?;
    let state_root = kalisto::cluster::spec::ClusterSpec::state_root_for(&name)?;
    let state = ClusterState::load(state_root.as_path())?;
    let runner = SystemRunner;
    let binary = kalisto::kubectl::binary(&state.k3s_version, &runner)?;
    if args.path {
        println!("{}", binary.display());
        return Ok(());
    }
    let mut kubectl_args = args.args.as_slice();
    if kubectl_args
        .first()
        .map(|argument| argument == "--")
        .unwrap_or(false)
    {
        kubectl_args = &kubectl_args[1..];
    }
    if kubectl_args.is_empty() {
        return Err(Error::InvalidArgs(
            "no kubectl arguments provided; pass them after `kalisto kubectl --name <cluster> --`"
                .to_owned(),
        ));
    }
    let program = binary.to_string_lossy().to_string();
    let output = runner.run_with_env(
        &program,
        kubectl_args,
        &[("KUBECONFIG", &state.kubeconfig_path)],
    )?;
    print!("{}", output.stdout);
    print!("{}", output.stderr);
    Ok(())
}

fn kubeconfig_export_command(path: &std::path::Path) -> String {
    format!(
        "export KUBECONFIG={}",
        shell_escape(path.as_os_str().to_string_lossy().as_ref())
    )
}

fn run_bench(args: BenchArgs) -> Result<()> {
    let (spec, runs) = args.into_cluster_spec()?;
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

fn print_table(rows: &[Vec<String>]) {
    print!("{}", render_table(rows));
}

fn render_stats(stats: &kalisto::cluster::manager::ClusterStats) -> String {
    let mut output = String::new();
    output.push_str(&format!("cluster: {}\n", stats.name));
    output.push_str(&format!("state root: {}\n", stats.state_root.display()));
    output.push_str(&format!(
        "kubeconfig: {}\n\n",
        stats.kubeconfig_path.display()
    ));

    let mut rows = vec![vec![
        "NODE".to_owned(),
        "ROLE".to_owned(),
        "STATE".to_owned(),
        "CPU".to_owned(),
        "MEM".to_owned(),
        "PID".to_owned(),
        "GUEST IP".to_owned(),
        "RSS".to_owned(),
        "VSZ".to_owned(),
        "ROOTFS".to_owned(),
        "RX".to_owned(),
        "TX".to_owned(),
    ]];
    for node in &stats.nodes {
        rows.push(vec![
            node.name.clone(),
            role_label(node.role).to_owned(),
            node_state_label(&node.backend_status),
            node.configured_cpus.to_string(),
            format!("{} GiB", node.configured_memory_gib),
            optional_u32(node.pid),
            optional_string(node.guest_ip.clone()),
            optional_bytes(node.rss_bytes),
            optional_bytes(node.vsz_bytes),
            optional_bytes(node.rootfs_bytes),
            optional_bytes(node.rx_bytes),
            optional_bytes(node.tx_bytes),
        ]);
    }
    output.push_str(&render_table(&rows));
    output
}

fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    if column_count == 0 {
        return String::new();
    }

    let widths: Vec<usize> = (0..column_count)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|value| visible_width(value))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut output = String::new();
    for (index, row) in rows.iter().enumerate() {
        for (column, width) in widths.iter().enumerate().take(column_count) {
            if column > 0 {
                output.push_str("  ");
            }
            let value = row.get(column).map(String::as_str).unwrap_or("");
            output.push_str(value);
            let value_width = visible_width(value);
            if *width > value_width {
                output.push_str(&" ".repeat(*width - value_width));
            }
        }
        output.push('\n');

        if index == 0 {
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    output.push_str("  ");
                }
                output.push_str(&format!("{:-<width$}", "", width = *width));
            }
            output.push('\n');
        }
    }

    output
}

fn follow_log_sections(sections: &[kalisto::cluster::manager::NodeLogSection]) -> Result<()> {
    let mut followers: Vec<LogFollower> = sections
        .iter()
        .map(|section| LogFollower {
            label: section.label.clone(),
            path: section.path.clone(),
            offset: section.content.len() as u64,
        })
        .collect();

    loop {
        for follower in &mut followers {
            follower.emit_pending()?;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

struct LogFollower {
    label: String,
    path: std::path::PathBuf,
    offset: u64,
}

impl LogFollower {
    fn emit_pending(&mut self) -> Result<()> {
        if !self.path.is_file() {
            return Ok(());
        }

        let mut file = File::open(&self.path).map_err(|source| Error::CommandIo {
            program: "open".to_owned(),
            source,
        })?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| Error::CommandIo {
                program: "seek".to_owned(),
                source,
            })?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::CommandIo {
                program: "read_to_end".to_owned(),
                source,
            })?;
        if bytes.is_empty() {
            return Ok(());
        }

        self.offset += bytes.len() as u64;
        let text = String::from_utf8(bytes)?;
        for line in text.lines() {
            println!("[{}] {}", self.label, line);
        }
        if !text.ends_with('\n') {
            print!("[{}] ", self.label);
        }
        Ok(())
    }
}

fn cluster_state_label(status: &kalisto::cluster::manager::ClusterStatus) -> String {
    let mut has_up = false;
    let mut has_down = false;
    let mut has_failed = false;

    for node in &status.nodes {
        match node_health(&node.backend_status) {
            NodeHealth::Up => has_up = true,
            NodeHealth::Down => has_down = true,
            NodeHealth::Failed => has_failed = true,
            NodeHealth::Unknown => {}
        }
    }

    let (label, color) = if has_failed {
        if has_up {
            ("degraded", ColorStyle::Yellow)
        } else {
            ("failed", ColorStyle::Red)
        }
    } else if has_up {
        ("up", ColorStyle::Green)
    } else if has_down {
        ("down", ColorStyle::Yellow)
    } else {
        ("unknown", ColorStyle::Dim)
    };

    colorize(label, color)
}

fn role_label(role: kalisto::k3s::NodeRole) -> &'static str {
    match role {
        kalisto::k3s::NodeRole::ControlPlane => "control-plane",
        kalisto::k3s::NodeRole::Worker => "worker",
    }
}

fn require_selected_cluster_name(name: Option<String>) -> Result<String> {
    name.ok_or_else(|| {
        Error::InvalidArgs(
            "no default cluster selected; use --name or `kalisto use <name>`".to_owned(),
        )
    })
}

fn load_cluster_state_if_present(name: &str) -> Result<ClusterState> {
    let state_root = kalisto::cluster::spec::ClusterSpec::state_root_for(name)?;
    let state_file = ClusterState::state_file(&state_root);
    if !state_file.is_file() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{name}` not found at {}",
            state_root.display()
        )));
    }
    ClusterState::load(&state_root)
}

fn plugin_status(state: Option<&ClusterState>, plugin: &PluginSpec) -> String {
    let Some(state) = state else {
        return "available".to_owned();
    };

    match plugin.id.as_str() {
        "cni/flannel" if state.cni_plugin == CniPlugin::Flannel => "installed".to_owned(),
        "cni/cilium" if state.cni_plugin == CniPlugin::Cilium => "installed".to_owned(),
        "lb/servicelb" if !state.component_toggles.disable_servicelb => "installed".to_owned(),
        _ if state.addons.iter().any(|installed| installed == &plugin.id) => "installed".to_owned(),
        _ => "available".to_owned(),
    }
}

fn render_conflicts(conflicts: &[String]) -> String {
    if conflicts.is_empty() {
        String::new()
    } else {
        conflicts.join(", ")
    }
}

fn node_state_label(backend_status: &str) -> String {
    match node_health(backend_status) {
        NodeHealth::Up => "up".to_owned(),
        NodeHealth::Down => "down".to_owned(),
        NodeHealth::Failed => "failed".to_owned(),
        NodeHealth::Unknown => "unknown".to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeBackendState {
    Running,
    Exited,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeHealth {
    Up,
    Down,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorStyle {
    Green,
    Yellow,
    Red,
    Dim,
}

fn backend_status_state(status: &str) -> Option<NodeBackendState> {
    for token in status.split_whitespace() {
        match token {
            "Running" => return Some(NodeBackendState::Running),
            "Exited" => return Some(NodeBackendState::Exited),
            "Stopped" => return Some(NodeBackendState::Stopped),
            _ => {}
        }
    }
    None
}

fn node_health(status: &str) -> NodeHealth {
    match backend_status_state(status) {
        Some(NodeBackendState::Running) => NodeHealth::Up,
        Some(NodeBackendState::Stopped) => NodeHealth::Down,
        Some(NodeBackendState::Exited) => NodeHealth::Failed,
        None => NodeHealth::Unknown,
    }
}

fn render_node_state(status: &str) -> String {
    match node_health(status) {
        NodeHealth::Up => colorize("up", ColorStyle::Green),
        NodeHealth::Down => colorize("down", ColorStyle::Yellow),
        NodeHealth::Failed => colorize("failed", ColorStyle::Red),
        NodeHealth::Unknown => colorize("unknown", ColorStyle::Dim),
    }
}

fn colorize(value: &str, style: ColorStyle) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return value.to_owned();
    }

    let code = match style {
        ColorStyle::Green => "32",
        ColorStyle::Yellow => "33",
        ColorStyle::Red => "31",
        ColorStyle::Dim => "2",
    };
    format!("\x1b[{code}m{value}\x1b[0m")
}

fn visible_width(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut index = 0usize;
    let mut width = 0usize;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b'm' {
                        break;
                    }
                }
                continue;
            }
        }

        let rest = &value[index..];
        if let Some(ch) = rest.chars().next() {
            width += 1;
            index += ch.len_utf8();
        } else {
            break;
        }
    }

    width
}

fn optional_u32(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn optional_string(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}

fn optional_bytes(value: Option<u64>) -> String {
    value.map(format_bytes).unwrap_or_else(|| "-".to_owned())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit_index = 0;
    let mut value = bytes as f64;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{bytes} {}", UNITS[unit_index])
    } else {
        format!("{value:.1} {}", UNITS[unit_index])
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kalisto::backend::BackendKind;
    use kalisto::cluster::addon::{CniPlugin, MetalLbConfig};
    use kalisto::cluster::spec::NodeResources;
    use kalisto::cluster::state::ClusterState;
    use kalisto::k3s::{ComponentToggles, NodeRole, ServerMode};
    use kalisto::plugin::PluginSpec;

    use super::{
        kubeconfig_export_command, plugin_status, render_conflicts, require_selected_cluster_name,
        should_destroy_cluster_after_scenario_success,
        should_preserve_cluster_after_scenario_failure,
    };

    #[test]
    fn destroy_batch_attempts_every_name_and_reports_failures() {
        let names = vec!["one".to_owned(), "broken".to_owned(), "two".to_owned()];
        let mut attempted = Vec::new();
        let result = super::destroy_cluster_batch(&names, |name| {
            attempted.push(name.to_owned());
            if name == "broken" {
                Err(kalisto::error::Error::InvalidArgs(
                    "backend unavailable".to_owned(),
                ))
            } else {
                Ok(())
            }
        });
        assert_eq!(attempted, names);
        assert!(result.unwrap_err().to_string().contains("broken"));
    }

    #[test]
    fn plugin_status_marks_selected_cni_as_installed() {
        let mut state = fixture_cluster_state();
        state.cni_plugin = CniPlugin::Cilium;

        let plugin = PluginSpec {
            id: "cni/cilium".to_owned(),
            category: "cni".to_owned(),
            install_kind: "manifest".to_owned(),
            conflicts: vec!["cni/flannel".to_owned()],
            description: "Cilium networking".to_owned(),
        };

        assert_eq!(plugin_status(Some(&state), &plugin), "installed");
    }

    #[test]
    fn render_conflicts_joins_entries() {
        assert_eq!(
            render_conflicts(&["cni/flannel".to_owned(), "lb/servicelb".to_owned()]),
            "cni/flannel, lb/servicelb"
        );
    }

    #[test]
    fn kubeconfig_export_command_escapes_path_for_shell() {
        let command = kubeconfig_export_command(std::path::Path::new("/tmp/kube'config.yaml"));
        assert_eq!(command, "export KUBECONFIG='/tmp/kube'\"'\"'config.yaml'");
    }

    #[test]
    fn require_selected_cluster_name_rejects_none() {
        let error = require_selected_cluster_name(None).expect_err("none should fail");
        assert!(error.to_string().contains("no default cluster selected"));
    }

    #[test]
    fn ephemeral_scenarios_destroy_on_success_only() {
        assert!(should_destroy_cluster_after_scenario_success(
            super::ScenarioClusterLifecycle::Ephemeral
        ));
        assert!(!should_destroy_cluster_after_scenario_success(
            super::ScenarioClusterLifecycle::Persistent
        ));
        assert!(should_preserve_cluster_after_scenario_failure(
            super::ScenarioClusterLifecycle::Ephemeral
        ));
        assert!(!should_preserve_cluster_after_scenario_failure(
            super::ScenarioClusterLifecycle::Persistent
        ));
    }

    #[test]
    fn harness_cluster_settings_default_to_flannel_and_zero_workers() {
        let mut spec = kalisto::cluster::spec::ClusterSpec::new(
            "smoke".to_owned(),
            PathBuf::from("/tmp/workspace"),
        );
        let scenario = super::ScenarioConfig {
            name: "smoke".to_owned(),
            namespace: "default".to_owned(),
            cluster_lifecycle: super::ScenarioClusterLifecycle::Ephemeral,
            cluster: kalisto::harness::ScenarioClusterConfig::default(),
            manifests: Vec::new(),
            waits: Vec::new(),
            probes: vec![kalisto::harness::ProbeConfig::Command(
                kalisto::harness::CommandProbeConfig {
                    name: "pods".to_owned(),
                    command: "kubectl get pods".to_owned(),
                },
            )],
        };

        super::apply_harness_cluster_settings(&mut spec, &scenario).unwrap();

        assert_eq!(spec.cni_plugin, CniPlugin::Flannel);
        assert_eq!(spec.worker_count, 0);
        assert_eq!(spec.requested_worker_count, Some(0));
        assert_eq!(spec.server_mode, ServerMode::Standard);
        assert!(spec.component_toggles.flannel_backend_none);
        assert_eq!(scenario.cluster.addons, vec!["cni/flannel".to_owned()]);
    }

    fn fixture_cluster_state() -> ClusterState {
        let mut component_toggles = ComponentToggles::default();
        CniPlugin::None.apply_component_defaults(&mut component_toggles);

        ClusterState {
            name: "dev".to_owned(),
            backend: BackendKind::Lima,
            workspace_root: PathBuf::from("."),
            template: "template://default".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: 0,
            control_plane_resources: NodeResources {
                cpus: 4,
                memory_gib: 6,
            },
            worker_resources: NodeResources {
                cpus: 2,
                memory_gib: 4,
            },
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            cni_plugin: CniPlugin::None,
            cilium_version: "1.19.1".to_owned(),
            addons: Vec::new(),
            metallb: MetalLbConfig::default(),
            server_mode: ServerMode::Standard,
            component_toggles,
            kubeconfig_path: PathBuf::from("kubeconfig.yaml"),
            token_path: PathBuf::from("node-token"),
            nodes: vec![kalisto::cluster::state::NodeState {
                name: "kalisto-dev-cp".to_owned(),
                role: NodeRole::ControlPlane,
                artifacts_root: PathBuf::from("control-plane"),
            }],
        }
    }
}

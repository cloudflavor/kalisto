use std::fs;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::probe::macos_lima::{ProbeConfig as LimaBaseConfig, internal_hostname};
use crate::process::{CommandRunner, SystemRunner, ensure_path_exists};

use super::config::Config;
use super::node::{
    Instance, bootstrap_server, bootstrap_worker, build_worker_instances, collect_logs,
    start_node_instance, start_worker_instances,
};
use super::report::{Report, Result as ProbeResult};
use super::util::{
    effective_network, first_line, format_duration, initial_readiness_command, unique_suffix,
};
use super::wait::{
    wait_for_host_http, wait_for_node_ready, wait_for_path, wait_for_stable_guest_success,
    wait_for_worker_process,
};
use crate::k3s::NodeRole;

pub fn run(config: &Config) -> Report {
    let runner = SystemRunner;
    run_with_runner(config, &runner)
}

pub fn run_with_runner(config: &Config, runner: &dyn CommandRunner) -> Report {
    let mut results = Vec::new();
    let mut instances = Vec::new();
    let cluster_boot_started = Instant::now();

    if !cfg!(target_os = "macos") {
        results.push(ProbeResult::skip(
            "host.os",
            "macOS-only probe; skipping on non-macOS host",
        ));
        return Report { results };
    }

    if let Err(error) = ensure_path_exists(&config.workspace_root) {
        results.push(ProbeResult::fail("workspace.root", error.to_string()));
        return Report { results };
    }

    let workspace_root = match config.workspace_root.canonicalize() {
        Ok(path) => path,
        Err(source) => {
            results.push(ProbeResult::fail(
                "workspace.root",
                Error::CommandIo {
                    program: "canonicalize".to_owned(),
                    source,
                }
                .to_string(),
            ));
            return Report { results };
        }
    };

    let helper_script = workspace_root
        .join("scripts")
        .join("macos")
        .join("k3s_server_probe.sh");
    if let Err(error) = ensure_path_exists(&helper_script) {
        results.push(ProbeResult::fail("probe.script", error.to_string()));
        return Report { results };
    }

    let artifact_root = workspace_root.join("target").join("lima-k3s-probe");
    let _ = fs::remove_dir_all(&artifact_root);
    if let Err(error) = fs::create_dir_all(&artifact_root) {
        results.push(ProbeResult::fail("artifact.dir", error.to_string()));
        return Report { results };
    }

    match runner.run("limactl", &["--version".into()]) {
        Ok(output) => results.push(ProbeResult::pass(
            "lima.cli",
            first_line(&output.stdout, "limactl is available"),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("lima.cli", error.to_string()));
            return Report { results };
        }
    }

    let cluster_suffix = unique_suffix();
    let base_config = LimaBaseConfig {
        template: config.template.clone(),
        workspace_root: workspace_root.clone(),
        keep_resources: config.keep_resources,
        enable_rosetta: config.enable_rosetta,
        network: effective_network(config),
    };

    let control_plane = Instance::new(
        format!("kalisto-cp-{cluster_suffix}"),
        NodeRole::ControlPlane,
        artifact_root.join("control-plane"),
    );
    if let Err(error) = fs::create_dir_all(&control_plane.artifacts.root) {
        results.push(ProbeResult::fail(
            "artifact.control-plane",
            error.to_string(),
        ));
        return Report { results };
    }

    match start_node_instance(
        runner,
        &base_config,
        &control_plane,
        &workspace_root,
        Some(config.api_port),
        4,
        6,
    ) {
        Ok(output) => {
            results.push(ProbeResult::pass(
                "lima.instance.start",
                format!(
                    "{} ({})",
                    control_plane.name,
                    first_line(&output.stdout, "lima instance started")
                ),
            ));
            instances.push(control_plane.name.clone());
            results.push(ProbeResult::pass(
                "timing.control-plane.vm-start",
                format!(
                    "{} from start request to control-plane VM up",
                    format_duration(cluster_boot_started.elapsed())
                ),
            ));
        }
        Err(error) => {
            results.push(ProbeResult::fail("lima.instance.start", error.to_string()));
            return finish(Report { results }, runner, &instances, config);
        }
    }

    match bootstrap_server(
        runner,
        config,
        &workspace_root,
        &helper_script,
        &control_plane,
    ) {
        Ok(_) => results.push(ProbeResult::pass(
            "k3s.install.start",
            format!("started control-plane bootstrap in {}", control_plane.name),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("k3s.install.start", error.to_string()));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }

    let worker_nodes = build_worker_instances(config.worker_count, &cluster_suffix, &artifact_root);
    let started_workers = match start_worker_instances(
        runner,
        &base_config,
        &workspace_root,
        &worker_nodes,
        &mut instances,
        &mut results,
    ) {
        Ok(workers) => workers,
        Err(report) => return finish(report, runner, &instances, config),
    };

    match wait_for_path(
        &control_plane.artifacts.kubeconfig,
        Duration::from_secs(180),
    ) {
        Ok(_) => results.push(ProbeResult::pass(
            "kubeconfig.present",
            "k3s wrote a kubeconfig into the mounted workspace",
        )),
        Err(error) => {
            results.push(ProbeResult::fail("kubeconfig.present", error.to_string()));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }

    match wait_for_path(&control_plane.artifacts.token, Duration::from_secs(180)) {
        Ok(_) => results.push(ProbeResult::pass(
            "k3s.token.present",
            "k3s wrote a node token into the mounted workspace",
        )),
        Err(error) => {
            results.push(ProbeResult::fail("k3s.token.present", error.to_string()));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }

    let ready_check =
        initial_readiness_command(config.server_mode, &control_plane.artifacts.kubeconfig);
    match wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &ready_check,
        Duration::from_secs(90),
        1,
        Duration::from_secs(2),
    ) {
        Ok(output) => results.push(ProbeResult::pass(
            "k3s.bootstrap.initial",
            first_line(
                &output.stdout,
                "k3s server responded during initial bootstrap",
            ),
        )),
        Err(error) => {
            results.push(ProbeResult::fail(
                "k3s.bootstrap.initial",
                error.to_string(),
            ));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }

    let stable_ready_check = format!(
        "ps -ef | grep -F '[k]3s server' >/dev/null && KUBECONFIG={} /usr/local/bin/k3s kubectl get --raw=/readyz",
        crate::probe::macos_lima::shell_escape(
            control_plane
                .artifacts
                .kubeconfig
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        )
    );
    match wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &stable_ready_check,
        Duration::from_secs(30),
        3,
        Duration::from_secs(5),
    ) {
        Ok(output) => results.push(ProbeResult::pass(
            "k3s.apiserver.innode",
            first_line(
                &output.stdout,
                "k3s API stayed reachable inside the Lima guest",
            ),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("k3s.apiserver.innode", error.to_string()));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }

    let api_check = format!("https://127.0.0.1:{}/readyz", config.api_port);
    match wait_for_host_http(runner, &api_check, Duration::from_secs(90)) {
        Ok(output) => results.push(ProbeResult::pass(
            "k3s.apiserver.hostport",
            output.stdout.trim().to_owned(),
        )),
        Err(error) => {
            results.push(ProbeResult::fail(
                "k3s.apiserver.hostport",
                error.to_string(),
            ));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    }
    results.push(ProbeResult::pass(
        "timing.control-plane.ready",
        format!(
            "{} from first VM boot to host API readiness",
            format_duration(cluster_boot_started.elapsed())
        ),
    ));

    let server_token = match fs::read_to_string(&control_plane.artifacts.token) {
        Ok(token) => token.trim().to_owned(),
        Err(error) => {
            results.push(ProbeResult::fail("k3s.token.read", error.to_string()));
            collect_logs(runner, &mut results, &control_plane);
            return finish(Report { results }, runner, &instances, config);
        }
    };

    let server_url = format!("https://{}:6443", internal_hostname(&control_plane.name));
    for started_worker in started_workers {
        let worker = started_worker.node;
        match bootstrap_worker(
            runner,
            config,
            &workspace_root,
            &helper_script,
            &worker,
            &server_url,
            &server_token,
        ) {
            Ok(_) => results.push(ProbeResult::pass(
                "worker.bootstrap.start",
                format!("started worker bootstrap in {}", worker.name),
            )),
            Err(error) => {
                results.push(ProbeResult::fail(
                    "worker.bootstrap.start",
                    format!("{}: {error}", worker.name),
                ));
                collect_logs(runner, &mut results, &worker);
                return finish(Report { results }, runner, &instances, config);
            }
        }

        match wait_for_worker_process(runner, &worker.name) {
            Ok(_) => results.push(ProbeResult::pass(
                "worker.process",
                format!("worker process started in {}", worker.name),
            )),
            Err(error) => {
                results.push(ProbeResult::fail(
                    "worker.process",
                    format!("{}: {error}", worker.name),
                ));
                collect_logs(runner, &mut results, &worker);
                return finish(Report { results }, runner, &instances, config);
            }
        }

        match wait_for_node_ready(runner, &control_plane, &worker.name) {
            Ok(output) => results.push(ProbeResult::pass(
                "worker.node.ready",
                format!(
                    "{} ({})",
                    worker.name,
                    first_line(&output.stdout, "Ready=True")
                ),
            )),
            Err(error) => {
                results.push(ProbeResult::fail(
                    "worker.node.ready",
                    format!("{}: {error}", worker.name),
                ));
                collect_logs(runner, &mut results, &control_plane);
                collect_logs(runner, &mut results, &worker);
                return finish(Report { results }, runner, &instances, config);
            }
        }
        results.push(ProbeResult::pass(
            "timing.worker.ready",
            format!(
                "{} became Ready in {} from worker VM start",
                worker.name,
                format_duration(started_worker.boot_started.elapsed())
            ),
        ));
    }

    if control_plane.artifacts.kubeconfig.exists() {
        results.push(ProbeResult::pass(
            "kubeconfig.host",
            format!(
                "host kubeconfig written to {}",
                control_plane.artifacts.kubeconfig.display()
            ),
        ));
    } else {
        results.push(ProbeResult::fail(
            "kubeconfig.host",
            format!(
                "expected host kubeconfig at {}",
                control_plane.artifacts.kubeconfig.display()
            ),
        ));
    }

    results.push(ProbeResult::pass(
        "timing.cluster.ready",
        format!(
            "{} from first VM boot to cluster-ready milestone",
            format_duration(cluster_boot_started.elapsed())
        ),
    ));

    finish(Report { results }, runner, &instances, config)
}

fn finish(
    mut report: Report,
    runner: &dyn CommandRunner,
    instance_names: &[String],
    config: &Config,
) -> Report {
    if config.keep_resources {
        report.results.push(ProbeResult::skip(
            "cleanup",
            format!("kept Lima instances `{}`", instance_names.join(", ")),
        ));
        return report;
    }

    let mut cleanup_errors = Vec::new();
    for instance_name in instance_names.iter().rev() {
        let _ = runner.run(
            "limactl",
            &[
                "-y".into(),
                "stop".into(),
                "--force".into(),
                instance_name.clone(),
            ],
        );
        if let Err(error) = runner.run(
            "limactl",
            &[
                "-y".into(),
                "delete".into(),
                "--force".into(),
                instance_name.clone(),
            ],
        ) {
            cleanup_errors.push(format!("{instance_name}: {error}"));
        }
    }

    if cleanup_errors.is_empty() {
        report.results.push(ProbeResult::pass(
            "cleanup",
            "removed Lima probe resources successfully",
        ));
    } else {
        report
            .results
            .push(ProbeResult::fail("cleanup", cleanup_errors.join("\n")));
    }

    report
}

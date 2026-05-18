use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::backend_for;
use crate::cluster::state::ClusterState;
use crate::error::{Error, Result};
use crate::harness::scenario::{ProbeConfig, ScenarioConfig, WaitConfig};
use crate::k3s::NodeRole;
use crate::process::CommandRunner;
use crate::shell::shell_escape;

#[derive(Debug, Clone)]
pub struct ScenarioRunReport {
    pub scenario: String,
    pub cluster: String,
    pub namespace: String,
    pub applied_manifests: Vec<PathBuf>,
    pub waits: Vec<String>,
    pub probes: Vec<String>,
}

#[derive(Clone, Copy)]
struct HarnessStepContext<'a> {
    backend: &'a dyn crate::backend::ClusterBackend,
    runner: &'a dyn CommandRunner,
    instance_name: &'a str,
    runtime_root: &'a Path,
    guest_kubeconfig: &'a Path,
    namespace: &'a str,
}

pub fn run_scenario(
    state: &ClusterState,
    scenario: &ScenarioConfig,
    runner: &dyn CommandRunner,
) -> Result<ScenarioRunReport> {
    scenario.validate()?;

    let control_plane = state
        .nodes
        .iter()
        .find(|node| node.role == NodeRole::ControlPlane)
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "cluster `{}` does not have a recorded control-plane node",
                state.name
            ))
        })?;
    let backend = backend_for(state.backend);
    let control_plane_name = &control_plane.name;
    let control_plane_runtime_root = control_plane.artifacts_root.as_path();
    let guest_artifact_root =
        backend.guest_artifact_root(control_plane_name, control_plane_runtime_root)?;
    let guest_kubeconfig = guest_artifact_root.join("kubeconfig.yaml");

    eprintln!(
        "harness: preparing namespace `{}` for scenario `{}`",
        scenario.namespace, scenario.name
    );

    // Wait for the node to be ready before applying manifests
    // This is necessary because CNI addons are applied asynchronously
    eprintln!("harness: waiting for node to be ready");
    wait_for_node_ready(
        backend,
        runner,
        control_plane_name,
        control_plane_runtime_root,
        &guest_kubeconfig,
    )?;

    ensure_namespace(
        backend,
        runner,
        control_plane_name,
        control_plane_runtime_root,
        &guest_kubeconfig,
        &scenario.namespace,
    )?;

    let mut report = ScenarioRunReport {
        scenario: scenario.name.clone(),
        cluster: state.name.clone(),
        namespace: scenario.namespace.clone(),
        applied_manifests: Vec::new(),
        waits: Vec::new(),
        probes: Vec::new(),
    };

    for manifest in &scenario.manifests {
        let label = format!("apply {}", manifest.display());
        eprintln!("harness: {label}");
        execute_step(
            HarnessStepContext {
                backend,
                runner,
                instance_name: control_plane_name,
                runtime_root: control_plane_runtime_root,
                guest_kubeconfig: &guest_kubeconfig,
                namespace: &scenario.namespace,
            },
            &label,
            Some(failure_diagnostics_script(&scenario.namespace, None)),
            |context| {
                apply_manifest(
                    context.backend,
                    context.runner,
                    context.instance_name,
                    context.runtime_root,
                    context.guest_kubeconfig,
                    context.namespace,
                    manifest,
                )
            },
        )?;
        report.applied_manifests.push(manifest.clone());
    }

    for wait in &scenario.waits {
        let label = wait_step_label(wait);
        eprintln!("harness: {label}");
        execute_step(
            HarnessStepContext {
                backend,
                runner,
                instance_name: control_plane_name,
                runtime_root: control_plane_runtime_root,
                guest_kubeconfig: &guest_kubeconfig,
                namespace: &scenario.namespace,
            },
            &label,
            Some(failure_diagnostics_script(&scenario.namespace, Some(wait))),
            |context| {
                run_wait(
                    context.backend,
                    context.runner,
                    context.instance_name,
                    context.runtime_root,
                    context.guest_kubeconfig,
                    context.namespace,
                    wait,
                )
            },
        )?;
        report.waits.push(wait.label());
    }

    for probe in &scenario.probes {
        let label = format!("probe {}", probe.name());
        eprintln!("harness: {label}");
        execute_step(
            HarnessStepContext {
                backend,
                runner,
                instance_name: control_plane_name,
                runtime_root: control_plane_runtime_root,
                guest_kubeconfig: &guest_kubeconfig,
                namespace: &scenario.namespace,
            },
            &label,
            Some(failure_diagnostics_script(&scenario.namespace, None)),
            |context| {
                run_probe(
                    context.backend,
                    context.runner,
                    context.instance_name,
                    context.runtime_root,
                    context.guest_kubeconfig,
                    context.namespace,
                    probe,
                )
            },
        )?;
        report.probes.push(probe.name().to_owned());
    }

    Ok(report)
}

fn execute_step<F>(
    context: HarnessStepContext<'_>,
    label: &str,
    diagnostics_script: Option<String>,
    operation: F,
) -> Result<()>
where
    F: FnOnce(HarnessStepContext<'_>) -> Result<()>,
{
    match operation(context) {
        Ok(()) => Ok(()),
        Err(error) => {
            let diagnostics = diagnostics_script
                .as_deref()
                .and_then(|script| {
                    run_guest_kubectl_script_capture(
                        context.backend,
                        context.runner,
                        context.instance_name,
                        context.runtime_root,
                        context.guest_kubeconfig,
                        context.namespace,
                        script,
                    )
                    .ok()
                })
                .filter(|output| !output.trim().is_empty());
            let mut message = format!("harness step `{label}` failed: {error}");
            if let Some(diagnostics) = diagnostics {
                message.push_str("\n\nDiagnostics:\n");
                message.push_str(&diagnostics);
            }
            Err(Error::InvalidArgs(message))
        }
    }
}

fn ensure_namespace(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
) -> Result<()> {
    if namespace == "default" {
        return Ok(());
    }
    let command = format!(
        "kubectl get namespace {namespace} >/dev/null 2>&1 || kubectl create namespace {namespace}",
        namespace = shell_escape(namespace),
    );
    run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        namespace,
        &command,
    )?;
    Ok(())
}

fn apply_manifest(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
    manifest_path: &Path,
) -> Result<()> {
    let content = fs::read_to_string(manifest_path).map_err(|source| Error::CommandIo {
        program: "read_to_string".to_owned(),
        source,
    })?;
    let temp_path = format!(
        "/tmp/kalisto-harness-{}-{}.yaml",
        sanitize_for_path(namespace),
        sanitize_for_path(
            manifest_path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("manifest"),
        )
    );
    let marker = heredoc_marker(&content);
    let command = format!(
        concat!(
            "cat <<'{marker}' > {temp_path}\n",
            "{content}\n",
            "{marker}\n",
            "kubectl -n {namespace} apply -f {temp_path}\n",
            "rm -f {temp_path}"
        ),
        marker = marker,
        temp_path = shell_escape(&temp_path),
        namespace = shell_escape(namespace),
        content = content,
    );
    run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        namespace,
        &command,
    )?;
    Ok(())
}

fn run_wait(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
    wait: &WaitConfig,
) -> Result<()> {
    let command = match wait {
        WaitConfig::Deployment { name, timeout_secs } => format!(
            "kubectl -n {namespace} rollout status deployment/{name} --timeout={timeout_secs}s",
            namespace = shell_escape(namespace),
            name = shell_escape(name),
            timeout_secs = timeout_secs,
        ),
        WaitConfig::StatefulSet { name, timeout_secs } => format!(
            "kubectl -n {namespace} rollout status statefulset/{name} --timeout={timeout_secs}s",
            namespace = shell_escape(namespace),
            name = shell_escape(name),
            timeout_secs = timeout_secs,
        ),
        WaitConfig::DaemonSet { name, timeout_secs } => format!(
            "kubectl -n {namespace} rollout status daemonset/{name} --timeout={timeout_secs}s",
            namespace = shell_escape(namespace),
            name = shell_escape(name),
            timeout_secs = timeout_secs,
        ),
        WaitConfig::Job { name, timeout_secs } => format!(
            "kubectl -n {namespace} wait --for=condition=complete job/{name} --timeout={timeout_secs}s",
            namespace = shell_escape(namespace),
            name = shell_escape(name),
            timeout_secs = timeout_secs,
        ),
    };
    run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        namespace,
        &command,
    )?;
    Ok(())
}

fn wait_for_node_ready(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
) -> Result<()> {
    // Wait for node to be ready - first wait for node object to exist.
    // Keep resource parsing in Rust to avoid depending on extra guest shell tools.
    let check_node = "kubectl get nodes -o name";
    for _ in 0..60 {
        if run_guest_kubectl_script_capture(
            backend,
            runner,
            instance_name,
            runtime_root,
            guest_kubeconfig,
            "default",
            check_node,
        )
        .map(|output| has_node_resource(&output))
        .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    // Wait for CNI (flannel) daemonset to be ready - this is required for node scheduling
    eprintln!("harness: waiting for CNI to be ready");
    let wait_cni =
        "kubectl wait --for=condition=ready pod -n kube-flannel -l app=flannel --timeout=300s || true"
            .to_string();
    let _ = run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        "default",
        &wait_cni,
    );

    // Then wait for node Ready condition
    let command = "kubectl wait --for=condition=Ready node --all --timeout=300s".to_string();
    run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        "default",
        &command,
    )?;
    Ok(())
}

fn run_probe(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
    probe: &ProbeConfig,
) -> Result<()> {
    let command = match probe {
        ProbeConfig::Command(probe) => probe.command.as_str(),
    };
    run_guest_kubectl_script(
        backend,
        runner,
        instance_name,
        runtime_root,
        guest_kubeconfig,
        namespace,
        command,
    )?;
    Ok(())
}

fn run_guest_kubectl_script(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
    script: &str,
) -> Result<()> {
    let wrapped = kubectl_script(guest_kubeconfig, namespace, script);
    backend.guest_sudo_shell(runner, instance_name, runtime_root, &wrapped)?;
    Ok(())
}

fn run_guest_kubectl_script_capture(
    backend: &dyn crate::backend::ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
    guest_kubeconfig: &Path,
    namespace: &str,
    script: &str,
) -> Result<String> {
    let wrapped = kubectl_script(guest_kubeconfig, namespace, script);
    let output = backend.guest_sudo_shell(runner, instance_name, runtime_root, &wrapped)?;
    let mut text = String::new();
    if !output.stdout.trim().is_empty() {
        text.push_str(output.stdout.trim_end());
    }
    if !output.stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(output.stderr.trim_end());
    }
    Ok(text)
}

fn has_node_resource(output: &str) -> bool {
    output
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with("node/"))
}

fn kubectl_script(guest_kubeconfig: &Path, namespace: &str, script: &str) -> String {
    format!(
        concat!(
            "set -eu\n",
            "export KUBECONFIG={kubeconfig}\n",
            "export KALISTO_NAMESPACE={namespace}\n",
            "kubectl() {{ /usr/local/bin/k3s kubectl \"$@\"; }}\n",
            "{script}\n"
        ),
        kubeconfig = shell_escape(guest_kubeconfig.as_os_str().to_string_lossy().as_ref()),
        namespace = shell_escape(namespace),
        script = script,
    )
}

fn wait_step_label(wait: &WaitConfig) -> String {
    match wait {
        WaitConfig::Deployment { name, timeout_secs } => {
            format!("wait for deployment/{name} ({timeout_secs}s)")
        }
        WaitConfig::StatefulSet { name, timeout_secs } => {
            format!("wait for statefulset/{name} ({timeout_secs}s)")
        }
        WaitConfig::DaemonSet { name, timeout_secs } => {
            format!("wait for daemonset/{name} ({timeout_secs}s)")
        }
        WaitConfig::Job { name, timeout_secs } => format!("wait for job/{name} ({timeout_secs}s)"),
    }
}

fn failure_diagnostics_script(namespace: &str, wait: Option<&WaitConfig>) -> String {
    let namespace = shell_escape(namespace);
    let mut script = format!(
        concat!(
            "set +e\n",
            "echo '== kubectl get all =='\n",
            "kubectl -n {namespace} get all 2>&1 || true\n",
            "echo\n",
            "echo '== kubectl get events =='\n",
            "kubectl -n {namespace} get events --sort-by=.lastTimestamp 2>&1 | tail -n 40 || true\n"
        ),
        namespace = namespace,
    );

    if let Some(WaitConfig::Job { name, .. }) = wait {
        let job_name = shell_escape(name);
        script.push_str(&format!(
            concat!(
                "\n",
                "echo\n",
                "echo '== kubectl describe job =='\n",
                "kubectl -n {namespace} describe job/{job_name} 2>&1 || true\n",
                "echo\n",
                "echo '== kubectl logs job =='\n",
                "kubectl -n {namespace} logs job/{job_name} 2>&1 || true\n"
            ),
            namespace = namespace,
            job_name = job_name,
        ));
    }

    script
}

fn sanitize_for_path(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

fn heredoc_marker(content: &str) -> String {
    let mut marker = "KALISTO_MANIFEST_EOF".to_owned();
    while content.contains(&marker) {
        marker.push('X');
    }
    marker
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::harness::scenario::WaitConfig;

    use super::{failure_diagnostics_script, has_node_resource, heredoc_marker, kubectl_script};

    #[test]
    fn kubectl_script_exports_expected_environment() {
        let script = kubectl_script(
            Path::new("/var/lib/kalisto/kalisto-dev-cp/kubeconfig.yaml"),
            "demo",
            "kubectl get pods",
        );

        assert!(
            script.contains("export KUBECONFIG='/var/lib/kalisto/kalisto-dev-cp/kubeconfig.yaml'")
        );
        assert!(script.contains("export KALISTO_NAMESPACE='demo'"));
        assert!(script.contains("kubectl() { /usr/local/bin/k3s kubectl \"$@\"; }"));
        assert!(script.contains("kubectl get pods"));
    }

    #[test]
    fn has_node_resource_detects_node_lines() {
        assert!(has_node_resource("node/kalisto-cp"));
        assert!(has_node_resource("warning: noisy output\nnode/kalisto-cp"));
        assert!(!has_node_resource("No resources found"));
        assert!(!has_node_resource(""));
    }

    #[test]
    fn heredoc_marker_avoids_manifest_collisions() {
        let marker = heredoc_marker("KALISTO_MANIFEST_EOF\nKALISTO_MANIFEST_EOFX");
        assert_eq!(marker, "KALISTO_MANIFEST_EOFXX");
    }

    #[test]
    fn job_wait_diagnostics_include_describe_and_logs() {
        let script = failure_diagnostics_script(
            "demo-job",
            Some(&WaitConfig::Job {
                name: "migration-check".to_owned(),
                timeout_secs: 180,
            }),
        );

        assert!(script.contains("kubectl -n 'demo-job' get all"));
        assert!(script.contains("kubectl -n 'demo-job' get events"));
        assert!(script.contains("kubectl -n 'demo-job' describe job/'migration-check'"));
        assert!(script.contains("kubectl -n 'demo-job' logs job/'migration-check'"));
    }
}

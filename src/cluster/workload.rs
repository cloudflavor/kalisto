//! Apply user workload manifests to a running Kalisto cluster control plane.

use crate::backend::backend_for;
use crate::cluster::manager::{instance_status_is_running, require_cluster_state_root};
use crate::cluster::node::Instance;
use crate::cluster::state::ClusterState;
use crate::error::{Error, Result};
use crate::k3s::NodeRole;
use crate::process::CommandRunner;
use crate::shell::shell_escape;

/// Reserved heredoc delimiter used to stream the manifest to guest kubectl.
pub const MANIFEST_HEREDOC_DELIMITER: &str = "KALISTO_MANIFEST_EOF_9F3B7E";

#[derive(Debug, Clone)]
pub struct ManifestApplyOutcome {
    pub cluster: String,
    pub node: String,
    pub output: String,
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
/// Apply a Kubernetes manifest on a running cluster through the control
/// plane's own kubectl. The cluster must already be up; workload lifecycle is
/// delegated to Kubernetes itself.
pub fn apply_manifest(
    name: &str,
    manifest: &str,
    runner: &dyn CommandRunner,
) -> Result<ManifestApplyOutcome> {
    let manifest = manifest.trim();
    if manifest.is_empty() {
        return Err(Error::InvalidArgs("workload manifest is empty".to_owned()));
    }
    if manifest
        .lines()
        .any(|line| line.trim_end() == MANIFEST_HEREDOC_DELIMITER)
    {
        return Err(Error::InvalidArgs(format!(
            "workload manifest must not contain the reserved delimiter `{MANIFEST_HEREDOC_DELIMITER}`"
        )));
    }

    let state_root = require_cluster_state_root(name)?;
    let state = ClusterState::load(state_root.as_path())?;
    let backend = backend_for(state.backend);
    let control_plane_state = state
        .nodes
        .iter()
        .find(|node| node.role == NodeRole::ControlPlane)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidArgs("cluster state is missing a control-plane node".to_owned())
        })?;
    let control_plane = Instance::new(
        control_plane_state.name.clone(),
        NodeRole::ControlPlane,
        control_plane_state.artifacts_root.clone(),
    );
    let status = backend.query_instance_status(
        runner,
        &control_plane.name,
        &control_plane.artifacts.root,
    )?;
    if !instance_status_is_running(&status) {
        return Err(Error::InvalidArgs(format!(
            "control-plane `{}` is not running; start the cluster before applying workloads",
            control_plane.name
        )));
    }

    let guest_artifacts =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    let guest_kubeconfig = guest_artifacts.join("kubeconfig.yaml");
    let command = format!(
        "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl apply -f - <<'{delimiter}'\n{manifest}\n{delimiter}",
        kubeconfig = shell_escape(guest_kubeconfig.as_os_str().to_string_lossy().as_ref()),
        delimiter = MANIFEST_HEREDOC_DELIMITER,
    );
    let output = backend.guest_sudo_shell(
        runner,
        &control_plane.name,
        &control_plane.artifacts.root,
        &command,
    )?;
    Ok(ManifestApplyOutcome {
        cluster: state.name,
        node: control_plane.name,
        output: output.stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_manifest() {
        let error = apply_manifest("unused", "   \n", &crate::process::SystemRunner)
            .expect_err("empty manifest must be rejected");
        assert!(matches!(error, crate::error::Error::InvalidArgs(_)));
    }

    #[test]
    fn rejects_reserved_delimiter() {
        let manifest = format!("apiVersion: v1\nkind: ConfigMap\n{MANIFEST_HEREDOC_DELIMITER}\n");
        let error = apply_manifest("unused", &manifest, &crate::process::SystemRunner)
            .expect_err("reserved delimiter must be rejected");
        assert!(matches!(error, crate::error::Error::InvalidArgs(_)));
    }
}

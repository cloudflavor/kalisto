use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::lima::shell_escape;
use crate::k3s::ServerMode;

use super::config::Config;

pub fn initial_readiness_command(server_mode: ServerMode, kubeconfig_path: &Path) -> String {
    match server_mode {
        ServerMode::Standard => format!(
            "KUBECONFIG={} /usr/local/bin/k3s kubectl get nodes -o wide",
            shell_escape(kubeconfig_path.as_os_str().to_string_lossy().as_ref())
        ),
        ServerMode::Agentless => format!(
            "KUBECONFIG={} /usr/local/bin/k3s kubectl get --raw=/readyz",
            shell_escape(kubeconfig_path.as_os_str().to_string_lossy().as_ref())
        ),
    }
}

pub fn effective_network(config: &Config) -> String {
    config.network.clone().unwrap_or_else(|| {
        if config.worker_count > 0 {
            "lima:user-v2".to_owned()
        } else {
            "vzNAT".to_owned()
        }
    })
}

pub fn acceptable_apiserver_status(status: &str) -> bool {
    matches!(status.trim(), "200" | "401" | "403")
}

pub fn first_line(text: &str, fallback: &'static str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

pub fn format_duration(duration: Duration) -> String {
    let total_millis = duration.as_millis();
    let seconds = total_millis / 1000;
    let millis = total_millis % 1000;
    format!("{seconds}.{millis:03}s")
}

pub fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_millis())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::k3s::{ComponentToggles, ServerMode};

    use super::{
        Config, acceptable_apiserver_status, effective_network, format_duration,
        initial_readiness_command,
    };

    #[test]
    fn agentless_mode_uses_readyz_check() {
        let command =
            initial_readiness_command(ServerMode::Agentless, Path::new("/tmp/kubeconfig.yaml"));
        assert!(command.contains("kubectl get --raw=/readyz"));
    }

    #[test]
    fn host_port_check_accepts_authenticated_and_unauthenticated_responses() {
        assert!(acceptable_apiserver_status("200"));
        assert!(acceptable_apiserver_status("401"));
        assert!(acceptable_apiserver_status("403"));
        assert!(!acceptable_apiserver_status("500"));
    }

    #[test]
    fn multi_node_probe_defaults_to_user_v2_network() {
        let mut config = Config::new(PathBuf::from("."));
        config.worker_count = 1;
        assert_eq!(effective_network(&config), "lima:user-v2");
    }

    #[test]
    fn duration_format_uses_seconds_and_millis() {
        assert_eq!(format_duration(Duration::from_millis(17234)), "17.234s");
    }

    #[test]
    fn component_toggle_defaults_remain_minimal() {
        let exports = ComponentToggles::default().export_commands().join("; ");
        assert!(exports.contains("export KALISTO_K3S_DISABLE_TRAEFIK=1"));
        assert!(exports.contains("export KALISTO_K3S_DISABLE_LOCAL_STORAGE=1"));
        assert!(exports.contains("export KALISTO_K3S_DISABLE_COREDNS=0"));
        assert!(exports.contains("export KALISTO_K3S_DISABLE_NETWORK_POLICY=1"));
        assert!(exports.contains("export KALISTO_K3S_DISABLE_KUBE_PROXY=0"));
        assert!(exports.contains("export KALISTO_K3S_FLANNEL_BACKEND_NONE=0"));
    }
}

use std::path::Path;
use std::time::Duration;

use crate::k3s::ServerMode;
use crate::shell::shell_escape;

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

pub fn acceptable_apiserver_status(status: &str) -> bool {
    matches!(status.trim(), "200" | "401" | "403")
}

pub fn format_duration(duration: Duration) -> String {
    let total_millis = duration.as_millis();
    let seconds = total_millis / 1000;
    let millis = total_millis % 1000;
    format!("{seconds}.{millis:03}s")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use crate::k3s::ServerMode;

    use super::{acceptable_apiserver_status, format_duration, initial_readiness_command};

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
    fn duration_format_uses_seconds_and_millis() {
        assert_eq!(format_duration(Duration::from_millis(17234)), "17.234s");
    }
}

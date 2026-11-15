use std::path::PathBuf;

pub const KUBELET_RESOLV_CONF_PATH: &str = "/etc/kalisto/kubelet-resolv.conf";
pub const KUBELET_RESOLV_CONF_NAMESERVERS: [&str; 2] = ["1.1.1.1", "2606:4700:4700::1111"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMode {
    Standard,
    Agentless,
}

impl ServerMode {
    #[must_use]
    pub fn disable_agent(self) -> bool {
        matches!(self, Self::Agentless)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    ControlPlane,
    Worker,
}

impl NodeRole {
    #[must_use]
    pub fn as_env_value(self) -> &'static str {
        match self {
            Self::ControlPlane => "server",
            Self::Worker => "agent",
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // CLI arguments / persisted config surface; bool-per-flag is the design
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentToggles {
    pub disable_traefik: bool,
    pub disable_servicelb: bool,
    pub disable_metrics_server: bool,
    pub disable_local_storage: bool,
    pub disable_coredns: bool,
    pub disable_network_policy: bool,
    pub disable_kube_proxy: bool,
    pub flannel_backend_none: bool,
}

impl Default for ComponentToggles {
    fn default() -> Self {
        Self {
            disable_traefik: true,
            disable_servicelb: true,
            disable_metrics_server: true,
            disable_local_storage: true,
            disable_coredns: false,
            disable_network_policy: true,
            disable_kube_proxy: false,
            flannel_backend_none: false,
        }
    }
}

impl ComponentToggles {
    #[must_use]
    pub fn export_commands(self) -> [String; 8] {
        [
            export_toggle("KALISTO_K3S_DISABLE_TRAEFIK", self.disable_traefik),
            export_toggle("KALISTO_K3S_DISABLE_SERVICELB", self.disable_servicelb),
            export_toggle(
                "KALISTO_K3S_DISABLE_METRICS_SERVER",
                self.disable_metrics_server,
            ),
            export_toggle(
                "KALISTO_K3S_DISABLE_LOCAL_STORAGE",
                self.disable_local_storage,
            ),
            export_toggle("KALISTO_K3S_DISABLE_COREDNS", self.disable_coredns),
            export_toggle(
                "KALISTO_K3S_DISABLE_NETWORK_POLICY",
                self.disable_network_policy,
            ),
            export_toggle("KALISTO_K3S_DISABLE_KUBE_PROXY", self.disable_kube_proxy),
            export_toggle(
                "KALISTO_K3S_FLANNEL_BACKEND_NONE",
                self.flannel_backend_none,
            ),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct BootstrapSpec {
    pub workspace_root: PathBuf,
    pub artifact_dir: PathBuf,
    pub node_role: NodeRole,
    pub node_name: String,
    pub k3s_version: String,
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
    pub use_systemd: bool,
    pub tls_sans: Vec<String>,
    pub server_url: Option<String>,
    pub token: Option<String>,
}

impl BootstrapSpec {
    pub fn export_commands<F>(&self, mut escape: F) -> Vec<String>
    where
        F: FnMut(&str) -> String,
    {
        let workspace_root = self
            .workspace_root
            .as_os_str()
            .to_string_lossy()
            .into_owned();
        let artifact_dir = self.artifact_dir.as_os_str().to_string_lossy().into_owned();

        let mut commands = vec![
            format!("export KALISTO_WORKSPACE_ROOT={}", escape(&workspace_root)),
            format!("export KALISTO_ARTIFACT_DIR={}", escape(&artifact_dir)),
            format!(
                "export KALISTO_K3S_NODE_ROLE={}",
                self.node_role.as_env_value()
            ),
            format!("export KALISTO_K3S_NODE_NAME={}", escape(&self.node_name)),
            format!("export K3S_VERSION={}", escape(&self.k3s_version)),
            format!(
                "export KALISTO_K3S_KUBELET_RESOLV_CONF={}",
                escape(KUBELET_RESOLV_CONF_PATH)
            ),
            export_toggle("KALISTO_K3S_USE_SYSTEMD", self.use_systemd),
            export_toggle(
                "KALISTO_K3S_DISABLE_AGENT",
                self.server_mode.disable_agent(),
            ),
        ];
        commands.extend(self.component_toggles.export_commands());
        if !self.tls_sans.is_empty() {
            commands.push(format!(
                "export KALISTO_K3S_TLS_SANS={}",
                escape(&self.tls_sans.join(","))
            ));
        }

        if let Some(server_url) = &self.server_url {
            commands.push(format!(
                "export KALISTO_K3S_SERVER_URL={}",
                escape(server_url)
            ));
        }
        if let Some(token) = &self.token {
            commands.push(format!("export KALISTO_K3S_TOKEN={}", escape(token)));
        }

        commands
    }
}

#[must_use]
pub fn kubelet_resolv_conf_contents() -> String {
    let mut contents = String::new();
    for nameserver in KUBELET_RESOLV_CONF_NAMESERVERS {
        contents.push_str("nameserver ");
        contents.push_str(nameserver);
        contents.push('\n');
    }
    contents
}

#[must_use]
pub fn kubelet_resolv_conf_arg() -> String {
    format!("resolv-conf={KUBELET_RESOLV_CONF_PATH}")
}

fn export_toggle(name: &str, enabled: bool) -> String {
    format!("export {name}={}", if enabled { "1" } else { "0" })
}

#[cfg(test)]
mod tests {
    use super::{BootstrapSpec, ComponentToggles, NodeRole, ServerMode};
    use std::path::PathBuf;

    #[test]
    fn default_component_toggles_define_minimal_baseline() {
        let toggles = ComponentToggles::default();
        assert!(toggles.disable_traefik);
        assert!(toggles.disable_servicelb);
        assert!(toggles.disable_metrics_server);
        assert!(toggles.disable_local_storage);
        assert!(!toggles.disable_coredns);
        assert!(toggles.disable_network_policy);
        assert!(!toggles.disable_kube_proxy);
        assert!(!toggles.flannel_backend_none);
    }

    #[test]
    fn bootstrap_spec_renders_expected_env_exports() {
        let spec = BootstrapSpec {
            workspace_root: PathBuf::from("/workspace"),
            artifact_dir: PathBuf::from("/workspace/target/k3s"),
            node_role: NodeRole::Worker,
            node_name: "worker-0".to_owned(),
            k3s_version: "v1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
            use_systemd: false,
            tls_sans: vec!["127.0.0.1".to_owned(), "cp.example.test".to_owned()],
            server_url: Some("https://cp.example.test:6443".to_owned()),
            token: Some("secret".to_owned()),
        };

        let exports = spec
            .export_commands(|value| format!("'{value}'"))
            .join("; ");
        assert!(exports.contains("export KALISTO_K3S_NODE_ROLE=agent"));
        assert!(exports.contains("export KALISTO_K3S_NODE_NAME='worker-0'"));
        assert!(exports.contains("export KALISTO_K3S_TLS_SANS='127.0.0.1,cp.example.test'"));
        assert!(exports.contains("export KALISTO_K3S_SERVER_URL='https://cp.example.test:6443'"));
        assert!(exports.contains("export KALISTO_K3S_TOKEN='secret'"));
        assert!(
            exports.contains(
                "export KALISTO_K3S_KUBELET_RESOLV_CONF='/etc/kalisto/kubelet-resolv.conf'"
            )
        );
        assert!(exports.contains("export KALISTO_K3S_DISABLE_COREDNS=0"));
    }

    #[test]
    fn kubelet_resolv_conf_contents_render_cloudflare_dual_stack_defaults() {
        let contents = super::kubelet_resolv_conf_contents();

        assert!(contents.contains("nameserver 1.1.1.1"));
        assert!(contents.contains("nameserver 2606:4700:4700::1111"));
    }
}

use std::path::Path;
use std::time::Duration;

use tracing::info;

use crate::backend::ClusterBackend;
use crate::cluster::node::Instance;
use crate::cluster::wait::{GuestWait, LiveLogTail, wait_for_stable_guest_success_with_tail};
use crate::error::{Error, Result};
use crate::k3s::ComponentToggles;
use crate::process::CommandRunner;
use crate::shell::shell_escape;

pub const DEFAULT_CILIUM_VERSION: &str = "1.19.1";
const CILIUM_MANIFEST_PATH: &str = "/var/lib/rancher/k3s/server/manifests/kalisto-cilium.yaml";
const TRAEFIK_MANIFEST_PATH: &str = "/var/lib/rancher/k3s/server/manifests/kalisto-traefik.yaml";
const METALLB_MANIFEST_URL: &str = "https://raw.githubusercontent.com/metallb/metallb/v0.15.3/config/manifests/metallb-native.yaml";
const METALLB_WAIT_COMMAND: &str = concat!(
    "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n metallb-system rollout status deployment/controller --timeout=30s && ",
    "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n metallb-system rollout status daemonset/speaker --timeout=30s"
);
const LONGHORN_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/longhorn/longhorn/v1.10.0/deploy/longhorn.yaml";
const LONGHORN_WAIT_COMMAND: &str = concat!(
    "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n longhorn-system rollout status deployment/longhorn-driver-deployer --timeout=30s && ",
    "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n longhorn-system rollout status daemonset/longhorn-manager --timeout=30s"
);
const LOCAL_PATH_MANIFEST_URL: &str = "https://raw.githubusercontent.com/rancher/local-path-provisioner/v0.0.34/deploy/local-path-storage.yaml";
const LOCAL_PATH_WAIT_COMMAND: &str = "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n local-path-storage rollout status deployment/local-path-provisioner --timeout=30s";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CniPlugin {
    None,
    Flannel,
    Cilium,
}

impl CniPlugin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Flannel => "flannel",
            Self::Cilium => "cilium",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "flannel" => Some(Self::Flannel),
            "cilium" => Some(Self::Cilium),
            _ => None,
        }
    }

    pub fn apply_component_defaults(self, toggles: &mut ComponentToggles) {
        match self {
            Self::None => {
                toggles.flannel_backend_none = true;
                toggles.disable_coredns = false;
                toggles.disable_network_policy = true;
            }
            Self::Flannel => {
                toggles.flannel_backend_none = false;
            }
            Self::Cilium => {
                toggles.flannel_backend_none = true;
                toggles.disable_coredns = false;
                toggles.disable_network_policy = true;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonId {
    CniFlannel,
    CniCilium,
    IngressTraefik,
    LbServiceLb,
    LbMetalLb,
    StorageLonghorn,
    StorageLocalPath,
}

impl AddonId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CniFlannel => "cni/flannel",
            Self::CniCilium => "cni/cilium",
            Self::IngressTraefik => "ingress/traefik",
            Self::LbServiceLb => "lb/servicelb",
            Self::LbMetalLb => "lb/metallb",
            Self::StorageLonghorn => "storage/longhorn",
            Self::StorageLocalPath => "storage/local-path",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cni/flannel" => Some(Self::CniFlannel),
            "cni/cilium" => Some(Self::CniCilium),
            "ingress/traefik" => Some(Self::IngressTraefik),
            "lb/servicelb" => Some(Self::LbServiceLb),
            "lb/metallb" => Some(Self::LbMetalLb),
            "storage/longhorn" => Some(Self::StorageLonghorn),
            "storage/local-path" => Some(Self::StorageLocalPath),
            _ => None,
        }
    }
}

struct RemoteYamlAddonSpec<'a> {
    addon: AddonId,
    label: &'a str,
    url: &'a str,
    wait_command_template: &'a str,
    timeout: Duration,
}

pub fn install_post_ready_addons(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    cni: CniPlugin,
    cilium_version: &str,
    addons: &[String],
) -> Result<()> {
    if cni == CniPlugin::Cilium {
        install_cilium(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            cilium_version,
        )?;
    }

    for addon in addons {
        let addon = AddonId::parse(addon)
            .ok_or_else(|| Error::InvalidArgs(format!("unsupported add-on `{addon}`")))?;
        install_addon(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            addon,
            cilium_version,
        )?;
    }

    Ok(())
}

pub fn install_catalog_addon(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    addon: &str,
) -> Result<()> {
    install_catalog_addon_with_cilium_version(
        backend,
        runner,
        control_plane,
        guest_kubeconfig,
        addon,
        DEFAULT_CILIUM_VERSION,
    )
}

pub fn install_catalog_addon_with_cilium_version(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    addon: &str,
    cilium_version: &str,
) -> Result<()> {
    let addon = AddonId::parse(addon)
        .ok_or_else(|| Error::InvalidArgs(format!("unsupported add-on `{addon}`")))?;
    install_addon(
        backend,
        runner,
        control_plane,
        guest_kubeconfig,
        addon,
        cilium_version,
    )
}

fn install_addon(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    addon: AddonId,
    cilium_version: &str,
) -> Result<()> {
    match addon {
        AddonId::CniFlannel => {
            info!(
                node = %control_plane.name,
                addon = "flannel",
                "flannel CNI is managed by k3s bootstrap flags"
            );
            Ok(())
        }
        AddonId::CniCilium => install_cilium(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            cilium_version,
        ),
        AddonId::IngressTraefik => {
            install_traefik(backend, runner, control_plane, guest_kubeconfig)
        }
        AddonId::LbServiceLb => {
            info!(
                node = %control_plane.name,
                addon = "servicelb",
                "k3s ServiceLB is managed by k3s bootstrap flags"
            );
            Ok(())
        }
        AddonId::LbMetalLb => install_remote_yaml_addon(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            RemoteYamlAddonSpec {
                addon,
                label: "metallb",
                url: METALLB_MANIFEST_URL,
                wait_command_template: METALLB_WAIT_COMMAND,
                timeout: Duration::from_secs(300),
            },
        ),
        AddonId::StorageLonghorn => install_remote_yaml_addon(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            RemoteYamlAddonSpec {
                addon,
                label: "longhorn",
                url: LONGHORN_MANIFEST_URL,
                wait_command_template: LONGHORN_WAIT_COMMAND,
                timeout: Duration::from_secs(600),
            },
        ),
        AddonId::StorageLocalPath => install_remote_yaml_addon(
            backend,
            runner,
            control_plane,
            guest_kubeconfig,
            RemoteYamlAddonSpec {
                addon,
                label: "local-path",
                url: LOCAL_PATH_MANIFEST_URL,
                wait_command_template: LOCAL_PATH_WAIT_COMMAND,
                timeout: Duration::from_secs(300),
            },
        ),
    }
}

fn install_cilium(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    cilium_version: &str,
) -> Result<()> {
    info!(
        node = %control_plane.name,
        addon = "cilium",
        version = cilium_version,
        "installing post-ready add-on"
    );

    apply_guest_manifest(
        backend,
        runner,
        control_plane,
        CILIUM_MANIFEST_PATH,
        &render_cilium_helmchart(cilium_version),
    )?;

    if should_wait_for_addons() {
        let wait_command = format!(
            concat!(
                "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n kube-system rollout status daemonset/cilium --timeout=30s && ",
                "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n kube-system rollout status deployment/cilium-operator --timeout=30s"
            ),
            kubeconfig = shell_escape(guest_kubeconfig.as_os_str().to_string_lossy().as_ref()),
        );
        wait_for_guest_rollout(
            backend,
            runner,
            control_plane,
            &wait_command,
            "cilium",
            Duration::from_secs(600),
        )
        .map_err(|error| {
            Error::InvalidArgs(format!(
                "failed to install cilium {} on cluster control-plane {}: {error}",
                cilium_version, control_plane.name
            ))
        })?;
        info!(
            node = %control_plane.name,
            addon = "cilium",
            version = cilium_version,
            "post-ready add-on is ready"
        );
    } else {
        info!(
            node = %control_plane.name,
            addon = "cilium",
            version = cilium_version,
            "post-ready add-on manifest applied (async readiness)"
        );
    }

    Ok(())
}

fn install_traefik(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
) -> Result<()> {
    info!(
        node = %control_plane.name,
        addon = "traefik",
        "installing post-ready add-on"
    );

    apply_guest_manifest(
        backend,
        runner,
        control_plane,
        TRAEFIK_MANIFEST_PATH,
        &render_traefik_helmchart(),
    )?;

    if should_wait_for_addons() {
        let wait_command = format!(
            "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl -n kube-system rollout status deployment/traefik --timeout=30s",
            kubeconfig = shell_escape(guest_kubeconfig.as_os_str().to_string_lossy().as_ref()),
        );
        wait_for_guest_rollout(
            backend,
            runner,
            control_plane,
            &wait_command,
            "traefik",
            Duration::from_secs(300),
        )
        .map_err(|error| {
            Error::InvalidArgs(format!(
                "failed to install add-on ingress/traefik on cluster control-plane {}: {error}",
                control_plane.name
            ))
        })?;
        info!(
            node = %control_plane.name,
            addon = "traefik",
            "post-ready add-on is ready"
        );
    } else {
        info!(
            node = %control_plane.name,
            addon = "traefik",
            "post-ready add-on manifest applied (async readiness)"
        );
    }

    Ok(())
}

fn install_remote_yaml_addon(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_kubeconfig: &Path,
    spec: RemoteYamlAddonSpec<'_>,
) -> Result<()> {
    info!(node = %control_plane.name, addon = spec.addon.as_str(), url = spec.url, "installing post-ready add-on");
    let manifest = fetch_manifest(runner, spec.url)?;
    let manifest_path = format!(
        "/var/lib/rancher/k3s/server/manifests/kalisto-{}.yaml",
        spec.addon.as_str().replace('/', "-")
    );
    apply_guest_manifest(backend, runner, control_plane, &manifest_path, &manifest)?;
    if should_wait_for_addons() {
        let wait_command = spec.wait_command_template.replace(
            "{kubeconfig}",
            shell_escape(guest_kubeconfig.as_os_str().to_string_lossy().as_ref()).as_str(),
        );
        wait_for_guest_rollout(
            backend,
            runner,
            control_plane,
            &wait_command,
            spec.label,
            spec.timeout,
        )
        .map_err(|error| {
            Error::InvalidArgs(format!(
                "failed to install add-on {} on cluster control-plane {}: {error}",
                spec.addon.as_str(),
                control_plane.name
            ))
        })?;
        info!(
            node = %control_plane.name,
            addon = spec.addon.as_str(),
            "post-ready add-on is ready"
        );
    } else {
        info!(
            node = %control_plane.name,
            addon = spec.addon.as_str(),
            "post-ready add-on manifest applied (async readiness)"
        );
    }
    Ok(())
}

fn should_wait_for_addons() -> bool {
    std::env::var("KALISTO_WAIT_FOR_ADDONS")
        .ok()
        .map(|value| parse_truthy_flag(&value))
        .unwrap_or(false)
}

fn parse_truthy_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn fetch_manifest(runner: &dyn CommandRunner, url: &str) -> Result<String> {
    let output = runner.run("curl", &["-fsSL".to_owned(), url.to_owned()])?;
    Ok(output.stdout)
}

fn apply_guest_manifest(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    manifest_path: &str,
    manifest: &str,
) -> Result<()> {
    let install_command = format!(
        "mkdir -p /var/lib/rancher/k3s/server/manifests\ncat <<'EOF' > {manifest_path}\n{manifest}\nEOF",
        manifest_path = manifest_path,
        manifest = manifest,
    );
    backend.guest_sudo_shell(
        runner,
        &control_plane.name,
        &control_plane.artifacts.root,
        &install_command,
    )?;
    Ok(())
}

fn wait_for_guest_rollout(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    command: &str,
    label: &str,
    timeout: Duration,
) -> Result<()> {
    let mut tail = LiveLogTail::new(
        &control_plane.artifacts.stderr_log,
        format!("{}:{label}", control_plane.name),
    );
    wait_for_stable_guest_success_with_tail(
        backend,
        runner,
        GuestWait {
            instance_name: &control_plane.name,
            runtime_root: &control_plane.artifacts.root,
            command,
            timeout,
            consecutive_successes: 1,
            interval: Duration::from_secs(5),
        },
        &mut tail,
    )?;
    Ok(())
}

fn render_cilium_helmchart(version: &str) -> String {
    format!(
        concat!(
            "apiVersion: helm.cattle.io/v1\n",
            "kind: HelmChart\n",
            "metadata:\n",
            "  name: cilium\n",
            "  namespace: kube-system\n",
            "spec:\n",
            "  bootstrap: true\n",
            "  repo: https://helm.cilium.io/\n",
            "  chart: cilium\n",
            "  version: {version}\n",
            "  targetNamespace: kube-system\n",
            "  valuesContent: |-\n",
            "    operator:\n",
            "      replicas: 1\n"
        ),
        version = version,
    )
}

fn render_traefik_helmchart() -> String {
    concat!(
        "apiVersion: helm.cattle.io/v1\n",
        "kind: HelmChart\n",
        "metadata:\n",
        "  name: traefik\n",
        "  namespace: kube-system\n",
        "spec:\n",
        "  repo: https://helm.traefik.io/traefik\n",
        "  chart: traefik\n",
        "  targetNamespace: kube-system\n",
    )
    .to_owned()
}

#[cfg(test)]
mod tests {
    use crate::k3s::ComponentToggles;

    use super::{
        AddonId, CniPlugin, DEFAULT_CILIUM_VERSION, parse_truthy_flag, render_cilium_helmchart,
        render_traefik_helmchart,
    };

    #[test]
    fn cni_plugin_roundtrips() {
        assert_eq!(CniPlugin::parse("none"), Some(CniPlugin::None));
        assert_eq!(CniPlugin::parse("flannel"), Some(CniPlugin::Flannel));
        assert_eq!(CniPlugin::parse("cilium"), Some(CniPlugin::Cilium));
        assert_eq!(CniPlugin::None.as_str(), "none");
        assert_eq!(CniPlugin::Flannel.as_str(), "flannel");
        assert_eq!(CniPlugin::Cilium.as_str(), "cilium");
    }

    #[test]
    fn none_defaults_disable_flannel_backend() {
        let mut toggles = ComponentToggles::default();
        CniPlugin::None.apply_component_defaults(&mut toggles);
        assert!(toggles.flannel_backend_none);
        assert!(!toggles.disable_coredns);
        assert!(toggles.disable_network_policy);
    }

    #[test]
    fn cilium_defaults_disable_flannel_backend() {
        let mut toggles = ComponentToggles::default();
        CniPlugin::Cilium.apply_component_defaults(&mut toggles);
        assert!(toggles.flannel_backend_none);
        assert!(!toggles.disable_coredns);
        assert!(toggles.disable_network_policy);
    }

    #[test]
    fn cilium_manifest_uses_default_version() {
        let manifest = render_cilium_helmchart(DEFAULT_CILIUM_VERSION);
        assert!(manifest.contains("bootstrap: true"));
        assert!(manifest.contains("repo: https://helm.cilium.io/"));
        assert!(manifest.contains("chart: cilium"));
        assert!(manifest.contains("version: 1.19.1"));
        assert!(manifest.contains("replicas: 1"));
    }

    #[test]
    fn traefik_manifest_uses_official_helm_repo() {
        let manifest = render_traefik_helmchart();
        assert!(manifest.contains("kind: HelmChart"));
        assert!(manifest.contains("name: traefik"));
        assert!(manifest.contains("repo: https://helm.traefik.io/traefik"));
        assert!(manifest.contains("chart: traefik"));
    }

    #[test]
    fn addon_ids_roundtrip() {
        assert_eq!(AddonId::parse("cni/flannel"), Some(AddonId::CniFlannel));
        assert_eq!(AddonId::parse("cni/cilium"), Some(AddonId::CniCilium));
        assert_eq!(
            AddonId::parse("ingress/traefik"),
            Some(AddonId::IngressTraefik)
        );
        assert_eq!(AddonId::parse("lb/servicelb"), Some(AddonId::LbServiceLb));
        assert_eq!(AddonId::parse("lb/metallb"), Some(AddonId::LbMetalLb));
        assert_eq!(
            AddonId::parse("storage/longhorn"),
            Some(AddonId::StorageLonghorn)
        );
        assert_eq!(
            AddonId::parse("storage/local-path"),
            Some(AddonId::StorageLocalPath)
        );
        assert_eq!(AddonId::IngressTraefik.as_str(), "ingress/traefik");
        assert_eq!(AddonId::LbServiceLb.as_str(), "lb/servicelb");
        assert_eq!(AddonId::LbMetalLb.as_str(), "lb/metallb");
    }

    #[test]
    fn parse_truthy_flag_accepts_common_true_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_truthy_flag(value), "expected `{value}` to be truthy");
        }
    }

    #[test]
    fn parse_truthy_flag_rejects_non_true_values() {
        for value in ["", "0", "false", "no", "off", "maybe"] {
            assert!(!parse_truthy_flag(value), "expected `{value}` to be falsy");
        }
    }
}

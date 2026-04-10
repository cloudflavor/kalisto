use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::schema::KalistoConfig;

pub fn config_path(project_root: &Path) -> PathBuf {
    project_root.join("kalisto.toml")
}

pub fn load_project_config(project_root: &Path) -> Result<Option<KalistoConfig>> {
    let path = config_path(project_root);
    if !path.is_file() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path).map_err(|source| Error::CommandIo {
        program: "read_to_string".to_owned(),
        source,
    })?;
    let mut config = toml::from_str::<KalistoConfig>(&content).map_err(|error| {
        Error::InvalidArgs(format!("failed to parse {}: {error}", path.display()))
    })?;
    config.resolve_relative_paths(project_root);
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{config_path, load_project_config};

    #[test]
    fn load_project_config_returns_none_when_file_is_missing() {
        let root =
            std::env::temp_dir().join(format!("kalisto-config-missing-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();

        let config = load_project_config(&root).unwrap();

        assert!(config.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_project_config_reads_kalisto_toml() {
        let root = std::env::temp_dir().join(format!("kalisto-config-load-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            config_path(&root),
            r#"
[cluster]
name = "dev"
backend = "lima"

[plugins.cilium]
enabled = true
"#,
        )
        .unwrap();

        let config = load_project_config(&root).unwrap().unwrap();

        assert_eq!(config.cluster.name.as_deref(), Some("dev"));
        assert_eq!(config.cluster.backend.as_deref(), Some("lima"));
        assert_eq!(config.plugins.cilium.enabled, Some(true));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_project_config_resolves_harness_manifest_paths() {
        let root =
            std::env::temp_dir().join(format!("kalisto-config-harness-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            config_path(&root),
            r#"
[[harness.scenarios]]
name = "smoke"
manifests = ["k8s/app.yaml"]

[[harness.scenarios.probes]]
type = "command"
name = "pods"
command = "kubectl get pods"
"#,
        )
        .unwrap();

        let config = load_project_config(&root).unwrap().unwrap();

        assert_eq!(
            config.harness.scenarios[0].manifests[0],
            root.join("k8s/app.yaml")
        );
        let _ = fs::remove_dir_all(root);
    }
}

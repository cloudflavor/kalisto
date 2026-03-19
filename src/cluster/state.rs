use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::k3s::{NodeRole, ServerMode};

#[derive(Debug, Clone)]
pub struct NodeState {
    pub name: String,
    pub role: NodeRole,
    pub artifacts_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClusterState {
    pub name: String,
    pub workspace_root: PathBuf,
    pub template: String,
    pub api_port: u16,
    pub worker_count: usize,
    pub enable_rosetta: bool,
    pub network: String,
    pub k3s_version: String,
    pub server_mode: ServerMode,
    pub kubeconfig_path: PathBuf,
    pub token_path: PathBuf,
    pub nodes: Vec<NodeState>,
}

impl ClusterState {
    pub fn state_file(root: &Path) -> PathBuf {
        root.join("cluster.state")
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root).map_err(|source| Error::CommandIo {
            program: "create_dir_all".to_owned(),
            source,
        })?;

        let mut lines = vec![
            format!("name\t{}", escape(&self.name)),
            format!(
                "workspace_root\t{}",
                escape(&self.workspace_root.display().to_string())
            ),
            format!("template\t{}", escape(&self.template)),
            format!("api_port\t{}", self.api_port),
            format!("worker_count\t{}", self.worker_count),
            format!("enable_rosetta\t{}", self.enable_rosetta),
            format!("network\t{}", escape(&self.network)),
            format!("k3s_version\t{}", escape(&self.k3s_version)),
            format!(
                "server_mode\t{}",
                match self.server_mode {
                    ServerMode::Standard => "standard",
                    ServerMode::Agentless => "agentless",
                }
            ),
            format!(
                "kubeconfig_path\t{}",
                escape(&self.kubeconfig_path.display().to_string())
            ),
            format!(
                "token_path\t{}",
                escape(&self.token_path.display().to_string())
            ),
        ];

        for node in &self.nodes {
            lines.push(format!(
                "node\t{}\t{}\t{}",
                escape(&node.name),
                match node.role {
                    NodeRole::ControlPlane => "control-plane",
                    NodeRole::Worker => "worker",
                },
                escape(&node.artifacts_root.display().to_string())
            ));
        }

        fs::write(Self::state_file(root), lines.join("\n")).map_err(|source| Error::CommandIo {
            program: "write".to_owned(),
            source,
        })
    }

    pub fn load(root: &Path) -> Result<Self> {
        let content =
            fs::read_to_string(Self::state_file(root)).map_err(|source| Error::CommandIo {
                program: "read_to_string".to_owned(),
                source,
            })?;

        let mut name = None;
        let mut workspace_root = None;
        let mut template = None;
        let mut api_port = None;
        let mut worker_count = None;
        let mut enable_rosetta = None;
        let mut network = None;
        let mut k3s_version = None;
        let mut server_mode = None;
        let mut kubeconfig_path = None;
        let mut token_path = None;
        let mut nodes = Vec::new();

        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.first().copied() {
                Some("name") => name = parts.get(1).map(|value| unescape(value)),
                Some("workspace_root") => {
                    workspace_root = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("template") => template = parts.get(1).map(|value| unescape(value)),
                Some("api_port") => {
                    api_port = parts.get(1).and_then(|value| value.parse::<u16>().ok())
                }
                Some("worker_count") => {
                    worker_count = parts.get(1).and_then(|value| value.parse::<usize>().ok())
                }
                Some("enable_rosetta") => {
                    enable_rosetta = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("network") => network = parts.get(1).map(|value| unescape(value)),
                Some("k3s_version") => k3s_version = parts.get(1).map(|value| unescape(value)),
                Some("server_mode") => {
                    server_mode = match parts.get(1).copied() {
                        Some("standard") => Some(ServerMode::Standard),
                        Some("agentless") => Some(ServerMode::Agentless),
                        _ => None,
                    }
                }
                Some("kubeconfig_path") => {
                    kubeconfig_path = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("token_path") => {
                    token_path = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("node") if parts.len() == 4 => {
                    let role = match parts[2] {
                        "control-plane" => Some(NodeRole::ControlPlane),
                        "worker" => Some(NodeRole::Worker),
                        _ => None,
                    };
                    if let Some(role) = role {
                        nodes.push(NodeState {
                            name: unescape(parts[1]),
                            role,
                            artifacts_root: PathBuf::from(unescape(parts[3])),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(Self {
            name: required(name, "name")?,
            workspace_root: required(workspace_root, "workspace_root")?,
            template: required(template, "template")?,
            api_port: required(api_port, "api_port")?,
            worker_count: required(worker_count, "worker_count")?,
            enable_rosetta: required(enable_rosetta, "enable_rosetta")?,
            network: required(network, "network")?,
            k3s_version: required(k3s_version, "k3s_version")?,
            server_mode: required(server_mode, "server_mode")?,
            kubeconfig_path: required(kubeconfig_path, "kubeconfig_path")?,
            token_path: required(token_path, "token_path")?,
            nodes,
        })
    }
}

fn required<T>(value: Option<T>, field: &str) -> Result<T> {
    value.ok_or_else(|| Error::InvalidArgs(format!("invalid cluster state: missing {field}")))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('t') => output.push('\t'),
                Some('n') => output.push('\n'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::k3s::{NodeRole, ServerMode};

    use super::{ClusterState, NodeState};

    #[test]
    fn state_roundtrips() {
        let root = std::env::temp_dir().join(format!("kalisto-state-{}", std::process::id()));
        let state = ClusterState {
            name: "dev".to_owned(),
            workspace_root: std::path::PathBuf::from("/tmp/work"),
            template: "/tmp/template.yaml".to_owned(),
            api_port: 16443,
            worker_count: 1,
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            kubeconfig_path: std::path::PathBuf::from("/tmp/kubeconfig.yaml"),
            token_path: std::path::PathBuf::from("/tmp/token"),
            nodes: vec![
                NodeState {
                    name: "cp".to_owned(),
                    role: NodeRole::ControlPlane,
                    artifacts_root: std::path::PathBuf::from("/tmp/cp"),
                },
                NodeState {
                    name: "w0".to_owned(),
                    role: NodeRole::Worker,
                    artifacts_root: std::path::PathBuf::from("/tmp/w0"),
                },
            ],
        };

        state.save(&root).unwrap();
        let loaded = ClusterState::load(&root).unwrap();
        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.worker_count, 1);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.nodes[1].name, "w0");

        let _ = fs::remove_dir_all(root);
    }
}

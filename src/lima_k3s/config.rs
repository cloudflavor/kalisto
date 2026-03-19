use std::path::PathBuf;

use crate::k3s::{ComponentToggles, ServerMode};

#[derive(Debug, Clone)]
pub struct Config {
    pub template: String,
    pub workspace_root: PathBuf,
    pub api_port: u16,
    pub keep_resources: bool,
    pub k3s_version: String,
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
    pub enable_rosetta: bool,
    pub worker_count: usize,
    pub network: Option<String>,
}

impl Config {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            template: "template://default".to_owned(),
            workspace_root,
            api_port: 16443,
            keep_resources: false,
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
            enable_rosetta: true,
            worker_count: 0,
            network: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Config;

    #[test]
    fn defaults_to_expected_template_and_worker_shape() {
        let config = Config::new(PathBuf::from("."));
        assert_eq!(config.template, "template://default");
        assert_eq!(config.worker_count, 0);
        assert_eq!(config.network, None);
    }
}

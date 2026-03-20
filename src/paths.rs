use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub fn current_dir() -> Result<PathBuf> {
    env::current_dir().map_err(|source| Error::CommandIo {
        program: "current_dir".to_owned(),
        source,
    })
}

pub fn default_project_root(project_root: Option<PathBuf>) -> Result<PathBuf> {
    match project_root {
        Some(path) => Ok(path),
        None => current_dir(),
    }
}

pub fn default_cluster_name(project_root: Option<&Path>) -> Result<String> {
    let root = match project_root {
        Some(path) => path.to_path_buf(),
        None => current_dir()?,
    };

    Ok(root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "kalisto".to_owned()))
}

pub fn config_root() -> Result<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config").map(|root| root.join("kalisto"))
}

pub fn data_root() -> Result<PathBuf> {
    base_dir("XDG_DATA_HOME", ".local/share").map(|root| root.join("kalisto"))
}

fn base_dir(xdg_var: &str, home_suffix: &str) -> Result<PathBuf> {
    match env::var_os(xdg_var) {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => {
            let home = env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| Error::InvalidArgs(format!("neither {xdg_var} nor HOME is set")))?;
            Ok(home.join(home_suffix))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{config_root, data_root, default_cluster_name};

    #[test]
    fn default_cluster_name_uses_current_directory() {
        let name = default_cluster_name(None).unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn xdg_roots_end_in_kalisto() {
        assert!(config_root().unwrap().ends_with("kalisto"));
        assert!(data_root().unwrap().ends_with("kalisto"));
    }
}

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::process::CommandRunner;

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
    if let Some(name) = selected_cluster_name()? {
        return Ok(name);
    }

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

pub fn selected_cluster_name() -> Result<Option<String>> {
    read_default_cluster_name()
}

pub fn default_cluster_file() -> Result<PathBuf> {
    Ok(config_root()?.join("default-cluster"))
}

pub fn read_default_cluster_name() -> Result<Option<String>> {
    let path = default_cluster_file()?;
    if !path.is_file() {
        return Ok(None);
    }

    let value = fs::read_to_string(&path).map_err(|source| Error::CommandIo {
        program: "read_to_string".to_owned(),
        source,
    })?;
    let name = value.trim();
    if name.is_empty() || !default_cluster_state_exists(name)? {
        Ok(None)
    } else {
        Ok(Some(name.to_owned()))
    }
}

pub fn write_default_cluster_name(name: &str) -> Result<()> {
    let path = default_cluster_file()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::CommandIo {
            program: "create_dir_all".to_owned(),
            source,
        })?;
    }
    fs::write(path, format!("{name}\n")).map_err(|source| Error::CommandIo {
        program: "write".to_owned(),
        source,
    })
}

pub fn clear_default_cluster_name_if_matches(name: &str) -> Result<()> {
    let path = default_cluster_file()?;
    // The state may already have been deleted by destroy. Read the selection
    // directly rather than discarding it as stale before it can be cleared.
    let current = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::CommandIo {
                program: "read_to_string".to_owned(),
                source,
            });
        }
    };
    if current.trim() != name {
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(source) => Err(Error::CommandIo {
            program: "remove_file".to_owned(),
            source,
        }),
    }
}

pub fn handoff_default_cluster_file_to_invoking_user(runner: &dyn CommandRunner) -> Result<()> {
    let Some((uid, gid)) = sudo_invoking_user_ids() else {
        return Ok(());
    };
    let path = default_cluster_file()?;
    if !path.exists() {
        return Ok(());
    }

    runner.run(
        "chown",
        &[format!("{uid}:{gid}"), path.display().to_string()],
    )?;
    Ok(())
}

pub fn config_root() -> Result<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config").map(|root| root.join("kalisto"))
}

pub fn data_root() -> Result<PathBuf> {
    base_dir("XDG_DATA_HOME", ".local/share").map(|root| root.join("kalisto"))
}

pub fn artifacts_root() -> Result<PathBuf> {
    Ok(data_root()?.join("artifacts"))
}

fn base_dir(xdg_var: &str, home_suffix: &str) -> Result<PathBuf> {
    match env::var_os(xdg_var) {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => effective_home()
            .map(|home| home.join(home_suffix))
            .ok_or_else(|| Error::InvalidArgs(format!("neither {xdg_var} nor HOME is set"))),
    }
}

fn effective_home() -> Option<PathBuf> {
    if let Some(sudo_home) = sudo_invoking_user_home() {
        return Some(sudo_home);
    }

    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn default_cluster_state_exists(name: &str) -> Result<bool> {
    Ok(data_root()?.join("clusters").join(name).is_dir())
}

fn sudo_invoking_user_ids() -> Option<(u32, u32)> {
    let uid = env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
    let gid = env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
    Some((uid, gid))
}

fn sudo_invoking_user_home() -> Option<PathBuf> {
    let sudo_user = env::var_os("SUDO_USER")
        .filter(|value| !value.is_empty())
        .and_then(|value| value.into_string().ok())?;
    if sudo_user == "root" {
        return None;
    }

    let root_home = if cfg!(target_os = "macos") {
        Path::new("/Users")
    } else {
        Path::new("/home")
    };
    Some(root_home.join(sudo_user))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        artifacts_root, clear_default_cluster_name_if_matches, config_root, data_root,
        default_cluster_name, read_default_cluster_name, selected_cluster_name,
        write_default_cluster_name,
    };
    use crate::test_support::env_lock;

    #[test]
    fn default_cluster_name_uses_current_directory() {
        let name = default_cluster_name(None).unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn xdg_roots_end_in_kalisto() {
        assert!(config_root().unwrap().ends_with("kalisto"));
        assert!(data_root().unwrap().ends_with("kalisto"));
        assert!(artifacts_root().unwrap().ends_with("kalisto/artifacts"));
    }

    #[test]
    fn data_root_prefers_invoking_user_home_under_sudo() {
        let _guard = env_lock();
        let original_home = std::env::var_os("HOME");
        let original_sudo_user = std::env::var_os("SUDO_USER");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");

        unsafe {
            std::env::set_var("HOME", "/root");
            std::env::set_var("SUDO_USER", "alice");
            std::env::remove_var("XDG_DATA_HOME");
        }

        let resolved = data_root().unwrap();
        let expected = if cfg!(target_os = "macos") {
            Path::new("/Users/alice/.local/share/kalisto")
        } else {
            Path::new("/home/alice/.local/share/kalisto")
        };
        assert_eq!(resolved, expected);

        restore_env("HOME", original_home);
        restore_env("SUDO_USER", original_sudo_user);
        restore_env("XDG_DATA_HOME", original_xdg_data_home);
    }

    #[test]
    fn xdg_data_home_still_overrides_sudo_user_home() {
        let _guard = env_lock();
        let original_home = std::env::var_os("HOME");
        let original_sudo_user = std::env::var_os("SUDO_USER");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");

        unsafe {
            std::env::set_var("HOME", "/root");
            std::env::set_var("SUDO_USER", "alice");
            std::env::set_var("XDG_DATA_HOME", "/tmp/kalisto-data");
        }

        assert_eq!(data_root().unwrap(), Path::new("/tmp/kalisto-data/kalisto"));

        restore_env("HOME", original_home);
        restore_env("SUDO_USER", original_sudo_user);
        restore_env("XDG_DATA_HOME", original_xdg_data_home);
    }

    #[test]
    fn default_cluster_name_prefers_profile_file() {
        let _guard = env_lock();
        let original_home = std::env::var_os("HOME");
        let original_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let original_sudo_user = std::env::var_os("SUDO_USER");
        let config_home =
            std::env::temp_dir().join(format!("kalisto-config-{}", std::process::id()));
        let data_home = std::env::temp_dir().join(format!("kalisto-data-{}", std::process::id()));

        unsafe {
            std::env::set_var("HOME", "/tmp/ignored-home");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            std::env::set_var("XDG_DATA_HOME", &data_home);
            std::env::remove_var("SUDO_USER");
        }

        fs::create_dir_all(
            data_home
                .join("kalisto")
                .join("clusters")
                .join("profile-dev"),
        )
        .unwrap();
        write_default_cluster_name("profile-dev").unwrap();
        assert_eq!(
            read_default_cluster_name().unwrap(),
            Some("profile-dev".to_owned())
        );
        assert_eq!(
            default_cluster_name(Some(Path::new("/tmp/workspace-name"))).unwrap(),
            "profile-dev"
        );
        assert_eq!(
            selected_cluster_name().unwrap(),
            Some("profile-dev".to_owned())
        );

        clear_default_cluster_name_if_matches("profile-dev").unwrap();
        assert_eq!(read_default_cluster_name().unwrap(), None);
        assert_eq!(selected_cluster_name().unwrap(), None);

        restore_env("HOME", original_home);
        restore_env("XDG_CONFIG_HOME", original_xdg_config_home);
        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        restore_env("SUDO_USER", original_sudo_user);
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(data_home);
    }

    #[test]
    fn stale_default_cluster_profile_is_ignored() {
        let _guard = env_lock();
        let original_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let config_home =
            std::env::temp_dir().join(format!("kalisto-stale-config-{}", std::process::id()));
        let data_home =
            std::env::temp_dir().join(format!("kalisto-stale-data-{}", std::process::id()));

        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        write_default_cluster_name("missing").unwrap();
        assert_eq!(read_default_cluster_name().unwrap(), None);
        assert_eq!(selected_cluster_name().unwrap(), None);
        clear_default_cluster_name_if_matches("missing").unwrap();
        assert!(!super::default_cluster_file().unwrap().exists());

        restore_env("XDG_CONFIG_HOME", original_xdg_config_home);
        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(data_home);
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
}

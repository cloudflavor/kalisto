#![cfg(unix)]
//! Host-safe CI coverage: native key generation needs no external tools.
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Owned(PathBuf);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn native_keygen_works_without_path_tools_and_never_prints_private_key() {
    let root = std::env::temp_dir().join(format!(
        "kalisto-keys-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let _owned = Owned(root.clone());
    let path = root.join("private.agekey");
    let output = Command::new(env!("CARGO_BIN_EXE_kalisto"))
        .env("PATH", "")
        .current_dir(&root)
        .args(["archive-keygen", "--output"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("recipient: age1"));
    assert!(!stdout.contains("AGE-SECRET-KEY-"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("AGE-SECRET-KEY-"));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let original = fs::read(&path).unwrap();
    let duplicate = Command::new(env!("CARGO_BIN_EXE_kalisto"))
        .env("PATH", "")
        .current_dir(&root)
        .args(["archive-keygen", "--output"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(fs::read(&path).unwrap(), original);
}

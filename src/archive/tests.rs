use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;

use super::*;
use crate::cluster::state::NodeState;
use crate::process::Output;
use crate::test_support::env_lock;

fn manifest() -> Manifest {
    Manifest {
        schema: 1,
        architecture: std::env::consts::ARCH.into(),
        lima_version: crate::runtime::LIMA_VERSION.into(),
        contains_secrets: true,
        state: json!({}),
        files: vec![],
    }
}
fn temporary() -> Scratch {
    Scratch::new(&std::env::temp_dir()).unwrap()
}

#[test]
fn archive_round_trip_preserves_sparse_logical_bytes_and_private_mode() {
    let root = temporary();
    let source = root.0.join("source");
    let mut bytes = vec![0; 2 * 1024 * 1024];
    bytes[10..17].copy_from_slice(b"secrets");
    fs::write(&source, &bytes).unwrap();
    let output = root.0.join("bundle.kalisto");
    format::write(&output, manifest(), &[("base-image".into(), source)]).unwrap();
    let stage = temporary();
    let result = format::read(&output, &stage.0, DEFAULT_MAX_BYTES).unwrap();
    assert_eq!(fs::read(stage.0.join("base-image")).unwrap(), bytes);
    assert_eq!(result.files.len(), 1);
    assert_eq!(
        fs::metadata(output).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn archive_writer_never_overwrites_existing_output() {
    let root = temporary();
    let output = root.0.join("existing");
    fs::write(&output, "preserve").unwrap();
    assert!(format::write(&output, manifest(), &[]).is_err());
    assert_eq!(fs::read_to_string(output).unwrap(), "preserve");
}
#[test]
fn payload_allowlist_rejects_traversal_and_host_files() {
    for path in [
        "../escape",
        "/tmp/escape",
        "nodes/00/disk",
        "nodes/64/disk",
        "assets/../token",
        "runtime/0/k3s.pid",
        "nodes/0/lima.yaml",
        "nodes/0/ssh.config",
        "assets/a\\b",
    ] {
        assert!(!format::valid_path(path), "{path}");
    }
}
fn raw_archive(
    root: &Path,
    names: &[(&str, tar::EntryType, &[u8])],
    m: Option<Manifest>,
) -> PathBuf {
    let path = root.join("bad.kalisto");
    let encoder = GzEncoder::new(fs::File::create(&path).unwrap(), Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    for (name, kind, data) in names {
        let mut header = tar::Header::new_gnu();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_entry_type(*kind);
        header.set_cksum();
        tar.append(&header, *data).unwrap();
    }
    if let Some(m) = m {
        let data = serde_json::to_vec(&m).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "manifest.json", data.as_slice())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    path
}
#[test]
fn extractor_rejects_links_and_traversal_without_writing_outside_staging() {
    for (name, kind) in [
        ("../escape", tar::EntryType::Regular),
        ("base-image", tar::EntryType::Symlink),
        ("base-image", tar::EntryType::Link),
        ("base-image", tar::EntryType::GNULongName),
    ] {
        let root = temporary();
        let stage = temporary();
        let path = raw_archive(&root.0, &[(name, kind, b"")], Some(manifest()));
        assert!(format::read(&path, &stage.0, DEFAULT_MAX_BYTES).is_err());
        assert!(fs::read_dir(&stage.0).unwrap().next().is_none());
    }
}
#[test]
fn extractor_rejects_duplicates_checksums_and_size_overruns() {
    let root = temporary();
    let stage = temporary();
    let path = raw_archive(
        &root.0,
        &[("base-image", tar::EntryType::Regular, b"bad")],
        Some(manifest()),
    );
    assert!(format::read(&path, &stage.0, DEFAULT_MAX_BYTES).is_err());
    let stage = temporary();
    assert!(format::read(&path, &stage.0, 1).is_err());
    let root = temporary();
    let stage = temporary();
    let path = raw_archive(
        &root.0,
        &[
            ("base-image", tar::EntryType::Regular, b"x"),
            ("base-image", tar::EntryType::Regular, b"y"),
        ],
        Some(manifest()),
    );
    assert!(format::read(&path, &stage.0, DEFAULT_MAX_BYTES).is_err());
}
#[test]
fn extractor_rejects_trailing_bytes_and_truncated_gzip() {
    let root = temporary();
    let path = root.0.join("bundle");
    format::write(&path, manifest(), &[]).unwrap();
    let original = fs::read(&path).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"other member")
        .unwrap();
    assert!(format::read(&path, &temporary().0, DEFAULT_MAX_BYTES).is_err());
    fs::write(&path, &original[..original.len() - 5]).unwrap();
    assert!(format::read(&path, &temporary().0, DEFAULT_MAX_BYTES).is_err());
}

struct Environment {
    root: Scratch,
    short_home: PathBuf,
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Environment {
    fn new() -> Self {
        let root = temporary();
        let old = ["XDG_DATA_HOME", "LIMA_HOME"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        let short_home = PathBuf::from(format!("/tmp/ka-{:x}", format::nonce().unwrap()));
        fs::create_dir(root.0.join("lima")).unwrap();
        std::os::unix::fs::symlink(root.0.join("lima"), &short_home).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root.0.join("data"));
            std::env::set_var("LIMA_HOME", &short_home);
        }
        Self {
            root,
            short_home,
            old,
        }
    }
    fn state(&self) -> ClusterState {
        let mut spec = ClusterSpec::new("saved".into(), self.root.0.clone());
        spec.backend = BackendKind::Lima;
        crate::pull::test_fixture(&spec);
        let root = spec.state_root();
        fs::create_dir_all(&root).unwrap();
        let image = self.root.0.join("base.gz");
        fs::write(&image, b"os image").unwrap();
        let template = self.root.0.join("base.yaml");
        fs::write(
            &template,
            serde_json::to_vec(&json!({"images":[{"location":image}]})).unwrap(),
        )
        .unwrap();
        let runtime = root.join("runtime/control-plane");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("node-token"), "DUMMY_SECRET").unwrap();
        fs::write(runtime.join("k3s.pid"), "stale").unwrap();
        fs::write(runtime.join("k3s.stderr.log"), "sensitive log omitted").unwrap();
        let name = "kalisto-saved-cp".to_owned();
        let vm = lima_home().unwrap().join(&name);
        fs::create_dir_all(&vm).unwrap();
        fs::write(
            vm.join("disk"),
            b"sqlite; helm releases; installed containers; local PV bytes",
        )
        .unwrap();
        fs::write(vm.join("vz-efi"), b"firmware").unwrap();
        let state = ClusterState {
            name: spec.name,
            backend: spec.backend,
            workspace_root: self.root.0.clone(),
            template: template.display().to_string(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: spec.api_port,
            worker_count: 0,
            control_plane_resources: spec.control_plane_resources,
            worker_resources: spec.worker_resources,
            enable_rosetta: false,
            network: "vzNAT".into(),
            k3s_version: spec.k3s_version,
            cni_plugin: spec.cni_plugin,
            cilium_version: spec.cilium_version,
            addons: spec.addons,
            metallb: spec.metallb,
            server_mode: spec.server_mode,
            component_toggles: spec.component_toggles,
            kubeconfig_path: root.join("kubeconfig.yaml"),
            token_path: root.join("node-token"),
            nodes: vec![NodeState {
                name,
                role: NodeRole::ControlPlane,
                artifacts_root: runtime,
            }],
        };
        state.save(&root).unwrap();
        state
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.short_home);
        for (key, value) in &self.old {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}
struct Runner {
    running: bool,
}
impl CommandRunner for Runner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        if program == "limactl" && args[0] == "list" {
            return Ok(Output {stdout:json!({"status":if self.running {"Running"} else {"Stopped"},"config":{"vmType":"vz","mounts":[]}}).to_string(),stderr:String::new()});
        }
        Err(invalid(format!("unexpected command: {program} {args:?}")))
    }
}
#[test]
fn export_import_preserves_guest_data_identity_and_credentials_without_starting() {
    let _lock = env_lock();
    let environment = Environment::new();
    let state = environment.state();
    let path = environment.root.0.join("saved.kalisto");
    export_with_runner(&state.name, &path, false, &Runner { running: false }).unwrap();
    let root = ClusterSpec::state_root_for(&state.name).unwrap();
    fs::remove_dir_all(&root).unwrap();
    fs::remove_dir_all(lima_home().unwrap().join(&state.nodes[0].name)).unwrap();
    import_with_runner(
        &path,
        Some(16444),
        DEFAULT_MAX_BYTES,
        &Runner { running: false },
        false,
    )
    .unwrap();
    let restored = ClusterState::load(&root).unwrap();
    assert_eq!(restored.api_port, 16444);
    assert_eq!(
        fs::read_to_string(restored.nodes[0].artifacts_root.join("node-token")).unwrap(),
        "DUMMY_SECRET"
    );
    assert_eq!(
        fs::read(lima_home().unwrap().join(&state.nodes[0].name).join("disk")).unwrap(),
        b"sqlite; helm releases; installed containers; local PV bytes"
    );
    assert!(!restored.nodes[0].artifacts_root.join("k3s.pid").exists());
    assert!(
        !restored.nodes[0]
            .artifacts_root
            .join("k3s.stderr.log")
            .exists()
    );
    let config: serde_json::Value = serde_json::from_slice(
        &fs::read(
            lima_home()
                .unwrap()
                .join(&state.nodes[0].name)
                .join("lima.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(config["provision"], json!([]));
    assert!(
        crate::pull::load(&spec_for(&restored).unwrap())
            .unwrap()
            .root
            .starts_with(&root)
    );
}
#[test]
fn export_requires_stop_consent_and_import_never_overwrites_existing_cluster() {
    let _lock = env_lock();
    let environment = Environment::new();
    let state = environment.state();
    let path = environment.root.0.join("saved.kalisto");
    assert!(export_with_runner(&state.name, &path, false, &Runner { running: true }).is_err());
    assert!(!path.exists());
    export_with_runner(&state.name, &path, false, &Runner { running: false }).unwrap();
    assert!(
        import_with_runner(
            &path,
            None,
            DEFAULT_MAX_BYTES,
            &Runner { running: false },
            false
        )
        .is_err()
    );
    assert!(ClusterState::load(&ClusterSpec::state_root_for(&state.name).unwrap()).is_ok());
}
#[test]
fn encrypted_restore_authenticates_before_publishing_state_or_virtual_machines() {
    let _lock = env_lock();
    let environment = Environment::new();
    let state = environment.state();
    // Deterministic fixture noise forces several age chunks; corrupt the final
    // tag only after earlier decrypted payload bytes have reached private staging.
    let mut value = 0x1234_abcd_u32;
    let disk = (0..256 * 1024)
        .map(|_| {
            value ^= value << 13;
            value ^= value >> 17;
            value ^= value << 5;
            value.to_le_bytes()[0]
        })
        .collect::<Vec<_>>();
    fs::write(
        lima_home().unwrap().join(&state.nodes[0].name).join("disk"),
        &disk,
    )
    .unwrap();
    let key = age::x25519::Identity::generate();
    let path = environment.root.0.join("saved.kalisto.age");
    let protection = ExportEncryption::Recipients(vec![key.to_public()]);
    let outcome = export_protected_with_runner(
        &state.name,
        &path,
        false,
        &Runner { running: false },
        &protection,
    )
    .unwrap();
    assert!(outcome.encrypted);
    let root = ClusterSpec::state_root_for(&state.name).unwrap();
    let vm = lima_home().unwrap().join(&state.nodes[0].name);
    fs::remove_dir_all(&root).unwrap();
    fs::remove_dir_all(&vm).unwrap();
    let credentials = ImportDecryption::Identities(vec![key]);
    let original = fs::read(&path).unwrap();
    assert!(
        original.len() > 2 * 65536,
        "fixture must span multiple authenticated chunks"
    );
    let mut corrupt = original.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(
        import_protected_with_runner(
            &path,
            None,
            DEFAULT_MAX_BYTES,
            &Runner { running: false },
            false,
            &credentials
        )
        .is_err()
    );
    assert!(!root.exists());
    assert!(!vm.exists());
    assert!(
        fs::read_dir(data_root().unwrap().join("archive-staging"))
            .unwrap()
            .next()
            .is_none()
    );
    fs::write(&path, original).unwrap();
    import_protected_with_runner(
        &path,
        None,
        DEFAULT_MAX_BYTES,
        &Runner { running: false },
        false,
        &credentials,
    )
    .unwrap();
    let restored = ClusterState::load(&root).unwrap();
    assert_eq!(fs::read(vm.join("disk")).unwrap(), disk);
    assert_eq!(
        fs::read(restored.nodes[0].artifacts_root.join("node-token")).unwrap(),
        b"DUMMY_SECRET"
    );
}

#[test]
fn plaintext_age_filename_is_rejected_before_stopping_anything() {
    assert!(
        export_protected_with_runner(
            "not-created",
            Path::new("misleading.age"),
            true,
            &Runner { running: true },
            &ExportEncryption::Plaintext
        )
        .is_err()
    );
}

#[test]
fn operation_lock_excludes_concurrent_mutation_and_releases_on_drop() {
    let root = temporary();
    let state = root.0.join("cluster");
    let held = crate::operation::lock(&state).unwrap();
    assert!(crate::operation::lock(&state).is_err());
    drop(held);
    assert!(crate::operation::lock(&state).is_ok());
}

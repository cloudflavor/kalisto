#![cfg(target_os = "macos")]
//! Opt-in CI evidence: actual three-node export/destroy/import/boot round trip.
//! The sensitive archive is deleted, never retained as a CI artifact.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use kalisto::cluster::addon::CniPlugin;
use kalisto::cluster::manager;
use kalisto::cluster::spec::ClusterSpec;

struct Owned {
    name: String,
    archive: PathBuf,
    identity: PathBuf,
}
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = manager::destroy(&self.name);
        let _ = kalisto::paths::clear_default_cluster_name_if_matches(&self.name);
        let _ = fs::remove_file(&self.archive);
        let _ = fs::remove_file(&self.identity);
    }
}
fn product(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_kalisto"))
        .current_dir(root)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kalisto {args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn kubectl(kubeconfig: &Path, args: &[&str]) -> String {
    let output = Command::new("kubectl")
        .arg("--kubeconfig")
        .arg(kubeconfig)
        .arg("--request-timeout=10s")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kubectl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn native_cold_archive_restores_kubernetes_objects_and_worker_local_data() {
    if std::env::var("KALISTO_LIVE_CLUSTER_ARCHIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let root = PathBuf::from(std::env::var_os("KALISTO_ARCHIVE_TEST_ROOT").expect(
        "set KALISTO_ARCHIVE_TEST_ROOT to owned CI storage on the mounted external build volume",
    ));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("ar-{}-{nonce:x}", std::process::id());
    let evidence = root.join(&name);
    fs::create_dir_all(&evidence).unwrap();
    let workspace = evidence.join("empty-cwd");
    fs::create_dir(&workspace).unwrap();
    assert!(!ClusterSpec::state_root_for(&name).unwrap().exists());
    let lima_home = std::env::var_os("LIMA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".lima"));
    for suffix in ["cp", "w0", "w1"] {
        assert!(!lima_home.join(format!("kalisto-{name}-{suffix}")).exists());
    }
    let archive = evidence.join("private.kalisto.age");
    let identity = evidence.join("private.agekey");
    let _owned = Owned {
        name: name.clone(),
        archive: archive.clone(),
        identity: identity.clone(),
    };
    let recipient = kalisto::archive::generate_identity(&identity).unwrap();
    product(
        &workspace,
        &[
            "pull",
            "--name",
            &name,
            "--backend",
            "lima",
            "--addons",
            "cni/cilium",
        ],
    );
    product(
        &workspace,
        &[
            "up",
            "--name",
            &name,
            "--backend",
            "lima",
            "--workers",
            "2",
            "--addons",
            "cni/cilium",
        ],
    );
    let kubeconfig = manager::kubeconfig_path(&name).unwrap();
    kubectl(
        &kubeconfig,
        &[
            "create",
            "configmap",
            "archive-proof",
            "--from-literal=value=configuration-survives",
        ],
    );
    kubectl(
        &kubeconfig,
        &[
            "create",
            "secret",
            "generic",
            "archive-proof",
            "--from-literal=value=dummy-test-secret",
        ],
    );
    let worker = format!("kalisto-{name}-w0");
    let lima = kalisto::runtime::program("limactl").expect("pull prepared Lima");
    let output=Command::new(lima).args(["shell","--tty=false",&worker,"sudo","sh","-lc",
        "mkdir -p /var/lib/kalisto-archive-proof; printf 'persistent-before-export' >/var/lib/kalisto-archive-proof/value"]).output().unwrap();
    assert!(output.status.success());
    let mut spec = ClusterSpec::new(name.clone(), workspace.clone());
    spec.backend = kalisto::backend::BackendKind::Lima;
    spec.cni_plugin = CniPlugin::Cilium;
    spec.cni_plugin
        .apply_component_defaults(&mut spec.component_toggles);
    let assets = kalisto::pull::load(&spec).unwrap();
    let pod = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"archive-proof"},"spec":{
        "nodeName":worker,"containers":[{"name":"proof","image":assets.probe_image(),"imagePullPolicy":"Never",
        "command":["sh","-c","test -s /data/value && sleep 3600"],"volumeMounts":[{"name":"persistent","mountPath":"/data"}]}],
        "volumes":[{"name":"persistent","hostPath":{"path":"/var/lib/kalisto-archive-proof","type":"Directory"}}]}});
    let manifest = evidence.join("pod.json");
    fs::write(&manifest, serde_json::to_vec(&pod).unwrap()).unwrap();
    kubectl(&kubeconfig, &["apply", "-f", manifest.to_str().unwrap()]);
    kubectl(
        &kubeconfig,
        &[
            "wait",
            "--for=condition=Ready",
            "pod/archive-proof",
            "--timeout=120s",
        ],
    );
    product(
        &workspace,
        &[
            "export",
            "--name",
            &name,
            "--output",
            archive.to_str().unwrap(),
            "--recipient",
            &recipient,
            "--stop",
        ],
    );
    product(&workspace, &["destroy", "--name", &name]);
    product(
        &workspace,
        &[
            "import",
            archive.to_str().unwrap(),
            "--identity",
            identity.to_str().unwrap(),
        ],
    );
    // Import alone must not boot nodes or silently restore old host paths.
    let stopped = manager::status(&name).unwrap();
    assert!(format!("{stopped:?}").contains("Stopped"));
    product(
        &workspace,
        &["up", "--name", &name, "--addons", "cni/cilium"],
    );
    let kubeconfig = manager::kubeconfig_path(&name).unwrap();
    kubectl(
        &kubeconfig,
        &[
            "wait",
            "--for=condition=Ready",
            "nodes",
            "--all",
            "--timeout=120s",
        ],
    );
    assert_eq!(
        kubectl(
            &kubeconfig,
            &[
                "get",
                "configmap",
                "archive-proof",
                "-o",
                "jsonpath={.data.value}"
            ]
        ),
        "configuration-survives"
    );
    assert_eq!(
        kubectl(
            &kubeconfig,
            &[
                "get",
                "secret",
                "archive-proof",
                "-o",
                "jsonpath={.data.value}"
            ]
        ),
        "ZHVtbXktdGVzdC1zZWNyZXQ="
    );
    kubectl(
        &kubeconfig,
        &[
            "wait",
            "--for=condition=Ready",
            "pod/archive-proof",
            "--timeout=120s",
        ],
    );
    assert_eq!(
        kubectl(
            &kubeconfig,
            &["exec", "archive-proof", "--", "cat", "/data/value"]
        ),
        "persistent-before-export"
    );
    fs::write(evidence.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "restored_nodes":3,"kubernetes_configmap_preserved":true,"kubernetes_secret_preserved":true,
        "worker_local_volume_preserved":true,"import_did_not_start_nodes":true,"sensitive_archive_retained":false,"age_encrypted_round_trip":true
    })).unwrap()).unwrap();
}

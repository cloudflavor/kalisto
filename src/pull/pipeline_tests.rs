use std::sync::Mutex;

use super::*;
use crate::process::Output;
use crate::test_support::env_lock;

#[derive(Default)]
struct RecordingRunner {
    commands: Mutex<Vec<String>>,
    fail_image_pull: bool,
}

impl CommandRunner for RecordingRunner {
    #[allow(clippy::too_many_lines)] // orchestrator; split tracked for a follow-up
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        self.commands
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        let mut stdout = String::new();
        match program {
            "curl" => {
                let destination = &args[args.iter().position(|arg| arg == "-o").unwrap() + 1];
                let content = if args.last().unwrap().contains("sha256sum-") {
                    [
                        "k3s",
                        "k3s-arm64",
                        "k3s-airgap-images-arm64.tar.zst",
                        "k3s-airgap-images-amd64.tar.zst",
                    ]
                    .iter()
                    .fold(String::new(), |mut content, name| {
                        let _ = writeln!(content, "{}  {name}", "a".repeat(64));
                        content
                    })
                } else if args.last().unwrap().ends_with("/checksums.txt") {
                    [
                        "Darwin_arm64",
                        "Darwin_x86_64",
                        "Linux_arm64",
                        "Linux_x86_64",
                    ]
                    .iter()
                    .fold(String::new(), |mut content, platform| {
                        let _ = writeln!(
                            content,
                            "{}  go-containerregistry_{platform}.tar.gz",
                            "a".repeat(64)
                        );
                        content
                    })
                } else if args.last().unwrap().ends_with(".sha256sum") {
                    let filename = args
                        .last()
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .trim_end_matches(".sha256sum");
                    format!("{}  {filename}\n", "a".repeat(64))
                } else if args.last().unwrap().ends_with("/kubectl.sha256") {
                    "a".repeat(64)
                } else if args.last().unwrap().ends_with("/k3s-images.txt") {
                    "docker.io/rancher/mirrored-coredns-coredns:1.12.3\ndocker.io/rancher/mirrored-pause:3.6\ndocker.io/rancher/klipper-lb:v0.4.13\ndocker.io/rancher/mirrored-library-traefik:3.3.6\n".to_owned()
                } else {
                    "download".to_owned()
                };
                fs::write(destination, content).unwrap();
            }
            "sha256sum" | "shasum" => stdout = format!("{}  file\n", "a".repeat(64)),
            "helm" if args[0] == "template" => {
                assert!(args.iter().any(|arg| arg == "envoy.enabled=false"));
                assert!(args.iter().any(|arg| arg == "l7Proxy=true"));
                assert!(
                    args.iter()
                        .any(|arg| arg == "readinessProbe.periodSeconds=1")
                );
                assert!(
                    args.iter()
                        .any(|arg| arg == "envoy.readinessProbe.periodSeconds=1")
                );
                stdout = "kind: DaemonSet\nmetadata:\n  name: cilium\nspec:\n  template:\n    spec:\n      containers:\n        - image: quay.io/cilium/cilium:v1\n---\nkind: Deployment\nmetadata:\n  name: cilium-operator\nspec:\n  template:\n    spec:\n      containers:\n        - image: quay.io/cilium/operator-generic:v1\n---\nkind: Secret\nmetadata:\n  name: hubble-tls\nstringData:\n  tls.key: fake-private-material-not-for-shared-cache\n".to_owned();
            }
            "helm" if args[0] == "pull" => {
                let directory =
                    &args[args.iter().position(|arg| arg == "--destination").unwrap() + 1];
                let version = &args[args.iter().position(|arg| arg == "--version").unwrap() + 1];
                fs::write(
                    Path::new(directory).join(format!("cilium-{version}.tgz")),
                    "chart",
                )
                .unwrap();
            }
            "firecracker" => stdout = "Firecracker v1.13.1".to_owned(),
            "helm" | "limactl" => {}
            "crane" if args[0] == "version" => stdout = "v0.20.3".to_owned(),
            "crane" if args[0] == "digest" => stdout = format!("sha256:{}\n", "b".repeat(64)),
            "crane" if self.fail_image_pull => {
                return Err(invalid("simulated interrupted image download"));
            }
            "crane" => {
                let layout = Path::new(args.last().unwrap());
                fs::create_dir(layout).unwrap();
                let manifests = args[5..args.len()-1].iter().map(|image| {
                    serde_json::json!({
                        "digest": image.rsplit_once('@').unwrap().1,
                        "annotations": {"org.opencontainers.image.ref.name": image.replace("docker.io/", "index.docker.io/")}
                    })
                }).collect::<Vec<_>>();
                fs::write(
                    layout.join("index.json"),
                    serde_json::to_vec(
                        &serde_json::json!({"schemaVersion":2,"manifests":manifests}),
                    )
                    .unwrap(),
                )
                .unwrap();
            }
            "tar" if args[0] == "-tzf" => stdout = "crane\nlinux-amd64/helm\n".into(),
            "tar" if args[0] == "-xzf" => {
                let destination =
                    Path::new(&args[args.iter().position(|arg| arg == "-C").unwrap() + 1]);
                fs::write(destination.join("crane"), "tool").unwrap();
                for platform in ["linux-amd64", "linux-arm64", "darwin-amd64", "darwin-arm64"] {
                    fs::create_dir_all(destination.join(platform)).unwrap();
                    fs::write(destination.join(platform).join("helm"), "tool").unwrap();
                }
            }
            "mke2fs" => {
                let source = Path::new(&args[args.iter().position(|arg| arg == "-d").unwrap() + 1]);
                assert_eq!(
                    fs::read_to_string(source.join("images.tar")).unwrap(),
                    "oci archive"
                );
                assert!(!source.join("cilium.yaml").exists());
                fs::write(args.last().unwrap(), "prepared ext4 image").unwrap();
            }
            "tar" => {
                let layout = &args[args.iter().position(|arg| arg == "-C").unwrap() + 1];
                let index: serde_json::Value = serde_json::from_slice(
                    &fs::read(Path::new(layout).join("index.json")).unwrap(),
                )
                .unwrap();
                assert!(index["manifests"].as_array().unwrap().iter().any(
                    |entry| entry["annotations"]["org.opencontainers.image.ref.name"]
                        == "docker.io/rancher/mirrored-pause:3.6"
                ));
                for manifest in index["manifests"].as_array().unwrap() {
                    assert!(
                        !manifest["annotations"]["org.opencontainers.image.ref.name"]
                            .as_str()
                            .unwrap()
                            .starts_with("index.docker.io/"),
                        "offline names must match CRI's canonical Docker registry"
                    );
                }
                let destination = &args[args.iter().position(|arg| arg == "-cf").unwrap() + 1];
                fs::write(destination, "oci archive").unwrap();
            }
            _ => {
                return Err(invalid(format!(
                    "unexpected command during pull: {program}"
                )));
            }
        }
        Ok(Output {
            stdout,
            stderr: String::new(),
        })
    }
}

struct TestEnvironment {
    root: PathBuf,
    previous: Option<std::ffi::OsString>,
}

impl TestEnvironment {
    fn new(suffix: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("kalisto-prepare-{suffix}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let previous = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root.join("data"));
        }
        Self { root, previous }
    }

    fn spec(&self) -> ClusterSpec {
        let image = self.root.join("node.qcow2");
        let template = self.root.join("node.yaml");
        fs::write(&image, "node image").unwrap();
        fs::write(
            &template,
            format!("images:\n  - location: {}\n", image.display()),
        )
        .unwrap();
        let mut spec = ClusterSpec::new("prepared".into(), self.root.clone());
        spec.backend = BackendKind::Lima;
        spec.cni_plugin = CniPlugin::Cilium;
        spec.template = template.display().to_string();
        spec
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn firecracker_pull_prepares_offline_images_and_a_shared_readonly_asset_disk() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("firecracker");
    let mut spec = ClusterSpec::new("firecracker".into(), environment.root.clone());
    spec.backend = BackendKind::Firecracker;
    spec.cni_plugin = CniPlugin::Cilium;
    let kernel = environment.root.join("vmlinux");
    let rootfs = environment.root.join("rootfs.raw");
    fs::write(&kernel, "kernel").unwrap();
    fs::write(&rootfs, "rootfs").unwrap();
    spec.firecracker_kernel = Some(kernel.clone());
    spec.firecracker_rootfs = Some(rootfs.clone());
    let runner = RecordingRunner::default();

    let (artifacts, assets) = prepare_firecracker(&spec, &runner).unwrap();

    assert_eq!(artifacts.kernel, kernel);
    assert_eq!(artifacts.rootfs, rootfs);
    assert!(
        assets
            .root
            .join(crate::firecracker_assets::DISK_FILENAME)
            .is_file()
    );
    assert!(assets.probe_image().contains("@sha256:"));
    assert_eq!(assets.rollouts().count(), 2);
    assert!(!spec.state_root().exists());
    assert!(
        crate::kubectl::binary(&spec.k3s_version, &runner)
            .unwrap()
            .is_file()
    );
    assert!(
        runner.commands.lock().unwrap().iter().all(|command| {
            !command.starts_with("limactl ")
                && !command.starts_with("container ")
                && !command.starts_with("unshare ")
        }),
        "Firecracker pull must not create nodes or prepare other backends"
    );
}

#[test]
fn pull_publishes_a_complete_profile_without_creating_cluster_state_or_vms() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("complete");
    let spec = environment.spec();
    let runner = RecordingRunner::default();
    let assets = prepare_with_runner(&spec, &runner).unwrap();
    assert!(assets.root.join("images.tar").is_file());
    assert!(!spec.state_root().exists());
    assert!(
        runner
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|command| !command.starts_with("limactl"))
    );
    assert!(assets.probe_image().contains("@sha256:"));
    assert!(
        assets
            .manifest
            .image_refs
            .contains_key("docker.io/rancher/mirrored-pause:3.6")
    );
    assert!(
        assets
            .manifest
            .image_refs
            .contains_key("docker.io/rancher/mirrored-coredns-coredns:1.12.3")
    );
    assert!(
        !assets
            .manifest
            .image_refs
            .keys()
            .any(|image| image.contains("traefik"))
    );
    assert_eq!(assets.rollouts().count(), 2);
}

#[test]
fn repeated_pull_reuses_verified_assets_without_network_or_chart_rendering() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("cached");
    let spec = environment.spec();
    let runner = RecordingRunner::default();
    prepare_with_runner(&spec, &runner).unwrap();
    runner.commands.lock().unwrap().clear();
    prepare_with_runner(&spec, &runner).unwrap();
    assert!(
        runner
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|command| command.starts_with("sha256sum "))
    );
}

#[test]
fn interrupted_pull_never_publishes_a_partial_profile_and_releases_its_lock() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("interrupted");
    let spec = environment.spec();
    let runner = RecordingRunner {
        fail_image_pull: true,
        ..RecordingRunner::default()
    };
    assert!(prepare_with_runner(&spec, &runner).is_err());
    assert!(load(&spec).is_err());
    assert!(
        fs::read_dir(cache_root().unwrap())
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == "prepare.lock")
    );
}

#[test]
fn up_rejects_missing_assets_before_creating_state_or_calling_the_backend() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("missing");
    let spec = environment.spec();
    let runner = RecordingRunner::default();
    assert!(crate::cluster::manager::up_with_runner(&spec, &runner).is_err());
    assert!(!spec.state_root().exists());
    assert!(runner.commands.lock().unwrap().is_empty());
}

#[test]
fn node_start_mounts_prepared_assets_read_only_and_stages_images_before_bootstrap() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("stage");
    let spec = environment.spec();
    let runner = RecordingRunner::default();
    let assets = prepare_with_runner(&spec, &runner).unwrap();
    let expected_mount = format!("--mount-only={} ", assets.root.display());
    let config = crate::backend::BackendConfig {
        template: spec.template.clone(),
        workspace_root: environment.root.clone(),
        enable_rosetta: false,
        network: "lima:user-v2".into(),
        firecracker_kernel: None,
        firecracker_rootfs: None,
        stream_vm_logs: false,
        boot_assets_root: Some(assets),
    };
    let node = Instance::new(
        "worker".into(),
        crate::k3s::NodeRole::Worker,
        environment.root.join("worker"),
    );
    fs::create_dir_all(&node.artifacts.root).unwrap();
    runner.commands.lock().unwrap().clear();
    crate::cluster::node::start_node_instance(
        &crate::backend::lima::BACKEND,
        &runner,
        &config,
        &node,
        None,
        2,
        4,
    )
    .unwrap();
    let commands = runner.commands.lock().unwrap();
    assert!(
        commands
            .iter()
            .any(|command| command.contains(&expected_mount))
    );
    assert!(commands.iter().any(|command| command.contains("ln -sfn")
        && command.contains("agent/images/kalisto-images.tar")));
    assert!(
        !commands
            .iter()
            .any(|command| command.contains("kalisto-k3s-images.tar.zst"))
    );
}

#[test]
fn prepared_cilium_apply_uses_local_yaml_and_waits_for_the_rendered_resources() {
    let _guard = env_lock();
    let environment = TestEnvironment::new("apply");
    let spec = environment.spec();
    let assets = prepare_with_runner(&spec, &RecordingRunner::default()).unwrap();
    let runner = RecordingRunner::default();
    let cached = fs::read_to_string(assets.root.join("cilium.yaml")).unwrap();
    assert!(!cached.contains("fake-private-material-not-for-shared-cache"));
    let node = Instance::new(
        "cp".into(),
        crate::k3s::NodeRole::ControlPlane,
        environment.root.join("cp"),
    );
    assets
        .install_cilium(
            &crate::backend::lima::BACKEND,
            &runner,
            &node,
            Path::new("/runtime/kubeconfig"),
        )
        .unwrap();
    let path = node
        .artifacts
        .root
        .join(format!("cilium-v{SCHEMA}-{}.json", assets.manifest.profile));
    let rendered = fs::read_to_string(&path).unwrap();
    assert!(rendered.contains("fake-private-material-not-for-shared-cache"));
    assets
        .install_cilium(
            &crate::backend::lima::BACKEND,
            &runner,
            &node,
            Path::new("/runtime/kubeconfig"),
        )
        .unwrap();
    let commands = runner.commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.starts_with("helm template"))
            .count(),
        1,
        "resuming a cluster must reuse its own generated TLS secrets"
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("apply --server-side --field-manager=kalisto -f"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("rollout status"))
    );
    assert!(
        commands
            .iter()
            .all(|command| !command.contains("https://helm.cilium.io"))
    );
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(path).unwrap().permissions()) & 0o777,
        0o600
    );
}

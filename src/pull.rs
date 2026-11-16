//! Prepare immutable offline k3s boot assets without creating cluster state or nodes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::artifact;
use crate::backend::{BackendKind, ClusterBackend};
use crate::cluster::addon::CniPlugin;
use crate::cluster::node::Instance;
use crate::cluster::spec::{ClusterSpec, ImageArch};
use crate::error::{Error, Result};
use crate::paths::artifacts_root;
use crate::process::{CommandRunner, SystemRunner};
use crate::shell::shell_escape;

const SCHEMA: u32 = 4;
const PROBE_IMAGE: &str = "docker.io/library/busybox:1.36.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    name: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Rollout {
    namespace: String,
    resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    profile: String,
    k3s_version: String,
    probe_image: String,
    image_refs: BTreeMap<String, String>,
    files: Vec<CachedFile>,
    rollouts: Vec<Rollout>,
}

#[derive(Debug, Clone)]
pub struct PreparedAssets {
    pub root: PathBuf,
    manifest: Manifest,
}

#[derive(Debug)]
pub enum PullOutcome {
    AppleContainer(PreparedAssets),
    Lima(PreparedAssets),
    Firecracker(artifact::ResolvedFirecrackerArtifacts),
}

impl std::fmt::Display for PullOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (backend, assets) = match self {
            Self::AppleContainer(assets) => ("apple-container", assets),
            Self::Lima(assets) => ("lima", assets),
            Self::Firecracker(artifacts) => {
                return write!(
                    formatter,
                    "backend: firecracker\nkernel: {}\nrootfs: {}",
                    artifacts.kernel.display(),
                    artifacts.rootfs.display()
                );
            }
        };
        write!(
            formatter,
            "backend: {backend}\nprepared assets: {}\nprobe image: {}",
            assets.root.display(),
            assets.probe_image()
        )
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn cache_root() -> Result<PathBuf> {
    Ok(artifacts_root()?.join(format!("boot-v{SCHEMA}")))
}

fn profile(spec: &ClusterSpec) -> Result<String> {
    if !spec.backend.uses_prepared_assets() {
        return Err(invalid(
            "pull supports the Lima and apple-container backends",
        ));
    }
    if spec.backend == BackendKind::AppleContainer
        && spec.image_arch.unwrap_or_else(ImageArch::host_default) != ImageArch::Aarch64
    {
        return Err(invalid("apple-container requires native arm64 node images"));
    }
    if spec.cni_plugin == CniPlugin::Flannel
        || spec.addons.iter().any(|addon| addon != "lb/servicelb")
    {
        return Err(invalid(
            "offline pull supports no CNI or cni/cilium and optional lb/servicelb; other add-ons are not yet supported",
        ));
    }
    for version in [&spec.k3s_version, &spec.cilium_version] {
        if version.is_empty()
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte))
            || version == "."
            || version == ".."
        {
            return Err(invalid("invalid artifact version"));
        }
    }
    let toggles = spec.component_toggles;
    let enabled = [
        !toggles.disable_traefik,
        !toggles.disable_servicelb || spec.addons.iter().any(|addon| addon == "lb/servicelb"),
        !toggles.disable_metrics_server,
        !toggles.disable_local_storage,
        !toggles.disable_coredns,
    ];
    let components = enabled
        .iter()
        .enumerate()
        .fold(0u8, |mask, (bit, enabled)| {
            mask | (u8::from(*enabled) << bit)
        });
    Ok(format!(
        "{}{}-{}-{}-{}-{components:02x}",
        if spec.backend == BackendKind::AppleContainer {
            "apple-container-v2-"
        } else {
            ""
        },
        spec.k3s_version,
        spec.image_arch
            .unwrap_or_else(ImageArch::host_default)
            .lima_artifact_suffix(),
        spec.cni_plugin.as_str(),
        if spec.cni_plugin == CniPlugin::Cilium {
            &spec.cilium_version
        } else {
            "base"
        },
    ))
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn prepare(spec: &ClusterSpec) -> Result<PullOutcome> {
    match spec.backend {
        BackendKind::AppleContainer => {
            prepare_apple_container(spec, &SystemRunner).map(PullOutcome::AppleContainer)
        }
        BackendKind::Lima => prepare_lima(spec, &SystemRunner).map(PullOutcome::Lima),
        BackendKind::Firecracker => {
            prepare_firecracker(spec, &SystemRunner).map(PullOutcome::Firecracker)
        }
    }
}

fn prepare_apple_container(
    spec: &ClusterSpec,
    runner: &dyn CommandRunner,
) -> Result<PreparedAssets> {
    profile(spec)?;
    crate::runtime::prepare(
        runner,
        BackendKind::AppleContainer,
        spec.cni_plugin == CniPlugin::Cilium,
    )?;
    crate::apple_container::runtime::prepare(runner)?;
    crate::kubectl::binary(&spec.k3s_version, runner)?;
    prepare_with_runner(spec, runner)
}

fn prepare_lima(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<PreparedAssets> {
    profile(spec)?;
    crate::runtime::prepare(
        runner,
        BackendKind::Lima,
        spec.cni_plugin == CniPlugin::Cilium,
    )?;
    crate::kubectl::binary(&spec.k3s_version, runner)?;
    prepare_with_runner(spec, runner)
}

fn prepare_firecracker(
    spec: &ClusterSpec,
    runner: &dyn CommandRunner,
) -> Result<artifact::ResolvedFirecrackerArtifacts> {
    let artifacts = artifact::prepare_firecracker(spec, runner)?;
    crate::kubectl::binary(&spec.k3s_version, runner)?;
    Ok(artifacts)
}

#[allow(clippy::too_many_lines)] // orchestrator; split tracked for a follow-up
/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn prepare_with_runner(
    spec: &ClusterSpec,
    runner: &dyn CommandRunner,
) -> Result<PreparedAssets> {
    let key = profile(spec)?;
    let cache = cache_root()?;
    fs::create_dir_all(&cache).map_err(io_error)?;
    let destination = cache.join(&key);
    // Serialize node-image downloads as well as publication of boot profiles.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache.join("prepare.lock"))
        .map_err(io_error)?;
    lock.try_lock()
        .map_err(|error| invalid(format!("another asset preparation is active: {error}")))?;
    let mut scratch = PullScratch {
        _lock: lock,
        staging: None,
    };
    if spec.backend == BackendKind::Lima {
        let resolved = artifact::pull_cluster_spec(spec, runner)?;
        validate_lima_template(&resolved.template)?;
    }
    if spec
        .state_root()
        .join("archive/assets/manifest.json")
        .is_file()
    {
        let assets = load(spec)?;
        for file in &assets.manifest.files {
            artifact::verify_sha256(
                runner,
                &assets.root.join(&file.name),
                &file.sha256,
                &file.name,
            )?;
        }
        return Ok(assets);
    }
    if destination.exists() {
        let assets = load(spec).map_err(|error| invalid(format!("{error}; damaged cache at {}; stop clusters using this profile, remove only this cache directory, then rerun kalisto pull", destination.display())))?;
        for file in &assets.manifest.files {
            artifact::verify_sha256(
                runner,
                &assets.root.join(&file.name),
                &file.sha256,
                &file.name,
            )?;
        }
        if spec.backend == BackendKind::AppleContainer {
            let image = crate::apple_container::image::prepare(spec, &assets.root, runner)?;
            if image != assets.apple_node_image()? {
                return Err(invalid("cached Apple node image identity changed"));
            }
        }
        return Ok(assets);
    }

    runner
        .run("crane", &["version".into()])
        .map_err(|error| invalid(format!("pull requires crane >= 0.20.3: {error}")))?;
    if spec.cni_plugin == CniPlugin::Cilium {
        runner
            .run("helm", &["version".into(), "--short".into()])
            .map_err(|error| invalid(format!("pull requires helm for Cilium: {error}")))?;
    }

    // Publishing a whole directory keeps failed/interrupted pulls invisible to up.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    let staging = cache.join(format!(".prepare-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging).map_err(io_error)?;
    scratch.staging = Some(staging.clone());
    let arch = spec
        .image_arch
        .unwrap_or_else(ImageArch::host_default)
        .lima_artifact_suffix();
    let release = format!(
        "https://github.com/k3s-io/k3s/releases/download/{}",
        spec.k3s_version
    );
    let sums_name = format!("sha256sum-{arch}.txt");
    let downloads = k3s_download_root(spec)?;
    let sums_path = artifact::fetch_into_cache(
        runner,
        &downloads,
        &format!("{release}/{sums_name}"),
        None,
        "K3s release checksums",
        &sums_name,
    )?;
    let sums = fs::read_to_string(sums_path).map_err(io_error)?;
    let binary = if arch == "arm64" { "k3s-arm64" } else { "k3s" };
    let archive = format!("k3s-airgap-images-{arch}.tar.zst");
    let mut files = Vec::new();
    for (remote, local) in [(binary, "k3s"), (archive.as_str(), "k3s-images.tar.zst")] {
        let sha256 = release_checksum(&sums, remote)?;
        let path = staging.join(local);
        let cached = artifact::fetch_into_cache(
            runner,
            &downloads,
            &format!("{release}/{remote}"),
            Some(&sha256),
            local,
            remote,
        )?;
        if fs::hard_link(&cached, &path).is_err() {
            fs::copy(&cached, &path).map_err(io_error)?;
        }
        files.push(cached_file(&path, local, sha256)?);
    }

    let mut documents = if spec.cni_plugin == CniPlugin::Cilium {
        runner.run(
            "helm",
            &[
                "pull".into(),
                "cilium".into(),
                "--repo".into(),
                "https://helm.cilium.io/".into(),
                "--version".into(),
                spec.cilium_version.clone(),
                "--destination".into(),
                staging.display().to_string(),
            ],
        )?;
        let chart = staging.join(format!("cilium-{}.tgz", spec.cilium_version));
        files.push(hash_file(
            runner,
            &staging,
            &format!("cilium-{}.tgz", spec.cilium_version),
        )?);
        render_cilium(runner, &chart, &spec.k3s_version)?
    } else {
        Vec::new()
    };
    let mut images = BTreeSet::from([PROBE_IMAGE.to_owned()]);
    // K3s' full air-gap archive also contains disabled ingress, metrics and
    // storage images. Import only the components enabled by this boot profile.
    let image_list = artifact::fetch_into_cache(
        runner,
        &downloads,
        &format!("{release}/k3s-images.txt"),
        None,
        "K3s release image inventory",
        "k3s-images.txt",
    )?;
    images.extend(required_k3s_images(
        &fs::read_to_string(image_list).map_err(io_error)?,
        spec,
    )?);
    for document in &documents {
        collect_images(document, &mut images);
    }
    let mut pinned = BTreeMap::new();
    for image in images {
        let output = runner.run(
            "crane",
            &[
                "digest".into(),
                "--platform".into(),
                format!("linux/{arch}"),
                image.clone(),
            ],
        )?;
        pinned.insert(image.clone(), pin_reference(&image, output.stdout.trim())?);
    }
    for document in &mut documents {
        pin_images(document, &pinned)?;
    }
    // Hubble's default Helm TLS generator emits private keys. Cache assets,
    // never cluster identities; up renders the cached chart per cluster.
    documents.retain(|document| document.get("kind").and_then(Value::as_str) != Some("Secret"));
    let rollouts = rollout_inventory(&documents)?;
    if spec.cni_plugin == CniPlugin::Cilium && rollouts.is_empty() {
        return Err(invalid(
            "rendered Cilium chart contains no rollout resources",
        ));
    }
    if !documents.is_empty() {
        let mut yaml = String::from(
            "# Asset inventory only; kalisto up renders private per-cluster TLS secrets.\n",
        );
        for document in &documents {
            yaml.push_str("---\n");
            yaml.push_str(
                &serde_yaml::to_string(document).map_err(|error| invalid(error.to_string()))?,
            );
        }
        fs::write(staging.join("cilium.yaml"), yaml).map_err(io_error)?;
        files.push(hash_file(runner, &staging, "cilium.yaml")?);
    }

    // Preserve OCI manifests/digests. Docker-save archives can rewrite manifests
    // and break digest-pinned imagePullPolicy=Never lookups after import.
    let layout = staging.join("oci");
    let mut args = vec![
        "pull".into(),
        "--format=oci".into(),
        "--annotate-ref".into(),
        "--platform".into(),
        format!("linux/{arch}"),
    ];
    args.extend(pinned.values().cloned().collect::<BTreeSet<_>>());
    args.push(layout.display().to_string());
    runner.run("crane", &args)?;
    canonicalize_oci_index(&layout.join("index.json"), &pinned)?;
    runner.run(
        "tar",
        &[
            "-C".into(),
            layout.display().to_string(),
            "-cf".into(),
            staging.join("images.tar").display().to_string(),
            ".".into(),
        ],
    )?;
    files.push(hash_file(runner, &staging, "images.tar")?);
    fs::remove_dir_all(layout).map_err(io_error)?;
    if spec.backend == BackendKind::AppleContainer {
        let image = crate::apple_container::image::prepare(spec, &staging, runner)?;
        fs::write(staging.join("apple-node-image"), format!("{image}\n")).map_err(io_error)?;
        files.push(hash_file(runner, &staging, "apple-node-image")?);
        files.push(hash_file(runner, &staging, "apple-node-image.tar")?);
    }
    let manifest = Manifest {
        schema: SCHEMA,
        profile: key,
        k3s_version: spec.k3s_version.clone(),
        probe_image: pinned[PROBE_IMAGE].clone(),
        image_refs: pinned,
        files,
        rollouts,
    };
    let content =
        serde_json::to_vec_pretty(&manifest).map_err(|error| invalid(error.to_string()))?;
    fs::write(staging.join("manifest.json"), content).map_err(io_error)?;
    fs::rename(&staging, &destination).map_err(io_error)?;
    scratch.staging = None;
    load(spec)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn load(spec: &ClusterSpec) -> Result<PreparedAssets> {
    let imported = spec.state_root().join("archive/assets");
    if imported.join("manifest.json").is_file() {
        return load_from_root(spec, &imported);
    }
    load_from_root(spec, &cache_root()?.join(profile(spec)?))
}

pub(crate) fn load_from_root(spec: &ClusterSpec, root: &Path) -> Result<PreparedAssets> {
    let key = profile(spec)?;
    let content = fs::read(root.join("manifest.json")).map_err(|error| invalid(format!(
        "boot assets unavailable at {}: {error}; run `kalisto pull` with the same --arch, --k3s-version and --addons before up",
        root.display(),
    )))?;
    let manifest: Manifest = serde_json::from_slice(&content)
        .map_err(|error| invalid(format!("invalid boot asset manifest: {error}")))?;
    if manifest.schema != SCHEMA
        || manifest.profile != key
        || manifest.k3s_version != spec.k3s_version
    {
        return Err(invalid("boot asset profile mismatch; run kalisto pull"));
    }
    let chart_name = format!("cilium-{}.tgz", spec.cilium_version);
    let mut required = if spec.cni_plugin == CniPlugin::Cilium {
        vec![
            "k3s",
            "k3s-images.tar.zst",
            "images.tar",
            "cilium.yaml",
            chart_name.as_str(),
        ]
    } else {
        vec!["k3s", "k3s-images.tar.zst", "images.tar"]
    };
    if spec.backend == BackendKind::AppleContainer {
        required.extend(["apple-node-image", "apple-node-image.tar"]);
    }
    let names = manifest
        .files
        .iter()
        .map(|file| file.name.as_str())
        .collect::<BTreeSet<_>>();
    if names != required.into_iter().collect::<BTreeSet<_>>() || names.len() != manifest.files.len()
    {
        return Err(invalid("incomplete boot asset inventory; run kalisto pull"));
    }
    for file in &manifest.files {
        let metadata = fs::metadata(root.join(&file.name)).map_err(io_error)?;
        if !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() != file.bytes
            || !valid_digest(&format!("sha256:{}", file.sha256))
        {
            return Err(invalid(format!(
                "cached asset {} is invalid; repair cache with kalisto pull before up",
                file.name
            )));
        }
    }
    if !valid_image_reference(&manifest.probe_image)
        || manifest.image_refs.get(PROBE_IMAGE) != Some(&manifest.probe_image)
        || manifest
            .image_refs
            .values()
            .any(|image| !valid_image_reference(image))
    {
        return Err(invalid("invalid cached probe image"));
    }
    if spec.cni_plugin == CniPlugin::Cilium {
        let documents =
            parse_documents(&fs::read_to_string(root.join("cilium.yaml")).map_err(io_error)?)?;
        if documents
            .iter()
            .any(|document| document.get("kind").and_then(Value::as_str) == Some("Secret"))
        {
            return Err(invalid(
                "shared boot cache must not contain cluster secrets",
            ));
        }
        if manifest.rollouts.is_empty() || manifest.rollouts != rollout_inventory(&documents)? {
            return Err(invalid("cached Cilium rollout inventory mismatch"));
        }
    }
    let assets = PreparedAssets {
        root: root.canonicalize().map_err(io_error)?,
        manifest,
    };
    if spec.backend == BackendKind::AppleContainer {
        assets.apple_node_image()?;
    }
    Ok(assets)
}

impl PreparedAssets {
    pub(crate) fn filenames(&self) -> impl Iterator<Item = &str> {
        self.manifest.files.iter().map(|file| file.name.as_str())
    }

    pub(crate) fn apple_node_image(&self) -> Result<String> {
        let reference = fs::read_to_string(self.root.join("apple-node-image")).map_err(io_error)?;
        let reference = reference.trim();
        if !valid_image_reference(reference) {
            return Err(invalid("cached Apple node image must be digest-pinned"));
        }
        Ok(reference.to_owned())
    }

    #[must_use]
    pub fn probe_image(&self) -> &str {
        &self.manifest.probe_image
    }

    pub fn rollouts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.manifest
            .rollouts
            .iter()
            .map(|rollout| (rollout.namespace.as_str(), rollout.resource.as_str()))
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn stage_node(
        &self,
        backend: &dyn ClusterBackend,
        runner: &dyn CommandRunner,
        node: &Instance,
    ) -> Result<()> {
        let binary = shell_escape(&self.root.join("k3s").display().to_string());
        let version = shell_escape(&self.manifest.k3s_version);
        let mut command = format!(
            "set -eu\nif [ -e /var/lib/rancher/k3s/server/manifests/kalisto-cilium.yaml ]; then echo 'legacy Helm-managed Cilium requires migration; use a fresh cluster for prepared startup' >&2; exit 1; fi\ncurrent=$(/usr/local/bin/k3s --version 2>/dev/null | awk 'NR==1 {{print $3}}')\nif [ \"$current\" != {version} ]; then\n  if ps -eo comm= | grep -Eq '^[[:space:]]*k3s-(server|agent)$'; then echo 'stop the cluster before changing k3s version' >&2; exit 1; fi\n  install -m 0755 {binary} /usr/local/bin/k3s\nfi\nmkdir -p /var/lib/rancher/k3s/agent/images\ntouch /var/lib/rancher/k3s/agent/images/.cache.json\n"
        );
        let _ = writeln!(
            command,
            "ln -sfn {} /var/lib/rancher/k3s/agent/images/kalisto-images.tar",
            shell_escape(&self.root.join("images.tar").display().to_string()),
        );
        backend.guest_sudo_shell(runner, &node.name, &node.artifacts.root, &command)?;
        Ok(())
    }

    pub(crate) fn prepare_cilium_manifest(
        &self,
        runner: &dyn CommandRunner,
        node: &Instance,
    ) -> Result<PathBuf> {
        let started = std::time::Instant::now();
        let host_manifest = node
            .artifacts
            .root
            .join(format!("cilium-v{SCHEMA}-{}.json", self.manifest.profile));
        if !host_manifest.exists() {
            fs::create_dir_all(&node.artifacts.root).map_err(io_error)?;
            let chart = self
                .manifest
                .files
                .iter()
                // Cache manifest file names are generated locally and always lowercase.
                .find(|file| {
                    std::path::Path::new(&file.name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("tgz"))
                })
                .ok_or_else(|| invalid("cached Cilium chart is missing"))?;
            let mut documents = render_cilium(
                runner,
                &self.root.join(&chart.name),
                &self.manifest.k3s_version,
            )?;
            for document in &mut documents {
                pin_images(document, &self.manifest.image_refs)?;
            }
            if rollout_inventory(&documents)? != self.manifest.rollouts {
                return Err(invalid("cached chart rollout inventory changed"));
            }
            // JSON preserves scalar types across Kubernetes' YAML 1.1 decoder.
            let content = serde_json::to_vec(&serde_json::json!({
                "apiVersion": "v1", "kind": "List", "items": documents,
            }))
            .map_err(|error| invalid(error.to_string()))?;
            write_private_manifest(&host_manifest, &content)?;
            tracing::info!(elapsed = ?started.elapsed(), "local Cilium TLS manifest rendered");
        }
        Ok(host_manifest)
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn install_cilium(
        &self,
        backend: &dyn ClusterBackend,
        runner: &dyn CommandRunner,
        node: &Instance,
        kubeconfig: &Path,
    ) -> Result<()> {
        let kubectl = format!(
            "KUBECONFIG={} /usr/local/bin/k3s kubectl",
            shell_escape(&kubeconfig.display().to_string())
        );
        let host_manifest = self.prepare_cilium_manifest(runner, node)?;
        let guest_manifest = backend
            .guest_artifact_root(&node.name, &node.artifacts.root)?
            .join(
                host_manifest
                    .file_name()
                    .ok_or_else(|| invalid("invalid per-cluster manifest path"))?,
            );
        let mut command = format!(
            "{kubectl} apply --server-side --field-manager=kalisto -f {}",
            shell_escape(&guest_manifest.display().to_string())
        );
        for rollout in &self.manifest.rollouts {
            let _ = write!(
                command,
                " && {kubectl} -n {} rollout status {} --timeout=60s",
                shell_escape(&rollout.namespace),
                shell_escape(&rollout.resource)
            );
        }
        backend.guest_sudo_shell(runner, &node.name, &node.artifacts.root, &command)?;
        Ok(())
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn validate_lima_template(template: &str) -> Result<()> {
    let path = Path::new(template);
    let content = fs::read_to_string(path).map_err(|error| {
        invalid(format!(
            "offline startup requires a local Lima template: {template}: {error}"
        ))
    })?;
    let value: Value =
        serde_yaml::from_str(&content).map_err(|error| invalid(error.to_string()))?;
    if value.get("base").is_some() {
        return Err(invalid(
            "offline startup does not support inherited Lima templates; use a flattened managed node template",
        ));
    }
    let images = value
        .get("images")
        .and_then(Value::as_sequence)
        .filter(|images| !images.is_empty())
        .ok_or_else(|| invalid("offline Lima template must contain local images"))?;
    for image in images {
        let location = image
            .get("location")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Lima image location is missing"))?;
        let location = location.strip_prefix("file://").unwrap_or(location);
        if !Path::new(location).is_absolute() || !Path::new(location).is_file() {
            return Err(invalid(format!(
                "Lima image must be a prepared absolute local file, not {location}"
            )));
        }
    }
    Ok(())
}

fn render_cilium(
    runner: &dyn CommandRunner,
    chart: &Path,
    k3s_version: &str,
) -> Result<Vec<Value>> {
    let kubernetes_version = k3s_version
        .trim_start_matches('v')
        .split('+')
        .next()
        .unwrap_or_default();
    let output = runner.run("helm", &[
        "template".into(), "cilium".into(), chart.display().to_string(),
        "--namespace".into(), "kube-system".into(), "--include-crds".into(), "--skip-tests".into(),
        "--kube-version".into(), kubernetes_version.to_owned(),
        "--set".into(), "operator.replicas=1".into(),
        // Embedded Envoy keeps L7 policy support without three separately
        // life-cycled proxy pods in a small local cluster.
        "--set".into(), "envoy.enabled=false".into(),
        "--set".into(), "l7Proxy=true".into(),
        // Keep the same health checks, without up to 30s of observation lag.
        "--set".into(), "readinessProbe.periodSeconds=1".into(),
        "--set".into(), "envoy.readinessProbe.periodSeconds=1".into(),
        "--set".into(), "ipam.operator.clusterPoolIPv4PodCIDRList[0]=10.42.0.0/16".into(),
    ]).map_err(|error| match error {
        // Partial Helm stdout can contain generated TLS keys; never log it.
        Error::CommandFailed { code, .. } => invalid(format!("local Cilium chart rendering failed with exit code {code:?}; diagnostics redacted because partial output may contain private keys")),
        other => other,
    })?;
    let mut documents = parse_documents(&output.stdout)?;
    for document in &mut documents {
        if let Some(containers) = document
            .get_mut("spec")
            .and_then(|value| value.get_mut("template"))
            .and_then(|value| value.get_mut("spec"))
            .and_then(|value| value.get_mut("containers"))
            .and_then(Value::as_sequence_mut)
        {
            for container in containers {
                if container.get("name").and_then(Value::as_str) != Some("cilium-agent") {
                    continue;
                }
                if let Some(probe) = container
                    .get_mut("startupProbe")
                    .filter(|probe| probe.is_mapping())
                {
                    let period = probe
                        .get("periodSeconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(10);
                    let attempts = probe
                        .get("failureThreshold")
                        .and_then(Value::as_u64)
                        .unwrap_or(3);
                    // Same health endpoint and failure window, observed sooner.
                    probe["initialDelaySeconds"] = Value::Number(0.into());
                    probe["periodSeconds"] = Value::Number(1.into());
                    probe["failureThreshold"] =
                        Value::Number(period.saturating_mul(attempts).into());
                }
            }
        }
    }
    Ok(documents)
}

fn required_k3s_images(inventory: &str, spec: &ClusterSpec) -> Result<BTreeSet<String>> {
    let mut images = BTreeSet::new();
    let mut dns = false;
    let mut pause = false;
    for image in inventory
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let repository = image
            .rsplit_once(':')
            .map_or(image, |(repository, _)| repository);
        if !image.starts_with("docker.io/rancher/") {
            continue;
        }
        let core_dns = repository.ends_with("/mirrored-coredns-coredns");
        let sandbox = repository.ends_with("/mirrored-pause");
        dns |= core_dns;
        pause |= sandbox;
        let toggles = spec.component_toggles;
        let needed = sandbox
            || (core_dns && !toggles.disable_coredns)
            || (!toggles.disable_traefik
                && (repository.ends_with("/mirrored-library-traefik")
                    || repository.ends_with("/klipper-helm")))
            || (!toggles.disable_metrics_server
                && repository.ends_with("/mirrored-metrics-server"))
            || (!toggles.disable_local_storage
                && (repository.ends_with("/local-path-provisioner")
                    || repository.ends_with("/mirrored-library-busybox")))
            || (repository.ends_with("/klipper-lb")
                && (!toggles.disable_servicelb
                    || spec.addons.iter().any(|addon| addon == "lb/servicelb")));
        if needed {
            images.insert(image.to_owned());
        }
    }
    if !dns || !pause {
        return Err(invalid(
            "K3s release inventory lacks required DNS/sandbox images",
        ));
    }
    Ok(images)
}

fn write_private_manifest(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("per-cluster manifest parent is missing"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    let temporary = path.with_extension(format!("{nonce}.tmp"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(io_error)?;
    let result = file
        .write_all(content)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(io_error)
}

fn parse_documents(content: &str) -> Result<Vec<Value>> {
    serde_yaml::Deserializer::from_str(content)
        .map(|document| {
            let mut value =
                Value::deserialize(document).map_err(|error| invalid(error.to_string()))?;
            normalize_volume_modes(&mut value);
            Ok(value)
        })
        .collect()
}

fn canonicalize_oci_index(path: &Path, pinned: &BTreeMap<String, String>) -> Result<()> {
    let mut index: serde_json::Value = serde_json::from_slice(&fs::read(path).map_err(io_error)?)
        .map_err(|error| invalid(error.to_string()))?;
    let manifests = index
        .get_mut("manifests")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| invalid("OCI index has no manifests"))?;
    let mut actual = BTreeSet::new();
    for manifest in manifests.iter_mut() {
        let digest = manifest
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let annotations = manifest
            .get_mut("annotations")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| invalid("OCI image has no reference annotation"))?;
        for key in [
            "org.opencontainers.image.ref.name",
            "io.containerd.image.name",
        ] {
            if let Some(name) = annotations.get(key).and_then(serde_json::Value::as_str)
                && let Some(rest) = name.strip_prefix("index.docker.io/")
            {
                let canonical = format!("docker.io/{rest}");
                annotations.insert(key.to_owned(), canonical.into());
            }
        }
        let name = annotations
            .get("org.opencontainers.image.ref.name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid("OCI image reference is missing"))?;
        if name.rsplit_once('@').map(|(_, hash)| hash) != Some(digest.as_str()) {
            return Err(invalid("OCI descriptor digest differs from pinned image"));
        }
        actual.insert(name.to_owned());
    }
    if actual != pinned.values().cloned().collect::<BTreeSet<_>>() {
        return Err(invalid(
            "OCI image names differ from offline workload references",
        ));
    }
    // K3s' built-in manifests request release tags. Keep their exact canonical
    // aliases as well as immutable names; both refer to the verified digest.
    let aliases = pinned
        .iter()
        .filter(|(source, _)| source.starts_with("docker.io/rancher/") && !source.contains('@'))
        .map(|(source, target)| {
            let mut descriptor = manifests
                .iter()
                .find(|entry| {
                    entry["annotations"]["org.opencontainers.image.ref.name"].as_str()
                        == Some(target.as_str())
                })
                .cloned()
                .ok_or_else(|| invalid("missing K3s image descriptor"))?;
            descriptor["annotations"]["org.opencontainers.image.ref.name"] = source.clone().into();
            descriptor["annotations"]["io.containerd.image.name"] = source.clone().into();
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>>>()?;
    manifests.extend(aliases);
    fs::write(
        path,
        serde_json::to_vec(&index).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)
}

fn normalize_volume_modes(value: &mut Value) {
    match value {
        Value::Mapping(map) => {
            for (key, value) in map {
                let field = key.as_str().unwrap_or_default();
                if matches!(field, "data" | "stringData" | "annotations" | "labels") {
                    continue;
                }
                if matches!(field, "defaultMode" | "mode")
                    && let Some(mode) = value.as_str()
                    && mode.starts_with('0')
                    && mode.len() > 1
                    && mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
                    && let Ok(number) = u32::from_str_radix(mode, 8)
                {
                    *value = Value::Number(number.into());
                } else {
                    normalize_volume_modes(value);
                }
            }
        }
        Value::Sequence(values) => {
            for value in values {
                normalize_volume_modes(value);
            }
        }
        _ => {}
    }
}

fn collect_images(value: &Value, images: &mut BTreeSet<String>) {
    match value {
        Value::Mapping(map) => {
            if let Some(image) = map.get("image").and_then(Value::as_str) {
                images.insert(image.to_owned());
            }
            for value in map.values() {
                collect_images(value, images);
            }
        }
        Value::Sequence(values) => {
            for value in values {
                collect_images(value, images);
            }
        }
        _ => {}
    }
}

fn pin_images(value: &mut Value, images: &BTreeMap<String, String>) -> Result<()> {
    match value {
        Value::Mapping(map) => {
            let key = Value::String("image".into());
            if let Some(image) = map.get(&key).and_then(Value::as_str) {
                let pinned = images
                    .get(image)
                    .ok_or_else(|| invalid(format!("unresolved image {image}")))?
                    .clone();
                map.insert(key, Value::String(pinned));
                map.insert(
                    Value::String("imagePullPolicy".into()),
                    Value::String("Never".into()),
                );
            }
            for value in map.values_mut() {
                pin_images(value, images)?;
            }
        }
        Value::Sequence(values) => {
            for value in values {
                pin_images(value, images)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn rollout_inventory(documents: &[Value]) -> Result<Vec<Rollout>> {
    let mut rollouts = Vec::new();
    for document in documents {
        let kind = match document.get("kind").and_then(Value::as_str) {
            Some("DaemonSet") => "daemonset",
            Some("Deployment") => "deployment",
            _ => continue,
        };
        let metadata = &document["metadata"];
        let name = metadata
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("rollout name is missing"))?;
        let namespace = metadata
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or("kube-system");
        rollouts.push(Rollout {
            namespace: namespace.to_owned(),
            resource: format!("{kind}/{name}"),
        });
    }
    Ok(rollouts)
}

fn pin_reference(image: &str, digest: &str) -> Result<String> {
    if !valid_digest(digest) || image.is_empty() || image.chars().any(char::is_whitespace) {
        return Err(invalid(format!(
            "invalid resolved image digest for {image}"
        )));
    }
    let reference = image.split('@').next().unwrap_or_default();
    let repository = match reference.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => reference,
    };
    Ok(format!("{repository}@{digest}"))
}

fn valid_digest(digest: &str) -> bool {
    digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_image_reference(image: &str) -> bool {
    image
        .rsplit_once('@')
        .is_some_and(|(repository, digest)| !repository.is_empty() && valid_digest(digest))
}

fn release_checksum(content: &str, filename: &str) -> Result<String> {
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(checksum), Some(name)) = (fields.next(), fields.next())
            && name.trim_start_matches('*') == filename
            && valid_digest(&format!("sha256:{checksum}"))
        {
            return Ok(checksum.to_owned());
        }
    }
    Err(invalid(format!(
        "K3s release checksum missing for {filename}"
    )))
}

fn k3s_download_root(spec: &ClusterSpec) -> Result<PathBuf> {
    Ok(artifacts_root()?
        .join("downloads")
        .join(&spec.k3s_version)
        .join(
            spec.image_arch
                .unwrap_or_else(ImageArch::host_default)
                .lima_artifact_suffix(),
        ))
}

pub(crate) fn k3s_binary(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<PathBuf> {
    let root = k3s_download_root(spec)?;
    let arch = spec
        .image_arch
        .unwrap_or_else(ImageArch::host_default)
        .lima_artifact_suffix();
    let release = format!(
        "https://github.com/k3s-io/k3s/releases/download/{}",
        spec.k3s_version
    );
    let sums_name = format!("sha256sum-{arch}.txt");
    let sums = artifact::fetch_into_cache(
        runner,
        &root,
        &format!("{release}/{sums_name}"),
        None,
        "K3s checksums",
        &sums_name,
    )?;
    let filename = if arch == "arm64" { "k3s-arm64" } else { "k3s" };
    let sha = release_checksum(&fs::read_to_string(sums).map_err(io_error)?, filename)?;
    artifact::fetch_into_cache(
        runner,
        &root,
        &format!("{release}/{filename}"),
        Some(&sha),
        "K3s binary",
        filename,
    )
}

fn cached_file(path: &Path, name: &str, sha256: String) -> Result<CachedFile> {
    let bytes = fs::metadata(path).map_err(io_error)?.len();
    if bytes == 0 {
        return Err(invalid(format!("empty downloaded asset {name}")));
    }
    Ok(CachedFile {
        name: name.to_owned(),
        bytes,
        sha256,
    })
}

fn hash_file(runner: &dyn CommandRunner, root: &Path, name: &str) -> Result<CachedFile> {
    let path = root.join(name);
    cached_file(&path, name, artifact::file_sha256(runner, &path)?)
}

struct PullScratch {
    _lock: fs::File,
    staging: Option<PathBuf>,
}
impl Drop for PullScratch {
    fn drop(&mut self) {
        if let Some(staging) = &self.staging {
            let _ = fs::remove_dir_all(staging);
        }
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}
fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "boot asset cache".to_owned(),
        source,
    }
}

#[cfg(test)]
mod pipeline_tests;

#[cfg(test)]
pub(crate) fn test_fixture(spec: &ClusterSpec) {
    let root = cache_root().unwrap().join(profile(spec).unwrap());
    fs::create_dir_all(&root).unwrap();
    let mut files = Vec::new();
    for name in ["k3s", "k3s-images.tar.zst", "images.tar"] {
        fs::write(root.join(name), "test").unwrap();
        files.push(CachedFile {
            name: name.to_owned(),
            bytes: 4,
            sha256: "a".repeat(64),
        });
    }
    let manifest = Manifest {
        schema: SCHEMA,
        profile: profile(spec).unwrap(),
        k3s_version: spec.k3s_version.clone(),
        probe_image: format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        image_refs: BTreeMap::from([(
            PROBE_IMAGE.to_owned(),
            format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        )]),
        files,
        rollouts: Vec::new(),
    };
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn kubernetes_octal_permissions_are_numbers_but_config_and_labels_stay_strings() {
        let docs = super::parse_documents("kind: Pod\nmetadata:\n  labels:\n    mode: '0400'\nspec:\n  volumes:\n  - projected:\n      defaultMode: 0400\n  - secret:\n      items:\n      - key: token\n        path: token\n        mode: 0644\n---\nkind: ConfigMap\ndata:\n  mode: '0400'\n").unwrap();
        assert_eq!(
            docs[0]["spec"]["volumes"][0]["projected"]["defaultMode"].as_u64(),
            Some(256)
        );
        assert_eq!(
            docs[0]["spec"]["volumes"][1]["secret"]["items"][0]["mode"].as_u64(),
            Some(420)
        );
        assert_eq!(docs[0]["metadata"]["labels"]["mode"].as_str(), Some("0400"));
        assert_eq!(docs[1]["data"]["mode"].as_str(), Some("0400"));
    }
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn platform_digest_replaces_index_digest_and_preserves_registry_port() {
        let digest = format!("sha256:{}", "b".repeat(64));
        assert_eq!(
            pin_reference("localhost:5000/cilium:v1@sha256:old", &digest).unwrap(),
            format!("localhost:5000/cilium@{digest}")
        );
    }

    #[test]
    fn malformed_registry_digest_is_rejected() {
        assert!(pin_reference("quay.io/cilium/cilium:v1", "sha256:short").is_err());
    }

    #[test]
    fn release_checksum_requires_exact_filename() {
        let checksum = "a".repeat(64);
        assert!(release_checksum(&format!("{checksum}  k3s-arm64-extra\n"), "k3s-arm64").is_err());
    }

    #[test]
    fn release_checksum_accepts_binary_marker() {
        let checksum = "a".repeat(64);
        assert_eq!(
            release_checksum(&format!("{checksum} *k3s-arm64\n"), "k3s-arm64").unwrap(),
            checksum
        );
    }

    #[test]
    fn rendered_containers_and_init_containers_are_pinned_without_registry_fallback() {
        let mut documents = parse_documents("kind: DaemonSet\nmetadata:\n  name: cilium\nspec:\n  template:\n    spec:\n      initContainers:\n        - image: quay.io/cilium/cilium:v1\n      containers:\n        - image: quay.io/cilium/cilium:v1\n          imagePullPolicy: Always\n").unwrap();
        let mut images = BTreeSet::new();
        collect_images(&documents[0], &mut images);
        let pinned = format!("quay.io/cilium/cilium@sha256:{}", "c".repeat(64));
        let replacements =
            BTreeMap::from([("quay.io/cilium/cilium:v1".to_owned(), pinned.clone())]);
        pin_images(&mut documents[0], &replacements).unwrap();
        let pod = &documents[0]["spec"]["template"]["spec"];
        assert_eq!(images.len(), 1);
        for field in ["containers", "initContainers"] {
            assert_eq!(pod[field][0]["image"].as_str(), Some(pinned.as_str()));
            assert_eq!(pod[field][0]["imagePullPolicy"].as_str(), Some("Never"));
        }
    }

    #[test]
    fn rollout_inventory_includes_envoy_when_chart_enables_it() {
        let documents = parse_documents("kind: DaemonSet\nmetadata:\n  name: cilium-envoy\n  namespace: kube-system\n---\nkind: Deployment\nmetadata:\n  name: cilium-operator\n").unwrap();
        assert_eq!(
            rollout_inventory(&documents)
                .unwrap()
                .iter()
                .map(|rollout| rollout.resource.as_str())
                .collect::<Vec<_>>(),
            vec!["daemonset/cilium-envoy", "deployment/cilium-operator"]
        );
    }

    #[test]
    fn cache_rejects_truncated_archives_without_starting_vms() {
        let _guard = env_lock();
        let old = std::env::var_os("XDG_DATA_HOME");
        let root = std::env::temp_dir().join(format!("kalisto-pull-cache-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &root);
        }
        let mut spec = ClusterSpec::new("cache".into(), root.clone());
        spec.backend = BackendKind::Lima;
        test_fixture(&spec);
        let assets = load(&spec).unwrap();
        fs::write(assets.root.join("images.tar"), "x").unwrap();
        assert!(load(&spec).is_err());
        unsafe {
            match old {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_addons_fail_before_preparation() {
        let mut spec = ClusterSpec::new("offline".into(), ".".into());
        spec.backend = BackendKind::Lima;
        spec.addons.push("storage/longhorn".into());
        assert!(profile(&spec).is_err());
    }
}

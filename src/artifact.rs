use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::backend::BackendKind;
use crate::cluster::spec::{ClusterSpec, DEFAULT_K3S_VERSION, ImageArch};
use crate::error::{Error, Result};
use crate::paths::{artifacts_root, config_root};
use crate::process::CommandRunner;
use crate::shell::shell_escape;

const LIMA_ARTIFACT_DIR: &str = "node-image";
const FIRECRACKER_ARTIFACT_DIR: &str = "firecracker-node";
const FIRECRACKER_ARTIFACT_MANIFEST_FILENAME: &str = "firecracker-artifacts.manifest";
const DEFAULT_FIRECRACKER_KERNEL_FILENAME: &str = "kalisto-vmlinux";
const DEFAULT_FIRECRACKER_ROOTFS_FILENAME: &str = "ubuntu-24.04-minimal-k3s-amd64-rootfs.raw";

#[derive(Debug, Clone)]
pub struct ResolvedFirecrackerArtifacts {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
}

#[derive(Debug, Clone)]
struct FirecrackerRuntimeInfo {
    artifact_track: String,
    arch: String,
}

#[derive(Debug, Clone)]
struct ArtifactManagerConfig {
    managed_cache_root: PathBuf,
    search_roots: Vec<PathBuf>,
    managed_k3s_version: String,
    kernel_filename: String,
    rootfs_filename: String,
    kernel_url: Option<String>,
    rootfs_url: Option<String>,
    kernel_sha256: Option<String>,
    rootfs_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ResolveRequest<'a> {
    default_filename: &'a str,
    remote_url: Option<&'a str>,
    expected_sha256: Option<&'a str>,
    cache_root: &'a Path,
    label: &'a str,
}

#[derive(Debug, Default, Clone)]
struct ManifestData {
    entries: Vec<(String, String)>,
}

fn default_lima_image_arch_suffix() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" | "amd64" => "amd64",
        _ => "arm64",
    }
}

fn default_lima_finalized_image_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}-finalized.qcow2",
        default_lima_image_arch_suffix()
    )
}

fn default_lima_finalized_template_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}-finalized.yaml",
        default_lima_image_arch_suffix()
    )
}

fn default_lima_seed_image_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}.qcow2",
        default_lima_image_arch_suffix()
    )
}

fn default_lima_seed_template_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}.yaml",
        default_lima_image_arch_suffix()
    )
}

impl ManifestData {
    fn parse(content: &str) -> Self {
        let mut entries = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                entries.push((key.trim().to_owned(), value.trim().to_owned()));
            }
        }
        Self { entries }
    }

    fn value(&self, key: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value.clone())
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn pull_cluster_spec(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<ClusterSpec> {
    match spec.backend {
        BackendKind::AppleContainer => Ok(spec.clone()),
        BackendKind::Lima => {
            let mut resolved = spec.clone();
            if spec.template == "template://default" {
                resolved.template = resolve_lima_template(spec, runner, true)?
                    .display()
                    .to_string();
            }
            if let Some(template) = crate::native_image::pack_cached_template(&resolved)? {
                resolved.template = template.display().to_string();
            }
            Ok(resolved)
        }
        BackendKind::Firecracker => resolve_cluster_spec(spec, runner),
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn resolve_cluster_spec(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<ClusterSpec> {
    match spec.backend {
        BackendKind::AppleContainer => Ok(spec.clone()),
        BackendKind::Lima => {
            let mut resolved = spec.clone();
            if spec.template == "template://default" {
                resolved.template = resolve_lima_template(spec, runner, false)?
                    .display()
                    .to_string();
            }
            Ok(resolved)
        }
        BackendKind::Firecracker => {
            let artifacts = prepare_firecracker(spec, runner)?;
            let mut resolved = spec.clone();
            resolved.firecracker_kernel = Some(artifacts.kernel);
            resolved.firecracker_rootfs = Some(artifacts.rootfs);
            Ok(resolved)
        }
    }
}

/// Resolve configured local or managed Firecracker kernel/rootfs artifacts,
/// downloading missing artifacts only when their URLs are explicitly configured.
pub(crate) fn prepare_firecracker(
    spec: &ClusterSpec,
    runner: &dyn CommandRunner,
) -> Result<ResolvedFirecrackerArtifacts> {
    ArtifactManagerConfig::from_env(spec, runner)?.resolve_firecracker(spec, runner)
}

fn resolve_lima_template(
    spec: &ClusterSpec,
    runner: &dyn CommandRunner,
    allow_download: bool,
) -> Result<PathBuf> {
    let cache_root = artifacts_root()?.join(LIMA_ARTIFACT_DIR);
    let search_roots = lima_search_roots(&spec.workspace_root)?;

    for root in &search_roots {
        if let Some(template) = local_lima_template(root) {
            return Ok(template);
        }
    }

    for root in &search_roots {
        if let Some(image) = local_lima_image(root) {
            return render_lima_template(
                root,
                &image,
                spec.image_arch.unwrap_or_else(ImageArch::host_default),
            );
        }
    }

    if let Some(image_url) = std::env::var_os("KALISTO_LIMA_IMAGE_URL") {
        let image_url = image_url.to_string_lossy().into_owned();
        let image_filename = std::env::var("KALISTO_LIMA_IMAGE_FILENAME")
            .unwrap_or_else(|_| default_lima_image_filename(&image_url));
        let image_sha256 = std::env::var("KALISTO_LIMA_IMAGE_SHA256").ok();
        let cached = cache_root.join(url_file_name(&image_url).unwrap_or(&image_filename));
        if !allow_download && !cached.is_file() {
            return Err(Error::InvalidArgs(
                "Lima image is not cached; run `kalisto pull` before up".to_owned(),
            ));
        }
        let image = fetch_into_cache(
            runner,
            &cache_root,
            &image_url,
            image_sha256.as_deref(),
            "lima image",
            &image_filename,
        )?;
        return render_lima_template(
            &cache_root,
            &image,
            spec.image_arch.unwrap_or_else(ImageArch::host_default),
        );
    }

    if allow_download {
        return crate::native_image::prepare(spec, runner);
    }

    Err(Error::InvalidArgs(
        "no managed Lima template found for Kalisto; install Kalisto Lima artifacts under ~/.local/share/kalisto/artifacts/node-image, place them under <project>/artifacts/node-image, set KALISTO_LIMA_IMAGE_URL and run kalisto pull, or pass --template explicitly"
            .to_owned(),
    ))
}

fn lima_search_roots(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![artifacts_root()?.join(LIMA_ARTIFACT_DIR)];
    for root in workspace_root.ancestors() {
        roots.push(root.join("artifacts").join(LIMA_ARTIFACT_DIR));
    }
    if let Ok(config_root) = config_root() {
        roots.push(config_root.join(LIMA_ARTIFACT_DIR));
    }
    Ok(roots)
}

fn local_lima_template(cache_root: &Path) -> Option<PathBuf> {
    let finalized = cache_root.join(default_lima_finalized_template_filename());
    if finalized.is_file() {
        return Some(finalized);
    }

    let seeded = cache_root.join(default_lima_seed_template_filename());
    if seeded.is_file() {
        return Some(seeded);
    }

    None
}

fn local_lima_image(cache_root: &Path) -> Option<PathBuf> {
    let finalized = cache_root.join(default_lima_finalized_image_filename());
    if finalized.is_file() {
        return Some(finalized);
    }

    let seeded = cache_root.join(default_lima_seed_image_filename());
    if seeded.is_file() {
        return Some(seeded);
    }

    None
}

fn render_lima_template(
    cache_root: &Path,
    image_path: &Path,
    image_arch: ImageArch,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_root).map_err(io_error("create_dir_all"))?;
    let file_name = image_path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let template_name = if file_name.contains("-finalized.") {
        default_lima_finalized_template_filename()
    } else {
        default_lima_seed_template_filename()
    };
    let template_path = cache_root.join(template_name);
    let template = format!(
        concat!(
            "vmType: \"vz\"\n",
            "mountType: \"virtiofs\"\n",
            "mounts: []\n",
            "containerd:\n",
            "  system: false\n",
            "  user: false\n",
            "cpus: 2\n",
            "memory: \"4GiB\"\n",
            "disk: \"12GiB\"\n",
            "images:\n",
            "  - location: \"{image}\"\n",
            "    arch: \"{arch}\"\n"
        ),
        image = image_path.display(),
        arch = image_arch.lima_template_value(),
    );
    fs::write(&template_path, template).map_err(io_error("write"))?;
    Ok(template_path)
}

fn default_lima_image_filename(image_url: &str) -> String {
    let file_name = url_file_name(image_url).unwrap_or_default();
    if file_name.contains("-finalized.") {
        default_lima_finalized_image_filename()
    } else {
        default_lima_seed_image_filename()
    }
}

fn load_manifest(
    runner: &dyn CommandRunner,
    cache_root: &Path,
    search_roots: &[PathBuf],
) -> Result<ManifestData> {
    if let Ok(manifest_url) = std::env::var("KALISTO_FIRECRACKER_MANIFEST_URL") {
        let content = read_manifest_url(runner, cache_root, &manifest_url)?;
        return Ok(ManifestData::parse(&content));
    }

    for root in search_roots {
        let manifest_path = root.join(FIRECRACKER_ARTIFACT_MANIFEST_FILENAME);
        if manifest_path.is_file() {
            let content = fs::read_to_string(&manifest_path).map_err(io_error("read_to_string"))?;
            return Ok(ManifestData::parse(&content));
        }
    }

    Ok(ManifestData::default())
}

fn read_manifest_url(
    runner: &dyn CommandRunner,
    cache_root: &Path,
    manifest_url: &str,
) -> Result<String> {
    if let Some(path) = manifest_url.strip_prefix("file://") {
        return fs::read_to_string(path).map_err(io_error("read_to_string"));
    }
    let manifest_path = Path::new(manifest_url);
    if manifest_path.is_file() {
        return fs::read_to_string(manifest_path).map_err(io_error("read_to_string"));
    }

    fs::create_dir_all(cache_root).map_err(io_error("create_dir_all"))?;
    let destination = cache_root.join(FIRECRACKER_ARTIFACT_MANIFEST_FILENAME);
    runner.run(
        "curl",
        &[
            "-fL".into(),
            "--retry".into(),
            "3".into(),
            "--connect-timeout".into(),
            "10".into(),
            "-o".into(),
            destination.display().to_string(),
            manifest_url.to_owned(),
        ],
    )?;
    fs::read_to_string(destination).map_err(io_error("read_to_string"))
}

fn detect_firecracker_runtime_info(runner: &dyn CommandRunner) -> Result<FirecrackerRuntimeInfo> {
    let installed_version = if let Some(version) = std::env::var("KALISTO_FIRECRACKER_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|version| normalize_version_string(&version))
        .transpose()?
    {
        version
    } else {
        detect_installed_firecracker_version(runner)?.ok_or_else(|| {
            Error::InvalidArgs(
                "unable to determine installed Firecracker version; set KALISTO_FIRECRACKER_VERSION or install `firecracker`".to_owned(),
            )
        })?
    };
    let artifact_track = firecracker_artifact_track(&installed_version)?;
    let arch = detect_firecracker_artifact_arch()?;

    Ok(FirecrackerRuntimeInfo {
        artifact_track,
        arch,
    })
}

fn detect_installed_firecracker_version(runner: &dyn CommandRunner) -> Result<Option<String>> {
    match runner.run("firecracker", &["--version".into()]) {
        Ok(output) => {
            let combined = format!("{}\n{}", output.stdout, output.stderr);
            parse_firecracker_version(&combined).map(Some)
        }
        Err(Error::CommandFailed { .. } | Error::CommandIo { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_firecracker_version(output: &str) -> Result<String> {
    for token in output.split_whitespace() {
        let cleaned =
            token.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-'));
        if cleaned.is_empty() {
            continue;
        }

        let candidate = if let Some(stripped) = cleaned.strip_prefix('v') {
            stripped
        } else {
            cleaned
        };
        if candidate.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
            && candidate.split('.').count() >= 3
        {
            return Ok(format!("v{candidate}"));
        }
    }

    Err(Error::InvalidArgs(format!(
        "unable to parse Firecracker version from `{}`",
        output.trim()
    )))
}

fn normalize_version_string(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let prefixed = if trimmed.starts_with('v') {
        trimmed.to_owned()
    } else {
        format!("v{trimmed}")
    };
    firecracker_artifact_track(&prefixed)?;
    Ok(prefixed)
}

fn firecracker_artifact_track(version: &str) -> Result<String> {
    let normalized = version.trim().trim_start_matches('v');
    let mut parts = normalized.split('.');
    let major = parts
        .next()
        .filter(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
        .ok_or_else(|| Error::InvalidArgs(format!("invalid Firecracker version `{version}`")))?;
    let minor = parts
        .next()
        .filter(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
        .ok_or_else(|| Error::InvalidArgs(format!("invalid Firecracker version `{version}`")))?;
    Ok(format!("v{major}.{minor}"))
}

fn detect_firecracker_artifact_arch() -> Result<String> {
    let raw = std::env::var("KALISTO_FIRECRACKER_ARCH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
    match raw.as_str() {
        "x86_64" | "amd64" => Ok("x86_64".to_owned()),
        "aarch64" | "arm64" => Ok("aarch64".to_owned()),
        other => Err(Error::InvalidArgs(format!(
            "unsupported Firecracker artifact architecture `{other}`"
        ))),
    }
}

fn versioned_artifact_root(root: &Path, runtime: &FirecrackerRuntimeInfo) -> PathBuf {
    root.join(&runtime.artifact_track).join(&runtime.arch)
}

fn latest_local_kernel_candidate(root: &Path) -> Result<Option<PathBuf>> {
    if !root.is_dir() {
        return Ok(None);
    }

    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in fs::read_dir(root).map_err(io_error("read_dir"))? {
        let entry = entry.map_err(io_error("read_dir"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let Some(version) = kernel_version_from_filename(file_name) else {
            continue;
        };
        let version_key = numeric_version_key(&version);
        match &best {
            Some((best_key, _)) if &version_key <= best_key => {}
            _ => best = Some((version_key, path)),
        }
    }

    Ok(best.map(|(_, path)| path))
}

fn kernel_version_from_filename(file_name: &str) -> Option<String> {
    if !file_name.starts_with("vmlinux-")
        || file_name.contains('/')
        || file_name.ends_with(".config")
        || file_name.ends_with(".debug.gz")
        || file_name.contains("-no-acpi")
    {
        return None;
    }

    let version = file_name.strip_prefix("vmlinux-")?;
    if version.is_empty() || !version.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        return None;
    }

    Some(version.to_owned())
}

fn numeric_version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

impl ArtifactManagerConfig {
    fn from_env(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<Self> {
        let default_cache_root = artifacts_root()?.join(FIRECRACKER_ARTIFACT_DIR);
        let cache_root = std::env::var_os("KALISTO_FIRECRACKER_ARTIFACT_CACHE_DIR")
            .filter(|value| !value.is_empty())
            .map_or(default_cache_root, PathBuf::from);

        let runtime_info = match detect_firecracker_runtime_info(runner) {
            Ok(runtime_info) => Some(runtime_info),
            Err(error) => {
                warn!(
                    error = %error,
                    "unable to detect installed Firecracker version for managed artifact discovery"
                );
                None
            }
        };

        let configured_artifact_root = std::env::var_os("KALISTO_FIRECRACKER_ARTIFACT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let workspace_artifact_root = spec
            .workspace_root
            .join("artifacts")
            .join(FIRECRACKER_ARTIFACT_DIR);
        let managed_cache_root = runtime_info.as_ref().map_or_else(
            || cache_root.clone(),
            |runtime| versioned_artifact_root(&cache_root, runtime),
        );

        let mut search_roots = Vec::new();
        if let Some(runtime) = runtime_info.as_ref() {
            if let Some(root) = configured_artifact_root.as_ref() {
                search_roots.push(versioned_artifact_root(root, runtime));
            }
            search_roots.push(versioned_artifact_root(&workspace_artifact_root, runtime));
            search_roots.push(managed_cache_root.clone());
        }
        if let Some(root) = configured_artifact_root {
            search_roots.push(root);
        }
        search_roots.push(workspace_artifact_root);
        search_roots.push(cache_root.clone());
        dedupe_paths(&mut search_roots);

        let manifest = load_manifest(runner, &cache_root, &search_roots)?;

        Ok(Self {
            managed_cache_root,
            search_roots,
            managed_k3s_version: std::env::var("KALISTO_FIRECRACKER_MANAGED_K3S_VERSION")
                .ok()
                .or_else(|| manifest.value("K3S_VERSION"))
                .unwrap_or_else(|| DEFAULT_K3S_VERSION.to_owned()),
            kernel_filename: std::env::var("KALISTO_FIRECRACKER_KERNEL_FILENAME")
                .ok()
                .or_else(|| manifest.value("KERNEL_FILENAME"))
                .unwrap_or_else(|| DEFAULT_FIRECRACKER_KERNEL_FILENAME.to_owned()),
            rootfs_filename: std::env::var("KALISTO_FIRECRACKER_ROOTFS_FILENAME")
                .ok()
                .or_else(|| manifest.value("ROOTFS_FILENAME"))
                .unwrap_or_else(|| DEFAULT_FIRECRACKER_ROOTFS_FILENAME.to_owned()),
            kernel_url: std::env::var("KALISTO_FIRECRACKER_KERNEL_URL")
                .ok()
                .or_else(|| manifest.value("KERNEL_URL")),
            rootfs_url: std::env::var("KALISTO_FIRECRACKER_ROOTFS_URL")
                .ok()
                .or_else(|| manifest.value("ROOTFS_URL")),
            kernel_sha256: std::env::var("KALISTO_FIRECRACKER_KERNEL_SHA256")
                .ok()
                .or_else(|| manifest.value("KERNEL_SHA256")),
            rootfs_sha256: std::env::var("KALISTO_FIRECRACKER_ROOTFS_SHA256")
                .ok()
                .or_else(|| manifest.value("ROOTFS_SHA256")),
        })
    }

    fn resolve_firecracker(
        &self,
        spec: &ClusterSpec,
        runner: &dyn CommandRunner,
    ) -> Result<ResolvedFirecrackerArtifacts> {
        if spec.firecracker_rootfs.is_none() && spec.k3s_version != self.managed_k3s_version {
            return Err(Error::InvalidArgs(format!(
                "auto-managed Firecracker rootfs only supports k3s version `{}`; pass --firecracker-rootfs for `{}`",
                self.managed_k3s_version, spec.k3s_version
            )));
        }

        let kernel = self.resolve_kernel(spec.firecracker_kernel.as_deref(), runner)?;
        let rootfs = self.resolve_one(
            spec.firecracker_rootfs.as_deref(),
            ResolveRequest {
                default_filename: &self.rootfs_filename,
                remote_url: self.rootfs_url.as_deref(),
                expected_sha256: self.rootfs_sha256.as_deref(),
                cache_root: &self.managed_cache_root,
                label: "rootfs",
            },
            runner,
        )?;

        Ok(ResolvedFirecrackerArtifacts { kernel, rootfs })
    }

    fn resolve_kernel(
        &self,
        explicit: Option<&Path>,
        runner: &dyn CommandRunner,
    ) -> Result<PathBuf> {
        if let Some(path) = explicit {
            ensure_file(path, "kernel")?;
            if let Some(checksum) = self.kernel_sha256.as_deref() {
                verify_sha256(runner, path, checksum, "kernel")?;
            }
            return Ok(path.to_path_buf());
        }

        if let Some(local) = self.find_local_kernel_candidate(runner)? {
            return Ok(local);
        }

        self.resolve_one(
            None,
            ResolveRequest {
                default_filename: &self.kernel_filename,
                remote_url: self.kernel_url.as_deref(),
                expected_sha256: self.kernel_sha256.as_deref(),
                cache_root: &self.managed_cache_root,
                label: "kernel",
            },
            runner,
        )
    }

    fn resolve_one(
        &self,
        explicit: Option<&Path>,
        request: ResolveRequest<'_>,
        runner: &dyn CommandRunner,
    ) -> Result<PathBuf> {
        if let Some(path) = explicit {
            ensure_file(path, request.label)?;
            return finalize_resolved_artifact(runner, path, request);
        }

        let mut candidate_filenames = vec![request.default_filename.to_owned()];
        if let Some(filename) = request.remote_url.and_then(url_file_name)
            && candidate_filenames
                .iter()
                .all(|candidate| candidate != filename)
        {
            candidate_filenames.push(filename.to_owned());
        }
        for root in &self.search_roots {
            for filename in &candidate_filenames {
                let candidate = root.join(filename);
                if candidate.is_file() {
                    return finalize_resolved_artifact(runner, &candidate, request);
                }
            }
        }

        let Some(url) = request.remote_url else {
            return Err(Error::InvalidArgs(missing_artifact_message(
                request.label,
                request.default_filename,
                &self.search_roots,
            )));
        };

        let fetched = fetch_into_cache(
            runner,
            request.cache_root,
            url,
            request.expected_sha256,
            request.label,
            request.default_filename,
        )?;
        finalize_resolved_artifact(runner, &fetched, request)
    }

    fn find_local_kernel_candidate(&self, runner: &dyn CommandRunner) -> Result<Option<PathBuf>> {
        for root in &self.search_roots {
            let configured = root.join(&self.kernel_filename);
            if configured.is_file() {
                if let Some(checksum) = self.kernel_sha256.as_deref() {
                    verify_sha256(runner, &configured, checksum, "kernel")?;
                }
                return Ok(Some(configured));
            }

            if let Some(candidate) = latest_local_kernel_candidate(root)? {
                if let Some(checksum) = self.kernel_sha256.as_deref() {
                    verify_sha256(runner, &candidate, checksum, "kernel")?;
                }
                return Ok(Some(candidate));
            }
        }

        Ok(None)
    }
}

pub(crate) fn fetch_into_cache(
    runner: &dyn CommandRunner,
    cache_root: &Path,
    url: &str,
    expected_sha256: Option<&str>,
    label: &str,
    default_filename: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_root).map_err(io_error("create_dir_all"))?;

    let filename = url_file_name(url).unwrap_or(default_filename);
    let destination = cache_root.join(filename);
    if destination.is_file() {
        if let Some(checksum) = expected_sha256 {
            verify_sha256(runner, &destination, checksum, label)?;
        }
        return Ok(destination);
    }

    let temporary = destination.with_extension("download");
    let _ = fs::remove_file(&temporary);
    info!(artifact = label, url, destination = %destination.display(), "fetching managed artifact");
    runner.run(
        "curl",
        &[
            "-fL".into(),
            "--retry".into(),
            "3".into(),
            "--connect-timeout".into(),
            "10".into(),
            "-o".into(),
            temporary.display().to_string(),
            url.to_owned(),
        ],
    )?;

    if let Some(checksum) = expected_sha256
        && let Err(error) = verify_sha256(runner, &temporary, checksum, label)
    {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &destination).map_err(io_error("rename"))?;
    Ok(destination)
}

fn finalize_resolved_artifact(
    runner: &dyn CommandRunner,
    candidate: &Path,
    request: ResolveRequest<'_>,
) -> Result<PathBuf> {
    let materialized = maybe_materialize_rootfs(runner, candidate, request)?;
    if let Some(checksum) = request.expected_sha256 {
        verify_sha256(runner, &materialized, checksum, request.label)?;
    }
    Ok(materialized)
}

fn maybe_materialize_rootfs(
    runner: &dyn CommandRunner,
    candidate: &Path,
    request: ResolveRequest<'_>,
) -> Result<PathBuf> {
    if request.label != "rootfs" {
        return Ok(candidate.to_path_buf());
    }
    let Some(compression) = compressed_artifact_extension(candidate) else {
        return Ok(candidate.to_path_buf());
    };

    fs::create_dir_all(request.cache_root).map_err(io_error("create_dir_all"))?;
    let source = if candidate.parent() == Some(request.cache_root) {
        candidate.to_path_buf()
    } else {
        let file_name = candidate
            .file_name()
            .ok_or_else(|| {
                Error::InvalidArgs(format!(
                    "unable to materialize rootfs from {}: missing file name",
                    candidate.display()
                ))
            })?
            .to_os_string();
        let copied = request.cache_root.join(file_name);
        if !copied.is_file() {
            fs::copy(candidate, &copied).map_err(io_error("copy"))?;
        }
        copied
    };

    let output_file_name =
        decompressed_file_name(&source).unwrap_or_else(|| request.default_filename.to_owned());
    let output = request.cache_root.join(output_file_name);
    let temporary = output.with_extension("inflate");
    let _ = fs::remove_file(&temporary);

    let decompressor = match compression {
        "xz" => "xz -dc",
        "gz" => "gzip -dc",
        "zst" => "zstd -dc",
        _ => {
            return Err(Error::InvalidArgs(format!(
                "unsupported rootfs compression for {}",
                source.display()
            )));
        }
    };
    let command = format!(
        "set -eu; {decompressor} {source} > {temporary}",
        decompressor = decompressor,
        source = shell_escape(&source.display().to_string()),
        temporary = shell_escape(&temporary.display().to_string()),
    );
    if let Err(error) = runner.run("sh", &["-lc".to_owned(), command]) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &output).map_err(io_error("rename"))?;
    Ok(output)
}

pub(crate) fn verify_sha256(
    runner: &dyn CommandRunner,
    path: &Path,
    expected: &str,
    label: &str,
) -> Result<()> {
    let actual = file_sha256(runner, path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::InvalidArgs(format!(
            "firecracker {label} checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        )))
    }
}

pub(crate) fn file_sha256(runner: &dyn CommandRunner, path: &Path) -> Result<String> {
    if let Ok(output) = runner.run("sha256sum", &[path.display().to_string()]) {
        return parse_checksum_output(&output.stdout, path);
    }
    if let Ok(output) = runner.run(
        "shasum",
        &["-a".into(), "256".into(), path.display().to_string()],
    ) {
        return parse_checksum_output(&output.stdout, path);
    }
    Err(Error::InvalidArgs(format!(
        "unable to verify artifact checksum for {}: neither `sha256sum` nor `shasum` succeeded",
        path.display()
    )))
}

fn parse_checksum_output(output: &str, path: &Path) -> Result<String> {
    output
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "checksum command returned no digest for {}",
                path.display()
            ))
        })
}

fn ensure_file(path: &Path, label: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(Error::InvalidArgs(format!(
            "firecracker {label} path does not exist or is not a file: {}",
            path.display()
        )))
    }
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut unique = Vec::with_capacity(paths.len());
    for path in paths.drain(..) {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
    }
    *paths = unique;
}

fn url_file_name(url: &str) -> Option<&str> {
    let path = url.split('?').next()?;
    Path::new(path)
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|value| !value.is_empty())
}

fn compressed_artifact_extension(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(OsStr::to_str) {
        Some("xz") => Some("xz"),
        Some("gz") => Some("gz"),
        Some("zst") => Some("zst"),
        _ => None,
    }
}

fn decompressed_file_name(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    for suffix in [".xz", ".gz", ".zst"] {
        if let Some(stripped) = file_name.strip_suffix(suffix) {
            return Some(stripped.to_owned());
        }
    }
    None
}

fn missing_artifact_message(
    label: &str,
    default_filename: &str,
    search_roots: &[PathBuf],
) -> String {
    let locations = search_roots
        .iter()
        .map(|root| root.join(default_filename).display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "firecracker {label} artifact was not configured and no managed artifact was found. Checked: {locations}. Set --firecracker-{label}, populate the local artifact cache, configure KALISTO_FIRECRACKER_MANIFEST_URL, or configure KALISTO_FIRECRACKER_{}_URL",
        label.to_ascii_uppercase()
    )
}

fn io_error(program: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::CommandIo {
        program: program.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::backend::BackendKind;
    use crate::cluster::spec::{ClusterSpec, DEFAULT_K3S_VERSION, ImageArch};
    use crate::error::{Error, Result};
    use crate::process::{CommandRunner, Output};
    use crate::test_support::env_lock;

    use super::{
        ArtifactManagerConfig, DEFAULT_FIRECRACKER_KERNEL_FILENAME,
        DEFAULT_FIRECRACKER_ROOTFS_FILENAME, FIRECRACKER_ARTIFACT_MANIFEST_FILENAME,
        LIMA_ARTIFACT_DIR, compressed_artifact_extension, decompressed_file_name,
        default_lima_finalized_image_filename, default_lima_finalized_template_filename,
        default_lima_seed_template_filename, io_error, render_lima_template, resolve_cluster_spec,
    };

    #[derive(Default)]
    struct FakeRunner {
        checksums: HashMap<PathBuf, String>,
        curl_stdout: HashMap<String, String>,
        downloads: Mutex<Vec<(String, PathBuf)>>,
        commands: Mutex<Vec<String>>,
        firecracker_version: Option<String>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            self.commands.lock().unwrap().push(program.to_owned());
            match program {
                "curl" => {
                    let source = args
                        .last()
                        .cloned()
                        .ok_or_else(|| Error::InvalidArgs("missing curl source".to_owned()))?;
                    if let Some(destination) = args
                        .windows(2)
                        .find(|window| window[0] == "-o")
                        .map(|window| PathBuf::from(&window[1]))
                    {
                        let source_path = PathBuf::from(source.trim_start_matches("file://"));
                        fs::copy(&source_path, &destination).map_err(io_error("copy"))?;
                        self.downloads
                            .lock()
                            .unwrap()
                            .push((source, destination.clone()));
                        Ok(Output {
                            stdout: String::new(),
                            stderr: String::new(),
                        })
                    } else {
                        let stdout = self.curl_stdout.get(&source).cloned().ok_or_else(|| {
                            Error::InvalidArgs(format!("missing fake curl stdout for {source}"))
                        })?;
                        Ok(Output {
                            stdout,
                            stderr: String::new(),
                        })
                    }
                }
                "firecracker" => Ok(Output {
                    stdout: format!(
                        "Firecracker {}\n",
                        self.firecracker_version
                            .clone()
                            .unwrap_or_else(|| "v1.13.1".to_owned())
                    ),
                    stderr: String::new(),
                }),
                "sha256sum" | "shasum" => {
                    let path = PathBuf::from(args.last().cloned().unwrap_or_default());
                    let checksum = self.checksums.get(&path).cloned().ok_or_else(|| {
                        Error::InvalidArgs(format!("missing fake checksum for {}", path.display()))
                    })?;
                    Ok(Output {
                        stdout: format!("{checksum}  {}\n", path.display()),
                        stderr: String::new(),
                    })
                }
                other => Err(Error::InvalidArgs(format!("unexpected command `{other}`"))),
            }
        }
    }

    #[test]
    fn resolve_cluster_spec_prefers_workspace_artifacts() {
        let root = temp_dir("workspace");
        let artifact_root = root.join("artifacts").join("firecracker-node");
        fs::create_dir_all(&artifact_root).unwrap();
        let kernel = artifact_root.join(DEFAULT_FIRECRACKER_KERNEL_FILENAME);
        let rootfs = artifact_root.join(DEFAULT_FIRECRACKER_ROOTFS_FILENAME);
        fs::write(&kernel, "kernel").unwrap();
        fs::write(&rootfs, "rootfs").unwrap();

        let spec = firecracker_spec(&root);
        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();

        assert_eq!(
            resolved.firecracker_kernel.as_deref(),
            Some(kernel.as_path())
        );
        assert_eq!(
            resolved.firecracker_rootfs.as_deref(),
            Some(rootfs.as_path())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_prefers_versioned_cache_artifacts() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("versioned-cache");
        let data_home = root.join("xdg-data");
        let arch = super::detect_firecracker_artifact_arch().unwrap();
        let artifact_root = root
            .join("artifacts")
            .join("firecracker-node")
            .join("v1.13")
            .join(arch);
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }
        fs::create_dir_all(&artifact_root).unwrap();
        let kernel = artifact_root.join("vmlinux-6.1.141");
        let rootfs = artifact_root.join(DEFAULT_FIRECRACKER_ROOTFS_FILENAME);
        fs::write(&kernel, "kernel").unwrap();
        fs::write(&rootfs, "rootfs").unwrap();

        let spec = firecracker_spec(&root);
        let runner = FakeRunner {
            firecracker_version: Some("v1.13.1".to_owned()),
            ..FakeRunner::default()
        };
        let resolved = resolve_cluster_spec(&spec, &runner).unwrap();

        assert_eq!(
            resolved.firecracker_kernel.as_deref(),
            Some(kernel.as_path())
        );
        assert_eq!(
            resolved.firecracker_rootfs.as_deref(),
            Some(rootfs.as_path())
        );

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_firecracker_fetches_only_manifest_kernel_and_rootfs() {
        let _guard = env_lock();
        let names = [
            "XDG_DATA_HOME",
            "KALISTO_FIRECRACKER_ARTIFACT_CACHE_DIR",
            "KALISTO_FIRECRACKER_MANIFEST_URL",
            "KALISTO_FIRECRACKER_VERSION",
        ];
        let original_env = names.map(|name| (name, std::env::var_os(name)));
        let root = temp_dir("fetch");
        let remote_root = root.join("remote");
        let cache_root = root.join("cache");
        let data_home = root.join("xdg-data");
        let lima_root = data_home
            .join("kalisto")
            .join("artifacts")
            .join(LIMA_ARTIFACT_DIR);
        fs::create_dir_all(lima_root.parent().unwrap()).unwrap();
        fs::write(&lima_root, "not a Lima image directory").unwrap();
        fs::create_dir_all(&remote_root).unwrap();
        let remote_kernel = remote_root.join("kernel.bin");
        let remote_rootfs = remote_root.join("rootfs.raw");
        fs::write(&remote_kernel, "kernel").unwrap();
        fs::write(&remote_rootfs, "rootfs").unwrap();
        let manifest = root.join(FIRECRACKER_ARTIFACT_MANIFEST_FILENAME);
        fs::write(
            &manifest,
            format!(
                "KERNEL_URL=file://{}\nROOTFS_URL=file://{}\n",
                remote_kernel.display(),
                remote_rootfs.display()
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
            std::env::set_var("KALISTO_FIRECRACKER_ARTIFACT_CACHE_DIR", &cache_root);
            std::env::set_var("KALISTO_FIRECRACKER_MANIFEST_URL", &manifest);
            std::env::set_var("KALISTO_FIRECRACKER_VERSION", "v1.13.1");
        }
        let spec = firecracker_spec(&root.join("workspace"));
        let runner = FakeRunner::default();

        let resolved = super::prepare_firecracker(&spec, &runner).unwrap();
        let versioned_cache = cache_root
            .join("v1.13")
            .join(super::detect_firecracker_artifact_arch().unwrap());

        assert_eq!(resolved.kernel, versioned_cache.join("kernel.bin"));
        assert_eq!(resolved.rootfs, versioned_cache.join("rootfs.raw"));
        assert_eq!(fs::read(&resolved.kernel).unwrap(), b"kernel");
        assert_eq!(fs::read(&resolved.rootfs).unwrap(), b"rootfs");
        assert_eq!(runner.downloads.lock().unwrap().len(), 2);
        assert_eq!(*runner.commands.lock().unwrap(), ["curl", "curl"]);
        assert!(lima_root.is_file());

        for (name, value) in original_env {
            restore_env(name, value);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_requires_explicit_rootfs_for_custom_k3s_version() {
        let root = temp_dir("version");
        let config = ArtifactManagerConfig {
            managed_cache_root: root.join("cache"),
            search_roots: vec![root.join("artifacts")],
            managed_k3s_version: DEFAULT_K3S_VERSION.to_owned(),
            kernel_filename: DEFAULT_FIRECRACKER_KERNEL_FILENAME.to_owned(),
            rootfs_filename: DEFAULT_FIRECRACKER_ROOTFS_FILENAME.to_owned(),
            kernel_url: None,
            rootfs_url: None,
            kernel_sha256: None,
            rootfs_sha256: None,
        };
        let mut spec = firecracker_spec(&root);
        spec.k3s_version = "v0.0.0-test".to_owned();

        let error = config
            .resolve_firecracker(&spec, &FakeRunner::default())
            .unwrap_err();
        match error {
            Error::InvalidArgs(message) => assert!(message.contains("only supports k3s version")),
            other => panic!("unexpected error: {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_verifies_checksums_when_configured() {
        let root = temp_dir("checksum");
        let artifact_root = root.join("artifacts");
        fs::create_dir_all(&artifact_root).unwrap();
        let kernel = artifact_root.join(DEFAULT_FIRECRACKER_KERNEL_FILENAME);
        let rootfs = artifact_root.join(DEFAULT_FIRECRACKER_ROOTFS_FILENAME);
        fs::write(&kernel, "kernel").unwrap();
        fs::write(&rootfs, "rootfs").unwrap();

        let mut checksums = HashMap::new();
        checksums.insert(kernel.clone(), "abc123".to_owned());
        checksums.insert(rootfs.clone(), "def456".to_owned());
        let runner = FakeRunner {
            checksums,
            ..FakeRunner::default()
        };
        let config = ArtifactManagerConfig {
            managed_cache_root: root.join("cache"),
            search_roots: vec![artifact_root],
            managed_k3s_version: DEFAULT_K3S_VERSION.to_owned(),
            kernel_filename: DEFAULT_FIRECRACKER_KERNEL_FILENAME.to_owned(),
            rootfs_filename: DEFAULT_FIRECRACKER_ROOTFS_FILENAME.to_owned(),
            kernel_url: None,
            rootfs_url: None,
            kernel_sha256: Some("abc123".to_owned()),
            rootfs_sha256: Some("def456".to_owned()),
        };

        let resolved = config
            .resolve_firecracker(&firecracker_spec(&root), &runner)
            .unwrap();
        assert_eq!(resolved.kernel, kernel);
        assert_eq!(resolved.rootfs, rootfs);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_uses_local_manifest_filenames() {
        let root = temp_dir("manifest");
        let artifact_root = root.join("artifacts").join("firecracker-node");
        fs::create_dir_all(&artifact_root).unwrap();
        let kernel = artifact_root.join("custom-kernel");
        let rootfs = artifact_root.join("custom-rootfs.raw");
        fs::write(&kernel, "kernel").unwrap();
        fs::write(&rootfs, "rootfs").unwrap();
        fs::write(
            artifact_root.join(FIRECRACKER_ARTIFACT_MANIFEST_FILENAME),
            "KERNEL_FILENAME=custom-kernel\nROOTFS_FILENAME=custom-rootfs.raw\n",
        )
        .unwrap();

        let spec = firecracker_spec(&root);
        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();
        assert_eq!(
            resolved.firecracker_kernel.as_deref(),
            Some(kernel.as_path())
        );
        assert_eq!(
            resolved.firecracker_rootfs.as_deref(),
            Some(rootfs.as_path())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_prefers_finalized_lima_template() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("lima-finalized-template");
        let image_root = root.join("artifacts").join(LIMA_ARTIFACT_DIR);
        let finalized = image_root.join(default_lima_finalized_template_filename());
        let seeded = image_root.join(default_lima_seed_template_filename());
        fs::create_dir_all(&image_root).unwrap();
        fs::write(&finalized, "finalized\n").unwrap();
        fs::write(&seeded, "seeded\n").unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root.join("xdg-data"));
        }
        let mut spec = ClusterSpec::new("default".to_owned(), root.clone());
        spec.backend = BackendKind::Lima;

        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();

        assert_eq!(resolved.template, finalized.display().to_string());

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_prefers_xdg_lima_template_over_workspace() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("lima-xdg-template");
        let data_home = root.join("xdg-data");
        let workspace_root = root.join("workspace");
        let workspace_template = workspace_root
            .join("artifacts")
            .join(LIMA_ARTIFACT_DIR)
            .join(default_lima_finalized_template_filename());
        let xdg_template = data_home
            .join("kalisto")
            .join("artifacts")
            .join(LIMA_ARTIFACT_DIR)
            .join(default_lima_finalized_template_filename());
        fs::create_dir_all(workspace_template.parent().unwrap()).unwrap();
        fs::create_dir_all(xdg_template.parent().unwrap()).unwrap();
        fs::write(&workspace_template, "workspace\n").unwrap();
        fs::write(&xdg_template, "xdg\n").unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }
        let mut spec = ClusterSpec::new("default".to_owned(), workspace_root);
        spec.backend = BackendKind::Lima;

        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();

        assert_eq!(resolved.template, xdg_template.display().to_string());

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_finds_lima_template_in_workspace_ancestors() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("lima-ancestor-template");
        let workspace_root = root.join("examples").join("harness").join("job-smoke");
        let ancestor_template = root
            .join("artifacts")
            .join(LIMA_ARTIFACT_DIR)
            .join(default_lima_finalized_template_filename());
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(ancestor_template.parent().unwrap()).unwrap();
        fs::write(&ancestor_template, "ancestor\n").unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root.join("xdg-data"));
        }
        let mut spec = ClusterSpec::new("default".to_owned(), workspace_root);
        spec.backend = BackendKind::Lima;

        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();

        assert_eq!(resolved.template, ancestor_template.display().to_string());

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apple_container_resolvers_ignore_lima_disks_and_firecracker() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("apple-ignores-lima-template");
        let data_home = root.join("xdg-data");
        let image_root = data_home
            .join("kalisto")
            .join("artifacts")
            .join(LIMA_ARTIFACT_DIR);
        let stem = format!(
            "ubuntu-24.04-minimal-k3s-{}-finalized",
            ImageArch::host_default().lima_artifact_suffix()
        );
        let cached_template = image_root.join(format!("{stem}.yaml"));
        let raw = image_root.join(format!("{stem}.raw"));
        let packed = image_root.join(format!("{stem}.raw.gz"));
        fs::create_dir_all(&image_root).unwrap();
        fs::write(&cached_template, "invalid: [yaml").unwrap();
        fs::write(&raw, "Lima disk").unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }
        let mut spec = ClusterSpec::new("default".to_owned(), root.join("workspace"));
        spec.backend = BackendKind::AppleContainer;
        let runner = FakeRunner::default();

        for template in [
            "template://default".to_owned(),
            cached_template.display().to_string(),
        ] {
            spec.template = template.clone();
            let resolved = resolve_cluster_spec(&spec, &runner).unwrap();
            let pulled = super::pull_cluster_spec(&spec, &runner).unwrap();
            assert_eq!(resolved.template, template);
            assert_eq!(pulled.template, template);
            assert!(resolved.firecracker_kernel.is_none());
            assert!(pulled.firecracker_rootfs.is_none());
        }
        assert!(runner.commands.lock().unwrap().is_empty());
        assert!(!packed.exists());
        assert_eq!(fs::read_to_string(&cached_template).unwrap(), "invalid: [yaml");
        assert_eq!(fs::read_to_string(&raw).unwrap(), "Lima disk");

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_rejects_generic_lima_default_template() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let original_lima_image_url = std::env::var_os("KALISTO_LIMA_IMAGE_URL");
        let original_lima_image_filename = std::env::var_os("KALISTO_LIMA_IMAGE_FILENAME");
        let root = temp_dir("lima-default-template");
        let data_home = root.join("xdg-data");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
            std::env::remove_var("KALISTO_LIMA_IMAGE_URL");
            std::env::remove_var("KALISTO_LIMA_IMAGE_FILENAME");
        }
        let mut spec = ClusterSpec::new("default".to_owned(), root.clone());
        spec.backend = BackendKind::Lima;
        spec.template = "template://default".to_owned();

        let error = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap_err();
        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("no managed Lima template found"));
                assert!(message.contains("~/.local/share/kalisto/artifacts/node-image"));
                assert!(message.contains("KALISTO_LIMA_IMAGE_URL"));
                assert!(message.contains("--template"));
            }
            other => panic!("unexpected error: {other}"),
        }

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        restore_env("KALISTO_LIMA_IMAGE_URL", original_lima_image_url);
        restore_env("KALISTO_LIMA_IMAGE_FILENAME", original_lima_image_filename);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_fetches_lima_image_and_renders_template() {
        let _guard = env_lock();
        let root = temp_dir("lima-fetch");
        let data_home = root.join("xdg-data");
        let image_source = root.join(default_lima_finalized_image_filename());
        fs::write(&image_source, "lima-image").unwrap();

        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let original_lima_image_url = std::env::var_os("KALISTO_LIMA_IMAGE_URL");
        let original_lima_image_filename = std::env::var_os("KALISTO_LIMA_IMAGE_FILENAME");

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
            std::env::set_var(
                "KALISTO_LIMA_IMAGE_URL",
                format!("file://{}", image_source.display()),
            );
            std::env::remove_var("KALISTO_LIMA_IMAGE_FILENAME");
        }

        let mut spec = ClusterSpec::new("default".to_owned(), root.join("workspace"));
        spec.backend = BackendKind::Lima;
        spec.template = "template://default".to_owned();
        spec.image_arch = Some(ImageArch::Aarch64);

        let runner = FakeRunner::default();
        let resolved = super::pull_cluster_spec(&spec, &runner).unwrap();
        let template_path = PathBuf::from(&resolved.template);
        let cached_image = data_home
            .join("kalisto")
            .join("artifacts")
            .join("node-image")
            .join(default_lima_finalized_image_filename());

        assert_eq!(
            template_path,
            data_home
                .join("kalisto")
                .join("artifacts")
                .join("node-image")
                .join(default_lima_finalized_template_filename())
        );
        assert!(cached_image.is_file());
        assert!(template_path.is_file());
        let template = fs::read_to_string(&template_path).unwrap();
        assert!(template.contains(&cached_image.display().to_string()));
        assert_eq!(runner.downloads.lock().unwrap().len(), 1);

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        restore_env("KALISTO_LIMA_IMAGE_URL", original_lima_image_url);
        restore_env("KALISTO_LIMA_IMAGE_FILENAME", original_lima_image_filename);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cluster_spec_renders_workspace_lima_image_when_template_is_default() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = temp_dir("lima-workspace-image");
        let data_home = root.join("xdg-data");
        let workspace_root = root.join("workspace");
        let image_root = workspace_root.join("artifacts").join(LIMA_ARTIFACT_DIR);
        let image_path = image_root.join(default_lima_finalized_image_filename());
        fs::create_dir_all(&image_root).unwrap();
        fs::write(&image_path, "lima-image").unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        let mut spec = ClusterSpec::new("default".to_owned(), workspace_root.clone());
        spec.backend = BackendKind::Lima;
        spec.template = "template://default".to_owned();
        spec.image_arch = Some(ImageArch::X86_64);

        let resolved = resolve_cluster_spec(&spec, &FakeRunner::default()).unwrap();
        let template_path = PathBuf::from(&resolved.template);
        let expected_template = image_root.join(default_lima_finalized_template_filename());
        assert_eq!(template_path, expected_template);
        assert!(template_path.is_file());
        let template = fs::read_to_string(&template_path).unwrap();
        assert!(template.contains(&image_path.display().to_string()));
        assert!(template.contains("arch: \"x86_64\""));

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn render_lima_template_uses_explicit_arch() {
        let root = temp_dir("lima-template-amd64");
        let artifact_root = root.join(LIMA_ARTIFACT_DIR);
        let image_path = artifact_root.join("ubuntu-24.04-minimal-k3s-amd64-finalized.qcow2");
        fs::create_dir_all(&artifact_root).unwrap();
        fs::write(&image_path, "lima-image").unwrap();

        let template_path =
            render_lima_template(&artifact_root, &image_path, ImageArch::X86_64).unwrap();
        let template = fs::read_to_string(template_path).unwrap();
        assert!(template.contains("arch: \"x86_64\""));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compressed_rootfs_helpers_detect_suffixes() {
        assert_eq!(
            compressed_artifact_extension(Path::new("rootfs.raw.xz")),
            Some("xz")
        );
        assert_eq!(
            compressed_artifact_extension(Path::new("rootfs.raw.gz")),
            Some("gz")
        );
        assert_eq!(
            compressed_artifact_extension(Path::new("rootfs.raw.zst")),
            Some("zst")
        );
        assert_eq!(compressed_artifact_extension(Path::new("rootfs.raw")), None);

        assert_eq!(
            decompressed_file_name(Path::new("rootfs.raw.xz")).as_deref(),
            Some("rootfs.raw")
        );
        assert_eq!(
            decompressed_file_name(Path::new("rootfs.raw.gz")).as_deref(),
            Some("rootfs.raw")
        );
        assert_eq!(
            decompressed_file_name(Path::new("rootfs.raw.zst")).as_deref(),
            Some("rootfs.raw")
        );
        assert_eq!(decompressed_file_name(Path::new("rootfs.raw")), None);
    }

    fn firecracker_spec(workspace_root: &Path) -> ClusterSpec {
        let mut spec = ClusterSpec::new("kalisto".to_owned(), workspace_root.to_path_buf());
        spec.backend = BackendKind::Firecracker;
        spec
    }

    fn temp_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kalisto-artifact-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe {
                std::env::set_var(name, value);
            },
            None => unsafe {
                std::env::remove_var(name);
            },
        }
    }
}

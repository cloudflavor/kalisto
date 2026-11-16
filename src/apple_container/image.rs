//! Prepare a reusable Linux node rootfs during pull, never Kubernetes identities.
//!
//! Export ordinary containers to an external OCI archive. Pull that archive over
//! a temporary loopback registry so an existing Apple image helper never extracts
//! a second copy into its own temporary directory.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::apple_container::runtime;
use crate::cluster::spec::{ClusterSpec, ImageArch};
use crate::error::{Error, Result};
use crate::process::CommandRunner;
use crate::shell::shell_escape;

// Official Ubuntu 24.04 arm64/v8 image, verified against docker-library/repo-info:
// https://github.com/docker-library/repo-info/blob/master/repos/ubuntu/remote/24.04.md
const BASE_IMAGE: &str = "docker.io/library/ubuntu@sha256:c92376659a7f14cc1f51615d73c9f4b7d8e18d59f370e02b4ed683ab4485c66c";
const IMAGE_REPOSITORY: &str = "docker.io/kalisto/apple-k3s-node";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

const NODE_INIT: &str = r#"#!/bin/sh
set -eu
rm -f /run/kalisto-node-ready
# Nested containerd mounts must propagate to the kubelet, and Cilium needs bpffs.
mount --make-rshared /
mkdir -p /sys/fs/cgroup /sys/fs/bpf
if ! mountpoint -q /sys/fs/cgroup; then
    mount -t cgroup2 -o nosuid,nodev,noexec cgroup2 /sys/fs/cgroup
else
    mount -o remount,rw /sys/fs/cgroup
fi
test -f /sys/fs/cgroup/cgroup.controllers
# Empty the delegated parent before enabling controllers. Apple vmexec puts
# later execs into PID1's current cgroup (ManagedProcess.start in 0.54.0).
mkdir -p /sys/fs/cgroup/kalisto-init
printf '1\n' > /sys/fs/cgroup/kalisto-init/cgroup.procs
printf '%s\n' "$$" > /sys/fs/cgroup/kalisto-init/cgroup.procs
# A readiness exec started before PID1 moved may still be exiting. Do not move
# arbitrary PIDs from a changing proc list: an exited PID makes that racy.
remaining=100
while [ -n "$(cat /sys/fs/cgroup/cgroup.procs)" ]; do
    if [ "$remaining" -eq 0 ]; then
        echo 'Kalisto cgroup parent still contains processes' >&2
        exit 1
    fi
    remaining=$((remaining - 1))
    sleep 0.01
done
for controller in cpu cpuset io memory pids; do
    if ! grep -qw "$controller" /sys/fs/cgroup/cgroup.controllers; then
        echo "Kalisto kernel lacks cgroup controller: $controller" >&2
        exit 1
    fi
    printf '+%s\n' "$controller" > /sys/fs/cgroup/cgroup.subtree_control
done
if ! mountpoint -q /sys/fs/bpf; then
    mount -t bpf -o nosuid,nodev,noexec bpf /sys/fs/bpf
fi
# The managed kernel provides these built in: no host module tree is mounted.
sysctl -w net.ipv4.ip_forward=1
sysctl -w net.bridge.bridge-nf-call-iptables=1
sysctl -w net.bridge.bridge-nf-call-ip6tables=1
iptables -t nat -L -n >/dev/null
if [ ! -s /etc/machine-id ]; then
    tr -d '-' < /proc/sys/kernel/random/uuid > /etc/machine-id
fi
mkdir -p /run/kalisto
cat /proc/sys/kernel/random/boot_id > /run/kalisto-node-ready
exec sleep infinity
"#;

/// Build, import and unpack the exact k3s node image using the selected runtime.
#[allow(clippy::too_many_lines)] // Keep the owned preparation container and cleanup in one scope.
pub(crate) fn prepare(
    spec: &ClusterSpec,
    assets_root: &Path,
    runner: &dyn CommandRunner,
) -> Result<String> {
    if spec.image_arch.unwrap_or_else(ImageArch::host_default) != ImageArch::Aarch64 {
        return Err(invalid("Apple container node images require arm64"));
    }
    let assets_root = fs::canonicalize(assets_root).map_err(io_error)?;
    let assets = assets_root
        .to_str()
        .filter(|path| !path.contains(':'))
        .ok_or_else(|| invalid("Apple container assets path must be UTF-8 without ':'"))?;
    for required in ["k3s", "images.tar"] {
        if !assets_root.join(required).is_file() {
            return Err(Error::MissingPath(assets_root.join(required)));
        }
    }
    let archive = assets_root.join("apple-node-image.tar");
    let reference_file = assets_root.join("apple-node-image");
    if reference_file.exists() || archive.exists() {
        let reference = fs::read_to_string(&reference_file).map_err(io_error)?;
        let reference = reference.trim();
        let hash = reference
            .strip_prefix(&format!("{IMAGE_REPOSITORY}@sha256:"))
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| invalid("invalid cached Apple node image reference"))?;
        if hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(invalid("cached Apple node image digest must be lowercase"));
        }
        if !archive.is_file() {
            return Err(Error::MissingPath(archive));
        }
        super::registry::import_image(runner, &archive, reference)?;
        return Ok(reference.to_owned());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    let name = format!("kalisto-image-prepare-{}-{nonce}", std::process::id());
    let scratch = assets_root.join(format!(".{name}"));
    fs::create_dir(&scratch).map_err(io_error)?;
    let mut attempted_create = false;
    let result = (|| {
        super::registry::prepare_remote_image(runner, BASE_IMAGE)?;
        let args = vec![
            "run".into(),
            "--name".into(),
            name.clone(),
            "--platform".into(),
            "linux/arm64".into(),
            "--uid".into(),
            "0".into(),
            "--gid".into(),
            "0".into(),
            "--kernel".into(),
            runtime::kernel_path()?.display().to_string(),
            "--init-image".into(),
            runtime::init_image()?,
            "--volume".into(),
            format!("{assets}:{assets}:ro"),
            "--entrypoint".into(),
            "/bin/sh".into(),
            BASE_IMAGE.into(),
            "-c".into(),
            preparation_script(&assets_root.join("k3s"), &spec.k3s_version),
        ];
        attempted_create = true;
        runtime::run(runner, &args)?;
        let rootfs = scratch.join("rootfs.tar");
        // Apple 1.7.1 ContainerExport moves its temporary archive to this path.
        runtime::run(
            runner,
            &[
                "export".into(),
                "--output".into(),
                rootfs.display().to_string(),
                name.clone(),
            ],
        )?;
        let mut layer = fs::File::open(rootfs).map_err(io_error)?;
        let archive = assets_root.join("apple-node-image.tar");
        let output = fs::File::create(&archive).map_err(io_error)?;
        let reference = write_image(&mut layer, output, &spec.k3s_version)?;
        super::registry::import_image(runner, &archive, &reference)?;
        Ok(reference)
    })();
    let cleanup_container = if attempted_create {
        runtime::run(runner, &["delete".into(), "--force".into(), name]).map(|_| ())
    } else {
        Ok(())
    };
    let cleanup_files = fs::remove_dir_all(scratch).map_err(io_error);
    match (result, cleanup_container, cleanup_files) {
        (Ok(reference), Ok(()), Ok(())) => Ok(reference),
        (Err(error), Ok(()), Ok(())) | (Ok(_), Err(error), _) | (Ok(_), _, Err(error)) => {
            Err(error)
        }
        (Err(error), container, files) => Err(invalid(format!(
            "{error}; image preparation cleanup: container={container:?}, files={files:?}"
        ))),
    }
}

fn preparation_script(binary: &Path, version: &str) -> String {
    format!(
        r#"set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates coreutils findutils gawk grep iproute2 iptables kmod mount procps socat tar tini util-linux
install -m 0755 {binary} /usr/local/bin/k3s
actual=$(/usr/local/bin/k3s --version | awk 'NR==1 {{print $3}}')
test "$actual" = {version}
mkdir -p /usr/local/sbin /etc/rancher/k3s /etc/kalisto
cat > /usr/local/sbin/kalisto-node-init <<'KALISTO_NODE_INIT'
{node_init}KALISTO_NODE_INIT
chmod 0755 /usr/local/sbin/kalisto-node-init
# The pinned Kata kernel enables OverlayFS. Native snapshots copy the whole
# Cilium rootfs for every init container instead of sharing immutable layers.
printf 'snapshotter: overlayfs\n' > /etc/rancher/k3s/config.yaml
printf '%s\n' {version} > /etc/kalisto/k3s-version
# Reusable OS only: never run k3s server/agent during image preparation.
rm -f /etc/machine-id /var/lib/dbus/machine-id /var/lib/systemd/random-seed
: > /etc/machine-id
apt-get clean
rm -rf /var/lib/apt/lists/* /var/log/apt/*
"#,
        binary = shell_escape(&binary.display().to_string()),
        version = shell_escape(version),
        node_init = NODE_INIT,
    )
}

fn write_image<R: Read + Seek, W: Write>(
    layer: &mut R,
    output: W,
    version: &str,
) -> Result<String> {
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = layer.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        size += count as u64;
    }
    if size == 0 {
        return Err(invalid("Apple container exported an empty node rootfs"));
    }
    layer.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let layer_hash = format!("{:x}", hash.finalize());
    let config = json_bytes(&json!({
        "architecture": "arm64", "os": "linux", "variant": "v8",
        "config": {
            "User": "0:0", "WorkingDir": "/", "StopSignal": "SIGTERM",
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Entrypoint": ["/usr/bin/tini", "-g", "--", "/usr/local/sbin/kalisto-node-init"],
            "Labels": {
                "org.opencontainers.image.title": "Kalisto k3s Apple container node",
                "org.opencontainers.image.version": version,
                "org.opencontainers.image.base.name": BASE_IMAGE,
            },
        },
        "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{layer_hash}")]},
    }))?;
    let config_hash = digest(&config);
    let manifest = json_bytes(&json!({
        "schemaVersion": 2, "mediaType": MANIFEST_TYPE,
        "config": descriptor(CONFIG_TYPE, &config_hash, config.len() as u64),
        "layers": [descriptor(LAYER_TYPE, &layer_hash, size)],
    }))?;
    let manifest_hash = digest(&manifest);
    let reference = format!("{IMAGE_REPOSITORY}@sha256:{manifest_hash}");
    let mut manifest_descriptor = descriptor(MANIFEST_TYPE, &manifest_hash, manifest.len() as u64);
    manifest_descriptor["platform"] =
        json!({"os": "linux", "architecture": "arm64", "variant": "v8"});
    // Apple 1.7.1 OCIImageArchive uses these three annotations for local lookup.
    manifest_descriptor["annotations"] = json!({
        "org.opencontainers.image.ref.name": reference,
        "io.containerd.image.name": reference,
        "com.apple.containerization.image.name": reference,
    });
    let index = json_bytes(&json!({"schemaVersion": 2, "manifests": [manifest_descriptor]}))?;
    let mut archive = tar::Builder::new(output);
    let layout = b"{\"imageLayoutVersion\":\"1.0.0\"}";
    append(&mut archive, "oci-layout", layout.len() as u64, &layout[..])?;
    append(
        &mut archive,
        "index.json",
        index.len() as u64,
        index.as_slice(),
    )?;
    append(
        &mut archive,
        &format!("blobs/sha256/{config_hash}"),
        config.len() as u64,
        config.as_slice(),
    )?;
    append(
        &mut archive,
        &format!("blobs/sha256/{manifest_hash}"),
        manifest.len() as u64,
        manifest.as_slice(),
    )?;
    append(
        &mut archive,
        &format!("blobs/sha256/{layer_hash}"),
        size,
        layer,
    )?;
    archive.finish().map_err(io_error)?;
    Ok(reference)
}

fn append<W: Write, R: Read>(
    archive: &mut tar::Builder<W>,
    path: &str,
    size: u64,
    data: R,
) -> Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive
        .append_data(&mut header, path, data)
        .map_err(io_error)
}

fn descriptor(media_type: &str, hash: &str, size: u64) -> Value {
    json!({"mediaType": media_type, "digest": format!("sha256:{hash}"), "size": size})
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}

fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "Apple node image filesystem".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Cursor, Read};

    use serde_json::Value;

    use super::{digest, write_image};

    #[test]
    fn oci_archive_binds_local_reference_and_descriptors_to_actual_content() {
        let mut layer = tar::Builder::new(Vec::new());
        super::append(&mut layer, "etc/example", 4, &b"node"[..]).unwrap();
        let layer = layer.into_inner().unwrap();
        let mut archive = Vec::new();
        let reference =
            write_image(&mut Cursor::new(&layer), &mut archive, "v1.33.5+k3s1").unwrap();
        let mut members = BTreeMap::new();
        for entry in tar::Archive::new(archive.as_slice()).entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            members.insert(path, content);
        }
        let index: Value = serde_json::from_slice(&members["index.json"]).unwrap();
        let descriptor = &index["manifests"][0];
        let manifest_digest = descriptor["digest"].as_str().unwrap();
        assert_eq!(
            reference,
            format!("docker.io/kalisto/apple-k3s-node@{manifest_digest}")
        );
        let manifest_data = &members[&format!("blobs/{}", manifest_digest.replace(':', "/"))];
        assert_eq!(format!("sha256:{}", digest(manifest_data)), manifest_digest);
        assert_eq!(
            manifest_data.len() as u64,
            descriptor["size"].as_u64().unwrap()
        );
        let manifest: Value = serde_json::from_slice(manifest_data).unwrap();
        for descriptor in [&manifest["config"], &manifest["layers"][0]] {
            let hash = descriptor["digest"].as_str().unwrap();
            let data = &members[&format!("blobs/{}", hash.replace(':', "/"))];
            assert_eq!(format!("sha256:{}", digest(data)), hash);
            assert_eq!(data.len() as u64, descriptor["size"].as_u64().unwrap());
        }
        let config_digest = manifest["config"]["digest"].as_str().unwrap();
        let config: Value =
            serde_json::from_slice(&members[&format!("blobs/{}", config_digest.replace(':', "/"))])
                .unwrap();
        assert_eq!(
            config["rootfs"]["diff_ids"][0],
            manifest["layers"][0]["digest"]
        );
        assert_eq!(members[&format!("blobs/sha256/{}", digest(&layer))], layer);
    }

    #[test]
    fn changing_node_rootfs_changes_immutable_local_reference() {
        let first = write_image(&mut Cursor::new(b"first"), Vec::new(), "v1.33.5+k3s1").unwrap();
        let second = write_image(&mut Cursor::new(b"second"), Vec::new(), "v1.33.5+k3s1").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn empty_export_is_rejected_before_loading() {
        assert!(write_image(&mut Cursor::new(b""), Vec::new(), "v1.33.5+k3s1").is_err());
    }
    #[test]
    fn loopback_import_serves_exact_manifest_and_layer_without_extracting() {
        use std::fs;
        use std::io::Write;
        use std::net::TcpStream;
        use std::time::Duration;

        use super::super::registry::ArchiveRegistry;

        struct ArchiveFile(std::path::PathBuf);
        impl Drop for ArchiveFile {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        struct LayoutDirectory(std::path::PathBuf);
        impl Drop for LayoutDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn request(registry: &ArchiveRegistry, method: &str, path: &str) -> Vec<u8> {
            let address = registry.reference().split('/').next().unwrap();
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: {address}\r\n\r\n"
            )
            .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        }
        fn body(response: &[u8]) -> &[u8] {
            let end = response
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            &response[end + 4..]
        }

        let path = std::env::temp_dir().join(format!(
            "kalisto-loopback-import-{}.tar",
            std::process::id()
        ));
        let output = fs::File::create_new(&path).unwrap();
        let path = ArchiveFile(path);
        let layer = b"unique node layer bytes, not an archive header".repeat(8192);
        let reference = write_image(&mut Cursor::new(&layer), output, "v1.33.5+k3s1").unwrap();
        let layout = path.0.with_extension("layout");
        fs::create_dir(&layout).unwrap();
        let layout = LayoutDirectory(layout);
        tar::Archive::new(fs::File::open(&path.0).unwrap())
            .unpack(&layout.0)
            .unwrap();
        for source in [&path.0, &layout.0] {
            let registry = ArchiveRegistry::start(source, &reference).unwrap();
            let manifest_digest = reference.rsplit_once('@').unwrap().1;
            let manifest_path = format!("/v2/kalisto-node/manifests/{manifest_digest}");
            let response = request(&registry, "GET", &manifest_path);
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert_eq!(
                format!("sha256:{}", digest(body(&response))),
                manifest_digest
            );
            let manifest: Value = serde_json::from_slice(body(&response)).unwrap();
            let layer_digest = manifest["layers"][0]["digest"].as_str().unwrap();
            let blob_path = format!("/v2/kalisto-node/blobs/{layer_digest}");
            assert_eq!(
                body(&request(&registry, "GET", &blob_path)),
                layer.as_slice()
            );
            assert!(body(&request(&registry, "HEAD", &blob_path)).is_empty());
            assert!(
                request(&registry, "GET", "/v2/kalisto-node/blobs/../../etc/passwd")
                    .starts_with(b"HTTP/1.1 404")
            );
            assert!(request(&registry, "DELETE", &blob_path).starts_with(b"HTTP/1.1 405"));
        }
    }
}

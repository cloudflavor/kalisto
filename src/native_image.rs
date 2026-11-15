//! Build a reusable node disk on macOS/VZ without Docker or host privileges.
//! Only OS preparation runs here: no Kubernetes server, node identity or CNI is started.

use std::fs;
use std::io::{BufReader, Write};

use flate2::{Compression, write::GzEncoder};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::artifact;
use crate::cluster::spec::{ClusterSpec, ImageArch};
use crate::error::{Error, Result};
use crate::paths::{artifacts_root, data_root};
use crate::process::CommandRunner;
use crate::shell::shell_escape;

const UBUNTU_RELEASE: &str = "release-20261001";

pub fn prepare(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<PathBuf> {
    if std::env::consts::OS != "macos" {
        return Err(invalid("native Lima image preparation requires macOS"));
    }
    let arch = spec.image_arch.unwrap_or_else(ImageArch::host_default);
    if arch != ImageArch::host_default() {
        return Err(invalid(
            "VZ image preparation requires the Mac's native architecture",
        ));
    }
    let suffix = arch.lima_artifact_suffix();
    let root = artifacts_root()?.join("lima-node");
    fs::create_dir_all(&root).map_err(io_error)?;
    let image = root.join(format!(
        "ubuntu-24.04-minimal-k3s-{suffix}-finalized.raw.gz"
    ));
    let template = root.join(format!("ubuntu-24.04-minimal-k3s-{suffix}-finalized.yaml"));
    if image.is_file() {
        write_template(&template, &image, arch)?;
        return Ok(template);
    }
    let cached_raw = image.with_extension("");
    if cached_raw.is_file() {
        compress_disk(&cached_raw, &image)?;
        write_template(&template, &image, arch)?;
        return Ok(template);
    }
    let (checksum, mirror_arch) = match arch {
        ImageArch::Aarch64 => (
            "fbf6b15c50c474dc6dee5b3c74da212bb687a2625e877227828e6492d88dc151",
            "arm64",
        ),
        ImageArch::X86_64 => (
            "a8eb6570f87a941f3de2e000c046b9367f8ab1423756a94830d454fae8ad80ff",
            "amd64",
        ),
    };
    let seed_url = format!(
        "https://cloud-images.ubuntu.com/minimal/releases/noble/{UBUNTU_RELEASE}/ubuntu-24.04-minimal-cloudimg-{mirror_arch}.img"
    );
    let seed = artifact::fetch_into_cache(
        runner,
        &root.join("downloads"),
        &seed_url,
        Some(checksum),
        "Ubuntu node seed",
        "ubuntu.img",
    )?;
    let raw_seed = seed.with_extension(format!("{}.raw", &checksum[..16]));
    if !raw_seed.is_file() {
        let temporary = raw_seed.with_extension("raw.partial");
        if temporary.exists() {
            fs::remove_file(&temporary).map_err(io_error)?;
        }
        crate::qcow::to_raw(&seed, &temporary)?;
        fs::rename(&temporary, &raw_seed).map_err(io_error)?;
    }
    let binary = crate::pull::k3s_binary(spec, runner)?;
    let builder_home = data_root()?.join("i");
    fs::create_dir_all(&builder_home).map_err(io_error)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    let name = format!("i{nonce:x}");
    let config_path = builder_home.join(format!("{name}.yaml"));
    let cache_root = artifacts_root()?.canonicalize().map_err(io_error)?;
    let binary = binary.canonicalize().map_err(io_error)?;
    let config = json!({
        "vmType": "vz", "mountType": "virtiofs", "cpus": 4, "memory": "4GiB", "disk": "4GiB",
        "containerd": {"system": false, "user": false},
        "mounts": [{"location": cache_root, "writable": false}],
        "images": [{"location": raw_seed, "arch": arch.lima_template_value()}],
        "provision": [{"mode": "system", "script": provision_script(&binary, &spec.k3s_version)}]
    });
    fs::write(
        &config_path,
        serde_yaml::to_string(&config).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)?;
    let env = [("LIMA_HOME", builder_home.as_path())];
    let result = (|| {
        runner.run_with_env(
            "limactl",
            &[
                "start".into(),
                "--tty=false".into(),
                format!("--name={name}"),
                "--network=vzNAT".into(),
                "--timeout=15m".into(),
                config_path.display().to_string(),
            ],
            &env,
        )?;
        // Guest root is provided by Lima; no sudo or other privileged command is run on the Mac.
        runner.run_with_env("limactl", &[
            "shell".into(), "--tty=false".into(), name.clone(), "sudo".into(), "sh".into(), "-lc".into(),
            "set -eu; test -f /etc/kalisto/node-image.env; test ! -d /var/lib/rancher/k3s/server; apt-get clean; rm -rf /var/lib/apt/lists/*; journalctl --rotate || true; journalctl --vacuum-time=1s || true; cloud-init clean --logs --seed --machine-id; rm -f /etc/netplan/50-cloud-init.yaml /etc/ssh/ssh_host_*; find /root /home -type f -name authorized_keys -delete; sync".into(),
        ], &env)?;
        runner.run_with_env(
            "limactl",
            &["stop".into(), "--tty=false".into(), name.clone()],
            &env,
        )?;
        let disk = builder_home.join(&name).join("disk");
        if !disk.is_file() {
            return Err(invalid("native builder did not produce a VZ disk"));
        }
        compress_disk(&disk, &image)?;
        write_template(&template, &image, arch)?;
        Ok(template.clone())
    })();
    let _ = runner.run_with_env(
        "limactl",
        &[
            "delete".into(),
            "--tty=false".into(),
            "--force".into(),
            name,
        ],
        &env,
    );
    let _ = fs::remove_file(config_path);
    result
}

/// Upgrade only the managed raw-image reference; preserve custom template fields.
pub fn pack_cached_template(spec: &ClusterSpec) -> Result<Option<PathBuf>> {
    if spec.backend != crate::backend::runtime::BackendKind::Lima {
        return Ok(None);
    }
    let arch = spec.image_arch.unwrap_or_else(ImageArch::host_default);
    let root = artifacts_root()?.join("lima-node");
    let stem = format!(
        "ubuntu-24.04-minimal-k3s-{}-finalized",
        arch.lima_artifact_suffix()
    );
    let template = root.join(format!("{stem}.yaml"));
    let raw = root.join(format!("{stem}.raw"));
    if spec.template != template.display().to_string() || !raw.is_file() {
        return Ok(None);
    }
    let mut document: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&template).map_err(io_error)?)
            .map_err(|error| invalid(error.to_string()))?;
    let Some(images) = document
        .get_mut("images")
        .and_then(serde_yaml::Value::as_sequence_mut)
    else {
        return Ok(None);
    };
    let image = root.join(format!("{stem}.raw.gz"));
    let mut changed = false;
    for entry in images {
        if entry.get("location").and_then(serde_yaml::Value::as_str) == raw.to_str() {
            if !image.is_file() {
                compress_disk(&raw, &image)?;
            }
            entry["location"] = serde_yaml::Value::String(image.display().to_string());
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    let temporary = template.with_extension("yaml.partial");
    fs::write(
        &temporary,
        serde_yaml::to_string(&document).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)?;
    fs::rename(temporary, &template).map_err(io_error)?;
    Ok(Some(template))
}

fn compress_disk(source: &Path, destination: &Path) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    let temporary = destination.with_extension(format!("{nonce}.partial"));
    let result = (|| {
        let input = fs::File::open(source).map_err(io_error)?;
        let output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error)?;
        let mut encoder = GzEncoder::new(output, Compression::fast());
        std::io::copy(
            &mut BufReader::with_capacity(1024 * 1024, input),
            &mut encoder,
        )
        .map_err(io_error)?;
        encoder.flush().map_err(io_error)?;
        encoder
            .finish()
            .map_err(io_error)?
            .sync_all()
            .map_err(io_error)?;
        fs::rename(&temporary, destination).map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn provision_script(binary: &Path, version: &str) -> String {
    format!(
        r"#!/bin/sh
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get -o Acquire::Retries=5 update
apt-get -o Acquire::Retries=5 install -y --no-install-recommends dbus rsync lsb-release systemd-resolved iptables procps socat kmod
install -m 0755 {binary} /usr/local/bin/k3s
/usr/local/bin/k3s --version
mkdir -p /etc/kalisto /etc/modules-load.d /etc/sysctl.d
printf 'overlay\nbr_netfilter\n' >/etc/modules-load.d/kalisto-k3s.conf
printf 'net.ipv4.ip_forward=1\nnet.bridge.bridge-nf-call-iptables=1\nnet.bridge.bridge-nf-call-ip6tables=1\n' >/etc/sysctl.d/90-kalisto-k3s.conf
ln -sfn /run/systemd/resolve/stub-resolv.conf /etc/resolv.conf
printf '%s\n' {marker} >/etc/kalisto/node-image.env
",
        binary = shell_escape(&binary.display().to_string()),
        marker = shell_escape(&format!(
            "K3S_VERSION={version}\nIMAGE_FLAVOR=ubuntu-24.04-minimal"
        )),
    )
}

fn write_template(path: &Path, image: &Path, arch: ImageArch) -> Result<()> {
    let config = json!({
        "vmType": "vz", "mountType": "virtiofs", "mounts": [],
        "containerd": {"system": false, "user": false},
        "cpus": 2, "memory": "4GiB", "disk": "12GiB",
        "images": [{"location": image, "arch": arch.lima_template_value()}]
    });
    fs::write(
        path,
        serde_yaml::to_string(&config).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}
fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "native node image".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_export_preserves_source_and_all_logical_disk_bytes() {
        use std::io::Read;
        let root =
            std::env::temp_dir().join(format!("kalisto-native-compress-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.raw");
        let destination = root.join("node.raw.gz");
        let mut bytes = vec![0; 1024 * 1024];
        bytes[65_536..131_072].fill(42);
        fs::write(&source, &bytes).unwrap();
        compress_disk(&source, &destination).unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(fs::File::open(destination).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, bytes);
        assert_eq!(fs::read(source).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guest_provisioning_never_starts_a_cluster_or_requires_checkout_scripts() {
        let script = provision_script(Path::new("/cache/k3s"), "v1.33.5+k3s1");
        assert!(!script.contains("k3s server"));
        assert!(!script.contains("k3s agent"));
        assert!(!script.contains("scripts/"));
        assert!(script.contains("rsync"));
        assert!(script.contains("node-image.env"));
    }
}

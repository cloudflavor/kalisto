# Kalisto

Early validation harness for a local-development Kubernetes runtime on macOS using Lima.

Current scope:

- macOS backend: Lima (`vz` + Rosetta)
- Linux backend: Firecracker (in progress)
- Kubernetes distro: `k3s`
- image pipeline: container-built seed image plus macOS finalization pass

## Product commands

The first product-facing commands are available now:

```bash
cargo run -- up
cargo run -- ls
cargo run -- down --name kalisto
cargo run -- destroy --name kalisto
cargo run -- status --name kalisto
cargo run -- kubeconfig --name kalisto
cargo run -- bench --runs 3
```

Lifecycle progress logs are enabled by default at `info` level, so `up`, `down`, and `bench`
show stage transitions as they happen. Raw streamed Lima startup output and live `k3s`
bootstrap logs are only shown at `debug`. Use `RUST_LOG` to raise or lower verbosity:

```bash
RUST_LOG=debug cargo run -- up
RUST_LOG=warn cargo run -- bench --runs 3
```

Optional `up` knobs:

```bash
cargo run -- up \
  --project-root . \
  --workers 0 \
  --api-port 16443 \
  --template /Users/runner/projects/rust/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml
```

Kalisto stores its own files under XDG-style roots:

```text
config: ~/.config/kalisto
state:  ~/.local/share/kalisto/clusters/<name>/
```

The benchmark command runs the real lifecycle path repeatedly and prints per-run timings:

```bash
cargo run -- bench \
  --runs 3
```

## Lima image pipeline

Kalisto uses a two-stage image flow:

1. a Linux-container seed image build that runs in GitLab or `opal`
2. a macOS-only finalization pass that boots the seed once under Lima and exports a warmed node disk

Relevant files:

- [images/macos/lima-node/ubuntu-24.04.yaml](/Users/runner/projects/rust/kalisto/images/macos/lima-node/ubuntu-24.04.yaml)
- [scripts/macos/provision_lima_node.sh](/Users/runner/projects/rust/kalisto/scripts/macos/provision_lima_node.sh)
- [scripts/macos/build_lima_node_image.sh](/Users/runner/projects/rust/kalisto/scripts/macos/build_lima_node_image.sh)
- [scripts/macos/finalize_lima_node_image.sh](/Users/runner/projects/rust/kalisto/scripts/macos/finalize_lima_node_image.sh)
- [scripts/macos/render_lima_artifact_template.sh](/Users/runner/projects/rust/kalisto/scripts/macos/render_lima_artifact_template.sh)
- [/.opal/config.toml](/Users/runner/projects/rust/kalisto/.opal/config.toml)
- [/.gitlab-ci.yml](/Users/runner/projects/rust/kalisto/.gitlab-ci.yml)

The checked-in `opal` config raises the default Apple `container` job VM size for this repo to `6` CPUs and `8g` of memory, which keeps the `libguestfs` image build from running in the tiny default microVM.

Build the seed image:

```bash
./scripts/macos/build_lima_node_image.sh
```

Finalize it on macOS:

```bash
sh scripts/macos/finalize_lima_node_image.sh
```

Prerequisites for the macOS finalization step:

- `limactl`
- `qemu-img`
- `sha256sum` or `shasum`

If `qemu-img` is not already installed on macOS, install it before running the
finalization script, for example with Homebrew:

```bash
brew install qemu
```

Default artifacts:

- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.qcow2`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.yaml`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.manifest`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.qcow2`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.manifest`

## Releasing Lima images

For a usable Kalisto Lima release, the important runtime artifact is the finalized
disk image. Kalisto can render the matching Lima template locally from that image,
so the minimum published artifact set is:

- required:
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.qcow2`
- recommended:
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.manifest`
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml`
- optional intermediate seed artifacts:
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.qcow2`
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.yaml`
  - `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.manifest`

Build and finalize the release image with these scripts:

```bash
./scripts/macos/build_lima_node_image.sh
sh scripts/macos/finalize_lima_node_image.sh
```

The finalize step requires `qemu-img` because Kalisto now exports the warmed
Lima VM disk as a compact finalized `qcow2` artifact instead of publishing the
raw sparse disk directly.

The first script produces the seed image and manifest. The second script boots
that seed once under Lima, waits for the guest to contain the Kalisto node-image
marker and working `k3s` binary, then exports the finalized image, template, and
manifest.

To install a release artifact into Kalisto's managed XDG cache locally:

```bash
mkdir -p ~/.local/share/kalisto/artifacts/lima-node
cp artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.qcow2 \
  ~/.local/share/kalisto/artifacts/lima-node/
sh scripts/macos/render_lima_artifact_template.sh \
  ~/.local/share/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.qcow2 \
  ~/.local/share/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml
```

Kalisto now looks in this XDG cache first:

```text
~/.local/share/kalisto/artifacts/lima-node/
```

If you publish the finalized image at a stable URL, Kalisto can fetch it on miss
into that cache when these environment variables are set:

```bash
export KALISTO_LIMA_IMAGE_URL=<published-url>/ubuntu-24.04-minimal-k3s-arm64-finalized.qcow2
export KALISTO_LIMA_IMAGE_SHA256=<sha256>
```

`KALISTO_LIMA_IMAGE_FILENAME` is also supported if the published file name differs
from the default finalized image name.

## Firecracker artifact pipeline

The Linux Firecracker path should reuse the same Ubuntu guest lineage as Lima, but emit Firecracker-specific artifacts.

Current shape:

- shared guest provisioning script:
  - [scripts/image/provision_kalisto_node.sh](/home/runner/projects/rust/kalisto/scripts/image/provision_kalisto_node.sh)
- Linux kernel builder:
  - [scripts/linux/build_firecracker_kernel.sh](/home/runner/projects/rust/kalisto/scripts/linux/build_firecracker_kernel.sh)
- Linux rootfs builder:
  - [scripts/linux/build_firecracker_rootfs.sh](/home/runner/projects/rust/kalisto/scripts/linux/build_firecracker_rootfs.sh)
- unified artifact manifest renderer:
  - [scripts/linux/render_firecracker_artifact_manifest.sh](/home/runner/projects/rust/kalisto/scripts/linux/render_firecracker_artifact_manifest.sh)
- prerequisites:
  - [docs/prerequisites-linux-firecracker.md](/home/runner/projects/rust/kalisto/docs/prerequisites-linux-firecracker.md)

For rootless Linux Firecracker networking, install `slirp4netns` and use
`--network slirp4netns`.

Build the Firecracker kernel, rootfs, and unified manifest locally on Linux:

```bash
./scripts/linux/build_firecracker_kernel.sh
./scripts/linux/build_firecracker_rootfs.sh
./scripts/linux/render_firecracker_artifact_manifest.sh
```

By default, the Linux Firecracker scripts detect the installed Firecracker
version and host architecture, then place artifacts under a versioned directory
such as `artifacts/firecracker-node/v1.13/x86_64/`.

Default artifacts for a host running Firecracker `v1.13.x` on `x86_64`:

- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux`
- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux.config`
- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux.manifest`
- `artifacts/firecracker-node/v1.13/x86_64/ubuntu-24.04-minimal-k3s-amd64-rootfs.raw`
- `artifacts/firecracker-node/v1.13/x86_64/ubuntu-24.04-minimal-k3s-amd64-rootfs.manifest`
- `artifacts/firecracker-node/v1.13/x86_64/firecracker-artifacts.manifest`

## Firecracker artifact manager

`kalisto up` on Linux now resolves Firecracker boot artifacts in this order:

1. explicit CLI paths:
   - `--firecracker-kernel`
   - `--firecracker-rootfs`
2. versioned artifact roots for the detected Firecracker release track and host architecture:
   - `artifacts/firecracker-node/<major.minor>/<arch>/`
   - `~/.local/share/kalisto/artifacts/firecracker-node/<major.minor>/<arch>/`
3. legacy artifact roots:
   - `artifacts/firecracker-node/`
   - `~/.local/share/kalisto/artifacts/firecracker-node/`
4. optional fetch-on-miss URLs declared explicitly or through a published Kalisto manifest

On Linux, Kalisto now detects the installed Firecracker version from `firecracker --version`, maps it to the matching upstream artifact track such as `v1.13`, and prefers the cache path under:

```text
~/.local/share/kalisto/artifacts/firecracker-node/v1.13/x86_64/
```

If no managed artifacts are present locally, Kalisto can fetch them into that
versioned directory from either:

- `KALISTO_FIRECRACKER_MANIFEST_URL`, which supplies the managed filenames,
  checksums, URLs, and baked `k3s` version
- explicit `KALISTO_FIRECRACKER_KERNEL_URL` and
  `KALISTO_FIRECRACKER_ROOTFS_URL` overrides

The default managed kernel artifact is now Kalisto-owned instead of discovered
from the upstream Firecracker CI bucket. The writable Firecracker rootfs
remains Kalisto-managed and is still expected to come from a local workspace
artifact, a published Kalisto artifact set, or an explicit path override.

Supported fetch-on-miss environment variables:

- `KALISTO_FIRECRACKER_MANIFEST_URL`
- `KALISTO_FIRECRACKER_KERNEL_URL`
- `KALISTO_FIRECRACKER_ROOTFS_URL`
- `KALISTO_FIRECRACKER_KERNEL_FILENAME`
- `KALISTO_FIRECRACKER_ROOTFS_FILENAME`
- `KALISTO_FIRECRACKER_KERNEL_SHA256`
- `KALISTO_FIRECRACKER_ROOTFS_SHA256`
- `KALISTO_FIRECRACKER_ARTIFACT_DIR`
- `KALISTO_FIRECRACKER_ARTIFACT_CACHE_DIR`
- `KALISTO_FIRECRACKER_VERSION`
- `KALISTO_FIRECRACKER_ARCH`

Current limitation:

- auto-managed Firecracker rootfs resolution only supports the `k3s` version declared by the managed artifact set
- non-default `k3s` versions still require an explicit `--firecracker-rootfs`

The GitLab pipeline now publishes a combined `publish:firecracker-artifacts` job artifact set. A stable manifest URL for the default branch can be addressed through GitLab's latest-artifact endpoint pattern:

```text
<project-url>/-/jobs/artifacts/<ref>/raw/artifacts/firecracker-node/<major.minor>/<arch>/firecracker-artifacts.manifest?job=publish:firecracker-artifacts
```

## Lima substrate probe

Run the Lima substrate probe:

```bash
cargo run -- probe-macos-lima --workspace-root .
```

Optional overrides:

```bash
cargo run -- probe-macos-lima \
  --workspace-root . \
  --template /Users/runner/projects/rust/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml \
  --network vzNAT
```

What it validates:

- `limactl` reachability
- Lima instance startup with `vmType=vz`
- writable workspace mount visibility
- cgroup and overlayfs visibility
- Rosetta/binfmt visibility when enabled

## Lima k3s probe

Run the Lima-backed `k3s` probe:

```bash
cargo run -- probe-macos-lima-k3s --workspace-root .
```

Agentless control-plane probe:

```bash
cargo run -- probe-macos-lima-k3s \
  --workspace-root . \
  --agentless
```

Minimal multi-node probe:

```bash
cargo run -- probe-macos-lima-k3s \
  --workspace-root . \
  --template /Users/runner/projects/rust/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml \
  --workers 1
```

The current bootstrap baseline intentionally disables bundled add-ons at server start so they can become post-boot plugin boundaries:

- `traefik`
- `servicelb`
- `metrics-server`
- `local-storage`
- `coredns`
- embedded network policy

The current baseline keeps:

- `containerd`
- `kube-proxy`
- Flannel

## Live tests

The live Lima tests are opt-in:

```bash
KALISTO_LIVE_LIMA_TESTS=1 \
cargo test --test macos_lima_live -- --nocapture
```

```bash
KALISTO_LIVE_LIMA_K3S_TESTS=1 \
cargo test --test macos_lima_k3s_live -- --nocapture
```

For a live worker-join probe:

```bash
KALISTO_LIVE_LIMA_K3S_TESTS=1 \
KALISTO_LIMA_K3S_WORKERS=1 \
cargo test --test macos_lima_k3s_live -- --nocapture
```

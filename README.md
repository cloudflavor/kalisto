# Kalisto

Early validation harness for a local-development Kubernetes runtime on macOS using Lima.

Current scope:

- macOS backend: Lima (`vz` + Rosetta)
- Kubernetes distro: `k3s`
- image pipeline: container-built seed image plus macOS finalization pass

## Product commands

The first product-facing commands are available now:

```bash
cargo run -- up --workspace-root .
cargo run -- status --workspace-root .
cargo run -- kubeconfig --workspace-root .
cargo run -- down --workspace-root .
```

Optional `up` knobs:

```bash
cargo run -- up \
  --workspace-root . \
  --workers 1 \
  --api-port 16443 \
  --template /Users/runner/projects/rust/kalisto/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml
```

Cluster state is stored under:

```text
.kalisto/clusters/<name>/
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

Default artifacts:

- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.qcow2`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.yaml`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.img`
- `artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml`

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

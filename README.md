# Kalisto

Local-development Kubernetes runtime using k3s on macOS and Linux.

Current scope:

- macOS backend: native Lima `vz` (Rosetta opt-in)
- opt-in macOS backend: Apple `container` running Kalisto-managed k3s nodes
- Linux backend: Firecracker (in progress)
- Kubernetes distro: `k3s`
- image pipeline: native VZ OS preparation; optional legacy container builders

## Binary releases

Pushed version tags publish packages to [forgejo Releases](https://forgejo.example.test/cloudflavor/kalisto/releases):
**macOS amd64/arm64 and static Linux amd64/arm64**, with SHA-256 checksums.
All targets must build successfully before the release is published.
See [release targets, verification and tagging](docs/releases.md). Linux backend
support remains in progress; available binaries do not imply feature parity.

## Quick Start

Install the standalone macOS binary, then run from any directory:

```bash
kalisto pull --addons cni/cilium
kalisto up --workers 2 --addons cni/cilium
```

Preparation is per artifact profile (architecture, K3s/Cilium versions, and enabled
components), **not per cluster or working directory**. Reuse that cache for fresh
boots; rerun `pull` when the profile changes or cache is removed. Repeating `pull`
verifies/reuses a complete cache rather than rebuilding the OS image. Bare
`pull`/`up` do **not** select a CNI.

`pull` fetches checksum-verified managed Lima, Helm, Crane, K3s, and
platform-digest-pinned OCI images including a BusyBox diagnostic image. It prepares
a sanitized compressed Ubuntu disk via a temporary native VZ VM, never starting
Kubernetes or creating cluster state; the builder is deleted after use. Standard
flow requires no checkout, manual templates, Homebrew, Docker, QEMU install, or
host sudo. Cache lives under `artifacts/boot-v4` in user-scoped Kalisto data.
Preparation can take several minutes and stays outside cold cluster startup timing.

`up` requires a complete cache, mounts it read-only, imports local images on fresh
VMs, and applies Cilium without Helm download jobs. The cached chart is rendered
locally during `up` so generated Hubble TLS keys are unique to each cluster, not
shared cache contents. Its private manifest is stored with mode `0600` in that
cluster's runtime directory and reused on resume. All rendered DaemonSets and
Deployments must finish rolling out. Cilium uses embedded Envoy with L7 policy
support enabled rather than a separate proxy DaemonSet. Default CNI remains `none`:
explicit Cilium selection is required for pod networking. MCP exposes the same
preparation through `clusters.pull`, using the lifecycle arguments of `clusters.up`.

The prepared startup profiles support Lima and Apple containers, no CNI or Cilium,
and optional `lb/servicelb` only. Other startup add-ons are rejected rather than downloaded
silently; install those explicitly after startup. Existing Helm-managed Cilium
clusters require migration; prepared startup currently targets fresh clusters.

The opt-in release benchmark in `tests/macos_three_node_boot_budget.rs` passed
in **56.07s, 53.84s, 54.26s, and 54.83s** on the development Mac. Each run used the actual
binary from an empty directory with system-only `PATH`, fresh three VMs, three
Ready nodes, authenticated host API, Cilium rollouts, and DNS/Service HTTP probes
from both workers. Image imports and VM creation remain inside the timer;
`pull` preparation stays outside it. These measurements are not a universal
hardware performance guarantee.

## Apple container backend

On Apple silicon with macOS 26 or newer, select Apple's lightweight Linux VM runtime
without using its experimental Kubernetes cluster implementation:

```bash
kalisto pull --backend apple-container --addons cni/cilium
kalisto up --backend apple-container --workers 2 --addons cni/cilium
kalisto kubectl -- get nodes
kalisto down
kalisto up
```

Kalisto owns the k3s server, agents, Cilium, readiness checks, and cluster state.
It does not use `container cluster`, `container machine`, kubeadm, kind, Docker,
or an external registry to bootstrap prepared nodes. Lima remains the macOS default.

`pull` installs a pinned, signed Apple runtime in Kalisto's user data, prepares its
kernel and init image, and composes a digest-pinned Ubuntu/k3s node image. A temporary
ordinary container installs Linux prerequisites, then is deleted; no cluster identity
is baked into the reusable image. The cached node OCI archive can restore a pruned
runtime image store when `pull` is repeated.

`up` requires the local node image and prepared assets; it never installs tools,
pulls missing images, or changes the selected CNI. Nodes start concurrently on an
owned NAT network, with only their own runtime directory and read-only boot assets
mounted from the host. No home directory, SSH agent, or global shared image store
is exposed. Workload registry access remains available after startup.

Apple's service is shared within a macOS login session. Kalisto refuses to take
over a running service with a different version or installation/data root; it does
not stop another installation automatically. Stop that installation yourself before
preparing Kalisto's managed runtime.

The Apple backend uses the same three-node, authenticated-API, Cilium rollout,
worker DNS, and cross-node Service HTTP budget scenario as Lima:

```bash
KALISTO_LIVE_APPLE_CONTAINER_BOOT_BUDGET=1 \
  cargo test --release --test macos_three_node_boot_budget \
  apple_container_k3s_has_functional_three_node_cilium_under_one_minute -- --nocapture
```

Run live validation only in the authorized native macOS CI job. The existing Lima
timings above are not Apple-container measurements. Cold archive export/import
currently remains Lima-only; existing Lima clusters and archives are not converted
by selecting a different backend.

## Install workloads

After `up`, Cilium and CoreDNS provide pod networking and DNS. Supply a Kubernetes
client such as `kubectl`; the standalone host command is not added to `PATH`:

```bash
export KUBECONFIG="$(kalisto kubeconfig --path)"
kubectl get nodes
kubectl -n kube-system get pods
kubectl apply -f app.yaml
```

Or work without a host Kubernetes client: `kalisto apply` ships a manifest to the
running cluster, and `kalisto kubectl` runs a checksum-verified managed kubectl
binary matching the cluster's Kubernetes version (prepared by `pull`, otherwise fetched on first use):

`app.yaml` is user-provided. New workload images may require registry access after
startup; the boot cache does not include every application. Ingress is not installed
by default.

## Save / restore installed clusters

Installed workloads and VM-local data already survive `down` / `up`. To transfer a
cold cluster with its disks, credentials and boot assets:

```bash
kalisto archive-keygen --output /secure/dev.agekey
# Replace age1... with the printed public recipient:
kalisto export --name dev --output /secure/dev.kalisto.age --recipient age1... --stop
# On a compatible target without a same-name cluster or Lima instances:
kalisto import /secure/dev.kalisto.age --identity /secure/dev.agekey
kalisto up --name dev
```

Without `--stop`, export requires stopped nodes. Import never overwrites or boots
anything, and preserves original cluster/node names. Native `age` encryption needs
no external tools. Alternatively use `--passphrase` for terminal prompts or a private
`--passphrase-file` for automation. Keep recovery keys separate from backups.
**No encryption flags means an unencrypted, secret-bearing archive.** External host
mounts/additional disks/custom provisioning are rejected rather than omitted.
See [archive safety, compatibility and validation limits](docs/cluster-archives.md).
Native restore evidence is still pending; mocked disk tests are not live VM proof.

## AI agents

**CLI + a small skill/instruction layer is the default recommendation for coding
agents with shell access.** Kalisto also already includes an optional stdio MCP
server for structured tools and background lifecycle tasks. No new server is needed.
Lifecycle MCP calls require a task-capable client; not every MCP integration supports
that. See [AI agent workflows, MCP setup, and safety rules](docs/ai-agents.md).

## Product commands

Use the installed binary; a checkout and `cargo run` are not required:

```bash
kalisto pull --addons cni/cilium
kalisto up --workers 2 --addons cni/cilium
kalisto ls
kalisto --json ls
kalisto apply --name kalisto --manifest app.yaml
kalisto kubectl --name kalisto -- get nodes
kalisto down --name kalisto
kalisto destroy --name kalisto
kalisto status --name kalisto
kalisto kubeconfig --name kalisto
kalisto bench --runs 3 --workers 2 --addons cni/cilium
```

`down` stops VMs but retains state/disks. `destroy` deletes them irreversibly.
`destroy --all` / `destroy -a` deletes all saved clusters; use only for an explicitly
authorized full cleanup. Prepared artifacts remain cached.

Lifecycle progress logs are enabled by default at `info` level, so `up`, `down`, and `bench`
show stage transitions as they happen. Raw streamed Lima startup output and live `k3s`
bootstrap logs are only shown at `debug`. Use `RUST_LOG` to raise or lower verbosity:

```bash
RUST_LOG=debug kalisto up --addons cni/cilium
RUST_LOG=warn kalisto bench --runs 3 --workers 2 --addons cni/cilium
```

Optional `up` knobs:

```bash
kalisto up \
  --addons cni/cilium \
  --project-root . \
  --workers 0 \
  --api-port 16443
```

Install ingress explicitly after prepared startup (these are online operations,
not part of the prepared boot budget):

```bash
# Traefik ingress + MetalLB
kalisto addon install --name kalisto ingress/traefik
kalisto addon install --name kalisto lb/metallb

# Or Kong ingress + MetalLB
kalisto addon install --name kalisto ingress/kong
kalisto addon install --name kalisto lb/metallb
```

MetalLB address configuration is config-only via `kalisto.toml`. Add this after
prepared base startup; enabling it before `pull` selects an unsupported offline profile:

```toml
[plugins.metallb]
enabled = true
mode = "layer2"
addresses = ["192.168.1.240-192.168.1.250"]
```

You can also supply a full MetalLB config manifest from disk:

```toml
[plugins.metallb]
enabled = true
config_file = "manifests/metallb.yaml"
```

Notes:

- `addresses` and `config_file` are mutually exclusive.
- If `addresses` or `config_file` is set, Kalisto auto-enables `lb/metallb`.
- Pick ranges that are reachable from your host network and do not overlap active DHCP/static assignments.

Current ingress defaults baked into add-on manifests:

- Traefik:
  - `service.type=LoadBalancer`
  - Kubernetes Ingress `publishedService` enabled
- Kong:
  - `ingressClass=kong`, `createIngressClass=true`
  - `ingressController.env.publish_service=kube-system/kong-kong-proxy`
  - `proxy.type=LoadBalancer`

## Ingress hostnames

To expose an app, create a Kubernetes `Ingress` and set `spec.rules[].host` to your own domain (or an `xip.kali.st` hostname).

Resolve ingress service IPs:

```bash
KUBECONFIG_PATH="$(kalisto kubeconfig --path)"
kubectl --kubeconfig "$KUBECONFIG_PATH" -n kube-system get svc traefik
kubectl --kubeconfig "$KUBECONFIG_PATH" -n kube-system get svc kong-kong-proxy
```

If you want wildcard-style local hostnames, you can still use `xip.kali.st`.
Example host format:

- `myapp.<LB_IP>.xip.kali.st`
- example: `myapp.192.168.1.50.xip.kali.st`

Traefik Ingress example:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: myapp-traefik
spec:
  ingressClassName: traefik
  rules:
    - host: myapp.192.168.1.50.xip.kali.st
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: myapp
                port:
                  number: 80
```

Kong Ingress example:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: myapp-kong
spec:
  ingressClassName: kong
  rules:
    - host: myapp.192.168.1.50.xip.kali.st
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: myapp
                port:
                  number: 80
```

## `kalisto expose svc` target UX

The target workflow is:

```bash
kalisto expose svc myapp --port 80 --ingress traefik
```

Expected behavior for this planned command:

- discover ingress LoadBalancer IP
- generate host like `<service>.<lb-ip>.xip.kali.st`
- create/update Ingress resource with selected ingress class
- optionally annotate the ingress controller service for domain automation
- print final URL

Current status:

- not implemented yet; use explicit Ingress manifests as shown above
- add-on install does not apply a global domain annotation by default
- MetalLB IP pool configuration is read from `kalisto.toml` only

Kalisto stores its own files under XDG-style roots:

```text
config: ~/.config/kalisto
state:  ~/.local/share/kalisto/clusters/<name>/
```

The optional benchmark command runs the real lifecycle path repeatedly and prints
per-run timings:

```bash
kalisto bench --runs 3 --workers 2 --addons cni/cilium
```

This prints lifecycle timings; the functional three-node budget test below performs
stronger API, DNS, and cross-node traffic checks.

## Three-node cold-start budget

The release test runs the actual CLI from an empty working directory with system-only
`PATH`. `pull` runs outside the timer; all three fresh cluster VMs are created inside
it. Managed native VZ preparation supplies the sanitized compressed raw node image:
no manual template, hand-built qcow2, or host Helm/Crane installation is required.

The same clock includes image imports, three Ready nodes, authenticated host API,
every rendered Cilium DaemonSet/Deployment rollout, DNS from both workers, and
cross-node HTTP through a Service. Cilium uses embedded Envoy with L7 policy support;
there is no separate Envoy DaemonSet. Probe images use cached platform digests with
`imagePullPolicy: Never`. Preparation never creates cluster state or benchmark VMs;
precreated cluster VMs and manual `k3s` bootstrap do not qualify.

Four development-Mac runs passed in **56.07s, 53.84s, 54.26s, and 54.83s**. These are
measurements, not a universal hardware guarantee or a sub-minute claim for first-time
preparation.

Maintainer benchmark on a CI macOS runner:

```bash
KALISTO_LIVE_THREE_NODE_BOOT_BUDGET=1 \
  cargo test --release --test macos_three_node_boot_budget -- --nocapture
```

The test supplies its own empty workdir and does not use `KALISTO_LIMA_TEMPLATE`.
Evidence under `target/kalisto/boot-budget/` includes aggregate `summary.json`, per-run
`result.json`, preparation/startup logs, pod/events JSON, and VM boot/image-import
logs captured before cleanup. CI is the default execution path; local builds and
benchmarks require explicit user authorization. On this development Mac, build/image
artifacts, caches, and large temporaries must stay on `/Volumes/Builds/runner/`;
cluster VM runtime disks may use the local SSD.

## Contributor harness examples

The examples below are source-checkout development commands, not installation
prerequisites for binary users. Sample E2E harness projects live under
[examples/](examples/):

```bash
cargo run -- test ls --project-root examples/harness/nginx-smoke
cargo run -- test ls --project-root examples/harness/job-smoke
```

## Build versioning

`cargo build` generates a build version used by both `kalisto --version` and MCP
`serverInfo.version`.

Version rules:

- exact clean release tag `vX.Y.Z` at `HEAD` -> `X.Y.Z`
- off-tag or dirty builds with a nearest release tag `vX.Y.Z` ->
  `X.Y.(Z+1)-dev.<commits_since_vX.Y.Z>+g<short_sha>[.dirty]`
- repositories without a matching release tag ->
  `<Cargo.toml version>-dev.<commit_count>+g<short_sha>[.dirty]`

Examples:

- on tag `v1.4.2` (clean): `1.4.2`
- 7 commits after `v1.4.2`: `1.4.3-dev.7+gabc1234def56`
- same commit with tracked local edits: `1.4.3-dev.7+gabc1234def56.dirty`

To force a specific version string in CI or local builds:

```bash
KALISTO_BUILD_VERSION=1.4.3-rc.1+build42 cargo build
```

## Advanced: legacy Lima image build helpers

The standard binary-only path is `kalisto pull`: it prepares a sanitized compressed
Ubuntu raw disk through native VZ without starting Kubernetes. No scripts in this
section are required for that flow.

The optional maintainer scripts below provide a separate two-stage image flow:

1. a Linux-container seed image build using the maintainer scripts / GitLab config
2. a macOS finalization pass that boots the seed under Lima and exports a finalized OS disk

Relevant files:

- [images/macos/lima-node/ubuntu-24.04.yaml](images/macos/lima-node/ubuntu-24.04.yaml)
- [scripts/macos/provision_lima_node.sh](scripts/macos/provision_lima_node.sh)
- [scripts/macos/build_lima_node_image.sh](scripts/macos/build_lima_node_image.sh)
- [scripts/macos/finalize_lima_node_image.sh](scripts/macos/finalize_lima_node_image.sh)
- [scripts/macos/render_lima_artifact_template.sh](scripts/macos/render_lima_artifact_template.sh)
- [/.gitlab-ci.yml](.gitlab-ci.yml)

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

## Advanced: releasing prebuilt Lima images

This optional publishing workflow uses the legacy helpers above, not the managed
native preparation performed by `kalisto pull`.

For a published prebuilt image, the important runtime artifact is the finalized
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

The legacy finalize script requires `qemu-img` to export a compact finalized
`qcow2`. The standard native `kalisto pull` path instead exports compressed raw
and does not require a host QEMU installation.

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
  - [scripts/image/provision_kalisto_node.sh](scripts/image/provision_kalisto_node.sh)
- Linux kernel builder:
  - [scripts/linux/build_firecracker_kernel.sh](scripts/linux/build_firecracker_kernel.sh)
- Linux rootfs builder:
  - [scripts/linux/build_firecracker_rootfs.sh](scripts/linux/build_firecracker_rootfs.sh)
- unified artifact manifest renderer:
  - [scripts/linux/render_firecracker_artifact_manifest.sh](scripts/linux/render_firecracker_artifact_manifest.sh)
- prerequisites:
  - [docs/prerequisites-linux-firecracker.md](docs/prerequisites-linux-firecracker.md)

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
  --template /absolute/path/to/custom-node.yaml \
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
  --template /absolute/path/to/custom-node.yaml \
  --workers 1
```

These legacy k3s probes use a different baseline from prepared `kalisto up`.
Their default configuration disables these bundled components:

- `traefik`
- `servicelb`
- `metrics-server`
- `local-storage`
- embedded network policy

The default probe baseline keeps:

- `containerd`
- `kube-proxy`
- CoreDNS
- Flannel

Prepared product startup instead defaults to no CNI and requires explicit Cilium
selection for networking; the probe defaults above do not describe that workflow.

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

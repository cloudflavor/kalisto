# Linux Firecracker Prerequisites

This guide covers Linux host prerequisites, prepared offline startup, and the
validated Firecracker lifecycle. Kernel/rootfs images are backend-specific:
provide local artifacts or a configured manifest/download source until a public
default artifact source is published.

## Scope

These prerequisites are for:

- Linux hosts
- Firecracker as the VM engine
- `k3s` inside the guest

This is separate from the default macOS Apple-container path and optional Lima path.

Native functional evidence currently covers Linux `x86_64`, not ARM64.
The Linux ARM64 release archive is cross-built and smoke-tested through user-mode
QEMU; that does not validate native ARM64 KVM or Firecracker backend behavior.

## Host Requirements

The Linux host must provide:

- `x86_64` Linux
- hardware virtualization support enabled in firmware and exposed to the host CPU
- KVM available at `/dev/kvm`
- Firecracker installed and runnable from `$PATH`
- `debugfs` available for NoCloud seed injection into the Firecracker rootfs
- `mke2fs` available to build the shared read-only boot-asset disk during `pull`
- OpenSSH `ssh`, `ssh-keygen` and `scp` for guest bootstrap and per-cluster file transfer
- optionally, a running SSH agent (`SSH_AUTH_SOCK`) to forward host credentials into
  guests; when set, Kalisto adds `-o ForwardAgent=yes` to every guest connection, so
  guest-side `git`/`ssh` can authenticate with the host agent's keys
- `ip` available for Linux networking setup
- `iptables` and `sysctl` available; the current host availability check requires
  both even for rootless networking, while bridge mode uses them for host rules
- `slirp4netns` available for the rootless Firecracker network mode (`--network slirp4netns`)
- `unshare` and `nsenter` available for the rootless Firecracker network mode
- `curl` available for host-side API readiness checks
- `nc` available for Unix socket / port checks when needed
- `jq` available for JSON inspection and debugging when needed

The user running Kalisto must be able to:

- open `/dev/kvm`
- create and manage the networking devices needed for the bridge-backed Firecracker network mode
- create user and network namespaces for the rootless Firecracker network mode
- create Unix sockets, runtime files, and logs under the Kalisto workspace/state roots

## Validated Environment

Native validation on 2026-11-16 used:

- OS: Fedora Linux 44
- architecture: `x86_64`
- active Firecracker: `~/.local/bin/firecracker`
- Firecracker version: [`v1.18.0`](https://github.com/firecracker-microvm/firecracker/releases/tag/v1.18.0), checksum-verified 2026-11-16
- KVM present: `/dev/kvm`
- KVM kernel modules loaded
- CPU virtualization extensions visible
- tools present:
  - `/usr/bin/debugfs`
  - `/usr/bin/ip`
  - `/usr/sbin/iptables`
  - `/usr/bin/curl`
  - `/usr/bin/nc`
  - `/usr/bin/jq`

The active binary lives under `~/.local/share/firecracker/v1.18.0/`; its symlink
in `~/.local/bin/` takes precedence over the system RPM. `/usr/bin/firecracker`
remains version 1.13.1; replacing that system-managed installation requires sudo.

## Guest Artifact Requirements

Kalisto needs Firecracker-specific boot artifacts. The repo can now build them locally and in CI, but a stable published source is still being finalized.

At minimum, the Firecracker backend needs:

- a Linux kernel image suitable for Firecracker boot
- a writable raw ext4 root filesystem with NoCloud/cloud-init and OpenSSH guest support;
  not a qcow2 image, a partitioned cloud disk, or a Lima template

These should be treated as backend-specific artifacts even if they are built from the same Ubuntu guest recipe used by Lima.

`kalisto pull --backend firecracker --addons cni/cilium` resolves the configured
kernel/rootfs, installs managed Linux Crane/Helm and matching host kubectl, and
prepares the same digest-pinned K3s/Cilium/diagnostic images used by the other
backends. A shared `firecracker-assets.ext4` disk contains the selected K3s binary
and OCI boot archive. Preparation creates no cluster state or Kubernetes nodes.

`up` attaches that disk read-only to each fresh VM, mounts it at the prepared
asset path, imports images locally, and applies a locally rendered Cilium
manifest with per-cluster TLS secrets. Kernel/rootfs downloads are refused during
startup. Fresh VM creation, image import, all Cilium rollouts, and Kubernetes
readiness remain inside the startup timer; pre-created VMs do not qualify.

Supply local artifacts or the manifest/artifact URLs described below: there is
still no implicit public kernel/rootfs download source. For explicit local files:

```bash
kalisto pull --backend firecracker --addons cni/cilium \
  --firecracker-kernel /path/to/kalisto-vmlinux --firecracker-rootfs /path/to/rootfs.raw
kalisto up --backend firecracker --workers 2 --addons cni/cilium \
  --firecracker-kernel /path/to/kalisto-vmlinux --firecracker-rootfs /path/to/rootfs.raw
```

No CNI is selected implicitly. Profiles support no CNI or explicit Cilium and
optional ServiceLB; other startup add-ons must be installed explicitly later.
Stopped-node disks and SSH identities are preserved during `down` / `up`.

An existing Helm-managed Cilium cluster is not converted in place. Prepared
startup rejects its legacy manifest rather than mixing installation methods.
Use a fresh cluster name; back up valuable workloads/data before explicitly
destroying an old cluster.

The intended artifact split is:

- Apple containers (macOS default):
  - Apple Linux kernel/init image
  - Kalisto-managed Ubuntu/k3s OCI node image

- Lima:
  - Lima template YAML
  - Lima-compatible disk artifact
- Firecracker:
  - Firecracker kernel image
  - Firecracker rootfs disk image

Reusing the same Ubuntu base is desirable. Reusing the Lima template artifact directly is not.

## Backend Contract

The Firecracker implementation provides:

- backend auto-selection based on host OS
  - macOS -> Apple containers (Lima explicitly optional)
  - Linux -> Firecracker
- persisted backend identity in cluster state
- Firecracker runtime configuration surface
- Firecracker boot artifact discovery or explicit configuration
- guest command execution path for bootstrap and readiness checks
- cleanup and status reporting for Firecracker microVMs

## Required Config Inputs

Kalisto now supports both explicit artifact paths and managed artifact resolution.

Supported explicit inputs:

- Firecracker kernel image
- Firecracker rootfs image

Supported managed inputs:

- local workspace artifacts under `artifacts/firecracker-node/`
- versioned managed cache under the Kalisto data root:
  `~/.local/share/kalisto/artifacts/firecracker-node/<major.minor>/<arch>/`
- published manifest and artifact URLs

The Linux resolver now also detects the installed Firecracker version and host
architecture, and uses that to prefer a versioned cache/search path. For
example, a host running Firecracker `v1.18.0` on `x86_64` will first look under:

- `artifacts/firecracker-node/v1.18/x86_64/`
- `~/.local/share/kalisto/artifacts/firecracker-node/v1.18/x86_64/`

During `pull`, missing managed artifacts can be fetched from a configured
manifest or explicit URLs and cached locally. During `up`, remote manifests must
already be cached and missing kernel/rootfs artifacts fail before VM boot.
An explicit local rootfs still must be a writable-compatible filesystem image.

The Linux helper scripts now follow the same layout. For a host on Firecracker
`v1.18.x`, the default local outputs are:

- `artifacts/firecracker-node/v1.18/x86_64/kalisto-vmlinux`
- `artifacts/firecracker-node/v1.18/x86_64/kalisto-vmlinux.config`
- `artifacts/firecracker-node/v1.18/x86_64/kalisto-vmlinux.manifest`
- `artifacts/firecracker-node/v1.18/x86_64/ubuntu-24.04-minimal-k3s-amd64-rootfs.raw`
- `artifacts/firecracker-node/v1.18/x86_64/firecracker-artifacts.manifest`

If no usable artifact source is configured or discoverable, the Firecracker backend should fail clearly before attempting boot.

## Networking Expectations

The Linux Firecracker path should assume Linux-native networking, not Lima-style networking.

That means:

- no Lima network names such as `lima:user-v2`
- no Lima template-driven port forwarding
- Firecracker-specific network setup must be handled by the Linux backend

Kalisto currently has two Linux Firecracker networking modes:

- `bridge`:
  - creates host bridge/TAP devices
  - requires host-level networking privileges
  - depends on `ip`, `iptables`, and `sysctl`
- `slirp4netns`:
  - runs Firecracker inside an unprivileged user+net namespace
  - uses `slirp4netns` for egress and host port forwards
  - depends on `slirp4netns`, `unshare`, and `nsenter`

`slirp4netns` is now the default Firecracker mode on Linux.

Use `--network bridge` only when you explicitly want host bridge/TAP networking
and have the required host privileges (sudo).

## Live Functional Boot Test

The opt-in Linux test installs a copy of the real CLI into a private `install/bin`
prefix and runs it from an empty working directory, without executing the checkout
binary. Each run has dedicated XDG data/config roots and fresh unique cluster
state. `pull` is outside the clock. The strict 60-second deadline includes three
fresh VMs, three Ready nodes, authenticated host API, every rendered Cilium rollout,
and DNS plus cross-node Service HTTP probes from both workers using cached images
with pull policy `Never`. Afterward it checks stop/resume ConfigMap persistence,
scale-out to four Ready nodes with a multi-endpoint Service already present, then
scale-down to three Ready nodes. Shrinking must remove worker index 2, retain the
original control-plane/worker node UIDs, and preserve DNS/Service HTTP from both
remaining workers. These maintenance checks are outside the startup clock.
Evidence is retained under `target/kalisto/firecracker-budget/`; only the
test-owned cluster is destroyed.

Native validation on the environment above measured:

| Stage | Elapsed |
| --- | --- |
| Preparation, outside the startup clock | 40.68s |
| Fresh startup and all functional probes | **42.95s** |

Stop/resume preserved the ConfigMap, and scale-out reached four Ready nodes
afterward. These are observed timings, not a universal hardware guarantee.

These recorded results predate the newly added scale-down assertions; passing
those assertions still requires a new native CI run. Package compilation or
user-mode emulation must not be substituted for that result.

```bash
KALISTO_LIVE_FIRECRACKER_TESTS=1 \
KALISTO_FIRECRACKER_KERNEL=/path/to/kalisto-vmlinux \
KALISTO_FIRECRACKER_ROOTFS=/path/to/rootfs.raw \
  cargo test --release --locked --test linux_firecracker_live -- --nocapture
```

Run this command only on the configured native CI host. Dispatch
`.forgejo/workflows/ci.yml` with `firecracker_live=true` for the dedicated
`firecracker-live` job on the existing `tomis-host` runner. It does not depend on
showcase recording and never changes the Firecracker service. Kernel compilation
is opt-in via the separate `prepare_firecracker_kernel=true` dispatch input.
The job uses configured `KALISTO_FIRECRACKER_KERNEL` /
`KALISTO_FIRECRACKER_ROOTFS`, or discovers the existing versioned host artifact
cache for a manual run. Missing prerequisites fail rather than skip the live test.

Prepare a compatible kernel ahead of time using the existing
[`build_firecracker_kernel.sh`](../scripts/linux/build_firecracker_kernel.sh)
artifact procedure, or explicitly request the source-pinned CI preparation once.
CI preparation creates a new owned kernel directory, never replaces user
artifacts, and retains the verified kernel for subsequent runs without rebuilding.
The native job requires `kalisto-vmlinux.config` and `kalisto-vmlinux.manifest`
beside the image, matching `KERNEL_SHA256` / `KERNEL_CONFIG_SHA256` values,
and built-in `CONFIG_NETFILTER_XT_MATCH_STATISTIC=y` for multi-endpoint
kube-proxy Services. A checksum-valid older kernel without this option is not
sufficient. Provide a prepared raw ext4 rootfs separately; the CI job does not
create a rootfs or publish guest artifacts.

To enable this job on every main push after a successful manual run, configure
the repository's `KALISTO_FIRECRACKER_KERNEL` Actions variable with that verified
prepared kernel. Until configured, regular main CI skips the native Linux job;
the historical functional observation above remains evidence, not a continuously
enforced native gate. See [release CI configuration](releases.md#ci-configuration)
for storage, runner variables, installed-archive smoke checks and the unverified
Intel macOS / Linux ARM64 boundaries.

## Operational Notes

Each VM owns a writable rootfs and SSH identity. Nodes in a prepared profile
share only the read-only boot-asset disk. Per-cluster manifests are transferred
over SSH/SCP with mode `0600`; Kubernetes credentials and generated TLS keys do
not enter the shared cache.

`down` stops the VMs while retaining their disks and identities. `up` resumes
that persisted profile. `destroy` removes the named cluster but leaves the
prepared boot cache available for future fresh clusters.

## Inspection Script

The repository includes a helper script to inspect a Linux host for Firecracker prerequisites:

- [scripts/linux/inspect_firecracker_env.sh](../scripts/linux/inspect_firecracker_env.sh)

Run it and capture the output when validating a Linux machine for Firecracker support.

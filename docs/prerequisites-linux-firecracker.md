# Linux Firecracker Prerequisites

This document records the current prerequisites for running Kalisto on Linux with a Firecracker backend.

It is based on:

- the current Kalisto codebase shape
- the verified local host details captured in `details.log`

The repository now includes a first-pass Firecracker artifact pipeline for:

- a Kalisto-owned Firecracker kernel build
- rootfs generation from the Ubuntu cloud image
- a combined artifact manifest for managed fetches

The final publication location for those artifacts is still in progress.

## Scope

These prerequisites are for:

- Linux hosts
- Firecracker as the VM engine
- `k3s` inside the guest

This is separate from the existing macOS + Lima path.

## Host Requirements

The Linux host must provide:

- `x86_64` Linux
- hardware virtualization support enabled in firmware and exposed to the host CPU
- KVM available at `/dev/kvm`
- Firecracker installed and runnable from `$PATH`
- `ip` available for Linux networking setup
- `curl` available for host-side API readiness checks
- `nc` available for Unix socket / port checks when needed
- `jq` available for JSON inspection and debugging when needed

The user running Kalisto must be able to:

- open `/dev/kvm`
- create and manage the networking devices needed for Firecracker
- create Unix sockets, runtime files, and logs under the Kalisto workspace/state roots

## Verified On The Current Host

The current host details already confirm these points:

- OS: Fedora Linux 43
- architecture: `x86_64`
- Firecracker: `/usr/bin/firecracker`
- Firecracker version: `v1.13.1`
- KVM present: `/dev/kvm`
- KVM kernel modules loaded
- CPU virtualization extensions visible
- tools present:
  - `/usr/bin/ip`
  - `/usr/bin/curl`
  - `/usr/bin/nc`
  - `/usr/bin/jq`

## Guest Artifact Requirements

Kalisto needs Firecracker-specific boot artifacts. The repo can now build them locally and in CI, but a stable published source is still being finalized.

At minimum, the Firecracker backend needs:

- a Linux kernel image suitable for Firecracker boot
- a writable root filesystem image for the guest

These should be treated as backend-specific artifacts even if they are built from the same Ubuntu guest recipe used by Lima.

The intended artifact split is:

- Lima:
  - Lima template YAML
  - Lima-compatible disk artifact
- Firecracker:
  - Firecracker kernel image
  - Firecracker rootfs disk image

Reusing the same Ubuntu base is desirable. Reusing the Lima template artifact directly is not.

## Project Requirements

For the Firecracker engine to work in Kalisto, the project must provide:

- backend auto-selection based on host OS
  - macOS -> Lima
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
example, a host running Firecracker `v1.13.1` on `x86_64` will first look under:

- `artifacts/firecracker-node/v1.13/x86_64/`
- `~/.local/share/kalisto/artifacts/firecracker-node/v1.13/x86_64/`

If no managed artifacts are found locally, Kalisto can fetch them from an
explicitly configured Kalisto manifest or explicit artifact URLs and cache them
locally. The rootfs still needs to be a writable Kalisto-managed disk artifact
or an explicit local path.

The Linux helper scripts now follow the same layout. For a host on Firecracker
`v1.13.x`, the default local outputs are:

- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux`
- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux.config`
- `artifacts/firecracker-node/v1.13/x86_64/kalisto-vmlinux.manifest`
- `artifacts/firecracker-node/v1.13/x86_64/ubuntu-24.04-minimal-k3s-amd64-rootfs.raw`
- `artifacts/firecracker-node/v1.13/x86_64/firecracker-artifacts.manifest`

If no usable artifact source is configured or discoverable, the Firecracker backend should fail clearly before attempting boot.

## Networking Expectations

The Linux Firecracker path should assume Linux-native networking, not Lima-style networking.

That means:

- no Lima network names such as `lima:user-v2`
- no Lima template-driven port forwarding
- Firecracker-specific network setup must be handled by the Linux backend

The exact networking implementation is still to be defined, but the host must support the devices and privileges that implementation requires.

## Operational Notes

The existing `k3s` bootstrap flow assumes:

- a writable artifact directory shared with the orchestration layer
- guest command execution for bootstrap and readiness checks
- host-side access to the Kubernetes API endpoint

The Firecracker backend must satisfy those same product-level requirements with Firecracker-specific mechanics.

## Inspection Script

The repository includes a helper script to inspect a Linux host for Firecracker prerequisites:

- [/home/runner/projects/rust/kalisto/scripts/linux/inspect_firecracker_env.sh](/home/runner/projects/rust/kalisto/scripts/linux/inspect_firecracker_env.sh)

Run it and capture the output when validating a Linux machine for Firecracker support.

# Kalisto Implementation Tracker

This document is the live implementation tracker for the project. Update it as work lands so the current product shape, completed work, and next steps stay explicit.

## Current Goal

Turn the macOS Lima validation harness into a usable local-development CLI with:

- `kalisto up`
- `kalisto down`
- `kalisto status`
- `kalisto kubeconfig`

The current supported runtime is:

- macOS backend: Lima
- Linux backend: Firecracker
- Kubernetes distro: `k3s`
- Image pipeline: shared Ubuntu guest provisioning with backend-specific artifacts

## Completed

### Core runtime

- Lima-only macOS scope
- extracted Lima `k3s` runtime modules under `src/lima_k3s/`
- `structopt`-based CLI parsing
- `tracing` + `tracing_subscriber` lifecycle logging with default `info` stage output
- first-pass product commands:
  - `kalisto up`
  - `kalisto down`
  - `kalisto ls`
  - `kalisto remove`
  - `kalisto status`
  - `kalisto kubeconfig`
  - `kalisto bench`
- global cluster state now lives under `~/.local/share/kalisto/clusters/<name>/`
- default Kalisto config root reserved under `~/.config/kalisto`
- live Lima substrate probe
- live Lima `k3s` control-plane and worker-join probe

### Image pipeline

- GitLab / `opal` pipeline for:
  - `fmt`
  - `clippy`
  - `test`
  - release binary build
  - Lima seed image build
- Firecracker rootfs build job
- Firecracker kernel build job
- Firecracker combined artifact publish job
- Ubuntu 24.04 minimal ARM seed image build
- Ubuntu 24.04 minimal AMD64 Firecracker rootfs build
- Kalisto-owned AMD64 Firecracker kernel build for `k3s`
- cached `k3s` asset reuse
- macOS finalization pass for warmed Lima image artifacts
- shared guest provisioning script for Lima and Firecracker image builds
- repo-scoped `opal` runtime config for larger Apple `container` job VMs

### Benchmarks observed so far

- finalized image control-plane host API ready: about `39.7s`
- finalized image `1cp + 1worker` cluster ready: about `74.1s`

## In Progress

### CLI productization

- separate backend lifecycle from bootstrap orchestration
- move the current `up` path off probe-oriented helpers where it still reuses them indirectly
  - worker startup and shared runtime helpers now use the real Lima backend module
- cluster product lifecycle now routes through an explicit backend interface
- backend identity is now persisted in cluster spec/state
- product bootstrap and wait helpers now live under `src/cluster/` instead of `src/lima_k3s/`
- documented Linux Firecracker host and artifact prerequisites under `docs/`
- host-default backend selection now maps Linux to Firecracker and macOS to Lima
- Firecracker kernel/rootfs inputs now exist in CLI/spec/state plumbing
- Firecracker artifact manager now resolves kernel/rootfs from explicit paths, local workspace artifacts, a managed local cache, or configured fetch URLs
- Firecracker artifact manager now understands a published manifest that can supply filenames, checksums, URLs, and the managed `k3s` version
- Firecracker kernel resolution no longer falls back to the upstream Firecracker CI bucket; the managed kernel is now expected to come from Kalisto-built artifacts or an explicit manifest/URL
- Firecracker backend now writes NoCloud seed artifacts, launches a microVM process, exposes guest SSH exec, tracks pid-based status, and cleans up runtime networking
- Firecracker control-plane bootstrap now runs through the shared cluster bootstrap path with backend-derived guest artifact roots
- control-plane artifact persistence now works across both mounted-artifact and SSH copy-out backends
- control-plane API endpoint and TLS SAN handling now derive from the selected backend instead of assuming Lima localhost forwarding
- Firecracker runtime networking now uses a shared Linux bridge so the host and workers can reach the control plane
- live Firecracker cluster bring-up now reaches VM boot, cloud-init, SSH, k3s startup, and control-plane artifact extraction on a real host
- the current live Firecracker blocker is kernel support for the `k3s` networking requirements, which is why the managed kernel pipeline is being moved to a Kalisto-built artifact
- make lifecycle progress visible during `up/down/bench` so the CLI is not silent while Lima boots

## Next

1. Build and publish the Kalisto-owned Firecracker kernel artifact, then retry live Firecracker bring-up with that kernel

2. Point the artifact manager at a real published Firecracker manifest URL by default once the publication location is finalized

3. Make `up` use the finalized image path by default in real Lima runs and benchmark it again

4. Add mount/workspace fields to the cluster spec and state model

5. Record repeated real `kalisto bench` samples against the finalized image path

6. Reduce duplication between probe-oriented Lima helpers and product backend helpers

7. Move the Lima probe path onto the shared backend-neutral cluster bootstrap/wait surface where it fits cleanly

## Follow-up

- plugin/add-on install surface after cluster boot
- workspace mount and dev workflow integration

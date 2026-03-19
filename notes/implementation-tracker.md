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
- Kubernetes distro: `k3s`
- Image pipeline: container-built seed image plus macOS finalization pass

## Completed

### Core runtime

- Lima-only macOS scope
- extracted Lima `k3s` runtime modules under `src/lima_k3s/`
- `structopt`-based CLI parsing
- first-pass product commands:
  - `kalisto up`
  - `kalisto down`
  - `kalisto status`
  - `kalisto kubeconfig`
- persisted cluster state under `.kalisto/clusters/<name>/`
- live Lima substrate probe
- live Lima `k3s` control-plane and worker-join probe

### Image pipeline

- GitLab / `opal` pipeline for:
  - `fmt`
  - `clippy`
  - `test`
  - release binary build
  - Lima seed image build
- Ubuntu 24.04 minimal ARM seed image build
- cached `k3s` asset reuse
- macOS finalization pass for warmed Lima image artifacts
- repo-scoped `opal` runtime config for larger Apple `container` job VMs

### Benchmarks observed so far

- finalized image control-plane host API ready: about `39.7s`
- finalized image `1cp + 1worker` cluster ready: about `74.1s`

## In Progress

### CLI productization

- separate backend lifecycle from bootstrap orchestration
- move the current `up` path off probe-oriented helpers where it still reuses them indirectly

## Next

1. Add Lima backend lifecycle module
   - create/start/stop/delete instances
   - keep `limactl` details out of command handlers

2. Make `up` use the finalized image path by default in real runs and benchmark it again

3. Add mount/workspace fields to the cluster spec and state model

4. Add benchmark command surface for repeated timing runs

5. Reduce duplication between probe-oriented Lima helpers and product backend helpers

## Follow-up

- plugin/add-on install surface after cluster boot
- workspace mount and dev workflow integration
- Linux backend after the macOS CLI path is stable

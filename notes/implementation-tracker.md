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
  - worker startup and shared runtime helpers now use the real Lima backend module
- make lifecycle progress visible during `up/down/bench` so the CLI is not silent while Lima boots

## Next

1. Make `up` use the finalized image path by default in real runs and benchmark it again

2. Add mount/workspace fields to the cluster spec and state model

3. Record repeated real `kalisto bench` samples against the finalized image path

4. Reduce duplication between probe-oriented Lima helpers and product backend helpers

## Follow-up

- plugin/add-on install surface after cluster boot
- workspace mount and dev workflow integration
- Linux backend after the macOS CLI path is stable

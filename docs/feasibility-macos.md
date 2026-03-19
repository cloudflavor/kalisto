# macOS Feasibility

This file records tested behavior for the Lima-based macOS backend only.

## Confirmed

- Lima `vz` instances work as the macOS machine substrate for Kalisto.
- Writable workspace mounts work for the current probe flow.
- Rosetta/binfmt is visible inside the guest when enabled.
- The Lima substrate probe passes on this host.
- The Lima `k3s` control-plane probe passes on this host.
- A Lima multi-node probe with one control plane and one worker passes on this host.
- The worker reaches `Ready` and the control-plane kubeconfig is written to the mounted workspace.

## Image Pipeline

- Kalisto uses a Linux-container seed-image build and a macOS finalization pass.
- The finalized Lima image is sparse:
  - apparent size: `12 GiB`
  - actual disk usage on this host: about `1.0 GiB`

## Benchmarks

Measured on this host:

- seed image, `1 control plane + 1 worker`: `95.201s`
- finalized image, `1 control plane + 1 worker`: `74.147s`
- finalized image, control-plane host API ready: `39.735s`

## Current Baseline

The default server bootstrap disables these bundled add-ons so they can become post-boot plugin boundaries:

- `traefik`
- `servicelb`
- `metrics-server`
- `local-storage`
- `coredns`
- embedded network policy

The baseline still keeps:

- `containerd`
- `kube-proxy`
- Flannel

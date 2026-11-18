# Kalisto implementation tracker

This tracker describes the product and its evidence boundaries, not the original
Lima probe prototype. Keep it aligned with the CLI, native tests and release
artifacts when behavior changes.

## Current product

- Standalone local Kubernetes CLI using K3s.
- macOS default: Apple containers on Apple silicon and macOS 26+, reusing a
  compatible installed/running Apple `container` service.
- Optional macOS backend: Lima with native VZ.
- Linux default: Firecracker with rootless `slirp4netns`; privileged bridge
  networking is an explicit alternative.
- `kalisto pull` prepares versioned images, managed tools and matching startup
  assets. `kalisto up` consumes that cache without startup downloads.
- CNI is explicit: no CNI by default, or selected `cni/cilium`. Do not substitute
  Flannel to meet a startup budget.
- Cluster state/configuration use the user's data/configuration directories;
  installed execution does not require a source checkout.

## Delivered

### Lifecycle and development workflows

- Backend-neutral cluster orchestration and persisted backend/profile identity.
- Create, start/resume, stop, inspect, list and destroy named clusters.
- Managed kubeconfig/kubectl, workload application, add-ons, logs, benchmarks,
  CLI progress and MCP/AI-agent integration.
- Scale-out joins additional workers and checks their current-boot readiness.
- Scale-down drains highest-index workers using Kubernetes eviction, protects
  Node identity with UID checks, and persists each completed removal. Interrupted
  removal can recover through `up` and a repeated `scale` command without
  recreating deleted workers; removed VM disks/local data are not retained.
- Stop/resume preserves cluster disks and state; readiness verifies current boot
  identity, node address and authenticated API-to-kubelet connectivity.
- Lima cold export/import with native age encryption; see
  [archive limitations](../docs/cluster-archives.md).

### Images and distribution

- Prepared Apple-container, Lima and Firecracker artifact paths.
- Linux kernel/rootfs builders and managed explicit-path/cache/manifest
  resolution. Firecracker startup never downloads missing guest artifacts.
- Shared read-only Firecracker boot-asset disk; per-cluster writable disks,
  SSH identities and private manifests.
- Rootless Firecracker guest addresses are reserved under a per-cluster file
  lock, persisted across stop/resume and released only with node removal.
- Three-target release packaging (macOS ARM64, Linux ARM64 and Linux x86_64),
  checksums/provenance and draft-first GitHub Releases publication from reviewed
  main tags, without website staging or Bunny artifact uploads;
  see [release tooling](../docs/releases.md).
- macOS build outputs/caches/large temporaries remain on mounted external
  storage. Borrowed runtimes and unrelated clusters are not cleanup targets.

### Public showcase and entry points

- [Documentation](https://cloudflavor.io/kalisto/docs/getting-started/) and
  [downloads](https://cloudflavor.io/kalisto/#download) are reachable.
- Eight real CLI recordings are hosted on asciinema.org: startup, HTTP
  deployment, state-preserving resume and scale-out on each native backend.
  [Recording URLs and provenance](../docs/demos.md) identify their source
  revisions; website deployment must not contain the casts.
- Downloads link to GitHub Releases without a pinned current version or a
  per-release website update. Historical recording revisions remain immutable
  evidence, not a claim about the latest release or current main.

## Functional evidence

The acceptance scenario includes fresh three-node creation, authenticated host
API, every selected Cilium rollout, DNS from both workers and cross-node Service
HTTP, all within sixty seconds after preparation.

| Backend | Observed functional result | Evidence boundary |
| --- | --- | --- |
| Apple containers | 38.11s | Native run 4478, source `015c15f`; not a measurement of every later revision. |
| Firecracker | 42.95s; preparation separately 40.68s | Fedora 44 x86_64, Firecracker 1.18.0; persistence and scale-out verified outside the startup clock. |
| Firecracker CI | Full functional deadline, persistence, scale-out and scale-down passed | Native run 5028 / UI 242, source `3eb6dcb`; verified owned kernel now configured for ongoing main validation. |
| Lima | 56.07s, 53.84s, 54.26s, 54.83s | Historical native VZ development-Mac runs, not Apple-container results. |

The source acceptance tests are `tests/macos_three_node_boot_budget.rs` and
`tests/linux_firecracker_live.rs`. Keep same-revision CI evidence with candidate
packages. Lifecycle timings or cross-compilation alone do not establish full
functional readiness. See the
[published performance boundaries](https://cloudflavor.io/kalisto/docs/performance/).

## Remaining work and explicit limits

- Native scale-down acceptance must pass on the candidate source; host-safe
  regression coverage alone is not a native lifecycle measurement.
- A stable version still requires a reviewed version/tag and successful release
  publication. Do not relabel a CI snapshot as stable.
- Firecracker still requires explicit compatible kernel/rootfs artifacts or
  configured download sources; there is no public default artifact source.
- Linux native lifecycle evidence is x86_64-specific. An ARM64 Linux package is
  not proof of a native ARM64 Firecracker cluster.
- macOS binaries are unsigned and not notarized.
- Lima archive import has no completed native restored-workload acceptance
  evidence or sub-minute guarantee; archives are not supported on every backend.

## Verification policy

Use targeted CI checks first and authorized native tests for real lifecycle
proof. No local builds, tests, formatters or benchmarks without explicit local
authorization. Recordings are actual terminal sessions, never fabricated output;
review privacy and successful commands before uploading to the user's linked
asciinema.org account.

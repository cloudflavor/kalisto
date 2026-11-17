# Genuine Kalisto use-case recordings

These eight recordings execute the actual Kalisto CLI: four scenes on native
Apple-silicon macOS with Apple container, and four on native x86_64 Linux with
Firecracker. They are scripted real sessions, not synthesized terminal output.
Printed `$` prompts show the executed arguments. Interactive progress graphics
remain enabled in the recorded PTY; `CI`, explicit logging overrides and
`NO_COLOR` are not passed to scenes. Preparation, source compilation, ownership
checks and cleanup are outside the recordings; genuine read-only assertions stay
on the clock. These use cases do not replace either backend's cold-boot,
functional Cilium/DNS/cross-node HTTP acceptance test or make a sixty-second claim.

## Scenarios

| Recording | Actual behavior checked |
| --- | --- |
| `start.cast` | Start a fresh prepared Apple-container or Firecracker cluster with two workers and explicit `cni/cilium`; query authenticated `/readyz`, require exactly three Ready nodes, and wait for every rollout listed in the real preparation manifest. |
| `deploy.cast` | `kalisto apply --manifest app.yaml`; wait for two HTTP replicas on distinct workers, inspect pods and Service with managed `kalisto kubectl`, and require the exact real HTTP response through an owned loopback-only port-forward. |
| `resume.cast` | Create `demo-state` with `value=saved-before-stop`, stop only the owned cluster with `kalisto down`, inspect that all three nodes are actually Stopped, resume with the same profile, and verify the ConfigMap's original UID and data plus the surviving HTTP workload and response. |
| `scale.cast` | Show the original three Ready nodes, run `kalisto scale --workers 3`, require exactly four Ready nodes, increase the HTTP Deployment to three replicas, require distinct worker placements, and repeat the HTTP proof. |

On Apple-container resume, guest addresses can change while Kubernetes still
reports the previous boot's Ready condition. Kalisto passes each freshly
inspected guest IP to K3s and requires the current boot ID, current InternalIP,
Ready status and a working authenticated API-to-kubelet health request.
K3s completes its own serving-certificate issuance; Kalisto does not delete
certificates or rotate the cluster CA, node credentials or keys. The resume
scene's real port-forward verifies the surviving workload immediately afterward.

The HTTP fixture is [`scripts/demos/app.yaml`](../scripts/demos/app.yaml): a small
BusyBox HTTP server, a content ConfigMap, and a Service. Required pod anti-affinity
and control-plane exclusion make worker placement observable rather than assumed.
`kalisto pull` already prepares BusyBox 1.36.1 as its offline probe image. The
recorder reads the **observed digest** from successful preparation output, checks
it against the preparation manifest, and pins the emitted `app.yaml` to that
exact image with `imagePullPolicy: Never`. It does not introduce a registry
credential or download an application image during the scenes.

The public response is:

```text
Hello from Kalisto. This workload survives a cluster restart.
```

## Dispatch and artifacts

Dispatch **Record Kalisto showcase** (`.forgejo/workflows/showcase.yml`) for Apple
container and **Record Kalisto Firecracker showcase**
(`.forgejo/workflows/showcase-linux.yml`) for Linux, selecting the same reviewed
source branch/revision. Both have only a `workflow_dispatch` trigger: pushes,
pull requests and releases do not start captures.
Dispatch is an explicit authorization to compile and run owned infrastructure in
native CI; do not run the recorder locally. No new keys are required.

Apple-container repository variables:

- `KALISTO_MACOS_RUNNER`: the native macOS arm64 runner label.
- `KALISTO_MACOS_VOLUME`: a mounted external volume, not `/`.
- `KALISTO_MACOS_BUILD_ROOT`: an absolute directory strictly beneath that volume.

The macOS runner must already have its Rust toolchain and Python 3.9+, curl,
Homebrew `container` 1.7.1, its compatible **running** release daemon, and installed
asciinema **3.2.1**. The macOS workflow does not install or upgrade either tool,
restart the service, change its roots/properties/configuration, or stop it.
An unavailable, stopped, incompatible, or differently owned daemon fails capture.
The recorder checks the Homebrew installation, launchd API PID/start count, service
programs/roots, properties and configuration digest before and after preparation,
each scene, and destruction. It also requires Kalisto's selection to be borrowed
(`managed: false`) and verifies that private installer/install-directory metadata
is unchanged. Cached kernels and init images are preparation assets, not a private engine.

Builds, the fresh source checkout, node state, images, managed tools, configuration,
recordings, logs, and temporary files stay on the configured external storage.
Existing native CI `cargo-home`, `release-rustup`, and `sccache` conventions are
reused. The persistent compilation directory is `showcase-target`; the freshly
built binary is copied into the owned job workspace so another job cannot replace
the recorded executable. Cluster state and private configuration are isolated per
job; prepared assets reuse the existing `apple-container-ci/artifacts` cache using
Kalisto's normal profile locking. Cleanup never removes that shared cache.
Captures serialize without cancelling other jobs or taking ownership of their clusters.

The Linux workflow runs on `tomis-host`, with KVM access, unprivileged user/network
namespaces and the [Firecracker prerequisites](prerequisites-linux-firecracker.md).
It prefers the existing user-installed Firecracker 1.18.0. Optional
`KALISTO_LINUX_BUILD_ROOT` selects private showcase storage; otherwise it uses
`~/.cache/kalisto-showcase-firecracker`. Optional `KALISTO_FIRECRACKER_ROOTFS`
provides an explicit prepared root filesystem; otherwise the workflow discovers
an existing user-cache artifact, never a live node disk. Before recording, CI
builds Linux 6.19.9 from checksum-pinned upstream source with the repository's
kernel fragment into its owned workspace. Existing user kernels are untouched.
`CONFIG_NETFILTER_XT_MATCH_STATISTIC=y` is mandatory: kube-proxy needs this match
for multi-endpoint Service rules, including the Kubernetes Service used by a
new worker's Cilium initialization. CI runs the native cold-boot and scale
acceptance test against that kernel before capturing the four scenes.
Asciinema 3.2.1 is installed into the owned CI workspace, without replacing
system or user tools. The recorder fingerprints pre-existing Firecracker
processes and checks that they survive unchanged. Cleanup destroys only its
marker-owned cluster and retains shared prepared assets.
Firecracker VMs, the namespace anchor and slirp helper run in independent
sessions with null stdin, so closing a recording or ordinary terminal does not
stop the cluster. `down` and `destroy` retain explicit lifecycle ownership.

`KALISTO_LINUX_CA_BUNDLE` optionally supplies an existing runner CA bundle for
authenticated source checkout when the forge uses a private certificate chain.

Each workflow checks out `GITHUB_SHA`, builds
`cargo build --locked --release --bin kalisto`, and invokes
`python3 scripts/demos/capture.py` from its owned checkout. The orchestrator
requires an actual dispatch environment. Scene commands use an isolated data
root and `DEMO_PROFILE_JSON`, including explicit backend and Firecracker artifact
arguments where applicable.

Each successful backend run uploads a seven-day review artifact:
`kalisto-showcase-<full-source-commit>` for Apple container and
`kalisto-showcase-firecracker-<full-source-commit>` for Firecracker. Each contains:

```text
start.cast
deploy.cast
resume.cast
scale.cast
app.yaml
provenance.json
```

Every command must exit successfully, each scene must finish all real assertions,
the owned cluster must be destroyed, the borrowed daemon must remain unchanged,
and every file must pass the publication gate. asciinema uses `--return`, and
scene/subprocess failure exit codes are preserved. Partial casts never count as
successful demos. No asciinema upload/auth/stream command, release publication, or
website deployment is performed.

Review the downloaded candidates, then upload them from the user's linked
asciinema.org account. Do not use anonymous uploads or put `.cast` files in the
website deployment. The website stores recording URLs, observed durations,
checksums, source revisions and architecture-correct workload fixtures, and
uses the official asciinema.org embed so player and recording assets are served
by asciinema. A CI artifact is a review candidate, not automatic publication. A failed
recording or privacy check does not upload raw casts. Failure evidence remains in
the mode-0700 owned `kalisto-showcase.*` workspace under the configured build root,
particularly `private/`; locate the matching `.showcase-owner` run ID on the runner
without copying its machine paths or raw output into public logs. Raw evidence is
never an artifact upload input. After private diagnosis, remove only that owned
workspace once its cleanup succeeds; retention of failed workspaces is intentional
and requires operator disk housekeeping.

Scale failures save a mode-0600 `scale-failure.json` under cluster state before
rolling back new workers. It contains only those workers' node conditions,
events, Cilium pod diagnostics and bounded K3s logs, without serialized command
arguments. The recorder copies it into `private/failure-scale-failure.json`
before destroying its owned cluster. This snapshot is never a public artifact.

## Source-level reproduction

The workflow is the supported recording/reproduction entrypoint. Its scripts are:

- `capture.py`: dispatch/storage ownership, compilation, daemon fingerprinting,
  collision checks, actual `pull`, scene capture, cleanup, and publication metadata.
- `scene.py`: real scene commands and assertions; copied with the emitted manifest
  into the isolated empty demo working directory before asciinema starts it.
- `app.yaml`: the public fixture source; the artifact carries its actual prepared
  digest for reproduction independent of a moving image tag.

For understanding the CLI sequence (not an instruction to execute locally), each
start/resume uses the same concrete profile, with an owned name and available
loopback API port supplied by the orchestrator:

```sh
kalisto pull --name "$DEMO_CLUSTER" --backend apple-container --workers 2 --addons cni/cilium --api-port "$DEMO_API_PORT"
kalisto up --name "$DEMO_CLUSTER" --backend apple-container --workers 2 --addons cni/cilium --api-port "$DEMO_API_PORT"
kalisto kubectl --name "$DEMO_CLUSTER" -- get nodes
kalisto apply --name "$DEMO_CLUSTER" --manifest app.yaml
kalisto down --name "$DEMO_CLUSTER"
# Resume: run the same up command, not create/destroy.
kalisto scale --name "$DEMO_CLUSTER" --workers 3
kalisto destroy --name "$DEMO_CLUSTER"
```

`kalisto apply` uses the actual control-plane workload path; `kalisto kubectl`
uses the matching managed client and private cluster kubeconfig without printing
that file. Port-forwards run in a dedicated process group bound to `127.0.0.1`,
with private logs, and are stopped after the actual curl proof. A PID/start-time
ownership sidecar permits cleanup of a forward left by an interrupted scene.
The port-forward prompt is visible; its asynchronous diagnostics and readiness
polls are not recorded. Extra JSON inspections are genuine hidden assertions,
not substitutes for displayed CLI output.

A fresh random `showcase-` cluster name is checked against saved state and the
backend's successful runtime snapshot before mutation. Cleanup uses only its
saved owned name, never `destroy --all`, arbitrary discovery, engine shutdown or
a host-wide reset. The workflow's `always()` cleanup also handles normal failures or
interruption; a hard runner/power loss may require private operator cleanup. Both
capture and cleanup validate the owned workspace/run marker. Failed cleanup retains
the workspace instead of deleting ownership information or concealing the failure.

## Timing, privacy, and provenance

Asciinema records **asciicast v2**, output only, at **110×30**, with no idle cap,
concatenation, speed-up, trimming or fabricated output. Its configuration is
isolated from personal settings. The official hosted embed uses speed 1 and an
idle-time limit greater than the entire recorded duration, preserving every wait.
All output events remain in order at their original numeric timestamps.
`duration_seconds` is the last output event timestamp, not an independent
stopwatch claim.
The recorder's final successful exit metadata (`x`, `0`) is validated and omitted
from the output-only publication; unsuccessful exit metadata rejects publication.

The only permitted terminal-output substitutions are exact known private prefixes:

- This job's absolute workspace prefix → `<demo-workspace>`.
- The configured absolute external build-root prefix → `<build-storage>`.
- The actual runner HOME prefix → `<runner-home>`.
- Explicit Firecracker kernel/rootfs filenames → `<firecracker-kernel>` / `<firecracker-rootfs>`.

Longer prefixes match first. A replacement may span PTY chunks: the neutral label
is emitted at the first matching event and the remaining matched characters are
removed from subsequent events, preserving **all** events and their timestamps,
including any now-empty output event. Nothing else in command output is rewritten;
unknown personal paths/usernames, internal infrastructure, or credentials fail the
gate instead of receiving broader redaction.

The header retains original v2/dimensions/recording timestamp and includes only a
fixed public title, relative `python3 scene.py <scene>` command, and fixed TERM and
SHELL values. Incidental environment/header data and idle-limit metadata are not
published. Commands shown inside the recordings likewise use public executable
names and relative `app.yaml`, never absolute checkout/script paths. Kubeconfig
**paths** can become neutral labels; kubeconfig contents, bearer tokens, credentials,
private repository/service URLs, and signed URLs must never be recorded or published.
No CI secrets, proxy credentials, shell startup hooks, personal kubeconfig, or
container overrides are passed to scene subprocesses.
Existing proxy and TLS trust-store environment is inherited only by the unrecorded
build/preparation phase; it is not forwarded to recorded scenes.

The conservative publication gate rejects known runner identity, unredacted private
path roots, credential markers, private hostnames, unknown/credential-bearing URLs,
and IPs other than loopback or default k3s Pod/Service ranges (`10.42.0.0/16` and
`10.43.0.0/16`). Legitimate Pod/Service addresses and the owned loopback HTTP URL are
part of the demonstration. The gate is intentionally narrow: unexpected output
requires private review and a real fix/re-recording, not silently widening redaction
or editing the public demonstration. Automated scanning cannot prove absence of
all possible sensitive data; human publication review remains mandatory.

`provenance.json` records schema version 1, the observed full source commit, backend,
platform, observed Kalisto/asciinema versions, UTC collection time, excluded
preparation, original timing, scripted execution, exact redaction policy, actual
fixture digest/hash, and each final cast's SHA-256, real duration, and dimensions.
Linux provenance also records the actual kernel version and source, fragment,
final configuration and kernel SHA-256 hashes, plus the required built-in match.
Hashes cover the actual publication candidates after the documented path/header
handling. There are no placeholder hashes, timings, scenes, or successful fallbacks.

Recorder options are documented by [asciinema's official CLI guide](https://docs.asciinema.org/manual/cli/quick-start/)
and its [CLI option definitions](https://github.com/asciinema/asciinema/blob/v3.2.1/src/cli.rs).
The source CLI contract is `src/cli.rs`; preparation's pinned probe image and
rollout manifest are defined in `src/pull.rs`. If those interfaces change, update
the source recorder and regenerate genuine recordings rather than retaining a
compatibility shim or an older claim.

The host-safe `scripts/demos/test_capture.py` regression suite exercises every
two-chunk split of a private path, multiple spanning substitutions, concatenated
identity/credential rejection, unchanged timing/header privacy, and exit metadata.
During native CI verification, run
`python3 -m unittest discover -s scripts/demos -p 'test_*.py' -v`.
It does not invoke the engine or produce publication recordings.

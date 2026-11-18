# Tagged binary releases

Development, PRs, builds, tests and release validation run on
[Iceberg](https://iceberg.home.arpa/cloudflavor/kalisto). GitHub receives validated
changes afterward; do not execute workflows there.

Pushed SemVer tags (`vMAJOR.MINOR.PATCH`, with optional prerelease/build suffixes)
trigger `.forgejo/workflows/release.yml`, which publishes to the Forgejo instance
and repository supplied by the workflow environment. There is no hardcoded release
server or repository, and this document does not assert that a release is available.
Invalid `v*` tags fail rather than publish incorrectly versioned binaries.

## Download packages

On successful tagged publication, the release contains four archives and `SHA256SUMS`:

| Package | Rust target | Build / installed-package smoke check |
| --- | --- | --- |
| `kalisto-TAG-macos-arm64.tar.gz` | `aarch64-apple-darwin` | macOS arm64 runner; native `--version` and `ls` |
| `kalisto-TAG-macos-amd64.tar.gz` | `x86_64-apple-darwin` | Cross-compiled on macOS arm64; Mach-O architecture verified; no native Intel execution |
| `kalisto-TAG-linux-amd64.tar.gz` | `x86_64-unknown-linux-musl` | Linux x86_64 runner; native `--version` and `ls` |
| `kalisto-TAG-linux-arm64.tar.gz` | `aarch64-unknown-linux-musl` | Linux cross-build; `--version` and `ls` through user-mode QEMU, not native ARM64 validation |

`TAG` includes the `v` prefix, for example `v0.1.0`. `amd64` also means x86_64;
`arm64` also means aarch64. Each archive contains `kalisto` (mode 0755) and
`BUILD-INFO.json` recording tag, embedded version, full source commit and target.
Packaging normalizes archive timestamps/ownership; this is not a guarantee of
bit-for-bit reproducible binaries across different compiler/SDK installations.

Linux binaries are static musl executables: users do not need QEMU or a matching
glibc release on their native architecture. macOS binaries are unsigned and not
notarized. CI never installs Rosetta automatically.

### Functional support and remaining release prerequisites

Compilation, package smoke, and functional backend validation are separate:

| Host | Functional evidence | Limitation |
| --- | --- | --- |
| macOS Apple silicon | Native Apple-container three-node Cilium acceptance in CI, including the sixty-second startup assertion | Requires the installed Apple `container` runtime and prepared backend assets; optional Lima is not covered by this job |
| Linux x86_64 | Native Fedora 44 / Firecracker v1.18.0 validation observed fresh three-node Cilium, authenticated API, DNS/cross-node Service HTTP, stop/resume persistence and scale-out; fresh startup plus probes took 42.95s | Requires accessible KVM, rootless networking prerequisites and explicit prepared kernel/rootfs; this is not universal platform or feature parity |
| macOS Intel | x86_64 package cross-compilation and architecture inspection only | No native Intel macOS runner is registered; native install/runtime and Lima backend acceptance remain unverified |
| Linux ARM64 | Static cross-build and user-mode QEMU package smoke only | Native ARM64 Firecracker/KVM lifecycle remains unverified |

See [Linux prerequisites and recorded native evidence](prerequisites-linux-firecracker.md).
The new Linux CI job also asserts shrink-back to two workers, retained node identities,
and functional DNS/Service traffic after shrinking; that new coverage needs a passing
native CI run before claiming an observed scale-down result.

The registered native runners are macOS arm64 (`macos`) and Linux x86_64
(`tomis-host`, also carrying the container-runner label `tomis-ci`). No Intel
label or hardware is inferred from the cross-built package. Provisioning a real
Intel macOS runner and recording native installed-package plus backend acceptance
is an external prerequisite for claiming Intel runtime support.

A public stable release and default Firecracker kernel/rootfs source are separate
publication prerequisites; neither is created by a candidate build. Until published,
Linux users must supply local artifacts or an explicitly configured artifact source.
The release builder extracts each archive into a temporary install prefix and runs
supported smoke commands from an empty directory with isolated XDG roots. The
native Linux functional test likewise executes an installed copy, not the checkout
binary, from an empty working directory.

Download the correct archive and `SHA256SUMS`, then verify before installation:

```bash
sha256sum -c SHA256SUMS
# macOS alternative:
shasum -a 256 -c SHA256SUMS
```

Download all archives to check the entire checksum list, or select the matching
line for the single archive downloaded. Checksums detect corruption, not an
independently authenticated publisher signature. Trust the configured release source.

## Pipeline and publication safety

- Builds use locked dependencies and Rust 1.89.0. Tests, Clippy, formatting and
  release-tooling regression checks gate all four builds.
- PRs and manual `workflow_dispatch` runs build/upload CI candidates and verify
  the combined four-target inventory. **They never create a release.**
- Only a pushed release tag reaches publication. The publisher validates archive
  inventory, embedded metadata, architecture and exact tag/commit provenance.
- After all builds pass, it creates a Forgejo **draft**, uploads four archives and
  checksums, downloads them to verify hashes, rechecks that the tag has not moved,
  then makes the complete release visible.
- Failed publication leaves any created release as draft; no partial public release.
- Existing published releases are never overwritten. Matching partial draft uploads
  can resume. A differing asset, unexpected attachment, changed tag or different
  source stops publication for inspection rather than deleting user assets.
- macOS source workspaces, Rust toolchain/cache, targets and temporary output use
  configured, mounted external storage. Missing mounts fail before checkout/build;
  there is no internal-disk fallback. Each native job has a unique checkout,
  temporary directory and cluster state. Cargo targets and immutable prepared
  assets retain their external shared cache paths to avoid cold rebuilds. Cleanup
  removes only the owned workspace; shared caches and unrelated jobs are preserved.
- Release compilation remaps checkout, Cargo/rustup cache and home-directory paths
  to neutral prefixes in all four binaries, preserving configured compiler flags.
- Artifact actions are pinned to verified Forgejo-compatible commits. Release tokens
  are neither printed nor included in archives; credential-bearing asset requests
  are never redirected to a different origin.

## CI configuration

Set these Forgejo repository Actions variables out of band; none has a private
default in Git. Do not put actual host paths or private image names in the workflow.

| Variable | Required value |
| --- | --- |
| `KALISTO_LINUX_RUNNER` | Label of a Linux Docker-capable runner. |
| `KALISTO_CI_IMAGE` | Runner-accessible Debian-compatible CI image with Git, Bash, CA certificates, Python 3.9+, Rust/rustup, rustfmt, Clippy, sccache and Node.js for artifact actions. The release builder installs Rust 1.89.0. ARM64 Linux smoke requires `qemu-aarch64`; the job installs `qemu-user` through apt as root or passwordless sudo if absent. |
| `KALISTO_CI_CONTAINER_OPTIONS` | Optional container options, including any host cache bind mounts and their container destinations expected by the image's `CARGO_HOME`/sccache settings. Empty means no host cache mounts. |
| `KALISTO_MACOS_RUNNER` | Label of a trusted native macOS runner with Xcode command-line tools, Git, Bash, Python 3.9+, Rust/rustup, Node.js and the Homebrew Apple `container` runtime. Never assign an untrusted/fork job to this shared host. |
| `KALISTO_MACOS_VOLUME` | Absolute path to an already-mounted external volume, not `/`. |
| `KALISTO_MACOS_BUILD_ROOT` | Writable directory strictly beneath that volume, on the same filesystem. Workspaces and shared `cargo-home`, `release-rustup`, `sccache`, `apple-container-ci/target`, `apple-container-ci/artifacts` and per-target `release-target` caches live here. |
| `KALISTO_LINUX_BUILD_ROOT` | Optional absolute, user-owned mode-0700 storage directory for the native Linux job; defaults to `~/.cache/kalisto-firecracker-ci`. It reuses `firecracker-ci/target` and creates an owned temporary checkout, without changing host services or networking configuration. |
| `KALISTO_FIRECRACKER_KERNEL` | Optional prepared absolute kernel path on `tomis-host`. Setting it enables native Linux acceptance on main pushes. The sibling `.config` and `.manifest` must match kernel/config SHA256 values and include built-in `CONFIG_NETFILTER_XT_MATCH_STATISTIC=y`. Ordinary runs never rebuild or replace it. |
| `KALISTO_FIRECRACKER_ROOTFS` | Optional prepared absolute raw ext4 rootfs path on `tomis-host`. If omitted, native CI searches the existing versioned Firecracker artifact cache as described in the Linux guide. |
| `KALISTO_LINUX_CA_BUNDLE` | Optional existing CA bundle for native Linux source checkout and website download staging against the configured Forgejo origin. |

The native runner must have `cargo`, `rustc` and `rustup` on `PATH`, with the
toolchain needed by normal CI available under the external `release-rustup` directory.
Populate it using `RUSTUP_HOME="$KALISTO_MACOS_BUILD_ROOT/release-rustup"` and
`CARGO_HOME="$KALISTO_MACOS_BUILD_ROOT/cargo-home"`; do not copy personal credentials
into the shared cache. Every push to `main` runs the native functional job against
that exact merged commit; manual CI dispatches also run it. These runs are not
cancelled by later pushes. The job enables
`KALISTO_LIVE_APPLE_CONTAINER_BOOT_BUDGET=1` and runs the full three-node Cilium
test with its existing sixty-second end-to-end assertion. It borrows an already
running Homebrew service, or starts an installed unregistered service without
replacing a daemon, installing a kernel, changing daemon roots or stopping it.
Per-job `XDG_DATA_HOME`/`XDG_CONFIG_HOME` isolate cluster ownership; cleanup never
targets users' cluster state.

Native Linux acceptance is independent of showcase recordings. Dispatch **CI**
with `firecracker_live=true` to run `firecracker-live` on the existing `tomis-host`.
It reuses Firecracker v1.18.0, KVM and prepared host images; it does not install
tools, record demos or publish artifacts. Kernel preparation requires the separate
explicit `prepare_firecracker_kernel=true` dispatch input. When explicit image
variables are absent, an ordinary manual run searches
`~/.local/share/kalisto/artifacts/firecracker-node/{v1.18,v1.13}/x86_64/` for
`kalisto-vmlinux` and `ubuntu-24.04-minimal-k3s-amd64-rootfs.raw`.
Kernel/config sidecar hashes and the multi-endpoint Service kernel option are
checked before compilation. Missing or incompatible prerequisites fail explicitly.
Setting `KALISTO_FIRECRACKER_KERNEL` opts main pushes into this native gate;
without it, the job is skipped on ordinary pushes, not reported as native success.
To prepare a compatible kernel without replacing user artifacts, dispatch with
both `firecracker_live=true` and `prepare_firecracker_kernel=true`. This uses the
existing builder and host build tools with checksum-pinned Linux 6.19.9 source and
the current repository fragment. A new private `firecracker-ci/kalisto-kernel.*`
directory retains only the kernel/config/manifest plus its run ownership marker;
source and large build temporaries stay in the owned checkout and are cleaned.
The existing user kernel and rootfs are untouched. After native acceptance passes,
configure the printed verified kernel path as `KALISTO_FIRECRACKER_KERNEL` to
activate the main gate and avoid repeating kernel compilation. Retain that
directory while the variable references it.

Private Linux evidence stays in
`$CARGO_TARGET_DIR/kalisto/firecracker-budget/<unique-cluster>/`; no guest logs,
credentials or cluster state are uploaded. The test destroys only its unique
cluster, and the workflow removes only its own checkout.

Forgejo supplies `GITHUB_SERVER_URL` (HTTPS origin), `GITHUB_REPOSITORY`
(`owner/name`), `GITHUB_TOKEN`, `GITHUB_EVENT_NAME`, `GITHUB_REF`, `GITHUB_SHA`,
`GITHUB_RUN_ID` and `GITHUB_ENV`. Publication requires the workflow token to read
tags and read/write releases and attachments in that repository. No other
destination is inferred. HTTPS asset downloads and redirects remain restricted
to that configured origin. Candidates need the instance's artifact service.

`.gitlab-ci.yml` uses public Rust, Ubuntu and Debian images and standard GitLab
environment variables; it has no private runner label or registry dependency.
GitLab runners must support the declared image architectures and the existing
privileged image-building tools. To keep native build/cache/output off internal
storage, configure the runner's `builds_dir` and cache storage on its external
volume; project-local paths and job-specific scratch directories then stay there.

CI validation includes:

```bash
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked -- --test-threads=1
```

Use a manual release workflow run to build and verify all four candidates without
publishing. Run native functional checks only on the configured CI runner; they
create and destroy their own real cluster, not a mock.

For versioned candidate preparation, set the existing release workflow's optional
manual-dispatch input `candidate_tag` to the intended SemVer tag, such as `v0.1.0`.
All four packages then embed that version and the exact dispatched source commit.
This does not create a tag or publish a release, and the input cannot enable the
tag-push publication step. Leave it empty for ordinary `ci-<commit>` development
packages.

## Create a release

Choose a reviewed commit with green CI and a new version. Require passing native
startup acceptance and the complete four-target candidate inventory for that exact
commit before creating the release tag. Example only:

```bash
git tag -s v0.1.0 COMMIT
git push origin v0.1.0
```

`origin` must point to the configured Forgejo repository. The pipeline resolves lightweight and annotated tags,
checks out their exact commit and embeds the tag version (including prerelease
suffixes). Tags with prerelease suffixes create prereleases. Do not force-move release
tags; create a new version instead. A publication rerun against an already published
release stops safely without changing it.

The workflow itself does not create version tags. No tag or release is created merely
by opening its PR or running a manual candidate build.

## Stage published website downloads

After publishing, dispatch **Stage verified website downloads** with the published
`tag`. Staging resolves that tag's source commit independently of the workflow
checkout, so an older release can be staged from a newer reviewed workflow.
It verifies the four archives, provenance, checksums and unchanged tag before
exposing files. Identical existing caches are reused; different operator data
is never overwritten. A CI candidate is not a published release and cannot pass
this staging gate.

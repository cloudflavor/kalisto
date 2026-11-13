# Tagged binary releases

Pushed SemVer tags (`vMAJOR.MINOR.PATCH`, with optional prerelease/build suffixes)
trigger `.forgejo/workflows/release.yml`. Releases publish only to
[forgejo, cloudflavor/kalisto](https://forgejo.example.test/cloudflavor/kalisto/releases).
Invalid `v*` tags fail rather than publish incorrectly versioned binaries.

## Download packages

Each release contains four archives and `SHA256SUMS`:

| Package | Rust target | Build / smoke check |
| --- | --- | --- |
| `kalisto-TAG-macos-arm64.tar.gz` | `aarch64-apple-darwin` | macOS runner; native `--version` |
| `kalisto-TAG-macos-amd64.tar.gz` | `x86_64-apple-darwin` | Cross-compiled on macOS arm64; Mach-O architecture verified |
| `kalisto-TAG-linux-amd64.tar.gz` | `x86_64-unknown-linux-musl` | Linux runner; native `--version` |
| `kalisto-TAG-linux-arm64.tar.gz` | `aarch64-unknown-linux-musl` | Linux cross-build; `--version` through user-mode QEMU |

`TAG` includes the `v` prefix, for example `v0.1.0`. `amd64` also means x86_64;
`arm64` also means aarch64. Each archive contains `kalisto` (mode 0755) and
`BUILD-INFO.json` recording tag, embedded version, full source commit and target.
Packaging normalizes archive timestamps/ownership; this is not a guarantee of
bit-for-bit reproducible binaries across different compiler/SDK installations.

Linux binaries are static musl executables: users do not need QEMU or a matching
glibc release on their native architecture. Linux backend functionality remains
in progress; compilation/smoke checks do not prove full Firecracker feature parity.
macOS binaries are unsigned and not notarized. Intel output is architecture-checked,
not runtime-tested on Intel hardware; CI never installs Rosetta automatically.

Download the correct archive and `SHA256SUMS`, then verify before installation:

```bash
sha256sum -c SHA256SUMS
# macOS alternative:
shasum -a 256 -c SHA256SUMS
```

Download all archives to check the entire checksum list, or select the matching
line for the single archive downloaded. Checksums detect corruption, not an
independently authenticated publisher signature. Trust the forgejo release source.

## Pipeline and publication safety

- Builds use locked dependencies and Rust 1.89.0. Tests, Clippy, formatting and
  release-tooling regression checks gate all four builds.
- PRs and manual `workflow_dispatch` runs build/upload CI candidates and verify
  the combined four-target inventory. **They never create a release.**
- Only a pushed release tag reaches publication. The publisher validates archive
  inventory, embedded metadata, architecture and exact tag/commit provenance.
- After all builds pass, it creates an forgejo **draft**, uploads four archives and
  checksums, downloads them to verify hashes, rechecks that the tag has not moved,
  then makes the complete release visible.
- Failed publication leaves any created release as draft; no partial public release.
- Existing published releases are never overwritten. Matching partial draft uploads
  can resume. A differing asset, unexpected attachment, changed tag or different
  source stops publication for inspection rather than deleting user assets.
- macOS source workspaces, Rust toolchain/cache, targets and temporary output use the
  mounted external `/Volumes/XcodeBuilds/runner` volume. Missing mount fails before
  checkout/build; there is no internal-disk fallback. Only job-owned workspaces are
  removed; shared caches and unrelated CI jobs are preserved.
- Artifact actions are pinned to verified Forgejo-compatible commits. Release tokens
  are neither printed nor included in archives; credential-bearing asset requests
  are never redirected to a different origin.

## Create a release

Choose a reviewed commit with green CI and a new version. Example only:

```bash
git tag -s v0.1.0 COMMIT
git push origin v0.1.0
```

`origin` must point to forgejo. The pipeline resolves lightweight and annotated tags,
checks out their exact commit and embeds the tag version (including prerelease
suffixes). Tags with prerelease suffixes create prereleases. Do not force-move release
tags; create a new version instead. A publication rerun against an already published
release stops safely without changing it.

The workflow itself does not create version tags. No tag or release is created merely
by opening its PR or running a manual candidate build.

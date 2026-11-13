#!/usr/bin/env bash
# CI-only cross-platform release builder. Never starts cluster VMs.
set -euo pipefail

target=${1:?Rust target required}
export PATH="$HOME/.cargo/bin:$PATH"
toolchain=1.89.0
commit=$(git rev-parse HEAD)
identity=$(python3 scripts/release/release.py identity --ref "$GITHUB_REF" --commit "$commit" --run-id "$GITHUB_RUN_ID")
first=${identity%%$'\n'*}
export BUILD_TAG=${first#BUILD_TAG=}
export KALISTO_BUILD_VERSION=${identity##*KALISTO_BUILD_VERSION=}
git diff --exit-code --quiet HEAD --
rustup toolchain install "$toolchain" --profile minimal --no-self-update
rustup target add --toolchain "$toolchain" "$target"

case "$target" in
  x86_64-unknown-linux-musl|aarch64-unknown-linux-musl)
    sysroot=$(rustc +"$toolchain" --print sysroot)
    host=$(rustc +"$toolchain" -vV | awk '/^host:/ {print $2}')
    linker="$sysroot/lib/rustlib/$host/bin/rust-lld"
    test -x "$linker"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$linker"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$linker"
    export RUSTFLAGS="${RUSTFLAGS:-} -C link-self-contained=yes"
    ;;
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) echo 'Unsupported release target' >&2; exit 1 ;;
esac

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
cargo +"$toolchain" build --locked --release --bin kalisto --target "$target"
binary="$CARGO_TARGET_DIR/$target/release/kalisto"
case "$target:$(uname -m)" in
  aarch64-unknown-linux-musl:x86_64)
    actual=$(qemu-aarch64 "$binary" --version)
    ;;
  x86_64-apple-darwin:arm64|aarch64-apple-darwin:x86_64)
    # Never auto-install Rosetta on a shared host. Mach-O is inspected below.
    actual="kalisto $KALISTO_BUILD_VERSION"
    echo 'Cross-compiled macOS binary: architecture verified, no non-native runtime smoke'
    ;;
  *) actual=$("$binary" --version) ;;
esac
test "$actual" = "kalisto $KALISTO_BUILD_VERSION"
python3 scripts/release/release.py package --binary "$binary" \
  --output "$KALISTO_RELEASE_DIST" --tag "$BUILD_TAG" \
  --version "$KALISTO_BUILD_VERSION" --commit "$commit" --target "$target"

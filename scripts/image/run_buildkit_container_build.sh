#!/bin/sh
set -eu

WORKSPACE_ROOT="${WORKSPACE_ROOT:-$(pwd)}"
UBUNTU_RELEASE="${KALISTO_UBUNTU_RELEASE:-noble}"
UBUNTU_ARCH="${KALISTO_UBUNTU_ARCH:-arm64}"
K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"
OUTPUT_DIR="${KALISTO_BUILDKIT_OUTPUT_DIR:-${WORKSPACE_ROOT}/artifacts/lima-rootfs/${UBUNTU_ARCH}}"
CONTEXT_DIR="${KALISTO_BUILDKIT_CONTEXT_DIR:-${WORKSPACE_ROOT}/.tmp-buildkit-context}"
BUILDKIT_VERSION="${KALISTO_BUILDKIT_VERSION:-v0.26.2}"
CACHE_DIR="${KALISTO_BUILDKIT_CACHE_DIR:-${WORKSPACE_ROOT}/.cache/buildkit/${BUILDKIT_VERSION}/${UBUNTU_ARCH}}"

case "${UBUNTU_ARCH}" in
  arm64)
    BUILD_PLATFORM="linux/arm64/v8"
    DEFAULT_MIRROR="http://ftp.fau.de/ubuntu-ports"
    DEFAULT_K3S_CACHE_PATH="${WORKSPACE_ROOT}/.cache/k3s/${K3S_VERSION}-k3s-arm64"
    ;;
  amd64)
    BUILD_PLATFORM="linux/amd64"
    DEFAULT_MIRROR="http://ch.archive.ubuntu.com/ubuntu"
    DEFAULT_K3S_CACHE_PATH="${WORKSPACE_ROOT}/.cache/k3s/${K3S_VERSION}-k3s"
    ;;
  *)
    echo "unsupported Ubuntu target architecture: ${UBUNTU_ARCH}" >&2
    exit 1
    ;;
esac

UBUNTU_MIRROR="${KALISTO_UBUNTU_MIRROR:-${DEFAULT_MIRROR}}"
K3S_CACHE_PATH="${KALISTO_K3S_CACHE_PATH:-${DEFAULT_K3S_CACHE_PATH}}"
PACKAGE_LIST="${KALISTO_UBUNTU_PACKAGE_LIST:-systemd-sysv cloud-init openssh-server sudo iptables kmod socat linux-image-virtual grub-common grub2-common grub-efi-${UBUNTU_ARCH}-bin}"

K3S_ASSET_URL="${KALISTO_K3S_ASSET_URL:-}"

if [ ! -f "${K3S_CACHE_PATH}" ]; then
  if [ -z "${K3S_ASSET_URL}" ]; then
    case "${UBUNTU_ARCH}" in
      arm64) K3S_ASSET_URL="https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/k3s-arm64" ;;
      amd64) K3S_ASSET_URL="https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/k3s" ;;
    esac
  fi
  mkdir -p "$(dirname "${K3S_CACHE_PATH}")"
  curl -fL --retry 5 --retry-delay 2 --connect-timeout 15 "${K3S_ASSET_URL}" -o "${K3S_CACHE_PATH}"
  chmod 0755 "${K3S_CACHE_PATH}"
fi

if [ ! -f "${K3S_CACHE_PATH}" ]; then
  echo "missing staged k3s binary: ${K3S_CACHE_PATH}" >&2
  exit 1
fi

case "${UBUNTU_ARCH}" in
  arm64) BUILDKIT_RELEASE_ARCH="arm64" ;;
  amd64) BUILDKIT_RELEASE_ARCH="amd64" ;;
esac

BUILDKIT_BIN_DIR="${CACHE_DIR}/bin"
BUILDKITD_BIN="${BUILDKIT_BIN_DIR}/buildkitd"
BUILDKITCTL_BIN="${BUILDKIT_BIN_DIR}/buildctl"
BUILDKIT_TAR="${CACHE_DIR}/buildkit-${BUILDKIT_VERSION}.linux-${BUILDKIT_RELEASE_ARCH}.tar.gz"

for REQUIRED in curl tar; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done

if [ ! -x "${BUILDKITD_BIN}" ] || [ ! -x "${BUILDKITCTL_BIN}" ]; then
  mkdir -p "${CACHE_DIR}"
  if [ ! -f "${BUILDKIT_TAR}" ]; then
    curl -fL --retry 5 --retry-delay 2 --connect-timeout 15 "https://github.com/moby/buildkit/releases/download/${BUILDKIT_VERSION}/buildkit-${BUILDKIT_VERSION}.linux-${BUILDKIT_RELEASE_ARCH}.tar.gz" -o "${BUILDKIT_TAR}"
  fi
  rm -rf "${BUILDKIT_BIN_DIR}"
  mkdir -p "${BUILDKIT_BIN_DIR}"
  tar -xzf "${BUILDKIT_TAR}" -C "${CACHE_DIR}"
fi

rm -rf "${CONTEXT_DIR}" "${OUTPUT_DIR}"
mkdir -p "${CONTEXT_DIR}" "${OUTPUT_DIR}"

cp "${WORKSPACE_ROOT}/scripts/image/Containerfile.lima-node" "${CONTEXT_DIR}/Containerfile"
cp "${WORKSPACE_ROOT}/scripts/image/build_lima_rootfs_in_container.sh" "${CONTEXT_DIR}/build_lima_rootfs_in_container.sh"
cp "${WORKSPACE_ROOT}/scripts/image/provision_kalisto_node.sh" "${CONTEXT_DIR}/provision_lima_node.sh"
cp "${K3S_CACHE_PATH}" "${CONTEXT_DIR}/k3s"
: > "${CONTEXT_DIR}/.dockerignore"

ROOTFS_EXPORT_DIR="${OUTPUT_DIR}/rootfs"
rm -rf "${ROOTFS_EXPORT_DIR}"
mkdir -p "${ROOTFS_EXPORT_DIR}"

normalize_exported_symlinks() {
  rootfs_dir="$1"
  find "${rootfs_dir}" -type l | while IFS= read -r link_path; do
    target_path="$(readlink "${link_path}")"
    case "${target_path}" in
      /*)
        resolved_target="${rootfs_dir}${target_path}"
        if [ ! -e "${resolved_target}" ] && [ ! -L "${resolved_target}" ]; then
          continue
        fi
        ln -sfnr "${resolved_target}" "${link_path}"
        ;;
    esac
  done
}

BUILDKIT_ROOT_DIR="/tmp/buildkit-state-${UBUNTU_ARCH}"
BUILDKIT_RUN_DIR="/tmp/buildkit-run-${UBUNTU_ARCH}"
BUILDKIT_SOCK_PATH="${BUILDKIT_RUN_DIR}/buildkitd.sock"
BUILDKIT_SOCK="unix://${BUILDKIT_SOCK_PATH}"
rm -rf "${BUILDKIT_RUN_DIR}"
mkdir -p "${BUILDKIT_RUN_DIR}"
rm -rf "${BUILDKIT_ROOT_DIR}"
mkdir -p "${BUILDKIT_ROOT_DIR}"
"${BUILDKITD_BIN}" --root "${BUILDKIT_ROOT_DIR}" --addr "${BUILDKIT_SOCK}" >"${CACHE_DIR}/buildkitd.log" 2>&1 &
BUILDKITD_PID=$!
cleanup_buildkit() {
  kill "${BUILDKITD_PID}" >/dev/null 2>&1 || true
  wait "${BUILDKITD_PID}" 2>/dev/null || true
}
trap cleanup_buildkit EXIT INT TERM
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if [ -S "${BUILDKIT_SOCK_PATH}" ] && "${BUILDKITCTL_BIN}" --addr "${BUILDKIT_SOCK}" debug workers >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if [ ! -S "${BUILDKIT_SOCK_PATH}" ] || ! "${BUILDKITCTL_BIN}" --addr "${BUILDKIT_SOCK}" debug workers >/dev/null 2>&1; then
  cat "${CACHE_DIR}/buildkitd.log" >&2 || true
  echo "buildkitd failed to start" >&2
  exit 1
fi

"${BUILDKITCTL_BIN}" --addr "${BUILDKIT_SOCK}" build \
  --progress=plain \
  --frontend dockerfile.v0 \
  --local context="${CONTEXT_DIR}" \
  --local dockerfile="${CONTEXT_DIR}" \
  --opt filename=Containerfile \
  --opt platform="${BUILD_PLATFORM}" \
  --opt build-arg:UBUNTU_BASE_IMAGE="ubuntu:${UBUNTU_RELEASE}" \
  --opt build-arg:UBUNTU_MIRROR="${UBUNTU_MIRROR}" \
  --opt build-arg:PACKAGE_LIST="${PACKAGE_LIST}" \
  --opt build-arg:K3S_VERSION="${K3S_VERSION}" \
  --output type=local,dest="${ROOTFS_EXPORT_DIR}"

normalize_exported_symlinks "${ROOTFS_EXPORT_DIR}"

echo "wrote ${ROOTFS_EXPORT_DIR}"

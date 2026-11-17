#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
# shellcheck source=/dev/null
source "${ROOT_DIR}/scripts/linux/firecracker_artifact_context.sh"
kalisto_detect_firecracker_context

OUTPUT_DIR="${KALISTO_FIRECRACKER_OUTPUT_DIR:-${ROOT_DIR}/artifacts/firecracker-node/${KALISTO_FIRECRACKER_ARTIFACT_TRACK}/${KALISTO_FIRECRACKER_ARTIFACT_ARCH}}"
WORK_DIR="${KALISTO_FIRECRACKER_KERNEL_WORK_DIR:-${ROOT_DIR}/artifacts/firecracker-node/work/kernel}"
RUN_PARENT_DIR="${KALISTO_FIRECRACKER_KERNEL_TMP_DIR:-${TMPDIR:-/tmp}/kalisto-firecracker-kernel}"
KERNEL_VERSION="${KALISTO_FIRECRACKER_KERNEL_VERSION:-6.19.9}"
KERNEL_FILENAME="${KALISTO_FIRECRACKER_KERNEL_FILENAME:-kalisto-vmlinux}"
KERNEL_CONFIG_FRAGMENT="${KALISTO_FIRECRACKER_KERNEL_CONFIG_FRAGMENT:-${ROOT_DIR}/scripts/linux/firecracker-k3s-x86_64.config}"
KERNEL_MAJOR="$(printf '%s' "${KERNEL_VERSION}" | cut -d. -f1)"
KERNEL_SOURCE_TARBALL="linux-${KERNEL_VERSION}.tar.xz"
KERNEL_SOURCE_URL="${KALISTO_FIRECRACKER_KERNEL_SOURCE_URL:-https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/${KERNEL_SOURCE_TARBALL}}"
KERNEL_SOURCE_EXPECTED_SHA256="${KALISTO_FIRECRACKER_KERNEL_SOURCE_SHA256:-}"
SOURCE_ARCHIVE_PATH="${WORK_DIR}/${KERNEL_SOURCE_TARBALL}"
FINAL_KERNEL_OUTPUT_PATH="${OUTPUT_DIR}/${KERNEL_FILENAME}"
FINAL_KERNEL_CONFIG_OUTPUT_PATH="${OUTPUT_DIR}/${KERNEL_FILENAME}.config"
FINAL_MANIFEST_PATH="${OUTPUT_DIR}/${KERNEL_FILENAME}.manifest"
JOBS="${KALISTO_FIRECRACKER_KERNEL_JOBS:-$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || printf '4')}"

case "${KALISTO_FIRECRACKER_ARTIFACT_ARCH}" in
  x86_64)
    KERNEL_ARCH="x86"
    DEFCONFIG_TARGET="x86_64_defconfig"
    ;;
  *)
    echo "unsupported Kalisto Firecracker kernel build architecture: ${KALISTO_FIRECRACKER_ARTIFACT_ARCH}" >&2
    exit 1
    ;;
esac

for required in awk bash cp curl file grep make rsync sed sha256sum shasum sort tail tar xz; do
  if command -v "${required}" >/dev/null 2>&1; then
    continue
  fi
  if [ "${required}" = "sha256sum" ] || [ "${required}" = "shasum" ]; then
    continue
  fi
  echo "${required} is required" >&2
  exit 1
done

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

GCC_MAJOR=
if command -v gcc >/dev/null 2>&1; then
  GCC_MAJOR="$(gcc -dumpfullversion -dumpversion 2>/dev/null | cut -d. -f1)"
fi

if [ ! -f "${KERNEL_CONFIG_FRAGMENT}" ]; then
  echo "missing Kalisto Firecracker kernel config fragment: ${KERNEL_CONFIG_FRAGMENT}" >&2
  exit 1
fi

mkdir -p "${OUTPUT_DIR}" "${WORK_DIR}" "${RUN_PARENT_DIR}"
RUN_WORK_DIR="$(mktemp -d "${RUN_PARENT_DIR}/run-XXXXXX")"
SOURCE_DIR="${RUN_WORK_DIR}/linux-${KERNEL_VERSION}"
BUILD_DIR="${RUN_WORK_DIR}/build-${KERNEL_VERSION}-${KALISTO_FIRECRACKER_ARTIFACT_ARCH}"
OPENSSL_COMPAT_INCLUDE_DIR="${RUN_WORK_DIR}/openssl-compat"
KERNEL_OUTPUT_PATH="${RUN_WORK_DIR}/${KERNEL_FILENAME}"
KERNEL_CONFIG_OUTPUT_PATH="${RUN_WORK_DIR}/${KERNEL_FILENAME}.config"
MANIFEST_PATH="${RUN_WORK_DIR}/${KERNEL_FILENAME}.manifest"

cleanup() {
  chmod -R u+w "${RUN_WORK_DIR}" 2>/dev/null || true
  rm -rf "${RUN_WORK_DIR}"
}
trap cleanup EXIT INT TERM

if [ ! -f /usr/include/openssl/engine.h ]; then
  mkdir -p "${OPENSSL_COMPAT_INCLUDE_DIR}/openssl"
  cat > "${OPENSSL_COMPAT_INCLUDE_DIR}/openssl/engine.h" <<'EOF'
#ifndef KALISTO_OPENSSL_ENGINE_COMPAT_H
#define KALISTO_OPENSSL_ENGINE_COMPAT_H

typedef struct engine_st ENGINE;

void ENGINE_load_builtin_engines(void);
ENGINE *ENGINE_by_id(const char *id);
int ENGINE_init(ENGINE *e);
int ENGINE_ctrl_cmd_string(ENGINE *e, const char *cmd_name, const char *arg, int cmd_optional);
long ENGINE_ctrl_cmd(ENGINE *e, const char *cmd_name, long i, void *p, void (*f)(void), int cmd_optional);

#endif /* KALISTO_OPENSSL_ENGINE_COMPAT_H */
EOF
  export HOSTCFLAGS="${HOSTCFLAGS:+${HOSTCFLAGS} }-I${OPENSSL_COMPAT_INCLUDE_DIR}"
fi

if [ ! -f "${SOURCE_ARCHIVE_PATH}" ]; then
  echo "downloading Linux kernel source ${KERNEL_VERSION} to ${SOURCE_ARCHIVE_PATH}"
  curl -fL --retry 3 --connect-timeout 10 -o "${SOURCE_ARCHIVE_PATH}" "${KERNEL_SOURCE_URL}"
else
  echo "reusing cached kernel source at ${SOURCE_ARCHIVE_PATH}"
fi

if command -v sha256sum >/dev/null 2>&1; then
  SOURCE_SHA256="$(sha256sum "${SOURCE_ARCHIVE_PATH}" | awk '{print $1}')"
else
  SOURCE_SHA256="$(shasum -a 256 "${SOURCE_ARCHIVE_PATH}" | awk '{print $1}')"
fi
if [ -n "${KERNEL_SOURCE_EXPECTED_SHA256}" ] && [ "${SOURCE_SHA256}" != "${KERNEL_SOURCE_EXPECTED_SHA256}" ]; then
  echo "Linux kernel source checksum does not match the pinned input" >&2
  exit 1
fi

if [ ! -d "${SOURCE_DIR}" ]; then
  tar \
    --delay-directory-restore \
    --no-same-owner \
    --no-same-permissions \
    --mode='u+rwX,go+rX,go-w' \
    -C "${RUN_WORK_DIR}" \
    -xf "${SOURCE_ARCHIVE_PATH}"
  chmod -R u+rwX "${SOURCE_DIR}"
fi

if [ -n "${GCC_MAJOR}" ] && [ "${GCC_MAJOR}" -ge 15 ] 2>/dev/null; then
  X86_MAKEFILE="${SOURCE_DIR}/arch/x86/Makefile"
  if [ -f "${X86_MAKEFILE}" ] && ! grep -q '^REALMODE_CFLAGS += -std=gnu11$' "${X86_MAKEFILE}"; then
    printf '%s\n' 'REALMODE_CFLAGS += -std=gnu11' >> "${X86_MAKEFILE}"
  fi
  export KCFLAGS="${KCFLAGS:+${KCFLAGS} }-std=gnu11"
  export HOSTCFLAGS="${HOSTCFLAGS:+${HOSTCFLAGS} }-std=gnu11"
fi

mkdir -p "${BUILD_DIR}"

make -C "${SOURCE_DIR}" O="${BUILD_DIR}" ARCH="${KERNEL_ARCH}" "${DEFCONFIG_TARGET}"
# Merge the fragment first, then run olddefconfig explicitly for this arch.
"${SOURCE_DIR}/scripts/kconfig/merge_config.sh" -m -O "${BUILD_DIR}" "${BUILD_DIR}/.config" "${KERNEL_CONFIG_FRAGMENT}"
make -C "${SOURCE_DIR}" O="${BUILD_DIR}" ARCH="${KERNEL_ARCH}" olddefconfig

while IFS= read -r required; do
  [ -z "${required}" ] && continue
  if ! grep -q "^${required}=y$" "${BUILD_DIR}/.config"; then
    echo "required kernel option is missing after merge: ${required}" >&2
    exit 1
  fi
done <<'EOF'
CONFIG_NF_TABLES
CONFIG_NFT_COMPAT
CONFIG_NFT_NAT
CONFIG_NFT_CT
CONFIG_NFT_MASQ
CONFIG_NFT_REDIR
CONFIG_NFT_SOCKET
CONFIG_NFT_TPROXY
CONFIG_NFT_REJECT
CONFIG_NFT_REJECT_INET
CONFIG_NFT_REJECT_IPV4
CONFIG_NFT_REJECT_IPV6
CONFIG_NETFILTER_XT_NAT
CONFIG_IP_NF_TARGET_REJECT
CONFIG_IP6_NF_TARGET_REJECT
CONFIG_NF_REJECT_IPV4
CONFIG_NF_REJECT_IPV6
CONFIG_NETFILTER_XT_MATCH_CONNTRACK
CONFIG_NETFILTER_XT_MATCH_COMMENT
CONFIG_NETFILTER_XT_MATCH_SOCKET
CONFIG_NETFILTER_XT_MATCH_STATISTIC
CONFIG_NETFILTER_XT_TARGET_CT
CONFIG_NETFILTER_XT_TARGET_MASQUERADE
CONFIG_NETFILTER_XT_TARGET_TPROXY
CONFIG_NF_SOCKET_IPV4
CONFIG_NF_SOCKET_IPV6
EOF

make -C "${SOURCE_DIR}" O="${BUILD_DIR}" ARCH="${KERNEL_ARCH}" -j"${JOBS}" vmlinux

cp "${BUILD_DIR}/vmlinux" "${KERNEL_OUTPUT_PATH}"
cp "${BUILD_DIR}/.config" "${KERNEL_CONFIG_OUTPUT_PATH}"

if ! file "${KERNEL_OUTPUT_PATH}" | grep -q "ELF 64-bit"; then
  echo "built kernel is not an ELF image: ${KERNEL_OUTPUT_PATH}" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  KERNEL_SHA256="$(sha256sum "${KERNEL_OUTPUT_PATH}" | awk '{print $1}')"
  CONFIG_SHA256="$(sha256sum "${KERNEL_CONFIG_OUTPUT_PATH}" | awk '{print $1}')"
else
  KERNEL_SHA256="$(shasum -a 256 "${KERNEL_OUTPUT_PATH}" | awk '{print $1}')"
  CONFIG_SHA256="$(shasum -a 256 "${KERNEL_CONFIG_OUTPUT_PATH}" | awk '{print $1}')"
fi

KERNEL_APPARENT_SIZE_BYTES="$(wc -c < "${KERNEL_OUTPUT_PATH}" | tr -d ' ')"
KERNEL_DISK_USAGE="$(du -h "${KERNEL_OUTPUT_PATH}" | awk '{print $1}')"

cat > "${MANIFEST_PATH}" <<EOF
FIRECRACKER_VERSION=${KALISTO_FIRECRACKER_VERSION_NORMALIZED}
FIRECRACKER_ARTIFACT_TRACK=${KALISTO_FIRECRACKER_ARTIFACT_TRACK}
FIRECRACKER_ARCH=${KALISTO_FIRECRACKER_ARTIFACT_ARCH}
KERNEL_VERSION=${KERNEL_VERSION}
KERNEL_SOURCE_URL=${KERNEL_SOURCE_URL}
KERNEL_SOURCE_SHA256=${SOURCE_SHA256}
KERNEL_PATH=${KERNEL_OUTPUT_PATH}
KERNEL_FILENAME=$(basename -- "${KERNEL_OUTPUT_PATH}")
KERNEL_FORMAT=elf
KERNEL_SHA256=${KERNEL_SHA256}
KERNEL_APPARENT_SIZE_BYTES=${KERNEL_APPARENT_SIZE_BYTES}
KERNEL_DISK_USAGE=${KERNEL_DISK_USAGE}
KERNEL_CONFIG_PATH=${KERNEL_CONFIG_OUTPUT_PATH}
KERNEL_CONFIG_SHA256=${CONFIG_SHA256}
KERNEL_CONFIG_FRAGMENT=${KERNEL_CONFIG_FRAGMENT}
EOF

rm -f "${FINAL_KERNEL_OUTPUT_PATH}" "${FINAL_KERNEL_CONFIG_OUTPUT_PATH}" "${FINAL_MANIFEST_PATH}"
mv "${KERNEL_OUTPUT_PATH}" "${FINAL_KERNEL_OUTPUT_PATH}"
mv "${KERNEL_CONFIG_OUTPUT_PATH}" "${FINAL_KERNEL_CONFIG_OUTPUT_PATH}"
mv "${MANIFEST_PATH}" "${FINAL_MANIFEST_PATH}"

echo "created ${FINAL_KERNEL_OUTPUT_PATH}"
echo "wrote ${FINAL_KERNEL_CONFIG_OUTPUT_PATH}"
echo "wrote ${FINAL_MANIFEST_PATH}"

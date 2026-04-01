#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
. "${ROOT_DIR}/scripts/linux/firecracker_artifact_context.sh"
kalisto_detect_firecracker_context

PROVISION_SCRIPT="${KALISTO_IMAGE_PROVISION_SCRIPT:-${ROOT_DIR}/scripts/image/provision_kalisto_node.sh}"
OUTPUT_DIR="${KALISTO_FIRECRACKER_OUTPUT_DIR:-${ROOT_DIR}/artifacts/firecracker-node/${KALISTO_FIRECRACKER_ARTIFACT_TRACK}/${KALISTO_FIRECRACKER_ARTIFACT_ARCH}}"
WORK_DIR="${KALISTO_FIRECRACKER_WORK_DIR:-/tmp/kalisto-firecracker-rootfs}"
K3S_CACHE_DIR="${KALISTO_K3S_CACHE_DIR:-${ROOT_DIR}/artifacts/firecracker-node/cache/k3s}"
BASE_IMAGE_URL="${KALISTO_BASE_IMAGE_URL:-https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img}"
IMAGE_BASENAME="${KALISTO_IMAGE_BASENAME:-ubuntu-24.04-minimal-k3s-amd64}"
K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"
K3S_ASSET="${KALISTO_K3S_ASSET:-k3s}"
LIBGUESTFS_BACKEND="${LIBGUESTFS_BACKEND:-libvirt}"
ROOTFS_SOURCE_PARTITION="${KALISTO_FIRECRACKER_ROOTFS_SOURCE_PARTITION:-/dev/sda1}"
ROOTFS_TARGET_SIZE_MIB="${KALISTO_FIRECRACKER_ROOTFS_SIZE_MIB:-8192}"
ROOTFS_ARCHIVE_FORMAT="${KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_FORMAT:-xz}"
ROOTFS_ARCHIVE_LEVEL="${KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_LEVEL:-6}"

if [ ! -f "${PROVISION_SCRIPT}" ]; then
  echo "missing provision script: ${PROVISION_SCRIPT}" >&2
  exit 1
fi

for REQUIRED in curl cp dd e2fsck parted qemu-img resize2fs truncate virt-customize; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

case "${ROOTFS_TARGET_SIZE_MIB}" in
  ''|*[!0-9]*)
    echo "KALISTO_FIRECRACKER_ROOTFS_SIZE_MIB must be a positive integer" >&2
    exit 1
    ;;
  0)
    echo "KALISTO_FIRECRACKER_ROOTFS_SIZE_MIB must be greater than zero" >&2
    exit 1
    ;;
esac

ROOTFS_TARGET_SIZE_BYTES="$((ROOTFS_TARGET_SIZE_MIB * 1024 * 1024))"

case "${ROOTFS_ARCHIVE_FORMAT}" in
  xz|gz|zst|none)
    ;;
  *)
    echo "KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_FORMAT must be one of: xz, gz, zst, none" >&2
    exit 1
    ;;
esac

if [ "${ROOTFS_ARCHIVE_FORMAT}" = "xz" ] && ! command -v xz >/dev/null 2>&1; then
  echo "xz is required for KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_FORMAT=xz" >&2
  exit 1
fi
if [ "${ROOTFS_ARCHIVE_FORMAT}" = "gz" ] && ! command -v gzip >/dev/null 2>&1; then
  echo "gzip is required for KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_FORMAT=gz" >&2
  exit 1
fi
if [ "${ROOTFS_ARCHIVE_FORMAT}" = "zst" ] && ! command -v zstd >/dev/null 2>&1; then
  echo "zstd is required for KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_FORMAT=zst" >&2
  exit 1
fi

run_e2fsck() {
  set +e
  e2fsck -fy "$1" >/dev/null
  status=$?
  set -e
  if [ "${status}" -gt 1 ]; then
    echo "e2fsck failed for $1 with exit status ${status}" >&2
    exit "${status}"
  fi
}

mkdir -p \
  "${OUTPUT_DIR}" \
  "${WORK_DIR}" \
  "${K3S_CACHE_DIR}"

RUN_WORK_DIR="$(mktemp -d "${WORK_DIR}/run-XXXXXX")"
LIBGUESTFS_TMPDIR="${KALISTO_FIRECRACKER_LIBGUESTFS_TMPDIR:-${RUN_WORK_DIR}/tmp/libguestfs}"
LIBGUESTFS_CACHEDIR="${KALISTO_FIRECRACKER_LIBGUESTFS_CACHEDIR:-${RUN_WORK_DIR}/cache/libguestfs}"
XDG_RUNTIME_DIR="${KALISTO_FIRECRACKER_XDG_RUNTIME_DIR:-${RUN_WORK_DIR}/run}"

mkdir -p \
  "${LIBGUESTFS_TMPDIR}" \
  "${LIBGUESTFS_CACHEDIR}" \
  "${XDG_RUNTIME_DIR}"

BASE_IMAGE_PATH="${WORK_DIR}/$(basename -- "${BASE_IMAGE_URL}")"
WORKING_IMAGE_PATH="${RUN_WORK_DIR}/${IMAGE_BASENAME}.qcow2"
ROOTFS_PATH="${RUN_WORK_DIR}/${IMAGE_BASENAME}-rootfs.raw"
MANIFEST_PATH="${RUN_WORK_DIR}/${IMAGE_BASENAME}-rootfs.manifest"
WHOLE_DISK_RAW_PATH="${RUN_WORK_DIR}/${IMAGE_BASENAME}-disk.raw"
FINAL_ROOTFS_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}-rootfs.raw"
FINAL_MANIFEST_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}-rootfs.manifest"
K3S_CACHE_PATH="${K3S_CACHE_DIR}/${K3S_VERSION}-${K3S_ASSET}"
ROOT_DEVICE_HINT="${KALISTO_FIRECRACKER_ROOT_DEVICE_HINT:-/dev/vda}"

cleanup() {
  chmod -R u+w "${RUN_WORK_DIR}" 2>/dev/null || true
  rm -rf "${RUN_WORK_DIR}"
}
trap cleanup EXIT INT TERM

download_atomic() {
  url="$1"
  destination="$2"
  tmp_path="${destination}.download.$$"
  rm -f "${tmp_path}"
  curl -fL "${url}" -o "${tmp_path}"
  mv "${tmp_path}" "${destination}"
}

if [ -f "${BASE_IMAGE_PATH}" ] && ! qemu-img info "${BASE_IMAGE_PATH}" >/dev/null 2>&1; then
  echo "cached base image is invalid, removing ${BASE_IMAGE_PATH}"
  rm -f "${BASE_IMAGE_PATH}"
fi

if [ ! -f "${BASE_IMAGE_PATH}" ]; then
  echo "downloading base image to ${BASE_IMAGE_PATH}"
  download_atomic "${BASE_IMAGE_URL}" "${BASE_IMAGE_PATH}"
else
  echo "reusing cached base image at ${BASE_IMAGE_PATH}"
fi

cp "${BASE_IMAGE_PATH}" "${WORKING_IMAGE_PATH}"

if [ ! -f "${K3S_CACHE_PATH}" ]; then
  echo "downloading k3s asset ${K3S_ASSET} to ${K3S_CACHE_PATH}"
  download_atomic "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/${K3S_ASSET}" "${K3S_CACHE_PATH}"
else
  echo "reusing cached k3s asset at ${K3S_CACHE_PATH}"
fi
chmod 0755 "${K3S_CACHE_PATH}"

export LIBGUESTFS_BACKEND
export LIBGUESTFS_TMPDIR
export LIBGUESTFS_CACHEDIR
export TMPDIR="${LIBGUESTFS_TMPDIR}"
export XDG_RUNTIME_DIR

virt-customize \
  -a "${WORKING_IMAGE_PATH}" \
  --mkdir /usr/local/lib/kalisto \
  --mkdir /usr/local/lib/kalisto/cache \
  --upload "${PROVISION_SCRIPT}:/usr/local/lib/kalisto/provision_kalisto_node.sh" \
  --upload "${K3S_CACHE_PATH}:/usr/local/lib/kalisto/cache/k3s" \
  --chmod 0755:/usr/local/lib/kalisto/provision_kalisto_node.sh \
  --chmod 0755:/usr/local/lib/kalisto/cache/k3s \
  --write /etc/fstab:"LABEL=cloudimg-rootfs / ext4 discard,commit=30,errors=remount-ro 0 1" \
  --run-command "export K3S_VERSION='${K3S_VERSION}'; /usr/local/lib/kalisto/provision_kalisto_node.sh" \
  --run-command "cloud-init clean --logs --seed || true" \
  --truncate /etc/machine-id \
  --delete /var/lib/dbus/machine-id

if command -v virt-sparsify >/dev/null 2>&1; then
  virt-sparsify --in-place "${WORKING_IMAGE_PATH}"
fi

rm -f "${ROOTFS_PATH}"
rm -f "${WHOLE_DISK_RAW_PATH}"
qemu-img convert -f qcow2 -O raw "${WORKING_IMAGE_PATH}" "${WHOLE_DISK_RAW_PATH}"

ROOTFS_SOURCE_PARTITION_NUMBER="${ROOTFS_SOURCE_PARTITION#/dev/sd[a-z]}"
PARTITION_METADATA="$(
  parted -sm "${WHOLE_DISK_RAW_PATH}" unit s print \
    | awk -F: -v partition="${ROOTFS_SOURCE_PARTITION_NUMBER}" '$1 == partition { print $2 ":" $4; exit }'
)"
if [ -z "${PARTITION_METADATA}" ]; then
  echo "unable to locate source partition ${ROOTFS_SOURCE_PARTITION} in ${WHOLE_DISK_RAW_PATH}" >&2
  exit 1
fi
PARTITION_START_SECTORS="${PARTITION_METADATA%%:*}"
PARTITION_SIZE_SECTORS="${PARTITION_METADATA##*:}"
PARTITION_START_SECTORS="${PARTITION_START_SECTORS%s}"
PARTITION_SIZE_SECTORS="${PARTITION_SIZE_SECTORS%s}"

dd \
  if="${WHOLE_DISK_RAW_PATH}" \
  of="${ROOTFS_PATH}" \
  bs=512 \
  skip="${PARTITION_START_SECTORS}" \
  count="${PARTITION_SIZE_SECTORS}" \
  status=none
rm -f "${WHOLE_DISK_RAW_PATH}"
run_e2fsck "${ROOTFS_PATH}"

CURRENT_ROOTFS_SIZE_BYTES="$(wc -c < "${ROOTFS_PATH}" | tr -d ' ')"
if [ "${ROOTFS_TARGET_SIZE_BYTES}" -gt "${CURRENT_ROOTFS_SIZE_BYTES}" ]; then
  echo "expanding rootfs to ${ROOTFS_TARGET_SIZE_MIB}MiB"
  truncate -s "${ROOTFS_TARGET_SIZE_BYTES}" "${ROOTFS_PATH}"
  run_e2fsck "${ROOTFS_PATH}"
  resize2fs "${ROOTFS_PATH}" >/dev/null
  run_e2fsck "${ROOTFS_PATH}"
fi

if command -v sha256sum >/dev/null 2>&1; then
  ROOTFS_SHA256="$(sha256sum "${ROOTFS_PATH}" | awk '{print $1}')"
else
  ROOTFS_SHA256="$(shasum -a 256 "${ROOTFS_PATH}" | awk '{print $1}')"
fi
ROOTFS_APPARENT_SIZE_BYTES="$(wc -c < "${ROOTFS_PATH}" | tr -d ' ')"
ROOTFS_DISK_USAGE="$(du -h "${ROOTFS_PATH}" | awk '{print $1}')"
ROOTFS_VIRTUAL_SIZE="$(
  qemu-img info --output=json "${ROOTFS_PATH}" \
    | tr -d '\n' \
    | sed -n 's/.*"virtual-size":[[:space:]]*\([0-9][0-9]*\).*/\1/p'
)"

ROOTFS_ARCHIVE_PATH=
ROOTFS_ARCHIVE_SHA256=
ROOTFS_ARCHIVE_DISK_USAGE=
FINAL_ROOTFS_ARCHIVE_PATH=
if [ "${ROOTFS_ARCHIVE_FORMAT}" != "none" ]; then
  ROOTFS_ARCHIVE_PATH="${ROOTFS_PATH}.${ROOTFS_ARCHIVE_FORMAT}"
  FINAL_ROOTFS_ARCHIVE_PATH="${FINAL_ROOTFS_PATH}.${ROOTFS_ARCHIVE_FORMAT}"
  rm -f "${ROOTFS_ARCHIVE_PATH}"
  case "${ROOTFS_ARCHIVE_FORMAT}" in
    xz)
      xz "-${ROOTFS_ARCHIVE_LEVEL}" -T0 -k -f "${ROOTFS_PATH}"
      ;;
    gz)
      gzip "-${ROOTFS_ARCHIVE_LEVEL}" -c "${ROOTFS_PATH}" > "${ROOTFS_ARCHIVE_PATH}"
      ;;
    zst)
      zstd "-${ROOTFS_ARCHIVE_LEVEL}" -f -q "${ROOTFS_PATH}" -o "${ROOTFS_ARCHIVE_PATH}"
      ;;
  esac
  if command -v sha256sum >/dev/null 2>&1; then
    ROOTFS_ARCHIVE_SHA256="$(sha256sum "${ROOTFS_ARCHIVE_PATH}" | awk '{print $1}')"
  else
    ROOTFS_ARCHIVE_SHA256="$(shasum -a 256 "${ROOTFS_ARCHIVE_PATH}" | awk '{print $1}')"
  fi
  ROOTFS_ARCHIVE_DISK_USAGE="$(du -h "${ROOTFS_ARCHIVE_PATH}" | awk '{print $1}')"
fi

rm -f "${FINAL_ROOTFS_PATH}" "${FINAL_MANIFEST_PATH}"
mv "${ROOTFS_PATH}" "${FINAL_ROOTFS_PATH}"
if [ -n "${ROOTFS_ARCHIVE_PATH}" ]; then
  rm -f "${FINAL_ROOTFS_ARCHIVE_PATH}"
  mv "${ROOTFS_ARCHIVE_PATH}" "${FINAL_ROOTFS_ARCHIVE_PATH}"
  ROOTFS_ARCHIVE_PATH="${FINAL_ROOTFS_ARCHIVE_PATH}"
fi

cat > "${MANIFEST_PATH}" <<EOF
IMAGE_BASENAME=${IMAGE_BASENAME}
FIRECRACKER_VERSION=${KALISTO_FIRECRACKER_VERSION_NORMALIZED}
FIRECRACKER_ARTIFACT_TRACK=${KALISTO_FIRECRACKER_ARTIFACT_TRACK}
FIRECRACKER_ARCH=${KALISTO_FIRECRACKER_ARTIFACT_ARCH}
BASE_IMAGE_URL=${BASE_IMAGE_URL}
K3S_VERSION=${K3S_VERSION}
ROOTFS_PATH=${ROOTFS_PATH}
ROOTFS_SHA256=${ROOTFS_SHA256}
ROOTFS_APPARENT_SIZE_BYTES=${ROOTFS_APPARENT_SIZE_BYTES}
ROOTFS_DISK_USAGE=${ROOTFS_DISK_USAGE}
ROOTFS_VIRTUAL_SIZE=${ROOTFS_VIRTUAL_SIZE}
ROOTFS_TARGET_SIZE_MIB=${ROOTFS_TARGET_SIZE_MIB}
ROOT_DEVICE_HINT=${ROOT_DEVICE_HINT}
ROOTFS_SOURCE_PARTITION=${ROOTFS_SOURCE_PARTITION}
LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND}
LIBGUESTFS_TMPDIR=${LIBGUESTFS_TMPDIR}
LIBGUESTFS_CACHEDIR=${LIBGUESTFS_CACHEDIR}
XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}
K3S_ASSET=${K3S_ASSET}
K3S_CACHE_PATH=${K3S_CACHE_PATH}
WORKING_IMAGE_PATH=${WORKING_IMAGE_PATH}
ROOTFS_ARCHIVE_FORMAT=${ROOTFS_ARCHIVE_FORMAT}
ROOTFS_ARCHIVE_PATH=${ROOTFS_ARCHIVE_PATH}
ROOTFS_ARCHIVE_SHA256=${ROOTFS_ARCHIVE_SHA256}
ROOTFS_ARCHIVE_DISK_USAGE=${ROOTFS_ARCHIVE_DISK_USAGE}
EOF

mv "${MANIFEST_PATH}" "${FINAL_MANIFEST_PATH}"

echo "created ${FINAL_ROOTFS_PATH}"
if [ -n "${ROOTFS_ARCHIVE_PATH}" ]; then
  echo "created ${ROOTFS_ARCHIVE_PATH}"
fi
echo "wrote ${FINAL_MANIFEST_PATH}"

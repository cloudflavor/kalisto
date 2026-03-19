#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
PROVISION_SCRIPT="${KALISTO_IMAGE_PROVISION_SCRIPT:-${ROOT_DIR}/scripts/macos/provision_lima_node.sh}"
OUTPUT_DIR="${KALISTO_IMAGE_OUTPUT_DIR:-${ROOT_DIR}/artifacts/lima-node}"
WORK_DIR="${KALISTO_IMAGE_WORK_DIR:-${ROOT_DIR}/artifacts/lima-node/work}"
K3S_CACHE_DIR="${KALISTO_K3S_CACHE_DIR:-${ROOT_DIR}/artifacts/lima-node/cache/k3s}"
BASE_IMAGE_URL="${KALISTO_BASE_IMAGE_URL:-https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-arm64.img}"
IMAGE_BASENAME="${KALISTO_IMAGE_BASENAME:-ubuntu-24.04-minimal-k3s-arm64}"
K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"
K3S_ASSET="${KALISTO_K3S_ASSET:-k3s-arm64}"
LIBGUESTFS_BACKEND="${LIBGUESTFS_BACKEND:-direct}"

if [ ! -f "${PROVISION_SCRIPT}" ]; then
  echo "missing provision script: ${PROVISION_SCRIPT}" >&2
  exit 1
fi

for REQUIRED in curl cp qemu-img virt-customize; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done

mkdir -p "${OUTPUT_DIR}" "${WORK_DIR}" "${K3S_CACHE_DIR}"

BASE_IMAGE_PATH="${WORK_DIR}/$(basename -- "${BASE_IMAGE_URL}")"
IMAGE_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}.qcow2"
MANIFEST_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}.manifest"
FIRSTBOOT_SCRIPT="${WORK_DIR}/provision_lima_node.firstboot.sh"
K3S_CACHE_PATH="${K3S_CACHE_DIR}/${K3S_VERSION}-${K3S_ASSET}"

if [ ! -f "${BASE_IMAGE_PATH}" ]; then
  echo "downloading base image to ${BASE_IMAGE_PATH}"
  curl -fL "${BASE_IMAGE_URL}" -o "${BASE_IMAGE_PATH}"
else
  echo "reusing cached base image at ${BASE_IMAGE_PATH}"
fi

cp "${BASE_IMAGE_PATH}" "${IMAGE_PATH}"

if [ ! -f "${K3S_CACHE_PATH}" ]; then
  echo "downloading k3s asset ${K3S_ASSET} to ${K3S_CACHE_PATH}"
  curl -fL "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/${K3S_ASSET}" -o "${K3S_CACHE_PATH}"
else
  echo "reusing cached k3s asset at ${K3S_CACHE_PATH}"
fi
chmod 0755 "${K3S_CACHE_PATH}"

export LIBGUESTFS_BACKEND

cat > "${FIRSTBOOT_SCRIPT}" <<EOF
#!/bin/sh
set -eu
export K3S_VERSION='${K3S_VERSION}'
/usr/local/lib/kalisto/provision_lima_node.sh
EOF
chmod 0755 "${FIRSTBOOT_SCRIPT}"

virt-customize \
  -a "${IMAGE_PATH}" \
  --mkdir /usr/local/lib/kalisto \
  --mkdir /usr/local/lib/kalisto/cache \
  --upload "${PROVISION_SCRIPT}:/usr/local/lib/kalisto/provision_lima_node.sh" \
  --upload "${K3S_CACHE_PATH}:/usr/local/lib/kalisto/cache/k3s" \
  --chmod 0755:/usr/local/lib/kalisto/provision_lima_node.sh \
  --chmod 0755:/usr/local/lib/kalisto/cache/k3s \
  --firstboot "${FIRSTBOOT_SCRIPT}" \
  --truncate /etc/machine-id \
  --delete /var/lib/dbus/machine-id

if command -v virt-sparsify >/dev/null 2>&1; then
  virt-sparsify --in-place "${IMAGE_PATH}"
fi

IMAGE_SHA256="$(sha256sum "${IMAGE_PATH}" | awk '{print $1}')"
IMAGE_VIRTUAL_SIZE="$(
  qemu-img info --output=json "${IMAGE_PATH}" \
    | tr -d '\n' \
    | sed -n 's/.*"virtual-size":[[:space:]]*\([0-9][0-9]*\).*/\1/p'
)"

cat > "${MANIFEST_PATH}" <<EOF
IMAGE_BASENAME=${IMAGE_BASENAME}
BASE_IMAGE_URL=${BASE_IMAGE_URL}
K3S_VERSION=${K3S_VERSION}
IMAGE_PATH=${IMAGE_PATH}
IMAGE_SHA256=${IMAGE_SHA256}
IMAGE_VIRTUAL_SIZE=${IMAGE_VIRTUAL_SIZE}
LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND}
K3S_ASSET=${K3S_ASSET}
K3S_CACHE_PATH=${K3S_CACHE_PATH}
EOF

echo "created ${IMAGE_PATH}"
echo "wrote ${MANIFEST_PATH}"

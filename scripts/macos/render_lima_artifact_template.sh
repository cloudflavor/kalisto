#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
IMAGE_PATH="${1:-${ROOT_DIR}/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.qcow2}"
OUTPUT_PATH="${2:-${ROOT_DIR}/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.yaml}"
TEMPLATE_ARCH="${KALISTO_LIMA_TEMPLATE_ARCH:-}"
if [ ! -f "${IMAGE_PATH}" ]; then
  echo "missing image artifact: ${IMAGE_PATH}" >&2
  exit 1
fi

if [ -z "${TEMPLATE_ARCH}" ]; then
  case "$(basename -- "${IMAGE_PATH}")" in
    *-amd64.*)
      TEMPLATE_ARCH="x86_64"
      ;;
    *)
      TEMPLATE_ARCH="aarch64"
      ;;
  esac
fi

mkdir -p "$(dirname -- "${OUTPUT_PATH}")"

cat > "${OUTPUT_PATH}" <<EOF
vmType: "vz"
mountType: "virtiofs"
mounts: []
containerd:
  system: false
  user: false
cpus: 2
memory: "4GiB"
disk: "12GiB"
images:
  - location: "${IMAGE_PATH}"
    arch: "${TEMPLATE_ARCH}"
EOF

echo "wrote ${OUTPUT_PATH}"

#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
RENDER_SCRIPT="${ROOT_DIR}/scripts/macos/render_lima_artifact_template.sh"
INPUT_IMAGE_PATH="${1:-${ROOT_DIR}/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64.qcow2}"
OUTPUT_IMAGE_PATH="${2:-${ROOT_DIR}/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.img}"
OUTPUT_TEMPLATE_PATH="${3:-${ROOT_DIR}/artifacts/lima-node/ubuntu-24.04-minimal-k3s-arm64-finalized.yaml}"
OUTPUT_MANIFEST_PATH="${OUTPUT_IMAGE_PATH%.*}.manifest"
WORK_DIR="${KALISTO_FINALIZE_WORK_DIR:-${ROOT_DIR}/target/finalize-lima-image}"
INSTANCE_NAME="${KALISTO_FINALIZE_INSTANCE_NAME:-kalisto-image-finalize-$(date +%s)}"
WAIT_TIMEOUT_SECONDS="${KALISTO_FINALIZE_TIMEOUT_SECONDS:-600}"
NETWORK="${KALISTO_FINALIZE_NETWORK:-vzNAT}"
ROSETTA_ENABLED="${ROSETTA_ENABLED:-true}"

if [ ! -f "${INPUT_IMAGE_PATH}" ]; then
  echo "missing seed image: ${INPUT_IMAGE_PATH}" >&2
  exit 1
fi

if [ ! -f "${RENDER_SCRIPT}" ]; then
  echo "missing render script: ${RENDER_SCRIPT}" >&2
  exit 1
fi

for REQUIRED in limactl cp du wc awk tr; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

mkdir -p "${WORK_DIR}" "$(dirname -- "${OUTPUT_IMAGE_PATH}")" "$(dirname -- "${OUTPUT_TEMPLATE_PATH}")"
TEMP_TEMPLATE_PATH="${WORK_DIR}/${INSTANCE_NAME}.yaml"

cleanup() {
  if limactl list --format '{{ .Name }}' "${INSTANCE_NAME}" >/dev/null 2>&1; then
    limactl stop --tty=false --force "${INSTANCE_NAME}" >/dev/null 2>&1 || true
    limactl delete --tty=false --force "${INSTANCE_NAME}" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT INT TERM

ROSETTA_ENABLED="${ROSETTA_ENABLED}" sh "${RENDER_SCRIPT}" "${INPUT_IMAGE_PATH}" "${TEMP_TEMPLATE_PATH}"

echo "starting temporary Lima instance ${INSTANCE_NAME} from ${INPUT_IMAGE_PATH}"
limactl start \
  --tty=false \
  --name="${INSTANCE_NAME}" \
  --network="${NETWORK}" \
  --timeout=15m \
  "${TEMP_TEMPLATE_PATH}"

deadline=$(( $(date +%s) + WAIT_TIMEOUT_SECONDS ))
while :; do
  if limactl shell --tty=false "${INSTANCE_NAME}" sudo sh -lc "test -f /etc/kalisto/node-image.env && test -x /usr/local/bin/k3s && /usr/local/bin/k3s --version >/dev/null 2>&1" >/dev/null 2>&1; then
    break
  fi
  if [ "$(date +%s)" -ge "${deadline}" ]; then
    echo "timed out waiting for finalized image marker in ${INSTANCE_NAME}" >&2
    exit 1
  fi
  sleep 5
done

INSTANCE_DIR="$(limactl list --format '{{ .Dir }}' "${INSTANCE_NAME}" | tr -d '\r')"
if [ -z "${INSTANCE_DIR}" ]; then
  echo "failed to resolve Lima instance directory for ${INSTANCE_NAME}" >&2
  exit 1
fi

SOURCE_DISK_PATH="${INSTANCE_DIR}/disk"
if [ ! -f "${SOURCE_DISK_PATH}" ]; then
  echo "missing finalized disk at ${SOURCE_DISK_PATH}" >&2
  exit 1
fi

echo "stopping temporary Lima instance ${INSTANCE_NAME}"
limactl shell --tty=false "${INSTANCE_NAME}" sudo sync >/dev/null 2>&1 || true
limactl stop --tty=false "${INSTANCE_NAME}" || limactl stop --tty=false --force "${INSTANCE_NAME}"

rm -f "${OUTPUT_IMAGE_PATH}" "${OUTPUT_MANIFEST_PATH}" "${OUTPUT_TEMPLATE_PATH}"

echo "exporting finalized disk to ${OUTPUT_IMAGE_PATH}"
if cp -c "${SOURCE_DISK_PATH}" "${OUTPUT_IMAGE_PATH}" 2>/dev/null; then
  :
else
  cp "${SOURCE_DISK_PATH}" "${OUTPUT_IMAGE_PATH}"
fi

ROSETTA_ENABLED="${ROSETTA_ENABLED}" sh "${RENDER_SCRIPT}" "${OUTPUT_IMAGE_PATH}" "${OUTPUT_TEMPLATE_PATH}"

if command -v sha256sum >/dev/null 2>&1; then
  IMAGE_SHA256="$(sha256sum "${OUTPUT_IMAGE_PATH}" | awk '{print $1}')"
else
  IMAGE_SHA256="$(shasum -a 256 "${OUTPUT_IMAGE_PATH}" | awk '{print $1}')"
fi
IMAGE_APPARENT_SIZE_BYTES="$(wc -c < "${OUTPUT_IMAGE_PATH}" | tr -d ' ')"
IMAGE_DISK_USAGE="$(du -h "${OUTPUT_IMAGE_PATH}" | awk '{print $1}')"

cat > "${OUTPUT_MANIFEST_PATH}" <<EOF
SEED_IMAGE_PATH=${INPUT_IMAGE_PATH}
FINALIZED_IMAGE_PATH=${OUTPUT_IMAGE_PATH}
FINALIZED_TEMPLATE_PATH=${OUTPUT_TEMPLATE_PATH}
LIMA_INSTANCE_NAME=${INSTANCE_NAME}
LIMA_INSTANCE_DIR=${INSTANCE_DIR}
NETWORK=${NETWORK}
ROSETTA_ENABLED=${ROSETTA_ENABLED}
IMAGE_SHA256=${IMAGE_SHA256}
IMAGE_APPARENT_SIZE_BYTES=${IMAGE_APPARENT_SIZE_BYTES}
IMAGE_DISK_USAGE=${IMAGE_DISK_USAGE}
EOF

echo "created ${OUTPUT_IMAGE_PATH}"
echo "wrote ${OUTPUT_TEMPLATE_PATH}"
echo "wrote ${OUTPUT_MANIFEST_PATH}"

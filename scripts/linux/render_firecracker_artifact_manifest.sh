#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
. "${ROOT_DIR}/scripts/linux/firecracker_artifact_context.sh"
kalisto_detect_firecracker_context

OUTPUT_DIR="${KALISTO_FIRECRACKER_OUTPUT_DIR:-${ROOT_DIR}/artifacts/firecracker-node/${KALISTO_FIRECRACKER_ARTIFACT_TRACK}/${KALISTO_FIRECRACKER_ARTIFACT_ARCH}}"
IMAGE_BASENAME="${KALISTO_IMAGE_BASENAME:-ubuntu-24.04-minimal-k3s-amd64}"
ROOTFS_PATH="${KALISTO_FIRECRACKER_ROOTFS_PATH:-${OUTPUT_DIR}/${IMAGE_BASENAME}-rootfs.raw}"
ROOTFS_ARCHIVE_PATH="${KALISTO_FIRECRACKER_ROOTFS_ARCHIVE_PATH:-${ROOTFS_PATH}.xz}"
MANIFEST_PATH="${KALISTO_FIRECRACKER_ARTIFACT_MANIFEST_PATH:-${OUTPUT_DIR}/firecracker-artifacts.manifest}"
ROOTFS_MANIFEST_PATH="${KALISTO_FIRECRACKER_ROOTFS_MANIFEST_PATH:-${OUTPUT_DIR}/${IMAGE_BASENAME}-rootfs.manifest}"
ARTIFACT_BASE_URL="${KALISTO_FIRECRACKER_ARTIFACT_BASE_URL:-}"
KERNEL_URL="${KALISTO_FIRECRACKER_KERNEL_URL:-}"
KERNEL_PATH="${KALISTO_FIRECRACKER_KERNEL_PATH:-}"
KERNEL_FILENAME="${KALISTO_FIRECRACKER_KERNEL_FILENAME:-kalisto-vmlinux}"

if [ -z "${KERNEL_PATH}" ]; then
  if [ -f "${OUTPUT_DIR}/${KERNEL_FILENAME}" ]; then
    KERNEL_PATH="${OUTPUT_DIR}/${KERNEL_FILENAME}"
  else
    for CANDIDATE in "${OUTPUT_DIR}"/vmlinux-*; do
      if [ -f "${CANDIDATE}" ]; then
        KERNEL_PATH="${CANDIDATE}"
      fi
    done
  fi
fi

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

ROOTFS_FILENAME="$(basename -- "${ROOTFS_PATH}")"
ROOTFS_SHA256=
if [ -f "${ROOTFS_PATH}" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    ROOTFS_SHA256="$(sha256sum "${ROOTFS_PATH}" | awk '{print $1}')"
  else
    ROOTFS_SHA256="$(shasum -a 256 "${ROOTFS_PATH}" | awk '{print $1}')"
  fi
fi

if [ -z "${ROOTFS_SHA256}" ] && [ -f "${ROOTFS_MANIFEST_PATH}" ]; then
  ROOTFS_SHA256="$(sed -n 's/^ROOTFS_SHA256=//p' "${ROOTFS_MANIFEST_PATH}" | tail -n 1)"
  ROOTFS_FILENAME_FROM_MANIFEST="$(sed -n 's/^ROOTFS_PATH=//p' "${ROOTFS_MANIFEST_PATH}" | tail -n 1 | sed 's#.*/##')"
  if [ -n "${ROOTFS_FILENAME_FROM_MANIFEST}" ]; then
    ROOTFS_FILENAME="${ROOTFS_FILENAME_FROM_MANIFEST}"
  fi
fi

if [ -z "${ROOTFS_SHA256}" ]; then
  echo "missing rootfs checksum: provide ${ROOTFS_PATH} or ROOTFS_SHA256 in ${ROOTFS_MANIFEST_PATH}" >&2
  exit 1
fi

KERNEL_SHA256=
if [ -n "${KERNEL_PATH}" ] && [ -f "${KERNEL_PATH}" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    KERNEL_SHA256="$(sha256sum "${KERNEL_PATH}" | awk '{print $1}')"
  else
    KERNEL_SHA256="$(shasum -a 256 "${KERNEL_PATH}" | awk '{print $1}')"
  fi
fi

K3S_VERSION=
if [ -f "${ROOTFS_MANIFEST_PATH}" ]; then
  K3S_VERSION="$(sed -n 's/^K3S_VERSION=//p' "${ROOTFS_MANIFEST_PATH}" | tail -n 1)"
fi

ROOTFS_ARCHIVE_FILENAME=
ROOTFS_ARCHIVE_SHA256=
if [ -f "${ROOTFS_ARCHIVE_PATH}" ]; then
  ROOTFS_ARCHIVE_FILENAME="$(basename -- "${ROOTFS_ARCHIVE_PATH}")"
  if command -v sha256sum >/dev/null 2>&1; then
    ROOTFS_ARCHIVE_SHA256="$(sha256sum "${ROOTFS_ARCHIVE_PATH}" | awk '{print $1}')"
  else
    ROOTFS_ARCHIVE_SHA256="$(shasum -a 256 "${ROOTFS_ARCHIVE_PATH}" | awk '{print $1}')"
  fi
fi

mkdir -p "$(dirname -- "${MANIFEST_PATH}")"
{
  echo "FIRECRACKER_VERSION=${KALISTO_FIRECRACKER_VERSION_NORMALIZED}"
  echo "FIRECRACKER_ARTIFACT_TRACK=${KALISTO_FIRECRACKER_ARTIFACT_TRACK}"
  echo "FIRECRACKER_ARCH=${KALISTO_FIRECRACKER_ARTIFACT_ARCH}"
  echo "IMAGE_BASENAME=${IMAGE_BASENAME}"
  if [ -n "${K3S_VERSION}" ]; then
    echo "K3S_VERSION=${K3S_VERSION}"
  fi
  if [ -n "${KERNEL_PATH}" ] && [ -f "${KERNEL_PATH}" ]; then
    echo "KERNEL_FILENAME=$(basename -- "${KERNEL_PATH}")"
  fi
  if [ -n "${KERNEL_SHA256}" ]; then
    echo "KERNEL_SHA256=${KERNEL_SHA256}"
  fi
  echo "ROOTFS_FILENAME=${ROOTFS_FILENAME}"
  echo "ROOTFS_SHA256=${ROOTFS_SHA256}"
  if [ -n "${ROOTFS_ARCHIVE_FILENAME}" ]; then
    echo "ROOTFS_ARCHIVE_FILENAME=${ROOTFS_ARCHIVE_FILENAME}"
  fi
  if [ -n "${ROOTFS_ARCHIVE_SHA256}" ]; then
    echo "ROOTFS_ARCHIVE_SHA256=${ROOTFS_ARCHIVE_SHA256}"
  fi
  if [ -n "${KERNEL_URL}" ]; then
    echo "KERNEL_URL=${KERNEL_URL}"
  elif [ -n "${ARTIFACT_BASE_URL}" ] && [ -n "${KERNEL_PATH}" ] && [ -f "${KERNEL_PATH}" ]; then
    echo "KERNEL_URL=${ARTIFACT_BASE_URL}/$(basename -- "${KERNEL_PATH}")"
  fi
  if [ -n "${ARTIFACT_BASE_URL}" ]; then
    if [ -n "${ROOTFS_ARCHIVE_FILENAME}" ]; then
      echo "ROOTFS_URL=${ARTIFACT_BASE_URL}/${ROOTFS_ARCHIVE_FILENAME}"
    else
      echo "ROOTFS_URL=${ARTIFACT_BASE_URL}/${ROOTFS_FILENAME}"
    fi
  fi
} > "${MANIFEST_PATH}"

echo "wrote ${MANIFEST_PATH}"

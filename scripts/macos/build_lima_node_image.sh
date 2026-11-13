#!/bin/sh
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
PROVISION_SCRIPT="${KALISTO_IMAGE_PROVISION_SCRIPT:-${ROOT_DIR}/scripts/image/provision_kalisto_node.sh}"
ROOTFS_BUILD_SCRIPT="${KALISTO_ROOTFS_BUILD_SCRIPT:-${ROOT_DIR}/scripts/image/run_buildkit_container_build.sh}"
OUTPUT_DIR="${KALISTO_IMAGE_OUTPUT_DIR:-${ROOT_DIR}/artifacts/lima-node}"
WORK_DIR="${KALISTO_IMAGE_WORK_DIR:-${ROOT_DIR}/artifacts/lima-node/work}"
SCRATCH_DIR="${KALISTO_IMAGE_SCRATCH_DIR:-${WORK_DIR}}"
K3S_CACHE_DIR="${KALISTO_K3S_CACHE_DIR:-${ROOT_DIR}/artifacts/lima-node/cache/k3s}"
ROOTFS_DIR="${SCRATCH_DIR}/rootfs"
RAW_IMAGE_PATH="${SCRATCH_DIR}/rootfs.raw"
ROOTFS_FS_PATH="${SCRATCH_DIR}/rootfs.ext4"
EFI_FS_PATH="${SCRATCH_DIR}/efi.img"
PREBUILT_ROOTFS_DIR="${KALISTO_PREBUILT_ROOTFS_DIR:-}"
PREBUILT_ROOTFS_ARCHIVE="${KALISTO_PREBUILT_ROOTFS_ARCHIVE:-}"
UBUNTU_ARCH="${KALISTO_UBUNTU_ARCH:-arm64}"

case "${UBUNTU_ARCH}" in
  arm64)
    IMAGE_ARCH_SUFFIX="arm64"
    TEMPLATE_ARCH="aarch64"
    DEFAULT_K3S_ASSET="k3s-arm64"
    DEFAULT_UBUNTU_MIRROR="http://ports.ubuntu.com/ubuntu-ports"
    GRUB_EFI_TARGET="arm64-efi"
    EFI_BOOT_FILENAME="BOOTAA64.EFI"
    ;;
  amd64)
    IMAGE_ARCH_SUFFIX="amd64"
    TEMPLATE_ARCH="x86_64"
    DEFAULT_K3S_ASSET="k3s"
    DEFAULT_UBUNTU_MIRROR="http://archive.ubuntu.com/ubuntu"
    GRUB_EFI_TARGET="x86_64-efi"
    EFI_BOOT_FILENAME="BOOTX64.EFI"
    ;;
  *)
    echo "unsupported Ubuntu target architecture: ${UBUNTU_ARCH}" >&2
    exit 1
    ;;
esac

IMAGE_BASENAME="${KALISTO_IMAGE_BASENAME:-ubuntu-24.04-minimal-k3s-${IMAGE_ARCH_SUFFIX}}"
K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"
K3S_ASSET="${KALISTO_K3S_ASSET:-${DEFAULT_K3S_ASSET}}"
LIBGUESTFS_BACKEND="${LIBGUESTFS_BACKEND:-direct}"
IMAGE_SIZE="${KALISTO_IMAGE_SIZE:-12G}"

if [ ! -f "${PROVISION_SCRIPT}" ]; then
  echo "missing provision script: ${PROVISION_SCRIPT}" >&2
  exit 1
fi
if [ ! -f "${ROOTFS_BUILD_SCRIPT}" ]; then
  echo "missing rootfs build script: ${ROOTFS_BUILD_SCRIPT}" >&2
  exit 1
fi

for REQUIRED in chroot dd grub-mkstandalone mcopy mmd mkfs.ext4 mkfs.vfat parted qemu-img truncate; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done

cleanup() {
  rm -rf "${SCRATCH_DIR}"
}

trap cleanup EXIT INT TERM

mkdir -p "${OUTPUT_DIR}" "${WORK_DIR}" "${K3S_CACHE_DIR}" "${SCRATCH_DIR}"

IMAGE_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}.qcow2"
MANIFEST_PATH="${OUTPUT_DIR}/${IMAGE_BASENAME}.manifest"
K3S_CACHE_PATH="${K3S_CACHE_DIR}/${K3S_VERSION}-${K3S_ASSET}"

if [ ! -f "${K3S_CACHE_PATH}" ]; then
  echo "downloading k3s asset ${K3S_ASSET} to ${K3S_CACHE_PATH}"
  curl -fL "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/${K3S_ASSET}" -o "${K3S_CACHE_PATH}"
else
  echo "reusing cached k3s asset at ${K3S_CACHE_PATH}"
fi
chmod 0755 "${K3S_CACHE_PATH}"

rm -rf "${SCRATCH_DIR}" "${IMAGE_PATH}"
mkdir -p "${SCRATCH_DIR}"

ROOTFS_SOURCE_DIR="${PREBUILT_ROOTFS_DIR}"
if [ -n "${PREBUILT_ROOTFS_ARCHIVE}" ]; then
  if [ ! -f "${PREBUILT_ROOTFS_ARCHIVE}" ]; then
    echo "missing prebuilt rootfs archive: ${PREBUILT_ROOTFS_ARCHIVE}" >&2
    exit 1
  fi
  tar -C "${SCRATCH_DIR}" -xf "${PREBUILT_ROOTFS_ARCHIVE}"
  if [ ! -d "${ROOTFS_DIR}" ]; then
    echo "prebuilt rootfs archive did not unpack to ${ROOTFS_DIR}" >&2
    exit 1
  fi
  ROOTFS_SOURCE_DIR="${ROOTFS_DIR}"
elif [ -z "${ROOTFS_SOURCE_DIR}" ]; then
  export KALISTO_IMAGE_PROVISION_SCRIPT="${PROVISION_SCRIPT}"
  export KALISTO_K3S_CACHE_PATH="${K3S_CACHE_PATH}"
  export KALISTO_UBUNTU_ARCH="${UBUNTU_ARCH}"
  sh "${ROOTFS_BUILD_SCRIPT}" "${ROOTFS_DIR}"
  ROOTFS_SOURCE_DIR="${ROOTFS_DIR}"
elif [ ! -d "${ROOTFS_SOURCE_DIR}" ]; then
  echo "missing prebuilt rootfs directory: ${ROOTFS_SOURCE_DIR}" >&2
  exit 1
else
  cp -R "${ROOTFS_SOURCE_DIR}" "${ROOTFS_DIR}"
  ROOTFS_SOURCE_DIR="${ROOTFS_DIR}"
fi

for REQUIRED_GUEST_TOOL in dbus-daemon rsync lsb_release resolvectl; do
  if [ ! -x "${ROOTFS_SOURCE_DIR}/usr/bin/${REQUIRED_GUEST_TOOL}" ]; then
    echo "node rootfs is missing ${REQUIRED_GUEST_TOOL}; rebuild the rootfs before assembling the node image" >&2
    exit 1
  fi
done

# BuildKit owns /etc/resolv.conf during RUN. Install the guest resolver link
# only after rootfs export, before populating the ext4 image.
ln -sfn /run/systemd/resolve/stub-resolv.conf "${ROOTFS_SOURCE_DIR}/etc/resolv.conf"
rm -f "${ROOTFS_SOURCE_DIR}/etc/cloud/cloud.cfg.d/99_kalisto_network.cfg" \
  "${ROOTFS_SOURCE_DIR}/etc/systemd/network/10-kalisto-dhcp.network" \
  "${ROOTFS_SOURCE_DIR}/etc/netplan/10-kalisto-dhcp.yaml" \
  "${ROOTFS_SOURCE_DIR}/etc/systemd/system/getty.target.wants/serial-getty@ttyAMA0.service"

SPECIAL_PERMISSIONS_MANIFEST="${ROOTFS_SOURCE_DIR}/etc/kalisto/rootfs-special-permissions.manifest"
if [ -f "${SPECIAL_PERMISSIONS_MANIFEST}" ]; then
  while read -r MODE OWNER GROUP ABSOLUTE_PATH; do
    [ -n "${ABSOLUTE_PATH}" ] || continue
    TARGET_PATH="${ROOTFS_SOURCE_DIR}${ABSOLUTE_PATH}"
    if [ ! -e "${TARGET_PATH}" ] || [ -L "${TARGET_PATH}" ]; then
      continue
    fi
    chown "${OWNER}:${GROUP}" "${TARGET_PATH}"
    chmod "${MODE}" "${TARGET_PATH}"
  done < "${SPECIAL_PERMISSIONS_MANIFEST}"
fi

qemu-img create -f raw "${RAW_IMAGE_PATH}" "${IMAGE_SIZE}" >/dev/null

parted -s "${RAW_IMAGE_PATH}" mklabel gpt
parted -s "${RAW_IMAGE_PATH}" unit MiB mkpart ESP fat32 1 513
parted -s "${RAW_IMAGE_PATH}" set 1 esp on
parted -s "${RAW_IMAGE_PATH}" unit MiB mkpart primary ext4 513 100%

RAW_IMAGE_BYTES="$(
  qemu-img info --output=json "${RAW_IMAGE_PATH}" \
    | tr -d '\n' \
    | sed -n 's/.*"virtual-size":[[:space:]]*\([0-9][0-9]*\).*/\1/p'
)"
EFI_SIZE_KIB=$((512 * 1024))
ROOT_START_MIB=513
ROOT_START_BYTES=$((ROOT_START_MIB * 1024 * 1024))
ROOT_PARTITION_SECTORS="$(sgdisk -i 2 "${RAW_IMAGE_PATH}" 2>/dev/null | sed -n 's/^Partition size:[[:space:]]*\([0-9][0-9]*\) sectors.*/\1/p' | head -n 1)"
if [ -z "${ROOT_PARTITION_SECTORS}" ]; then
  echo "failed to determine root partition sector count for ${RAW_IMAGE_PATH}" >&2
  exit 1
fi
ROOTFS_SIZE_BYTES=$((ROOT_PARTITION_SECTORS * 512))

mkfs.vfat -C -n EFI "${EFI_FS_PATH}" "${EFI_SIZE_KIB}" >/dev/null
truncate -s "${ROOTFS_SIZE_BYTES}" "${ROOTFS_FS_PATH}"
mkfs.ext4 -F -L cloudimg-rootfs -d "${ROOTFS_SOURCE_DIR}" "${ROOTFS_FS_PATH}" >/dev/null

cat > "${ROOTFS_SOURCE_DIR}/boot/grub/grub.cfg" <<EOF
set root=(hd0,gpt2)
set default=0
set timeout=0
terminal_input console
terminal_output console
linux /boot/vmlinuz root=/dev/vda2 rootfstype=ext4 rootwait console=tty1 console=hvc0 console=ttyAMA0 loglevel=7 systemd.log_level=debug
initrd /boot/initrd.img
boot
EOF

grub-mkstandalone \
  -O "${GRUB_EFI_TARGET}" \
  -o "${SCRATCH_DIR}/${EFI_BOOT_FILENAME}" \
  --modules="part_gpt ext2 search search_label linux" \
  --locales="" \
  --fonts="" \
  "boot/grub/grub.cfg=${ROOTFS_SOURCE_DIR}/boot/grub/grub.cfg"

mmd -i "${EFI_FS_PATH}" ::/EFI ::/EFI/BOOT >/dev/null 2>&1 || true
mcopy -o -i "${EFI_FS_PATH}" "${SCRATCH_DIR}/${EFI_BOOT_FILENAME}" "::/EFI/BOOT/${EFI_BOOT_FILENAME}"

dd if="${EFI_FS_PATH}" of="${RAW_IMAGE_PATH}" bs=1M seek=1 conv=notrunc,sparse status=none
dd if="${ROOTFS_FS_PATH}" of="${RAW_IMAGE_PATH}" bs=1M seek="${ROOT_START_MIB}" conv=notrunc,sparse status=none

qemu-img convert -f raw -O qcow2 -c "${RAW_IMAGE_PATH}" "${IMAGE_PATH}"

export KALISTO_LIMA_TEMPLATE_ARCH="${TEMPLATE_ARCH}"
sh "${ROOT_DIR}/scripts/macos/render_lima_artifact_template.sh" "${IMAGE_PATH}" "${OUTPUT_DIR}/${IMAGE_BASENAME}.yaml"

IMAGE_SHA256="$(sha256sum "${IMAGE_PATH}" | awk '{print $1}')"
IMAGE_VIRTUAL_SIZE="$(
  qemu-img info --output=json "${IMAGE_PATH}" \
    | tr -d '\n' \
    | sed -n 's/.*"virtual-size":[[:space:]]*\([0-9][0-9]*\).*/\1/p'
)"

cat > "${MANIFEST_PATH}" <<EOF
IMAGE_BASENAME=${IMAGE_BASENAME}
ROOTFS_BUILD_SCRIPT=${ROOTFS_BUILD_SCRIPT}
UBUNTU_RELEASE=${KALISTO_UBUNTU_RELEASE:-noble}
UBUNTU_MIRROR=${KALISTO_UBUNTU_MIRROR:-${DEFAULT_UBUNTU_MIRROR}}
UBUNTU_ARCH=${UBUNTU_ARCH}
TEMPLATE_ARCH=${TEMPLATE_ARCH}
K3S_VERSION=${K3S_VERSION}
IMAGE_PATH=${IMAGE_PATH}
IMAGE_SHA256=${IMAGE_SHA256}
IMAGE_VIRTUAL_SIZE=${IMAGE_VIRTUAL_SIZE}
IMAGE_SIZE=${IMAGE_SIZE}
K3S_ASSET=${K3S_ASSET}
K3S_CACHE_PATH=${K3S_CACHE_PATH}
EOF

echo "created ${IMAGE_PATH}"
echo "wrote ${MANIFEST_PATH}"

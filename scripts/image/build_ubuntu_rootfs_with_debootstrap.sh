#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 ROOTFS_DIR" >&2
  exit 1
fi

ROOTFS_DIR="$1"
ROOT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
PROVISION_SCRIPT="${KALISTO_IMAGE_PROVISION_SCRIPT:-${ROOT_DIR}/scripts/image/provision_kalisto_node.sh}"
K3S_CACHE_PATH="${KALISTO_K3S_CACHE_PATH:?KALISTO_K3S_CACHE_PATH is required}"
UBUNTU_RELEASE="${KALISTO_UBUNTU_RELEASE:-noble}"
UBUNTU_ARCH="${KALISTO_UBUNTU_ARCH:-arm64}"
UBUNTU_VARIANT="${KALISTO_UBUNTU_VARIANT:-minbase}"
HOST_ARCH="$(dpkg --print-architecture)"

case "${UBUNTU_ARCH}" in
  arm64)
    DEFAULT_UBUNTU_MIRROR="http://ftp.fau.de/ubuntu-ports"
    ;;
  amd64)
    DEFAULT_UBUNTU_MIRROR="http://ch.archive.ubuntu.com/ubuntu"
    ;;
  *)
    echo "unsupported Ubuntu target architecture: ${UBUNTU_ARCH}" >&2
    exit 1
    ;;
esac

UBUNTU_MIRROR="${KALISTO_UBUNTU_MIRROR:-${DEFAULT_UBUNTU_MIRROR}}"
PACKAGE_LIST="${KALISTO_UBUNTU_PACKAGE_LIST:-systemd-sysv cloud-init openssh-server sudo iptables kmod socat linux-image-virtual}"

if [ ! -f "${PROVISION_SCRIPT}" ]; then
  echo "missing provision script: ${PROVISION_SCRIPT}" >&2
  exit 1
fi
if [ ! -f "${K3S_CACHE_PATH}" ]; then
  echo "missing staged k3s binary: ${K3S_CACHE_PATH}" >&2
  exit 1
fi
for REQUIRED in chroot debootstrap; do
  if ! command -v "${REQUIRED}" >/dev/null 2>&1; then
    echo "${REQUIRED} is required" >&2
    exit 1
  fi
done
if [ "${HOST_ARCH}" != "${UBUNTU_ARCH}" ]; then
  echo "host architecture ${HOST_ARCH} does not match target Ubuntu architecture ${UBUNTU_ARCH}; run this job under a matching image platform" >&2
  exit 1
fi

rm -rf "${ROOTFS_DIR}"
mkdir -p "${ROOTFS_DIR}"

if ! debootstrap \
  --arch="${UBUNTU_ARCH}" \
  --variant="${UBUNTU_VARIANT}" \
  --components=main,universe \
  "${UBUNTU_RELEASE}" \
  "${ROOTFS_DIR}" \
  "${UBUNTU_MIRROR}"
then
  if [ -f "${ROOTFS_DIR}/debootstrap/debootstrap.log" ]; then
    cat "${ROOTFS_DIR}/debootstrap/debootstrap.log" >&2
  fi
  exit 1
fi

cp /etc/resolv.conf "${ROOTFS_DIR}/etc/resolv.conf"
mkdir -p "${ROOTFS_DIR}/usr/sbin"
cat > "${ROOTFS_DIR}/usr/sbin/policy-rc.d" <<'EOF'
#!/bin/sh
exit 101
EOF
chmod 0755 "${ROOTFS_DIR}/usr/sbin/policy-rc.d"

chroot "${ROOTFS_DIR}" apt-get update
chroot "${ROOTFS_DIR}" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ${PACKAGE_LIST}

mkdir -p \
  "${ROOTFS_DIR}/etc/cloud/cloud.cfg.d" \
  "${ROOTFS_DIR}/etc/kalisto" \
  "${ROOTFS_DIR}/usr/local/lib/kalisto/cache" \
  "${ROOTFS_DIR}/boot/efi" \
  "${ROOTFS_DIR}/boot/grub"

cat > "${ROOTFS_DIR}/etc/cloud/cloud.cfg.d/99_kalisto.cfg" <<'EOF'
datasource_list: [ NoCloud, None ]
preserve_hostname: false
manage_etc_hosts: true
EOF

cat > "${ROOTFS_DIR}/etc/fstab" <<'EOF'
LABEL=cloudimg-rootfs / ext4 discard,commit=30,errors=remount-ro 0 1
LABEL=EFI /boot/efi vfat umask=0077 0 1
EOF

cp "${PROVISION_SCRIPT}" "${ROOTFS_DIR}/usr/local/lib/kalisto/provision_lima_node.sh"
cp "${K3S_CACHE_PATH}" "${ROOTFS_DIR}/usr/local/lib/kalisto/cache/k3s"
chmod 0755 \
  "${ROOTFS_DIR}/usr/local/lib/kalisto/provision_lima_node.sh" \
  "${ROOTFS_DIR}/usr/local/lib/kalisto/cache/k3s"

chroot "${ROOTFS_DIR}" env K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}" /usr/local/lib/kalisto/provision_lima_node.sh

cat > "${ROOTFS_DIR}/boot/grub/grub.cfg" <<'EOF'
search --no-floppy --label --set=root cloudimg-rootfs
set default=0
set timeout=0
terminal_input console
terminal_output console
linux /vmlinuz root=LABEL=cloudimg-rootfs console=tty1
initrd /initrd.img
boot
EOF

rm -f \
  "${ROOTFS_DIR}/etc/resolv.conf" \
  "${ROOTFS_DIR}/usr/sbin/policy-rc.d" \
  "${ROOTFS_DIR}/etc/ssh/ssh_host_"* \
  "${ROOTFS_DIR}/var/lib/dbus/machine-id"
truncate -s 0 "${ROOTFS_DIR}/etc/machine-id"

chroot "${ROOTFS_DIR}" apt-get clean
rm -rf \
  "${ROOTFS_DIR}/var/lib/apt/lists"/* \
  "${ROOTFS_DIR}/tmp"/* \
  "${ROOTFS_DIR}/var/tmp"/*

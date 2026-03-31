#!/bin/sh
set -eu

UBUNTU_MIRROR="${UBUNTU_MIRROR:-}"
PACKAGE_LIST="${PACKAGE_LIST:?PACKAGE_LIST is required}"
K3S_VERSION="${K3S_VERSION:?K3S_VERSION is required}"

apt_retry() {
  apt-get \
    -o Acquire::Retries=5 \
    -o Acquire::http::Timeout=30 \
    -o Acquire::https::Timeout=30 \
    "$@"
}

if [ -n "${UBUNTU_MIRROR}" ]; then
  find /etc/apt -type f \( -name '*.list' -o -name '*.sources' \) -exec \
    sed -i \
      -e "s|http://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}|g" \
      -e "s|http://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}|g" \
      -e "s|http://ports.ubuntu.com/ubuntu-ports|${UBUNTU_MIRROR}|g" \
      {} +
fi

mkdir -p /usr/sbin
cat > /usr/sbin/policy-rc.d <<'EOF'
#!/bin/sh
exit 101
EOF
chmod 0755 /usr/sbin/policy-rc.d

apt_retry update
apt_retry install -y --no-install-recommends ${PACKAGE_LIST}

mkdir -p /etc/cloud/cloud.cfg.d /etc/kalisto /usr/local/lib/kalisto/cache /boot/efi /boot/grub /etc/systemd/network /etc/netplan
mkdir -p /etc/systemd/system/ssh.service.d

cat > /etc/cloud/cloud.cfg.d/99_kalisto.cfg <<'EOF'
datasource_list: [ NoCloud, None ]
preserve_hostname: false
manage_etc_hosts: true
EOF

cat > /etc/cloud/cloud.cfg.d/99_kalisto_network.cfg <<'EOF'
network:
  config: disabled
EOF

cat > /etc/systemd/network/10-kalisto-dhcp.network <<'EOF'
[Match]
Type=ether

[Network]
DHCP=yes
IPv6AcceptRA=yes
EOF

cat > /etc/netplan/10-kalisto-dhcp.yaml <<'EOF'
network:
  version: 2
  renderer: networkd
  ethernets:
    default-en:
      match:
        name: "en*"
      dhcp4: true
      dhcp6: true
    default-eth:
      match:
        name: "eth*"
      dhcp4: true
      dhcp6: true
EOF

cat > /etc/systemd/system/ssh.service.d/10-kalisto-hostkeys.conf <<'EOF'
[Service]
ExecStartPre=
ExecStartPre=/usr/bin/ssh-keygen -A
ExecStartPre=/usr/sbin/sshd -t
EOF

cat > /etc/fstab <<'EOF'
LABEL=cloudimg-rootfs / ext4 discard,commit=30,errors=remount-ro 0 1
LABEL=EFI /boot/efi vfat umask=0077 0 1
EOF

chmod 0755 /usr/local/lib/kalisto/provision_lima_node.sh /usr/local/lib/kalisto/cache/k3s
K3S_VERSION="${K3S_VERSION}" /usr/local/lib/kalisto/provision_lima_node.sh
cloud-init clean --logs --seed || true

# BuildKit's local directory export drops setuid/setgid bits. Record them so
# the image assembly step can reapply the original metadata before mkfs.ext4.
find / -xdev \( -perm -4000 -o -perm -2000 \) -printf '%m %U %G %p\n' | sort > /etc/kalisto/rootfs-special-permissions.manifest

ssh-keygen -A
systemctl disable ssh.socket >/dev/null 2>&1 || true
systemctl enable systemd-networkd.service ssh.service serial-getty@hvc0.service serial-getty@ttyAMA0.service
systemctl enable systemd-resolved.service 2>/dev/null || true

cat > /boot/grub/grub.cfg <<'EOF'
search --no-floppy --label --set=root cloudimg-rootfs
set default=0
set timeout=0
terminal_input console
terminal_output console
linux /boot/vmlinuz root=LABEL=cloudimg-rootfs console=tty1 console=hvc0 console=ttyAMA0 loglevel=7 systemd.log_level=debug
initrd /boot/initrd.img
boot
EOF

rm -f /usr/sbin/policy-rc.d /etc/ssh/ssh_host_* /var/lib/dbus/machine-id
truncate -s 0 /etc/machine-id

apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

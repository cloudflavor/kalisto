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

mkdir -p /usr/local/lib/kalisto
cat > /usr/local/lib/kalisto/run_lima_cidata_boot.sh <<'EOF'
#!/bin/sh
set -eu
exec >/dev/console 2>&1
set -x

MARKER=/var/lib/kalisto/lima-cidata-boot-ran
mkdir -p /var/lib/kalisto
date -u +"%Y-%m-%dT%H:%M:%SZ" > "$MARKER.start"

LIMA_CIDATA_MNT=/mnt/lima-cidata
LIMA_CIDATA_SHADOW=/run/lima-cidata

resolve_cidata_path() {
  lower="$1"
  upper="$2"
  if [ -e "$LIMA_CIDATA_MNT/$lower" ]; then
    printf '%s
' "$LIMA_CIDATA_MNT/$lower"
    return 0
  fi
  if [ -e "$LIMA_CIDATA_MNT/$upper" ]; then
    printf '%s
' "$LIMA_CIDATA_MNT/$upper"
    return 0
  fi
  return 1
}

LIMA_CIDATA_DEV=
for candidate in /dev/disk/by-label/cidata /dev/disk/by-label/CIDATA /dev/vdb; do
  if [ -e "$candidate" ]; then
    LIMA_CIDATA_DEV="$candidate"
    break
  fi
done

if [ -z "$LIMA_CIDATA_DEV" ]; then
  echo "lima-cidata-boot: no cidata device found" >&2
  date -u +"%Y-%m-%dT%H:%M:%SZ no-device" > "$MARKER.error"
  exit 1
fi

echo "lima-cidata-boot: using cidata device $LIMA_CIDATA_DEV"

mkdir -p "$LIMA_CIDATA_MNT"
mountpoint -q "$LIMA_CIDATA_MNT" 2>/dev/null || mount -o ro,mode=0700,dmode=0700,overriderockperm,exec "$LIMA_CIDATA_DEV" "$LIMA_CIDATA_MNT"
ls -la "$LIMA_CIDATA_MNT" || true
date -u +"%Y-%m-%dT%H:%M:%SZ mounted" > "$MARKER.mounted"

rm -rf "$LIMA_CIDATA_SHADOW"
mkdir -p "$LIMA_CIDATA_SHADOW" "$LIMA_CIDATA_SHADOW/util"
ln -s "$(resolve_cidata_path 'boot.sh' 'BOOT.SH')" "$LIMA_CIDATA_SHADOW/boot.sh"
ln -s "$(resolve_cidata_path 'lima.env' 'LIMA.ENV')" "$LIMA_CIDATA_SHADOW/lima.env"
ln -s "$(resolve_cidata_path 'param.env' 'PARAM.ENV')" "$LIMA_CIDATA_SHADOW/param.env"
ln -s "$(resolve_cidata_path 'etc_environment' 'ETC_ENVIRONMENT')" "$LIMA_CIDATA_SHADOW/etc_environment"
ln -s "$(resolve_cidata_path 'user-data' 'USER_DATA')" "$LIMA_CIDATA_SHADOW/user-data"
ln -s "$(resolve_cidata_path 'meta-data' 'META_DATA')" "$LIMA_CIDATA_SHADOW/meta-data"
ln -s "$(resolve_cidata_path 'network-config' 'NETWORK_CONFIG')" "$LIMA_CIDATA_SHADOW/network-config"
ln -s "$(resolve_cidata_path 'lima-guestagent' 'LIMA_GUESTAGENT')" "$LIMA_CIDATA_SHADOW/lima-guestagent"
ln -s "$(resolve_cidata_path 'util/print_cidata_fstab.lua' 'UTIL/PRINT_CIDATA_FSTAB.LUA')" "$LIMA_CIDATA_SHADOW/util/print_cidata_fstab.lua"
if [ -d "$LIMA_CIDATA_MNT/BOOT" ]; then
  ln -s "$LIMA_CIDATA_MNT/BOOT" "$LIMA_CIDATA_SHADOW/boot.essential.Linux"
fi

while read -r line; do [ -n "$line" ] && export "$line"; done <"$LIMA_CIDATA_SHADOW/lima.env"

if ! id "$LIMA_CIDATA_USER" >/dev/null 2>&1; then
  useradd -m -u "$LIMA_CIDATA_UID" -d "$LIMA_CIDATA_HOME" -s "$LIMA_CIDATA_SHELL" -c "$LIMA_CIDATA_COMMENT" "$LIMA_CIDATA_USER"
fi
usermod -aG sudo "$LIMA_CIDATA_USER" 2>/dev/null || true
mkdir -p /etc/sudoers.d
printf '%s ALL=(ALL) NOPASSWD:ALL
' "$LIMA_CIDATA_USER" > "/etc/sudoers.d/90-lima-$LIMA_CIDATA_USER"
chmod 0440 "/etc/sudoers.d/90-lima-$LIMA_CIDATA_USER"
mkdir -p "$LIMA_CIDATA_HOME/.ssh"
awk '
  /ssh-authorized-keys:/ { in_keys=1; next }
  in_keys && /^[[:space:]]*-[[:space:]]*"/ {
    line=$0
    sub(/^[[:space:]]*-[[:space:]]*"/, "", line)
    sub(/"[[:space:]]*$/, "", line)
    print line
    exit
  }
  in_keys && /^[^[:space:]-]/ { exit }
' "$LIMA_CIDATA_SHADOW/user-data" > "$LIMA_CIDATA_HOME/.ssh/authorized_keys"
chown -R "$LIMA_CIDATA_USER:$LIMA_CIDATA_USER" "$LIMA_CIDATA_HOME/.ssh"
chmod 0700 "$LIMA_CIDATA_HOME/.ssh"
chmod 0600 "$LIMA_CIDATA_HOME/.ssh/authorized_keys"

export LIMA_CIDATA_MNT="$LIMA_CIDATA_SHADOW"
exec "$LIMA_CIDATA_SHADOW/boot.sh"
EOF
chmod 0755 /usr/local/lib/kalisto/run_lima_cidata_boot.sh

cat > /etc/systemd/system/lima-cidata-boot.service <<'EOF'
[Unit]
Description=Run Lima cidata boot payload
After=local-fs.target systemd-remount-fs.service
Before=cloud-init-local.service ssh.service ssh.socket network-pre.target
Wants=network-pre.target

[Service]
Type=oneshot
StandardOutput=journal+console
StandardError=journal+console
ExecStart=/usr/local/lib/kalisto/run_lima_cidata_boot.sh

[Install]
WantedBy=basic.target
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

ssh-keygen -A
systemctl disable ssh.socket >/dev/null 2>&1 || true
systemctl enable lima-cidata-boot.service systemd-networkd.service ssh.service serial-getty@hvc0.service serial-getty@ttyAMA0.service
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

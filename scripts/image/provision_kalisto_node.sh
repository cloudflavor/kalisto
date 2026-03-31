#!/bin/sh
set -eu

K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"

export DEBIAN_FRONTEND=noninteractive

apt_retry() {
  apt-get \
    -o Acquire::Retries=5 \
    -o Acquire::http::Timeout=30 \
    -o Acquire::https::Timeout=30 \
    "$@"
}

apt_retry update
apt_retry install -y --no-install-recommends iptables procps socat

mkdir -p /etc/modules-load.d /etc/sysctl.d /etc/kalisto /usr/local/lib/kalisto
mkdir -p /var/lib/kalisto

cat > /etc/modules-load.d/kalisto-k3s.conf <<'EOF'
overlay
br_netfilter
EOF

cat > /etc/sysctl.d/90-kalisto-k3s.conf <<'EOF'
net.ipv4.ip_forward=1
net.bridge.bridge-nf-call-iptables=1
net.bridge.bridge-nf-call-ip6tables=1
EOF

TMP_K3S=/tmp/k3s
rm -f "${TMP_K3S}"

if [ -x /usr/local/lib/kalisto/cache/k3s ]; then
  cp /usr/local/lib/kalisto/cache/k3s "${TMP_K3S}"
  chmod 0755 "${TMP_K3S}"
  if "${TMP_K3S}" --version >/dev/null 2>&1; then
    mv "${TMP_K3S}" /usr/local/bin/k3s
  else
    rm -f "${TMP_K3S}"
  fi
fi

if [ ! -x /usr/local/bin/k3s ]; then
  echo "k3s binary was not staged into /usr/local/lib/kalisto/cache/k3s" >&2
  exit 1
fi

rm -f /usr/local/lib/kalisto/cache/k3s /usr/local/lib/kalisto/provision_lima_node.sh
rmdir /usr/local/lib/kalisto/cache 2>/dev/null || true
rmdir /usr/local/lib/kalisto 2>/dev/null || true

PURGE_PACKAGES=
for PACKAGE in \
  apport \
  bash-completion \
  byobu \
  grub-common \
  grub2-common \
  grub-efi-amd64-bin \
  grub-efi-arm64-bin \
  groff-base \
  man-db \
  motd-news-config \
  multipath-tools \
  open-iscsi \
  pollinate \
  rsyslog \
  snapd \
  ubuntu-advantage-tools \
  update-manager-core
do
  if dpkg -s "${PACKAGE}" >/dev/null 2>&1; then
    PURGE_PACKAGES="${PURGE_PACKAGES} ${PACKAGE}"
  fi
done
if [ -n "${PURGE_PACKAGES}" ]; then
  apt_retry purge -y --auto-remove ${PURGE_PACKAGES}
fi

cat > /etc/kalisto/node-image.env <<EOF
K3S_VERSION=${K3S_VERSION}
IMAGE_FLAVOR=ubuntu-24.04-minimal
EOF

date -u +"%Y-%m-%dT%H:%M:%SZ" > /var/lib/kalisto/image-ready

apt_retry autoremove -y
apt-get clean
rm -rf /var/lib/apt/lists/*

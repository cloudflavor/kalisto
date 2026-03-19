#!/bin/sh
set -eu

K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"

export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends ca-certificates curl iptables kmod procps socat

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

modprobe overlay || true
modprobe br_netfilter || true
sysctl --system || true

ARCH="$(uname -m)"
case "${ARCH}" in
  aarch64|arm64)
    CANDIDATES="k3s-arm64 k3s-aarch64 k3s"
    ;;
  x86_64|amd64)
    CANDIDATES="k3s k3s-amd64 k3s-x86_64"
    ;;
  *)
    echo "unsupported architecture: ${ARCH}" >&2
    exit 1
    ;;
esac

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
  for ASSET in ${CANDIDATES}; do
    URL="https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/${ASSET}"
    if curl -fsSL "${URL}" -o "${TMP_K3S}"; then
      chmod 0755 "${TMP_K3S}"
      if "${TMP_K3S}" --version >/dev/null 2>&1; then
        mv "${TMP_K3S}" /usr/local/bin/k3s
        break
      fi
    fi
    rm -f "${TMP_K3S}"
  done
fi

if [ ! -x /usr/local/bin/k3s ]; then
  echo "k3s binary was not installed for ${ARCH}" >&2
  exit 1
fi

cat > /etc/kalisto/node-image.env <<EOF
K3S_VERSION=${K3S_VERSION}
IMAGE_FLAVOR=ubuntu-24.04-minimal
EOF

date -u +"%Y-%m-%dT%H:%M:%SZ" > /var/lib/kalisto/image-ready

apt-get autoremove -y
apt-get clean
rm -rf /var/lib/apt/lists/*

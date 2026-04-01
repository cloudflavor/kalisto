#!/bin/sh
set -eu

KALISTO_WORKSPACE_ROOT="${KALISTO_WORKSPACE_ROOT:-/workspace}"
ARTIFACT_DIR="${KALISTO_ARTIFACT_DIR:-${KALISTO_WORKSPACE_ROOT}/target/k3s-probe}"
KUBECONFIG_PATH="${ARTIFACT_DIR}/kubeconfig.yaml"
TOKEN_PATH="${ARTIFACT_DIR}/node-token"
STDOUT_LOG="${ARTIFACT_DIR}/k3s.stdout.log"
STDERR_LOG="${ARTIFACT_DIR}/k3s.stderr.log"
K3S_VERSION="${K3S_VERSION:-v1.33.5+k3s1}"
KALISTO_K3S_NODE_ROLE="${KALISTO_K3S_NODE_ROLE:-server}"
KALISTO_K3S_NODE_NAME="${KALISTO_K3S_NODE_NAME:-}"
KALISTO_K3S_SERVER_URL="${KALISTO_K3S_SERVER_URL:-}"
KALISTO_K3S_TOKEN="${KALISTO_K3S_TOKEN:-}"
KALISTO_K3S_TLS_SANS="${KALISTO_K3S_TLS_SANS:-}"
KALISTO_K3S_KUBELET_RESOLV_CONF="${KALISTO_K3S_KUBELET_RESOLV_CONF:-/etc/kalisto/kubelet-resolv.conf}"
KALISTO_K3S_DISABLE_AGENT="${KALISTO_K3S_DISABLE_AGENT:-0}"
KALISTO_K3S_USE_SYSTEMD="${KALISTO_K3S_USE_SYSTEMD:-0}"
KALISTO_K3S_DISABLE_TRAEFIK="${KALISTO_K3S_DISABLE_TRAEFIK:-1}"
KALISTO_K3S_DISABLE_SERVICELB="${KALISTO_K3S_DISABLE_SERVICELB:-1}"
KALISTO_K3S_DISABLE_METRICS_SERVER="${KALISTO_K3S_DISABLE_METRICS_SERVER:-1}"
KALISTO_K3S_DISABLE_LOCAL_STORAGE="${KALISTO_K3S_DISABLE_LOCAL_STORAGE:-1}"
KALISTO_K3S_DISABLE_COREDNS="${KALISTO_K3S_DISABLE_COREDNS:-1}"
KALISTO_K3S_DISABLE_NETWORK_POLICY="${KALISTO_K3S_DISABLE_NETWORK_POLICY:-1}"
KALISTO_K3S_DISABLE_KUBE_PROXY="${KALISTO_K3S_DISABLE_KUBE_PROXY:-0}"
KALISTO_K3S_FLANNEL_BACKEND_NONE="${KALISTO_K3S_FLANNEL_BACKEND_NONE:-0}"

mkdir -p "${ARTIFACT_DIR}" /etc/kalisto /etc/rancher/k3s /var/lib/rancher/k3s

cat > "${KALISTO_K3S_KUBELET_RESOLV_CONF}" <<'EOF'
nameserver 1.1.1.1
nameserver 2606:4700:4700::1111
EOF

rm -f /etc/resolv.conf
cat > /etc/resolv.conf <<'EOF'
nameserver 1.1.1.1
nameserver 2606:4700:4700::1111
EOF

ensure_runtime_dependencies() {
  if [ -f /etc/kalisto/node-image.env ] && [ -x /usr/local/bin/k3s ] && /usr/local/bin/k3s --version >/dev/null 2>&1 && command -v curl >/dev/null 2>&1 && command -v ps >/dev/null 2>&1; then
    return 0
  fi

  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends curl ca-certificates procps
}

ensure_runtime_dependencies

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

tls_san_args() {
  if [ -z "${KALISTO_K3S_TLS_SANS}" ]; then
    return 0
  fi

  PREV_IFS="${IFS}"
  IFS=','
  for SAN in ${KALISTO_K3S_TLS_SANS}; do
    if [ -n "${SAN}" ]; then
      printf ' --tls-san %s' "${SAN}"
    fi
  done
  IFS="${PREV_IFS}"
}

TMP_K3S=/tmp/k3s
if ! [ -x /usr/local/bin/k3s ] || ! /usr/local/bin/k3s --version >/dev/null 2>&1; then
  rm -f /usr/local/bin/k3s
  rm -f "${TMP_K3S}"
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

cat > /usr/local/bin/kalisto-k3s-node <<EOF
#!/bin/sh
set -eu

export K3S_KUBECONFIG_MODE=0644

KUBELET_ARGS="--kubelet-arg=resolv-conf=\${KALISTO_K3S_KUBELET_RESOLV_CONF:-/etc/kalisto/kubelet-resolv.conf}"

tls_san_args() {
  if [ -z "\${KALISTO_K3S_TLS_SANS:-}" ]; then
    return 0
  fi

  PREV_IFS="\${IFS}"
  IFS=','
  for SAN in \${KALISTO_K3S_TLS_SANS}; do
    if [ -n "\${SAN}" ]; then
      printf ' --tls-san %s' "\${SAN}"
    fi
  done
  IFS="\${PREV_IFS}"
}

NODE_NAME_ARGS=
if [ -n "\${KALISTO_K3S_NODE_NAME:-}" ]; then
  NODE_NAME_ARGS="--node-name \${KALISTO_K3S_NODE_NAME}"
fi

case "\${KALISTO_K3S_NODE_ROLE:-server}" in
  server)
    EXTRA_ARGS=
    TLS_SAN_ARGS="$(tls_san_args)"
    if [ "\${KALISTO_K3S_DISABLE_TRAEFIK:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable=traefik"
    fi
    if [ "\${KALISTO_K3S_DISABLE_SERVICELB:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable=servicelb"
    fi
    if [ "\${KALISTO_K3S_DISABLE_METRICS_SERVER:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable=metrics-server"
    fi
    if [ "\${KALISTO_K3S_DISABLE_LOCAL_STORAGE:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable=local-storage"
    fi
    if [ "\${KALISTO_K3S_DISABLE_COREDNS:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable=coredns"
    fi
    if [ "\${KALISTO_K3S_DISABLE_NETWORK_POLICY:-1}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable-network-policy"
    fi
    if [ "\${KALISTO_K3S_DISABLE_KUBE_PROXY:-0}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --disable-kube-proxy"
    fi
    if [ "\${KALISTO_K3S_FLANNEL_BACKEND_NONE:-0}" = "1" ]; then
      EXTRA_ARGS="\${EXTRA_ARGS} --flannel-backend=none"
    fi

    if [ "\${KALISTO_K3S_DISABLE_AGENT:-0}" = "1" ]; then
      exec /bin/sh -lc "/usr/local/bin/k3s server \
    \${NODE_NAME_ARGS} \
    --disable-agent \
    --write-kubeconfig ${KUBECONFIG_PATH} \
    --write-kubeconfig-mode 0644 \
    \${KUBELET_ARGS} \
    \${TLS_SAN_ARGS} \
    --prefer-bundled-bin \${EXTRA_ARGS}"
    fi

    exec /bin/sh -lc "/usr/local/bin/k3s server \
  \${NODE_NAME_ARGS} \
  --write-kubeconfig ${KUBECONFIG_PATH} \
  --write-kubeconfig-mode 0644 \
  \${KUBELET_ARGS} \
  \${TLS_SAN_ARGS} \
  --prefer-bundled-bin \${EXTRA_ARGS}"
    ;;
  agent)
    if [ -z "\${KALISTO_K3S_SERVER_URL:-}" ] || [ -z "\${KALISTO_K3S_TOKEN:-}" ]; then
      echo "agent role requires KALISTO_K3S_SERVER_URL and KALISTO_K3S_TOKEN" >&2
      exit 1
    fi

    exec /bin/sh -lc "/usr/local/bin/k3s agent \
  \${NODE_NAME_ARGS} \
  --server \${KALISTO_K3S_SERVER_URL} \
  --token \${KALISTO_K3S_TOKEN} \
  \${KUBELET_ARGS} \
  --prefer-bundled-bin"
    ;;
  *)
    echo "unsupported node role: \${KALISTO_K3S_NODE_ROLE}" >&2
    exit 1
    ;;
esac
EOF

chmod 0755 /usr/local/bin/kalisto-k3s-node

is_k3s_node_running() {
  case "${KALISTO_K3S_NODE_ROLE}" in
    server)
      ps -eo comm=,args= | awk '$1 == "k3s" && $0 ~ /(^|[[:space:]])server([[:space:]]|$)/ { found = 1 } END { exit found ? 0 : 1 }'
      ;;
    agent)
      ps -eo comm=,args= | awk '$1 == "k3s" && $0 ~ /(^|[[:space:]])agent([[:space:]]|$)/ { found = 1 } END { exit found ? 0 : 1 }'
      ;;
    *)
      return 1
      ;;
  esac
}

if [ "${KALISTO_K3S_USE_SYSTEMD}" = "1" ]; then
  test -s /etc/machine-id || dbus-uuidgen --ensure=/etc/machine-id

  cat > /etc/systemd/system/kalisto-k3s.service <<EOF
[Unit]
Description=Kalisto k3s node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Delegate=yes
Environment=KALISTO_K3S_NODE_ROLE=${KALISTO_K3S_NODE_ROLE}
Environment=KALISTO_K3S_NODE_NAME=${KALISTO_K3S_NODE_NAME}
Environment=KALISTO_K3S_SERVER_URL=${KALISTO_K3S_SERVER_URL}
Environment=KALISTO_K3S_TOKEN=${KALISTO_K3S_TOKEN}
Environment=KALISTO_K3S_TLS_SANS=${KALISTO_K3S_TLS_SANS}
Environment=KALISTO_K3S_KUBELET_RESOLV_CONF=${KALISTO_K3S_KUBELET_RESOLV_CONF}
Environment=KALISTO_K3S_DISABLE_AGENT=${KALISTO_K3S_DISABLE_AGENT}
Environment=KALISTO_K3S_DISABLE_KUBE_PROXY=${KALISTO_K3S_DISABLE_KUBE_PROXY}
ExecStart=/bin/sh -lc '/usr/local/bin/kalisto-k3s-node >>"${STDOUT_LOG}" 2>>"${STDERR_LOG}"'
Restart=on-failure
RestartSec=5
KillMode=process

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable kalisto-k3s.service >/dev/null 2>&1 || true
  systemctl restart kalisto-k3s.service
else
  if is_k3s_node_running; then
    exit 0
  fi

  nohup /usr/local/bin/kalisto-k3s-node >"${STDOUT_LOG}" 2>"${STDERR_LOG}" &
fi

case "${KALISTO_K3S_NODE_ROLE}" in
  server)
    for _ in $(seq 1 90); do
      if [ -s "${KUBECONFIG_PATH}" ] && [ -s /var/lib/rancher/k3s/server/node-token ]; then
        cp /var/lib/rancher/k3s/server/node-token "${TOKEN_PATH}"
        exit 0
      fi
      sleep 2
    done

    echo "timed out waiting for kubeconfig or node token for server role" >&2
    exit 1
    ;;
  agent)
    for _ in $(seq 1 90); do
      if pgrep -f "/usr/local/bin/k3s agent" >/dev/null 2>&1; then
        exit 0
      fi
      sleep 2
    done

    echo "timed out waiting for k3s agent process" >&2
    exit 1
    ;;
esac

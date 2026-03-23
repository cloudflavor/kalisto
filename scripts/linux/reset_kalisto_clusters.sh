#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  reset_kalisto_clusters.sh [--all | <cluster>...]

Deletes Kalisto cluster state/runtime for the requested clusters and kills
Kalisto-owned Firecracker processes. Intended as a local reset tool for the
current non-backward-compatible cluster state format.

Examples:
  ./scripts/linux/reset_kalisto_clusters.sh --all
  ./scripts/linux/reset_kalisto_clusters.sh first second

Notes:
  - Run with sudo if cluster files or Firecracker processes are owned by root.
  - When run as root via sudo, the script still targets the invoking user's
    Kalisto config/data directories.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

invoking_user="${SUDO_USER:-}"
if [[ -n "${invoking_user}" && "${invoking_user}" != "root" ]]; then
  if [[ "$(uname -s)" == "Darwin" ]]; then
    user_home="/Users/${invoking_user}"
  else
    user_home="/home/${invoking_user}"
  fi
else
  user_home="${HOME}"
fi

config_root="${XDG_CONFIG_HOME:-${user_home}/.config}/kalisto"
data_root="${XDG_DATA_HOME:-${user_home}/.local/share}/kalisto"
clusters_root="${data_root}/clusters"
runtime_root="${repo_root}/target/kalisto/clusters"
default_cluster_file="${config_root}/default-cluster"

delete_cluster_paths() {
  local cluster_name="$1"
  rm -rf "${clusters_root}/${cluster_name}"
  rm -rf "${runtime_root}/${cluster_name}"
}

read_default_cluster() {
  if [[ -f "${default_cluster_file}" ]]; then
    tr -d '\n' < "${default_cluster_file}"
  fi
}

cluster_names=()
if [[ $# -eq 0 || "${1:-}" == "--all" ]]; then
  if [[ -d "${clusters_root}" ]]; then
    while IFS= read -r entry; do
      cluster_names+=("${entry}")
    done < <(find "${clusters_root}" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
  fi
  if [[ -d "${runtime_root}" ]]; then
    while IFS= read -r entry; do
      found=0
      for existing in "${cluster_names[@]}"; do
        if [[ "${existing}" == "${entry}" ]]; then
          found=1
          break
        fi
      done
      if [[ "${found}" -eq 0 ]]; then
        cluster_names+=("${entry}")
      fi
    done < <(find "${runtime_root}" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
  fi
else
  cluster_names=("$@")
fi

for cluster_name in "${cluster_names[@]}"; do
  if [[ -n "${cluster_name}" ]]; then
    echo "removing cluster paths for ${cluster_name}"
    delete_cluster_paths "${cluster_name}"
  fi
done

default_cluster="$(read_default_cluster || true)"
if [[ -n "${default_cluster}" ]]; then
  clear_default=0
  if [[ $# -eq 0 || "${1:-}" == "--all" ]]; then
    clear_default=1
  else
    for cluster_name in "${cluster_names[@]}"; do
      if [[ "${cluster_name}" == "${default_cluster}" ]]; then
        clear_default=1
        break
      fi
    done
  fi
  if [[ "${clear_default}" -eq 1 ]]; then
    rm -f "${default_cluster_file}"
  fi
fi

if command -v pkill >/dev/null 2>&1; then
  pkill -TERM -f 'firecracker --id kalisto-' >/dev/null 2>&1 || true
  sleep 1
  pkill -KILL -f 'firecracker --id kalisto-' >/dev/null 2>&1 || true
fi

if [[ "$(uname -s)" == "Linux" ]] && command -v ip >/dev/null 2>&1; then
  while IFS= read -r tap_name; do
    if [[ -n "${tap_name}" ]]; then
      ip link del "${tap_name}" >/dev/null 2>&1 || true
    fi
  done < <(ip -o link show 2>/dev/null | awk -F': ' '/^[0-9]+: ktap/ {print $2}')

  if ip link show kalistofc0 >/dev/null 2>&1; then
    ip link set dev kalistofc0 down >/dev/null 2>&1 || true
    ip link del dev kalistofc0 type bridge >/dev/null 2>&1 || true
  fi
fi

echo "kalisto cleanup complete"

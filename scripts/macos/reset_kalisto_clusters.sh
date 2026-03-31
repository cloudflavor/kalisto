#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  reset_kalisto_clusters.sh [--all | <cluster>...]

Deletes Kalisto cluster state/runtime for the requested clusters and forcefully
stops and deletes Kalisto-owned Lima instances. Intended as a local reset tool
for the current non-backward-compatible cluster state format.

Examples:
  ./scripts/macos/reset_kalisto_clusters.sh --all
  ./scripts/macos/reset_kalisto_clusters.sh default
  ./scripts/macos/reset_kalisto_clusters.sh first second

Notes:
  - sudo should not be needed on macOS during normal use.
  - If previous runs left root-owned Kalisto files behind, rerun this script
    with sudo once. Lima commands will still run as the invoking user.
  - When run as root via sudo, the script still targets the invoking user's
    Kalisto config/data directories.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "reset_kalisto_clusters.sh is intended for macOS" >&2
  exit 1
fi

invoking_user="${SUDO_USER:-}"
if [[ -n "${invoking_user}" && "${invoking_user}" != "root" ]]; then
  user_home="/Users/${invoking_user}"
else
  user_home="${HOME}"
fi

config_root="${XDG_CONFIG_HOME:-${user_home}/.config}/kalisto"
data_root="${XDG_DATA_HOME:-${user_home}/.local/share}/kalisto"
clusters_root="${data_root}/clusters"
runtime_root="${repo_root}/target/kalisto/clusters"
default_cluster_file="${config_root}/default-cluster"

append_unique() {
  local value="$1"
  local existing
  for existing in "${cluster_names[@]}"; do
    if [[ "${existing}" == "${value}" ]]; then
      return 0
    fi
  done
  cluster_names+=("${value}")
}

collect_cluster_dirs() {
  local root="$1"
  local entry
  if [[ ! -d "${root}" ]]; then
    return 0
  fi
  while IFS= read -r entry; do
    if [[ -n "${entry}" ]]; then
      append_unique "${entry}"
    fi
  done < <(find "${root}" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; 2>/dev/null | sort)
}

read_default_cluster() {
  if [[ -f "${default_cluster_file}" ]]; then
    tr -d '\n' < "${default_cluster_file}"
  fi
}

run_limactl() {
  if [[ -n "${invoking_user}" && "${invoking_user}" != "root" ]]; then
    sudo -u "${invoking_user}" env HOME="${user_home}" PATH="${PATH}" limactl "$@"
  else
    limactl "$@"
  fi
}

delete_cluster_paths() {
  local cluster_name="$1"
  rm -rf "${clusters_root}/${cluster_name}"
  rm -rf "${runtime_root}/${cluster_name}"
}

delete_instance_if_present() {
  local instance_name="$1"
  if ! command -v limactl >/dev/null 2>&1; then
    return 0
  fi
  if run_limactl list --format '{{ .Name }}' "${instance_name}" >/dev/null 2>&1; then
    echo "removing lima instance ${instance_name}"
    run_limactl stop --tty=false --force "${instance_name}" >/dev/null 2>&1 || true
    run_limactl delete --tty=false --force "${instance_name}" >/dev/null 2>&1 || true
  fi
}

delete_instances_for_cluster() {
  local cluster_name="$1"
  local instance_name
  if ! command -v limactl >/dev/null 2>&1; then
    return 0
  fi
  while IFS= read -r instance_name; do
    if [[ -n "${instance_name}" ]]; then
      delete_instance_if_present "${instance_name}"
    fi
  done < <(
    run_limactl list --format '{{ .Name }}' 2>/dev/null \
      | awk -v prefix="kalisto-${cluster_name}-" 'index($0, prefix) == 1 { print $0 }'
  )
}

delete_all_kalisto_instances() {
  local instance_name
  if ! command -v limactl >/dev/null 2>&1; then
    return 0
  fi
  while IFS= read -r instance_name; do
    if [[ -n "${instance_name}" ]]; then
      delete_instance_if_present "${instance_name}"
    fi
  done < <(run_limactl list --format '{{ .Name }}' 2>/dev/null | awk 'index($0, "kalisto-") == 1 { print $0 }')
}

cluster_names=()
delete_all_instances=0
if [[ $# -eq 0 || "${1:-}" == "--all" ]]; then
  collect_cluster_dirs "${clusters_root}"
  collect_cluster_dirs "${runtime_root}"
  delete_all_instances=1
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
  if [[ "${delete_all_instances}" -eq 1 ]]; then
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

if [[ "${delete_all_instances}" -eq 1 ]]; then
  delete_all_kalisto_instances
else
  for cluster_name in "${cluster_names[@]}"; do
    if [[ -n "${cluster_name}" ]]; then
      delete_instances_for_cluster "${cluster_name}"
    fi
  done
fi

echo "kalisto cleanup complete"

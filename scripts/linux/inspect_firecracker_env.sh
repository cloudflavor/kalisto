#!/usr/bin/env bash
set -euo pipefail

section() {
  printf '\n== %s ==\n' "$1"
}

show_cmd() {
  printf '\n$ %s\n' "$*"
  "$@" 2>&1 || true
}

show_path() {
  local label="$1"
  local path="${2:-}"
  printf '\n%s: %s\n' "$label" "${path:-<unset>}"
  if [[ -n "$path" ]]; then
    if [[ -e "$path" ]]; then
      show_cmd ls -lh "$path"
      show_cmd file "$path"
    else
      printf 'missing: %s\n' "$path"
    fi
  fi
}

section "Host"
show_cmd date -Iseconds
show_cmd uname -a
show_cmd id
show_cmd sh -lc 'grep -m1 "^PRETTY_NAME=" /etc/os-release || cat /etc/os-release'

section "Firecracker"
show_cmd which firecracker
show_cmd firecracker --version
show_cmd sh -lc 'firecracker --help | sed -n "1,80p"'

section "KVM"
show_cmd ls -l /dev/kvm
show_cmd sh -lc 'getfacl /dev/kvm 2>/dev/null || true'
show_cmd sh -lc 'groups || true'
show_cmd sh -lc 'lsmod | grep -E "^kvm(_| )|^kvm$" || true'
show_cmd sh -lc 'egrep -c "(vmx|svm)" /proc/cpuinfo || true'

section "Networking Tools"
show_cmd which ip
show_cmd ip link show
show_cmd sh -lc 'ip tuntap help 2>&1 | sed -n "1,40p"'
show_cmd which curl
show_cmd which nc
show_cmd which jq

section "Candidate Artifacts"
show_path "KALISTO_FIRECRACKER_KERNEL" "${KALISTO_FIRECRACKER_KERNEL:-}"
show_path "KALISTO_FIRECRACKER_ROOTFS" "${KALISTO_FIRECRACKER_ROOTFS:-}"

section "Common Search Locations"
show_cmd sh -lc 'find "$HOME" /var/lib /var/tmp /tmp -maxdepth 3 \( -name "vmlinux*" -o -name "bzImage*" -o -name "*.ext4" -o -name "*rootfs*" -o -name "*.img" \) 2>/dev/null | sort | head -n 200'

section "Summary"
cat <<'EOF'
Paste this script output back into the chat.

Optional before running:
  export KALISTO_FIRECRACKER_KERNEL=/absolute/path/to/vmlinux
  export KALISTO_FIRECRACKER_ROOTFS=/absolute/path/to/rootfs.ext4
EOF

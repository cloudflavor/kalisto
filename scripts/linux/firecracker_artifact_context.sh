#!/bin/sh

kalisto_normalize_firecracker_version() {
  version="$1"
  version="${version#v}"
  if [ -z "${version}" ]; then
    echo "missing Firecracker version" >&2
    return 1
  fi
  echo "v${version}"
}

kalisto_detect_firecracker_version() {
  if [ -n "${KALISTO_FIRECRACKER_VERSION:-}" ]; then
    kalisto_normalize_firecracker_version "${KALISTO_FIRECRACKER_VERSION}"
    return 0
  fi

  if command -v firecracker >/dev/null 2>&1; then
    version="$(
      firecracker --version 2>/dev/null \
        | sed -n 's/.*[Ff]irecracker[[:space:]]\+v\{0,1\}\([0-9][0-9.]*\).*/\1/p' \
        | head -n 1
    )"
    if [ -n "${version}" ]; then
      kalisto_normalize_firecracker_version "${version}"
      return 0
    fi
  fi

  echo "unable to detect Firecracker version; set KALISTO_FIRECRACKER_VERSION" >&2
  return 1
}

kalisto_firecracker_artifact_track() {
  version="$(kalisto_normalize_firecracker_version "$1")" || return 1
  version="${version#v}"
  major="$(printf '%s' "${version}" | cut -d. -f1)"
  minor="$(printf '%s' "${version}" | cut -d. -f2)"
  if [ -z "${major}" ] || [ -z "${minor}" ]; then
    echo "invalid Firecracker version: $1" >&2
    return 1
  fi
  echo "v${major}.${minor}"
}

kalisto_detect_firecracker_arch() {
  raw_arch="${KALISTO_FIRECRACKER_ARCH:-$(uname -m)}"
  case "${raw_arch}" in
    x86_64|amd64)
      echo "x86_64"
      ;;
    aarch64|arm64)
      echo "aarch64"
      ;;
    *)
      echo "unsupported Firecracker architecture: ${raw_arch}" >&2
      return 1
      ;;
  esac
}

kalisto_detect_firecracker_context() {
  KALISTO_FIRECRACKER_VERSION_NORMALIZED="$(kalisto_detect_firecracker_version)" || return 1
  KALISTO_FIRECRACKER_ARTIFACT_TRACK="${KALISTO_FIRECRACKER_ARTIFACT_TRACK:-$(kalisto_firecracker_artifact_track "${KALISTO_FIRECRACKER_VERSION_NORMALIZED}")}" || return 1
  KALISTO_FIRECRACKER_ARTIFACT_ARCH="$(kalisto_detect_firecracker_arch)" || return 1
  export KALISTO_FIRECRACKER_VERSION_NORMALIZED
  export KALISTO_FIRECRACKER_ARTIFACT_TRACK
  export KALISTO_FIRECRACKER_ARTIFACT_ARCH
}

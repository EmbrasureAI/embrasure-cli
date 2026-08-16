#!/bin/sh
set -eu

repository="EmbrasureAI/embrasure-cli"
version="${EMBRASURE_VERSION:-}"
if [ -z "$version" ]; then
  version="$(curl -fsSL -H 'Accept: application/vnd.github+json' -H 'User-Agent: embrasure-installer' "https://api.github.com/repos/${repository}/releases/latest" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)"
fi
if [ -z "$version" ]; then
  echo "embrasure: could not determine the latest release" >&2
  exit 1
fi
version="${version#v}"

case "$(uname -s):$(uname -m)" in
  Darwin:x86_64) target="x86_64-apple-darwin" ;;
  Darwin:arm64) target="aarch64-apple-darwin" ;;
  Linux:x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu" ;;
  *) echo "embrasure: unsupported platform $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

archive="embrasure-${version}-${target}.tar.gz"
base="https://github.com/${repository}/releases/download/v${version}"
temporary="$(mktemp -d "${TMPDIR:-/tmp}/embrasure-install.XXXXXX")"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
curl -fsSL "${base}/${archive}" -o "${temporary}/${archive}"
curl -fsSL "${base}/SHA256SUMS" -o "${temporary}/SHA256SUMS"
expected="$(awk -v name="$archive" '$2 == name { print $1 }' "${temporary}/SHA256SUMS")"
if [ -z "$expected" ]; then
  echo "embrasure: release checksum is missing for ${archive}" >&2
  exit 1
fi
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${temporary}/${archive}" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${temporary}/${archive}" | awk '{print $1}')"
else
  echo "embrasure: shasum or sha256sum is required" >&2
  exit 1
fi
if [ "$actual" != "$expected" ]; then
  echo "embrasure: checksum verification failed for ${archive}" >&2
  exit 1
fi
tar -xzf "${temporary}/${archive}" -C "$temporary"

if [ -n "${EMBRASURE_INSTALL_DIR:-}" ]; then
  install_dir="$EMBRASURE_INSTALL_DIR"
elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  install_dir="/usr/local/bin"
else
  install_dir="${HOME}/.local/bin"
fi
mkdir -p "$install_dir"
install -m 755 "${temporary}/embrasure-${version}-${target}/embrasure" "${install_dir}/embrasure"
echo "Installed Embrasure ${version} to ${install_dir}/embrasure"

case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) echo "Add ${install_dir} to PATH before running embrasure." >&2 ;;
esac

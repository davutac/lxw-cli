#!/bin/sh
# Installs lxw on macOS or Linux:
#   curl -fsSL https://raw.githubusercontent.com/davutac/lxw-cli/main/install.sh | sh
# Installs the latest release, or LXW_VERSION (e.g. 0.2.0), into ~/.local/bin, or
# LXW_INSTALL_DIR, after checking the binary against the release's SHA256SUMS.
set -eu

releases="https://github.com/davutac/lxw-cli/releases"
case "${LXW_VERSION:-latest}" in
  latest) base="$releases/latest/download" ;;
  *) base="$releases/download/v${LXW_VERSION#v}" ;;
esac

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) echo "lxw: unsupported system $(uname -s); on Windows use install.ps1" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=amd64 ;;
  *) echo "lxw: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
asset="lxw-$os-$arch"
dir="${LXW_INSTALL_DIR:-$HOME/.local/bin}"

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$1" "$2"
  else
    wget -qO "$1" "$2"
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch "$tmp/$asset" "$base/$asset"
fetch "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected=$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "lxw: checksum mismatch for $asset; not installed" >&2
  exit 1
fi

mkdir -p "$dir"
chmod +x "$tmp/$asset"
mv "$tmp/$asset" "$dir/lxw"
echo "Installed $("$dir/lxw" --version) to $dir/lxw"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH to run lxw." ;;
esac

#!/bin/sh
# Builds a fully static Linux executable (musl) in Docker.
# Usage: scripts/build-linux.sh [arm64|amd64]   (default: the Docker host's architecture)
# Output: dist/lxw-linux-<arch>
# amd64 on an arm64 host runs under emulation and is much slower.
set -eu
cd "$(dirname "$0")/.."
arch="${1:-$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/; s/x86_64/amd64/')}"
mkdir -p dist
docker run --rm --platform "linux/$arch" \
  -v "$PWD":/src -v "${CARGO_HOME:-$HOME/.cargo}/registry":/usr/local/cargo/registry -w /src \
  rust:1.97-alpine sh -c "
    apk add --no-cache musl-dev gcc g++ make cmake perl >/dev/null &&
    cargo build --release --locked --target-dir target/linux-$arch &&
    cp target/linux-$arch/release/lxw dist/lxw-linux-$arch"
echo "built dist/lxw-linux-$arch"

#!/usr/bin/env bash
# Build the Rust engine (static aarch64 musl) and package the Kodi add-on zip.
#
#   ./build.sh            -> dist/service.hueambi-<version>.zip
#
# Runs in a rust:1-alpine container. On x86 hosts Docker needs arm64 emulation
# (Docker Desktop has it, on Linux install qemu-user-static); an arm64 host is much faster.
set -euo pipefail
cd "$(dirname "$0")"

VERSION=$(sed -n 's/.*<addon [^>]*version="\([^"]*\)".*/\1/p' service.hueambi/addon.xml | head -n1)
[ -n "$VERSION" ] || { echo "version not found in service.hueambi/addon.xml" >&2; exit 1; }

docker run --rm --platform "${PLATFORM:-linux/arm64}" \
  -v "$PWD:/repo" -v hue-ambi-cargo:/usr/local/cargo/registry \
  -e VERSION="$VERSION" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -w /repo/engine rust:1-alpine sh -euc '
    apk add --no-cache musl-dev perl make gcc linux-headers zip >/dev/null
    cargo test --release
    cargo build --release
    install -Dm755 target/release/hue-ambi ../service.hueambi/bin/hue-ambi
    cd ..
    mkdir -p dist
    rm -f "dist/service.hueambi-$VERSION.zip"
    zip -qr -X "dist/service.hueambi-$VERSION.zip" service.hueambi \
      -x "*/__pycache__/*" "*.pyc" "service.hueambi/bin/.gitkeep"
    chown -R "$HOST_UID:$HOST_GID" engine/target service.hueambi/bin dist
  '

echo "dist/service.hueambi-$VERSION.zip"

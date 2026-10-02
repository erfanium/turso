#!/usr/bin/env bash
# Builds the native addon from the Turso workspace this package lives in and
# places it under prebuilds/<platform>-<arch>/.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
workspace="$(cd "$here/../../.." && pwd)"

cargo build --release -p turso_mysql_node --manifest-path "$workspace/Cargo.toml"

case "$(uname -s)" in
  Linux) platform=linux; lib=libturso_mysql_node.so ;;
  Darwin) platform=darwin; lib=libturso_mysql_node.dylib ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

out="$here/prebuilds/$platform-$arch"
mkdir -p "$out"
cp "$workspace/target/release/$lib" "$out/mylite.node"
if [ "$platform" = linux ]; then
  strip --strip-unneeded "$out/mylite.node"
else
  strip -x "$out/mylite.node"
fi
ls -la "$out/mylite.node"

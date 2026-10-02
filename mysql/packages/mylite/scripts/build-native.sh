#!/usr/bin/env bash
# Builds the native addon from the Turso workspace this package lives in and
# places it under prebuilds/<platform>-<arch>/.
#
# On Linux the published binary must load on older distributions than the one
# it was built on, so it is linked against glibc 2.17 with cargo-zigbuild
# (`pip install ziglang cargo-zigbuild`). Set MYLITE_NATIVE_GLIBC=host to link
# against the build machine's glibc instead.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
workspace="$(cd "$here/../../.." && pwd)"
manifest="$workspace/Cargo.toml"

case "$(uname -m)" in
  x86_64) arch=x64; rust_arch=x86_64 ;;
  aarch64 | arm64) arch=arm64; rust_arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

case "$(uname -s)" in
  Linux)
    platform=linux
    if [ "${MYLITE_NATIVE_GLIBC:-2.17}" = host ]; then
      cargo build --release -p turso_mysql_node --manifest-path "$manifest"
      built="$workspace/target/release/libturso_mysql_node.so"
    else
      triple="$rust_arch-unknown-linux-gnu"
      # RUSTFLAGS replaces the workspace's Linux linker flags, one of which
      # zig's linker does not accept.
      RUSTFLAGS="--cfg=tokio_unstable -C link-args=-Wl,-z,nodelete" \
        cargo zigbuild --release -p turso_mysql_node --manifest-path "$manifest" \
        --target "$triple.${MYLITE_NATIVE_GLIBC:-2.17}"
      built="$workspace/target/$triple/release/libturso_mysql_node.so"
    fi
    ;;
  Darwin)
    platform=darwin
    cargo build --release -p turso_mysql_node --manifest-path "$manifest"
    built="$workspace/target/release/libturso_mysql_node.dylib"
    ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

out="$here/prebuilds/$platform-$arch"
mkdir -p "$out"
cp "$built" "$out/mylite.node"
if [ "$platform" = linux ]; then
  strip --strip-unneeded "$out/mylite.node"
else
  strip -x "$out/mylite.node"
fi
ls -la "$out/mylite.node"

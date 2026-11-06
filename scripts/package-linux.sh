#!/usr/bin/env bash
# Build both GNU/Linux archives locally. No upload or tap modification.
# On macOS, Docker builds on a Linux filesystem from a source snapshot.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$PWD/target"
# Keep developer home/source paths out of panic locations embedded in binaries.
# Cargo treats plain RUSTFLAGS as whitespace-separated; preserve that convention.
if [ -z "${CARGO_ENCODED_RUSTFLAGS+x}" ] && [ -n "${RUSTFLAGS:-}" ]; then
  CARGO_ENCODED_RUSTFLAGS=$(python3 -c 'import os; print(chr(31).join(os.environ["RUSTFLAGS"].split()), end="")')
fi
export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+${CARGO_ENCODED_RUSTFLAGS}$'\x1f'}--remap-path-prefix=$HOME=/home/build"$'\x1f'"--remap-path-prefix=$PWD=/tildy"
export COPYFILE_DISABLE=1
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
OUT=${1:-target/release-assets/$VERSION}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
if [ -z "${TILDY_LICENSE_DIR:-}" ]; then
  TILDY_LICENSE_DIR="$OUT/notices"
  mkdir -p "$TILDY_LICENSE_DIR"
  cp LICENSE NOTICE "$TILDY_LICENSE_DIR/"
  rm -rf "$TILDY_LICENSE_DIR/licenses"
  cp -R licenses "$TILDY_LICENSE_DIR/licenses"
  python3 scripts/collect-licenses.py "$TILDY_LICENSE_DIR/licenses/dependencies"
fi

if [ "$(uname -s)" != Linux ]; then
  IMAGE=${TILDY_LINUX_BUILDER_IMAGE:-tildy-linux-builder:rust-1.95.0}
  if [ -z "${TILDY_LINUX_BUILDER_IMAGE:-}" ]; then
    docker build -f tests/linux/Dockerfile -t "$IMAGE" tests/linux >&2
  fi
  tar --no-xattrs -cf - Cargo.toml Cargo.lock src shell schema fixtures scripts/package-linux.sh scripts/package-archive.py -C "$TILDY_LICENSE_DIR" LICENSE NOTICE licenses |
    docker run --rm --init -i --cpus 4 -e CARGO_BUILD_JOBS=4 "$IMAGE" bash -c \
      'set -euo pipefail; tar -xf -; TILDY_LICENSE_DIR=/work bash scripts/package-linux.sh /work/assets >&2; tar -C /work/assets -cf - .' |
    tar -C "$OUT" -xf -
  exit 0
fi

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-x86_64-linux-gnu-gcc}
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}
for target in aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  cargo build --locked --release --target "$target"
  python3 scripts/package-archive.py "$OUT/tildy-$VERSION-$target.tar.gz" "target/$target/release/tildy" "$TILDY_LICENSE_DIR"
done

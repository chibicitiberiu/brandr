#!/bin/sh
# Build static brandr binaries (musl, no runtime dependencies) in containers:
#   i586   -> 32-bit PCs, Pentium/MMX and later (no SSE, no CMOV)
#   x86_64 -> 64-bit PCs
# Output: dist/<arch>/brandr. Needs docker (or podman as docker).
# Uses the rust-musl-cross images, which carry a real musl cross gcc for the C
# dependencies (bzip2, xz).
set -eu
HERE=$(cd "$(dirname "$0")/.." && pwd)
DOCKER=${DOCKER:-docker}
CACHE=${CACHE:-$HERE/target/static-cache}
TARGETS=${*:-i586 x86_64}
mkdir -p "$CACHE"

for arch in $TARGETS; do
    triple=$arch-unknown-linux-musl
    echo "== $triple"
    cflags=""
    [ "$arch" = i586 ] && cflags="-march=i586 -mtune=pentium -mno-sse -mno-sse2 -mfpmath=387"
    "$DOCKER" run --rm -u "$(id -u):$(id -g)" \
        -e CARGO_HOME=/cache/cargo-home -e "CFLAGS_${arch}_unknown_linux_musl=$cflags" \
        -v "$HERE:/src" -v "$CACHE:/cache" -w /src \
        "ghcr.io/rust-cross/rust-musl-cross:$arch-musl" \
        cargo build --release --locked --target "$triple" --target-dir /cache/target
    mkdir -p "$HERE/dist/$arch"
    cp "$CACHE/target/$triple/release/brandr" "$HERE/dist/$arch/brandr"
    ls -l "$HERE/dist/$arch/brandr"
done

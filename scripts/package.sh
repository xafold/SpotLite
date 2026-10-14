#!/bin/bash
# A release: the app and the engine built for it, with the libraries they run on, the install script, README.md,
# LICENSE and the agent skill (SKILL.md), in dist/spotlite-<version>-linux-<arch>.tar.gz. Source paths are written in
# as /spotlite and /cargo, and no file may name the home folder it was built in (it stops if one does).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
VERSION=$(grep -m1 '^version' app/src-tauri/Cargo.toml | cut -d'"' -f2)
NAME=spotlite-$VERSION-linux-$(uname -m)
scripts/fetch-pdfium.sh
scripts/fetch-layer-shell.sh
scripts/fetch-vulkan.sh || true   # without it, a CPU-only llama.cpp
scripts/build-llama.sh

# Built in a folder of its own (Tauri writes the app's source folder into it, which compiler flags can't change), its
# path written in as /spotlite.
BUILD_ROOT=${SPOTLITE_BUILD_ROOT:-/tmp/spotlite-build}
mkdir -p "$BUILD_ROOT"
rsync -a --delete --exclude target --exclude gen engine app "$BUILD_ROOT/"
ln -sfn "$ROOT/Vendor" "$BUILD_ROOT/Vendor"
CARGO=${CARGO_HOME:-$HOME/.cargo}
RUSTUP=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="--remap-path-prefix=$BUILD_ROOT=/spotlite --remap-path-prefix=$ROOT=/spotlite --remap-path-prefix=$CARGO=/cargo --remap-path-prefix=$RUSTUP=/rustup"
export CFLAGS="-ffile-prefix-map=$BUILD_ROOT=/spotlite -ffile-prefix-map=$ROOT=/spotlite -ffile-prefix-map=$CARGO=/cargo"
export CXXFLAGS="$CFLAGS"
export SPOTLITE_PACKAGE=1 CARGO_TARGET_DIR="$BUILD_ROOT/target"
cargo build --release --quiet --manifest-path "$BUILD_ROOT/engine/Cargo.toml"
cargo build --release --quiet --manifest-path "$BUILD_ROOT/app/src-tauri/Cargo.toml"

OUT=dist/$NAME
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$BUILD_ROOT/target/release/spotlite" "$BUILD_ROOT/target/release/spotlite-app" "$OUT/"
strip "$OUT/spotlite" "$OUT/spotlite-app"
cp -a Vendor/llama/lib/*.so* Vendor/pdfium/lib/libpdfium.so Vendor/gtk-layer-shell/*.so* "$OUT/"
cp app/src-tauri/icons/128x128.png "$OUT/spotlite.png"
cp scripts/install.sh README.md LICENSE skills/spotlite/SKILL.md "$OUT/"
if grep -rl --binary-files=text -e "$HOME" "$OUT"; then
  echo "these files name $HOME: not packaged" >&2
  exit 1
fi
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "dist/$NAME.tar.gz ($(du -h "dist/$NAME.tar.gz" | cut -f1))"

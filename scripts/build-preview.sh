#!/bin/sh
# Builds the page's preview module (crates/preview) into app/public/preview/ with wasm-bindgen.
# Uses `wasm-bindgen` from PATH, or the copy worker-build downloads (same version as Cargo.lock).
set -e
cd "$(dirname "$0")/.."
version=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/"/, "", $3); print $3 }' Cargo.lock)
bindgen=$(command -v wasm-bindgen || true)
if [ -z "$bindgen" ]; then
  bindgen=$(ls "$HOME"/Library/Caches/worker-build/wasm-bindgen-*-"$version"/wasm-bindgen "$HOME"/.cache/worker-build/wasm-bindgen-*-"$version"/wasm-bindgen 2>/dev/null | head -1)
fi
[ -n "$bindgen" ] || { echo "wasm-bindgen $version not found: cargo install wasm-bindgen-cli --version $version" >&2; exit 1; }
cargo build -p preview --release --target wasm32-unknown-unknown
"$bindgen" --target web --no-typescript --out-dir app/public/preview target/wasm32-unknown-unknown/release/preview.wasm
ls -l app/public/preview

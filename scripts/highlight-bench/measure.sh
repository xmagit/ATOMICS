#!/usr/bin/env bash
# Measures the syntect regex engines for the wasm highlighter choice.
# Usage: scripts/highlight-bench/measure.sh [sample files...]   (default: a few of this repository's sources)
# Builds with `-j 2`; run it on a quiet machine.
set -euo pipefail
cd "$(dirname "$0")"
export CARGO_TARGET_DIR="$PWD/target"   # not the shared one: the sizes are of this crate alone
root=../..
samples=("$@")
[ ${#samples[@]} -gt 0 ] || samples=("$root/crates/atomics/src/app.rs" "$root/crates/atomics-core/src/config.rs" "$root/README.md" "$root/scripts/highlight-bench/measure.sh" "$root/Cargo.toml")
for engine in onig fancy; do
  echo "== native, $engine"
  env -u CARGO_MAKEFLAGS cargo build --release -j 2 --features "$engine" --bin bench
  ./target/release/bench "${samples[@]}"
  ls -l target/release/bench | awk '{print "native_binary_bytes", $5}'
done
echo "== wasm32, fancy (onig is C and does not build for wasm32-unknown-unknown)"
env -u CARGO_MAKEFLAGS cargo build --release -j 2 --features fancy --lib --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/highlight_bench.wasm
ls -l "$wasm" | awk '{print "wasm_bytes", $5}'
gzip -9 -c "$wasm" | wc -c | awk '{print "wasm_gzip_bytes", $1}'

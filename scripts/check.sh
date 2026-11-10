#!/bin/sh
# The workspace gate, with the pinned Nix toolchain and an ATOMICS test slot.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

if [ "${1:-}" = --run-gate ]; then
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --no-fail-fast --locked
  exit
fi
[ "$#" -eq 0 ] || { echo 'Usage: scripts/check.sh' >&2; exit 2; }

if command -v atomics >/dev/null 2>&1; then
  exec atomics queue --class test -- nix --extra-experimental-features 'nix-command flakes' \
    develop .#default --command sh "$root/scripts/check.sh" --run-gate
fi
[ -z "${ATOMICS_SOCKET:-}${ATOMICS_HOST_SOCKET:-}" ] || {
  echo 'ATOMICS is running, but atomics is missing from PATH; cannot acquire a test slot.' >&2
  exit 1
}
exec nix --extra-experimental-features 'nix-command flakes' \
  develop .#default --command sh "$root/scripts/check.sh" --run-gate

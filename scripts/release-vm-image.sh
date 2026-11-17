#!/usr/bin/env bash
# Build unsigned VM release assets on an aarch64-linux Nix builder. No release secret belongs here.
# The release workflow signs the exact .raw.zst and .bundle.json bytes with sign-release.sh.
# Usage: bash scripts/release-vm-image.sh [OUTPUT_DIRECTORY] [standard|hardened]
set -euo pipefail
flavor=${2:-standard}
case "$flavor" in
  standard) attr=atomics-vm-release; prefix=atomics-vm ;;
  hardened) attr=atomics-vm-release-hardened; prefix=atomics-vm-hardened ;;
  *) echo "release-vm-image: unknown flavor: $flavor" >&2; exit 64 ;;
esac
for tool in nix zstd python3 sha256sum; do
  command -v "$tool" >/dev/null || { echo "release-vm-image: needs $tool on PATH" >&2; exit 69; }
done
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
out=${1:-"$repo/target/vm-release"}
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1)
if [ "${GITHUB_REF_TYPE:-}" = tag ] && [ "$GITHUB_REF_NAME" != "v$version" ]; then
  echo 'Release tag must match workspace.package.version' >&2; exit 1
fi
image=$(nix build "$repo#$attr-image" --no-link --print-out-paths)
bundle=$(nix build "$repo#$attr-bundle" --no-link --print-out-paths)
name=$prefix-$version-aarch64-linux
# Compress zero extents without first copying the sparse raw disk; setup restores holes while
# decompressing. zstd's content size records the full disk length, not its allocated blocks.
zstd -T0 -6 -f "$image/atomics-vm.raw" -o "$out/$name.raw.zst"
python3 - "$bundle" "$out" "$name" "$version" "$image/atomics-vm.raw" "$flavor" <<'PY'
import hashlib
import json
from pathlib import Path
import sys
bundle, out, name, version, raw, flavor = sys.argv[1:]
bundle, out = Path(bundle), Path(out)
image = out / f"{name}.raw.zst"
with image.open('rb') as source:
    digest = hashlib.file_digest(source, 'sha256').hexdigest()
files = {}
for path in sorted(bundle.rglob('*')):
    if path.is_file():
        content = path.read_text()
        if '/nix/store' in content:
            raise SystemExit('Mac bundle contains a Nix store reference')
        files[str(path.relative_to(bundle))] = {
            'contents': content, 'executable': bool(path.stat().st_mode & 0o111)
        }
manifest = {'version': 1, 'releaseVersion': version, 'image': image.name,
            'imageSha256': digest, 'imageBytes': Path(raw).stat().st_size, 'files': files}
# Keep standard manifests readable by installed versions predating flavor selection.
if flavor != 'standard':
    manifest['flavor'] = flavor
(out / f'{name}.bundle.json').write_text(json.dumps(manifest, indent=2) + '\n')
for asset in [image, out / f'{name}.bundle.json']:
    with asset.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    Path(str(asset) + '.sha256').write_text(f'{digest}  {asset.name}\n')
PY
printf 'Unsigned VM release assets: %s\n' "$out"

#!/bin/sh
# Installs the latest ATOMICS release for this user:
#
#   curl -fsSL https://raw.githubusercontent.com/xmagit/ATOMICS/main/scripts/install.sh | sh
#
# It checks the download's SHA-256 and uses OpenSSL 3 to verify its signature against the release keys on the
# repository's main branch before unpacking. Pass --skip-signature or set ATOMICS_SKIP_SIGNATURE=1 for a checksum-only
# install. By default it asks nothing and the program opens setup on its first run. Pass --simple to open Simple setup
# after installation on Apple Silicon. Whatever it replaces is kept (`atomics rollback` goes back).
# ATOMICS_VERSION=0.1.7 installs that version instead of the latest.
set -eu

repo=xmagit/ATOMICS
# The trust anchors. Not from the release, whose files they are there to check.
keys_url="https://raw.githubusercontent.com/$repo/main/assets"
key_files='release-signing-key.pub release-signing-key-offline.pub'

fail() {
  printf 'atomics install: %s\n' "$*" >&2
  exit 1
}

fetch() { # url, file
  curl --proto '=https' --tlsv1.2 -fsSL --retry 2 -o "$2" "$1"
}

target_of() {
  case "$(uname -s) $(uname -m)" in
    'Linux x86_64') echo x86_64-unknown-linux-gnu ;;
    'Darwin arm64') echo aarch64-apple-darwin ;;
    *) fail "there is no build for $(uname -s) $(uname -m) yet. Build it from source: https://github.com/$repo#run" ;;
  esac
}

latest_tag() {
  fetch "https://api.github.com/repos/$repo/releases/latest" "$work/latest.json" ||
    fail "could not ask GitHub for the latest release. Is this machine online?"
  tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$work/latest.json" | head -n 1)
  [ -n "$tag" ] || fail "GitHub's answer names no release."
  echo "$tag"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    fail "neither sha256sum nor shasum is here, so the download cannot be checked."
  fi
}

check_sha256() { # archive, sidecar
  listed=$(cut -d' ' -f1 "$2")
  named=$(sed 's/^[^ ]*  *\*\{0,1\}//' "$2")
  [ "$named" = "$(basename "$1")" ] || fail "the release's checksum file names another file."
  [ "$(sha256_of "$1")" = "$listed" ] || fail "the download is damaged: its SHA-256 does not match the release's."
}

# OpenSSL 3 is what verifies a raw Ed25519 signature; macOS's own openssl is LibreSSL, which does not.
openssl3() {
  for candidate in openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
    if "$candidate" version 2>/dev/null | grep -q '^OpenSSL [3-9]'; then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

check_signature() { # archive, signature
  found=no
  for name in $key_files; do
    fetch "$keys_url/$name" "$work/$name" 2>/dev/null || continue
    found=yes
    if "$openssl" pkeyutl -verify -rawin -pubin -inkey "$work/$name" -in "$1" -sigfile "$2" >/dev/null 2>&1; then
      return 0
    fi
  done
  [ "$found" = yes ] || fail "could not read the release keys from $keys_url, so the signature cannot be checked."
  fail "the download is not signed with an ATOMICS release key."
}

main() {
  simple=no
  skip_signature=${ATOMICS_SKIP_SIGNATURE:-0}
  for argument in "$@"; do
    case "$argument" in
      --simple) simple=yes ;;
      --skip-signature) skip_signature=1 ;;
      -h|--help)
        printf '%s\n' 'Usage: install.sh [--simple] [--skip-signature]' \
          '--simple opens optional hardened-VM setup on an Apple Silicon Mac.' \
          '--skip-signature checks only the SHA-256. ATOMICS_SKIP_SIGNATURE=1 has the same effect.'
        return ;;
      *) fail "unknown option: $argument (see --help)" ;;
    esac
  done
  command -v curl >/dev/null 2>&1 || fail "curl is needed. Install it with your package manager, then run this again."
  command -v tar >/dev/null 2>&1 || fail "tar is needed. Install it with your package manager, then run this again."
  target=$(target_of)
  if [ "$simple" = yes ] && [ "$target" != aarch64-apple-darwin ]; then
    fail "Simple VM setup requires an Apple Silicon Mac. Run without --simple for Full setup."
  fi
  if [ "$skip_signature" != 1 ] && ! openssl=$(openssl3); then
    install_openssl="Install OpenSSL 3 with your package manager."
    if [ "$target" = aarch64-apple-darwin ]; then
      install_openssl="Install OpenSSL 3 with Homebrew.
  brew install openssl@3"
    fi
    retry="curl -fsSL https://raw.githubusercontent.com/$repo/main/scripts/install.sh |"
    if [ -n "${ATOMICS_VERSION:-}" ]; then
      retry_version=$(printf '%s' "$ATOMICS_VERSION" | sed "s/'/'\"'\"'/g")
      retry="$retry ATOMICS_VERSION='$retry_version'"
    fi
    retry="$retry sh"
    if [ "$simple" = yes ]; then
      retry="$retry -s -- --simple"
    fi
    fail "OpenSSL 3 is required to verify the release signature.
$install_openssl
Then rerun the installer.
  $retry"
  fi
  work=$(mktemp -d)
  trap 'rm -rf -- "$work"' EXIT
  trap 'exit 1' HUP INT TERM
  if [ -n "${ATOMICS_VERSION:-}" ]; then
    tag="v${ATOMICS_VERSION#v}"
  else
    tag=$(latest_tag)
  fi
  version=${tag#v}
  archive="atomics-$version-$target.tar.gz"
  base="https://github.com/$repo/releases/download/$tag"
  printf 'Downloading ATOMICS %s for %s\n' "$version" "$target"
  fetch "$base/$archive" "$work/$archive" || fail "release $tag has no build for $target."
  fetch "$base/$archive.sha256" "$work/$archive.sha256" || fail "release $tag has no checksum for $archive."
  check_sha256 "$work/$archive" "$work/$archive.sha256"
  if [ "$skip_signature" = 1 ]; then
    printf '%s\n' "atomics install: warning: the download's authenticity was not checked. Only its SHA-256 was verified." >&2
  else
    fetch "$base/$archive.sig" "$work/$archive.sig" || fail "release $tag has no signature for $archive."
    check_signature "$work/$archive" "$work/$archive.sig"
  fi
  mkdir "$work/unpacked"
  tar --no-same-owner -xzf "$work/$archive" -C "$work/unpacked" || fail "the release could not be unpacked."
  case "$target" in
    *-linux-*) program="$work/unpacked/atomics" ;;
    *) program="$work/unpacked/ATOMICS.app/Contents/MacOS/atomics" ;;
  esac
  [ -x "$program" ] || fail "the release holds no program."
  # With `curl | sh` this shell's stdin is the rest of this script, which nothing it runs may read.
  said=$("$program" --version 2>&1 </dev/null) || fail "the program does not run on this machine: $said
It needs glibc 2.35 or newer and libasound2. Install those with your package manager, or build it from source."
  case "$said" in
    "atomics $version "*) ;;
    *) fail "release $tag holds a program that says: $said" ;;
  esac
  # It installs itself from here, so that an update and a download put the same things in the same places.
  if [ "$simple" = yes ]; then
    "$program" install --simple </dev/null
  else
    "$program" install </dev/null
  fi
}

main "$@"

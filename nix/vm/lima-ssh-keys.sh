#!/usr/bin/env bash
set -euo pipefail
data=$1 dir=$2 user=$3
shift 3

# Validate the whole list before replacing either account's working key file.
if ! keys=$(yq -o=json '.' "$data" | jq -er --arg user "$user" '
  [.users | arrays | .[] | objects | select(.name == $user)] | select(length == 1) | .[0]."ssh-authorized-keys"
  | arrays | select(length > 0 and all(.[]; type == "string" and length > 0 and (test("[\u0000\r\n]") | not)))
  | .[]'); then
  echo "Missing or malformed SSH keys for $user in Lima cidata" >&2
  exit 1
fi
while IFS= read -r key; do
  read -r algorithm blob comment <<< "$key"
  if ! ssh-keygen -lf - <<< "$algorithm $blob" >/dev/null; then
    echo "Invalid SSH public key for $user in Lima cidata" >&2
    exit 1
  fi
done <<< "$keys"

install -d -m 0755 "$dir"
for login in "$user" "$@"; do
  tmp=$(mktemp "$dir/.lima-keys.XXXXXXXX")
  trap 'rm -f -- "$tmp"' EXIT
  printf '%s\n' "$keys" > "$tmp"
  chmod 0444 "$tmp"
  mv -fT -- "$tmp" "$dir/$login"
done

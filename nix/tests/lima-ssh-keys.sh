#!/usr/bin/env bash
# Run the production installer against a directory private to the check.
set -euo pipefail
installer=$1
ssh-keygen -q -t ed25519 -N '' -f first
ssh-keygen -q -t ed25519 -N '' -f second
key=$(cat first.pub)
provision() { bash "$installer" user-data keys guest admin; }
cidata() { jq -n --argjson keys "$1" '{users: [{name: "guest", "ssh-authorized-keys": $keys}]}' > user-data; }

cidata "$(jq -cn --arg key "$key" '[$key]')"
provision
for input in 'null' '[]' '""' '[null]' '[""]' '["invalid"]' \
  "$(jq -cn --arg k "$key" '[$k, null]')" \
  "$(jq -cn --arg k "$key" '[$k, "invalid"]')" \
  "$(jq -cn --arg k "$key" '[$k + "\n" + $k]')"; do
  cidata "$input"
  if provision; then echo "accepted malformed cidata" >&2; exit 1; fi
  cmp keys/guest first.pub
  cmp keys/admin first.pub
done
for invalid in '{}' '{"users":[{"name":"guest"}]}' '{"users":[{"name":"other"}]}'; do
  echo "$invalid" > user-data
  if provision; then echo "accepted missing login keys" >&2; exit 1; fi
done

cidata "$(jq -cn --arg key "$(cat second.pub)" '[$key]')"
provision
for login in guest admin; do
  cmp "keys/$login" second.pub
  test "$(stat -c %a "keys/$login")" = 444
done

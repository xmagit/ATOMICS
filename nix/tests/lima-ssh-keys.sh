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
for invalid in '{}' '{"users":[{"name":"guest"}]}' '{"users":[{"name":"other"}]}' \
  "$(jq -cn --arg k "$key" '{users: {guest: {name: "guest", "ssh-authorized-keys": [$k]}}}')" \
  "$(jq -cn --arg k "$key" '{name: "guest", "ssh-authorized-keys": [$k]} as $u | {users: [$u, $u]}')"; do
  echo "$invalid" > user-data
  if provision; then echo "accepted missing login keys" >&2; exit 1; fi
done

jq -n --arg first "$key" --arg second "$(cat second.pub)" \
  '{users: ["default", {name: "other"}, {name: "guest", "ssh-authorized-keys": [$first, $second]}]}' > user-data
provision
cat first.pub second.pub > expected
cmp keys/guest expected
cmp keys/admin expected

cidata "$(jq -cn --arg key "$(cat second.pub)" '[$key]')"
provision
for login in guest admin; do
  cmp "keys/$login" second.pub
  test "$(stat -c %a "keys/$login")" = 444
done

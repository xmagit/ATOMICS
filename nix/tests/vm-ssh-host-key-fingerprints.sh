#!/bin/sh
# Run inside the guest; optional arguments select public host-key files.
set -eu
if [ "$#" -eq 0 ]; then
  set -- /etc/ssh/ssh_host_*_key.pub
fi
for key do
  ssh-keygen -lf "$key" -E sha256
done

{ pkgs, standard, hardened }:
assert !standard.atomics.vm.hardened;
assert hardened.atomics.vm.flavor == "hardened";
assert hardened.atomics.vm.hardened && hardened.atomics.vm.host.enable;
assert !hardened.atomics.vm.egress.enable;
assert hardened.virtualisation.docker.rootless.enable;
assert !hardened.virtualisation.docker.enable;
assert !builtins.elem "wheel" hardened.users.users.atomics.extraGroups;
assert !builtins.elem "docker" hardened.users.users.atomics.extraGroups;
assert hardened.programs.atomics.macDesktop.noexecShares;
pkgs.runCommand "atomics-hardened-release-test" {
  nativeBuildInputs = [ pkgs.python3 ];
} ''
  python3 - ${hardened.programs.atomics.macDesktop.bundle} <<'PY'
  import json, pathlib, sys
  bundle = pathlib.Path(sys.argv[1])
  info = json.loads((bundle/'atomics-vm.json').read_text())
  assert info['flavor'] == 'hardened' and info['admin'] == 'admin' and info['host']['port'] == 47800, info
  mounts = json.loads((bundle/'lima.yaml').read_text())['mounts']
  repos = [m for m in mounts if m['location'].endswith('/repos')]
  assert repos and all(not m['writable'] for m in repos), mounts
  assert all(m['location'].endswith('/atomics-exchange') for m in mounts if m['writable']), mounts
  PY
  mkdir "$out"
''

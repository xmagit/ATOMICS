# Check the actual TOML files copied on first boot, including the account-independent release image.
{ pkgs, vm, vmWithHost, vmRelease, vmWithBroker, desktop }:

let
  inherit (pkgs) lib;
  seed = config:
    let unit = config.systemd.services.atomics-user-setup;
    in assert unit.serviceConfig.User == config.programs.atomics.user;
       unit.environment.ATOMICS_CONFIG_SEED;
  tools = map lib.getName vm.environment.systemPackages;
  checkKey = pkgs.callPackage ../vm/check-authorized-key.nix { openssh = vm.services.openssh.package; };
  tmpfiles = config: [
    "${config.system.build.etc}/etc/tmpfiles.d"
    config.users.users.${config.atomics.vm.user}.home
  ];
in
assert vm.services.cloud-init.settings.cloud_init_modules == [ "seed_random" ];
assert !vm.services.openssh.authorizedKeysInHomedir;
assert lib.all (cfg: lib.all (unit: lib.elem "atomics-ssh-keys.service" cfg.systemd.services.${unit}.requires)
  [ "lima-ready" "atomics-host" ]) [ vmRelease vmWithBroker ];
assert lib.all (cfg:
  let ssh = cfg.systemd.services."sshd@";
  in lib.elem "atomics-ssh-keys.service" ssh.wants && lib.elem "atomics-ssh-keys.service" ssh.after
    && !lib.elem "atomics-ssh-keys.service" ssh.requires && ssh.serviceConfig ? ExecCondition
) [ vmRelease vmWithBroker ];
assert !lib.elem "lima-ready.service" vm.systemd.services.atomics-user-setup.requiredBy;
assert lib.all (name: builtins.elem name tools) [ "gcc-wrapper" "pkg-config-wrapper" "python3" "coreutils-full" "sccache" ];
assert vm.programs.atomics.settings == { };
assert vmWithHost.systemd.services.atomics-host.serviceConfig.CPUWeight == 1000;
assert vmWithHost.systemd.services.atomics-host.serviceConfig.OOMScoreAdjust == -900;
assert vmWithHost.systemd.services.atomics-host.serviceConfig.OOMPolicy == "continue";
assert !(vmWithHost.systemd.services.atomics-host.serviceConfig ? RuntimeDirectory);
assert lib.elem "d /run/atomics 0700 ${vmWithHost.atomics.vm.user} users -" vmWithHost.systemd.tmpfiles.rules;
assert vmWithHost.systemd.services.atomics-host.serviceConfig ? ExecStopPost;
assert vm.security.polkit.enable && desktop.security.polkit.enable;
assert builtins.elem "/share/polkit-1" vm.environment.pathsToLink;
assert builtins.elem "atomics-secret-copy-policy" tools;
pkgs.runCommand "atomics-vm-config-test" {
  nativeBuildInputs = [ pkgs.python3 pkgs.coreutils-full pkgs.openssh pkgs.yq-go pkgs.jq checkKey ];
} ''
  bash ${./lima-ssh-keys.sh} ${../vm/lima-ssh-keys.sh}

  # Inspect the system tmpfiles directories shipped to both guests, including package rules.
  python3 - ${lib.escapeShellArgs (lib.concatMap tmpfiles [ vmRelease vmWithBroker ])} <<'PY'
  import shlex
  import sys
  from pathlib import Path

  for directory, home in zip(sys.argv[1::2], sys.argv[2::2], strict=True):
      rules = list(Path(directory).glob("*.conf"))
      assert rules, directory
      for file in rules:
          for line in file.read_text().splitlines():
              fields = shlex.shlex(line, posix=True)
              fields.whitespace_split = True
              if next(fields, None) is None:
                  continue
              target = Path(next(fields))
              if target.is_absolute():
                  # A rule such as /home/*/repos also targets this user's home.
                  assert not any(Path(home).match(str(p)) for p in (target, *target.parents)), (file, line)
  PY

  python3 - ${seed vmWithHost} ${seed vmRelease} ${seed desktop} <<'PY'
  import sys
  import tomllib

  for path in sys.argv[1:3]:
      with open(path, "rb") as file:
          config = tomllib.load(file)
      assert config["isolation"]["mode"] == "bare", config
      agents = {agent["name"]: agent for agent in config["agents"]}
      assert set(agents) == {"claude", "codex", "shell"}, agents
      assert agents["codex"]["command"] == ["codex", "--dangerously-bypass-approvals-and-sandbox"], agents
      assert agents["codex"]["kind"] == "generic", agents
      assert agents["codex"]["busy_text"] == "esc to interrupt", agents
      assert agents["claude"]["command"] == ["claude"] and agents["claude"]["kind"] == "claude", agents
      assert agents["shell"]["command"] == [] and agents["shell"]["kind"] == "shell", agents

  with open(sys.argv[3], "rb") as file:
      desktop = tomllib.load(file)
  assert "agents" not in desktop and "isolation" not in desktop, desktop
  PY
  probe=$(mktemp)
  test -f "$probe"
  rm "$probe"
  mkdir "$out"
''

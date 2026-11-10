# Check the actual TOML files copied on first boot, including the account-independent release image.
{ pkgs, vm, vmWithHost, vmRelease, desktop }:

let
  inherit (pkgs) lib;
  seed = config:
    let
      user = config.programs.atomics.user;
      home = config.users.users.${user}.home;
      # C creates a missing config without overwriting an existing one; keep its private mode.
      rule = lib.findFirst (lib.hasPrefix "C ${home}/.config/atomics/config.toml 0600 ${user} ")
        (throw "missing first-config seed") config.systemd.tmpfiles.rules;
    in lib.last (lib.splitString " " rule);
  tools = map lib.getName vm.environment.systemPackages;
in
assert lib.all (name: builtins.elem name tools) [ "gcc-wrapper" "pkg-config-wrapper" "python3" "coreutils-full" ];
assert vm.programs.atomics.settings == { };
assert vm.security.polkit.enable && desktop.security.polkit.enable;
assert builtins.elem "/share/polkit-1" vm.environment.pathsToLink;
assert builtins.elem "atomics-secret-copy-policy" tools;
pkgs.runCommand "atomics-vm-config-test" {
  nativeBuildInputs = [ pkgs.python3 pkgs.coreutils-full ];
} ''
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

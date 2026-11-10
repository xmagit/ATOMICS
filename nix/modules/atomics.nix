{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.atomics;
  toml = pkgs.formats.toml { };
  hasSeed = cfg.settings != { };
  user = lib.attrByPath [ cfg.user ] null config.users.users;
  # users.users.<name>.group is "" when unset.
  userGroup = if user.group == "" then "users" else user.group;
  configDir = "${user.home}/.config/atomics";
  seedFile = toml.generate "atomics-config.toml" cfg.settings;
  copyPolicy = pkgs.runCommand "atomics-secret-copy-policy" { } ''
    install -Dm644 ${../../assets/com.atomics.copy-secret.policy} "$out/share/polkit-1/actions/com.atomics.copy-secret.policy"
  '';

  # Text settings whose names suggest a credential, which the Nix store would show every local user.
  # Names of variables, files or commands that hold one (`api_key_env`) are fine.
  secretLike = name:
    let lower = lib.toLower name; in
    builtins.match ".*(token|secret|password|passwd|credential|bearer|authorization|api_?key|private_?key).*" lower != null
    && builtins.match ".*_(env|file|path|cmd|command)" lower == null;
  secretPaths = prefix: value:
    lib.concatLists (lib.mapAttrsToList (name: inner:
      let path = prefix ++ [ name ]; in
      lib.optional (builtins.isString inner && secretLike name) (lib.concatStringsSep "." path)
      ++ lib.optionals (builtins.isAttrs inner) (secretPaths path inner)
      ++ lib.optionals (builtins.isList inner)
        (lib.concatMap (item: lib.optionals (builtins.isAttrs item) (secretPaths path item)) inner))
      value);
  suspicious = secretPaths [ ] cfg.settings;
in
{
  imports = [ ./mac-desktop.nix ];

  options.programs.atomics = {
    enable = lib.mkEnableOption "the ATOMICS desktop application and CLI";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.atomics;
      defaultText = lib.literalExpression "self.packages.${pkgs.stdenv.hostPlatform.system}.atomics";
      description = "ATOMICS package to install for the desktop and command-line hooks/MCP.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Existing NixOS user whose ATOMICS config is seeded when settings is non-empty.";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = ''
        Initial TOML configuration copied only when the user's config.toml does not exist.
        The seed is a file in the Nix store, which every user of this machine can read: keep tokens,
        passwords and other secrets out of it and sign in inside the machine instead.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package copyPolicy ];
    # Fresh per-copy authentication, including on a headless VM (whose remote clients still
    # cannot copy). Linking the package's policy into the system path makes polkit load it.
    security.polkit.enable = true;
    environment.pathsToLink = [ "/share/polkit-1" ];
    assertions = [ {
      assertion = !hasSeed || (cfg.user != "" && user != null);
      message = "programs.atomics.user must name an existing NixOS user when programs.atomics.settings is non-empty";
    } ];
    warnings = lib.optional (suspicious != [ ]) ''
      programs.atomics.settings is copied into the world-readable Nix store, and these settings look like
      secrets: ${lib.concatStringsSep ", " suspicious}. Keep credentials out of it; sign in inside the machine.
    '';
    # The `:` prefix applies a mode or owner only when tmpfiles creates the folder, so an existing
    # ~/.config keeps the mode and owner its user gave it. ATOMICS's own folder and seed are private.
    systemd.tmpfiles.rules = lib.optionals (hasSeed && user != null) [
      "d ${user.home}/.config :0700 :${cfg.user} :${userGroup} - -"
      "d ${configDir} 0700 ${cfg.user} ${userGroup} - -"
      "C ${configDir}/config.toml 0600 ${cfg.user} ${userGroup} - ${seedFile}"
    ];
  };
}

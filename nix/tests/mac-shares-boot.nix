# Boots a machine with programs.atomics.macDesktop and checks what root's boot script leaves, with real
# users and reboots: the Mac-home tree is root's and the VM's user can't rename or replace any of it; a
# tree an earlier layout gave that user is taken over at the next boot; and a link that user planted in
# such a tree fails the unit at boot, with its target (/etc here) untouched. Virtiofs is stood in for by
# bind mounts of local folders, with the options the script asks for.
{ pkgs, module }:

let
  standIn = pkgs.writeShellScriptBin "atomics" "echo 'atomics (test stand-in)'";
  # `mount -t virtiofs -o OPTIONS TAG PATH`, as a bind mount of /srv/mac/TAG with those options.
  virtiofs = pkgs.writeShellScriptBin "mount" ''
    set -eu
    if [ "$#" -eq 6 ] && [ "$1" = -t ] && [ "$2" = virtiofs ] && [ "$3" = -o ]; then
      ${pkgs.util-linux}/bin/mount --bind "/srv/mac/$5" "$6"
      exec ${pkgs.util-linux}/bin/mount -o "remount,bind,$4" "$6"
    fi
    exec ${pkgs.util-linux}/bin/mount "$@"
  '';
in
pkgs.testers.runNixOSTest {
  name = "atomics-mac-shares-boot";
  nodes.machine = { lib, ... }: {
    imports = [ module ];
    virtualisation = { memorySize = 1024; cores = 2; };
    users.users.atomics = { isNormalUser = true; uid = 1000; };
    programs.atomics = {
      enable = true;
      package = standIn;
      user = "atomics";
      macDesktop = {
        enable = true;
        user = "alice";
        lima.cidata = "/etc/test-cidata";
        noexecShares = true;
        shares = {
          "/Users/alice/repos" = { };
          "/Users/alice/.atomics/briefs" = { };
        };
      };
    };
    environment.etc."test-cidata/user-data".text = ''
      #cloud-config
      mounts:
      - [repos, /Users/alice/repos, virtiofs, "ro,nosuid,nodev", "0", "0"]
      - [briefs, /Users/alice/.atomics/briefs, virtiofs, "ro,nosuid,nodev", "0", "0"]
    '';
    systemd.tmpfiles.rules = [
      "d /srv/mac/repos 0755 root root -"
      "d /srv/mac/briefs 0755 root root -"
      "f /srv/mac/repos/README 0644 root root - from the Mac"
    ];
    systemd.services.atomics-mac-shares.path = lib.mkBefore [ virtiofs ];
    # Lima's guest agent comes from the real cidata disk.
    systemd.services.lima-guestagent.enable = false;
  };

  testScript = ''
    user = "runuser -u atomics -- "

    def owner_mode(path):
        return machine.succeed(f"stat -c '%U:%G %a' {path}").strip()

    def reboot():
        machine.shutdown()
        machine.start()
        machine.wait_for_unit("multi-user.target")

    def roots_layout():
        machine.wait_for_unit("atomics-mac-shares.service")
        for folder in ["/Users", "/Users/alice", "/Users/alice/.atomics", "/Users/alice/Applications",
                       "/Users/alice/Applications/ATOMICS.app/Contents/MacOS", "/Applications/ATOMICS.app/Contents/MacOS"]:
            assert owner_mode(folder) == "root:root 755", (folder, owner_mode(folder))
        assert owner_mode("/Users/alice/.atomics/forward") == "atomics:users 700"
        machine.succeed("test \"$(readlink /Users/alice/.atomics/atomics.sock)\" = forward/atomics.sock")
        machine.succeed("test \"$(stat -c %U /Users/alice/.atomics/atomics.sock)\" = root")
        machine.succeed("test \"$(readlink -f /Users/alice/Applications/ATOMICS.app/Contents/MacOS/atomics)\" = \"$(readlink -f ${standIn}/bin/atomics)\"")
        for share in ["/Users/alice/repos", "/Users/alice/.atomics/briefs"]:
            options = machine.succeed(f"findmnt -n -o OPTIONS -M {share}").strip().split(",")
            assert all(o in options for o in ["ro", "nosuid", "nodev", "noexec"]), (share, options)
        machine.succeed("test -f /Users/alice/repos/README")
        machine.succeed("grep -qx ATOMICS_MAC_SOCKET_FOLDER=/Users/alice/.atomics/forward /etc/atomics/mac-desktop.env")

    with subtest("the Mac home is root's; the VM user keeps only the socket's folder"):
        roots_layout()
        for command in ["mv /Users/alice/.atomics /Users/alice/.atomics.old", "ln -s /etc /Users/alice/x",
                        "mv /Users/alice/Applications /tmp/elsewhere", "rm /Users/alice/.atomics/atomics.sock",
                        "mkdir /Users/alice/.atomics/briefs2", "rmdir /Users/alice/repos",
                        "mv /Users/alice/.atomics/forward /tmp/forward"]:
            machine.fail(user + command)
        machine.succeed(user + "touch /Users/alice/.atomics/forward/probe")

    # An earlier layout gave the VM user the Mac home and .atomics (the boot script installed them as theirs).
    machine.succeed("umount /Users/alice/repos /Users/alice/.atomics/briefs")
    machine.succeed("chown atomics:users /Users/alice /Users/alice/.atomics && chmod 0700 /Users/alice/.atomics")
    machine.succeed("chown -hR atomics:users /Users/alice/Applications")

    with subtest("a link swap of .atomics to /etc fails the next boot and leaves /etc alone"):
        machine.succeed(user + "mv /Users/alice/.atomics /Users/alice/.atomics.old")
        machine.succeed(user + "ln -s /etc /Users/alice/.atomics")
        reboot()
        machine.wait_until_succeeds("systemctl is-failed atomics-mac-shares.service")
        assert owner_mode("/etc") == "root:root 755", owner_mode("/etc")
        machine.fail("test -e /etc/forward || test -e /etc/briefs || test -L /etc/atomics.sock")
        machine.succeed("journalctl -b -u atomics-mac-shares.service | grep -q 'symbolic link'")
        # Nothing was changed: the refusal comes before any change.
        assert owner_mode("/Users/alice") == "atomics:users 755", owner_mode("/Users/alice")
        machine.fail("mountpoint -q /Users/alice/repos")

    with subtest("a link swap of a share's mount point to /etc fails the next boot and mounts nothing there"):
        machine.succeed(user + "rm /Users/alice/.atomics && " + "mv /Users/alice/.atomics.old /Users/alice/.atomics")
        machine.succeed(user + "mv /Users/alice/repos /Users/alice/repos.old && ln -s /etc /Users/alice/repos")
        reboot()
        machine.wait_until_succeeds("systemctl is-failed atomics-mac-shares.service")
        machine.fail("mountpoint -q /etc")
        machine.fail("test -e /etc/README")
        assert owner_mode("/etc") == "root:root 755", owner_mode("/etc")

    with subtest("without links, the next boot takes the earlier layout over"):
        machine.succeed(user + "rm /Users/alice/repos && mv /Users/alice/repos.old /Users/alice/repos")
        reboot()
        roots_layout()
        machine.succeed("test -f /Users/alice/.atomics/forward/probe")
        machine.fail(user + "mv /Users/alice/.atomics /Users/alice/.atomics.old")
  '';
}

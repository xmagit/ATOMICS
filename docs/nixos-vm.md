# ATOMICS NixOS VM

`nixosConfigurations.atomics-vm` is an ARM Linux guest for Apple Virtualization (VZ) on Apple Silicon.
It imports `nixosModules.atomics` and installs the ATOMICS CLI, Docker with Rosetta for amd64 containers,
Node.js, Git, the GitHub CLI, Helix (`hx`, the default `[editor] command`, which the editor panel runs in
the guest when the host runs there), Claude Code, Codex, Cargo, rustc, rustup, GCC, pkg-config, Python 3,
coreutils (including `mktemp`) and `atomics-onboard`, which signs in to
them ([Sign in inside the VM](#sign-in-inside-the-vm)). A team adds its own stack (SDKs, cloud CLIs,
more sign-ins and checks) with a module of its own;
[`nix/examples/dotnet-azure.nix`](../nix/examples/dotnet-azure.nix) is an example, off by default
([Example: .NET and Azure DevOps](#example-net-and-azure-devops)).

The guest is headless: it never draws ATOMICS's window. The window runs natively on the Mac and draws
with Metal. That is path C of [the display decision](decisions/2026-11-02-vm-display.md): VZ, the only
hypervisor with Rosetta for Linux, gives Linux guests no GPU, and a native window has headroom a virtual
GPU does not. The guest starts no desktop and no software renderer.

**Recommended: the ATOMICS host in the VM.** With `atomics.vm.host.enable` the guest runs
`atomics host` ([the headless host](daemon.md#headless-host-atomics-host)): agents, the chief, hooks,
MCP, approvals, plans, worktrees, git and builds all run in the VM, and `ATOMICS.app` on the Mac is only
their window, a client of that host over `atomics.v1` ([The host in the VM](#the-host-in-the-vm)). One
setting on the Mac, `[client] connect = "vm"`, does it. File access to the Mac is through the shared
folders. Lima also exposes Mac network services at `192.168.5.2`, including listeners on the Mac’s
loopback; enable the [network policy](#network-policy) to block that address except for resolver DNS
and root's DHCP renewals.

The older layout keeps the host on the Mac and starts each agent in the VM over ssh
([Agents in the VM](#agents-in-the-vm)); a bundle built without the host has it.

Quick start on an Apple Silicon Mac with Lima 2.2 and Rosetta (no Docker, Nix or checkout needed):

```sh
atomics vm setup                                        # download, verify, create, start and check
limactl shell --workdir=/home/atomics atomics atomics-onboard   # your sign-ins, in the VM
atomics vm config --apply                                # [client] connect = "vm" on this Mac
```

Install Lima with `brew install lima` and, if needed, Rosetta with
`softwareupdate --install-rosetta --agree-to-license`. Setup downloads the signed VM assets for the
installed ATOMICS version; that release must include them. It checks SHA-256 and an Ed25519 signature
against either release key built into the app before decompressing the image or running any bundle
script. The signed bundle also binds the image checksum to this version. A missing asset, bad checksum
or untrusted signature stops setup.

The downloaded image has the host enabled and contains no Mac account name. Setup fills the
Nix-generated Mac bundle template for the current account. Defaults are this Mac's logical cores
minus four (at least one), half its memory in whole GiB (at least one), and a 100 GiB sparse disk:

```sh
atomics vm setup --cpus 8 --memory-gib 16 --disk-gib 200
```

The disk must be at least 100 GiB. These flags apply to downloaded bundles; a local `--from DIR`
bundle retains the sizes it was built with. Existing instances retain their resources and data.
Downloads and decompression report progress, and temporary files are removed when setup finishes or
fails. Empty disk regions remain sparse (on macOS, setup punches holes on APFS). Lima keeps its own disk
copy after the first boot. The template supports standard `/Users/<account>` homes; use a custom
`--from DIR` bundle for other home paths.

For a custom image or a release without VM assets, the build route still works:

```sh
scripts/macos-vm-build.sh --host --instance atomics-vm
atomics vm setup --from target/atomics-vm
```

## How the Mac and the VM fit together

`programs.atomics.macDesktop` generates both sides from one definition: the guest's half into this
system, and the Mac's half into a bundle (`packages.aarch64-linux.atomics-vm-mac`).

With the host in the VM (recommended):

| Piece | On the Mac | In the VM |
| --- | --- | --- |
| UI | `ATOMICS.app`, Metal, a client of the VM's host (`[client] connect = "vm"`) | none |
| Runtime: agents, chief, terminals, plans, approvals, worktrees, git | none | `atomics host`, the `atomics-host` service, as the VM's user |
| How the window reaches it | `bin/atomics-vm-host-forward`: `ssh -L <socket>:127.0.0.1:<port>` through Lima's ssh config, held by the window, on a Unix socket in a folder only you can enter | `atomics.v1` on `127.0.0.1:<port>` (default 47800) only, with a token this Mac paired with |
| The pairing token | the macOS keychain, item `atomics-vm-host-v1` | a SHA-256 digest in `paired-clients.json` (0600) |
| Hooks, MCP, `atomics ctl`, `atomics queue` | none | the host's own socket, `/run/atomics/atomics.sock` (`ATOMICS_SOCKET`) |
| Sign-ins: agents, gh, the ticket system | none | in the VM; ATOMICS's own in `~/.local/share/atomics/credentials` (`ATOMICS_CREDENTIALS_DIR`) |
| Worktrees | `~/repos` on APFS | mounted over virtiofs at the **same path** |
| Builds, Docker | none | all here |

With the host on the Mac (agents over ssh):

| Piece | On the Mac | In the VM |
| --- | --- | --- |
| UI | `ATOMICS.app`, Metal | none |
| Agents and chief | `[[agents]] command` is a wrapper from the bundle (`bin/claude`, `bin/codex`) that runs `ssh -tt` into the VM | the same program, started through `atomics-vm-exec` in a login shell |
| Worktrees | `~/repos` on APFS | mounted over virtiofs at the **same path** (`/Users/<mac-user>/repos`), so the folder ATOMICS gives an agent exists here |
| Hooks, MCP, `atomics ctl`, `atomics queue` | the control socket, `~/.atomics/atomics.sock` | off by default; with the opt-in forward (`bin/atomics-vm-forward`), the same path, and `ATOMICS_SOCKET` names it |
| The `atomics` binary that hooks and MCP name | `~/Applications/ATOMICS.app/Contents/MacOS/atomics` | that path is a link to the guest's Linux `atomics` |
| Long task briefs | `~/.atomics/briefs` | mounted read-only at the same path |
| Builds, Docker, logins | none | all here |

The VM sees the shared folders. It can also reach Mac network services through Lima’s host address
`192.168.5.2`, including Mac loopback listeners, unless the [network policy](#network-policy) is enabled.
The control socket stays on the Mac unless you forward it, and forwarding it makes the VM as trusted as
your Mac account ([The socket forward](#the-socket-forward)). Inside the VM there is one trust domain
([Trust and secrets](#trust-and-secrets)).

## Settings

[`nix/atomics-vm.nix`](../nix/atomics-vm.nix) declares the VM's settings under `atomics.vm`:

| Option | Default | Meaning |
| --- | --- | --- |
| `user` | `atomics` | the VM's login user (UID 1000); agents, the chief and their sign-ins run as `<user>` in `/home/<user>` |
| `macUser` | `user` | optional build-time Mac account; `null` reads the account and shares from Lima cidata at boot, as release images do |
| `cpus` | `4` | virtual CPUs in the Lima definition |
| `memoryGiB` | `8` | memory in the Lima definition |
| `diskGiB` | `100` | size of the (sparse) disk image and of the Lima disk |
| `nixForAgents` | `true` | in hardened mode, preserve agent access to Nix; `false` allows only root and admin ([trade-off](#hardened-mode)) |
| `host.enable` | `false` | run the ATOMICS host here ([The host in the VM](#the-host-in-the-vm)); `scripts/macos-vm-build.sh --host` sets it |
| `host.port` | `47800` | its protocol server's port, on the guest's loopback only; the Mac's forward opens the same one |
| `host.workspace`, `host.projects` | none | `atomics host -w <workspace>` and the projects it opens, in the shared folders |
| `onboardSteps` | none | extra `atomics-onboard` step files ([below](#sign-in-inside-the-vm)) |
| `checks` | none | extra checks for `scripts/check-atomics-vm.sh` ([below](#verify-in-the-booted-guest)) |

`atomics.examples.dotnetAzure.enable` (default `false`) turns on the example stack. Set them in a flake
of your own that extends this configuration, then build from that flake:

```nix
{
  inputs.atomics.url = "github:xmagit/ATOMICS";
  outputs = { atomics, ... }: {
    nixosConfigurations.atomics-vm = atomics.nixosConfigurations.atomics-vm.extendModules {
      modules = [ {
        atomics.vm = {
          user = "atomics";
          macUser = "alice";        # your account name on the Mac
          cpus = 8;
          memoryGiB = 24;
          diskGiB = 200;
          host.enable = true;       # the ATOMICS host in the VM (recommended)
        };
      } ];
    };
    packages.aarch64-linux = let vm = self.nixosConfigurations.atomics-vm.config; in {
      atomics-vm-image = vm.system.build.image;
      atomics-vm-mac = vm.programs.atomics.macDesktop.bundle;
    };
  };
}
```

(Add `self` to the `outputs` arguments for the `packages` part.) CPUs and memory can also change after
the VM exists: `limactl edit atomics --cpus 8 --memory 24` with the VM stopped. The disk cannot shrink,
and the image cannot be shrunk. On boot, systemd-repart grows the root partition to the disk Lima supplied,
then btrfs grows to fill it. This also handles later disk increases.

## Build

On an `aarch64-linux` Nix builder, from this checkout or the flake above:

```sh
nix build .#nixosConfigurations.atomics-vm.config.system.build.toplevel
nix build .#atomics-vm-image
nix build .#atomics-vm-mac -o result-mac
```

The second command produces `result/atomics-vm.raw`, a sparse GPT disk of `diskGiB` with a 512 MiB EFI
partition and a btrfs root. The image uses NixOS's systemd-repart builder, which can populate btrfs
offline. Keep sparse files sparse when copying it. The third produces the Mac bundle: plain `sh`
scripts for macOS's own `ssh`, `base64` and `tr`, with no `/nix/store` reference, so the Mac needs no
Nix. `nix flake check` also builds the guest system with and without the example stack and the host
(`checks.aarch64-linux.atomics-vm`, `atomics-vm-dotnet-azure`, `atomics-vm-with-host`), and with hardened
mode and the enforcing network policy (`checks.aarch64-linux.atomics-vm-egress`, including nftables
ruleset validation). `checks.aarch64-linux.atomics-vm-egress-upgrade` checks the rules with an existing
dnsmasq account at UID 994 and an upstream resolver separate from Lima's gateway. The flake also runs the
bundle's wrappers against the guest helper with an ssh stand-in (`checks.aarch64-linux.atomics-vm-mac`,
[`nix/tests/mac-desktop.sh`](../nix/tests/mac-desktop.sh)), and runs the host as the service does
(`checks.aarch64-linux.atomics-vm-host`, [`nix/tests/vm-host.sh`](../nix/tests/vm-host.sh)): the real
`atomics host`, paired, reached through the bundle's `atomics-vm-host-forward` on a socket in a private
folder, as the window holds it, with an ssh stand-in, by an `atomics.v1` client that starts a stub agent
in a shared folder. It checks that the forward refuses a relative socket path or one with a colon, that
the agent runs there, that only a paired token or that start's own gets through the forward, that an
unpaired token stops working, that no token is written down, and that the host stops cleanly and takes
the agent with it.

On shared development machines, run builds through `atomics queue --class test -- <build command>`.

### Build on a Mac with Docker

A Mac needs no Nix: [`scripts/macos-vm-build.sh`](../scripts/macos-vm-build.sh) runs the
`aarch64-linux` build in a `nixos/nix` container (pinned by digest) and prints the image's and the
bundle's paths:

```sh
scripts/macos-vm-build.sh --host --instance atomics-vm [--out DIR] [--mac-user NAME] \
  [--cpus 4] [--memory-gib 8] [--disk-gib 100]
```

- It waits for a test slot through `atomics queue --class test` when ATOMICS runs.
- The Nix store is a named Docker volume (`ATOMICS_NIX_VOLUME`, default `atomics-nix-store`), so a
  second run builds only what changed, and a run with nothing changed copies nothing.
- The checkout is mounted read-only, and only git's files (tracked ones, and new ones git does not
  ignore) are copied into the container: `target/` never reaches the store. The ATOMICS package is
  built from the Cargo files, `crates/`, `assets/` and the guest check alone, so an edit to the docs
  or the Nix files does not rebuild it.
- `--mac-user` (default: you) and the sizes are the [settings](#settings); `--instance` names the
  Lima instance the bundle's scripts reach (default `atomics`; pick another when one of that name
  exists); `--host` turns on the host in the VM.
- `DIR` (default `target/atomics-vm`) gets `atomics-vm.raw`, a sparse disk, `bundle/`, the Mac side
  (checked for `/nix/store` references, which a Mac does not have), and `build.json`.

**The image step and Docker's seccomp profile.** NixOS's image builder runs systemd-repart under
`unshare --map-root-user`, which Docker's default seccomp profile refuses (`unshare failed: Operation
not permitted`). Without that namespace the build user, not root, owns the image's `/`, and the guest
fails at boot (systemd reports unsafe path transitions; nsncd, sshd and DHCP do not start). The
container therefore runs with [`scripts/macos-vm-build.seccomp.json`](../scripts/macos-vm-build.seccomp.json):
every system call Docker's default profile refuses a container without added capabilities stays
refused (mount, ptrace, kexec, bpf, keyrings, io_uring and the rest), except creating a user namespace.
The container gets no added capability and is not privileged, the builds still run as Nix's build
users, and the image is the same derivation, store path for store path, that any Linux builder makes.
The other route, building as the container's root with that `unshare` removed, needs a patched image
builder whose output no other builder shares, and builds as root.

Measured on an 18-core Apple Silicon Mac (Docker Desktop, 18 CPUs and 24 GB for its VM), with a store
volume that already held most of the closure:

| Build | Time | Note |
| --- | --- | --- |
| Image and bundle, first run on this checkout | 32 min 41 s | 49 derivations built, 14 fetched; the image step (systemd-repart populating btrfs) alone 18 min 53 s |
| Again after a change to ATOMICS's sources | 8-13 min | the package and the image again |
| Again with nothing changed | 6 s | evaluation only; nothing copied |

The image is a 100 GiB sparse disk with 7.5 GiB in use; copying it out of the container keeps it
sparse. The bundle is a few dozen KiB of `sh` scripts and text.

### Build release assets

On an ARM Linux Nix builder with Python 3.11 or newer and zstd:

```sh
bash scripts/release-vm-image.sh [OUTPUT_DIRECTORY]
```

This builds `atomics-vm-release-image` and `atomics-vm-release-bundle`: the generic host-enabled
image with default build sizes and the bundle from `mac-desktop.nix`, using `@ATOMICS_MAC_USER@`
placeholders. It writes `atomics-vm-<version>-aarch64-linux.raw.zst`, a `.bundle.json` containing the
bundle files, version and image digest, and a `.sha256` for each. zstd reads the sparse image directly;
setup recreates zero regions as holes while decompressing.

The tag workflow builds these without secrets, then the existing protected `sign` job runs
`scripts/sign-release.sh` on both assets' exact bytes, alongside the desktop archives. Each gets a
raw Ed25519 `.sig` using the same CI key (or the offline key for manual recovery). All six VM files
are attached before the release becomes latest. Never give the image build a signing key. See
[release signing](releasing.md) for the key and approval process.

The image build, Nix evaluation and first boot on a real Mac remain required release checks: verify
share permissions, executable paths, the host, root growth, a second boot, and `atomics vm update`.

## Set up the VM on the Mac

`atomics vm setup` downloads this version's image and fills its bundle for this Mac;
`atomics vm setup --from DIR` uses an existing build's folder. Both produce a checked, running VM:

1. checks for Lima 2.2 or newer and Rosetta;
2. installs the bundle (its `install.sh`: `~/.local/share/atomics-vm`, and the shared folders);
3. creates the Lima instance the bundle names, from the image (`limactl create`). An instance of that
   name that `atomics vm setup` did not create is never reused or replaced: setup stops and says so,
   and only `--replace` (which asks you to type the instance's name, or `--replace --yes`) deletes it
   and its disk. One it created before is kept, so running setup again is safe;
4. starts it through `atomics queue --class qa`;
5. runs [the guest check](#verify-in-the-booted-guest) in the guest, through the same queue;
6. lists the sign-ins left (`atomics-onboard --check`) and the command that does them. It signs in to
   nothing itself;
7. shows the change the bundle's `config.d.toml` makes to this Mac's host config file
   (`~/.config/atomics/config.d/<LocalHostName>.toml`) as a diff. Nothing is written:
   `atomics vm config --apply` writes it after you have read it, keeping a backup beside the file.

Then `atomics vm start`, `stop`, `status` (not set up, stopped, starting or running) and `check` (the
guest check and the sign-ins left). Setup installs a newer bundle each time and keeps the instance it
made. Use `atomics vm update` to update its system while keeping its disk and sign-ins. The desktop's top bar shows the VM too, while a bundle is
installed: **VM** with its state, and a menu to start or stop it. On Linux every `atomics vm` command
says that the VM path is for macOS.

### By hand

The bundle's `lima.yaml` is the host definition, generated from the settings above: by default
**4 vCPU, 8 GiB RAM, 100 GiB disk**, VZ, Rosetta enabled, user `<user>` (`/home/<user>`, UID 1000) and
the Mac folders the guest mounts. Install Lima 2.2 or newer and Apple's Rosetta on the Mac, copy the
built raw image and the bundle there, then:

```sh
sh result-mac/install.sh            # copies the bundle to ~/.local/share/atomics-vm, makes shared folders
limactl create --name=atomics \
  --set='.images[0].location = "/absolute/path/to/atomics-vm.raw"' ~/.local/share/atomics-vm/lima.yaml
atomics queue --class qa -- limactl start atomics
limactl shell --workdir=/home/<user> atomics
```

The control socket is not forwarded until you choose to; read [The socket forward](#the-socket-forward)
first.

`limactl create` also takes sizes for this one instance, without a rebuild:
`--set='.cpus = 8 | .memory = "24GiB"'`.

Then copy `~/.local/share/atomics-vm/config.d.toml` into this Mac's host file,
`~/.config/atomics/config.d/$(scutil --get LocalHostName).toml` (or merge it there). A host file's
`agents` list replaces the main file's, so `claude`, `codex` and `shell` now start in the VM, plans pick
them by those names, and `mac-shell` stays a shell on the Mac. The chief starts in the VM too. Every
VM agent runs `isolation = "bare"`: the VM is their boundary, and Docker Sandboxes would start them on
the Mac. A repository that requires sandboxes (`.atomics/config.toml`) refuses them.

Use Lima's default home so the persistent instance lives in `~/.lima/atomics`. Creation does not boot
the VM. After its first boot, Lima's own disk copy makes the instance independent of the original
build image. Leave it stopped with `limactl stop atomics` when it is not in use.

The definition is not in Lima's plain mode, because plain mode drops every mount. Lima's container
runtime and boot scripts stay unused. NixOS mounts the shares itself (`atomics-mac-shares.service`),
matching each mount point to the virtiofs tag Lima lists in its cidata `user-data` (tags are hashes,
not `mount0`). In a generic image (`macUser = null`), the service reads `ATOMICS_MAC_USER` from
Lima's `param.env` and takes the mount paths and access modes from `user-data` on every boot. It
creates the Mac-path executable links and socket directory, mounts the repositories and briefs,
and writes `/run/atomics-mac-desktop.env` for the guest check and the host's working directory.
The host and Lima readiness both require this service to succeed. A build-time `macUser` and
custom shares still work with older cidata that has no account parameter. Outside plain mode `limactl start` reports the VM degraded until Lima's guest agent
answers, so `lima-guestagent.service` runs Lima's own agent from the cidata disk. Every port forward
is ignored, for listeners on the guest's loopback as well as on all its addresses (a rule for
`0.0.0.0` alone leaves Lima forwarding loopback ones): the VM's listeners, such as a database
container's published port or the ATOMICS host's, stay in the VM as in plain mode, and nothing
shadows a port on the Mac. An instance made from an older bundle gets the loopback rule with
`limactl edit <instance> --set '.portForwards = [{"guestIP":"127.0.0.1","guestPortRange":[1,65535],"proto":"any","ignore":true},{"guestIP":"0.0.0.0","guestPortRange":[1,65535],"proto":"any","ignore":true}]'`
while it is stopped. Cloud-init imports Lima's SSH public key at boot;
the existing NixOS user owns the resulting account. The guest marks Lima's SSH and boot readiness after
that and after the shares are mounted. The user can use `sudo` and Docker ([Trust and secrets](#trust-and-secrets)). Checkouts that stay in the
VM belong in `~/repos` (`/home/<user>/repos`), on the guest's btrfs filesystem, so warm-folder clones
can use reflinks; the Mac's worktrees are at `/Users/<mac-user>/repos`.

The Rosetta VirtioFS share has Lima's `vz-rosetta` mount tag. NixOS mounts it at `/run/rosetta` and
registers its interpreter directly with binfmt's `F` flag (fix-binary), so it is available inside
`linux/amd64` containers. Lima's own binfmt provisioning is disabled. Docker's API stays inside the
guest; no unauthenticated TCP Docker listener is enabled.

Sign in inside the guest with `atomics-onboard` ([below](#sign-in-inside-the-vm)). Keep tokens, SSH
private keys, passwords, package feed credentials and agent login state on its writable disk. Do not
add them to Nix settings, image contents, flake inputs or the Nix store.

## Updating the VM in place

After updating ATOMICS on the Mac, run:

```sh
atomics vm update
```

It reads the installed bundle's `atomics-vm.json`, builds the guest's NixOS system and Mac bundle
inside the guest, and reinstalls the bundle with its `install.sh`, which runs on the Mac as you. The
bundle comes back over the same shell that built it, as a tar stream of plain files and folders: links,
devices, other tar entry kinds, absolute paths and `..` are refused while unpacking, before anything
runs. Without [hardened mode](#hardened-mode) that shell is `limactl shell` as `<user>`, the user every
agent runs as, so an agent can hand the Mac a bundle of its own: the VM and the Mac account are then one
trust domain for updates too ([Trust and secrets](#trust-and-secrets)). In hardened mode it is the admin
user's own ssh connection. New bundles save the CPU, memory and disk build inputs in the JSON; for
older bundles these come from Lima's saved configuration. The instance, guest user, Mac account,
install directory and host port are retained. The Mac needs no Nix or Docker for this update.

If `[update] source` points to a checkout in a folder shared with the guest at the same path, the build
uses `git+file://<checkout>?rev=<HEAD>`. Only that commit's tracked files enter the store; dirty and
untracked files are excluded. The command does not fetch or move HEAD. Otherwise it builds
`github:xmagit/ATOMICS/v<version>`, matching this Mac's own ATOMICS version, and prints which source it
selected. Keep a worktree's Git metadata in the shares too, so the guest can read its revision.

Before building and again before installing, it asks the host's socket in the guest for `list` and
refuses busy work: working or starting agents, agents waiting for you, and pending approvals or queue
work. New hosts also publish the in-app updater's full blocker list, including background work, shells
running commands and open editors. `--yes` explicitly continues despite reported busy work. An
unreachable host is an error; it is never taken to mean idle. In the older agents-over-ssh layout this
needs the socket forward to the Mac's host.

The build waits for a test slot and starting the VM waits for a QA slot, using the existing queue.
Nothing is selected until both outputs are built, fetched and checked. If a build or installation
fails, the VM keeps its current boot selection; a boot-staging failure restores the previous profile
and bootloader. The update uses `switch-to-configuration boot`, leaving the running system alone
until the restart. It never deletes or recreates the instance, its disk or its sign-ins.

The command prints whether the system changed, the bundle was reinstalled and the VM restarted. It
quits and reopens `ATOMICS.app` if installed, restarting the VM **only** when the system changed. The
window reopens as the Dock would open it, without the command's own `ATOMICS_*` variables: an update run
from a shell that points `ATOMICS_CONFIG` or `ATOMICS_HOME` at a scratch setup still reopens the VM's
client, on the app's own config. With
`--no-restart` it leaves both the window and VM running; a changed system takes effect on the next
boot. If restarting fails after installation, the command says the next boot is already installed and
how to start the VM again.

Keep the fresh-image route for a broken VM or disk-size changes: build a new image with
`scripts/macos-vm-build.sh`, delete the old instance with `limactl delete <instance>`, and run
`atomics vm setup --from DIR` again. **Deleting the instance loses its disk and sign-ins.** Save what
you need first. An in-place update does not resize or repair the disk.

## Your own NixOS modules

In the main config or this Mac's host config (`config.d/<host>.toml`):

```toml
[vm]
modules = ["~/repos/team/team.nix"]
```

For example, `team.nix` can add a package and a Docker setting:

```nix
{ pkgs, ... }: {
  environment.systemPackages = [ pkgs.ripgrep ];
  virtualisation.docker.daemon.settings."log-driver" = "local";
}
```

Run `atomics vm update` to apply it. When setup creates a new instance and the list is non-empty, it
runs this same build once after the first boot and checks the updated guest before listing sign-ins.
The modules stay in your config for later updates; no team setting belongs in ATOMICS's public flake.

`~` expands on the Mac. Modules inside the guest's shares are imported at their paths, preserving
relative imports such as `./tools.nix` and file references such as `./certificate.pem`. Keep those
files beside the module in the shared repository. A module outside the shares is copied as a single
file and prints a warning that relative paths in it will not resolve. A module that changes the
instance, user, home, install directory or host connection is refused by this update; create a new
instance for that layout instead.

## The host in the VM

With `atomics.vm.host.enable` (`scripts/macos-vm-build.sh --host`) the guest runs the ATOMICS host, and
the Mac's ATOMICS shows it.

**In the guest.** The `atomics-host` service runs `atomics host --remote-port <port>` (with
`-w <workspace>` and the projects, when set) in a login shell of the VM's user, so agents get the PATH
and variables an interactive session has, starting in the first writable shared folder (`~/repos` of the
Mac). It waits for the Mac's folders to be mounted, restarts on failure and stops cleanly on
`systemctl stop`, ending its agents. Its socket is `/run/atomics/atomics.sock` (mode 0700 folder),
which `ATOMICS_SOCKET` names in every login shell too, so hooks, MCP servers, `atomics ctl` and
`atomics queue` reach this host. Its agents' terminals get it from the host itself, as every ATOMICS
gives its terminals its own socket: their login shells inherit the service's
`__NIXOS_SET_ENVIRONMENT_DONE` and so skip the profile's session variables. A first config is seeded with `[isolation] mode = "bare"`: the VM is
the agents' boundary. `--remote-port` turns on the protocol server whatever the config says, on
`127.0.0.1` only (`ss -ltn` shows nothing else listening on it).

**Pairing.** The host takes two kinds of token: the one it writes to `protocol.json` on every start,
for clients in the guest, and a paired client's, which outlives restarts. `atomics ctl
'{"cmd":"pair_client","name":"ATOMICS on Mac"}'` on the host's socket mints one and answers it once;
the host keeps only its SHA-256 digest, in `paired-clients.json` (mode 0600). Pairing a name again
replaces its token; `unpair_client` forgets it, and `atomics ctl '{"cmd":"remote"}'` lists the paired
clients (names and times, never a token). Only the VM's user can reach the socket, the same user who
could read `protocol.json`. A paired client is a controller, and its name is the lease holder other
clients see ("Typing from ATOMICS on Mac").

**On the Mac.** `[client] connect = "vm"` (the bundle's `config.d.toml`, applied by
`atomics vm config --apply`), or `atomics --connect vm` for one run, opens `ATOMICS.app` as a client of
that host ([the desktop as a client](daemon.md#the-desktop-as-a-client)). Each time it connects it:

1. asks Lima whether the VM runs. When it does not, the window shows the VM's state and a **Start**
   button, never an empty window; Start runs `limactl start` through `atomics queue --class qa`;
2. starts the bundle's `bin/atomics-vm-host-forward` (`ssh -N -L <socket>:127.0.0.1:<port>` through
   Lima's ssh config, as the agent wrappers do) unless it runs, and waits for it to answer. The socket
   is in a folder the window makes for it in your temporary folder (mode 0700, removed with the
   forward), so only that forward can listen there;
3. reads its token from the keychain. Without one it pairs: `atomics ctl pair_client` in the guest
   over Lima's ssh, and the token goes into the keychain (`atomics-vm-host-v1`, account: the
   instance). A token the host refuses (its home was reset, or it was unpaired) stops the window,
   which asks whether to pair again: only **Pair again** forgets the token and pairs anew;
4. connects through that socket with the token.

The token goes to the window's own forward alone. On a port of the Mac's loopback it would go to
whatever listened there first: another account's program, or another forward. The window opens no
port; `bin/atomics-vm-host-forward <port>` still opens one on the Mac's loopback for another client
(`atomics --connect ws://127.0.0.1:<port>` with a token paired for it), with that risk. The host
refuses an upgrade without a token. The top bar says where agents run: **Agents in VM <instance>**,
with **Use this Mac**, which asks once, removes `[client] connect` from the config file that sets it
(keeping a backup), and opens ATOMICS again as the host on the Mac. The VM's agents keep running.
Closing the window leaves every agent running in the VM; opening it again shows them, their terminals
and the plans as they are.

**The ticket sign-in** runs in the guest, headless: `atomics tickets login` in a login shell there
prints an address to open in the Mac's browser and takes back the address the browser ends on. The
guest has no keyring, so ATOMICS keeps that sign-in, like every credential it stores, as private files
in `ATOMICS_CREDENTIALS_DIR` (`~/.local/share/atomics/credentials`: a 0700 folder of 0600 files, an
item each), which the service and every login shell name. The VM is one trust domain
([Trust and secrets](#trust-and-secrets)): any agent can read those files, as it can gh's and Claude's.

**End to end on an Apple Silicon Mac** (Lima 2.2, the default 4 vCPU and 8 GiB): `atomics vm setup`
created the instance in about a second and booted it to a running host in about 20 s, and the guest
check took 7 s. Opened with `[client] connect = "vm"` and nothing else in its config, `ATOMICS.app`
paired once (the token went into the keychain), showed the host's board, started a shell with
`--open shell:<folder>` in a worktree under `~/repos`, and showed its terminal and the worktree's diff,
both from the VM; in the guest, the shell's process ran as the VM's user with that worktree as its
folder. Closing the window ended the forward and left the shell running; a window opened later showed
it as it was. With the VM stopped the window showed its state and **Start** instead of a board.

**What a browser can do.** The forward carries `atomics.v1` with a bearer token, which a browser cannot
send on a WebSocket. A browser reaches a host through [the rendezvous](daemon.md#rendezvous) instead,
which the host in the VM can run too. The client the window uses is the browser client's own code
(atomics-web's driver, backend and views), run natively.

In this layout the [socket forward](#the-socket-forward) is not needed: hooks and MCP are local to the
VM's host. Keep it off.

## Sign in inside the VM

`atomics-onboard` (installed in the guest, source [`nix/vm/onboard.sh`](../nix/vm/onboard.sh)) walks
through every sign-in the chief and the agents need, in the guest. Run it from a terminal on the Mac:

```sh
limactl shell --workdir=/home/<user> atomics atomics-onboard
limactl shell --workdir=/home/<user> atomics atomics-onboard --check
```

Every step checks first and is skipped when it is done, so a second run signs in to nothing, and a
later run redoes only what has lapsed. A run ends with the list `--check` prints. `--check` asks
nothing, signs in to nothing and writes nothing. It prints one line per item, `ok`, `missing` with a
fixed reason, or `skipped`, and never an account, a token or a URL. It exits 1 while an item is
missing.

```text
ok       claude   Claude subscription (Claude Code login; includes claude.ai connectors)
missing  codex    Codex (ChatGPT login): not signed in
ok       gh       GitHub CLI and Git push to github.com (gh login)
ok       private  Credential files private to this user (no group or other access)
ok       host     ATOMICS host (in this VM, or the Mac's over the socket forward)
```

Each sign-in uses a device code or a pasted code: the tool prints an address, you open it in the
Mac's browser, and the code goes back into the terminal. Nothing needs a browser in the guest.

1. **Claude** (`claude`). `claude auth login` prints a sign-in link. Sign in with the Claude
   subscription and paste the code shown back into the terminal. The chief and the Claude agents use
   this login. claude.ai connectors come with the account. Claude Code keeps it in
   `~/.claude/.credentials.json`. Check: `claude auth status` exits 0 with a subscription sign-in,
   not an API key.
2. **Codex** (`codex`). `codex login --device-auth`. Device code sign-in must be allowed in ChatGPT's
   security settings; on a business workspace that is the admin's choice. Otherwise sign in through
   the Mac's browser: on the Mac, run the bundle's `bin/atomics-vm-codex-login`
   (`~/.local/share/atomics-vm/bin/atomics-vm-codex-login`). It runs `codex login` in the VM and
   forwards its callback port, 1455 on the Mac's loopback, while it runs; open the address it prints.
   Then run `atomics-onboard` again. Codex keeps the sign-in in `~/.codex/auth.json`. Check:
   `codex login status`.
3. **GitHub** (`gh`). `gh auth login --hostname github.com --git-protocol https --web` shows a one-time
   code and the address to enter it at, then `gh auth setup-git --hostname github.com` makes gh Git's
   credential helper for `https://github.com`, so agents can push. nixpkgs' gh writes the Nix store
   path of the program behind its wrapper there, which a garbage collection after an update removes,
   so `atomics-onboard` names gh where `PATH` finds it instead. Without a keyring, gh keeps its token
   in `~/.config/gh/hosts.yml`. Check: `gh auth status --hostname github.com` succeeds and Git's helper
   for github.com is gh, not a Nix store path. A signed-in gh that is not Git's helper only gets
   `setup-git` again.
4. **Private credential files** (`private`, after every sign-in). The sign-ins above keep plain-text
   tokens in the home folder ([Trust and secrets](#trust-and-secrets)). Each one that exists, and its
   folder, must give group and others no access (`0600` and `0700`, or stricter):
   `~/.claude/.credentials.json`, `~/.codex/auth.json` and `~/.config/gh/hosts.yml`, with any a step
   file names. `--check` reports the first that another user could open; a run without `--check` takes
   that access away. `atomics-onboard` runs its sign-ins with
   `umask 077`, so files they create start private.
5. **ATOMICS host** (`host`, check only, always last). When a socket is there (`ATOMICS_SOCKET`, else
   the default path), `atomics ctl '{"cmd":"list"}'` must answer; without one the item is `skipped`.
   With [the host in the VM](#the-host-in-the-vm) that is the guest's own host, which holds the ticket
   system's sign-in (`atomics tickets login` in the guest, kept in `ATOMICS_CREDENTIALS_DIR`). With the
   host on the Mac and the socket forward on, the chief in the guest reaches the `atomics_ticket_*`
   tools through the Mac's host and needs no ticket sign-in of its own.

**Step files.** More items come from bash files in `/etc/atomics/onboard.d` (or `$ATOMICS_ONBOARD_DIR`),
sourced in name order before the credential-file and host checks. `atomics.vm.onboardSteps` installs
them: `atomics.vm.onboardSteps."50-my-registry" = ./my-registry.sh;`. A step file calls
`onboard_step NAME LABEL` and defines `check_NAME` (quiet; returns 0 when done, else sets `$reason` to a
fixed text) and `login_NAME`. `onboard_option NAME HELP HANDLER` adds a `--NAME VALUE` option,
`onboard_prepare FUNCTION` runs before the sign-ins, for instance to ask for settings, and
`onboard_private PATH...` adds credential files (and their folders), relative to the home folder, to the
`private` check. The header of
[`nix/vm/onboard.sh`](../nix/vm/onboard.sh) lists the helpers a step file may use;
[`nix/examples/dotnet-azure/onboard.sh`](../nix/examples/dotnet-azure/onboard.sh) is a full example.

### Trust and secrets

With `atomics.vm.host.enable = true`, a fresh guest host config seeds `[isolation] mode = "bare"`
and the Codex command `["codex", "--dangerously-bypass-approvals-and-sandbox"]`. Codex runs without
its own sandbox or approval prompts, so it can write the worktree's git metadata and reach the host's
queue socket. The VM boundary is then the only sandbox boundary. This seed applies only to the host
in the VM; desktop host defaults and the agents-over-ssh layout are unchanged. An existing
`~/.config/atomics/config.toml` is never overwritten by the seed.
The flake check `checks.aarch64-linux.atomics-vm-config` checks the generated host seeds, the
unchanged desktop seed and the guest's build tools, including `mktemp`.

Without [hardened mode](#hardened-mode) the VM is one trust domain. Every agent, the chief and everything
they run (builds, package scripts, containers) run as `<user>`, and each of them can reach all of the following:

- **Plain-text credentials.** The guest has no keyring or Secret Service, so Claude Code, Codex and gh
  (and in the example, az, Git Credential Manager and the Azure Artifacts credential provider) keep their
  tokens in plain-text files in `/home/<user>`. Those files and their folders give group and others no
  access, which `atomics-onboard --check` verifies. The modes keep other users out, not other processes of `<user>`:
  any agent can read every sign-in in the VM, not only the one it uses.
- **Passwordless sudo.** `<user>` is in `wheel`, which needs no password, so any agent can become root.
- **The Mac bundle.** `atomics vm update` builds and fetches the Mac bundle as `<user>` and runs its
  `install.sh` on the Mac, so any agent can replace what it installs there, and so run code as you on
  the Mac at the next update.
- **The docker group.** `<user>` is in `docker`, which is root by another name: a container can mount
  the guest's `/`.

There is no isolation between agents inside the VM. The boundary is between the VM and the Mac: the
VM sees only the shares, and ATOMICS's control socket only if you forward it
([The socket forward](#the-socket-forward)). So sign in here only with accounts and tokens you would
give every agent, scoped to what they work on, and keep personal or administrative credentials on the
Mac. sshd takes only Lima's key: password and keyboard-interactive logins and root logins are off.

**Secrets.** You type each secret into the tool's own prompt, in the guest. `atomics-onboard` never
reads, prints or stores one. Its checks discard the tools' output, and it writes no log. It never
touches the Mac: on macOS it refuses to run. The package holds the script alone, so no value enters
the Nix store, the image or this repository. The flake check `checks.<system>.atomics-onboard` runs
[`nix/vm/onboard-test.sh`](../nix/vm/onboard-test.sh) against stub tools, with a demo step file. The
stubs print a random canary wherever a real tool prints or stores a secret. The test checks that the
canary is only in the tools' own credential files, and not in atomics-onboard's output or
`~/.gitconfig`. It also checks that a second run signs in to nothing, that `--check` reports every
item, and that a step file's item, option and preparation work. Folders given after the program are
searched too, and `--journal` searches the systemd journal. In the guest, from a checkout in a shared
folder:

```sh
limactl shell --workdir="$PWD" atomics bash --login -c \
  'bash nix/vm/onboard-test.sh "$(command -v atomics-onboard)" --journal /nix/store "$HOME"'
```

### Hardened mode

`atomics.vm.hardened = true` (in a module of your own, [Your own NixOS modules](#your-own-nixos-modules))
keeps root away from `<user>`, and so from the host, the chief, every agent and what they run:

- **No sudo.** `<user>` leaves `wheel`, and the rule cloud-init writes from Lima's user data on every boot
  (`/etc/sudoers.d/90-cloud-init-users`) is removed again before the host starts. The host's service also
  runs with `NoNewPrivileges`, so nothing it starts gains privileges through a setuid program.
- **Rootless Docker.** `<user>` leaves `docker` and gets Docker of its own, under its own systemd from boot.
  Containers run in its user namespace, with no more than `<user>` has. Their traffic, including sockets
  owned by subordinate UIDs, is covered by the [network policy](#network-policy). `/run/docker.sock`
  leads to it, so tools that know only that path work unchanged. Root's Docker is off; images and volumes
  from before start over.
- **Read-only shares.** The Mac's folders are mounted read-only, unless a module sets a share's `writable`.
  With the host in the VM, agents work on the VM's disk, and a writable `~/repos` would let one plant a git
  hook or a NixOS module that later runs on the Mac. An existing instance keeps Lima's own mount settings:
  make them match with `limactl edit <instance> --set '.mounts[].writable = false'` while it is stopped.
- **An admin user.** `atomics.vm.adminUser` (`admin`) is the one user with sudo. Only the key Lima gives
  `<user>` reaches it, read at every boot from Lima's read-only cidata disk, so nothing `<user>` can write
  changes who logs in as it. `atomics vm update` builds the system and selects it as this user, over Lima's
  ssh config (`ssh -F ~/.lima/<instance>/ssh.config -l admin lima-<instance>`), and asks about busy work as
  `<user>`. It also reads the new Mac bundle as the admin, over a connection of its own (no shared ssh
  control master), never with `limactl copy` or `limactl shell`, which log in as `<user>`. What reaches the
  Mac and its `install.sh` is the store path the admin's build printed. That keeps an agent from swapping
  the system or bundle on the way, but not from shaping what gets built: the build reads the `[update]
  source` checkout and your `[vm] modules` from the Mac's shares, so keep those read-only to the guest, as
  this mode's shares are by default.

`atomics.vm.nixForAgents = true` (the default) preserves agents' access to the Nix daemon for builds,
`nix develop` and `nix-shell`. Build processes, including fixed-output derivations and `builtin:fetchurl`,
run as build users and are filtered by the network policy. Evaluator fetchers such as
`builtins.fetchTarball` run as the calling user and are filtered too. Nix
[switches to the build UID before invoking builtin builders](https://github.com/NixOS/nix/blob/2.34.9/src/libstore/unix/build/derivation-builder.cc#L1335).

The daemon's own substitutions run as root, outside the domain allowlist. An untrusted agent can trigger
cache lookups and downloads for store paths it chooses. It cannot add arbitrary cache URLs: the daemon
[limits untrusted substituters to configured or explicitly trusted URLs](https://github.com/NixOS/nix/blob/2.34.9/src/libstore/daemon.cc#L231).
Cache responses and redirects still drive root-owned transfers; filtering build UIDs does not cover
those requests. Do not add agents to `nix.settings.trusted-users`, which would let them change
substituters and other restricted daemon settings.

For a stricter boundary, set `atomics.vm.nixForAgents = false` in hardened mode. This forces
`nix.settings.allowed-users` to root and the admin, closing agents' access to daemon-owned fetching.
Agents can still use installed toolchains, but Nix builds and development shells that require the daemon
must then go through the admin.

The first update that turns it on still runs as `<user>`, with sudo, and fetches its Mac bundle as
`<user>` too; later ones run as the admin. Turn it on from a VM you trust, or set it up fresh. Sign-ins
stay `<user>`'s, in plain-text files every agent can read, as above.

### Network policy

`atomics.vm.egress.enable = true` filters every UID except root and dnsmasq, including subordinate UIDs
in user namespaces and `nixbld*` build users. Their new connections reach only listed domains. dnsmasq
becomes the guest's resolver: it forwards listed domains, with their subdomains, to the configured upstream
(Lima's resolver by default, which asks the Mac's, and so its VPN's) and refuses every other name, so a
lookup cannot carry data out either.
Each address it hands out for a listed domain goes into an nftables set, and only those addresses are
allowed, on ports 80 and 443 (`webPorts`), or on a rule's own ports. Loopback and replies to established
connections are allowed. Only root and dnsmasq bypass the domain allowlist; other system users do not.
The rules resolve those account names when nftables loads them, so an upgraded guest keeps working
with dnsmasq's existing dynamically allocated UID.

New connections to Lima’s host address (`192.168.5.2`) are refused for every UID, including root, before
UID exemptions or domain rules. Only dnsmasq may open TCP/UDP connections to port 53 at
`atomics.vm.egress.upstream` (default `192.168.5.2`), so agents cannot query Lima’s resolver directly or
initiate connections to Mac services through it. Root alone may send UDP to gateway port 67 so dhcpcd
can renew the VM's lease by unicast. These exceptions precede the gateway reject, which still precedes
the general UID exemptions. `upstream` must be an IPv4 or IPv6 address literal; hostnames, ports, zone
identifiers and CIDR prefixes are rejected during evaluation. Established and related traffic is accepted
first so replies to Mac-initiated SSH and forwarded connections still work when Lima presents the gateway
address as their source. Use it with [hardened mode](#hardened-mode): without it any agent can become root
and lift it.

```nix
atomics.vm.egress = {
  enable = true;
  domains = [ "dev.azure.com" "login.microsoftonline.com" ];       # with their subdomains, on 80 and 443
  rules = [ { domains = [ "db.example.database.windows.net" ]; ports = [ 1433 ]; } ];
};
```

The defaults (`defaults = true`) cover what any agent here needs: Anthropic's and OpenAI's services,
GitHub with its packages and downloads, npm, NuGet, .NET downloads, crates.io and rustup, PyPI, Docker Hub,
MCR, NixOS, and documentation (Microsoft Learn, Stack Overflow and Stack Exchange, MDN and a few more).
A team adds its cloud, ticket system and databases. Refused connections are logged:
`journalctl -k -g atomics-egress` lists each with its destination; a refused name shows in a tool's own
error. With `enforce = false` connections outside the domain allowlist are only logged, which helps to
learn what work needs before enforcing. Names outside the list and new connections to Lima’s host address
(except resolver DNS and root's DHCP renewals) remain refused.

Limits: an allowed address is allowed whatever name led there, which matters behind shared CDNs, and an
allowed service that stores data (a GitHub repository, a package feed) can still receive what an agent sends.
The policy narrows where data can go; it does not make the sign-ins in the VM safe to leak.

### Shared Docker images

Agents share one Docker, so an image one pulled or built is there for the next. `atomics.vm.docker.images`
pulls a list at boot when missing, so the first agent after a fresh start, or after hardened mode resets
Docker, does not wait for them. An image without an arm64 build names its platform and runs under Rosetta:

```nix
atomics.vm.docker.images = [
  "redis:6.0-alpine"
  { name = "elasticsearch:7.2.1"; platform = "linux/amd64"; }
];
```

The pulls run as `atomics-docker-images`, a service of the VM's user in hardened mode
(`systemctl --user status atomics-docker-images`) and of root otherwise. A failed pull is retried every minute
and never holds anything else up. Images a repository builds itself stay its own: the first build is shared.

## Agents in the VM

This is the layout with the host on the Mac: a bundle built without the host. With
[the host in the VM](#the-host-in-the-vm) the host starts agents in the guest itself, and none of this
applies.

**Wrappers.** `bin/claude` and `bin/codex` are named like their programs because ATOMICS recognizes
agents by name (Codex's hooks key on it). Each one:

- connects through Lima's own ssh config (`~/.lima/atomics/ssh.config`, host `lima-atomics`) on a
  private connection: `ControlMaster=no`, so it never shares Lima's multiplexed master;
- passes `-o ObscureKeystrokeTiming=no`. OpenSSH 9.5 and later otherwise sends keystrokes on a 20 ms
  clock, which cost a frame or more per keystroke in the display spike;
- passes `-e none`, so no keystroke sequence such as `~.` ends the session, and `-tt` when ATOMICS
  gives it a terminal (`-T` otherwise, as for Codex's `--version` probe);
- sends the folder, the arguments and a few variables as NUL-separated fields in base64. No shell on
  either side re-parses them, so quotes, newlines and JSON arrive unchanged. A request beyond 100 kB
  (sshd takes about 128 KiB per command) is staged on stdin first;
- starts the program in a login shell in the VM, in the same folder, with the VM's own PATH, variables
  and logins.

Files keep working across the boundary: the VM sees the Mac's files as its own user's, so git needs
no `safe.directory`, and files the VM writes belong to the Mac user.

Only `TERM`, `COLORTERM`, `TERM_PROGRAM`, `COLORFGBG` and `ATOMICS_HOOK_PANE` cross (`forwardEnv`). The
Mac's PATH, compiler cache, job server and credential relay mean nothing in the VM and stay behind.
`bin/atomics-vm-run` runs any command the same way, or a login shell when given none; the `shell` agent
uses it.

**Hooks and MCP.** Claude's per-session `--settings` and `--mcp-config`, and Codex's `-c` overrides,
name the Mac binary and the socket. In the VM the binary's path runs the Linux `atomics`, and the
socket's path is the forward, when it is on. Claude hooks carry `--pane`. Codex's hook definitions must stay
identical across panes (Codex trusts them by their text), so ATOMICS puts the pane in the session's
`ATOMICS_HOOK_PANE` instead and the wrapper passes it on; process ancestry, the fallback, cannot cross
ssh. ATOMICS asks `codex --version` whether Codex supports hooks; it asks in `/`, a folder the VM has.
Inside the VM, `atomics queue` and `atomics ctl` reach the Mac's ATOMICS through `ATOMICS_SOCKET`.
All of this needs [the socket forward](#the-socket-forward), which is off by default.

**Options** (`programs.atomics.macDesktop`, in [`nix/modules/mac-desktop.nix`](../nix/modules/mac-desktop.nix)):

| Option | Default | Meaning |
| --- | --- | --- |
| `user`, `home` | `atomics.vm.macUser`, `/Users/<user>` | the Mac account; the VM creates `home`, owned by the VM user |
| `shares` | `<home>/repos` writable, `<home>/.atomics/briefs` | Mac folders mounted at the same path |
| `executables` | `ATOMICS.app`'s binary in `~/Applications` and `/Applications` | Mac paths that run the VM's `atomics` |
| `socket` | `<home>/.atomics/atomics.sock` | the control socket the opt-in forward carries |
| `agents` | `claude`, `codex` | wrappers to generate |
| `forwardEnv` | the five variables above | what crosses from the Mac's terminal |
| `installDir` | `<home>/.local/share/atomics-vm` | where the bundle lives on the Mac |
| `lima.instance`, `lima.settings` | `atomics`, VM size and VZ options | the Lima instance and its definition |

**Limits.**

- An agent starts only in a shared folder. Elsewhere its tile says `<folder> is not in this VM` and
  lists the shares. Worktrees beside the main checkout in `~/repos` are shared.
- Builds read and write worktrees over virtiofs: about 15% slower full .NET builds and 1.5–1.7×
  slower small incremental ones than the VM's own disk, per the display spike.
- Quitting or restarting ATOMICS closes the ssh sessions and stops the agents, as quitting does today.
  Claude sessions resume.
- Sign agents and the chief in inside the VM. The Mac's credential relay listens on the Mac's
  loopback; the bundle's config sets `bare_relay = false`.
- A test instance's `ATOMICS_HOME` keeps its briefs elsewhere; add that folder to `shares` if its
  Codex agents get long briefs.

## The socket forward

**Off by default.** ATOMICS's control socket carries its whole control API, and the socket's file mode
is its only check: whoever can connect may start agents (among them `mac-shell`, a shell on the Mac),
type into any session (`send`), approve or deny its prompts (`approve`), run commands in sandboxes,
read terminals and diffs, and use the ticket tools with the host's credentials. Forwarded into the VM,
that is every process there. **Turning the forward on makes the VM as trusted as your Mac account**: a
compromised agent, build script or container in the VM can then run commands on the Mac as you. A
restricted forward that would carry only the agent tools, with a token per pane, is not built.

**Without it**, agents in the VM run in their tiles as plain terminals. Their hooks and MCP servers
cannot reach ATOMICS: no status, turn ends or approval prompts on the Mac (agents ask in their own
terminal), no `atomics_report` or ticket tools, and a chief in the VM cannot start or steer agents.
`atomics queue` in the VM runs its command at once, without a slot, and says so on stderr.
`atomics-onboard` reports the host item as `skipped`.

**Turning it on.** `bin/atomics-vm-forward` holds one `ssh -N -R <socket>:<socket>` to the VM. Load
its launchd agent once you accept the trust above:

```sh
launchctl bootstrap gui/$(id -u) ~/.local/share/atomics-vm/launchd/ai.atomisk.atomics.vm-forward.atomics.plist
```

The launchd agent restarts it when the VM stops or restarts (every 30 s at most, logging to
`~/Library/Logs/atomics-vm-forward.log`); `launchctl bootout gui/$(id -u)/ai.atomisk.atomics.vm-forward.atomics`
turns it off again. sshd's `StreamLocalBindUnlink yes` replaces a socket a dead forward left. A second
ATOMICS instance with its own `ATOMICS_SOCKET`, such as a QA instance, needs its own forward:
`atomics-vm-forward /path/to/its.sock`.

## Measured

The built path, measured again with the display spike's scene and every tile in the VM through the
generated wrapper (a test VM with 14 vCPU and 24 GiB). The UI renders on Metal at the display's full
refresh: 119–120 fps at 120 Hz while the diff scrolls and 8 tiles stream (frame p99 8.6 ms), and
256–334 fps without vsync. A keystroke's echo is in a frame 7.7 ms p50 / 9.2 ms p95 after the key,
against 7.4 / 9.1 ms for a tile on the Mac. The wrapper's PTY round trip is 0.51 ms p50; plain ssh with
OpenSSH's defaults is 19.4 ms. The results, the scene, the method and the raw runs are in
[the v5 display benchmark](benchmarks/2026-11-v5-display/README.md).

## Verify in the booted guest

From a checkout in a shared folder on the Mac, run the check in the guest through the host's QA queue:

```sh
atomics queue --class qa -- limactl shell --workdir="$PWD" atomics bash --login -c 'exec bash scripts/check-atomics-vm.sh'
```

When the network policy is enforcing, the check first confirms that both the ordinary UID and a
subordinate UID (`unshare --user --map-users=auto --setuid 0`) are refused a direct HTTPS connection to
`1.1.1.1`. `bash scripts/check-atomics-vm.sh --egress-only` runs just this read-only probe. Run it from
a login shell: a process with `NoNewPrivileges` cannot use the setuid UID mapping helper. A namespace
setup error or a network timeout fails the check instead of counting as a refusal.

The check requires an ARM guest and a btrfs root, confirms Rosetta's fix-binary registration, finds
the base tools, and checks that sshd takes keys only and no root login. For the Mac side it reads `/etc/atomics/mac-desktop.env` and checks that each
share is mounted over virtiofs at its Mac path with the right access (writing a probe file in writable
ones), that the Mac's ATOMICS paths run this guest's `atomics`, that the socket folder belongs to the
user alone, and that sshd replaces stale sockets; with the forward running, it asks the Mac's ATOMICS
for its sessions. With the host in the VM it checks instead that the `atomics-host` service runs, that
its socket folder is the user's alone, that its protocol server listens on `127.0.0.1:<port>` and
nowhere else, and that an agent it starts (a shell, through `atomics ctl`) runs in the guest, in a
shared Mac folder: the process's working folder is checked through `/proc`. `atomics vm check` runs
it from the Mac. It clones a file with `cp --reflink=always` and runs a shell in a `linux/amd64`
container, which works only through Rosetta. Then it runs the checks in `/etc/atomics/check.d` in name
order (`atomics.vm.checks`); each one fails the run by exiting non-zero.

## Example: .NET and Azure DevOps

[`nix/examples/dotnet-azure.nix`](../nix/examples/dotnet-azure.nix) is an example of a team's stack,
off by default. Turn it on with `atomics.examples.dotnetAzure.enable = true;` in the flake above, or
copy it as the start of your own module. It adds:

- .NET SDK 9 with the .NET 8 and ASP.NET Core 8 runtimes, PowerShell, Azure CLI with its `azure-devops`
  extension built in (and a Fabric patch backported for Invoke 3), a Git helper using Azure CLI and
  the Azure Artifacts credential provider, which NuGet discovers through `NUGET_PLUGIN_PATHS`;
- four `atomics-onboard` steps ([`dotnet-azure/onboard.sh`](../nix/examples/dotnet-azure/onboard.sh)),
  and its plain-text token caches (`~/.azure`, `~/.local/.IdentityService`,
  `~/.local/share/MicrosoftCredentialProvider`) in the `private` check;
- a check ([`dotnet-azure/check.sh`](../nix/examples/dotnet-azure/check.sh)) for those tools, the
  built-in `az repos` command, the .NET 9 SDK and both 8.0 runtimes, that starts a fresh
  `linux/amd64` SQL Server 2022 container and waits for `SQL Server is now ready for client connections`.
  Its random SA password exists only at runtime;
  it publishes no port, uses no volume and removes its container on exit.

Its first `atomics-onboard` run asks for the Azure DevOps organization and the Azure Artifacts feeds,
suggesting the organization of the first Azure DevOps remote in `~/repos` and the feed URLs in its
`nuget.config` files. `--org NAME` and `--feed URL` (repeatable) set them without asking. They are kept
in `~/.config/atomics/onboard/azure-devops.conf` (mode 600), which holds names and URLs only. A URL
with a user part is never taken.

1. **Azure CLI** (`az`). `az login --use-device-code --allow-no-subscriptions`. For one tenant, run
   `az login --use-device-code --tenant <tenant>` yourself; the step then counts as done. az keeps
   it in `~/.azure`. Check: `az account get-access-token` for Azure DevOps's resource
   (`499b84ac-1321-427f-aa17-267ca6975798`) succeeds, with `--output none`. The packaged
   `azure-devops` extension makes `az repos` available without downloading an extension or using pip.
2. **Git push to Azure DevOps** (`ado-git`). The packaged `git-credential-azure-cli` helper is set
   up for `https://dev.azure.com` alone, in `~/.gitconfig`:

   ```ini
   [credential "https://dev.azure.com"]
       helper =
       helper = azure-cli
       useHttpPath = true
   ```

   The empty `helper` drops helpers configured before it, including Git Credential Manager. This
   step only configures Git; Azure CLI (step 1) does the sign-in. On `get` for this HTTPS host, the
   helper runs `az account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798 --query accessToken -o tsv`
   and answers `username=AzureDevOps` and `password=<token>`. It answers nothing on `store` or `erase`
   and keeps no tokens of its own. Azure CLI's on-disk refresh tokens survive VM restarts, so every
   agent running as the guest user can get a fresh access token without another device-code prompt.
   Check: the helper configuration matches and `git credential fill` returns a nonempty password
   with prompts disabled (`GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=false`, `SSH_ASKPASS=false`); its
   credential is discarded. If the Azure CLI sign-in expires or is revoked, repeat step 1.
3. **NuGet feeds** (`nuget`). For each feed, `CredentialProvider.Microsoft -U <feed> -R -F Json`
   with `ARTIFACTS_CREDENTIALPROVIDER_FORCE_CANSHOWDIALOG_TO=false`: device flow, its log and device
   code on stderr, the credential on stdout discarded. `dotnet restore` then finds the session token
   through `NUGET_PLUGIN_PATHS`. The provider keeps it in
   `~/.local/share/MicrosoftCredentialProvider`. Check: the same command with `-N` (never prompts).
4. **Azure DevOps MCP server** (`ado-mcp`).
   `claude mcp add --scope user azure-devops -- npx -y @azure-devops/mcp@<version> <org> --authentication azcli`,
   in the guest's `~/.claude.json`, with the release pinned in the step file. It signs in through `az`
   (step 1) and holds no secret of its own. User scope reaches every Claude session in the guest, the
   chief's included; other agents still need permission for its tools. A registration for another
   organization or release is replaced. Check: `~/.claude.json` registers exactly this command.

All worktrees for `<user>` share NuGet's default global package cache at `~/.nuget/packages`
(`/home/<user>/.nuget/packages`); no additional cache option is needed. For a sandboxed agent,
bind-mount it read/write at the agent's `~/.nuget/packages`.

The flake check `checks.<system>.atomics-onboard-dotnet-azure` runs
[`dotnet-azure/onboard-test.sh`](../nix/examples/dotnet-azure/onboard-test.sh), the same canary test
for these steps, using the real Git helper script with a stub Azure CLI. It checks noninteractive
credential fills, a restart with only on-disk state, and an unchanged second onboarding run. In the guest:

```sh
limactl shell --workdir="$PWD" atomics bash --login -c \
  'bash nix/examples/dotnet-azure/onboard-test.sh "$(command -v atomics-onboard)" \
     /etc/atomics/onboard.d/50-azure-devops.sh \
     nix/examples/dotnet-azure/git-credential-azure-cli.sh --journal /nix/store "$HOME"'
```

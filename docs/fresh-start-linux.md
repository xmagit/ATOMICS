# Fresh start on Linux

This page takes a clean Ubuntu desktop to a running chief with sandboxed agents. Each step ends with a
check. [How IMACS works](how-it-works.md) explains the pieces; if you are bringing an existing setup from
another machine, read [Moving to a new machine](moving-machines.md) on the old one first.

Paths below assume a new Linux install: config in `~/.config/imacs` and state in `~/.local/state/imacs`
(`$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` move them). If `~/.imacs` already exists, IMACS keeps using it
as its state folder.

## 1. Prerequisites

| What | Why |
| --- | --- |
| Ubuntu 24.04 or newer, in a desktop session | Docker Sandboxes support it. Other distributions can use sbx's static build; doctor warns, and setup carries on. |
| Hardware virtualization, `/dev/kvm`, and your session in the `kvm` group | Every sandbox is a microVM. |
| rustup and stable Rust | `scripts/linux-install.sh` builds IMACS from this checkout. |
| The build libraries and a Vulkan driver | The window (eframe/wgpu) and audio. |
| `secret-tool` and an unlocked Secret Service keyring | IMACS's credential store. There is no plaintext fallback. |
| `xdg-open` (`xdg-utils`) and the desktop portal | Opening files, ticket links and sign-in pages; file dialogs. |
| Docker Sandboxes, `sbx` v0.50.1 or newer | Sandboxed agents. |
| git, Claude Code, Codex | The agents and the chief. |

```sh
sudo apt-get install build-essential pkg-config git curl libasound2-dev libxkbcommon-dev \
  libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev \
  mesa-vulkan-drivers xdg-utils xdg-desktop-portal libsecret-tools gnome-keyring
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# KVM: load kvm_intel or kvm_amd if /dev/kvm is missing
ls -l /dev/kvm
sudo usermod -aG kvm "$USER"   # then log out and back in
id -Gn                         # must list kvm
```

`imacs setup` (step 3) checks the rest and prints the install command for anything missing:
`curl -fsSL https://claude.ai/install.sh | bash` for Claude Code, Node.js/npm and then
`npm install -g @openai/codex` for Codex, and for sbx:

```sh
curl -fsSL https://get.docker.com | sudo REPO_ONLY=1 sh
sudo apt install docker-sbx
```

Optional:

- Helix (`hx`) for editor mode, or Neovim via `[editor]` (doctor warns while neither is set up).
- `sccache`, shared by every agent's builds.
- Node.js's `npx`, for the Linear ticket bridge.
- Azure CLI 2.54 or newer, only if you enable Azure (step 7).
- Docker Engine, only for the container QA harness in [Testing on Linux](../README.md#testing-on-linux).
- A btrfs or reflink-enabled XFS filesystem for your repositories. New worktrees then start with a warm
  `target/`; on ext4 IMACS skips warming and agents build cold (see
  [Running many agents](../README.md#running-many-agents)).

## 2. Install IMACS

Clone this repository, then from the checkout:

```sh
scripts/linux-install.sh
imacs --version
```

The installer builds a release binary for the native target and atomically replaces `~/.local/bin/imacs`,
even while IMACS runs. It installs a desktop entry and icon under `~/.local/share`, then runs
`imacs setup` in your terminal. Add `~/.local/bin` to `PATH` if it says so.
`scripts/linux-install.sh --uninstall` removes the binary, desktop entry and icon, and keeps config and state.

## 3. Run `imacs setup`

```sh
imacs setup                  # or: imacs setup --workspace daily
```

Setup creates a missing config, then works through the machine in order. It shows each missing tool
(git, claude, codex, secret-tool, sbx) and each failed KVM check (KVM, `kvm` group) with what to do, and
waits: fix it in another terminal, then press Enter to check again, or `q` to stop. It goes on only when
all of them pass. A distribution other than Ubuntu 24.04+ is only a warning. After that it asks before each change:

1. Docker sign-in (`sbx login`, in the browser).
2. sbx's global network policy, only when none exists (`[isolation.sandbox] network`, balanced by default).
3. Claude. Claude agents run sandboxed by default. While neither a subscription token nor sbx's
   `anthropic` secret is stored, setup asks for the token `claude setup-token` prints and keeps it in the
   OS keychain for IMACS's credential relay. An Anthropic API key pasted there goes to sbx's
   `anthropic` secret instead.
4. An OpenAI API key, when sbx has no `openai` secret: into the keychain and to sbx, for sandboxed Codex.
5. sbx's Linear MCP server (`sbx mcp add linear`, `sbx mcp auth linear`) when a ticket profile uses
   Linear. See [Tickets](tickets.md#step-by-step-linear).
6. Removing sbx's `github` secret and misfiled secrets, and turning off SSH agent forwarding.

It then rechecks everything and runs `imacs doctor`. Values travel over stdin, hidden on a terminal, and
never appear in arguments, output, logs or config. Ctrl-C, EOF or a declined step stops setup, and
running it again resumes where it stopped. [Fresh machine: one guided
command](isolation.md#fresh-machine-one-guided-command) has the details.

To store or replace one credential without the rest of setup, use `--credential`. The value is read from
stdin only; there is no flag or argument that takes it:

```sh
claude setup-token                    # prints a sk-ant-oat01-… token
imacs setup --credential claude       # paste it
imacs setup --credential openai
imacs setup --credential linear
imacs setup --credential github
imacs setup --credential registry --registry-host registry.example.com
```

Without `secret-tool`, or with the keyring locked, these fail with `OS credential store unavailable;
unlock it and retry`. **Settings → Credentials** in IMACS shows and edits the same store. See
[Settings → Credentials](isolation.md#settings--credentials).

## 4. Check with `imacs doctor`

```sh
imacs doctor
imacs doctor -w daily        # with a workspace's isolation and ticket settings
```

Doctor changes nothing. It prints the machine's `Host:` name and the config files it loaded, then a line
per check: `✓` passed, `✗` failed with a hint. It exits with 1 only when git is missing or the config does
not load; everything else is a warning. On a new desktop, expect these until you start IMACS or opt in:

- `IMACS running` (start it in step 9) and `global Claude hooks installed` (optional, `imacs install-hooks`).
- `hx not found` without Helix.
- `CoW cache cloning … cache warming will be skipped` on ext4.
- `Azure CLI relay: off` until step 7.

The sandbox checks should all pass: `Docker Sandboxes ready`, the network policy, `sandbox Claude
credential: Claude subscription through IMACS's relay (OS keychain)`, the OpenAI secret, MCP, SSH
forwarding, the prerequisites and the three `sandbox Linux:` checks. `credential store` must not warn.

## 5. Build a warm template (optional)

A template is a saved sandbox snapshot with the toolchain, package caches and a first build in it, so
sandboxed agents start warm. Tags are local to this machine; build them again after moving. Name the tag
in the workspace file (or a repository's `.imacs/config.toml`), then build from the project's checkout:

```toml
# ~/.config/imacs/workspaces/<host>/daily.toml
[isolation.sandbox]
template = "myproject:local"
```

```sh
imacs --workspace daily sandbox template build myproject --agent codex
# or by folder:
imacs sandbox template build ~/repos/myproject --agent claude
```

Use the agent your sandboxed tasks run as. Without a tag the command stops with `set
[isolation.sandbox] template in the workspace or .imacs/config.toml before building`. For a Rust
repository the template runs `cargo check -p imacs-core` (it is made for this repository); for .NET pass
`--dotnet-project`. See [Warm templates](isolation.md#warm-templates).

## 6. Check the relay: `imacs sandbox relay-check`

```sh
imacs sandbox relay-check
```

It makes one throwaway Claude sandbox through the same steps as an agent's, checks that the sandbox
reaches the relay, that Claude answers through it on your subscription, that nothing in the VM looks like
a Claude token except IMACS placeholders, and that a revoked placeholder is refused. It always removes the
sandbox, its network rule and its folder, and ends with `PASS relay check` or a `FAIL` line. No line holds
a value.

This is the first thing to try on a new Linux desktop: the relay has run end to end on macOS, but not yet
on Linux. If it fails at the reachability step, the message names the sandbox and port: check that
`sbx policy ls <sandbox>` lists `localhost:<port>`, and whether `host.docker.internal` leads the VM to this
machine's loopback. If it says the keychain holds no token, store one with `imacs setup --credential
claude`. See [Sandboxed Claude on your subscription](isolation.md#sandboxed-claude-on-your-subscription).

## 7. Azure (opt-in)

Agents can query Application Insights and Azure Resource Manager through your host `az login`, without
an Azure credential in the VM. It is off until you turn it on:

1. Install the Azure CLI (2.54 or newer) and run `az login` on the host.
2. Turn on **Enable Azure CLI relay** in Settings → Credentials, or set `[isolation.sandbox.azure]
   enabled = true` in `config.toml` or this machine's `config.d/<host>.toml`.
3. `imacs doctor` should say `✓ Azure CLI relay: enabled, az found, logged in`.
4. Run the read-only check with an Application Insights application ID you choose:

   ```sh
   imacs queue --class qa -- imacs sandbox azure-check <application-UUID>
   ```

   It prints `PASS Azure relay check: count=<n>`. While Azure is off it stops with `Azure CLI relay is off;
   enable it in Settings → Credentials first`. The check reads `config.toml` and the host file, not a
   workspace's.

GET requests pass; writes wait for your approval in the agent's banner. See [Azure and Application
Insights through host `az login`](isolation.md#azure-and-application-insights-through-host-az-login).

## 8. Run the Linux sandbox smoke test

From this checkout:

```sh
imacs queue --class qa -- scripts/linux-sandbox-smoke.sh
```

The script runs doctor and `imacs sandbox relay-check`, then starts an isolated IMACS (its own state,
socket, config, runtime and temporary folders) with two sandboxed shells in disposable worktrees. It checks
terminal output, file sync and control latency, removes everything it made, and checks for leftovers. It
ends with `PASS Linux sandbox smoke (real sbx)`. `--skip-relay` leaves out the relay check; `--imacs`,
`--timeout` and `--latency-ms` are described in [Testing on Linux](../README.md#testing-on-linux).
Without a running IMACS, `imacs queue` runs the command at once.

## 9. Start IMACS and the chief

The chief runs bare, on this machine, with your own Claude Code login. Sign in once in a terminal:
run `claude` and follow the prompt. (With `[isolation] bare_relay = true` bare Claude sessions, the chief
included, use the subscription in the keychain through the relay instead.)

```sh
imacs ~/repos/myproject        # or start IMACS from the desktop entry
imacs --workspace daily        # after saving a workspace (Project › Open project…, then save)
```

Click **CHIEF** in the top bar, or press Ctrl+Shift+O (`cmd+o`; on Linux `cmd` is Ctrl+Shift). The chief
starts in the project's main checkout with every IMACS tool. A good first request is to run
`imacs_doctor` and read the result with you. From there it writes plans that wait for your approval on the
board; see [The chief agent](../README.md#the-chief-agent).

## 10. Restart onto a new build

```sh
git pull
scripts/linux-install.sh
```

Then ask the chief to restart (it calls `imacs_restart`), or run:

```sh
imacs ctl '{"cmd":"restart","reason":"installed the new build"}'
```

Restart refuses, and lists what blocks it, while agents work, an approval is pending or queued work
runs; wait for those rather than forcing. IMACS saves its plans and sessions, quits, and starts the same
binary with the same arguments. The chief resumes, and its next `imacs_wait` gets a `restarted` event
naming the old and new build; `imacs --version` prints the build too. On a machine where setup is
complete, the installer's final `imacs setup` asks nothing. If it stops with `setup input ended`, the new
binary is already installed: run `imacs setup` in a terminal. See [Restart onto a new
build](../README.md#restart-onto-a-new-build).

## 11. Verify cleanup

IMACS removes what it makes. Completing a plan, **Clean up plan…** and the **clean up…** dialog remove
worktrees together with their sandboxes, MCP registrations and relay rules. Closing a scratch session
removes its folder and sandbox. The next start sweeps whatever a crash left. To check by hand, once the
work you started is closed:

```sh
sbx ls                                    # imacs-<session id> only for sessions you can still resume
sbx mcp ls                                # imacs-<session id> for those same sessions, plus servers you added
git -C ~/repos/myproject worktree list    # the main checkout, plus worktrees of open plans and sessions
ls ~/.local/state/imacs/scratch           # one folder per open Ungrouped session
imacs doctor                              # "cleanup leftover (warning)" names anything not removed
```

- A stopped `imacs-<session id>` sandbox is kept on purpose while its worktree exists, so the session
  can resume. Removing the worktree (plan cleanup or **clean up…**) removes it.
- `imacs-template-…`, `imacs-relay-check-…` and `imacs-azure-check-…` exist only while those commands run.
  The two checks leave an empty `relay-check/` folder in the state folder.
- Scratch folders go when their session closes; quitting ends scratch sessions, and the next start
  removes their folders (a restart resumes them instead).
- IMACS only removes resources this state folder recorded in `sandbox-ownership.json`. Sandboxes of
  another IMACS home, such as a smoke test's, are left alone.

See [Lifecycle](isolation.md#lifecycle) for the full rules.

## Still macOS-only or not yet verified on Linux

- **App bundle.** `scripts/macos-app.sh` builds `IMACS.app` on macOS only; Linux uses
  `scripts/linux-install.sh` and its desktop entry. Restart works on both: the bundle on macOS, the same
  binary on Linux.
- **Relay and real sandboxes on Linux.** `imacs sandbox relay-check`, the real-sbx smoke test, warm
  template timings and the sandboxed Claude end-to-end run have only run on macOS. In particular,
  whether `host.docker.internal` reaches this machine's loopback from a Linux sandbox is unverified.
- **Sandbox memory and CPU on tiles.** The sampler finds each sandbox's VM process by its name or by the
  worktree it holds open (through `lsof`). Recorded process listings cover this; a real sbx VM on Linux
  has not been checked, and without a match the tile shows **Sandbox** alone.
- **Spotlight.** Doctor's indexing check exists on macOS only.
- **Microphone.** macOS asks for permission; Linux records from the default input device.
- **Wayland.** IMACS is built with Wayland and X11 support; only X11 has been exercised.
- **Credentials.** macOS's keychain is always there; Linux needs `secret-tool` and an unlocked Secret
  Service in the desktop session, so an SSH-only session cannot read or store credentials.

## Verified

On 2026-10-30 these steps were run against the code in the Linux QA container (Debian bookworm, Rust 1.98,
no KVM, no sbx, no Secret Service, a fresh `HOME`): `scripts/linux-install.sh` install and uninstall;
`imacs setup` listing the missing prerequisites with their install commands; `imacs setup --credential`
refusing a value flag and failing without a keyring; `imacs doctor`; `imacs sandbox relay-check`,
`imacs sandbox azure-check` and `imacs sandbox template build` stopping with the messages above; `imacs
queue` without a running IMACS; `scripts/linux-sandbox-smoke.sh --fake-sbx`; and the git bundle steps in
[Moving to a new machine](moving-machines.md). Steps that need KVM, a real sbx, a keyring, Claude or Azure
were checked against the code only.

# Moving to a new machine

ATOMICS splits what it keeps into config, which you can take with you, and runtime state, which belongs to
one machine. Credentials and sign-ins are set up again on the new machine. Plans still in progress need
one decision before you move: finish them, or carry their integration branch and plan them again.

[Using ATOMICS on several machines](../README.md#using-atomics-on-several-machines) covers sharing one config
between machines that run at the same time; this page is about leaving one behind.

## Copy: the config folder

`~/.config/atomics/` (or `$XDG_CONFIG_HOME/atomics`, or the folder of `ATOMICS_CONFIG`) holds:

| File | What to do |
| --- | --- |
| `config.toml` | Copy as is. Paths written as `~/…` expand against the new home. |
| `config.d/<host>.toml` | One file per machine. Write one for the new machine's host name: its cores, programs' paths, repositories. Keep the old one if the folder is shared. |
| `workspaces/<host>/<name>.toml` | Saved workspaces are per host. Copy the old host's files into `workspaces/<new host>/`, or ATOMICS on the new machine will not list them. Flat `workspaces/<name>.toml` files are read on every host. |
| `themes/` | Copy as is. |
| `*.bak` | Backups from applied config proposals; copy only if you want them. |

`atomics doctor` on the new machine prints its `Host:` name and every config file it loaded. Clone your
repositories to the same places under your home, so `~/repos/…` in `repos`, workspace `projects`,
`allow_hosts` and `mounts` keep pointing at them; put anything machine-specific in the new host file.

## Keep local: the state folder

The state folder is `~/.atomics` on macOS and existing installs, and `~/.local/state/atomics` (or
`$XDG_STATE_HOME/atomics`) on new Linux installs. Leave it behind. It holds `sessions.json`, `plans/`,
`briefs/`, `scratch/`, `git-pins/`, `sandbox-ownership.json`, `sandbox-templates.json`, the relay's port,
log and usage, and the crash and restart logs. These name this machine's paths, sessions, sandboxes and
conversations, which do not exist on the new one.

Do not copy `~/.atomics` to a new Linux home: an existing `~/.atomics` is used as the state folder, so ATOMICS
would load the old machine's sessions and plans.

## Set up again

- **ATOMICS.** Follow [Fresh start on Linux](fresh-start-linux.md): `atomics setup` signs in to Docker,
  sets sbx's network policy and secrets, registers sbx's Linear server when a ticket profile uses Linear,
  and stores the Claude token in the OS keychain. Store the other credentials you used with `atomics setup
  --credential <kind>`. The keychain and sbx's secrets never leave a machine.
- **Claude Code and Codex.** Sign in to both on the new machine. Run `claude setup-token` there for the
  relay's token.
- **Tickets.** Run the ticket bridge's command once to sign in, and export every `token_env` variable in
  your login shell. See [Tickets](tickets.md#step-by-step-relay).
- **Azure.** Run `az login` if you use the Azure relay.
- **Templates.** Saved sandbox templates are local to a machine and its architecture; build them again
  with `atomics sandbox template build`.
- **Global hooks.** If you used `atomics install-hooks`, run it again: the hooks name the installed binary.
- **The chief.** Its instructions come along: `[chief] prompt` in the config files, and each project's
  `ATOMICS.md` and `.atomics/` in the repository. Its conversation stays behind; the chief on the new machine
  starts fresh.

## Plans in progress

A plan file is not portable. In `plans/<id>.json`, the plan's `project` and each task's `repo` and
`worktree` are absolute paths on the old machine, `session` names a session in that machine's
`sessions.json`, and `resume_id` names a conversation that lives in the old machine's Claude folder or in
a sandbox there. The integration branch and task branches are refs in the old machine's clone, and are on
no remote unless you pushed them. So never copy plan files; choose one of these instead.

### Finish first (recommended)

On the old machine, let running tasks report, accept and integrate them, and let the final review run.
Then do what you normally do with finished work (open a PR, for example), and complete the plan.
Completing it removes clean task worktrees, their sandboxes and the task branches merged into the
integration branch; once the work is in the base branch, **Clean up plan…** removes the rest. Nothing is
left to move.

### Carry the integration branch

When a plan cannot finish before the move, take its accepted work along as a git bundle, without
publishing it, and plan the rest again on the new machine.

On the old machine:

1. Integrate every task whose work you want to keep (`atomics_plan_integrate`, or **integrate** on its card).
   Integration commits what a task left uncommitted, so the integration branch then holds all of it. Work
   you do not integrate stays behind.
2. Keep a copy of what is left to do: `atomics_plan_get {plan}` shows the goal and every task's brief and
   status.
3. Bundle the branch and check the bundle:

   ```sh
   git -C ~/repos/myproject bundle create ~/plan-<id>.bundle atomics/plan-<id>
   git bundle verify ~/plan-<id>.bundle
   ```

4. Copy the bundle to the new machine as you would any private file: it holds unpublished code.
5. Archive the plan (archiving keeps its branches).

On the new machine, after cloning the repository:

```sh
git -C ~/repos/myproject fetch ~/plan-<id>.bundle atomics/plan-<id>:atomics/plan-<id>
```

Then give the chief the goal and the remaining tasks, and ask it to create a plan with `integration`
set to `atomics/plan-<id>`. New task branches are created as usual (a task cannot reuse an existing
branch). Tasks with dependencies start from the integration branch, so they see the carried work; tasks
without dependencies start from `base_ref` and meet it when they are integrated. The final review covers
the whole branch. The old agents' conversations do not come along: briefs must stand on their own.

# GitHub in ATOMICS

ATOMICS shows the open pull requests of the repositories you have loaded. You can review a pull request's
diff in the diff viewer, merge it, and run your release workflows without leaving the window. This
works with any GitHub repository and any GitHub Enterprise Server host. The design is recorded in
[decisions/2026-10-31-github-integration.md](decisions/2026-10-31-github-integration.md).

## The counter and the controls

A git icon in the top bar shows how many open pull requests the counter's filter counts. It is
coloured as a warning when one of them is failing checks, and shows `–` until GitHub is connected. Click
it, or press **Command+G** on macOS or **Ctrl+G** on Linux, to open the controls. Press it again or
Esc to close them. While they are open they take the keyboard: nothing you type reaches the agent's
terminal or the editor behind them.

The controls list every open pull request, grouped by repository. Each row shows its number and title,
author, checks (✓ passing, ✗ failing, ● running), review state and merge state, such as `blocked` or
`behind`. A ◆ marks the rows the counter counts. The selected pull request offers:

- **Open**: the pull request in your browser.
- **View diff**: its diff, as GitHub shows it against the merge-base, in the diff viewer. Its header
  shows the head commit it was loaded at. It is read-only: there are no review comments, and the editor
  stays closed, including through its shortcut. Nothing is fetched, and no branch or worktree changes.
  The view closes when you pick a session or worktree, close the diff, or open another pull request.
- **Merge**: offered with the methods the repository allows (squash, merge commit, rebase), when GitHub
  says the pull request can merge. ATOMICS sends the head commit whose diff you loaded, or the head the
  list shows if you did not load it, and the confirmation names that commit. If the branch moved after
  you loaded the diff, GitHub refuses the merge instead of merging code you did not review. The
  confirmation warns you first; load the diff again to review the new head.
- **Admin merge**: shown for a blocked or behind pull request when GitHub reports
  `viewerCanMergeAsAdmin`, or every ruleset on its base branch lets you bypass it
  (`current_user_can_bypass` is `always` or `pull_requests_only`). If GitHub refuses the ruleset
  lookup with 403 or 404, it is offered only for repository admins (`viewerPermission: ADMIN`).
  This does what `gh pr merge --admin` does.
- The workflow actions configured for pull requests in that repository.

Workflow actions configured for a whole repository follow the list, each with its latest run's status.
Click a status to open that run.

Merges, admin merges and workflow runs each ask once before they happen: Enter confirms and Esc
cancels. Reading never asks.

### Keys in the controls

The controls take focus as soon as they open, so these keys work without clicking first. Esc or
Ctrl/Command+G closes them and returns focus to the pane or field that had it before.

| Key | |
|---|---|
| ↓ / ↑ (also with Shift) | next / previous pull request, across repositories |
| PageDown / PageUp | the first pull request of the next / previous repository |
| Enter | open the selected pull request in the browser |
| `D` | load its diff into the diff viewer |
| `M` | merge it (asks first) |
| Esc | close |
| Enter / Esc in a confirmation | confirm / cancel |

The other ATOMICS shortcuts and Ctrl/Command+1…9 keep working while the controls are open. A workflow
input in a confirmation keeps its keys while you type in it.

The shortcut is `[keys] github = "primary+g"`. `primary` is Command on macOS and Ctrl on Linux. Like
Ctrl/Command+1…9, the shortcut is taken before a focused terminal or editor, so the agent never sees it.
Its letter follows your keyboard layout, as the other shortcuts do: on Dvorak or Colemak it is the key
labelled G. If you bind it to a chord another binding uses, the top bar reports the conflict and the
default stays.

**On Linux this takes Claude Code's own Ctrl+G**, which opens the prompt in your editor. Inside ATOMICS,
Ctrl+G opens the GitHub controls, and Claude Code never receives it. To give it back, bind the controls
elsewhere, for example `[keys] github = "cmd+g"` (Ctrl+Shift+G on Linux). macOS is not affected: there
the shortcut is Command+G.

## Connect GitHub

Open the controls and press **Connect GitHub**:

1. If the `gh` CLI is signed in, ATOMICS asks "Use your gh account <login>?". Answering yes stores gh's token.
2. Otherwise ATOMICS opens `https://github.com/login/device` (or your Enterprise host's page) and puts a
   code on your clipboard. Paste the code there and press **Authorize**.

The browser sign-in uses the **ATOMICS GitHub App**:

| | |
|---|---|
| client ID | `Iv23liEnPEe65mKmXgWf` (public) |
| app | [`atomiskide`](https://github.com/apps/atomiskide) |

The app's permissions decide what ATOMICS can do, so no scopes are requested.

**A GitHub App for ATOMICS should opt out of user token expiration** (the app's settings, under
"Optional features"). Here is why. Refreshing an expiring user token needs the app's secret, and so does
revoking a token through the API. A desktop app cannot keep a secret, so ATOMICS never ships or stores
one. With expiration on, a token would simply stop working after eight hours.

The ATOMICS app has expiration turned off. If an app keeps it on, ATOMICS stores the expiry beside the
token and never refreshes. When the token expires, or GitHub refuses it, ATOMICS deletes it and shows
**Connect GitHub** again. The device flow needs no secret.

The app reaches only the repositories it is installed on. For each account with a listed repository the
app is not installed on, the controls show one row: **Install ATOMICS on <owner>**. The row opens
`https://github.com/apps/atomiskide/installations/new`.

`[github] client_id` and `[github] app_slug` replace these for a GitHub Enterprise host or a fork. A
client ID that starts with `Iv` is treated as a GitHub App. Any other client ID is treated as an OAuth
app, which is asked for the `repo` and `workflow` scopes.

Once connected, the controls show a **GitHub · <login>** chip. Click it to disconnect. That deletes the
token from the keychain and shows a **Revoke on GitHub** link to
[github.com/settings/apps/authorizations](https://github.com/settings/apps/authorizations), where you can
remove the app's authorization fully. ATOMICS does not call GitHub's revoke API, which needs the app's
secret.

The token is kept in the OS keychain's credential catalog as the `github-app`
entry, or `github-app:<host>` for an Enterprise host. It goes nowhere else:

- it is not written to a log, a plan or the config, and it is not shown;
- it is not a relay credential, so no agent and no sandbox can get it.

The separate `github` entry in Settings → Credentials is the read-only token for agents, and it is
unchanged. To use a personal access token instead of Connect GitHub, add it in Settings → Credentials as
**GitHub app sign-in**, with your Enterprise host in the host field if you have one.

## Configuration

```toml
[github]
enabled = true            # false hides the counter and the controls
hosts = ["git.example.com"]   # GitHub Enterprise hosts whose remotes count, besides github.com
repos = ["owner/name", "git.example.com/team/tool"]   # beyond the loaded projects' remotes
client_id = ""            # Connect GitHub's app; empty = the ATOMICS GitHub App (Iv23liEnPEe65mKmXgWf)
app_slug = ""             # its slug, for "Install ATOMICS on <owner>"; empty = atomiskide
filter = "mine"           # the counter: "mine", "review" (your review is requested) or "all"
poll_seconds = 60         # at least 15
```

The repositories are the GitHub remotes of the loaded projects, any remote on github.com or a listed
host, plus `repos`. For Enterprise hosts, ATOMICS uses `https://<host>/api/v3` and
`https://<host>/api/graphql`. On an Enterprise host, register a GitHub App (or an OAuth app) with
the device flow enabled, then set its `client_id` and, for a GitHub App, its `app_slug`. Turn off user token expiration for
the GitHub App. The GitHub App
needs these permissions: Pull requests (read and write), Contents (read and write, to merge), Checks
and Commit statuses (read), Actions (read and write, to run workflows), Workflows (read and write, to
merge pull requests that change `.github/workflows`) and Metadata (read).

ATOMICS asks for the list of pull requests with an ETag, so an unchanged list costs no rate limit.
Checks and mergeability are fetched only while the controls are open, on every poll while they are. When GitHub reports a rate
limit, ATOMICS waits as long as GitHub asks before polling again, or backs off from one minute up to 15.

### Workflow actions

ATOMICS ships no workflow actions. Add your own:

```toml
[[github.actions]]
label = "Release"
repo = "owner/name"         # or "host/owner/name"; empty: every listed repository
workflow = "release.yml"    # a workflow file name or its numeric id
ref = "{base}"              # empty: {base} for scope "pr", the default branch for scope "repo"
scope = "pr"                # "pr": a button on each pull request; "repo": a button on the repository
inputs = { sha = "{head_sha}", pr = "{pr}", version = "" }
```

| Placeholder | |
|---|---|
| `{repo}` | `owner/name` |
| `{pr}` | the pull request's number |
| `{head_sha}` | its head commit |
| `{head}` | its head branch |
| `{base}` | its base branch |

`repo` actions accept only `{repo}`. An unknown placeholder, a pull-request placeholder in a `repo`
action, or a workflow that is not a file name is reported in the top bar. An input left empty is asked
for in the confirmation. The confirmation shows the ref and every input before the workflow runs.

In the configuration tools, `github.hosts`, `github.repos`, `github.client_id` and `github.app_slug` are labelled
"sends credentials" and `github.actions` is labelled "runs commands", so the chief cannot change them
without you seeing it.

# GitHub integration: PR counter, controls modal, PR diffs, workflow actions

Status: accepted, 2026-10-31. User guide: [docs/github.md](../github.md).

ATOMICS shows the open pull requests of the repositories it has loaded, lets you review a PR's diff in
the diff viewer, merge it, and run configured workflows, without leaving the window. It works with any
GitHub repository and any GitHub Enterprise Server host. The code contains no repository names and
ships no repository-specific defaults.

## Sign-in

Sign-in is a single **Connect GitHub** button. It shows in the modal while no usable token is stored, and the
top-bar icon opens the modal on it.

1. **The `gh` CLI.** If `gh auth token --hostname <host>` prints a token, ATOMICS reads the login with
   `GET /user` and asks once: "Use your gh account <login>?". Confirming stores that token.
2. **Otherwise, the device flow.** ATOMICS requests a user code, opens `https://<host>/login/device`
   with the existing `connections::Browser`, and puts the code on the clipboard. The modal shows the code
   and "Paste it in the browser and press Authorize". ATOMICS then polls with the `interval` GitHub
   returns, adding 5 s on `slow_down`, and stores the token.

The device flow signs in to the **ATOMICS GitHub App**: `DEFAULT_CLIENT_ID = "Iv23liEnPEe65mKmXgWf"`
(a public ID, safe to ship) and `DEFAULT_APP_SLUG = "atomiskide"`. `[github] client_id` and `[github]
app_slug` override them, for a GitHub Enterprise host or a fork. The flow works with both kinds of app:

- **Scopes.** GitHub App client IDs start with `Iv`. Those get no `scope` parameter, because the app's
  permissions apply. Any other client ID is an OAuth app and is asked for `repo workflow`.
- **No refresh, no secret.** Refreshing a GitHub App's expiring user token, or revoking it through the
  API, needs the app's secret. A desktop app cannot keep a secret, so ATOMICS ships and stores none.
  The ATOMICS app opts out of token expiration, so its user tokens do not expire. If a token response
  still carries `expires_in` (an app that kept expiration on), ATOMICS stores the expiry beside the token
  and discards any refresh token. Every worker opens the host through `github::Signed`:
  - a token past its expiry is removed before it is sent;
  - a token GitHub answers 401 to is removed;
  - in both cases the modal shows Connect GitHub again, and the device flow needs no secret.
- **Installation coverage.** With a GitHub App user token, while the modal is open, ATOMICS lists
  `GET /user/installations` and each installation's repositories. Each account that has a listed
  repository without the app gets one row in the modal: **Install ATOMICS on <owner>**. The row opens
  `https://github.com/apps/<slug>/installations/new` (`https://<host>/github-apps/<slug>/installations/new`
  on Enterprise). There is no other UI for it. A failed coverage check shows no rows, never an error.
  OAuth and `gh` tokens reach what the user reaches, so their coverage is not checked.

Once connected, the modal header shows a "GitHub · <login>" chip with **Disconnect**. Disconnecting
deletes the keychain entry and shows a link to `https://<host>/settings/apps/authorizations`, where the
user can revoke the app fully. ATOMICS does not call the revoke API.

A **personal access token** is only a fallback, in Settings → Credentials ("GitHub app sign-in", with an
optional host for Enterprise). It is not part of the main flow.

All tokens go in the existing credential catalog (`credentials.rs`, one OS-keychain item), under
`github-app` for github.com and `github-app:<host>` for Enterprise hosts. An entry holds the token and, for
an app that kept expiration on, its expiry time. Only these entries may hold an expiry. The catalog's rules apply: the
value is validated (`gho_`, `ghp_`, `ghu_`, `github_pat_`), never quoted in errors, and only its status
is shown. This entry is **not** a relay credential: `Store::credential` returns `None` for it, so no
agent, sandbox, grant or relay log ever sees it. The existing read-only `github` relay entry for agents
is unchanged and separate. In memory the token is wrapped in a type whose `Debug` prints `<hidden>`. It
is never serialized, logged, or written to a plan or the store, and is sent only as an `Authorization`
header to the host's API.

## Repositories

The list is the GitHub remotes of the loaded projects plus `[github] repos = ["owner/name",
"host/owner/name"]`. A remote counts when its host is `github.com` or one of `[github] hosts`. HTTPS,
`ssh://` and scp-style `git@host:owner/name.git` URLs are parsed. Duplicates are merged. The API base
is `https://api.github.com` for github.com and `https://<host>/api/v3` (`/api/graphql`) otherwise.

## Polling and rate limits

A worker thread polls each repository every `[github] poll_seconds` (default 60, minimum 15) with
`GET /repos/{o}/{r}/pulls?state=open&per_page=100` and `If-None-Match`. A 304 costs no rate limit and
changes nothing. On 403 or 429 with rate-limit headers (`x-ratelimit-remaining: 0`, `retry-after`),
it waits until `retry-after` or `x-ratelimit-reset`, otherwise backing off exponentially from 60 s to
15 min. A 401 removes the token and shows Connect GitHub. Polling goes on while the window is hidden,
so the counter is current when it shows again. Details (checks, review state, mergeability) come from
one GraphQL query per repository. That query runs only while the modal is open: when it opens, after
any action, and on every poll while it stays open, because checks finish without changing the list's
ETag. The installation coverage check runs on the same polls.

## Counter filter

`[github] filter` picks which open PRs the top-bar counter counts:

- `mine`, the default: PRs the viewer authored;
- `review`: PRs where the viewer's review is requested;
- `all`: every open PR.

The modal always lists every open PR and marks the counted ones. With no token, the icon shows `–` and
opens the modal on Connect GitHub. The filter, repositories and actions are set in the config only;
the modal has no settings.

## PR diff in the viewer

**Load diff** requests `GET /repos/{o}/{r}/pulls/{n}` with `Accept: application/vnd.github.diff`. This is
the diff GitHub shows, against the merge-base. ATOMICS parses it with `diff::parse_unified` and prepares
it with the same highlighter the worktree diffs use. The result goes into the existing diff viewer as a
read-only view titled `owner/name #n`, with the head → base branch and the head commit:

- review comments cannot be sent (no agent receives them);
- "editor", double-click and the `[keys] toggle_editor` shortcut show a notice instead of opening a file;
- refresh re-fetches the diff.

Before the diff, ATOMICS reads the PR's head commit (`GET /repos/{o}/{r}/pulls/{n}`) and records it as the
reviewed head that Merge sends. Reading the head first means a push between the two requests leaves the
recorded head older than the diff, so the merge is refused rather than merging code that was not shown.

No `git fetch`, no ref and no worktree is touched, so an agent's checkout cannot change under it. The
view closes when you select a session or worktree, close the diff, or open another PR. Diffs over
GitHub's limit (HTTP 406) show the API's message; **Open** shows the PR on GitHub. Fetching the head into a hidden
ref was rejected: it needs a local clone of every listed repository, and fetching writes into a
repository an agent may be using.

## Merge and admin merge

The GraphQL query reads `mergeCommitAllowed`, `squashMergeAllowed`, `rebaseMergeAllowed` and, for each
PR, `mergeable`, `mergeStateStatus`, `reviewDecision`, `viewerCanMergeAsAdmin` and the head commit's
`statusCheckRollup`. **Merge** offers only the repository's allowed methods, in the order squash, merge,
rebase. It is enabled when `mergeStateStatus` is `CLEAN`, `HAS_HOOKS` or `UNSTABLE`. It sends `PUT
/repos/{o}/{r}/pulls/{n}/merge` with `merge_method` and `sha`. The `sha` is the head whose diff was
loaded in the diff viewer, or the head the list shows when no diff was loaded. A push after that makes
GitHub answer 409, so unreviewed code is never merged. The confirmation shows the commit it sends. When
the listed head has moved past the reviewed one, it says the merge will be refused and to load the diff
again.

**Admin merge** appears only when `viewerCanMergeAsAdmin` is true and the PR is `BLOCKED` or `BEHIND`.
GitHub sets that flag when the viewer may bypass the branch rules. Admin merge sends the same merge
call; GitHub accepts it because of the viewer's bypass right, as `gh pr merge --admin` does. Drafts,
`DIRTY` (conflicting) and `UNKNOWN` PRs offer neither button.

## Workflow actions

`[[github.actions]]` entries are configured per user:

```toml
[[github.actions]]
label = "Release"
repo = "owner/name"          # or "host/owner/name"; omitted: every listed repository
workflow = "release.yml"     # a file name or a numeric workflow id
ref = "{base}"               # default "{base}" for PR actions, else the default branch
scope = "pr"                 # "pr": a button on each PR; "repo": a button on the repository
inputs = { version = "", sha = "{head_sha}", pr = "{pr}" }
```

The placeholders are `{repo}` (owner/name), `{pr}` (the number), `{head_sha}`, `{head}` (the head branch)
and `{base}` (the base branch). `repo` scope accepts only `{repo}`. An unknown placeholder, or a PR
placeholder in `repo` scope, is a config warning shown in the top bar; the rest of the config applies. An input whose value is empty is
asked for in the confirmation dialog. Dispatching sends `POST
/repos/{o}/{r}/actions/workflows/{workflow}/dispatches`. Each action shows its latest run's status from
`GET .../workflows/{workflow}/runs?per_page=1`, with a link to the run. ATOMICS ships no actions.

## Confirmations

Merge, admin merge and dispatch each confirm once, in a dialog styled like "Close session?". The dialog
names the repository, PR, method and head commit, or the workflow, ref and filled-in inputs. Enter
confirms and Esc cancels. Reads never confirm:
listing, refreshing, open in browser and load diff. There is one in-flight action per PR. Its result
appears in the modal and as a toast.

## Shortcut

`[keys] github = "primary+g"`. `primary` is a new chord modifier: Command on macOS and Ctrl on Linux
(`cmd` keeps meaning Ctrl+Shift off macOS). The shortcut is consumed before the terminal, like
Ctrl/Command+1–9, so a focused agent or editor never receives it. Its letter is the one the keyboard
layout types, as for the other `[keys]` bindings, so on Dvorak or Colemak it is the key labelled G.
Only digits match their physical keys, like Ctrl/Command+1–9. Layouts without Latin letters get the
Latin key at that position from egui. Pressing it again, or Esc, closes the modal. On Linux, Ctrl+G is
also Claude Code's own shortcut for editing the prompt in an external editor. Inside ATOMICS it opens
the controls instead, unless `[keys] github` is bound elsewhere. At config load, `github` and
every other `[keys]` binding are resolved to their platform chords. A `github` binding that does not
parse, or equals another binding or Ctrl/Command+1–9, is reported in the top bar as a config warning
(the config still loads, and the config tools do not report a load error) and falls back to
`primary+g`. If `primary+g` is taken as well, there is no shortcut; the top-bar icon still opens the
modal.

## Keyboard in the modal

The modal follows the diff's model:

- **Shift+↓/↑** moves the selection to the next or previous PR, across repositories;
- **PageDown/PageUp** jumps to the first PR of the next or previous repository;
- **Enter** opens the selected PR in the browser and **D** loads its diff;
- **M** starts a merge, which still confirms;
- **Esc** closes the modal;
- in a confirmation, **Enter** confirms and **Esc** cancels.

While the modal is open it holds the keyboard: these keys act, and every other key, typed text and paste
is dropped. Nothing reaches the terminal or editor that had focus. The other ATOMICS shortcuts and
Ctrl/Command+1–9 still work. A text field in the modal, such as a workflow input, keeps its keys. Once the
modal closes, the terminal gets its keys back.

## UI shape

There is no new window. The modal is an `egui::Window` anchored at the center, like the other dialogs.
It stays minimal:

- the header, with the "GitHub · <login>" chip, or Connect GitHub when no token is stored;
- the PR list, grouped by repository, one row per PR: number, title, author, checks symbol, review
  state and merge state (`BLOCKED`, `BEHIND`, …);
- the selected PR's actions: **Open**, **View diff**, **Merge** (with a method menu when the repository
  allows several) and **Admin merge** when allowed, followed by its PR-scoped workflow actions;
- the repository-scoped workflow actions, each with its latest run's status.

The counter is one more top-bar action: a git-branch glyph and the count, coloured warning when a
counted PR is failing checks. On narrow bars it folds into the Actions menu with the other controls.

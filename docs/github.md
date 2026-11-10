# GitHub and Azure DevOps in ATOMICS

ATOMICS shows the open pull requests of the repositories you have loaded, on GitHub and on
[Azure DevOps](#azure-devops). You can review a pull request's
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
  A server error, rate limit or network failure disables admin merge from rulesets while the rest
  of the poll continues. Results are cached per repository and base branch for five minutes;
  retargeting a pull request uses the new base's rules.
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

## Pull requests on clients

Desktop clients connected to a host (including the VM window) and browsers use the shared
**Pull requests · N** list. Click it or use the same configured `[keys] github` shortcut
(default Command+G on macOS/iOS, Ctrl+G elsewhere) to open or close it. The host's own controls above
keep their existing actions and keys.

Client rows appear under **Assigned to me**, **My pull requests**, and **Other**, using the host's
viewer and reviewer data for both GitHub and Azure DevOps (including Azure team assignments).
As in the Azure groups below, Other starts folded above eight rows when personal or assigned rows
are present; click its heading to expand it. Arrow keys skip folded rows.

Each row starts with its title, marks drafts, and shows `repo#number`, author and `head → base`
in muted text. Click the title or **View diff** to load its read-only diff. **Open** opens the pull
request in the browser; only HTTP and HTTPS URLs are accepted. **↑/↓** selects a visible row,
**Enter** loads its diff, and **Esc** closes the list and returns the keyboard to the terminal.
The shortcut and list keys are taken before terminal input.

Group, author and draft metadata are optional additive protocol fields. With an older host,
missing group data appears under Other, missing author and draft indicators are omitted, and
all rows remain available. Older clients ignore the added metadata.

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

Connected github.com accounts are also stored as relayed secrets in the OS keychain. Each account
has its own entry, so connecting another account keeps earlier accounts. In a VM client, use
**Settings → Credentials → Connect GitHub account** in the Mac window: its device code and browser
sign-in run there, and the secret syncs over the VM administrator connection to the private broker.
The host protocol carries metadata and session placeholders, never the token. Refresh credentials
retries an offline sync. The legacy app entry still serves the host's pull-request controls;
Enterprise connections keep their existing controls and are not publishing accounts.

Choose the publishing account with the optional GitHub login in the identity settings:

```toml
[identity]
github = "account-one"

[identity.projects."~/project"]
github = "account-two"
```

The same project → workspace → host/main precedence as commit identity applies. The sidebar shows
**GitHub: account-two** for that project; the identity editor can change it. An unset account grants
no publishing credential. A named account must be connected and its secret must include the
project's scope. Tokens and placeholders are omitted from diagnostic representations.

## Git and pull requests through the broker

Sessions receive Git's URL rewrite and HTTP header settings in their environment. Both HTTPS and
GitHub SSH origin URLs are routed through the relay; ordinary `git fetch` and `git push origin task`
work without `gh`. Only a session placeholder enters that environment. No token, credential helper
containing a token, or token-bearing URL is written into the worktree or Git config.

The account's credential permits only this project's `owner/repo`:

- Git smart HTTP: `GET owner/repo.git/info/refs` for upload/receive discovery and
  `POST owner/repo.git/git-upload-pack` or `git-receive-pack`.
- REST: list/create `/repos/owner/repo/pulls` and update `/repos/owner/repo/pulls/<number>`.
  Merge, workflow, account, other repository and other API paths are refused.
- Before a push, the broker privately reads that repository's current default branch. A request
  that changes or deletes that branch is refused before any update reaches GitHub. Malformed or
  compressed receive-pack requests, pushes from shallow clones, signed pushes, tags and other ref
  namespaces are unsupported.
  Redirects are never followed. A failed default-branch lookup fails closed.

**Refuse pushes to the default branch** is a setting on each connected account secret, on by
default (metadata field `refuse_default_branch = true`). Turn it off under Settings → Credentials
only when that account is meant to push default branches. The change revokes old grants, syncs to
the VM broker and takes effect in new sessions. Reconnecting preserves the setting and scope.
When off, ordinary branch updates, including default-branch updates, are allowed. This is a secret
rule, not an unconditional restriction.

Agents can also call the permitted PR endpoints using `ATOMICS_GITHUB_API_URL` and
`Authorization: Bearer $ATOMICS_GITHUB_PLACEHOLDER`. The broker substitutes the account's real token.
They may push and open or update pull requests directly, headless included, within this rule.

## The chief's Publish card

The chief's `atomics_publish` tool takes `plan`, an optional `branch` (the integration branch by
default), `title`, `body` and `base`. It prepares a card on the plan's board, with repository,
account, branch, commit IDs and authors, title and the full description. The identity guard checks
commit authors and forbidden words before the card is prepared. No push or PR API call happens
while preparing it.

Click **Publish** to push the exact reviewed commit and create or update the branch's open PR.
The worker checks the branch, base, origin and identity again. Changed work requires a new request.
Double clicks are suppressed. A failure remains on the card; a successful PR URL is saved on the
plan's card and recorded as its `pr_opened` lifecycle stage. **Dismiss** removes the pending card.
Task agents ask the chief to use this tool; neither the tool nor a card-decision operation is
exposed in their MCP tools. Connected board clients can also review and click the card.

This is the chief's review workflow, enforced at the app and tool boundary. **Inside the VM it is
a convention, not a guarantee:** agents share the host UID and may use their scoped GitHub
credential directly, including making PR calls without a card or click. The broker does not require
a signed write grant. A desktop bare agent likewise shares the credential holder's UID. See
[isolation.md](isolation.md#github-publishing-and-the-shared-uid) for the trust boundary.

The separate read-only `github` credential in Settings remains available for its existing uses.

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

In the configuration tools, `github.hosts`, `github.repos`, `github.client_id`, `github.app_slug` and `github.azure_devops` are labelled
"sends credentials" and `github.actions` is labelled "runs commands", so the chief cannot change them
without you seeing it.

## Azure DevOps

Azure DevOps Services repositories show in the same counter and controls. Nothing is configured per
organization: a loaded project whose remote is on Azure DevOps is listed, in any of its URL shapes
(`https://dev.azure.com/<org>/<project>/_git/<repo>`, `https://<org>.visualstudio.com/…`, and the
`ssh.dev.azure.com:v3/…` and `vs-ssh.visualstudio.com:v3/…` SSH forms). `[github] repos` adds others as
`"<org>/<project>/<repo>"` or `"dev.azure.com/<org>/<project>/<repo>"`. `[github] azure_devops = false`
leaves Azure DevOps out.

**Sign-in is the Azure CLI's.** Run `az login` once; ATOMICS asks `az account get-access-token` for an Azure
DevOps token when it needs one, keeps it in memory until two minutes before it expires, and sends it only to
dev.azure.com. Nothing is stored, and no agent gets it. The controls show an **Azure DevOps · <name>** chip,
or, when az is missing or signed out, a line saying to run `az login` with **Check again**. `az logout` signs
out.

**Groups.** A team lists many pull requests, so each Azure DevOps repository shows:

- **Assigned to me**: you are a reviewer, directly or through one of your teams in that project (a team listed
  as a reviewer assigns the pull request to its members, as in Azure DevOps's own "Assigned to me");
- **My pull requests**: you opened them;
- **Other**: everyone else's. It starts folded when you have pull requests in the groups above and there are
  more than eight others; click it to open or fold it. Folded rows are skipped by the arrow keys.

The counter's `filter` works as on GitHub: `mine` counts your pull requests, `review` those assigned to you.

**Details.** For your own and assigned pull requests, ATOMICS reads the policy evaluations: build policies are
the checks, the minimum-reviewers policy and the votes give the review state, and any blocking policy that is
not approved makes it `blocked`. The branch's merge strategy policy decides the methods offered (squash, merge
commit, rebase; semi-linear merge is not offered). Other people's pull requests show the list's state only and
offer no merge. Up to 1000 active pull requests are read per repository.

**Merge** completes the pull request with `lastMergeSourceCommit` set to the head whose diff you loaded (or the
listed head), so Azure DevOps refuses it if the branch moved since. It does not delete the source branch or
transition work items. **Admin merge** completes past the policies (`bypassPolicy`) and shows only when you hold
the repository's "Bypass policies when completing pull requests" permission and the pull request is blocked.

**View diff** builds the diff from the two sides' blobs against the merge-base, through the API; no local
repository is fetched or changed. Files over 2 MB and binary files show as binary.

**Pipelines.** `[[github.actions]]` entries run Azure Pipelines on Azure DevOps repositories: `workflow` is the
pipeline's numeric id, `ref` its branch, and `inputs` its template parameters. The latest run's status comes
from the builds of that pipeline.

```toml
[[github.actions]]
label = "Deploy to test"
repo = "contoso/Platform/api"
workflow = "42"
scope = "pr"
inputs = { environment = "test" }
```

Azure DevOps Server (on premises) is not supported yet.


## An agent reports its PR

The normal flow is chief to agent: after asking the user, the chief tells the task agent to push
its branch and open a PR using its scoped credential. The agent includes the PR in its report:

```json
{
  "status": "done",
  "summary": "Pushed the branch and opened the requested pull request.",
  "pull_request": {
    "url": "https://github.com/group/repo/pull/12",
    "number": 12,
    "head": "task",
    "base": "main"
  }
}
```

ATOMICS binds the report to that agent's own task. The head must match its task branch and the
GitHub URL must match the PR number. It saves the metadata on the task, shows a PR link with
`head → base` on the card, and records the task's `pr_opened` stage, including before acceptance.
Repeated reports of the same PR do not duplicate the stage. These are agent-reported details;
recording them does not independently verify the PR with GitHub or accept the task. The chief can
read them with `atomics_plan_get`. The chief's integration-branch Publish workflow similarly
records its result URL and `pr_opened` on the plan.

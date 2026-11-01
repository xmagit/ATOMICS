# Review: Relay as ATOMICS's remote control (RELAY-11972, design d1)

Reviewed: `docs/decisions/2026-11-01-relay-remote-control.md` at `1747bdc` (called **d1** below; `d1:N` is its
line N). Reviewer: host session with every repository, read only, 2026-11-01. Nothing was built or run.

Code read, after `git fetch`:
- foundation-template `origin/main` `44128b5e` (kernel, called **FT**);
- relay-backend `origin/main` `089e68c8d` (**RB**);
- relay-spatial `origin/main` `279e8d8`, `next/` (**RS**, Flat).

Also read: platform-gitops `origin/main` (**GO**) for what is deployed, relay-frontend `origin/main` (**RF**,
now at relay-v1.atomisk.ai) as a porting source, and the running RELAY-11955 a1 worktree (uncommitted). Live
calls: `whoami` and `get_ticket RELAY-11972`.

## Verdict

**Not ready to become implementation tasks.** The design's ATOMICS core holds up: a local authority rule,
pasted text only, Held during permission prompts, an outbox with per-write nonces, and redaction through the
trace `Cleaner`. But four things must change first:

1. The chief's Q2 answer (a ticket-linked personal conversation) replaces d1's kernel comment threads. The
   personal-conversation code already supplies owner-only steering, owner-only reading and attention push.
   It has **no ticket link and no MCP surface**, and it **requires a personal agent**. Max's Q1 clarification
   lets the chief connect as a product agent such as Max Claude, which can't own one. So R1 must generalize
   the conversation, and parts of X2/X4 must be rewritten (H1).
2. Flat, which is what relay.atomisk.ai serves, has **no PWA at all**: it unregisters the old service worker.
   So push to the phone doesn't work today, and R3 becomes a build, not a check (H2).
3. ATOMICS's attention comments can't notify anyone. MCP comments carry no mentions, and where mentions do
   notify, the lock screen shows the comment text (H3).
4. The doc still describes the options the chief's answers rejected (Q3, Q4). Its Q1 owner list is right,
   but it lets the Human-kind check be switched off and expects Relay to add an owner to product agents. Its
   Q5 hope fails: Relay's GitHub linker doesn't match ATOMICS branches (H4, H5).

What can start now: X1's remainder (whoami, comment identity paths, outbox) once RELAY-11955 a1 merges
(M5). Everything else waits for a d2 revision of the doc along the fixes below.

---

## 1. Relay capabilities, located

| d1 cites | Status | Where (file:line) |
| --- | --- | --- |
| Web Push subscriptions (ATOM-10637) | **exists** | FT `src/BuildingBlocks/Foundation.Hosting/WebPush/WebPushController.cs:26-27` (session-only route), `:38` antiforgery, `:52-55` no impersonation, `:67-77` serializable, ≤ 10 devices; `WebPushPolicy.cs:8-31` P-256 keys, `:33-38` provider hosts, `:40-63` local deep links within allowed prefixes |
| Web Push delivery (ATOM-10637) | **exists** | FT `WebPushSender.cs:37-41` payload (title ≤ 140, **body ≤ 300 chars of the notification message**), `:46-49` 3500-byte fallback to `/notifications`, `:56` TTL 1 h, `Topic` = notification id (repeats replace); `Consumers/UserNotificationPublishedEventConsumer.cs:18-21` only persisted, non-toast, ≤ 1 h old. RB `config/config_overrides_template.json:511-520` allows `/tickets`, `/notifications` and others. GO `secrets/sven/relay--relay-web-push.sops.yaml:8` sets `WebPush__Enabled` (encrypted) |
| Service-worker display, click routing (ATOM-10640) | **gone from relay.atomisk.ai** | It lived in relay-frontend (RF `src/platform/pwa/webPush.ts`, `registerServiceWorker.ts`, `productManifest.ts`, `scripts/service-worker.template.js`), now served at relay-v1.atomisk.ai (GO `inventory/products.json:824-837`). Flat is the default deployment (`:813`). It **unregisters** that worker (RS `next/web/sw.js:1-12`, `next/web/bootstrap.js:8-20`) and ships no manifest (`next/crates/relay-flat/index.html:1-30`) |
| Push for personal agent progress, attention, completion (ATOM-10692) | **exists, personal sessions only** | FT `Foundation.Hosting/Operations/AgentRunnerControlPlane.cs:1797-1806` (Progress/Completed from session events), `:2366-2377` (Attention on `WaitingForApproval`, `WaitingForInput`, `Failed`, once per turn and state), `AgentWorkOperations.cs:449-454` (5-minute progress coalescing), `AgentSessionOperations.cs:37-41` (markers only on personal-session agent messages), `KernelService.Service/Consumers/PersonalAgentUpdateRecordedEventConsumer.cs:19-44` (owner only; generic titles `:33-38`; deep link `/notifications?…&personalSession=…` `:39`) |
| Owner-only steering of a personal session (ATOM-10680) | **exists, runner protocol** | Owner sends: FT `KernelService.WebApi/PersonalAgentMessagesController.cs:13-55` (`POST …/sessions/{id}/messages`, `RequestingUserId == CurrentUserId` `:34-35`), `Foundation.Hosting/Operations/PersonalAgentMessageService.cs:45-47`; generic steering `AgentIdentitiesController.cs:804-858`; no impersonation `AgentSessionOperatorPolicy.cs:45,87`. Runner side: attach `IntegrationService.WebApi/PersonalAgentSessionsController.cs:95-115` under runner auth `:17-18`, which needs a personal runner with Conversation and InteractiveSessions capabilities `:175-185`; work record `AgentRunnerControlPlane.cs:1097-1175`; events `AgentRunnersController.cs:430`; acknowledgement under lease auth `:524-557`. The Codex bridge is `tools/Foundation.AgentRunner/PersonalSessionBridge.cs:9-40`; the protocol itself is runtime-neutral |
| Queued/delivered steering in the PWA (RELAY-10691) | relay-frontend only | Flat has generic steering (RS `next/crates/relay-api/src/lib.rs:652-653`) and session reads (`:658`). It lists only operational sessions (`:419`, `operationalOnly=true`; FT `AgentIdentitiesController.SessionPages.cs:108` then leaves personal ones out). **Flat shows no personal conversations** |
| Personal conversations are owner-private | **exists** | FT `AgentIdentitiesController.cs:1013-1038`, `:1099-1130` (personal identities visible to their owner) |
| Comment author is trusted (ATOM-11178) | **exists** | FT `Kernel.SharedLogic/Messaging/AddObjectCommentCommandConsumer.cs:68-73` (author == acting user), `:41` 1-8000 chars, `:63-66` ≤ 25 mentions; RB `TicketService.Service/Consumers/AddTicketCommentCommandConsumer.cs:16-17` (tickets inherit it). Kind: FT `KernelService.ReadModel.PostgreSql/Queries/ObjectCommentQueries.cs:92` (joined from the author's **current** row), `ObjectCommentModels.cs:24-25`, enum `Foundation.Kernel.Abstractions/UserIdentity.cs:6-11`. Live value: `"authorIdentityKind":"Agent"` (PascalCase) |
| `@agent` mentions start work (ATOM-10120) | **exists, in RB, not FT** | RB `TicketService.Integration/TicketIntegrationOperations.cs:504-600`: alias parse `:516`, resolve with **no explicit mentions** `:529-530`, alias work refused for agent writers `:567-572`; parser `KernelService.Contracts/Models/AgentMentionResolution.cs:29-59`. MCP `add_ticket_comment` has no mentions parameter: RB `TicketMcpTools.cs:149-168` |
| A mention notifies the person | **exists, browser comments only, with text** | FT `KernelService.Service/Consumers/ObjectCommentSubmittedEventConsumer.cs:136-157` ("You were mentioned", **body preview** `:172-174`, ≤ 500 chars); persisted by default `Kernel.SharedLogic/Realtime/UserNotificationClient.cs:19-28`, so it is pushed |
| Development links (ATOM-10006) | **exists** | MCP row: RB `TicketService.ReadModel/Queries/TicketIntegrationReadModels.cs:31-49,78`. Matcher: `TicketService.Integration/GitHubWebhook.cs:24-28` events; branch-only parsing for PRs `:49-52`, workflow runs `:94-96`, check suites `:132-134`, deployments `:244-247`; commit-message parsing only for delivery `status` `:198`; regexes `:517-527`. Repositories: GO `inventory/products.json:486,493-501` |
| `whoami` with owner (d1 gap G-R3) | **exists** (RB #748, `5d5599f49`, 2026-10-31) | RB `TicketMcpTools.cs:64-69`; `TicketIntegrationOperations.cs:1570-1595` (`kind` = `human` / `personalAgent` / `agent`, `ownerUserId`, `ownerDisplayName`), record `:1619-1627`. Live for max-claude: `kind:"agent"`, no owner (a product agent, as expected) |
| OAuth "Connect as agent" (Q1) | **exists** | FT `KernelService.Infrastructure/OAuth/OAuthConsentService.cs:243-251` (agent mode by default; person mode gated), `:271-308` (creates or verifies the agent, `AgentOwnerUserId == userId` `:304`), `:451` one agent per owner and tree node, `:552` default name "<name>'s agent"; person mode off by default `KernelService.Contracts/Models/McpOAuthOptions.cs:21-25`, not enabled on SVEN (GO `inventory/products.json:502-505`) |
| Comment paging | backwards only | FT `ObjectCommentQueries.cs:47` (take ≤ 200), `:65-68` before-only, `:70-98` (sequence = row id); RB `TicketMcpTools.cs:11-33` (`commentsTake` default **100**, not 20) |
| Message threads / agent channels over MCP | **missing** | RB `TicketMcpTools.cs:11-391` lists every ticket tool; none reads or writes a conversation |
| Record-stage tool (RELAY-11954 part 2) | **missing** | Not in `TicketMcpTools.cs`; the nearest is `record_delivery_handoff` `:315` |
| Rate limit on MCP tool calls | **missing** | RB `IntegrationService.WebApi/IntegrationWebApiModule.cs:116-150` limits pairing and connector OAuth only; MCP is mapped at `:401` with no limiter |

**Q5 answer: no.** The branch regex is `^(prefix)/(number)-slug$` (`GitHubWebhook.cs:517-521`), so
`atomics/relay-11967-…` and `atomics/plan-…` never match. PR titles are not read for PRs at all. The
`#RELAY-11968 - ` regex (`:523-527`) applies only to commit messages on delivery-status events (`:198`), such as
a squash-merge commit. And neither `xmagit/ATOMICS` nor `atomisk-ai/relay-spatial` is a connected repository
(GO `inventory/products.json:493-501`).

---

## 2. The chief's answers against the doc

| Q | Chief's answer | Code says | Doc must change |
| --- | --- | --- | --- |
| Q1 | Chief's answer (8753): "only the human owner of the OAuth personal agent ATOMICS signs in as". **Superseded by Max's clarification:** ATOMICS's own identity and the steering human are separate. The chief may connect as any agent; in Max's setup that is Max Claude, a product agent with broad, sysadmin-like permissions and no `AgentOwnerUserId`. "Max Falk's agent" is a separate OAuth personal agent. Steering and approval rights come from a configured **owner list of human user ids**. It defaults to whoami's `ownerUserId` when the connection is a personal agent, and must be set explicitly when it isn't. Comments from any agent identity never steer | Supported. whoami returns `ownerUserId` only for agents that have one (`TicketIntegrationOperations.cs:1575-1592`); live Max Claude is `kind:"agent"` with no owner. Consent creates "<owner>'s agent" with `AgentOwnerUserId` (`OAuthConsentService.cs:271-308,552`). Every agent's comments carry kind `Agent`, including personal agents (`ObjectCommentQueries.cs:92`, `UserIdentity.cs:6-11`) | d1's rule already matches: `d1:343-347` (owners if set, else whoami's owner, else off) and checks 2-4 of `d1:358-365`. Fix these lines: <br>• `d1:232`: say "**human** user ids", and make the empty-list default apply only when `whoami.kind` is a personal agent. <br>• `d1:233`: `require_kind = ""` must not exist; agent comments never steer, so only the value is configurable (M2). <br>• `d1:369-372`: signing in as Max Claude with explicit `owners` is the intended setup; delete "(or Relay fills `owner_id`, G-R3)", because product agents have no owner by design. <br>• `d1:131-133`, `d1:444-445`, R1 step 4 `d1:666`: delete G-R3 (already shipped for personal agents, and not wanted for product agents). <br>• `d1:334`: the chip "max-claude (agent of Max Falk)" is wrong → "relay · Max Claude (agent) · steering: Max Falk", or "relay · Max Falk's agent · steering: Max Falk (owner)". <br>• Doctor note at `d1:555-556`: with no owner list and no whoami owner, say "set `[tickets.remote] owners`". <br>• Close open question 1 `d1:746-748`; Max Claude is not, and should not be, an owned agent |
| Q2 | A ticket-linked personal conversation, if the code supports a link | **No ticket link today**, no MCP surface, and personal sessions need a personal agent, which a Max Claude connection is not (H1) | Replace `d1:434-442` and R1 `d1:646-676` with H1's plan. `d1:129-130` G-R2 becomes "a ticket-linked conversation for any agent connection, built from the personal-session machinery, with an MCP surface". `d1:82` "Not a generic MCP surface" is right; keep it |
| Q3 | Owner only by default | Personal conversations are owner-private already (§1) | `d1:277` ("anyone who can read the ticket…"), `d1:389-390`, `d1:441-442` ("the scope comment wants others to see the thread") → owner only. Remove `target = "comments"` (`d1:285-287`): it would publish the conversation to every ticket reader. Close open question 3 |
| Q4 | Steering and `/approve` from the Conversation only | Owner-only server check on messages (§1) makes this the strongest channel | Remove `steer = "comments"` and `steer_prefix` (`d1:234-235`, `d1:253`), Option A's "on the plan's own ticket" (`d1:206-216`), and Option B (`d1:218-222`, X3 step 3). Post the approval request into the Conversation, not as a main comment (`d1:195-201`). Close open question 4 |
| Q5 | (asked of this review) | No match; see §1 and H5 | `d1:77`, `d1:327-329`, open question 5 |
| Q6 | Phone approval of permissions and merges is phase 2 | — | Already consistent (`d1:224-225`); close open question 6 |
| Q7 | No extra MFA for plan approval in v1; irreversible actions stay desktop-only | Approval → `approve_plan` → `plan` event; dispatch pushes nothing (`crates/atomics/src/app/plans.rs:266-273`) | Accept for **approval**. Steering is broader than approval (M4): `d1:394-395` and §4.3 must list the desktop-only actions and add the chief rule. Close open question 7 |
| Q8 | Target Flat (relay-spatial `next/`) | Flat is what relay.atomisk.ai serves (GO `products.json:813,837`), but it has no PWA (H2) | `d1:83` (relay-frontend "is the deployed app") and `d1:84` → as H2; `d1:479-480` "adopt foundation-frontend's push lifecycle" → port RF's files. Close open question 8 |
| Q9 | Adaptive polling, 5 s active, 60 s otherwise | `d1:236-237` already says this; no MCP rate limit exists, and a `get_ticket` poll is heavy (M9) | Define "active" (M9); forbid the newest-N `get_ticket` fallback at 5 s (`d1:244-246`). Close open question 9 |

---

## 3. Findings, by severity

### High (block the implementation tasks)

**H1. The personal conversation (Q2) needs a ticket link and an MCP surface; d1 plans neither.**
- What exists covers most of the remote:
  - owner-only send and read, with no impersonation (§1);
  - queued and delivered acknowledgements (`AgentRunnersController.cs:524-557`);
  - attention pushes from activity states (`AgentRunnerControlPlane.cs:2366-2377`) that map onto ATOMICS's
    "needs you": permission prompt → `WaitingForApproval`, plan approval or question → `WaitingForInput`,
    blocked → `Failed`;
  - Completed and Progress pushes with generic copy (`PersonalAgentUpdateRecordedEventConsumer.cs:33-38`).
- What is missing:
  1. **Ticket link.** `AttachPersonalAgentSessionRequest` is `{SessionId, Title, lease}` only (FT
     `Foundation.Contracts/AgentRunnerProtocol.cs:618-621`). The work record points at the session and at
     `/agents/sessions?…` (`AgentRunnerControlPlane.cs:1151-1159`). Add an optional linked item, checked as
     readable by the owner, and expose it to the owner only: on `get_ticket`, and as the deep link of its
     notifications.
  2. **Any agent connection, not only personal agents.** A personal session requires a personal agent
     identity whose owner approved the runner (`PersonalAgentSessionsController.cs:178-179`). Its requester
     is that owner (`AgentRunnerControlPlane.cs:1152-1153`). Its pushes go to that owner and require a
     personal agent (`AgentRunnerControlPlane.cs:1802-1804,2375-2376`;
     `PersonalAgentUpdateRecordedEventConsumer.cs:26-27`). Under Max's Q1 clarification, the chief connects
     as Max Claude, a product agent with no owner, so it **can't own a personal session**. R1 must add a
     ticket-linked conversation that:
     - any agent connection can open;
     - reuses the session machinery: events, steering, acknowledgements, and activity-driven pushes;
     - names its **steering humans** when it opens: ATOMICS's owner list, each checked server-side as an
       active Human who can read the ticket;
     - is readable and steerable only by those humans, and pushes only to them.

     The operational-session rule that anyone with ManageAgents on the node may read and steer
     (`AgentIdentitiesController.cs:1028-1037`) must not apply, or Q3's owner-only default fails. Letting the
     steered agent name its steerers is safe: authority stays local (`d1:53-55`), and the server only narrows
     who can write.
  3. **MCP surface.** Attach, events and acknowledgements use runner and lease authentication, and require a
     paired personal runner (`PersonalAgentSessionsController.cs:17-18,175-185`). ATOMICS holds only an MCP
     token, OAuth or otherwise. Speaking Relay's runner protocol would put a Relay client into ATOMICS, which
     breaks agnosticism. Add MCP tools for the connected agent instead:
     - open a conversation for a ticket, with its steering humans;
     - post events, with idempotent event ids and an activity state;
     - read steering after a cursor, each item carrying its author id and kind;
     - acknowledge steering.
  4. **Flat.** The ticket page needs the Conversation for its steering humans. Today the session list hides
     personal sessions (RS `relay-api/src/lib.rs:419`), and the new kind must be shown.
  5. **Size.** An event summary is ≤ 2000 characters (`AgentRunnerProtocol.cs:408`), not d1's 4000 (`d1:280`).
- **Fix:**
  - ATOMICS gets generic operations: `conversation_open` (`{uuid}`, `{owners}`), `conversation_post`
    (`{nonce}`, `{role}`, `{body}`, `{activity}`), `conversation_read` (`{after}`) and `conversation_ack`.
  - Its verdict function still requires author ∈ owners, kind Human and author ≠ `whoami.id`, as defence in
    depth.
  - R1 (kernel, ATOM ticket): the generalized conversation, its ticket link and the MCP tools.
  - Drop `threadKey`/`role` on kernel comments and `list_ticket_comments` (`d1:434-441`, R1 steps 1-3).

**H2. Flat has no manifest, no service worker and no push, and it retires the old worker.**
- `next/web/sw.js:1-12` unregisters itself and deletes the old shell cache. `bootstrap.js:8-20` triggers that
  on existing installs. `relay-flat/index.html` has no manifest link.
- relay-frontend's push stack now runs only at relay-v1.atomisk.ai (GO `products.json:824-837`). Since
  unregistering a worker drops its push subscription, phones on relay.atomisk.ai receive nothing today.
- d1 treats Flat's push as "unknown", to be confirmed in R3 step 1 (`d1:84`, `d1:149-150`, `d1:479-480`,
  `d1:702-703`).
- **Fix:** make R3 step 1 a build:
  - a manifest;
  - a push-only worker that replaces the retiring `sw.js` at the same path, with `push` and
    `notificationclick` handlers and still no `fetch` handler;
  - registration with antiforgery (`WebPushController.cs:38`);
  - a tap-gated permission flow;
  - routes for `/notifications?…&personalSession=…` and the ticket deep link.
- Port from RF `src/platform/pwa/webPush.ts` (52 lines), `registerServiceWorker.ts`, `productManifest.ts` and
  `scripts/service-worker.template.js`.
- The ticket asks for an offline shell. Flat removed the shell on purpose ("requests go to the current
  deployment", `sw.js:1-3`), so ask Max before adding a caching worker back.

**H3. Attention comments can't notify, and mention pushes leak text.**
- MCP comments pass no explicit mentions (`TicketIntegrationOperations.cs:529-530`), and the tool has no
  mentions parameter (`TicketMcpTools.cs:149-168`). So `@owner-handle` (`d1:200`), `notify_mention`
  (`d1:316-319`) and §5.1's mention-based rows (`d1:409-411`) never fire.
- Where mentions do fire (browser comments), the push shows up to 300 characters of the comment
  (`ObjectCommentSubmittedEventConsumer.cs:151,172-174`; `WebPushSender.cs:37-41`). That contradicts "lock
  screens never show conversation text" (`d1:387-388`).
- **Fix:** delete attention comments and `notify_mention`. Approval needed, needs you and blocked become
  activity states on the conversation (H1), which push generic copy; plan finished becomes a Completed
  event.
- R2 keeps ticket rules only for CI failed and deployed, with generic titles and **no** message body.
- If a mention path is ever added, the push body must be empty.

**H4. The doc still specifies what Q3 and Q4 rejected, and two Q1 details are wrong.** See the table in §2
for each line. The biggest items:
- main-comment steering and approval (`d1:206-256`), which Q4 removes;
- a conversation readable by every ticket reader (`d1:277`, `d1:389-390`, `d1:441-442`), which Q3 forbids;
- Q1: `require_kind = ""` turns the Human check off (`d1:233`, doctor `d1:555`), but agent comments must
  never steer;
- Q1: Relay is expected to give Max Claude an owner (G-R3, `d1:131-133`, `d1:371`, `d1:444-445`, `d1:666`),
  but product agents have no owner by design, and personal agents already report one.

d1's owner-list rule itself (`d1:343-347`, `d1:358-365`) already matches Max's Q1 clarification.

**Fix:** revise into d2. The ATOMICS security model in §4 shrinks: checks 2-6 of `d1:356-367` stay as defence
in depth on `conversation_read` items, and the comment-mode rules go. State Max's Q1 rule in its own words in
§4.1: ATOMICS's connection identity (any agent) is separate from the steering humans (the owner list).

**H5. Relay's GitHub linker won't show ATOMICS's PRs or CI, so ticket acceptance step 5 fails.** See §1 (Q5).
ATOMICS's integration branch is `atomics/plan-<id>` (`crates/atomics-core/src/plan.rs:513-521`), and its repo
isn't connected.
- **Fix, ATOMICS side (generic):**
  - the Relay profile sets `[tickets] branch = "relay/{key_number}-{slug}"`, using the existing template
    support (`crates/atomics-core/src/tickets.rs:597-622`);
  - a ticket-mode plan's integration branch follows that template, so `relay/11972-slug` and task branches
    `relay/11972-slug-d1` match the regex.
- **Fix, Relay side:** add `xmagit/ATOMICS` and `atomisk-ai/relay-spatial` to
  `IntegrationWeb__GitHubWebhooks__Repositories` (GO `products.json:493-501`). That is a gitops change, so it
  needs Max's go, and the GitHub app must be installed on `xmagit`.
- Then `record_stage` (X5) is needed only for repositories outside the app; keep it optional.

### Medium

**M1. R1 is aimed at the wrong repository.**
- The ticket MCP tools (`get_ticket`, `add_ticket_comment`, `whoami`) are relay-backend code
  (`TicketMcpTools.cs:11-391`), and so is the `@agent` path (`TicketIntegrationOperations.cs:504-600`). d1
  cites them as foundation-template#267 (`d1:74`, `d1:651-656`).
- Personal sessions are kernel code (§1).
- **Fix:**
  - R1 (kernel, ATOM ticket, `atom/` branch): the ticket link on personal sessions and the conversation MCP
    tools.
  - R2 (relay-backend): adopt R1, show the owner's linked conversation on `get_ticket`, and add the two
    ticket push rules.
  - Still one PR per repository.

**M2. Author-kind values differ in case and vocabulary.** Comments report `Human`/`Agent`/`System` (live
`get_ticket`; `UserIdentity.cs:6-11`). `whoami` reports `human`/`personalAgent`/`agent`
(`TicketIntegrationOperations.cs:1590`). d1 configures `require_kind = "human"` (`d1:233`) but expects
`"Human"` (`d1:363`). An exact compare fails closed: nothing ever steers. A quick fix to that tends to disable
the check, and d1 even offers `""` to disable it, which Max's rule ("comments from any agent identity never
steer") forbids. **Fix:**
- compare case-insensitively, or map values like `[tickets.states]`;
- an unmapped kind path disables remote steering rather than the check, and the doctor says why;
- test both casings in X2's table, and test that a personal agent's comment (kind `Agent`) never steers,
  even when its owner is on the list.

**M3. The plan digest doesn't cover what local approval covers.**
- `Plan::replace` keeps approval only when the goal and each task's id, title, brief, acceptance, deps, agent,
  args, isolation and repo are unchanged (`crates/atomics-core/src/plan.rs:425-445`).
- d1 hashes only ids, titles, deps and agents (`d1:203-204`, X3 step 1). So a rewritten brief, repo or args
  asks for approval again under the **same** digest.
- An `/approve` sent from a stale screen after the new request then approves work the owner never saw.
- **Fix:**
  - hash the goal plus exactly the `specification` fields of `Plan::replace`;
  - use 12 hex characters;
  - show each task's brief opening in the confirmation sheet;
  - X3 tests that changing a brief changes the digest.

**M4. Steering is more powerful than approval, and Q7's reasoning covers approval only.**
- A delivered steering message is the user's words. The chief then acts with its full local permissions,
  which for Max include pushing branches, updating PRs, creating tickets and dispatching releases. Its Relay
  connection is Max Claude, whose permissions are broad and sysadmin-like. So one phone message reaches
  more than a personal agent could.
- `Human` kind proves a human **credential**, not a keystroke. OAuth person mode (off: `McpOAuthOptions.cs:21-25`,
  unset in GO `products.json:502-505`) or any human-owned API credential would write as the owner. That matters
  less once steering comes only from the owner's session (H1), but it belongs in §4.4.
- **Fix:**
  - in §4.3, list the actions that stay desktop-only: merges, releases, deploys, cluster mutations, secrets,
    deletions;
  - add the chief rule "a message *via relay* may steer and ask, but those actions need a local
    confirmation";
  - keep person mode off for remote use;
  - X2 tests that the label is present on every delivered message.

**M5. X1 overlaps the running RELAY-11955 a1.**
- a1 (uncommitted, worktree `…relay-11955-a1-…`) already does all of this:
  - adds `{nonce}` to `PLACEHOLDERS`;
  - rejects `{nonce}` in read operations;
  - keeps a state push's nonce across heartbeat and sync retries;
  - retries a transport error once with the same nonce;
  - maps Relay `update_state` and `comment` with `idempotencyKey = "{nonce}"` (example profile in
    `config.rs`);
  - tests deduplication in both fake-server dialects.
- That is X1 steps 1 (nonce), 5 (`update_state`, `comment`) and 6 (`d1:512-522`).
- **Fix:** X1 depends on a1 merged. Keep only the `after`/`role`/`activity` placeholders, the `whoami`
  operation, `CommentPaths` identity fields, and the outbox. Drop the `create` mapping unless a task needs it.

**M6. Restart replay and outbox placement.**
- d1 saves the cursor in the plan file but doesn't say whether it is saved before or after the paste
  (`d1:247`). A crash in between either repeats or loses a message.
- **Fix:** adopt ATOM-10680's rule ("persist delivery intent before sending; ambiguous sends warn instead of
  executing twice"):
  - record the message id as *delivering* before `send_to_chief`;
  - on restart, show "may not have reached the chief" instead of pasting again;
  - acknowledge to the server only after `Sent`.
- Also, `plan_json` sends the whole plan to the chief (`crates/atomics/src/app/plans.rs:351-354`). Keep
  `outbox`, `own_comments` and cursors out of it, with `serde(skip)` or a sidecar file.

**M7. Agent-writer comments containing `@alias` are refused.** If any mirror or ATOMICS text still goes to
main comments, a chief turn that contains a Relay agent alias (`@max-agent`, say) is rejected
(`TicketIntegrationOperations.cs:516,567-572`). The outbox then retries it ten times and drops it. **Fix:** with
H1 this goes away. If any comment target remains, escape `@` in mirrored text.

**M8. A sequence cursor can skip rows.** The comment sequence is the row id (`ObjectCommentQueries.cs:70-98`).
It is assigned at insert and visible at commit, so a lower id that commits late is skipped by `seq > after`.
**Fix:** any server cursor R1 adds must be commit-ordered, or ATOMICS re-reads a short overlap and
deduplicates by id.

**M9. The polling cost is unbounded on the server.**
- A fallback poll is a full `get_ticket`: the description, up to 200 comments, and a search-index child count
  (`TicketIntegrationOperations.cs:1548-1567`). MCP calls have no rate limit (§1).
- **Fix:**
  - only a cursor operation may poll at 5 s, and the newest-N fallback polls at 60 s;
  - "active" means a chief turn or a remote message within 10 minutes, or a plan waiting for approval;
  - the 5 s cadence stops when the plan's chief session ends.

### Low

- **L1. Citations to fix in §1.1:**
  - `@agent` lives in relay-backend, not the kernel;
  - `commentsTake` defaults to 100 with a maximum of 200 (`d1:246` says 20);
  - "Codex only" (`d1:81`) applies to the bridge; the session protocol is runtime-neutral;
  - the source caveat `d1:21-25` can go, since this review supplies the file:line.
- **L2. Approve button contract.** Flat would parse ATOMICS's text to find the digest (`d1:466-468`).
  Document the approval event's format as part of the generic ATOMICS contract, or carry the digest in a
  structured event field.
- **L3. Test runs.** Full suites go through `atomics queue --class test` (`d1:487-488`, X6 `d1:726`).
- **L4. CI budget.** R3's phone-width browser journeys (`d1:709-711`) run locally, not in CI. CI keeps
  `check-fast`, the hosting tests and `verify-theme`.

### Agnosticism

ATOMICS code has no Relay names today. The word "relay" in Rust is the sandbox network relay
(`crates/atomics-core/src/isolation.rs`); the rest is the example profile (`config.rs:120-165`) and test
fixtures (`selfconfig.rs:967,1004`). a1 keeps Relay tool names in tests and the example profile only.

The design holds to that as long as:
- H1 is done through generic MCP operations, never through Relay's runner HTTP protocol;
- `require_kind` is mapped, not hard-coded (M2);
- the approval text format is ATOMICS's own contract (L2).

---

## 4. What the d2 task list should look like

| Task | Repo | Change from d1 |
| --- | --- | --- |
| X1 | ATOMICS | After a1: placeholders, `whoami` (`id`, `kind`, `owner_id` mapped; the owner is present only for personal agents), `CommentPaths` identity fields, outbox outside `plan_json` |
| X2 | ATOMICS | `[tickets.remote]` with conversation steering only. `owners` = human user ids; empty means whoami's owner only when the connection is a personal agent. Verdict function: author ∈ owners, kind Human (mapped, never optional), author ≠ `whoami.id`, own ids, cursor. Delivery intent before paste. Doctor: remote off with "set owners" when the list is empty and whoami has no owner (Max Claude); refuses owners that include `whoami.id` or a human connection (`whoami.kind == human`) |
| X3 | ATOMICS | Digest over the `Plan::replace` fields, 12 hex characters; approval request and `/approve` inside the Conversation; no state approval |
| X4 | ATOMICS | `conversation_*` operations; turns and tool blocks ≤ 2000 characters per event; activity states (needs you, waiting for approval, blocked) |
| X5 | ATOMICS | Relay profile `branch` template and ticket-mode integration branch (H5); optional `record_stage`; no attention comments |
| R1 | foundation-template (ATOM ticket) | Ticket-linked conversation for any agent connection, built from the personal-session machinery: named steering humans (Human, ticket readers), visible and steerable only by them, pushes to them; MCP tools to open, post, read and acknowledge |
| R2 | relay-backend | Adopt R1; the linked conversation on `get_ticket` for its steering humans; push rules for CI failed and deployed (generic, no body) |
| R3 | relay-spatial | PWA build (H2); the ticket's Conversation in Flat; Approve sheet; deep links |
| X6 | both | Device acceptance; webhook repositories added (gitops, Max's go) |

Wave A can still be X1 + R1, once Max approves d2.

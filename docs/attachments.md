# Files for the chief

Drop files anywhere on the window, or choose **Attach files**. The overlay says **Drop to send to the
chief**. ATOMICS reads and hashes the selected bytes, then shows a file list with **Send to chief** and
**Cancel**. Nothing reaches the chief before Send. The recipient is always the chief, regardless of the
session selected in the steer bar. Upload progress, success and refusals appear in the same dialog.

The full-mode chief trace shows each attachment's name, size, SHA-256 and host path. Simple-mode
composers use `atomics_ui::attachments::attach` with `Client::attachments`, once per frame, and can offer
the same picker with `attachments::button`. Desktop, native-client and browser backends share this flow.
Viewers cannot attach. The permission is the authenticated controller's session-message grant; a terminal
lease is unnecessary. It is checked on every attachment request, including begin, chunks and commit.

## Transport and integrity

A local desktop snapshots regular files on a worker, opening without following the final symlink and
rejecting directories and devices. The client hashes the snapshot with SHA-256. The host writes a fresh
private folder beneath `ATOMICS_HOME/attachments`, checks the size and hash, then delivers the file.
Duplicate names get separate file directories; existing files are never overwritten. Names are limited
to 120 ASCII letters, digits, spaces, dots, underscores and hyphens, with leading/trailing dots and spaces
removed. Other characters, including slashes, backslashes and control characters, become underscores.

For a VM client, the writable `~/atomics-exchange` share carries the snapshot in
`.atomics-attachments/<host-generated-id>/<file-number>`. The VM host gets the exchange location from its
user/host configuration, or `ATOMICS_EXCHANGE` exported by the VM's root-owned mount setup. No attachment
request can supply a source or destination path. The host walks the transfer directory with no-follow
descriptor-relative opens, copies into its own private storage and hashes the bytes it copied. A modified
exchange copy is refused, with no chief delivery. The exchange files are removed after ingestion.

Without a share, `atomics.v1` negotiates the `attachments` capability. The separate `attachment` message
has `limits`, `begin`, `chunk`, `commit` and `cancel` actions, answered by `attachment_reply`. Chunks contain
at most 64 KiB of decoded bytes, with an exact file index and offset. The client waits for each reply before
sending the next chunk. The host streams to disk, reserves the declared total against the staging budget,
checks the actual count/size/hash, and binds the transfer to its authenticated connection. Reconnect never
replays an attachment. Cancellation, disconnection, refusal and expiry remove unfinished host copies.

## Limits and retention

Only the user's main or host configuration can set these values. Repository and workspace configuration
cannot change them. Defaults are:

```toml
[attachments]
max_file_bytes = 52428800       # 50 MiB
max_files = 10
max_drop_bytes = 524288000      # 500 MiB
max_staging_bytes = 1073741824  # 1 GiB, across unfinished transfers
idle_secs = 120
timeout_secs = 600
# exchange = "~/atomics-exchange"
```

There is one unfinished drop per connection. The protocol's existing frame limit applies too.
Verified files remain with their trace archive. Trace rotation removes files whose attachment entries
have expired; `State::remove_plan_trace`, the existing plan deletion/cleanup hook, removes that plan's
attachments. Chief-only attachments follow rotation of `trace/chief.jsonl`.

## Trust boundary

MCP, control requests, hooks, typed messages and agent output have no attachment operation and cannot
choose a file path or mint an attachment trace entry. Browser input comes from trusted drop/picker
events, not `postMessage` or synthetic file-selection events. The host frames delivery as ATOMICS user
attachments with name, size, SHA-256, host path, and an explicit statement that the contents are user-supplied
data. The chief decides whether to copy a file into a task's worktree.

Processes of the same OS user are one trust domain, as for the agent channel. Bare agents can reach that
user's files and credentials; sandboxed agents cannot reach the host's private attachment directory.
Private modes (0700 directories, 0600 files) protect against other users, not unrestricted same-user
processes. The frame tells the chief to **check each SHA-256 before relying on the file**, because another
same-user process could change it after host verification. An authenticated custom client can speak the
protocol directly; the server cannot attest a physical gesture on another device.

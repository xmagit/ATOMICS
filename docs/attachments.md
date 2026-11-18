# Files for the chief

Drop files anywhere on the window, or choose **Attach files**. The overlay says **Drop to send to the
chief**. ATOMICS reads and hashes the selected bytes, then shows a file list with **Send to chief** and
**Cancel**. Nothing reaches the chief before Send. The recipient is always the chief, regardless of the
session selected in the steer bar. The dialog reports upload progress and refusals. A held or starting
chief shows a waiting status. The host records successful delivery in the provider conversation after
confirmation.

The full-mode chief trace shows each attachment's name, size, SHA-256 and host path. Simple-mode
composers use `atomics_ui::attachments::attach` with `Client::attachments`, once per frame, and can offer
the same picker with `attachments::button`. Desktop, native-client and browser backends share this flow.
Viewers cannot attach. The permission is the authenticated controller's session-message grant; a terminal
lease is unnecessary. It is checked on every attachment request, including begin, chunks and commit.

## Transport and integrity

The native picker and drop handler open each selected file once, refuse a final symlink, and check the
open descriptor for a regular file. A worker reads that descriptor within the host limits. The preview,
SHA-256 and upload all describe the same snapshot. The in-process desktop passes its snapshot directly
to the host writer. The host writes a fresh private folder beneath `ATOMICS_HOME/attachments`, checks
the size and hash, then delivers the file.
Duplicate names get separate file directories; existing files are never overwritten. Names are limited
to 120 ASCII letters, digits, spaces, dots, underscores and hyphens, with leading/trailing dots and spaces
removed. Other characters, including slashes, backslashes and control characters, become underscores.

For a VM client, the writable `~/atomics-exchange` share carries the snapshot in
`.atomics-attachments/<host-generated-id>/<file-number>`. The VM host gets the exchange location from its
user/host configuration, or `ATOMICS_EXCHANGE` exported by the VM's root-owned mount setup. No attachment
request can supply a source or destination path. The host walks the transfer directory with no-follow
descriptor-relative opens, copies into its own private storage and hashes the bytes it copied. A modified
exchange copy is refused, with no chief delivery. The client holds a cleanup guard until ingestion
completes or the upload ends. The host retains the exchange directory opened at begin, removes its
staging on completion or cancellation, and sweeps abandoned transfers in its namespace at startup
and on a timer.

The separate `--connect local` process connects to the host’s advertised TCP WebSocket. That transport
cannot pass Unix file descriptors. It sends acknowledged chunks of the already-opened file’s snapshot.
Accepting a pathname through this connection would let a controller request reads from the host.

Without a share, `atomics.v1` negotiates the `attachments` capability. The separate `attachment` message
has `limits`, `begin`, `chunk`, `commit`, `status` and `cancel` actions, answered by `attachment_reply`. Chunks contain
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
max_retained_bytes = 2147483648 # 2 GiB of retained files and reservations
min_free_bytes = 1073741824     # leave 1 GiB free on the host volume
idle_secs = 120
timeout_secs = 600
# exchange = "~/atomics-exchange"
```

Once the host submits a frame to a terminal, Cancel reports “Already sent to the chief”. Status continues waiting for the provider’s matching submission. The host saves the conversation receipt before Status reports success.
A queued commit keeps its delivery ID. The sender checks its outcome with `status`. A failed hook write puts the same delivery back in the conversation’s queue.

One process owns attachment storage per home. Each file reserves its payload size plus 16 KiB for directories and metadata, including empty files. Retained usage includes this charge after restart. There is one unfinished drop per connection. The protocol's existing frame limit applies too.
Once the provider has received a drop, the host records the chief conversation and each file’s trace order before telling the sender it was sent. After a restart, the host uses the verified manifest to restore any missing trace rows. A sender who cancels after submission continues to see delivery updates until the provider confirms or the host times out.

The sender’s active plan at Send determines retention. An unscoped drop belongs to the chief history.
The host removes interrupted private staging before reusing its budget at startup. If a crash interrupts delivery after the host accepts a verified drop, recovery keeps the files because the provider may already have their paths. The host inventory includes these files for manual deletion. Each verified file
has its own id and follows its individual trace entry. Trace rotation removes files whose attachment entries
have expired; `State::remove_plan_trace`, the existing plan deletion/cleanup hook, removes that plan's
attachments. Chief-only attachments follow rotation of `trace/special/chief.jsonl`.

### Manage retained files

On the host, list retained files and the volume’s available bytes with the local control socket.

```sh
atomics ctl '{"cmd":"attachment_storage"}'
```

Each entry includes its id, name, size, plan and host path. Each page contains up to 100 files.
Pass the returned `next` value as `after` to read the next page. To delete one file, use its id.

```sh
atomics ctl '{"cmd":"attachment_delete","id":"<id from attachment_storage>"}'
```

Deletion releases the file’s storage and removes it from chief lookups. The trace keeps the historical
receipt until rotation. The host checks retained bytes and outstanding reservations before accepting a
drop, and checks the free-space floor while writing. When space is scarce, the controller gets a refusal
and the owner chooses which retained files to delete.

## Trust boundary

The chief can look up a receipt for its current provider conversation by id through the read-only `atomics_attachments` MCP tool. The host
returns the verified name, byte count, SHA-256, path, and verification time in Unix milliseconds.
Only the host writes these records. The chief checks all fields against this independent record before
using an attachment. Ordinary messages, hooks and tool output can contain the same frame text.
Task agents have no attachment MCP tool. Control requests and MCP provide no upload operation.
The browser accepts trusted drop and picker events. The host frames delivery as ATOMICS user
attachments with name, size, SHA-256, host path, and an explicit statement that the contents are user-supplied
data. The chief decides whether to copy a file into a task's worktree.

Processes of the same OS user are one trust domain, as for the agent channel. Bare agents can reach that
user's files and credentials; sandboxed agents cannot reach the host's private attachment directory.
Private modes (0700 directories, 0600 files) protect against other users, not unrestricted same-user
processes. The frame tells the chief to **check each SHA-256 before relying on the file**, because another
same-user process could change it after host verification. An authenticated controller represents the
user. A custom controller can upload through the protocol without a GUI gesture.

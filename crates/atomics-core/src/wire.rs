//! The `atomics.v1` client protocol: JSON text frames, one `Envelope` per frame.
//!
//! Browser-safe: serde types and pure functions only, so the same code builds the host's server
//! and the wasm client. Ordering and tags follow `docs/daemon.md` (Client protocol). This module
//! carries the board (sessions, plans, diffs, approvals, cursors), terminal screens (`screen`
//! module), typing into terminals under a controller lease (`lease` module) and the journaled
//! mutations (`ops` module).
//!
//! Rules every decoder here applies:
//! - A frame over `MAX_FRAME_BYTES` is rejected before it is parsed.
//! - A major version other than ours is rejected before the body is read (except `hello`, whose
//!   `versions` list is negotiated). Minor versions are additive: unknown fields are ignored.
//! - An unknown `kind` fails explicitly; it is never ignored.
//! - Cursors and revisions are decimal strings, so browser number precision cannot corrupt them.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

use crate::diff::{FileDiff, Hunk};
use crate::lease::{self, GridSize, LeaseState};
pub use crate::ops::Grant;
use crate::ops::{ApprovalDecide, CommandResult, HostCommand, OP_TTL_SECS, PlanCommand, ReviewCommand, SessionCommand};
use crate::projection::{ApprovalInfo, DiffFileBody, DiffSummary, HostInfo, Loaded, PlanInfo, ReportInfo, SessionInfo};
pub use crate::screen::Part;
use crate::screen::{self, HistoryChunk, HistoryGet, ScreenDelta, ScreenSnapshot};

/// The WebSocket subprotocol name.
pub const SUBPROTOCOL: &str = "atomics.v1";
pub const VERSION: Version = Version { major: 1, minor: 0 };
/// Frames are capped before encryption; larger payloads are chunked by the transfer layer.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_SUBSCRIPTIONS: usize = 256;
pub const MAX_PENDING_OPS: usize = 64;
pub const MAX_QUEUED_FRAMES: usize = 256;
pub const MAX_VERSIONS: usize = 16;
pub const MAX_CAPABILITIES: usize = 64;
/// The longest identifier, path or request ID accepted in a frame field.
pub const MAX_ID_BYTES: usize = 1024;

/// Capabilities this implementation can offer in a `welcome`.
pub const CAPABILITIES: &[&str] = &[
    "board",
    "plans",
    "approvals",
    "diffs",
    TERMINALS,
    INPUT,
    MUTATIONS,
    HOST,
    PANELS,
    EXPLORER,
    PLAN_COMPLETE,
    TRACE,
    FIT,
    TUNNEL,
    MACHINE,
];
/// Projects, worktrees, folder subscriptions/pages and bounded whole-file reads. Needs `host`.
pub const EXPLORER: &str = "explorer";
/// Completing a plan through the mutation journal.
pub const PLAN_COMPLETE: &str = "plan_complete";
/// Screen streaming: `screen` topics, `screen_snapshot`/`screen_delta` and `history_get`.
pub const TERMINALS: &str = "terminals";
/// Typing into subscribed screens: the `lease_*` messages, `lease_state`, `terminal_input` and
/// `terminal_resize`. Needs `terminals`.
pub const INPUT: &str = "input";
/// Changing things: `approval_decide`, `plan_command`, `review_command` and `session_command`,
/// each answered by `command_result`. What a client may send is its `welcome`'s `may`.
pub const MUTATIONS: &str = "mutations";
/// What the host says about itself (`host_upsert`: the open workspace, ticket sign-in, account usage and governor load) and,
/// with `mutations`, `host_command`. A client without it is sent `skipped` in place of `host_upsert`;
/// every `state_snapshot` carries `host` either way, which an older client ignores.
pub const HOST: &str = "host";
/// Host editor PTYs, pull-request diffs and chief lifecycle commands. Requires `host`.
pub const PANELS: &str = "panels";
/// Paged chief trace and live revision notifications (`trace` subscription).
pub const TRACE: &str = "trace";
/// `terminal_fit`: the host has no desktop of its own (`atomics host`), so a client's pane gives a terminal its own
/// size. Only such a host offers it, and only with `input`.
pub const FIT: &str = "fit";
/// Bounded TCP previews, offered only on native remote connections.
pub const TUNNEL: &str = "tunnel";
/// Host machine snapshots, only for controllers with the host grant.
pub const MACHINE: &str = "machine";
pub const MAX_TUNNELS: usize = 16;
pub const TUNNEL_WINDOW: u32 = 64 * 1024;
pub const TUNNEL_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

/// An opaque, ordered 64-bit counter that travels as a decimal string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

/// A position in the host's event stream (per boot epoch).
pub type Cursor = Seq;
/// A per-collection or per-entity content revision.
pub type Revision = Seq;

impl Serialize for Seq {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<str>>::deserialize(d)?;
        if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(de::Error::custom("a cursor is a decimal string"));
        }
        text.parse().map(Seq).map_err(|_| de::Error::custom("cursor out of range"))
    }
}

/// Stable error codes. The client maps these to messages and to retry or re-pair actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Malformed,
    Oversize,
    UnsupportedVersion,
    UnknownKind,
    TooMany,
    NotFound,
    StaleRevision,
    Unauthorised,
    SlowClient,
    Internal,
    /// Another client holds the terminal's lease; acquiring it needs `takeover`.
    LeaseHeld,
    /// The lease named is not the current one (expired, released, taken over, ended with its
    /// connection or taken back on the desktop). The input or resize was not applied.
    Fenced,
    /// Input whose sequence number is not the next one in its lease. It was not applied.
    OutOfOrder,
    /// The host could not carry out the command; the message says why. Nothing changed.
    Rejected,
    /// The approval was already answered (by another client, the desktop or an earlier
    /// operation); the message says how. This decision changed nothing.
    AlreadyResolved,
    /// The operation ID was used before for a different command.
    Conflict,
    /// The host does not hold this device's key: it never paired there, or it was revoked. Sent
    /// only inside the encrypted channel ([`not_paired`]), so it is the one sign a browser forgets
    /// its key on.
    NotPaired,
    /// A code from a newer peer. Minor versions may add codes; an older client still reads the frame.
    #[serde(other)]
    Unknown,
}

/// Why a frame was refused. Convert with `into_envelope` to answer the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl WireError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn into_envelope(self, request_id: Option<String>, epoch: Option<String>) -> Envelope {
        Envelope { v: VERSION, epoch, request_id, message: Message::Error { code: self.code, message: self.message } }
    }
}

/// The host's refusal of a device whose key is not on its allowlist: a session's first frame
/// from that device, or a revocation during its session. The host seals it into the encrypted
/// channel and then closes the channel with `tunnel::close::NOT_PAIRED`. Only the host can seal
/// it, so a browser forgets its key on this frame alone; the bare close code arrives through the
/// broker unauthenticated, and a client keeps its key on it. An older client reads the code as
/// `unknown` and goes by the close code, as before.
pub fn not_paired() -> Envelope {
    WireError::new(ErrorCode::NotPaired, "this device is not paired with this host; pair it again from the host")
        .into_envelope(None, None)
}

/// Whether `envelope` is the host's [`not_paired`] refusal.
pub fn is_not_paired(envelope: &Envelope) -> bool {
    matches!(envelope.message, Message::Error { code: ErrorCode::NotPaired, .. })
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_frame_bytes: u32,
    pub max_subscriptions: u32,
    pub max_pending_ops: u32,
    pub max_queued_frames: u32,
    /// How long the host remembers an operation's outcome; see `ops::Pending`.
    #[serde(default)]
    pub op_ttl_secs: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES as u32,
            max_subscriptions: MAX_SUBSCRIPTIONS as u32,
            max_pending_ops: MAX_PENDING_OPS as u32,
            max_queued_frames: MAX_QUEUED_FRAMES as u32,
            op_ttl_secs: OP_TTL_SECS,
        }
    }
}

/// How far the host trusts a client: what a device's entry in the allowlist says once pairing
/// ships, and on loopback (whose clients hold the bearer token) a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// May type into terminals, answer approvals and send every command.
    Controller,
    /// Follows everything, including who types, but changes nothing: no lease, no mutation.
    Viewer,
}

impl Role {
    /// What the role allows, before capabilities narrow it.
    pub fn grants(self) -> &'static [Grant] {
        match self {
            Role::Controller => &[Grant::Type, Grant::Approve, Grant::Session, Grant::Plan, Grant::Review, Grant::Host],
            Role::Viewer => &[],
        }
    }

    /// What a client with this role may do over the capabilities it negotiated: typing needs
    /// `input`, `host` needs `host` and `mutations`, the rest `mutations`. This is `welcome`'s `may`.
    pub fn may(self, capabilities: &[String]) -> Vec<Grant> {
        let has = |name: &str| capabilities.iter().any(|c| c == name);
        self.grants()
            .iter()
            .copied()
            .filter(|grant| match grant {
                Grant::Type => has(INPUT),
                Grant::Host => has(HOST) && has(MUTATIONS),
                _ => has(MUTATIONS),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    NewClient,
    EpochChanged,
    CursorTooOld,
    CursorAhead,
}

/// The host's answer to "where was I": replay retained events after the cursor, or reset with a
/// `state_snapshot` at `cursor` (events after it follow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Resume {
    Replay { after: Cursor },
    Reset { cursor: Cursor, reason: ResetReason },
}

/// Decide replay or reset from what the client last applied. `oldest_retained` is the lowest
/// cursor still in the replay ring (`head + 1` when it is empty); replay needs every event after
/// `prior_cursor`, so the ring must reach back to `prior_cursor + 1`.
pub fn decide_resume(
    epoch: &str,
    head: Cursor,
    oldest_retained: Cursor,
    prior_epoch: Option<&str>,
    prior_cursor: Option<Cursor>,
) -> Resume {
    let reset = |reason| Resume::Reset { cursor: head, reason };
    let (Some(prior_epoch), Some(after)) = (prior_epoch, prior_cursor) else {
        return reset(ResetReason::NewClient);
    };
    if prior_epoch != epoch {
        return reset(ResetReason::EpochChanged);
    }
    if after > head {
        return reset(ResetReason::CursorAhead);
    }
    if after.0.saturating_add(1) < oldest_retained.0 {
        return reset(ResetReason::CursorTooOld);
    }
    Resume::Replay { after }
}

/// Pick the highest version both sides speak: the client's best minor for a major the host
/// supports, clamped to the host's minor. Fails with `unsupported_version` listing ours.
pub fn negotiate(client: &[Version], host: &[Version]) -> Result<Version, WireError> {
    host.iter()
        .filter_map(|h| {
            let c = client.iter().filter(|c| c.major == h.major).map(|c| c.minor).max()?;
            Some(Version { major: h.major, minor: c.min(h.minor) })
        })
        .max()
        .ok_or_else(|| {
            let ours: Vec<_> = host.iter().map(|v| format!("{}.{}", v.major, v.minor)).collect();
            WireError::new(ErrorCode::UnsupportedVersion, format!("this host speaks {}", ours.join(", ")))
        })
}

/// The capabilities both sides have, in the host's order.
pub fn common_capabilities(client: &[String], host: &[&str]) -> Vec<String> {
    host.iter().filter(|h| client.iter().any(|c| c == *h)).map(|h| h.to_string()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Allowed,
    AllowedAlways,
    Denied,
    Expired,
    Withdrawn,
}

impl Disposition {
    pub fn label(self) -> &'static str {
        match self {
            Disposition::Allowed => "allowed",
            Disposition::AllowedAlways => "allowed always",
            Disposition::Denied => "denied",
            Disposition::Expired => "expired",
            Disposition::Withdrawn => "withdrawn",
        }
    }
}

/// What a client may subscribe to. `diff` and `plan` topics narrow the board's live updates: a
/// client that has subscribed to no `diff` topic gets every worktree's `diff_summary`, and once it
/// has subscribed to any it gets only those worktrees' (likewise for plans). Sessions and
/// approvals are always sent, and an event left out arrives as `skipped`. A `screen` topic is a
/// visible terminal pane: it gets a snapshot, then deltas at about 200 ms, or about 100 ms while
/// `focused`. Subscribing again only updates `focused`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum Topic {
    Folder {
        worktree: String,
        path: String,
    },
    Trace,
    Machine,
    Diff {
        worktree: String,
    },
    Plan {
        id: String,
    },
    Screen {
        session: String,
        #[serde(default)]
        focused: bool,
    },
}

impl Topic {
    /// The subscription's identity; `focused` is a hint, not part of it.
    pub fn key(&self) -> String {
        match self {
            Topic::Folder { worktree, path } => format!("folder:{}:{worktree}:{path}", worktree.len()),
            Topic::Trace => "trace".into(),
            Topic::Machine => "machine".into(),
            Topic::Diff { worktree } => format!("diff:{worktree}"),
            Topic::Plan { id } => format!("plan:{id}"),
            Topic::Screen { session, .. } => format!("screen:{session}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revisions {
    pub sessions: Revision,
    pub plans: Revision,
    pub approvals: Revision,
    pub diffs: Revision,
    #[serde(default)]
    pub host: Revision,
}

/// Every message, tagged by `kind`. Fields beyond those listed are ignored on read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Machine {
        snapshot: crate::machine::Machine,
    },
    TunnelOpen {
        id: String,
        port: u16,
    },
    TunnelOpened {
        id: String,
    },
    TunnelRefused {
        id: String,
        reason: String,
    },
    /// Standard base64; decoded payload is at most TUNNEL_CHUNK bytes.
    TunnelData {
        id: String,
        bytes: String,
    },
    /// Credit returned only after bytes reach the TCP socket.
    TunnelAck {
        id: String,
        bytes: u32,
    },
    /// EOF in the sender's direction; the other direction may finish independently.
    TunnelClose {
        id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        abort: bool,
    },
    FolderGet {
        worktree: String,
        path: String,
        revision: Revision,
        offset: u32,
    },
    FolderPage(crate::folder::Page),
    Hello {
        client_id: String,
        versions: Vec<Version>,
        #[serde(default)]
        capabilities: Vec<String>,
        /// The epoch and cursor last applied, to resume; both absent for a first connection.
        #[serde(default)]
        prior_epoch: Option<String>,
        #[serde(default)]
        prior_cursor: Option<Cursor>,
    },
    Welcome {
        version: Version,
        capabilities: Vec<String>,
        limits: Limits,
        role: Role,
        resume: Resume,
        /// What this client may change; empty for a view-only one.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        may: Vec<Grant>,
    },
    Subscribe {
        topics: Vec<Topic>,
    },
    Unsubscribe {
        topics: Vec<Topic>,
    },
    StateSnapshot {
        cursor: Cursor,
        revisions: Revisions,
        sessions: Vec<SessionInfo>,
        plans: Vec<PlanInfo>,
        approvals: Vec<ApprovalInfo>,
        diffs: Vec<DiffSummary>,
        /// The host's own state; every part carries it whole.
        #[serde(default, skip_serializing_if = "is_default")]
        host: HostInfo,
        /// Set when the board does not fit in one frame, as for `screen_snapshot`: every part has
        /// the same `cursor` and `revisions` and a slice of the collections, and the client
        /// applies the snapshot once it holds all `count` parts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part: Option<Part>,
    },
    SessionUpsert {
        cursor: Cursor,
        session: SessionInfo,
    },
    SessionRemoved {
        cursor: Cursor,
        id: String,
    },
    PlanUpsert {
        cursor: Cursor,
        plan: PlanInfo,
    },
    PlanRemoved {
        cursor: Cursor,
        id: String,
    },
    /// An event outside the topics the client narrowed itself to. It stands in for that event so
    /// cursors stay consecutive, and changes nothing.
    Skipped {
        cursor: Cursor,
    },
    TaskReport {
        cursor: Cursor,
        plan: String,
        task: String,
        report: ReportInfo,
    },
    ApprovalPending {
        cursor: Cursor,
        approval: ApprovalInfo,
    },
    ApprovalResolved {
        cursor: Cursor,
        id: String,
        disposition: Disposition,
    },
    DiffSummary {
        cursor: Cursor,
        summary: DiffSummary,
    },
    /// The host's own state changed (`host` capability): the open workspace or the ticket sign-in.
    HostUpsert {
        cursor: Cursor,
        host: HostInfo,
    },
    /// Fetch a bounded page, at the revision announced by `trace_changed`.
    TraceGet {
        revision: Seq,
        offset: u32,
        limit: u32,
    },
    TracePage(crate::trace_feed::TracePage),
    /// Coalesced live notification. Subscribe again after reconnect to learn the current revision.
    TraceChanged {
        revision: Seq,
    },
    DiffGet {
        worktree: String,
        path: String,
        /// The revision the client holds, so the host can say it is stale.
        #[serde(default)]
        revision: Option<Revision>,
        /// Diff lines to skip, counted across the file's hunks: the `next_offset` of the page
        /// before. Absent for the first page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u32>,
    },
    DiffFile {
        body: DiffFileBody,
        /// This page stops short of the end of the file; ask again with `next_offset`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_offset: Option<u32>,
    },
    FileGet {
        worktree: String,
        path: String,
    },
    FileContent {
        worktree: String,
        path: String,
        content: String,
        truncated: bool,
    },
    ScreenSnapshot(ScreenSnapshot),
    ScreenDelta(ScreenDelta),
    HistoryGet(HistoryGet),
    HistoryChunk(HistoryChunk),
    /// Ask for a subscribed terminal's lease. Without `takeover` it fails while another client
    /// holds it; with it the other client's lease ends.
    LeaseAcquire {
        session: String,
        #[serde(default)]
        takeover: bool,
    },
    /// Keep a held lease alive without typing.
    LeaseRenew {
        session: String,
        lease: Seq,
    },
    LeaseRelease {
        session: String,
        lease: Seq,
    },
    /// Who holds a terminal's lease: the reply to a lease request, and sent whenever it changes to
    /// every client that negotiated `input` and follows the screen.
    LeaseState(LeaseState),
    /// Text for the terminal, encoded by the client from the screen's published modes. Applied
    /// only for the current lease and only as its next sequence number.
    TerminalInput {
        session: String,
        lease: Seq,
        seq: Seq,
        data: String,
    },
    /// Resize the terminal for the lease holder, or with no `size` return it to the host's own
    /// size. The size lasts as long as the lease.
    TerminalResize {
        session: String,
        lease: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<GridSize>,
    },
    /// Give the terminal its own size, as a desktop's pane does: the size it has whenever no lease holder's size is
    /// in force. Only with `fit`.
    TerminalFit {
        session: String,
        size: GridSize,
    },
    ApprovalDecide(ApprovalDecide),
    PlanCommand(PlanCommand),
    ReviewCommand(ReviewCommand),
    SessionCommand(SessionCommand),
    HostCommand(HostCommand),
    /// What became of a mutation: `accepted` first for work that takes a while, then `done`,
    /// `refused` or `uncertain`. Echoes the mutation's `request_id`.
    CommandResult(CommandResult),
    Ack {
        cursor: Cursor,
    },
    /// With no `screens`, resend the board as a `state_snapshot`. With `screens`, resend only those
    /// subscribed screens as snapshots (after a `screen_delta` that does not follow).
    Resync {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        screens: Vec<String>,
    },
    Ping {
        #[serde(default)]
        nonce: Option<String>,
    },
    Pong {
        #[serde(default)]
        nonce: Option<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

const KINDS: &[&str] = &[
    "machine",
    "tunnel_open",
    "tunnel_opened",
    "tunnel_refused",
    "tunnel_data",
    "tunnel_ack",
    "tunnel_close",
    "folder_get",
    "folder_page",
    "hello",
    "welcome",
    "subscribe",
    "unsubscribe",
    "state_snapshot",
    "session_upsert",
    "session_removed",
    "plan_upsert",
    "plan_removed",
    "skipped",
    "task_report",
    "approval_pending",
    "approval_resolved",
    "diff_summary",
    "host_upsert",
    "trace_get",
    "trace_page",
    "trace_changed",
    "diff_get",
    "diff_file",
    "file_get",
    "file_content",
    "screen_snapshot",
    "screen_delta",
    "history_get",
    "history_chunk",
    "lease_acquire",
    "lease_renew",
    "lease_release",
    "lease_state",
    "terminal_input",
    "terminal_resize",
    "terminal_fit",
    "approval_decide",
    "plan_command",
    "review_command",
    "session_command",
    "host_command",
    "command_result",
    "ack",
    "resync",
    "ping",
    "pong",
    "error",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: Version,
    /// The host's boot epoch, on every host message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// Echoed on the reply to a request that carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub message: Message,
}

impl Envelope {
    pub fn new(message: Message) -> Self {
        Self { v: VERSION, epoch: None, request_id: None, message }
    }

    pub fn host(epoch: &str, message: Message) -> Self {
        Self { v: VERSION, epoch: Some(epoch.into()), request_id: None, message }
    }

    pub fn reply_to(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// A `state_snapshot` at the cursor the projections were loaded at.
    pub fn snapshot(epoch: &str, loaded: &Loaded) -> Self {
        Self::host(
            epoch,
            Message::StateSnapshot {
                cursor: Seq(loaded.cursor),
                revisions: Revisions {
                    sessions: Seq(loaded.sessions.revision),
                    plans: Seq(loaded.plans.revision),
                    approvals: Seq(loaded.approvals.revision),
                    diffs: Seq(loaded.diffs.revision),
                    host: Seq(loaded.host.revision),
                },
                sessions: loaded.sessions.data.clone(),
                plans: loaded.plans.data.clone(),
                approvals: loaded.approvals.data.clone(),
                diffs: loaded.diffs.data.clone(),
                host: loaded.host.data.clone(),
                part: None,
            },
        )
    }
}

impl Message {
    /// Bounds a decoded message must keep: collection sizes and identifier lengths.
    pub fn validate(&self) -> Result<(), WireError> {
        let too_many = |what: &str, n: usize, max: usize| {
            (n > max).then(|| WireError::new(ErrorCode::TooMany, format!("{what}: {n} exceeds {max}")))
        };
        let long = |what: &str, s: &str| {
            (s.len() > MAX_ID_BYTES).then(|| WireError::new(ErrorCode::Oversize, format!("{what} is too long")))
        };
        let preview_bounds = |session: &SessionInfo| {
            too_many("previews", session.previews.len(), crate::preview::MAX_PREVIEWS).or_else(|| {
                session
                    .previews
                    .iter()
                    .find_map(|p| p.validate().err().map(|e| WireError::new(ErrorCode::Malformed, e)))
            })
        };
        let failure = match self {
            Message::Machine { snapshot } => too_many("ports", snapshot.ports.len(), crate::machine::MAX_PORTS)
                .or_else(|| {
                    too_many(
                        "containers",
                        snapshot.containers.as_ref().map_or(0, Vec::len),
                        crate::machine::MAX_CONTAINERS,
                    )
                })
                .or_else(|| too_many("disks", snapshot.disks.len(), crate::machine::MAX_DISKS)),
            Message::TunnelOpen { id, port } => long("id", id)
                .or_else(|| (*port == 0).then(|| WireError::new(ErrorCode::Malformed, "port must be nonzero"))),
            Message::TunnelOpened { id } | Message::TunnelClose { id, .. } => long("id", id),
            Message::TunnelRefused { id, reason } => long("id", id).or_else(|| long("reason", reason)),
            Message::TunnelAck { id, bytes } => long("id", id).or_else(|| {
                (*bytes == 0 || *bytes > TUNNEL_WINDOW)
                    .then(|| WireError::new(ErrorCode::Malformed, "invalid tunnel credit"))
            }),
            Message::TunnelData { id, bytes } => long("id", id).or_else(|| crate::preview::decode(bytes).err()),
            Message::Hello { client_id, versions, capabilities, prior_epoch, .. } => {
                too_many("versions", versions.len(), MAX_VERSIONS)
                    .or_else(|| too_many("capabilities", capabilities.len(), MAX_CAPABILITIES))
                    .or_else(|| long("client_id", client_id))
                    .or_else(|| prior_epoch.as_deref().and_then(|e| long("prior_epoch", e)))
                    .or_else(|| {
                        versions.is_empty().then(|| WireError::new(ErrorCode::Malformed, "hello offers no versions"))
                    })
            }
            Message::Subscribe { topics } | Message::Unsubscribe { topics } => {
                too_many("topics", topics.len(), MAX_SUBSCRIPTIONS).or_else(|| {
                    topics.iter().find_map(|t| match t {
                        Topic::Trace | Topic::Machine => None,
                        Topic::Diff { worktree } => long("worktree", worktree),
                        Topic::Plan { id } => long("plan id", id),
                        Topic::Screen { session, .. } => long("session", session),
                        Topic::Folder { worktree, path } => {
                            long("worktree", worktree).or_else(|| long("path", path)).or_else(|| {
                                (!crate::folder::valid_path(path)).then(|| {
                                    WireError::new(ErrorCode::Malformed, "folder path must be worktree-relative")
                                })
                            })
                        }
                    })
                })
            }
            Message::FolderGet { worktree, path, .. } => {
                long("worktree", worktree).or_else(|| long("path", path)).or_else(|| {
                    (!crate::folder::valid_path(path))
                        .then(|| WireError::new(ErrorCode::Malformed, "folder path must be worktree-relative"))
                })
            }
            Message::TracePage(page) => {
                page.data.validate()?;
                if page.data.entries.len() > crate::trace_feed::MAX_PAGE_ENTRIES
                    || json_len(page) > crate::trace_feed::MAX_PAGE_BYTES
                    || page.offset as usize > crate::trace::MAX_ENTRIES
                    || page.next_offset.is_some_and(|next| {
                        next != page.offset + page.data.entries.len() as u32
                            || next as usize > crate::trace::MAX_ENTRIES
                            || page.data.entries.is_empty()
                    })
                {
                    Some(WireError::new(ErrorCode::Malformed, "invalid trace page"))
                } else {
                    None
                }
            }
            Message::DiffGet { worktree, path, .. } | Message::FileGet { worktree, path } => {
                long("worktree", worktree).or_else(|| long("path", path))
            }
            Message::FolderPage(page) => too_many("folder entries", page.entries.len(), crate::folder::PAGE_ENTRIES)
                .or_else(|| long("worktree", &page.worktree))
                .or_else(|| long("path", &page.path))
                .or_else(|| page.entries.iter().find_map(|entry| long("entry path", &entry.path))),
            Message::SessionUpsert { session, .. } => preview_bounds(session),
            Message::StateSnapshot { part, sessions, .. } => part
                .and_then(|part| {
                    (part.count == 0 || part.index >= part.count)
                        .then(|| WireError::new(ErrorCode::Malformed, "a snapshot part is out of range"))
                })
                .or_else(|| sessions.iter().find_map(preview_bounds)),
            Message::Resync { reason, screens } => too_many("screens", screens.len(), MAX_SUBSCRIPTIONS)
                .or_else(|| reason.as_deref().and_then(|r| long("reason", r)))
                .or_else(|| screens.iter().find_map(|s| long("session", s))),
            Message::ScreenSnapshot(s) => long("session", &s.session)
                .or_else(|| screen::validate_snapshot(s).map(|why| WireError::new(ErrorCode::Malformed, why))),
            Message::ScreenDelta(d) => long("session", &d.session)
                .or_else(|| screen::validate_delta(d).map(|why| WireError::new(ErrorCode::Malformed, why))),
            Message::HistoryGet(h) => long("session", &h.session)
                .or_else(|| too_many("history lines", h.count as usize, screen::MAX_HISTORY_LINES as usize)),
            Message::HistoryChunk(h) => long("session", &h.session)
                .or_else(|| too_many("history lines", h.lines.len(), screen::MAX_HISTORY_LINES as usize)),
            Message::LeaseAcquire { session, .. }
            | Message::LeaseRenew { session, .. }
            | Message::LeaseRelease { session, .. } => long("session", session),
            Message::LeaseState(state) => {
                long("session", &state.session).or_else(|| state.holder.as_deref().and_then(|h| long("holder", h)))
            }
            Message::TerminalInput { session, data, .. } => long("session", session).or_else(|| {
                lease::invalid_input(data).map(|why| {
                    let code = if data.is_empty() { ErrorCode::Malformed } else { ErrorCode::Oversize };
                    WireError::new(code, why)
                })
            }),
            Message::TerminalResize { session, size, .. } => long("session", session)
                .or_else(|| size.and_then(|s| s.invalid()).map(|why| WireError::new(ErrorCode::Malformed, why))),
            Message::TerminalFit { session, size } => {
                long("session", session).or_else(|| size.invalid().map(|why| WireError::new(ErrorCode::Malformed, why)))
            }
            Message::ApprovalDecide(c) => c.validate().err(),
            Message::PlanCommand(c) => c.validate().err(),
            Message::ReviewCommand(c) => c.validate().err(),
            Message::SessionCommand(c) => c.validate().err(),
            Message::HostCommand(c) => c.validate().err(),
            Message::CommandResult(c) => c.validate().err(),
            _ => None,
        };
        failure.map_or(Ok(()), Err)
    }
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Bytes `value` takes as JSON, without building the text.
pub fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_or(0, |()| count.0)
}

/// A machine sample stays within the connection's frame budget. Prefer fewer rows over a
/// stream of oversize errors; the resource readings remain, and the UI reports omitted rows.
pub fn machine_frame(epoch: &str, snapshot: &crate::machine::Machine, budget: usize) -> Result<String, WireError> {
    let mut envelope = Envelope::host(epoch, Message::Machine { snapshot: snapshot.clone() });
    let budget = budget.min(MAX_FRAME_BYTES);
    while json_len(&envelope) > budget {
        let Message::Machine { snapshot } = &mut envelope.message else { unreachable!() };
        snapshot.truncated = true;
        let containers = snapshot.containers.as_ref();
        let sizes = [json_len(&snapshot.ports), json_len(&containers), json_len(&snapshot.disks)];
        match sizes.iter().enumerate().max_by_key(|(_, size)| *size).map(|(i, _)| i) {
            Some(0) if !snapshot.ports.is_empty() => snapshot.ports.truncate(snapshot.ports.len() / 2),
            Some(1) if snapshot.containers.as_ref().is_some_and(|v| !v.is_empty()) => {
                let containers = snapshot.containers.as_mut().expect("present");
                containers.truncate(containers.len() / 2);
            }
            Some(2) if !snapshot.disks.is_empty() => snapshot.disks.truncate(snapshot.disks.len() / 2),
            _ => return Err(WireError::new(ErrorCode::Oversize, "machine resource readings exceed the frame limit")),
        }
    }
    encode(&envelope)
}

/// One part of a board cut into `state_snapshot` parts.
#[derive(Default)]
struct Slice {
    sessions: Vec<SessionInfo>,
    plans: Vec<PlanInfo>,
    approvals: Vec<ApprovalInfo>,
    diffs: Vec<DiffSummary>,
}

/// Cuts the board's items, in order, into slices that fit the budget with the part's own fields.
struct Slicer {
    budget: usize,
    overhead: usize,
    size: usize,
    slices: Vec<Slice>,
}

impl Slicer {
    /// The slice `item` goes in: the current one while it has room, else a new one.
    fn room(&mut self, item: &impl Serialize, what: &str) -> Result<&mut Slice, WireError> {
        let n = json_len(item) + 1; // and its comma
        if self.overhead + n > self.budget {
            return Err(WireError::new(ErrorCode::Oversize, format!("one {what} is over the frame limit")));
        }
        if self.size + n > self.budget {
            self.slices.push(Slice::default());
            self.size = self.overhead;
        }
        self.size += n;
        Ok(self.slices.last_mut().expect("never empty"))
    }
}

/// The board as `state_snapshot` frames of at most `budget` bytes: one when it fits, otherwise
/// parts (`part {index, count}`, as `screen_snapshot` has) that each carry the same cursor and
/// revisions and a slice of the collections, in order. A single session, plan, approval or diff
/// summary over the budget cannot be cut and fails with `oversize`.
pub fn snapshot_frames(epoch: &str, loaded: &Loaded, budget: usize) -> Result<Vec<String>, WireError> {
    let budget = budget.min(MAX_FRAME_BYTES);
    let whole = Envelope::snapshot(epoch, loaded);
    if json_len(&whole) <= budget {
        return Ok(vec![encode(&whole)?]);
    }
    let Message::StateSnapshot { cursor, revisions, host, .. } = whole.message else {
        unreachable!("a snapshot is a state_snapshot")
    };
    let part = |slice: Slice, part: Option<Part>| {
        let Slice { sessions, plans, approvals, diffs } = slice;
        let (revisions, host) = (revisions.clone(), host.clone());
        let message = Message::StateSnapshot { cursor, revisions, sessions, plans, approvals, diffs, host, part };
        Envelope::host(epoch, message)
    };
    // What a part costs before its items, with the longest part numbers it could have.
    let overhead = json_len(&part(Slice::default(), Some(Part { index: u16::MAX, count: u16::MAX })));
    let mut slicer = Slicer { budget, overhead, size: overhead, slices: vec![Slice::default()] };
    for s in &loaded.sessions.data {
        slicer.room(s, "session")?.sessions.push(s.clone());
    }
    for p in &loaded.plans.data {
        slicer.room(p, "plan")?.plans.push(p.clone());
    }
    for a in &loaded.approvals.data {
        slicer.room(a, "approval")?.approvals.push(a.clone());
    }
    for d in &loaded.diffs.data {
        slicer.room(d, "diff summary")?.diffs.push(d.clone());
    }
    let count = u16::try_from(slicer.slices.len())
        .map_err(|_| WireError::new(ErrorCode::Oversize, "the board needs more parts than a snapshot can have"))?;
    slicer
        .slices
        .into_iter()
        .enumerate()
        .map(|(index, slice)| encode(&part(slice, (count > 1).then_some(Part { index: index as u16, count }))))
        .collect()
}

/// One page of a file's diff: the lines from `offset` (counted across the hunks) that fit in
/// `budget` bytes, and the offset of the next page when lines remain. A hunk cut at a page boundary
/// continues on the next page under the same header, its start lines those of its first line.
/// Fails with `not_found` when `offset` is past the end. A page always holds at least one line,
/// so a single line larger than a frame is left for `encode` to refuse.
pub fn diff_page(file: &FileDiff, offset: usize, budget: usize) -> Result<(FileDiff, Option<usize>), WireError> {
    let total: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    if offset > total || (offset == total && total > 0) {
        return Err(WireError::new(
            ErrorCode::NotFound,
            format!("the file has {total} diff lines; offset {offset} is past the end"),
        ));
    }
    let mut page = FileDiff {
        old_path: file.old_path.clone(),
        new_path: file.new_path.clone(),
        status: file.status,
        binary: file.binary,
        hunks: Vec::new(),
        omitted: file.omitted,
    };
    let mut size = json_len(&page);
    let mut index = 0usize;
    for hunk in &file.hunks {
        let mut cut = Hunk {
            header: hunk.header.clone(),
            old_start: hunk.old_start,
            new_start: hunk.new_start,
            lines: Vec::new(),
        };
        let overhead = json_len(&cut) + 1;
        if hunk.lines.is_empty() {
            // It sits before line `index`, so it belongs to the page that reached it first.
            if offset == 0 || index > offset {
                page.hunks.push(cut);
                size += overhead;
            }
            continue;
        }
        // The numbers the next line of this hunk has on each side.
        let (mut old_next, mut new_next) = (hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            let at = index;
            index += 1;
            let numbers = (old_next, new_next);
            old_next = line.old_no.map_or(old_next, |n| n + 1);
            new_next = line.new_no.map_or(new_next, |n| n + 1);
            if at < offset {
                continue;
            }
            let cost = json_len(line) + 1 + if cut.lines.is_empty() { overhead } else { 0 };
            if at > offset && size + cost > budget {
                if !cut.lines.is_empty() {
                    page.hunks.push(cut);
                }
                return Ok((page, Some(at)));
            }
            if cut.lines.is_empty() {
                (cut.old_start, cut.new_start) = numbers;
            }
            size += cost;
            cut.lines.push(line.clone());
        }
        if !cut.lines.is_empty() {
            page.hunks.push(cut);
        }
    }
    Ok((page, None))
}

/// Parse one frame. See the module docs for the order of the checks.
pub fn decode(frame: &str) -> Result<Envelope, WireError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(WireError::new(
            ErrorCode::Oversize,
            format!("frame is {} bytes; the limit is {MAX_FRAME_BYTES}", frame.len()),
        ));
    }
    let malformed = |e: &dyn fmt::Display| WireError::new(ErrorCode::Malformed, e.to_string());
    let value: serde_json::Value = serde_json::from_str(frame).map_err(|e| malformed(&e))?;
    let object = value.as_object().ok_or_else(|| malformed(&"a frame is a JSON object"))?;
    let kind = object.get("kind").and_then(|k| k.as_str()).ok_or_else(|| malformed(&"a frame has a string `kind`"))?;
    let v: Version =
        serde_json::from_value(object.get("v").cloned().ok_or_else(|| malformed(&"a frame has a version `v`"))?)
            .map_err(|e| malformed(&e))?;
    if kind != "hello" && v.major != VERSION.major {
        return Err(WireError::new(
            ErrorCode::UnsupportedVersion,
            format!(
                "version {}.{} is not supported; this host speaks {}.{}",
                v.major, v.minor, VERSION.major, VERSION.minor
            ),
        ));
    }
    if !KINDS.contains(&kind) {
        return Err(WireError::new(ErrorCode::UnknownKind, format!("unknown kind `{kind}`")));
    }
    let envelope: Envelope = serde_json::from_value(value).map_err(|e| malformed(&e))?;
    if let Some(id) = &envelope.request_id
        && id.len() > MAX_ID_BYTES
    {
        return Err(WireError::new(ErrorCode::Oversize, "request_id is too long"));
    }
    envelope.message.validate()?;
    Ok(envelope)
}

/// Serialize one frame, refusing one the peer would reject as oversize.
pub fn encode(envelope: &Envelope) -> Result<String, WireError> {
    let frame = serde_json::to_string(envelope).map_err(|e| WireError::new(ErrorCode::Internal, e.to_string()))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(WireError::new(
            ErrorCode::Oversize,
            format!("frame is {} bytes; the limit is {MAX_FRAME_BYTES}", frame.len()),
        ));
    }
    Ok(frame)
}

#[cfg(test)]
mod tests;

//! The ticket worker: runs the generic ticket operations against the configured MCP
//! server on a thread of its own, so a slow or broken ticket system never holds up
//! ATOMICS. Jobs run one at a time, in order, over one kept-open connection.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use atomics_core::remote::{PendingWrite, RemoteState, WITHHELD_TEXT};
use atomics_core::tickets::{
    self, Conversation, McpServer, RemoteError, Steerer, SteeringItem, TicketCard, TicketConfig, TicketLink, Vars,
};
use serde_json::Value;

use crate::mcp_client::{McpClient, McpError, OAuth};

/// After a connection failure, jobs fail at once for this long instead of each
/// waiting for a server that is down.
const BACKOFF: Duration = Duration::from_secs(15);

/// How long a variable that was set nowhere is not looked up again (each lookup may start
/// a login shell).
const MISSING_ENV: Duration = Duration::from_secs(60);

pub enum TicketOp {
    /// Resolve trace credentials on this worker, including cross-process token changes.
    TraceCleaner,
    Whoami,
    /// Stop retrying sidecars from a previous configuration.
    ResetOutboxes,
    /// Register a sidecar for retries, optionally durably adding a new write first.
    Outbox {
        home: PathBuf,
        plan: String,
        write: Option<Box<PendingWrite>>,
    },
    /// Open a plan's conversation (idempotent) and reconcile its sidecar. `recover` handles
    /// deliveries a previous process recorded and never finished; `reset` starts again at the
    /// server's cursor after the conversation was lost.
    RemoteOpen {
        home: PathBuf,
        plan: String,
        link: TicketLink,
        title: String,
        owners: Vec<String>,
        recover: bool,
        reset: bool,
    },
    /// Record the digest whose approval was requested, or none, in the sidecar.
    RemoteRequested {
        home: PathBuf,
        plan: String,
        digest: Option<String>,
    },
    RemoteSteerers(String),
    /// Read steering after the sidecar's cursor and judge each item.
    RemotePoll {
        home: PathBuf,
        plan: String,
        whoami: Box<tickets::Whoami>,
        owners: Vec<String>,
    },
    /// What delivering a polled item to the chief did; acknowledges and moves the cursor.
    RemoteSettle {
        home: PathBuf,
        plan: String,
        item: Box<SteeringItem>,
        outcome: RemoteOutcome,
    },
    // Consumers are added by the chief-tools and Tickets-panel tasks.
    #[allow(dead_code)]
    Search {
        query: String,
        semantic: bool,
    },
    #[allow(dead_code)]
    Children(TicketLink),
    #[allow(dead_code)]
    Related(TicketLink),
    #[allow(dead_code)]
    ReadCard(TicketLink),
    Get(TicketLink),
    /// Read full ticket details for a scoped agent request, without refreshing other links.
    Read(TicketLink),
    /// The tickets on a board.
    List(TicketLink),
    /// The caller retains the nonce across heartbeat/sync retries of a failed push.
    UpdateState(TicketLink, String, String),
    Comment(TicketLink, String),
    Create {
        board: Option<TicketLink>,
        title: String,
        body: String,
    },
}

pub enum TicketOutput {
    TraceCleaner(tickets::Cleaner),
    Whoami(tickets::Whoami),
    /// Messages the UI must surface as toasts (drops, refusals and exhausted retries).
    Outbox(Vec<String>),
    #[allow(dead_code)]
    Cards(Vec<TicketCard>),
    #[allow(dead_code)]
    Card(Box<TicketCard>),
    Ticket(TicketLink),
    Read(Value),
    Tickets(Vec<TicketLink>),
    /// A push went through; the ticket as read right after, when `get` is configured.
    Pushed(Option<Result<TicketLink, String>>),
    Remote(RemoteOutput),
}

/// How a steering item handed to the app was dealt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutcome {
    /// Sent, pending or queued for the chief.
    Delivered,
    /// Held behind a permission prompt; pasted once it is answered.
    Held,
    /// The chief could not receive it; retried on a later poll.
    Unavailable,
    /// Not tried, because an earlier item of the same poll was unavailable.
    Deferred,
    /// An ATOMICS command, answered in the conversation and never delivered.
    Command,
}

pub enum RemoteOutput {
    Opened {
        conversation: Conversation,
        /// Toasts: deliveries a restart interrupted.
        messages: Vec<String>,
        /// The digest whose approval this conversation was already asked for.
        requested: Option<String>,
    },
    Steerers(Vec<Steerer>),
    Polled {
        /// In order; each is recorded in the sidecar's `delivering` before it is returned.
        deliver: Vec<SteeringItem>,
        /// Items shown as "not sent to the chief", with the reason.
        refused: Vec<(SteeringItem, String)>,
    },
    /// The read failed; `throttled` for a 429, 5xx or lost connection, which backs off.
    /// `lost` when the conversation ended or its item became unreadable: polling stops.
    PollFailed {
        error: String,
        throttled: bool,
        lost: bool,
    },
    Settled(Vec<String>),
}

/// Statuses ATOMICS posts about a steering item (§3.4).
pub const DELIVERED_TEXT: &str = "delivered to the chief";
pub const HELD_TEXT: &str = "held until the permission prompt in ATOMICS is answered";
pub const UNDELIVERED_TEXT: &str = "not delivered: the chief cannot receive messages";
pub const RESTARTED_TEXT: &str = "may not have reached the chief because ATOMICS restarted; send it again if needed";
/// Starts the outbox message saying the plan's own conversation refused a write as ended.
pub const CONVERSATION_ENDED: &str = "The ticket system ended the remote conversation";
/// At most this many deliveries a minute per plan; the rest wait in order.
pub const DELIVERIES_PER_MINUTE: usize = 10;
/// Polls an unavailable chief gets before an item is given up.
pub const DELIVERY_ATTEMPTS: u32 = 3;

pub struct TicketJob<T> {
    pub config: Arc<TicketConfig>,
    pub op: TicketOp,
    pub tag: T,
}

/// Starts the worker. Each finished job comes back on `done` with its tag.
pub fn spawn<T: Clone + Send + 'static>(
    done: crate::host::Sender<(T, Result<TicketOutput, String>)>,
) -> Sender<TicketJob<T>> {
    spawn_with(done, OAuth::current())
}

/// Like `spawn`, with the keychain and browser that sign-in uses. The worker only reads saved
/// sign-ins: a job against a server that is not signed in fails with "sign-in required" and never
/// opens the browser; signing in is the user's act (`OAuth::sign_in`).
pub fn spawn_with<T: Clone + Send + 'static>(
    done: crate::host::Sender<(T, Result<TicketOutput, String>)>,
    oauth: OAuth,
) -> Sender<TicketJob<T>> {
    let (tx, rx) = std::sync::mpsc::channel::<TicketJob<T>>();
    std::thread::Builder::new()
        .name("atomics-tickets".into())
        .spawn(move || {
            let mut worker = Worker { oauth: Some(oauth), ..Worker::default() };
            let mut outboxes: Vec<(Arc<TicketConfig>, PathBuf, String, T)> = Vec::new();
            loop {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(job) => {
                        if matches!(job.op, TicketOp::ResetOutboxes) {
                            outboxes.clear();
                        }
                        if let TicketOp::Outbox { home, plan, .. } = &job.op {
                            outboxes.retain(|(_, h, p, _)| h != home || p != plan);
                            outboxes.push((job.config.clone(), home.clone(), plan.clone(), job.tag.clone()));
                        }
                        worker.prepare(&job.config);
                        let result = worker.run(&job.config, job.op);
                        let result = worker.clean(result);
                        if done.send((job.tag, result)).is_err() {
                            break;
                        }
                        // Observe config changes and disconnection before another retry.
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                let mut disconnected = false;
                outboxes.retain(|(config, home, plan, tag)| {
                    worker.prepare(config);
                    let result = worker.drain_outbox(config, home, plan);
                    // A broken sidecar is reported once; a new registration may try again.
                    let keep = result.is_ok();
                    let quiet = matches!(&result, Ok(TicketOutput::Outbox(messages)) if messages.is_empty());
                    if !quiet && done.send((tag.clone(), worker.clean(result))).is_err() {
                        disconnected = true;
                    }
                    keep
                });
                if disconnected {
                    break;
                }
            }
        })
        .expect("ticket worker thread");
    tx
}

#[derive(Default)]
struct Worker {
    /// The connection, with the server and token it was made with.
    client: Option<(McpServer, Option<String>, McpClient)>,
    /// The token the current job's connection should authenticate with, as it resolves now.
    token: Option<String>,
    /// The server that failed, when, and its cleaned error.
    down: Option<(McpServer, Instant, String)>,
    /// Cleans what the current job returns, with that job's secrets.
    cleaner: tickets::Cleaner,
    /// Variables recently found set nowhere.
    missing: std::collections::HashMap<String, Instant>,
    /// Where saved browser sign-ins are read from.
    oauth: Option<OAuth>,
    /// How the server refused the last call, when `[tickets.remote] errors` recognises it.
    refusal: Option<RemoteError>,
    /// The last failure was a lost connection, a timeout, a 429 or a 5xx.
    throttled: bool,
    /// When each plan's recent steering items were handed out, for the per-minute cap.
    handed: std::collections::HashMap<(PathBuf, String), std::collections::VecDeque<Instant>>,
}

impl Worker {
    /// Resolves the job's credentials as they are now. Everything the job brings back is
    /// cleaned with them, so a result arriving after a config change never carries the old
    /// ones. A connection made with another token is replaced, and the job is cleaned for
    /// that token too until the old connection is gone.
    fn prepare(&mut self, config: &TicketConfig) {
        self.token = config.server.bearer_env().map(str::to_string).and_then(|name| self.env(&name));
        let secrets = tickets::secrets_of(config, |name| self.env(name));
        let mut cleaner = tickets::Cleaner::new(secrets);
        if let Some((_, used, _)) = &self.client
            && *used != self.token
        {
            cleaner = cleaner.with(used.clone());
            self.client = None;
        }
        if let Some((_, _, client)) = &self.client {
            cleaner = cleaner.with(client.oauth_token().map(str::to_string));
        }
        self.cleaner = cleaner;
    }

    /// A variable as ATOMICS or the login shell sets it now. One set nowhere is not looked up
    /// again for a while: each lookup may start a login shell.
    fn env(&mut self, name: &str) -> Option<String> {
        if self.missing.get(name).is_some_and(|at| at.elapsed() < MISSING_ENV) {
            return std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        }
        let value = crate::setup::env_var(name);
        match &value {
            None => self.missing.insert(name.to_string(), Instant::now()),
            Some(_) => self.missing.remove(name),
        };
        value
    }

    /// Errors and everything read from the server, before it leaves the worker.
    fn clean(&self, result: Result<TicketOutput, String>) -> Result<TicketOutput, String> {
        let error = |error: String| self.cleaner.error(&error);
        let link = |link: TicketLink| self.cleaner.link(link);
        Ok(match result.map_err(error)? {
            TicketOutput::TraceCleaner(cleaner) => TicketOutput::TraceCleaner(cleaner),
            TicketOutput::Whoami(identity) => TicketOutput::Whoami(identity.cleaned(&self.cleaner)),
            TicketOutput::Outbox(messages) => TicketOutput::Outbox(messages.into_iter().map(error).collect()),
            TicketOutput::Ticket(ticket) => TicketOutput::Ticket(link(ticket)),
            TicketOutput::Tickets(tickets) => TicketOutput::Tickets(tickets.into_iter().map(link).collect()),
            TicketOutput::Read(value) => TicketOutput::Read(self.cleaner.value(value)),
            TicketOutput::Card(card) => TicketOutput::Card(Box::new(self.cleaner.card(*card))),
            TicketOutput::Cards(cards) => {
                TicketOutput::Cards(cards.into_iter().map(|card| self.cleaner.card(card)).collect())
            }
            TicketOutput::Pushed(read) => TicketOutput::Pushed(read.map(|read| read.map(link).map_err(error))),
            TicketOutput::Remote(output) => TicketOutput::Remote(match output {
                RemoteOutput::Opened { conversation, messages, requested } => RemoteOutput::Opened {
                    requested,
                    conversation: self.cleaner.conversation(conversation),
                    messages: messages.into_iter().map(error).collect(),
                },
                RemoteOutput::Steerers(steerers) => RemoteOutput::Steerers(
                    self.cleaner.conversation(Conversation { steerers, ..Conversation::default() }).steerers,
                ),
                RemoteOutput::Polled { deliver, refused } => RemoteOutput::Polled {
                    deliver: deliver.into_iter().map(|item| self.cleaner.steering(item)).collect(),
                    refused: refused.into_iter().map(|(item, why)| (self.cleaner.steering(item), error(why))).collect(),
                },
                RemoteOutput::PollFailed { error: why, throttled, lost } => {
                    RemoteOutput::PollFailed { error: error(why), throttled, lost }
                }
                RemoteOutput::Settled(messages) => RemoteOutput::Settled(messages.into_iter().map(error).collect()),
            }),
        })
    }

    fn run(&mut self, config: &TicketConfig, op: TicketOp) -> Result<TicketOutput, String> {
        let system = config.system.as_str();
        match op {
            TicketOp::ResetOutboxes => Ok(TicketOutput::Outbox(Vec::new())),
            TicketOp::RemoteOpen { home, plan, link, title, owners, recover, reset } => {
                self.remote_open(config, &home, &plan, &link, &title, owners, recover, reset)
            }
            TicketOp::RemoteRequested { home, plan, digest } => {
                let mut state = RemoteState::load(&home, &plan).map_err(|e| e.to_string())?;
                if state.requested_digest != digest {
                    state.requested_digest = digest;
                    state.save(&home, &plan).map_err(|e| e.to_string())?;
                }
                Ok(TicketOutput::Outbox(Vec::new()))
            }
            TicketOp::RemoteSteerers(conversation) => {
                let value = self.call(
                    config,
                    "conversation_steerers",
                    &Vars::new(system).with("conversation", Some(&conversation)),
                )?;
                let steerers = config.operation("conversation_steerers").expect("called").steerers(&value);
                Ok(TicketOutput::Remote(RemoteOutput::Steerers(steerers)))
            }
            TicketOp::RemotePoll { home, plan, whoami, owners } => {
                self.remote_poll(config, &home, &plan, &whoami, &owners)
            }
            TicketOp::RemoteSettle { home, plan, item, outcome } => {
                self.remote_settle(config, &home, &plan, &item, outcome)
            }
            TicketOp::Whoami => {
                let value = self.call(config, "whoami", &Vars::new(system))?;
                tickets::Whoami::read(config.operation("whoami").expect("called"), &value).map(TicketOutput::Whoami)
            }
            TicketOp::Outbox { home, plan, write } => {
                let mut messages = Vec::new();
                if let Some(mut write) = write {
                    // Resolve the same credentials as the trace before approval text reaches
                    // either the durable outbox or the conversation.
                    let mut blank = false;
                    if write.op == "conversation_post" {
                        let body = write.vars.get("body").and_then(Value::as_str).unwrap_or_default();
                        let cleaned = self.trace_cleaner(config)?.redacted(body);
                        // The server counts UTF-16 units after trimming, and refuses blank text.
                        let body = if write.mirror {
                            atomics_core::trace::cap_block(&cleaned)
                        } else {
                            tickets::utf16_prefix(cleaned.trim(), atomics_core::trace::MAX_EVENT_UNITS).to_string()
                        };
                        blank = body.trim().is_empty();
                        write.vars = write.vars.with("body", Some(&body));
                    }
                    let mut state = RemoteState::load(&home, &plan).map_err(|e| e.to_string())?;
                    if !blank && state.queue(&home, &plan, *write).map_err(|e| e.to_string())?.is_some() {
                        messages.push("Remote outbox full: dropped a tool block".into());
                    }
                }
                let TicketOutput::Outbox(mut drained) = self.drain_outbox(config, &home, &plan)? else {
                    unreachable!()
                };
                messages.append(&mut drained);
                Ok(TicketOutput::Outbox(messages))
            }
            TicketOp::TraceCleaner => Ok(TicketOutput::TraceCleaner(self.trace_cleaner(config)?)),
            TicketOp::Search { query, semantic } => {
                let name = if semantic { "semantic_search" } else { "search" };
                self.cards(config, name, &Vars::new(system).query(&query))
            }
            TicketOp::Children(link) | TicketOp::Related(link) | TicketOp::ReadCard(link) if !config.accepts(&link) => {
                Err(config.mismatch(&link).expect("mismatch"))
            }
            TicketOp::Children(link) => self.cards(config, "children", &Vars::new(system).ticket(&link)),
            TicketOp::Related(link) => self.cards(config, "related", &Vars::new(system).ticket(&link)),
            TicketOp::ReadCard(link) => {
                let name = if config.operation("read").is_some() { "read" } else { "get" };
                let output = self.call(config, name, &Vars::new(system).ticket(&link))?;
                let operation = config.operation(name).expect("called");
                let card = if name == "read" {
                    operation.card(&output, system)?
                } else {
                    operation.fallback_card(&output, system)?
                };
                Ok(TicketOutput::Card(Box::new(config.card_url(card)?)))
            }
            TicketOp::Read(link) => self.call(config, "get", &Vars::new(system).ticket(&link)).map(TicketOutput::Read),
            TicketOp::Get(link) => {
                let output = self.call(config, "get", &Vars::new(system).ticket(&link))?;
                Ok(TicketOutput::Ticket(
                    config.link_url(config.operation("get").expect("called").ticket(&output, system)?)?,
                ))
            }
            TicketOp::List(board) => {
                let name = config.list_operation().unwrap_or("list");
                let output = self.call(config, name, &Vars::new(system).ticket(&board).board(Some(&board)))?;
                let links = config.operation(name).expect("called").tickets(&output, system)?;
                Ok(TicketOutput::Tickets(
                    links.into_iter().map(|link| config.link_url(link)).collect::<Result<_, _>>()?,
                ))
            }
            TicketOp::UpdateState(link, state, nonce) => {
                self.call(
                    config,
                    "update_state",
                    &Vars::new(system).ticket(&link).with("state", Some(&state)).with("nonce", Some(&nonce)),
                )?;
                Ok(TicketOutput::Pushed(self.reread(config, link)))
            }
            TicketOp::Comment(link, body) => {
                let body = config.comment_body(&body);
                let nonce = uuid::Uuid::new_v4().to_string();
                self.call(
                    config,
                    "comment",
                    &Vars::new(system).ticket(&link).with("body", Some(&body)).with("nonce", Some(&nonce)),
                )?;
                Ok(TicketOutput::Pushed(self.reread(config, link)))
            }
            TicketOp::Create { board, title, body } => {
                let nonce = uuid::Uuid::new_v4().to_string();
                let vars = Vars::new(system)
                    .board(board.as_ref())
                    .with("title", Some(&title))
                    .with("body", Some(&body))
                    .with("nonce", Some(&nonce));
                let output = self.call(config, "create", &vars)?;
                let created = config.operation("create").expect("called").ticket(&output, system)?;
                if created.id.is_empty() {
                    return Err("the create operation's result has no id: set operations.create.result.id".into());
                }
                Ok(TicketOutput::Ticket(config.link_url(created)?))
            }
        }
    }

    fn trace_cleaner(&self, config: &TicketConfig) -> Result<tickets::Cleaner, String> {
        let secrets = match &self.oauth {
            Some(oauth) => oauth.secrets(&config.server).map_err(|err| err.to_string())?,
            None => Vec::new(),
        };
        Ok(self.cleaner.clone().with(secrets))
    }

    /// Runs only on the ticket worker. A failed reply leaves the original nonce and
    /// variables on disk; a process restart registers the same sidecar and replays it.
    fn drain_outbox(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
    ) -> Result<TicketOutput, String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.drain_outbox_at(config, home, plan, now_ms)
    }

    fn drain_outbox_at(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        now_ms: u64,
    ) -> Result<TicketOutput, String> {
        let mut state = RemoteState::load(home, plan).map_err(|e| e.to_string())?;
        state.reset_post_day(now_ms);
        let mut messages = Vec::new();
        // Each lane keeps its order: a delayed post holds later posts behind it, never acknowledgements.
        for lane in ["conversation_ack", "conversation_post"] {
            self.drain_lane(config, home, plan, &mut state, lane, now_ms, &mut messages)?;
        }
        Ok(TicketOutput::Outbox(messages))
    }

    #[allow(clippy::too_many_arguments)]
    fn drain_lane(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        state: &mut RemoteState,
        lane: &str,
        now_ms: u64,
        messages: &mut Vec<String>,
    ) -> Result<(), String> {
        let now = now_ms / 1000;
        // A final refusal often repeats for every queued write; it is said once.
        let say = |messages: &mut Vec<String>, message: String| {
            if !messages.contains(&message) {
                messages.push(message);
            }
        };
        while let Some(index) = state.outbox.iter().position(|write| write.op == lane) {
            let write = state.outbox[index].clone();
            if write.mirror && state.posted_today >= atomics_core::remote::DAILY_POSTS {
                state.outbox.remove(index);
                if !state.pause_shown {
                    messages.push("Conversation mirror paused: 1000 posts today".into());
                    state.pause_shown = true;
                }
                state.save(home, plan).map_err(|e| e.to_string())?;
                continue;
            }
            if write.next_at > now {
                break;
            }
            if write.op == "conversation_post" {
                if state.last_post_ms.is_some_and(|at| now_ms.saturating_sub(at) < config.remote.min_interval_ms) {
                    break;
                }
                // Persist before transport, also bounding refused posts and lost replies.
                state.last_post_ms = Some(now_ms);
                state.outbox[index].attempted = true;
                state.save(home, plan).map_err(|e| e.to_string())?;
            }
            let vars = write.vars.clone().with("nonce", Some(&write.nonce));
            let result = self.call(config, &write.op, &vars);
            // A server assigning sequences itself can race over one: a conflict on a post sent
            // without a sequence is transient.
            if self.refusal == Some(RemoteError::Conflict)
                && write.op == "conversation_post"
                && config.operation(&write.op).is_some_and(|op| !op.uses_placeholder("seq"))
            {
                self.refusal = None;
            }
            let conversation = write.vars.get("conversation").and_then(Value::as_str);
            match result {
                Ok(_) => {
                    state.outbox.remove(index);
                    if write.op == "conversation_post" {
                        state.posted_today += 1;
                    }
                }
                Err(_) if self.refusal == Some(RemoteError::ContentRefused) => {
                    state.outbox.remove(index);
                    messages.push("Remote text withheld: the ticket system refused it".into());
                    if write.op == "conversation_post" && !write.withheld {
                        // A different body needs a new event id; keep the reserved sequence.
                        let mut replacement = write;
                        replacement.nonce = uuid::Uuid::new_v4().to_string();
                        replacement.vars = replacement.vars.with("body", Some(WITHHELD_TEXT));
                        replacement.withheld = true;
                        replacement.mirror = false;
                        replacement.tool = false;
                        replacement.attempts = 0;
                        replacement.attempted = false;
                        replacement.next_at = 0;
                        state.outbox.insert(index, replacement);
                    }
                }
                // The plan's own conversation ended: its writes are dropped and the app stops
                // polling and opens it again, as after a lost read.
                Err(error)
                    if self.refusal == Some(RemoteError::Ended)
                        && conversation.is_some()
                        && conversation == state.conversation.as_deref() =>
                {
                    let ended = state.conversation.clone();
                    state.outbox.retain(|w| w.vars.get("conversation").and_then(Value::as_str) != ended.as_deref());
                    say(messages, format!("{CONVERSATION_ENDED}: {error}"));
                }
                // An ended conversation, a validation error or a used sequence fails the same way again.
                Err(error) if self.refusal.is_some() => {
                    state.outbox.remove(index);
                    say(messages, format!("Remote write dropped: {error}"));
                }
                Err(error) => {
                    if !state.outbox[index].retry(now) {
                        state.outbox.remove(index);
                        messages.push(format!("Remote write dropped after 10 attempts: {error}"));
                    }
                    state.save(home, plan).map_err(|e| e.to_string())?;
                    break;
                }
            }
            state.save(home, plan).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn remote_open(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        link: &TicketLink,
        title: &str,
        owners: Vec<String>,
        recover: bool,
        reset: bool,
    ) -> Result<TicketOutput, String> {
        let nonce = uuid::Uuid::new_v4().to_string();
        let title = Some(tickets::conversation_title(title))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| tickets::conversation_title(link.label()));
        let vars = Vars::new(&config.system)
            .ticket(link)
            .with("title", Some(&title))
            .with("nonce", Some(&nonce))
            .typed("owners", Value::from(owners));
        let value = self.call(config, "conversation_open", &vars)?;
        let conversation = config.operation("conversation_open").expect("called").conversation(&value)?;
        let mut state = RemoteState::load(home, plan).map_err(|e| e.to_string())?;
        let mut messages = Vec::new();
        if recover && !state.delivering.is_empty() {
            // Recorded intent without a recorded outcome: the paste may or may not have
            // happened, so it is never repeated. The writes go to the conversation it came from.
            let from = state.conversation.clone().unwrap_or_else(|| conversation.id.clone());
            for id in std::mem::take(&mut state.delivering).into_keys() {
                self.queue_status(config, home, plan, &mut state, &from, &id, RESTARTED_TEXT, true)?;
                state.remember(id);
                messages.push(format!("A remote message {RESTARTED_TEXT}"));
            }
        }
        // Another plan on the same ticket may have followed this conversation: one conversation per ticket.
        let (their_cursor, their_seen) =
            RemoteState::used_cursor(home, plan, &conversation.id).map_err(|e| e.to_string())?;
        if state.conversation.as_deref() != Some(conversation.id.as_str()) {
            // A new conversation starts at the server's newest revision: nothing written
            // before remote was on, or before a lost sidecar, is ever delivered. Taken over from
            // another plan, it starts where that plan stopped. Its events start at 1.
            state.conversation = Some(conversation.id.clone());
            state.cursor = their_cursor.clone().or_else(|| conversation.cursor.clone());
            state.retries.clear();
            state.requested_digest = None;
            state.next_seq = 1;
        } else if reset || state.cursor.is_none() {
            state.cursor = conversation.cursor.clone();
            if reset {
                state.retries.clear();
                state.requested_digest = None;
            }
        }
        // Taken back: what the other plan handled since is neither delivered again nor skipped.
        if their_cursor.is_some() && tickets::seq_after(their_cursor.as_ref(), state.cursor.as_ref()) {
            state.cursor = their_cursor;
        }
        for id in their_seen {
            state.remember(id);
        }
        state.next_seq =
            state.next_seq.max(RemoteState::used_seq(home, plan, &conversation.id).map_err(|e| e.to_string())?);
        if let Some(next) = conversation.next_seq {
            // The server knows the conversation's sequence; writes already queued keep theirs.
            let reserved = state.outbox.iter().filter_map(|w| w.vars.get("seq")?.as_u64()).max().map_or(0, |s| s + 1);
            state.next_seq = next.max(reserved);
        }
        let requested = state.requested_digest.clone();
        state.save(home, plan).map_err(|e| e.to_string())?;
        if let TicketOutput::Outbox(mut drained) = self.drain_outbox(config, home, plan)? {
            messages.append(&mut drained);
        }
        Ok(TicketOutput::Remote(RemoteOutput::Opened { conversation, messages, requested }))
    }

    fn remote_poll(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        whoami: &tickets::Whoami,
        owners: &[String],
    ) -> Result<TicketOutput, String> {
        let mut state = RemoteState::load(home, plan).map_err(|e| e.to_string())?;
        let conversation = state.conversation.clone().ok_or("the plan's conversation is not open")?;
        let vars = Vars::new(&config.system)
            .with("conversation", Some(&conversation))
            .typed("after", state.cursor.clone().unwrap_or(Value::Null));
        let value = match self.call(config, "conversation_read", &vars) {
            Ok(value) => value,
            Err(error) => {
                let lost = matches!(self.refusal, Some(RemoteError::Unreadable | RemoteError::Ended));
                return Ok(TicketOutput::Remote(RemoteOutput::PollFailed { error, throttled: self.throttled, lost }));
            }
        };
        let items = config.operation("conversation_read").expect("called").steering(&value, &conversation)?;
        let handed = self.handed.entry((home.to_path_buf(), plan.to_string())).or_default();
        while handed.front().is_some_and(|at| at.elapsed() >= Duration::from_secs(60)) {
            handed.pop_front();
        }
        let (mut deliver, mut refused) = (Vec::new(), Vec::new());
        // The cursor only moves over a settled prefix, so an item waiting its turn is read again.
        let mut waiting = false;
        for item in items {
            if state.delivering.contains_key(&item.id) {
                waiting = true;
                continue;
            }
            let verdict = tickets::steering_verdict(
                &item,
                whoami,
                owners,
                &config.remote.human_kind,
                state.cursor.as_ref(),
                &state.seen,
                &conversation,
            );
            match verdict {
                Err(refusal) => {
                    if !refusal.quiet() {
                        state.remember(item.id.clone());
                        refused.push((item.clone(), refusal.reason()));
                    }
                    if !waiting
                        && let Some(seq) = &item.seq
                        && tickets::seq_after(Some(seq), state.cursor.as_ref())
                    {
                        state.cursor = Some(seq.clone());
                    }
                }
                Ok(()) if handed.len() >= DELIVERIES_PER_MINUTE => waiting = true,
                Ok(()) => {
                    handed.push_back(Instant::now());
                    state.delivering.insert(item.id.clone(), atomics_core::store::now());
                    deliver.push(item);
                    waiting = true;
                }
            }
        }
        // Intent is durable before anything is pasted.
        state.save(home, plan).map_err(|e| e.to_string())?;
        Ok(TicketOutput::Remote(RemoteOutput::Polled { deliver, refused }))
    }

    fn remote_settle(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        item: &SteeringItem,
        outcome: RemoteOutcome,
    ) -> Result<TicketOutput, String> {
        let mut state = RemoteState::load(home, plan).map_err(|e| e.to_string())?;
        let conversation = item.conversation.clone();
        state.delivering.remove(&item.id);
        let (ack, status) = match &outcome {
            RemoteOutcome::Delivered => (true, Some(DELIVERED_TEXT.to_string())),
            RemoteOutcome::Held => (true, Some(HELD_TEXT.to_string())),
            RemoteOutcome::Command => (true, None),
            RemoteOutcome::Deferred => (false, None),
            RemoteOutcome::Unavailable => {
                let tries = state.retries.entry(item.id.clone()).or_default();
                *tries += 1;
                if *tries >= DELIVERY_ATTEMPTS { (false, Some(UNDELIVERED_TEXT.to_string())) } else { (false, None) }
            }
        };
        if outcome == RemoteOutcome::Command {
            let vars = Vars::new(&config.system)
                .with("conversation", Some(&conversation))
                .typed("ids", Value::from(vec![item.id.clone()]));
            let write = PendingWrite::new("conversation_ack", uuid::Uuid::new_v4().to_string(), vars);
            state.queue(home, plan, write).map_err(|e| e.to_string())?;
        }
        if status.is_some() || outcome == RemoteOutcome::Command {
            if let Some(status) = status {
                self.queue_status(config, home, plan, &mut state, &conversation, &item.id, &status, ack)?;
            }
            state.retries.remove(&item.id);
            state.remember(item.id.clone());
            if let Some(seq) = &item.seq
                && tickets::seq_after(Some(seq), state.cursor.as_ref())
                && state.delivering.is_empty()
            {
                state.cursor = Some(seq.clone());
            }
        }
        state.save(home, plan).map_err(|e| e.to_string())?;
        let TicketOutput::Outbox(messages) = self.drain_outbox(config, home, plan)? else { unreachable!() };
        Ok(TicketOutput::Remote(RemoteOutput::Settled(messages)))
    }

    /// Queue an optional acknowledgement and a status event about one steering item.
    #[allow(clippy::too_many_arguments)]
    fn queue_status(
        &mut self,
        config: &TicketConfig,
        home: &std::path::Path,
        plan: &str,
        state: &mut RemoteState,
        conversation: &str,
        id: &str,
        text: &str,
        ack: bool,
    ) -> Result<(), String> {
        let remote = &config.remote;
        let base = Vars::new(&config.system).with("conversation", Some(conversation));
        if ack {
            let vars = base.clone().typed("ids", Value::from(vec![id.to_string()]));
            let write = PendingWrite::new("conversation_ack", uuid::Uuid::new_v4().to_string(), vars);
            state.queue(home, plan, write).map_err(|e| e.to_string())?;
        }
        // No activity: a delivery status never changes what the conversation waits for, so an
        // open approval request stays open.
        let vars = base
            .with("kind", Some(&remote.kinds.status))
            .with("body", Some(text))
            .with("activity", Some(""))
            .with("turn", Some(&format!("steering:{plan}:{id}")));
        let write = PendingWrite::new("conversation_post", uuid::Uuid::new_v4().to_string(), vars);
        state.queue(home, plan, write).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn cards(&mut self, config: &TicketConfig, name: &str, vars: &Vars) -> Result<TicketOutput, String> {
        let output = self.call(config, name, vars)?;
        let cards = config.operation(name).expect("called").cards(&output, &config.system)?;
        Ok(TicketOutput::Cards(cards.into_iter().map(|card| config.card_url(card)).collect::<Result<_, _>>()?))
    }

    /// The ticket right after a push, so the card shows what the system now says.
    fn reread(&mut self, config: &TicketConfig, link: TicketLink) -> Option<Result<TicketLink, String>> {
        config.operation("get")?;
        Some(match self.run(config, TicketOp::Get(link)) {
            Ok(TicketOutput::Ticket(ticket)) => Ok(ticket),
            Ok(_) => Err("unexpected get result".into()),
            Err(err) => Err(err),
        })
    }

    /// Calls the tool behind a generic operation and returns what its result paths read from.
    fn call(&mut self, config: &TicketConfig, name: &str, vars: &Vars) -> Result<Value, String> {
        self.refusal = None;
        self.throttled = false;
        let operation = config.operation(name).ok_or_else(|| format!("operations.{name} is not configured"))?;
        let write = matches!(
            name,
            "update_state" | "comment" | "create" | "conversation_open" | "conversation_post" | "conversation_ack"
        );
        let idempotent = write && operation.uses_placeholder("nonce");
        if !write && operation.uses_placeholder("nonce") {
            return Err(format!("operations.{name}: {{nonce}} is only allowed in write operations"));
        }
        // Expand once: transport retries must send exactly the original write arguments.
        let arguments = operation.arguments(vars)?;
        if let Some((server, since, error)) = &self.down
            && *server == config.server
            && since.elapsed() < BACKOFF
        {
            return Err(format!("{} is unavailable: {error}", config.system));
        }
        for attempt in 0..=1 {
            if self.client.as_ref().is_none_or(|(server, used, _)| *server != config.server || *used != self.token) {
                self.client = None;
                let timeout = Duration::from_secs(config.timeout_secs.max(1));
                // The connection authenticates with the token this job resolved, and the worker records
                // the token the connection reports using, so the two can never quietly differ.
                let token = self.token.clone();
                let bearer = config.server.bearer_env().map(str::to_string);
                let env = |name: &str| {
                    if bearer.as_deref() == Some(name) { token.clone() } else { crate::setup::env_var(name) }
                };
                let oauth = config.server.uses_oauth().then_some(()).and(self.oauth.as_ref());
                match McpClient::connect_with(&config.server, timeout, env, oauth) {
                    Ok(client) => {
                        let used = client.token().map(str::to_string);
                        self.client = Some((config.server.clone(), used, client));
                    }
                    Err(err) => return Err(self.broken(&config.server, err)),
                }
            }
            let (_, _, client) = self.client.as_mut().expect("connected");
            let result = client.call_tool(&operation.tool, arguments.clone());
            // A refresh during the call may have changed the access token: clean for the current one.
            if let Some(token) = client.oauth_token() {
                self.cleaner = std::mem::take(&mut self.cleaner).with([token.to_string()]);
            }
            match result {
                Ok(result) => {
                    self.down = None;
                    return Ok(atomics_core::tickets::tool_output(&result));
                }
                Err(McpError::Transport { retryable: true, .. }) if idempotent && attempt == 0 => {
                    self.client = None;
                }
                Err(err) => {
                    if let McpError::Server(message) = &err {
                        self.refusal = config.remote.errors.classify(message);
                    }
                    return Err(self.broken(&config.server, err));
                }
            }
        }
        unreachable!("the final attempt returns")
    }

    /// The backoff remembers the failure cleaned with this job's secrets: a later job may
    /// run with others.
    fn broken(&mut self, server: &McpServer, err: McpError) -> String {
        self.throttled = matches!(err, McpError::Transport { retryable: true, .. });
        if matches!(err, McpError::SignInRequired(_)) {
            // No backoff: once the user signs in, the next job reconnects with the new tokens.
            self.client = None;
        }
        if let McpError::Transport { message, .. } = &err {
            self.client = None;
            self.down = Some((server.clone(), Instant::now(), self.cleaner.error(message)));
        }
        err.to_string()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::Receiver;

    pub(crate) fn remote_profile(prefix: &str, path: &std::path::Path) -> TicketConfig {
        let mut config = crate::mcp_client::tests::example_profile("meaning-example", path);
        config.remote.min_interval_ms = 0;
        let whoami = match prefix {
            "card_" => {
                json!({"id":"actor.ref", "kind":"actor.type", "name":"actor.name", "owner_id":"actor.owner.ref", "owner_name":"actor.owner.name"})
            }
            "issue_" => {
                json!({"id":"uuid", "kind":"kind", "name":"name", "owner_id":"owner_id", "owner_name":"owner_name"})
            }
            _ => {
                json!({"id":"userId", "kind":"kind", "name":"displayName", "owner_id":"ownerUserId", "owner_name":"ownerDisplayName"})
            }
        };
        config.operations.whoami =
            Some(serde_json::from_value(json!({"tool":format!("{prefix}whoami"), "result":whoami})).unwrap());
        config.operations.conversation_post = Some(serde_json::from_value(json!({
            "tool":format!("{prefix}conversation_post"), "arguments":{"conversation":"{conversation}", "eventId":"{nonce}", "sequence":"{seq}", "text":"{body}"}
        })).unwrap());
        config.operations.conversation_ack = Some(
            serde_json::from_value(json!({
                "tool":format!("{prefix}conversation_ack"), "arguments":{"conversation":"{conversation}", "ids":"{ids}"}
            }))
            .unwrap(),
        );
        config
    }

    /// The X1 profile with every conversation operation mapped in the dialect of `prefix`
    /// ("", "card_" or "issue_"), and `[tickets.remote]` on.
    pub(crate) fn conversation_profile(prefix: &str, path: &std::path::Path) -> TicketConfig {
        let mut config = remote_profile(prefix, path);
        let (open, read, steerers) = match prefix {
            "card_" => (
                json!({"id":"conversation.ref", "cursor":"conversation.head", "steerers":"conversation.people", "steerer_id":"ref", "steerer_name":"name"}),
                (
                    "entries",
                    json!({"id":"ref", "seq":"rev", "author_id":"by.ref", "author_kind":"by.type", "author":"by.name", "text":"body", "at":"when"}),
                ),
                json!({"steerers":"conversation.people", "steerer_id":"ref", "steerer_name":"name"}),
            ),
            "issue_" => (
                json!({"id":"id", "cursor":"cursor", "steerers":"steerers", "steerer_id":"id", "steerer_name":"name"}),
                (
                    "messages",
                    json!({"id":"id", "seq":"seq", "author_id":"author_id", "author_kind":"author_kind", "author":"author", "text":"text", "at":"at"}),
                ),
                json!({"steerers":"steerers", "steerer_id":"id", "steerer_name":"name"}),
            ),
            _ => (
                json!({"id":"conversationId", "cursor":"latestRevision", "steerers":"steeringUsers", "steerer_id":"userId", "steerer_name":"displayName"}),
                (
                    "items",
                    json!({"id":"steeringId", "seq":"revision", "author_id":"authorUserId", "author_kind":"authorKind", "author":"authorDisplayName", "text":"text", "at":"createdAtUtc"}),
                ),
                json!({"steerers":"steeringUsers", "steerer_id":"userId", "steerer_name":"displayName"}),
            ),
        };
        let op = |value: Value| serde_json::from_value(value).unwrap();
        config.operations.conversation_open = Some(op(json!({
            "tool":format!("{prefix}conversation_open"),
            "arguments":{"itemType":"tickets.ticket", "itemId":"{uuid}", "title":"{title}", "owners":"{owners}"}, "result":open
        })));
        config.operations.conversation_post = Some(op(json!({
            "tool":format!("{prefix}conversation_post"),
            "arguments":{"conversation":"{conversation}", "eventId":"{nonce}", "sequence":"{seq}", "kind":"{kind}",
                "text":"{body}", "activity":"{activity}", "turn":"{turn}"}
        })));
        config.operations.conversation_read = Some(op(json!({
            "tool":format!("{prefix}conversation_read"),
            "arguments":{"conversation":"{conversation}", "after":"{after}", "take":25}, "items":read.0, "result":read.1
        })));
        config.operations.conversation_steerers = Some(op(json!({
            "tool":format!("{prefix}conversation_steerers"), "arguments":{"conversation":"{conversation}"}, "result":steerers
        })));
        config.remote.enabled = true;
        config.remote.kinds.status = "Status".into();
        config.remote.activity.working = "Working".into();
        config.remote.activity.input = "WaitingForInput".into();
        config
    }

    fn set(path: &std::path::Path, edit: impl FnOnce(&mut Value)) {
        let mut state: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        edit(&mut state);
        std::fs::write(path, state.to_string()).unwrap();
    }

    fn server(path: &std::path::Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn remote_conversation_opens_reads_after_a_revision_acks_and_lists_steerers_in_all_three_dialects() {
        for prefix in ["", "card_", "issue_"] {
            let dir = crate::test_support::tempdir().unwrap();
            let home = dir.path();
            let path = crate::mcp_client::tests::fake_state(home, json!([]));
            let item = |id: &str, revision: u64, author: &str, kind: &str| json!({"id":id, "revision":revision, "author_id":author, "author_kind":kind, "author":"Example human", "text":"status?", "at":"now"});
            set(&path, |state| {
                state["steering"] = json!([item("old", 1, "human-1", "Human")]);
                state["refuse_steerers"] = json!(["human-2"]);
            });
            let config = conversation_profile(prefix, &path);
            let mut worker = Worker::default();
            worker.prepare(&config);
            let link = TicketLink {
                id: "00000000-0000-4000-8000-000000000001".into(),
                key: Some("DEMO-1".into()),
                ..Default::default()
            };
            let open = TicketOp::RemoteOpen {
                home: home.into(),
                plan: "plan".into(),
                link,
                title: "Goal".into(),
                owners: vec!["human-1".into(), "human-2".into()],
                recover: true,
                reset: false,
            };
            let TicketOutput::Remote(RemoteOutput::Opened { conversation, messages, .. }) =
                worker.run(&config, open).unwrap()
            else {
                panic!("{prefix}: open")
            };
            assert!(messages.is_empty());
            assert_eq!(conversation.id, "conversation-1", "{prefix}");
            assert_eq!(conversation.cursor, Some(json!(1)), "{prefix}");
            assert_eq!(conversation.steerers, vec![Steerer { id: "human-1".into(), name: "Example human 1".into() }]);
            let opened = server(&path)["opens"][0].clone();
            assert_eq!(opened["owners"], json!(["human-1", "human-2"]), "typed owners");
            assert_eq!(opened["itemId"], "00000000-0000-4000-8000-000000000001");
            let state = RemoteState::load(home, "plan").unwrap();
            assert_eq!(state.cursor, Some(json!(1)), "starts at the server's newest revision");
            set(&path, |state| {
                state["steering"].as_array_mut().unwrap().extend([
                    item("s-2", 2, "human-1", "Human"),
                    item("s-3", 3, "agent-9", "Agent"),
                    item("s-4", 4, "human-1", "human"),
                ]);
            });
            let whoami = tickets::Whoami { id: "agent-1".into(), kind: "personalAgent".into(), ..Default::default() };
            let poll = || TicketOp::RemotePoll {
                home: home.into(),
                plan: "plan".into(),
                whoami: Box::new(whoami.clone()),
                owners: vec!["human-1".into()],
            };
            let TicketOutput::Remote(RemoteOutput::Polled { deliver, refused }) = worker.run(&config, poll()).unwrap()
            else {
                panic!("{prefix}: poll")
            };
            assert_eq!(deliver.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["s-2", "s-4"], "{prefix}");
            assert_eq!(deliver[0].author.as_deref(), Some("Example human"));
            assert_eq!(refused.len(), 1);
            assert!(refused[0].1.contains("not a human"), "{}", refused[0].1);
            assert_eq!(server(&path)["calls"].as_array().unwrap().last().unwrap()["arguments"]["after"], json!(1));
            let state = RemoteState::load(home, "plan").unwrap();
            assert!(state.delivering.contains_key("s-2") && state.delivering.contains_key("s-4"), "intent first");
            // Items in flight are never handed out twice.
            let TicketOutput::Remote(RemoteOutput::Polled { deliver: again, .. }) =
                worker.run(&config, poll()).unwrap()
            else {
                panic!("poll")
            };
            assert!(again.is_empty());
            for item in deliver {
                let settle = TicketOp::RemoteSettle {
                    home: home.into(),
                    plan: "plan".into(),
                    item: Box::new(item),
                    outcome: RemoteOutcome::Delivered,
                };
                worker.run(&config, settle).unwrap();
            }
            let fake = server(&path);
            let acks: Vec<_> = fake["acks"].as_array().unwrap().iter().map(|ack| ack["ids"].clone()).collect();
            assert_eq!(acks, [json!(["s-2"]), json!(["s-4"])], "{prefix}");
            let posts = fake["posts"].as_array().unwrap();
            assert_eq!(posts.len(), 2);
            assert!(posts.iter().all(|post| post["text"] == DELIVERED_TEXT && post["kind"] == "Status"));
            let state = RemoteState::load(home, "plan").unwrap();
            assert!(state.delivering.is_empty());
            assert_eq!(state.cursor, Some(json!(4)));
            let TicketOutput::Remote(RemoteOutput::Polled { deliver, refused }) = worker.run(&config, poll()).unwrap()
            else {
                panic!("poll")
            };
            assert!(deliver.is_empty() && refused.is_empty());
            assert_eq!(server(&path)["calls"].as_array().unwrap().last().unwrap()["arguments"]["after"], json!(4));
            let TicketOutput::Remote(RemoteOutput::Steerers(steerers)) =
                worker.run(&config, TicketOp::RemoteSteerers("conversation-1".into())).unwrap()
            else {
                panic!("steerers")
            };
            assert_eq!(steerers.len(), 1, "{prefix}");
            assert_eq!(steerers[0].id, "human-1");
        }
    }

    #[test]
    fn remote_unavailable_chief_retries_three_polls_and_caps_deliveries_per_minute() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        set(&path, |state| state["cursor"] = json!(0));
        let config = conversation_profile("", &path);
        let mut worker = Worker::default();
        worker.prepare(&config);
        let link = TicketLink { id: "00000000-0000-4000-8000-000000000001".into(), ..Default::default() };
        let open = TicketOp::RemoteOpen {
            home: home.into(),
            plan: "plan".into(),
            link,
            title: "Goal".into(),
            owners: vec!["human-1".into()],
            recover: false,
            reset: false,
        };
        worker.run(&config, open).unwrap();
        set(&path, |state| {
            state["steering"] = json!((1..=12).map(|n| json!({"id":format!("s-{n}"), "revision":n, "author_id":"human-1", "author_kind":"Human", "text":"hi"})).collect::<Vec<_>>());
        });
        let whoami = tickets::Whoami { id: "agent-1".into(), kind: "personalAgent".into(), ..Default::default() };
        let poll = |worker: &mut Worker| {
            let op = TicketOp::RemotePoll {
                home: home.into(),
                plan: "plan".into(),
                whoami: Box::new(whoami.clone()),
                owners: vec!["human-1".into()],
            };
            let TicketOutput::Remote(RemoteOutput::Polled { deliver, .. }) = worker.run(&config, op).unwrap() else {
                panic!("poll")
            };
            deliver
        };
        let first = poll(&mut worker);
        assert_eq!(first.len(), DELIVERIES_PER_MINUTE, "the rest wait in order");
        for (n, item) in first.into_iter().enumerate() {
            let outcome = if n == 0 { RemoteOutcome::Unavailable } else { RemoteOutcome::Deferred };
            let op = TicketOp::RemoteSettle { home: home.into(), plan: "plan".into(), item: Box::new(item), outcome };
            worker.run(&config, op).unwrap();
        }
        assert!(poll(&mut worker).is_empty(), "the minute's budget is spent");
        worker.handed.clear();
        for attempt in 2..=DELIVERY_ATTEMPTS {
            let items = poll(&mut worker);
            assert_eq!(items[0].id, "s-1", "attempt {attempt}");
            worker.handed.clear();
            for (n, item) in items.into_iter().enumerate() {
                let outcome = if n == 0 { RemoteOutcome::Unavailable } else { RemoteOutcome::Deferred };
                let op =
                    TicketOp::RemoteSettle { home: home.into(), plan: "plan".into(), item: Box::new(item), outcome };
                worker.run(&config, op).unwrap();
            }
        }
        let fake = server(&path);
        assert!(fake["acks"].as_array().is_none_or(|acks| acks.is_empty()), "never acknowledged");
        assert_eq!(fake["posts"].as_array().unwrap().len(), 1);
        assert_eq!(fake["posts"][0]["text"], UNDELIVERED_TEXT);
        assert_eq!(poll(&mut worker)[0].id, "s-2", "the given-up item is not offered again");
    }

    #[test]
    fn ticket_remote_outbox_worker_retries_without_another_job() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let config = remote_profile("", &path);
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        server["fail"] = json!(["conversation_post"]);
        std::fs::write(&path, server.to_string()).unwrap();
        let (done, results) = crate::host::Wake::default().channel();
        let jobs = spawn(done);
        let op = TicketOp::Outbox {
            home: dir.path().into(),
            plan: "test".into(),
            write: Some(Box::new(PendingWrite::new(
                "conversation_post",
                uuid::Uuid::new_v4().to_string(),
                Vars::new(&config.system).with("conversation", Some("conversation-1")).with("body", Some("Hello")),
            ))),
        };
        jobs.send(TicketJob { config: Arc::new(config), op, tag: 1 }).unwrap();
        let (_, result) = results.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(result.is_ok());
        let persisted = RemoteState::load(dir.path(), "test").unwrap();
        assert_eq!(persisted.outbox[0].attempts, 1);
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        server["fail"] = json!([]);
        std::fs::write(&path, server.to_string()).unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
        while !RemoteState::load(dir.path(), "test").unwrap().outbox.is_empty() {
            assert!(Instant::now() < deadline, "outbox worker did not retry");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(jobs);
    }

    #[test]
    fn ticket_whoami_with_and_without_owner_in_all_three_dialects() {
        for prefix in ["", "card_", "issue_"] {
            for owner in [false, true] {
                let dir = crate::test_support::tempdir().unwrap();
                let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
                let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                state["whoami"] = json!({"userId":"agent-1", "kind":if owner {"personalAgent"} else {"agent"}, "displayName":"Example agent"});
                if owner {
                    state["whoami"]["ownerUserId"] = json!("human-1");
                    state["whoami"]["ownerDisplayName"] = json!("Example human");
                }
                std::fs::write(&path, state.to_string()).unwrap();
                let config = remote_profile(prefix, &path);
                let mut worker = Worker::default();
                worker.prepare(&config);
                let result = worker.run(&config, TicketOp::Whoami);
                let TicketOutput::Whoami(identity) = worker.clean(result).unwrap() else { panic!("whoami") };
                assert_eq!(identity.id, "agent-1");
                assert_eq!(identity.name, "Example agent");
                assert_eq!(identity.owner_id.as_deref(), owner.then_some("human-1"));
                assert_eq!(identity.owner_name.as_deref(), owner.then_some("Example human"));
            }
        }
    }

    #[test]
    fn approval_posts_are_cleaned_before_persistence_and_transport() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let config = remote_profile("", &path);
        set(&path, |state| state["fail"] = json!(["conversation_post"]));
        let mut worker = Worker { cleaner: tickets::Cleaner::new(vec!["known-secret".into()]), ..Default::default() };
        let text = format!("known-secret https://example.test/?access_token=hidden-token {}", "é".repeat(3000));
        let vars = Vars::new(&config.system).with("conversation", Some("conversation-1")).with("body", Some(&text));
        worker
            .run(
                &config,
                TicketOp::Outbox {
                    home: dir.path().into(),
                    plan: "approval".into(),
                    write: Some(Box::new(PendingWrite::new("conversation_post", "approval-event".into(), vars))),
                },
            )
            .unwrap();
        let persisted = RemoteState::load(dir.path(), "approval").unwrap();
        let body = persisted.outbox[0].vars.get("body").unwrap().as_str().unwrap();
        assert!(!body.contains("known-secret") && !body.contains("hidden-token"));
        assert_eq!(body.chars().count(), 2000);
        let sent = server(&path);
        assert_eq!(sent["calls"][0]["arguments"]["text"], body);
        assert_eq!(persisted.outbox[0].nonce, "approval-event");
    }

    #[test]
    fn ticket_remote_outbox_dropped_reply_reuses_nonce_and_server_stores_one_post() {
        for prefix in ["", "card_", "issue_"] {
            let dir = crate::test_support::tempdir().unwrap();
            let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
            let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            state["drop_reply"] = json!([format!("{prefix}conversation_post")]);
            std::fs::write(&path, state.to_string()).unwrap();
            let config = remote_profile(prefix, &path);
            let nonce = uuid::Uuid::new_v4().to_string();
            let vars =
                Vars::new(&config.system).with("conversation", Some("conversation-1")).with("body", Some("Hello"));
            let mut worker = Worker::default();
            worker.prepare(&config);
            worker
                .run(
                    &config,
                    TicketOp::Outbox {
                        home: dir.path().into(),
                        plan: "test".into(),
                        write: Some(Box::new(PendingWrite::new("conversation_post", nonce.clone(), vars))),
                    },
                )
                .unwrap();
            let state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let calls = state["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["arguments"], calls[1]["arguments"]);
            assert_eq!(calls[0]["arguments"]["eventId"], nonce);
            assert_eq!(calls[0]["arguments"]["sequence"], 1);
            assert_eq!(state["posts"].as_array().unwrap().len(), 1);
            assert!(RemoteState::load(dir.path(), "test").unwrap().outbox.is_empty());
            assert_eq!(RemoteState::load(dir.path(), "test").unwrap().posted_today, 1);
        }
    }

    #[test]
    fn mirror_hundred_rapid_turns_are_capped_coalesced_and_rate_limited_with_status_preserved() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let mut config = conversation_profile("", &path);
        config.remote.min_interval_ms = 2000;
        let home = dir.path();
        let mut state = RemoteState::default();
        for turn in 0..100 {
            for kind in ["user", "assistant", "tool"] {
                let vars = Vars::new(&config.system)
                    .with("activity", Some(""))
                    .with("turn", Some(""))
                    .with("kind", Some("assistant"))
                    .with("conversation", Some("conversation-1"))
                    .with("kind", Some(kind))
                    .with("turn", Some(&format!("turn-{turn}")))
                    .with("body", Some(&format!("{kind} {turn}")));
                let mut write = PendingWrite::new("conversation_post", format!("{kind}-{turn}"), vars);
                write.mirror = true;
                write.tool = kind == "tool";
                state.queue(home, "burst", write).unwrap();
            }
            if turn % 10 == 0 {
                let vars = Vars::new(&config.system)
                    .with("activity", Some(""))
                    .with("turn", Some(""))
                    .with("kind", Some("assistant"))
                    .with("conversation", Some("conversation-1"))
                    .with("kind", Some("status"))
                    .with("body", Some("Needs input"));
                state
                    .queue(home, "burst", PendingWrite::new("conversation_post", format!("status-{turn}"), vars))
                    .unwrap();
            }
            assert!(state.outbox.iter().filter(|w| w.mirror).count() <= 50);
        }
        assert_eq!(state.outbox.iter().filter(|w| !w.mirror).count(), 10);
        assert!(
            state
                .outbox
                .iter()
                .any(|w| w.skipped_turns > 0
                    && w.vars.get("body").unwrap().as_str().unwrap().ends_with("turns skipped)"))
        );
        assert!(state.outbox.iter().filter(|w| w.tool).count() <= 1, "tool blocks are dropped first");
        let total = state.outbox.len();
        let mut worker = Worker::default();
        let now = 2 * 86_400_000;
        worker.drain_outbox_at(&config, home, "burst", now).unwrap();
        assert_eq!(server(&path)["posts"].as_array().unwrap().len(), 1);
        worker.drain_outbox_at(&config, home, "burst", now + 1999).unwrap();
        assert_eq!(server(&path)["posts"].as_array().unwrap().len(), 1);
        // A new worker honours the persisted spacing as well.
        worker = Worker::default();
        for n in 1..total {
            worker.drain_outbox_at(&config, home, "burst", now + n as u64 * 2000).unwrap();
            assert_eq!(server(&path)["posts"].as_array().unwrap().len(), n + 1);
        }
        let posted = server(&path);
        let posts = posted["posts"].as_array().unwrap();
        assert_eq!(posts.iter().filter(|p| p["kind"] == "status").count(), 10);
        assert!(RemoteState::load(home, "burst").unwrap().outbox.is_empty());
        let seqs: Vec<_> = posts.iter().map(|p| p["sequence"].as_u64().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn mirror_daily_pause_keeps_status_approval_plan_and_resets_next_utc_day() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let config = conversation_profile("", &path);
        let now = 2 * 86_400_000;
        let mut state = RemoteState { post_day: 2, posted_today: 1000, ..Default::default() };
        for kind in ["assistant", "status", "approval", "plan", "user"] {
            let vars = Vars::new(&config.system)
                .with("activity", Some(""))
                .with("turn", Some(""))
                .with("kind", Some("assistant"))
                .with("conversation", Some("conversation-1"))
                .with("kind", Some(kind))
                .with("body", Some(kind))
                .with("turn", Some(kind));
            let mut write = PendingWrite::new("conversation_post", kind.into(), vars);
            write.mirror = matches!(kind, "assistant" | "user");
            state.queue(dir.path(), "daily", write).unwrap();
        }
        let mut worker = Worker::default();
        let TicketOutput::Outbox(messages) = worker.drain_outbox_at(&config, dir.path(), "daily", now).unwrap() else {
            panic!()
        };
        assert_eq!(messages, ["Conversation mirror paused: 1000 posts today"]);
        let posted = server(&path);
        let kinds: Vec<_> = posted["posts"].as_array().unwrap().iter().map(|p| p["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["status", "approval", "plan"]);
        let vars = Vars::new(&config.system)
            .with("activity", Some(""))
            .with("turn", Some(""))
            .with("kind", Some("assistant"))
            .with("conversation", Some("conversation-1"))
            .with("body", Some("New day"));
        let mut write = PendingWrite::new("conversation_post", "next-day".into(), vars);
        write.mirror = true;
        state = RemoteState::load(dir.path(), "daily").unwrap();
        state.queue(dir.path(), "daily", write).unwrap();
        worker.drain_outbox_at(&config, dir.path(), "daily", now + 86_400_000).unwrap();
        state = RemoteState::load(dir.path(), "daily").unwrap();
        assert_eq!(state.posted_today, 1);
        assert!(!state.pause_shown);
        assert_eq!(server(&path)["posts"].as_array().unwrap().last().unwrap()["text"], "New day");
    }

    #[test]
    fn mirror_outbox_content_refusal_posts_one_placeholder() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let config = remote_profile("", &path);
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        server["refuse_text"] = json!("sensitive text");
        server["fail_message"] = json!("credential-shaped text refused");
        std::fs::write(&path, server.to_string()).unwrap();
        let mut worker = Worker::default();
        let result = worker
            .run(
                &config,
                TicketOp::Outbox {
                    home: dir.path().into(),
                    plan: "test".into(),
                    write: Some(Box::new(PendingWrite::new(
                        "conversation_post",
                        uuid::Uuid::new_v4().to_string(),
                        Vars::new(&config.system)
                            .with("conversation", Some("conversation-1"))
                            .with("body", Some("sensitive text")),
                    ))),
                },
            )
            .unwrap();
        let TicketOutput::Outbox(messages) = result else { panic!("outbox") };
        assert_eq!(messages.len(), 1);
        let server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(server["calls"].as_array().unwrap().len(), 2);
        assert_ne!(server["calls"][0]["arguments"]["eventId"], server["calls"][1]["arguments"]["eventId"]);
        assert_eq!(server["calls"][0]["arguments"]["sequence"], server["calls"][1]["arguments"]["sequence"]);
        assert_eq!(server["posts"].as_array().unwrap().len(), 1);
        assert_eq!(server["posts"][0]["text"], WITHHELD_TEXT);
        assert!(RemoteState::load(dir.path(), "test").unwrap().outbox.is_empty());
    }

    #[test]
    fn ticket_remote_outbox_restart_ack_refusal_and_exhaustion() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let config = remote_profile("", &path);
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        server["fail"] = json!(["conversation_post"]);
        std::fs::write(&path, server.to_string()).unwrap();
        let nonce = uuid::Uuid::new_v4().to_string();
        let vars = Vars::new(&config.system).with("conversation", Some("conversation-1")).with("body", Some("hello"));
        {
            let mut worker = Worker::default();
            worker
                .run(
                    &config,
                    TicketOp::Outbox {
                        home: home.into(),
                        plan: "test".into(),
                        write: Some(Box::new(PendingWrite::new("conversation_post", nonce.clone(), vars.clone()))),
                    },
                )
                .unwrap();
        }
        let mut persisted = RemoteState::load(home, "test").unwrap();
        assert_eq!(persisted.outbox[0].attempts, 1);
        assert_eq!(persisted.outbox[0].nonce, nonce);
        persisted.outbox[0].next_at = 0;
        persisted.save(home, "test").unwrap();
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        server["fail"] = json!([]);
        std::fs::write(&path, server.to_string()).unwrap();
        let mut worker = Worker::default();
        worker.run(&config, TicketOp::Outbox { home: home.into(), plan: "test".into(), write: None }).unwrap();
        worker
            .run(
                &config,
                TicketOp::Outbox {
                    home: home.into(),
                    plan: "test".into(),
                    write: Some(Box::new(PendingWrite::new(
                        "conversation_ack",
                        uuid::Uuid::new_v4().to_string(),
                        vars.clone().typed("ids", json!(["message-1", "message-2"])),
                    ))),
                },
            )
            .unwrap();
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(server["posts"][0]["eventId"], nonce);
        assert_eq!(server["acks"][0]["ids"], json!(["message-1", "message-2"]));
        server["fail"] = json!(["conversation_post"]);
        server["fail_message"] = json!("credential-shaped text refused");
        std::fs::write(&path, server.to_string()).unwrap();
        let TicketOutput::Outbox(messages) = worker
            .run(
                &config,
                TicketOp::Outbox {
                    home: home.into(),
                    plan: "test".into(),
                    write: Some(Box::new(PendingWrite::new(
                        "conversation_post",
                        uuid::Uuid::new_v4().to_string(),
                        vars.clone(),
                    ))),
                },
            )
            .unwrap()
        else {
            panic!("outbox")
        };
        assert_eq!(messages.len(), 2, "original and one refused replacement");
        assert!(RemoteState::load(home, "test").unwrap().outbox.is_empty());
        let mut server: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let calls = server["calls"].as_array().unwrap();
        assert_eq!(calls.last().unwrap()["arguments"]["text"], WITHHELD_TEXT);
        assert_ne!(calls[calls.len() - 1]["arguments"]["eventId"], calls[calls.len() - 2]["arguments"]["eventId"]);
        server["fail_message"] = json!("temporarily unavailable");
        std::fs::write(&path, server.to_string()).unwrap();
        let mut persisted = RemoteState::load(home, "test").unwrap();
        let mut write =
            PendingWrite::new("conversation_post", uuid::Uuid::new_v4().to_string(), vars.typed("seq", json!(99)));
        write.attempts = 9;
        persisted.enqueue(write).unwrap();
        persisted.save(home, "test").unwrap();
        let TicketOutput::Outbox(messages) = worker.drain_outbox(&config, home, "test").unwrap() else {
            panic!("outbox")
        };
        assert!(messages[0].contains("10 attempts"));
        assert!(RemoteState::load(home, "test").unwrap().outbox.is_empty());
    }

    #[test]
    fn nonce_writes_retry_only_connection_failures_and_retryable_http_statuses_once() {
        use crate::mcp_client::tests::{http_connections, respond};
        use std::os::fd::AsRawFd;
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Zero means reset the socket after reading the write, before replying.
        for (status, nonce, persistent, expected) in [
            (200, true, false, 1), // A malformed MCP reply is not a connection failure.
            (400, true, false, 1),
            (401, true, false, 1),
            (403, true, false, 1),
            (404, true, false, 1),
            (408, true, false, 1),
            (422, true, false, 1),
            (429, true, false, 2),
            (503, true, false, 2),
            (0, true, false, 2),
            (503, true, true, 2),
            (0, true, true, 2),
            (503, false, false, 1),
            (0, false, false, 1),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let (url, requests) = http_connections(move |body, stream| {
                let result = match body["method"].as_str().unwrap() {
                    "initialize" => json!({"protocolVersion":"2025-06-18", "capabilities":{}}),
                    "notifications/initialized" => {
                        respond(stream, "202 Accepted", &[], "").unwrap();
                        return;
                    }
                    "tools/call" => {
                        let attempt = seen.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 || persistent {
                            if status == 0 {
                                let linger = libc::linger { l_onoff: 1, l_linger: 0 };
                                // SAFETY: a live socket fd and a correctly sized linger value.
                                assert_eq!(
                                    unsafe {
                                        libc::setsockopt(
                                            stream.as_raw_fd(),
                                            libc::SOL_SOCKET,
                                            libc::SO_LINGER,
                                            &linger as *const _ as *const libc::c_void,
                                            std::mem::size_of_val(&linger) as libc::socklen_t,
                                        )
                                    },
                                    0
                                );
                            } else {
                                respond(stream, &format!("{status} Failed"), &[], "rejected").unwrap();
                            }
                            return;
                        }
                        json!({"content":[]})
                    }
                    _ => panic!("unexpected request"),
                };
                respond(
                    stream,
                    "200 OK",
                    &[("Content-Type", "application/json".into())],
                    &json!({"jsonrpc":"2.0", "id":body["id"], "result":result}).to_string(),
                )
                .unwrap();
            });
            let config: TicketConfig = serde_json::from_value(json!({
                "system":"test", "server":{"url":url},
                "operations":{"comment":{"tool":"note", "arguments":if nonce {
                    json!({"body":"{body}", "nonce":"{nonce}"})
                } else { json!({"body":"{body}"}) }}}
            }))
            .unwrap();
            let mut worker = Worker::default();
            let result = worker.run(&config, TicketOp::Comment(TicketLink::default(), "Hello".into()));
            assert_eq!(result.is_ok(), expected == 2 && !persistent, "status {status}, nonce {nonce}");
            assert_eq!(calls.load(Ordering::SeqCst), expected, "status {status}, nonce {nonce}");
            let attempts: Vec<Value> = requests
                .try_iter()
                .filter_map(|request| {
                    let (_, body) = request.split_once("\n{")?;
                    let body: Value = serde_json::from_str(&format!("{{{body}")).unwrap();
                    (body["method"] == "tools/call").then(|| body["params"]["arguments"].clone())
                })
                .collect();
            assert_eq!(attempts.len(), expected);
            if expected == 2 {
                assert_eq!(attempts[0], attempts[1]);
                assert_eq!(uuid::Uuid::parse_str(attempts[0]["nonce"].as_str().unwrap()).unwrap().get_version_num(), 4);
            }
        }
    }

    #[test]
    fn all_composed_comment_types_are_bounded_and_mentions_are_neutralised_on_the_wire() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        use atomics_core::plan::Task;
        use atomics_core::tickets::{Author, agent_comment, chief_comment, dispatch_comment, report_comment};
        let dir = crate::test_support::tempdir().unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        let path = fake_state(dir.path(), json!([{"id":id, "key":"DEMO-1", "comments":[]}]));
        let mut config = example_profile("meaning-example", &path);
        config.max_comment_chars = 240;
        let original = format!("@max-agent @all @équipe user@example.com {}", "🦀é界".repeat(200));
        let task = Task { id: "a".into(), title: "@team".into(), brief: original.clone(), ..Default::default() };
        let author = Author::Agent("Test".into());
        let bodies = [
            dispatch_comment(&Author::Atomics, &task, "Test"),
            report_comment(&author, &task, "done", &original),
            chief_comment(&Author::Chief("Test".into()), &original),
            agent_comment(&author, &task, &original),
        ];
        let mut worker = Worker::default();
        for body in &bodies {
            worker
                .run(&config, TicketOp::Comment(TicketLink { id: id.into(), ..Default::default() }, body.clone()))
                .unwrap();
        }
        let state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let comments = state["tickets"][0]["comments"].as_array().unwrap();
        assert_eq!(comments.len(), bodies.len());
        for body in comments {
            let body = body.as_str().unwrap();
            assert_eq!(body.chars().count(), 240);
            assert!(body.ends_with(" … (truncated)"));
            assert!(
                body.contains("@\u{2060}max-agent @\u{2060}all @\u{2060}équipe user@\u{2060}example.com"),
                "{body}"
            );
            assert!(!body.contains("@team"));
        }
        assert_eq!(task.brief, original);
        assert_eq!(task.title, "@team");
    }

    #[test]
    fn ticket_example_writes_use_distinct_nonces_and_retry_without_duplicate_effects_in_both_dialects() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        for structured in [false, true] {
            let dir = crate::test_support::tempdir().unwrap();
            let id = "00000000-0000-4000-8000-000000000001";
            let path = fake_state(
                dir.path(),
                json!([{
                    "id":id, "key":"DEMO-1", "title":"Write", "state":"Backlog", "comments":[]
                }]),
            );
            let mut config = example_profile("meaning-example", &path);
            // Creation is deliberately absent from the shipped profile. Exercise the
            // generic nonce support using another fixture operation.
            config.operations.create = Some(serde_json::from_value(json!({
                "tool":"create_ticket", "arguments":{"title":"{title}", "body":"{body}", "idempotencyKey":"{nonce}"},
                "result":{"id":"ticket.id", "key":"ticket.key"}
            })).unwrap());
            let link = TicketLink { id: id.into(), key: Some("DEMO-1".into()), ..Default::default() };
            let mut worker = Worker::default();
            worker.prepare(&config);
            let running = config.states.running.clone().unwrap();
            let mut nonces = std::collections::HashSet::new();
            for name in ["update_state", "comment", "create"] {
                let tool = config.operation(name).unwrap().tool.clone();
                for _ in 0..2 {
                    let mut state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                    state["structured_results"] = json!(structured);
                    state["drop_reply"] = json!([tool]);
                    std::fs::write(&path, state.to_string()).unwrap();
                    let op = match name {
                        "update_state" => {
                            TicketOp::UpdateState(link.clone(), running.clone(), uuid::Uuid::new_v4().to_string())
                        }
                        "comment" => TicketOp::Comment(link.clone(), "Literal {nonce} in a report".into()),
                        _ => TicketOp::Create { board: None, title: "Same title".into(), body: "Same body".into() },
                    };
                    let result = worker.run(&config, op).unwrap();
                    if name != "create" {
                        let TicketOutput::Pushed(Some(Ok(fetched))) = result else { panic!("reread after {name}") };
                        assert_eq!(fetched.state.as_deref(), Some(running.as_str()));
                    } else {
                        let TicketOutput::Ticket(created) = result else { panic!("created ticket") };
                        assert!(!created.id.is_empty());
                    }
                    let state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                    let calls: Vec<_> =
                        state["calls"].as_array().unwrap().iter().filter(|call| call["tool"] == tool).collect();
                    let attempts = &calls[calls.len() - 2..];
                    assert_eq!(attempts[0]["arguments"], attempts[1]["arguments"]);
                    let nonce = attempts[0]["arguments"]["idempotencyKey"].as_str().unwrap();
                    assert_eq!(uuid::Uuid::parse_str(nonce).unwrap().get_version_num(), 4);
                    assert!(nonces.insert(nonce.to_string()), "fresh nonce for each logical write");
                    if name != "create" {
                        assert_eq!(attempts[0]["arguments"]["ticketId"], id);
                    }
                }
            }
            let state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(state["tickets"].as_array().unwrap().len(), 3, "each create applied once");
            assert_eq!(
                state["tickets"][0]["comments"],
                json!(["Literal {nonce} in a report", "Literal {nonce} in a report"])
            );
            assert_eq!(state["idempotency"].as_object().unwrap().len(), 6);
            // A repeated key with changed arguments is refused by the fixture.
            let nonce = state["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|call| call["tool"] == "add_ticket_comment")
                .unwrap()["arguments"]["idempotencyKey"]
                .as_str()
                .unwrap();
            let error = worker
                .call(
                    &config,
                    "comment",
                    &Vars::new(&config.system)
                        .ticket(&link)
                        .with("nonce", Some(nonce))
                        .with("body", Some("Changed body")),
                )
                .unwrap_err();
            assert!(error.contains("different arguments"), "{error}");
            let state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(state["tickets"][0]["comments"].as_array().unwrap().len(), 2);

            let calls = state["calls"].as_array().unwrap().len();
            for id in ["DEMO-1", "malformed-uuid"] {
                let invalid = TicketLink { id: id.into(), ..Default::default() };
                for op in [
                    TicketOp::UpdateState(invalid.clone(), running.clone(), uuid::Uuid::new_v4().to_string()),
                    TicketOp::Comment(invalid, "Must not be sent".into()),
                ] {
                    let Err(error) = worker.run(&config, op) else { panic!("UUID validation") };
                    assert!(error.contains("{uuid}"), "{error}");
                }
            }
            // Also guard programmatically assembled read configs, before connecting.
            config.operations.get.as_mut().unwrap().arguments.insert("invalid".into(), json!("{nonce}"));
            let Err(error) = worker.run(&config, TicketOp::Get(link)) else { panic!("read nonce rejected") };
            assert!(error.contains("{nonce}") && error.contains("operations.get"), "{error}");
            let state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(state["calls"].as_array().unwrap().len(), calls);
        }
    }

    fn card_profile(dialect: &str, path: &std::path::Path) -> TicketConfig {
        let (get, search, semantic, children, related, args, items, row, one, body, comments, links) = match dialect {
            "card" => (
                "card_get",
                "card_search",
                "card_semantic_search",
                "card_children",
                "card_related",
                json!({"card":"{id}"}),
                "cards",
                json!({"id":"ref", "key":"code", "title":"name", "state":"column.title", "url":"href", "score":"rank", "snippet":"excerpt", "assignee":"owner", "parent":"parent", "kind":"type"}),
                "card",
                "card.body",
                "card.notes",
                "card.development",
            ),
            "issue" => (
                "issue_read",
                "issue_search",
                "issue_semantic_search",
                "issue_children",
                "issue_related",
                json!({"id":"{key}"}),
                "issues",
                json!({"id":"uuid", "key":"id", "title":"title", "state":"status", "url":"url", "score":"score", "snippet":"snippet", "assignee":"assignee", "parent":"parent", "kind":"kind"}),
                "",
                "description",
                "comments",
                "links",
            ),
            _ => (
                "get_ticket",
                "find_keyword_tickets",
                "find_tickets",
                "get_children",
                "find_related_tickets",
                json!({"ticket":"{uuid}"}),
                "result.tickets",
                json!({"id":"id", "key":"number", "title":"subject", "state":"status", "url":"", "score":"score", "snippet":"snippet", "assignee":"assignee", "parent":"parentTicketId", "kind":"type"}),
                "ticket",
                "ticket.description",
                "comments",
                "ticket.developmentLinks",
            ),
        };
        let operation =
            |tool: &str, arguments: Value| json!({"tool":tool, "arguments":arguments, "items":items, "result":row});
        let mut read =
            operation(get, if dialect == "find" { json!({"ticketNumber":"{key_number}"}) } else { args.clone() });
        // The full result uses the same base fields, nested for two dialects.
        let paths = read["result"].as_object_mut().unwrap();
        for path in paths.values_mut() {
            let name = path.as_str().unwrap();
            if !one.is_empty() && !name.is_empty() {
                *path = json!(format!("{one}.{name}"));
            }
        }
        if dialect == "find" {
            read["result"]["kind"] = json!("ticket.ticketTypeKey");
            read["result"]["url"] = json!("url");
        }
        read["body"] = json!(body);
        read["comments"] = json!({"items":comments, "author":"authorDisplayName", "text":"body", "at":"createdAtUtc", "kind":"authorIdentityKind"});
        read["links"] = json!({"items":links, "label":"title", "url":"pullRequestUrl", "status":"pullRequestStatus", "ci":"continuousIntegrationStatus"});
        let mut get_mapping = read.clone();
        get_mapping["body"] = json!("");
        get_mapping["comments"] = json!({});
        get_mapping["links"] = json!({});
        serde_json::from_value(json!({
            "system":dialect,
            "server":{"command":[crate::mcp_client::tests::fake_server(), path], "env":{"API_KEY":"test-secret-7c2d"}},
            "link_url":"https://tickets.example/?selected={uuid}",
            "operations": {
                "search":operation(search, json!({"query":"{query}"})),
                "semantic_search":operation(semantic, if dialect == "find" {json!({"about":"{query}", "take":25})} else {json!({"query":"{query}"})}),
                "children":operation(children, args.clone()), "related":operation(related, args),
                "read":read, "get":get_mapping
            }
        })).unwrap()
    }

    #[test]
    fn ticket_ops_example_server_search_children_related_and_read_across_three_dialects() {
        for dialect in ["card", "issue", "find"] {
            let dir = crate::test_support::tempdir().unwrap();
            // A missing state file seeds the demo epic and its four children.
            let path = dir.path().join("demo.json");
            let config = card_profile(dialect, &path);
            let mut worker = Worker::default();
            worker.prepare(&config);
            let mut run = |config: &TicketConfig, op| {
                worker.prepare(config);
                let result = worker.run(config, op);
                worker.clean(result).unwrap()
            };
            let epic = TicketLink {
                system: dialect.into(),
                id: "00000000-0000-4000-8000-000000000001".into(),
                key: Some("DEMO-1".into()),
                ..Default::default()
            };
            for semantic in [false, true] {
                let TicketOutput::Cards(cards) =
                    run(&config, TicketOp::Search { query: "browser OAuth".into(), semantic })
                else {
                    panic!("cards")
                };
                assert_eq!(cards.len(), 1, "{dialect}");
                assert_eq!(cards[0].link.label(), "DEMO-2");
                assert!(cards[0].score.unwrap() > 0.7);
                assert_eq!(cards[0].assignee, Some(Some("Demo User".into())));
                assert_eq!(cards[0].kind.as_deref(), Some("task"));
                assert!(cards[0].link.url.is_some());
            }
            let TicketOutput::Cards(children) = run(&config, TicketOp::Children(epic.clone())) else {
                panic!("children")
            };
            assert_eq!(children.len(), 4);
            assert!(children.iter().all(|card| card.parent.as_deref() == Some(&epic.id)));
            let TicketOutput::Cards(related) = run(&config, TicketOp::Related(children[0].link.clone())) else {
                panic!("related")
            };
            assert_eq!(related.len(), 3);
            let TicketOutput::Card(read) = run(&config, TicketOp::ReadCard(children[0].link.clone())) else {
                panic!("read")
            };
            assert!(read.body.unwrap().contains("## Goal"));
            assert_eq!(read.comments[0].kind.as_deref(), Some("Agent"));
            assert_eq!(read.links[0].ci.as_deref(), Some("Passed"));
            let mut fallback = config.clone();
            fallback.operations.read = None;
            let TicketOutput::Card(read) = run(&fallback, TicketOp::ReadCard(epic.clone())) else { panic!("fallback") };
            assert!(read.body.unwrap().contains("## Acceptance"));
            assert!(!read.fields.is_empty());
            // List falls back to the configured direct-children operation.
            let TicketOutput::Tickets(list) = run(&config, TicketOp::List(epic.clone())) else { panic!("list") };
            assert_eq!(list.len(), 4);
            let mut state: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let literal = "  {id} \"quoted\" {query}  ";
            state["tickets"][0]["snippet"] = json!(format!("{literal} test-secret-7c2d"));
            state["tickets"][0]["description"] = json!(format!("test-secret-7c2d {}", "long body ".repeat(8000)));
            state["tickets"][0]["comments"][0]["body"] = json!("test-secret-7c2d in a comment");
            std::fs::write(&path, state.to_string()).unwrap();
            for semantic in [false, true] {
                let TicketOutput::Cards(cards) = run(&config, TicketOp::Search { query: literal.into(), semantic })
                else {
                    panic!("literal")
                };
                assert_eq!(cards.len(), 1);
                assert!(cards[0].snippet.as_ref().unwrap().contains("***"));
                assert!(!serde_json::to_string(&cards).unwrap().contains("test-secret-7c2d"));
                let calls: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                let args = &calls["calls"].as_array().unwrap().last().unwrap()["arguments"];
                assert_eq!(args.get("about").unwrap_or(&args["query"]), literal);
            }
            let TicketOutput::Card(read) = run(&config, TicketOp::ReadCard(epic)) else { panic!("cleaned read") };
            assert!(read.body.as_ref().unwrap().len() <= tickets::MAX_BODY_BYTES);
            assert_eq!(read.comments[0].text, "*** in a comment");
            assert!(!serde_json::to_string(&read).unwrap().contains("test-secret-7c2d"));
        }
    }

    /// A streamable HTTP MCP server whose every tool call fails, echoing the bearer token it
    /// was called with and the one its previous call had. Each call's token goes to the receiver.
    fn echo_server() -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (tx, tokens) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut previous: Option<String> = None;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut length, mut token) = (0, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization: bearer ") {
                        token = line["authorization: bearer ".len()..].trim().to_string();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let answer = match body["method"].as_str().unwrap_or_default() {
                    "initialize" => Some(json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
                    "tools/call" => {
                        let _ = tx.send(token.clone());
                        let mut text = format!("credential {token} rejected");
                        if let Some(previous) = previous.replace(token.clone()).filter(|p| *p != token) {
                            text.push_str(&format!("; last time {previous}"));
                        }
                        Some(json!({"isError": true, "content": [{"type": "text", "text": text}]}))
                    }
                    _ => None,
                };
                let response = match answer {
                    Some(result) => {
                        let text = json!({"jsonrpc": "2.0", "id": body["id"], "result": result}).to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                            text.len()
                        )
                    }
                    None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, tokens)
    }

    #[test]
    fn a_job_against_a_server_that_is_not_signed_in_fails_without_opening_a_browser() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, keychain, _) = crate::credentials::tests::machine(false);
        let marker = dir.path().join("browser-opened");
        let browser = dir.path().join("browser");
        std::fs::write(&browser, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
        let oauth = OAuth::new(keychain, crate::connections::Browser::at(browser.display().to_string()));
        let config: TicketConfig = toml::from_str(
            "system = \"echo\"\nserver = { url = \"http://127.0.0.1:9/mcp\", oauth = true }\n\
             [operations.comment]\ntool = \"note\"\narguments = { body = \"{body}\" }",
        )
        .unwrap();
        let (done, results) = crate::host::Wake::default().channel();
        let jobs = spawn_with(done, oauth);
        for tag in 0..2 {
            let op = TicketOp::Comment(TicketLink { id: "1".into(), ..TicketLink::default() }, "Hi".into());
            jobs.send(TicketJob { config: Arc::new(config.clone()), op, tag }).unwrap();
            let (_, result) = results.recv_timeout(Duration::from_secs(10)).unwrap();
            let error = result.err().unwrap();
            assert!(crate::mcp_client::is_sign_in_required(&error), "{error}");
        }
        assert!(!marker.exists(), "the worker never opens the browser");
    }

    #[test]
    fn a_rotated_token_reconnects_and_is_cleaned_together_with_the_old_one() {
        const CHILD: &str = "ATOMICS_TICKET_WORKER_TOKEN_CHILD";
        const NAME: &str = "tickets::tests::a_rotated_token_reconnects_and_is_cleaned_together_with_the_old_one";
        if std::env::var_os(CHILD).is_none() {
            // The test changes an environment variable, so it runs alone in a process of its own.
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env(CHILD, "1"),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // The token variable as written in a config, padded or not: the name is the same.
        for (round, name) in ["ECHO_TEST_TOKEN", " ECHO_TEST_TOKEN "].into_iter().enumerate() {
            let old = format!("oldkey-7c1d9e{round}");
            let new = format!("newkey-2b3c4d{round}");
            let (url, tokens) = echo_server();
            let config: TicketConfig = toml::from_str(&format!(
                "system = \"echo\"\nserver = {{ url = \"{url}\", token_env = \"{name}\" }}\n\
                 [operations.comment]\ntool = \"note\"\narguments = {{ body = \"{{body}}\" }}"
            ))
            .unwrap();
            let config = Arc::new(config);
            let (done, results) = crate::host::Wake::default().channel();
            let jobs = spawn(done);
            let comment = |n: u32| {
                let op = TicketOp::Comment(TicketLink { id: "1".into(), ..TicketLink::default() }, "Hi".into());
                jobs.send(TicketJob { config: config.clone(), op, tag: n }).unwrap();
                let (tag, result) = results.recv_timeout(Duration::from_secs(10)).unwrap();
                assert_eq!(tag, n);
                result.err().unwrap()
            };
            // SAFETY: this process runs only this test, and the worker reads the variable only
            // while a job runs; each job is awaited before the variable changes.
            unsafe { std::env::set_var("ECHO_TEST_TOKEN", &old) };
            assert_eq!(comment(1), "note failed: credential *** rejected", "{name:?}");
            assert_eq!(tokens.recv().unwrap(), old);
            unsafe { std::env::set_var("ECHO_TEST_TOKEN", &new) };
            let rotated = comment(2);
            assert_eq!(tokens.recv().unwrap(), new, "{name:?}: the connection authenticates with the new token");
            // The new connection's server echoes both tokens: the old one is cleaned too.
            assert_eq!(rotated, "note failed: credential *** rejected; last time ***", "{name:?}");
        }
    }

    fn outbox(
        worker: &mut Worker,
        config: &TicketConfig,
        home: &std::path::Path,
        write: Option<PendingWrite>,
    ) -> Vec<String> {
        let op = TicketOp::Outbox { home: home.into(), plan: "lanes".into(), write: write.map(Box::new) };
        let TicketOutput::Outbox(messages) = worker.run(config, op).unwrap() else { panic!("outbox") };
        messages
    }

    fn post(config: &TicketConfig, body: &str) -> PendingWrite {
        let vars = Vars::new(&config.system)
            .with("conversation", Some("conversation-1"))
            .with("kind", Some("status"))
            .with("activity", Some(""))
            .with("turn", Some("turn"))
            .with("body", Some(body));
        PendingWrite::new("conversation_post", uuid::Uuid::new_v4().to_string(), vars)
    }

    #[test]
    fn remote_outbox_skips_blank_posts_drops_final_refusals_and_acks_in_their_own_lane() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let config = remote_profile("", &path);
        let mut worker = Worker::default();
        worker.prepare(&config);
        let calls =
            |tool: &str| server(&path)["calls"].as_array().unwrap().iter().filter(|c| c["tool"] == tool).count();
        // Blank after cleaning: never queued, never sent.
        assert!(outbox(&mut worker, &config, home, Some(post(&config, " \n\t "))).is_empty());
        assert_eq!(calls("conversation_post"), 0);
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty());
        // The reference server's final refusals are dropped after one call, never retried.
        for message in [
            "The item conversation has ended; open a new one to continue.",
            "Another event already holds this sequence; omit the sequence to have the next one assigned.",
            "This event id was already used for other content.",
            "The event kind is not one of the listed kinds.",
            "The conversation does not exist, or belongs to another agent.",
        ] {
            set(&path, |state| {
                state["fail"] = json!(["conversation_post"]);
                state["fail_message"] = json!(message);
            });
            let before = calls("conversation_post");
            let messages = outbox(&mut worker, &config, home, Some(post(&config, "hello")));
            assert_eq!(calls("conversation_post"), before + 1, "{message}");
            assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty(), "{message}");
            assert!(messages.len() == 1 && messages[0].contains(message), "{messages:?}");
        }
        // A post waiting for its retry doesn't hold an acknowledgement back.
        set(&path, |state| state["fail_message"] = json!("the server is busy"));
        outbox(&mut worker, &config, home, Some(post(&config, "retried later")));
        let vars = Vars::new(&config.system).with("conversation", Some("conversation-1")).typed("ids", json!(["s-1"]));
        let ack = PendingWrite::new("conversation_ack", uuid::Uuid::new_v4().to_string(), vars);
        assert!(outbox(&mut worker, &config, home, Some(ack)).is_empty());
        assert_eq!(server(&path)["acks"][0]["ids"], json!(["s-1"]));
        let state = RemoteState::load(home, "lanes").unwrap();
        assert_eq!(state.outbox.len(), 1);
        assert_eq!((state.outbox[0].op.as_str(), state.outbox[0].attempts), ("conversation_post", 1));
        // A mapped error code decides before the messages do.
        let mut coded = config.clone();
        coded.remote.errors =
            tickets::RemoteErrors { code: "code".into(), ended: vec!["ended".into()], ..Default::default() };
        set(&path, |state| state["fail_message"] = json!(r#"{"code":"ended","message":"gone"}"#));
        let mut pending = RemoteState::load(home, "lanes").unwrap();
        pending.outbox[0].next_at = 0;
        pending.save(home, "lanes").unwrap();
        let messages = outbox(&mut worker, &coded, home, None);
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty(), "{messages:?}");
    }

    #[test]
    fn remote_posts_are_capped_by_utf16_units_after_trimming_near_the_limit() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let config = remote_profile("", &path);
        let mut worker = Worker::default();
        worker.prepare(&config);
        let emoji = "\u{1f600}";
        // Exactly 2000 units once trimmed, then just over: the fake server refuses anything longer.
        for (count, mirror) in [(1000, false), (1001, false), (1000, true), (1001, true), (1500, true)] {
            let mut write = post(&config, &format!("  \n{}  \n", emoji.repeat(count)));
            write.mirror = mirror;
            let messages = outbox(&mut worker, &config, home, Some(write));
            assert!(messages.is_empty(), "{count} {mirror}: {messages:?}");
            let state = server(&path);
            let text = state["posts"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().to_string();
            assert!(text.encode_utf16().count() <= 2000, "{count} {mirror}");
            assert_eq!(text.trim(), text);
            let kept = text.chars().take_while(|c| c.to_string() == emoji).count();
            match (count, mirror) {
                (1000, _) => assert_eq!(text, emoji.repeat(1000)),
                (_, false) => assert_eq!(text, emoji.repeat(1000)),
                _ => assert!(text.ends_with(&format!("(… {} more characters in ATOMICS)", count - kept)), "{text}"),
            }
        }
    }

    #[test]
    fn remote_open_starts_at_the_servers_next_sequence_when_the_profile_maps_it() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        set(&path, |state| {
            state["next_sequence"] = json!(true);
            state["sequences"] = json!({"conversation-1": {"1":"a", "2":"b", "3":"c"}});
        });
        let open = |plan: &str| TicketOp::RemoteOpen {
            home: home.into(),
            plan: plan.into(),
            link: TicketLink { id: "00000000-0000-4000-8000-000000000001".into(), ..Default::default() },
            title: "Goal".into(),
            owners: vec!["human-1".into()],
            recover: false,
            reset: false,
        };
        let mut config = conversation_profile("", &path);
        let mut worker = Worker::default();
        worker.prepare(&config);
        // Unmapped: the path stays optional and the plan starts at 1.
        worker.run(&config, open("unmapped")).unwrap();
        assert_eq!(RemoteState::load(home, "unmapped").unwrap().next_seq, 1);
        config.operations.conversation_open.as_mut().unwrap().result.next_sequence = "nextSequence".into();
        let TicketOutput::Remote(RemoteOutput::Opened { conversation, .. }) =
            worker.run(&config, open("mapped")).unwrap()
        else {
            panic!("opened")
        };
        assert_eq!(conversation.next_seq, Some(4));
        assert_eq!(RemoteState::load(home, "mapped").unwrap().next_seq, 4);
        let op =
            TicketOp::Outbox { home: home.into(), plan: "mapped".into(), write: Some(Box::new(post(&config, "next"))) };
        worker.run(&config, op).unwrap();
        assert_eq!(server(&path)["posts"].as_array().unwrap().last().unwrap()["sequence"], 4);
        // A sequence another plan already used is refused, and the refusal is final.
        let op = TicketOp::Outbox {
            home: home.into(),
            plan: "unmapped".into(),
            write: Some(Box::new(post(&config, "stale"))),
        };
        let TicketOutput::Outbox(messages) = worker.run(&config, op).unwrap() else { panic!("outbox") };
        assert!(messages[0].contains("Another event already holds this sequence"), "{messages:?}");
        assert!(RemoteState::load(home, "unmapped").unwrap().outbox.is_empty());
    }

    fn reference_codes() -> tickets::RemoteErrors {
        tickets::RemoteErrors {
            code: "code".into(),
            content_refused: vec!["content_refused".into()],
            ended: vec!["conversation_ended".into()],
            conflict: vec!["sequence_conflict".into()],
            invalid: vec!["validation".into()],
            unreadable: vec!["item_unreadable".into()],
        }
    }

    #[test]
    fn remote_coded_refusals_are_classified_and_an_unsequenced_conflict_is_retried() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let mut config = remote_profile("", &path);
        config.remote.errors = reference_codes();
        let mut worker = Worker::default();
        worker.prepare(&config);
        let posts = || {
            server(&path)["calls"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["tool"] == "conversation_post")
                .map(|c| c["arguments"].clone())
                .collect::<Vec<_>>()
        };
        // Only the code says the text was refused: the post is replaced once by the withheld note.
        set(&path, |state| {
            state["fail"] = json!(["conversation_post"]);
            state["fail_message"] = json!("nothing a message fragment matches");
            state["fail_code"] = json!("content_refused");
        });
        let messages = outbox(&mut worker, &config, home, Some(post(&config, "secret")));
        assert!(messages.iter().any(|m| m.contains("Remote text withheld")), "{messages:?}");
        let sent = posts();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["text"], WITHHELD_TEXT);
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty());
        // A conflict on a post that sent a sequence is final.
        set(&path, |state| {
            state["fail_message"] =
                json!("Another event already holds this sequence; omit the sequence to have the next one assigned.");
            state["fail_code"] = json!("sequence_conflict");
        });
        let messages = outbox(&mut worker, &config, home, Some(post(&config, "sequenced")));
        assert!(messages[0].starts_with("Remote write dropped"), "{messages:?}");
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty());
        // Without a sequence the server assigns it, and its conflict is a race to retry.
        let mut unsequenced = config.clone();
        unsequenced.operations.conversation_post = Some(
            serde_json::from_value(json!({"tool":"conversation_post",
                "arguments":{"conversation":"{conversation}", "eventId":"{nonce}", "text":"{body}"}}))
            .unwrap(),
        );
        set(&path, |state| {
            state["fail_message"] = json!("Another event took this event's id or sequence at the same time; retry it.");
        });
        let messages = outbox(&mut worker, &unsequenced, home, Some(post(&config, "raced")));
        assert!(messages.is_empty(), "{messages:?}");
        let pending = RemoteState::load(home, "lanes").unwrap();
        assert_eq!((pending.outbox.len(), pending.outbox[0].attempts), (1, 1), "kept for a retry");
        let nonce = pending.outbox[0].nonce.clone();
        set(&path, |state| state["fail"] = json!([]));
        let mut due = pending;
        due.outbox[0].next_at = 0;
        due.save(home, "lanes").unwrap();
        assert!(outbox(&mut worker, &unsequenced, home, None).is_empty());
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty());
        let last = posts().pop().unwrap();
        assert_eq!((last["eventId"].as_str(), last.get("sequence")), (Some(nonce.as_str()), None), "the same event");
    }

    #[test]
    fn remote_a_post_refused_as_ended_drops_the_conversations_writes_and_says_so() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let mut config = conversation_profile("", &path);
        config.remote.errors = reference_codes();
        let mut worker = Worker::default();
        worker.prepare(&config);
        let open = TicketOp::RemoteOpen {
            home: home.into(),
            plan: "lanes".into(),
            link: TicketLink { id: "00000000-0000-4000-8000-000000000001".into(), ..Default::default() },
            title: "Goal".into(),
            owners: vec!["human-1".into()],
            recover: false,
            reset: false,
        };
        worker.run(&config, open).unwrap();
        // A write to a conversation the plan no longer follows is only dropped.
        set(&path, |state| {
            state["fail"] = json!(["conversation_post"]);
            state["fail_message"] = json!("The item conversation has ended; open a new one to continue.");
            state["fail_code"] = json!("conversation_ended");
        });
        let mut old = post(&config, "to the old one");
        old.vars = old.vars.with("conversation", Some("conversation-0"));
        let messages = outbox(&mut worker, &config, home, Some(old));
        assert!(messages[0].starts_with("Remote write dropped"), "{messages:?}");
        // The plan's own conversation ended: every write queued for it goes, and the app is told.
        set(&path, |state| {
            state["fail"] = json!([]);
            state["ended"] = json!(true);
        });
        let mut state = RemoteState::load(home, "lanes").unwrap();
        let vars = Vars::new(&config.system).with("conversation", Some("conversation-1")).typed("ids", json!(["s-1"]));
        let mut ack = PendingWrite::new("conversation_ack", uuid::Uuid::new_v4().to_string(), vars);
        ack.next_at = u64::MAX;
        state.queue(home, "lanes", ack).unwrap();
        let messages = outbox(&mut worker, &config, home, Some(post(&config, "after the end")));
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].starts_with(CONVERSATION_ENDED), "{messages:?}");
        assert!(messages[0].contains("has ended"), "{messages:?}");
        assert!(RemoteState::load(home, "lanes").unwrap().outbox.is_empty());
    }

    #[test]
    fn remote_steering_is_neither_pasted_twice_nor_skipped_when_plans_take_a_conversation_over() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let path = crate::mcp_client::tests::fake_state(home, json!([]));
        let item = |id: &str, revision: u64| json!({"id":id, "revision":revision, "author_id":"human-1", "author_kind":"Human", "author":"Example human", "text":id, "at":"now"});
        set(&path, |state| state["steering"] = json!([item("old", 1)]));
        let config = conversation_profile("", &path);
        let mut worker = Worker::default();
        worker.prepare(&config);
        let whoami = tickets::Whoami { id: "agent-1".into(), kind: "personalAgent".into(), ..Default::default() };
        let open = |worker: &mut Worker, plan: &str| {
            let op = TicketOp::RemoteOpen {
                home: home.into(),
                plan: plan.into(),
                link: TicketLink { id: "00000000-0000-4000-8000-000000000001".into(), ..Default::default() },
                title: "Goal".into(),
                owners: vec!["human-1".into()],
                recover: false,
                reset: false,
            };
            worker.run(&config, op).unwrap();
        };
        let deliver = |worker: &mut Worker, plan: &str| -> Vec<String> {
            let poll = TicketOp::RemotePoll {
                home: home.into(),
                plan: plan.into(),
                whoami: Box::new(whoami.clone()),
                owners: vec!["human-1".into()],
            };
            let TicketOutput::Remote(RemoteOutput::Polled { deliver, .. }) = worker.run(&config, poll).unwrap() else {
                panic!("poll")
            };
            let ids = deliver.iter().map(|item| item.id.clone()).collect();
            for item in deliver {
                let settle = TicketOp::RemoteSettle {
                    home: home.into(),
                    plan: plan.into(),
                    item: Box::new(item),
                    outcome: RemoteOutcome::Delivered,
                };
                worker.run(&config, settle).unwrap();
            }
            ids
        };
        let add = |id: &str, revision: u64| {
            set(&path, |state| state["steering"].as_array_mut().unwrap().push(item(id, revision)))
        };
        open(&mut worker, "older");
        add("s-2", 2);
        assert_eq!(deliver(&mut worker, "older"), ["s-2"]);
        // Written before the newer plan opens, after the older one's cursor: the newer plan
        // starts where the older stopped, not at the server's newest revision.
        add("s-3", 3);
        open(&mut worker, "newer");
        assert_eq!(deliver(&mut worker, "newer"), ["s-3"], "s-2 not again, s-3 not skipped");
        // Taken back, the older plan skips what the newer one handled.
        add("s-4", 4);
        open(&mut worker, "older");
        let older = RemoteState::load(home, "older").unwrap();
        assert_eq!(older.cursor, Some(json!(3)));
        assert!(older.seen.contains(&"s-3".to_string()));
        assert_eq!(deliver(&mut worker, "older"), ["s-4"], "s-3 not again, s-4 not skipped");
        // And over to the newer plan once more.
        add("s-5", 5);
        open(&mut worker, "newer");
        assert_eq!(deliver(&mut worker, "newer"), ["s-5"]);
        let pasted: Vec<_> =
            server(&path)["acks"].as_array().unwrap().iter().map(|ack| ack["ids"][0].clone()).collect();
        assert_eq!(pasted, [json!("s-2"), json!("s-3"), json!("s-4"), json!("s-5")], "each acknowledged once");
    }
}

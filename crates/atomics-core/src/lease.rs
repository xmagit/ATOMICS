//! Typing into terminals over `atomics.v1`: the controller lease, its wire form and the reference
//! client ([`Typist`]). Browser-safe: plain data and pure functions.
//!
//! One client at a time holds a terminal's lease (`lease_acquire`; another holder is displaced
//! only with `takeover`). The host answers with a `lease_state` that carries the lease's fencing
//! token, and tells every client that follows the screen who holds it. `terminal_input` and
//! `terminal_resize` name that token; input is numbered from 1 within each lease and the host
//! applies it only in order. A lease ends when it is released, taken over, expires without input
//! or a renew, when its connection ends, or when the desktop types; input naming an ended lease is
//! refused (`fenced`) and never applied, so late input cannot land and nothing typed is replayed
//! on a later connection.
//!
//! Resizing is the holder's only, and only when it asks: holding the lease never resizes. A size
//! a holder sets lasts as long as its lease; then the terminal returns to the host's own size.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::screen::{MAX_COLS, MAX_ROWS};
use crate::wire::{ErrorCode, Message, Seq};

/// The most text one `terminal_input` carries; a longer paste is sent as consecutive inputs.
/// [`Typist`] keeps each one's JSON-escaped length within it too, so its frame always fits.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// After sending this much input [`Typist`] adds a `lease_renew`: its answer says what was applied,
/// so the input kept for [`Typist::refused`] stays bounded.
pub const RENEW_EVERY_BYTES: usize = 1 << 20;
/// The largest cell, in pixels, a client may report with a size.
pub const MAX_CELL_PIXELS: u16 = 512;

/// A terminal grid a lease holder asks for. Cell pixel sizes are what programs read with the
/// window size; absent, the host keeps its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_height: Option<u16>,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, cell_width: None, cell_height: None }
    }

    /// Why the host would refuse this size, if it would.
    pub fn invalid(&self) -> Option<&'static str> {
        let cell = |p: Option<u16>| p.is_some_and(|p| p == 0 || p > MAX_CELL_PIXELS);
        if self.cols < 2 || self.rows < 1 || self.cols > MAX_COLS || self.rows > MAX_ROWS {
            return Some("terminal dimensions are out of range");
        }
        (cell(self.cell_width) || cell(self.cell_height)).then_some("cell pixel sizes are out of range")
    }
}

/// Why a lease last changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseReason {
    /// A client took a free lease.
    Granted,
    /// A client took the lease from another (a visible takeover).
    TakenOver,
    Released,
    /// No input, resize or renew within the lease's time.
    Expired,
    /// The holder's connection ended, or it stopped following the screen.
    Disconnected,
    /// Someone typed on the desktop, which takes the terminal back.
    Desktop,
    /// A reason from a newer host. Minor versions may add reasons; an older client still reads the
    /// lease state.
    #[serde(other)]
    Unknown,
}

/// Who holds one terminal's lease, as the host tells one client. The token, the next input number
/// and the time-to-live are only told to the holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseState {
    pub session: String,
    /// Who holds the lease: a paired device's name (what it was paired under), else the
    /// holder's `client_id`; absent when nobody holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// Whether the holder is the client this is sent to.
    #[serde(default)]
    pub yours: bool,
    /// The fencing token to put in input, resizes, renews and the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<Seq>,
    /// The sequence number the next input must carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Seq>,
    /// How long the lease lasts without input, a resize or a renew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u32>,
    /// Whether the holder's own size is in force (viewers keep scaling the canonical grid).
    #[serde(default)]
    pub resized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<LeaseReason>,
}

/// Why the host would refuse this input, if it would.
pub(crate) fn invalid_input(data: &str) -> Option<&'static str> {
    if data.is_empty() {
        return Some("input is empty");
    }
    (data.len() > MAX_INPUT_BYTES).then_some("input is over the size limit; send it as consecutive inputs")
}

/// A client's side of one terminal's lease: the reference model for typing.
///
/// It keeps the fencing token while the host says this client holds the lease, numbers input from
/// 1 within each lease, splits long input at character boundaries so that each piece is at most
/// [`MAX_INPUT_BYTES`] once JSON-escaped, and forgets the lease on a disconnect, so input typed
/// before or during an outage is never sent on the next connection.
///
/// It keeps what it sent until it knows the host applied it, so that input refused with
/// `slow_client` can be typed again ([`refused`](Self::refused)). A `lease_state` with `next` for
/// the same lease, such as the answer to a renew, says what was applied; so does that refusal.
/// After every [`RENEW_EVERY_BYTES`] of input it asks with a renew.
#[derive(Debug, Clone, Default)]
pub struct Typist {
    pub session: String,
    lease: Option<Seq>,
    next: u64,
    /// Pieces sent under the lease and not known to be applied, oldest first.
    sent: VecDeque<(u64, String)>,
    /// Bytes sent since the last renew.
    unrenewed: usize,
    /// The last lease state the host sent for this terminal, until this client lets go of the lease: from then
    /// on nobody holds it, as far as this client can tell, whatever the host has yet to read.
    pub state: Option<LeaseState>,
    /// The lease this client let go of last.
    released: Option<Seq>,
}

impl Typist {
    pub fn new(session: impl Into<String>) -> Self {
        Self { session: session.into(), ..Self::default() }
    }

    /// Ask for the lease; `takeover` displaces another holder and is for an explicit user action.
    pub fn acquire(&self, takeover: bool) -> Message {
        Message::LeaseAcquire { session: self.session.clone(), takeover }
    }

    /// Follow what the host says. A new token restarts numbering where the host expects it;
    /// losing the lease drops the token, so nothing more is sent under it. The same token keeps
    /// the local numbering, which input in flight may have taken past the host's `next`.
    pub fn apply(&mut self, state: &LeaseState) {
        if state.session != self.session || self.out_of_date(state) {
            return;
        }
        match (state.yours, state.lease) {
            (true, Some(token)) if self.lease != Some(token) => {
                self.lease = Some(token);
                self.next = state.next.map_or(1, |n| n.0);
                self.sent.clear();
            }
            (true, Some(_)) => {
                if let Some(next) = state.next {
                    self.applied_before(next.0);
                }
            }
            _ => {
                self.lease = None;
                self.sent.clear();
            }
        }
        self.state = Some(state.clone());
    }

    /// The host refused input `seq` with `code`. On `slow_client` the program had not read the
    /// earlier input: everything before `seq` was applied and nothing from it on, so numbering
    /// rewinds to `seq` and the text from that piece on comes back, to type again (before anything
    /// typed since) once the program reads; its echo shows when. Pieces already sent behind it meet
    /// `out_of_order`, which the host does not count against the client; this answers `None` to
    /// those, and to every other refusal.
    pub fn refused(&mut self, seq: Seq, code: ErrorCode) -> Option<String> {
        if code != ErrorCode::SlowClient {
            return None;
        }
        let at = self.sent.iter().position(|(sent, _)| *sent == seq.0)?;
        self.sent.drain(..at);
        self.next = seq.0;
        Some(self.sent.drain(..).map(|(_, data)| data).collect())
    }

    /// The host says the lease's next input is `next`: everything numbered before it was applied.
    fn applied_before(&mut self, next: u64) {
        while self.sent.front().is_some_and(|(seq, _)| *seq < next) {
            self.sent.pop_front();
        }
    }

    pub fn may_type(&self) -> bool {
        self.lease.is_some()
    }

    /// The frames that type `data`, in order; none without the lease. Each carries at most
    /// [`MAX_INPUT_BYTES`] of escaped JSON: a control character takes six bytes in a frame
    /// (`\u001b`), so 64 KiB of them would not fit in one. A renew follows once
    /// [`RENEW_EVERY_BYTES`] have been sent since the last one.
    pub fn input(&mut self, data: &str) -> Vec<Message> {
        let Some(lease) = self.lease else { return Vec::new() };
        let mut out = Vec::new();
        let (mut start, mut encoded) = (0, 0);
        for (at, c) in data.char_indices() {
            let len = escaped_len(c);
            if encoded + len > MAX_INPUT_BYTES {
                out.push(self.typed(lease, &data[start..at]));
                (start, encoded) = (at, 0);
            }
            encoded += len;
        }
        if start < data.len() {
            out.push(self.typed(lease, &data[start..]));
        }
        if self.unrenewed >= RENEW_EVERY_BYTES {
            self.unrenewed = 0;
            out.extend(self.renew());
        }
        out
    }

    fn typed(&mut self, lease: Seq, data: &str) -> Message {
        let seq = Seq(self.next);
        self.next += 1;
        self.unrenewed += data.len();
        self.sent.push_back((seq.0, data.to_string()));
        Message::TerminalInput { session: self.session.clone(), lease, seq, data: data.to_string() }
    }

    /// Resize to `size` ("fit to this screen"), or back to the host's own size with `None`.
    pub fn resize(&self, size: Option<GridSize>) -> Option<Message> {
        Some(Message::TerminalResize { session: self.session.clone(), lease: self.lease?, size })
    }

    pub fn renew(&self) -> Option<Message> {
        Some(Message::LeaseRenew { session: self.session.clone(), lease: self.lease? })
    }

    pub fn release(&mut self) -> Option<Message> {
        let lease = self.lease.take()?;
        self.sent.clear();
        self.state = None;
        self.released = Some(lease);
        Some(Message::LeaseRelease { session: self.session.clone(), lease })
    }

    /// Whether `state` gives this client the lease it has let go of: sent before the host read the release, with
    /// the answer to that still to come. Tokens never come back, so it cannot be a new grant.
    pub fn out_of_date(&self, state: &LeaseState) -> bool {
        state.session == self.session && state.yours && state.lease.is_some() && state.lease == self.released
    }

    /// The connection ended. The host ended the lease with it; forget it here too.
    pub fn disconnected(&mut self) {
        self.lease = None;
        self.sent.clear();
        self.state = None;
        self.released = None;
    }
}

/// How many bytes `c` takes in a JSON string as serde_json writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        '\0'..='\u{1f}' => 6,
        c => c.len_utf8(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(token: u64, next: u64) -> LeaseState {
        LeaseState {
            session: "s".into(),
            holder: Some("phone".into()),
            yours: true,
            lease: Some(Seq(token)),
            next: Some(Seq(next)),
            ttl_ms: Some(30_000),
            resized: false,
            reason: Some(LeaseReason::Granted),
        }
    }

    fn sent(messages: &[Message]) -> Vec<(u64, u64, String)> {
        messages
            .iter()
            .map(|m| match m {
                Message::TerminalInput { lease, seq, data, .. } => (lease.0, seq.0, data.clone()),
                other => panic!("not input: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_typist_types_only_while_it_holds_the_lease_and_numbers_input_per_lease() {
        let mut typist = Typist::new("s");
        assert!(typist.input("ls\r").is_empty(), "no lease, no input");
        assert!(typist.resize(Some(GridSize::new(40, 20))).is_none());
        typist.apply(&granted(7, 1));
        assert_eq!(sent(&typist.input("l")), vec![(7, 1, "l".into())]);
        assert_eq!(sent(&typist.input("s\r")), vec![(7, 2, "s\r".into())]);
        // A renew reply for the same lease keeps the local numbering.
        typist.apply(&granted(7, 2));
        assert_eq!(sent(&typist.input("x")), vec![(7, 3, "x".into())]);

        // Another client took over: the token is gone.
        let lost = LeaseState { yours: false, lease: None, next: None, holder: Some("desk".into()), ..granted(7, 1) };
        typist.apply(&lost);
        assert!(!typist.may_type() && typist.input("y").is_empty());

        // A new lease starts again at 1 under its own token.
        typist.apply(&granted(9, 1));
        assert_eq!(sent(&typist.input("z")), vec![(9, 1, "z".into())]);
        // Another terminal's state does not touch this one.
        typist.apply(&LeaseState { session: "other".into(), ..lost });
        assert!(typist.may_type());
    }

    #[test]
    fn letting_go_shows_nobody_holding_it_until_the_host_says_otherwise() {
        let mut typist = Typist::new("s");
        typist.apply(&LeaseState { holder: Some("phone".into()), ..granted(7, 1) });
        assert_eq!(typist.state.as_ref().and_then(|s| s.holder.as_deref()), Some("phone"));

        // The frame the host reads is what it always was.
        let release = typist.release().unwrap();
        assert!(matches!(&release, Message::LeaseRelease { session, lease } if session == "s" && lease.0 == 7));
        assert!(!typist.may_type() && typist.state.is_none());
        assert!(typist.release().is_none(), "there is nothing more to let go of");

        // What the host said under that lease before it read the release changes nothing.
        let earlier = LeaseState { holder: Some("phone".into()), ..granted(7, 2) };
        assert!(typist.out_of_date(&earlier));
        typist.apply(&earlier);
        assert!(!typist.may_type() && typist.state.is_none() && typist.input("x").is_empty());
        assert!(!typist.out_of_date(&LeaseState { session: "other".into(), ..earlier }));

        // Its answer, someone else's lease and a new grant all count.
        let released = LeaseState {
            yours: false,
            lease: None,
            next: None,
            ttl_ms: None,
            holder: None,
            reason: Some(LeaseReason::Released),
            ..granted(7, 1)
        };
        typist.apply(&released);
        assert_eq!(typist.state.as_ref().map(|s| s.holder.clone()), Some(None));
        typist.apply(&LeaseState { holder: Some("desk".into()), ..released });
        assert_eq!(typist.state.as_ref().and_then(|s| s.holder.as_deref()), Some("desk"));
        typist.apply(&granted(8, 1));
        assert_eq!(sent(&typist.input("z")), vec![(8, 1, "z".into())]);
    }

    #[test]
    fn a_disconnect_forgets_the_lease_so_nothing_is_replayed() {
        let mut typist = Typist::new("s");
        typist.apply(&granted(3, 1));
        let _ = typist.input("echo hi");
        typist.disconnected();
        assert!(typist.input("\r").is_empty());
        assert!(typist.renew().is_none() && typist.release().is_none());
    }

    #[test]
    fn input_refused_as_slow_client_comes_back_to_be_typed_again_from_the_refused_piece() {
        let mut typist = Typist::new("s");
        typist.apply(&granted(4, 1));
        for text in ["ab", "cd", "ef"] {
            let _ = typist.input(text);
        }
        // The host applied 1 and refused 2; 3 was already on its way and met out_of_order.
        assert_eq!(typist.refused(Seq(2), ErrorCode::SlowClient).as_deref(), Some("cdef"));
        assert_eq!(typist.refused(Seq(3), ErrorCode::OutOfOrder), None);
        // Typed again once the program reads, the text takes the refused numbers.
        assert_eq!(sent(&typist.input("cdef")), vec![(4, 2, "cdef".into())]);
        // A renew's answer lets go of what was applied: nothing before it comes back.
        typist.apply(&granted(4, 3));
        assert_eq!(typist.refused(Seq(2), ErrorCode::SlowClient), None);
        assert_eq!(sent(&typist.input("g")), vec![(4, 3, "g".into())]);
        assert_eq!(typist.refused(Seq(3), ErrorCode::Fenced), None, "only slow_client hands input back");
        assert_eq!(typist.refused(Seq(3), ErrorCode::SlowClient).as_deref(), Some("g"));
        // A lease that ended keeps nothing to hand back.
        let _ = typist.input("h");
        typist.apply(&LeaseState { yours: false, lease: None, next: None, ..granted(4, 1) });
        assert_eq!(typist.refused(Seq(3), ErrorCode::SlowClient), None);
    }

    #[test]
    fn a_typist_renews_after_each_mebibyte_so_what_it_keeps_stays_bounded() {
        let mut typist = Typist::new("s");
        typist.apply(&granted(1, 1));
        let frames = typist.input(&"a".repeat(RENEW_EVERY_BYTES));
        assert_eq!(frames.len(), RENEW_EVERY_BYTES / MAX_INPUT_BYTES + 1);
        assert!(matches!(frames.last(), Some(Message::LeaseRenew { lease: Seq(1), .. })), "{:?}", frames.last());
        assert_eq!(sent(&typist.input("b")), vec![(1, 17, "b".into())], "no renew until the next mebibyte");
        // The renew's answer says the first 16 pieces were applied.
        typist.apply(&granted(1, 17));
        assert_eq!(typist.sent.len(), 1);
        assert_eq!(typist.refused(Seq(17), ErrorCode::SlowClient).as_deref(), Some("b"));
    }

    #[test]
    fn long_input_is_split_at_character_boundaries_in_order() {
        let mut typist = Typist::new("s");
        typist.apply(&granted(1, 1));
        let text = "é".repeat(MAX_INPUT_BYTES); // two bytes each
        let frames = sent(&typist.input(&text));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|(_, _, d)| d.len() <= MAX_INPUT_BYTES && invalid_input(d).is_none()));
        assert_eq!(frames.iter().map(|(_, s, _)| *s).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(frames.iter().map(|(_, _, d)| d.as_str()).collect::<String>(), text);
    }

    #[test]
    fn input_is_split_by_its_escaped_length_so_every_frame_fits() {
        let mut typist = Typist::new("s");
        typist.apply(&granted(1, 1));
        // Escape sequences, quotes and backslashes take two to six bytes once encoded.
        let text = "\u{1b}[A\"\\\u{7}é".repeat(MAX_INPUT_BYTES / 4);
        let messages = typist.input(&text);
        assert!(messages.len() > 1);
        for message in &messages {
            let Message::TerminalInput { data, .. } = message else { panic!("not input") };
            assert!(invalid_input(data).is_none());
            let escaped = serde_json::to_string(data).unwrap().len() - 2;
            assert_eq!(escaped, data.chars().map(escaped_len).sum::<usize>(), "escaped_len matches serde_json");
            assert!(escaped <= MAX_INPUT_BYTES);
            let frame = crate::wire::encode(&crate::wire::Envelope::new(message.clone())).expect("fits in a frame");
            assert!(crate::wire::decode(&frame).is_ok());
        }
        assert_eq!(sent(&messages).into_iter().map(|(_, _, d)| d).collect::<String>(), text);
    }

    #[test]
    fn an_unknown_reason_from_a_newer_host_is_read() {
        let state: LeaseState = serde_json::from_str(r#"{"session":"s","reason":"maintenance"}"#).unwrap();
        assert_eq!(state.reason, Some(LeaseReason::Unknown));
    }

    #[test]
    fn sizes_and_input_have_bounds() {
        assert!(GridSize::new(80, 24).invalid().is_none());
        assert!(
            GridSize { cell_width: Some(9), cell_height: Some(18), ..GridSize::new(MAX_COLS, MAX_ROWS) }
                .invalid()
                .is_none()
        );
        for bad in [
            GridSize::new(1, 24),
            GridSize::new(80, 0),
            GridSize::new(MAX_COLS + 1, 24),
            GridSize::new(80, MAX_ROWS + 1),
            GridSize { cell_width: Some(0), ..GridSize::new(80, 24) },
            GridSize { cell_height: Some(MAX_CELL_PIXELS + 1), ..GridSize::new(80, 24) },
        ] {
            assert!(bad.invalid().is_some(), "{bad:?}");
        }
        assert!(invalid_input("").is_some());
        assert!(invalid_input(&"a".repeat(MAX_INPUT_BYTES)).is_none());
        assert!(invalid_input(&"a".repeat(MAX_INPUT_BYTES + 1)).is_some());
    }
}

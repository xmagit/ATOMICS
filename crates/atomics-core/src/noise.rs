//! End-to-end encryption between a host and a browser over a rendezvous channel
//! (docs/daemon.md, "Rendezvous"). Browser-safe:
//! the web client and the host run this same code, so the two ends cannot drift apart.
//!
//! - **Pairing**, `Noise_IKpsk2_25519_ChaChaPoly_SHA256`: the browser has the host's static key
//!   and a single-use psk from the pairing link ([`Pairing`]) and sends its own static key
//!   encrypted. Both ends then show the grouped [`Established::code`] from the handshake hash,
//!   and the host adds the device only once its user says the two codes match.
//! - **Sessions**, `Noise_IK_25519_ChaChaPoly_SHA256`: a paired browser with its stored key. The
//!   host looks that key up in its allowlist once the browser's first transport message decrypts,
//!   and refuses one it does not hold inside the channel ([`wire::not_paired`]).
//! - The prologue binds `atomics.v1`, the protocol major and the host id ([`prologue`]): a
//!   handshake meant for another host or major fails.
//!
//! **Frames.** A channel's first frame is `[kind u8][Noise message 1]`, [`FIRST_FRAME_BYTES`]
//! long; until it has read that, the host parses nothing else. The host answers with message 2
//! ([`SECOND_FRAME_BYTES`]). From then on each frame is one Noise transport message, at most
//! [`MAX_MESSAGE_BYTES`]; an `atomics.v1` frame larger than one message travels in several
//! ([`Channel::seal`]). Each message's plaintext starts with a flags byte: [`MORE`] (the frame goes
//! on in the next message) and [`REKEY`] (the sender changes its key after this message; the
//! reader does the same after reading it). Noise's counter nonces make a replayed, reordered,
//! dropped or forged message fail, and any failure ends the channel.
//!
//! **A browser pairing** (C5 calls exactly this):
//! 1. `let device = Secret::generate()?` and `let pairing = Pairing::read(fragment)?`.
//! 2. `let (initiator, first) = Initiator::pair(&pairing, &device)?`; send `first`.
//! 3. On the host's answer, `let mut done = initiator.finish(&answer)?` and show `done.code`.
//! 4. Send [`PairRequest`] (JSON) through `done.channel.seal`, and read the host's [`PairAnswer`]
//!    through `done.channel.open`. If paired, keep `device`, `pairing.host_id` and
//!    `pairing.host_key`; the same channel then carries `atomics.v1`, starting with `hello`.
//!
//! **A later session**: `Initiator::connect(host_id, &host_key, &device)`, send the frame, then
//! `finish` the answer and send `hello` through the channel. A host that does not hold the key
//! answers all the same, and once `hello` decrypts it sends [`wire::not_paired`] through the
//! channel and closes it with `tunnel::close::NOT_PAIRED`. Only that sealed frame says the host
//! itself refused the device; the bare close code comes through the broker unauthenticated.
//!
//! **Which failure is which.** A [`Initiator::finish`] that fails means the answer did not come from
//! the host key this browser holds (or, pairing, from a host holding the psk). A transport message
//! that fails to open ends the channel but says nothing about the keys: it was cut, dropped or
//! forged on the way, and a new channel may well succeed.

use std::fmt;

use curve25519_dalek::montgomery::MontgomeryPoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::wire::{self, Role};

pub const PAIR_PROTOCOL: &str = "Noise_IKpsk2_25519_ChaChaPoly_SHA256";
pub const SESSION_PROTOCOL: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
/// An X25519 key or a psk.
pub const KEY_BYTES: usize = 32;
const TAG_BYTES: usize = 16;
/// Message 1 of IK and IKpsk2 with no payload: `e`, the encrypted `s`, and the payload's tag.
const MESSAGE_1_BYTES: usize = KEY_BYTES + (KEY_BYTES + TAG_BYTES) + TAG_BYTES;
/// A channel's first frame: the kind byte and message 1.
pub const FIRST_FRAME_BYTES: usize = 1 + MESSAGE_1_BYTES;
/// The host's answer, message 2 with no payload: `e` and the payload's tag.
pub const SECOND_FRAME_BYTES: usize = KEY_BYTES + TAG_BYTES;
/// Noise's limit on one message.
pub const MAX_MESSAGE_BYTES: usize = 65535;
/// The most frame bytes one transport message carries, after its flags byte and tag.
const MAX_CHUNK_BYTES: usize = MAX_MESSAGE_BYTES - TAG_BYTES - 1;
/// Flags: the frame continues in the next message.
pub const MORE: u8 = 1;
/// Flags: the sender rekeys after this message, and the reader after reading it.
pub const REKEY: u8 = 2;
/// Each direction rekeys after this many frame bytes; hosts also ask every hour
/// ([`Channel::rekey_soon`]).
pub const REKEY_BYTES: u64 = 1 << 30;
/// The pairing link's version, its first field.
pub const LINK_VERSION: &str = "1";
/// How long a pairing link may be used.
pub const PAIRING_LIFETIME_SECS: u64 = 300;
/// What a pairing URL template puts the link's fragment in place of.
pub const PAIRING_PLACEHOLDER: &str = "{pairing}";
/// The longest device name a host keeps, in characters.
pub const MAX_DEVICE_NAME: usize = 64;
/// The longest [`PairRequest`] frame a host reads.
pub const MAX_PAIR_REQUEST_BYTES: usize = 1024;
const CODE_CONTEXT: &[u8] = b"atomics-pair-code-v1";
const PROLOGUE_CONTEXT: &str = "atomics-noise-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseError {
    /// Not a frame of this protocol: a wrong size, kind or flags, or a malformed key or link.
    Malformed,
    /// Authentication failed: a wrong key, psk, host id or major, or a replayed, reordered or
    /// forged message.
    Refused,
    /// A frame over `wire::MAX_FRAME_BYTES`.
    TooBig,
    /// No system randomness, or a nonce used up (never in practice).
    Unavailable,
}

impl fmt::Display for NoiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoiseError::Malformed => "not a frame of the encrypted channel",
            NoiseError::Refused => "the encrypted channel failed to authenticate",
            NoiseError::TooBig => "a frame over the size limit",
            NoiseError::Unavailable => "no randomness, or the channel's nonces ran out",
        })
    }
}

impl std::error::Error for NoiseError {}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url (RFC 4648 §5).
fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let at = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (at(0) << 16) | (at(1) << 8) | at(2);
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

/// 32 bytes from exactly 43 characters of unpadded base64url, written canonically (the two spare
/// bits of the last character clear), so one key has exactly one text.
fn decode_key(text: &str) -> Option<Zeroizing<[u8; KEY_BYTES]>> {
    if text.len() != 43 {
        return None;
    }
    let mut out = Zeroizing::new([0u8; KEY_BYTES]);
    let (mut acc, mut bits, mut at) = (0u32, 0u32, 0usize);
    for c in text.bytes() {
        acc = (acc << 6) | ALPHABET.iter().position(|&a| a == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out[at] = (acc >> bits) as u8;
            at += 1;
            acc &= (1 << bits) - 1;
        }
    }
    (at == KEY_BYTES && acc == 0).then_some(out)
}

/// An X25519 public key; its text is 43 characters of unpadded base64url.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; KEY_BYTES]);

impl PublicKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    pub fn text(&self) -> String {
        encode(&self.0)
    }

    /// Only the canonical 43-character text.
    pub fn parse(text: &str) -> Option<Self> {
        decode_key(text).map(|bytes| Self(*bytes))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.text())
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl Serialize for PublicKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        PublicKey::parse(&text).ok_or_else(|| serde::de::Error::custom("not a 43-character base64url key"))
    }
}

/// A private X25519 key or a psk: wiped when dropped, and never shown by `Debug`.
#[derive(Clone)]
pub struct Secret(Zeroizing<[u8; KEY_BYTES]>);

impl Secret {
    /// 32 fresh bytes from the system (the browser's `crypto.getRandomValues` in wasm).
    pub fn generate() -> Result<Self, NoiseError> {
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        getrandom::fill(&mut bytes[..]).map_err(|_| NoiseError::Unavailable)?;
        Ok(Self(bytes))
    }

    /// A stored key. The caller should wipe its own copy.
    pub fn from_bytes(bytes: &[u8; KEY_BYTES]) -> Self {
        Self(Zeroizing::new(*bytes))
    }

    /// The bytes, for whoever stores this key.
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    /// The public key of this private key (X25519, clamped as RFC 7748 says).
    pub fn public(&self) -> PublicKey {
        PublicKey(MontgomeryPoint::mul_base_clamped(*self.0).0)
    }

    fn text(&self) -> Zeroizing<String> {
        Zeroizing::new(encode(&self.0[..]))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(hidden)")
    }
}

/// A host id as a link and the prologue carry it: what a service hands out (a GUID, say).
pub fn host_id_ok(host_id: &str) -> bool {
    (1..=64).contains(&host_id.len()) && host_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// What both ends mix in before the handshake: this protocol, `atomics.v1`, its major and the
/// host id, one per line.
pub fn prologue(host_id: &str) -> Vec<u8> {
    format!("{PROLOGUE_CONTEXT}\n{}\n{}\n{host_id}", wire::SUBPROTOCOL, wire::VERSION.major).into_bytes()
}

/// Why a pairing link was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkProblem {
    Malformed,
    /// A later link version than this code reads.
    Newer,
}

/// What a pairing link carries after its `#`: `<v>.<host_id>.<host key>.<psk>`. Its psk is a
/// secret, so `Debug` names the host alone.
#[derive(Clone)]
pub struct Pairing {
    pub host_id: String,
    pub host_key: PublicKey,
    psk: Secret,
}

impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing").field("host_id", &self.host_id).finish_non_exhaustive()
    }
}

impl Pairing {
    /// A new link for this host, with a fresh psk.
    pub fn new(host_id: &str, host_key: PublicKey) -> Result<Self, NoiseError> {
        if !host_id_ok(host_id) {
            return Err(NoiseError::Malformed);
        }
        Ok(Self { host_id: host_id.to_string(), host_key, psk: Secret::generate()? })
    }

    pub fn psk(&self) -> &Secret {
        &self.psk
    }

    /// The link's fragment, with or without its `#`, exactly as written: nothing trimmed or decoded.
    pub fn read(fragment: &str) -> Result<Self, LinkProblem> {
        let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
        let mut parts = fragment.split('.');
        let version = parts.next().unwrap_or_default();
        if version != LINK_VERSION {
            let later = !version.is_empty()
                && version.len() <= 3
                && !version.starts_with('0')
                && version.bytes().all(|b| b.is_ascii_digit());
            return Err(if later { LinkProblem::Newer } else { LinkProblem::Malformed });
        }
        let (Some(host_id), Some(key), Some(psk), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(LinkProblem::Malformed);
        };
        let (Some(host_key), Some(psk)) = (PublicKey::parse(key), decode_key(psk)) else {
            return Err(LinkProblem::Malformed);
        };
        if !host_id_ok(host_id) {
            return Err(LinkProblem::Malformed);
        }
        Ok(Self { host_id: host_id.to_string(), host_key, psk: Secret(psk) })
    }

    /// `<v>.<host_id>.<host key>.<psk>`.
    pub fn fragment(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{LINK_VERSION}.{}.{}.{}", self.host_id, self.host_key.text(), *self.psk.text()))
    }

    /// The link: `template` with the fragment in place of [`PAIRING_PLACEHOLDER`]
    /// (see [`pair_url_problem`]).
    pub fn link(&self, template: &str) -> Result<Zeroizing<String>, String> {
        if let Some(problem) = pair_url_problem(template) {
            return Err(problem);
        }
        Ok(Zeroizing::new(template.trim().replace(PAIRING_PLACEHOLDER, &self.fragment())))
    }
}

/// Why `template` cannot make pairing links, if it cannot. It must be an `https://` URL (or
/// `http://` to this machine) that ends in `#{pairing}`: the fragment is the one part of a URL a
/// browser never sends to a server, and the link's psk must never reach one.
pub fn pair_url_problem(template: &str) -> Option<String> {
    let template = template.trim();
    if template.is_empty() {
        return Some("no pair_url is set in [remote.rendezvous]".into());
    }
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|prefix| template.strip_prefix(prefix).is_some_and(|rest| rest.starts_with([':', '/', '#'])));
    if !template.starts_with("https://") && !local {
        return Some("pair_url must be an https:// address".into());
    }
    let address = template.strip_suffix(&format!("#{PAIRING_PLACEHOLDER}"));
    match address {
        Some(address) if !address.contains('#') && !address.contains(PAIRING_PLACEHOLDER) => None,
        _ => Some(format!(
            "pair_url must end in #{PAIRING_PLACEHOLDER}, once: the pairing secret goes only in the fragment"
        )),
    }
}

/// Which handshake a channel's first frame starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pair,
    Session,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Pair => 1,
            Kind::Session => 2,
        }
    }

    fn protocol(self) -> &'static str {
        match self {
            Kind::Pair => PAIR_PROTOCOL,
            Kind::Session => SESSION_PROTOCOL,
        }
    }

    /// The kind of a channel's first frame, which must be exactly [`FIRST_FRAME_BYTES`] long.
    pub fn of(first: &[u8]) -> Result<Self, NoiseError> {
        match (first.len(), first.first()) {
            (FIRST_FRAME_BYTES, Some(1)) => Ok(Kind::Pair),
            (FIRST_FRAME_BYTES, Some(2)) => Ok(Kind::Session),
            _ => Err(NoiseError::Malformed),
        }
    }
}

/// A handshake state for `kind`, as both ends build it.
fn handshake(
    kind: Kind,
    initiator: bool,
    host_id: &str,
    local: &Secret,
    remote: Option<&PublicKey>,
    psk: Option<&Secret>,
    ephemeral: Option<&[u8; KEY_BYTES]>,
) -> Result<snow::HandshakeState, NoiseError> {
    if !host_id_ok(host_id) {
        return Err(NoiseError::Malformed);
    }
    let prologue = prologue(host_id);
    let params: snow::params::NoiseParams = kind.protocol().parse().map_err(|_| NoiseError::Unavailable)?;
    let fail = |_| NoiseError::Unavailable;
    let mut builder =
        snow::Builder::new(params).prologue(&prologue).map_err(fail)?.local_private_key(&local.0[..]).map_err(fail)?;
    if let Some(remote) = remote {
        builder = builder.remote_public_key(&remote.0).map_err(fail)?;
    }
    match (kind, psk) {
        (Kind::Pair, Some(psk)) => builder = builder.psk(2, &psk.0).map_err(fail)?,
        (Kind::Session, None) => {}
        _ => return Err(NoiseError::Malformed),
    }
    // Only the tests' vectors fix it; a real handshake always draws a fresh one.
    if let Some(ephemeral) = ephemeral {
        builder = builder.fixed_ephemeral_key_for_testing_only(ephemeral);
    }
    let state = if initiator { builder.build_initiator() } else { builder.build_responder() };
    state.map_err(fail)
}

/// A 96-bit fingerprint of the Noise transcript, displayed on both devices.
fn code(hash: &[u8]) -> String {
    let digest = Sha256::new().chain_update(CODE_CONTEXT).chain_update(hash).finalize();
    digest[..12].iter().map(|byte| format!("{byte:02X}")).collect()
}

/// A finished handshake.
pub struct Established {
    pub channel: Channel,
    /// A fingerprint of the handshake hash. Both ends of a pairing show it: equal codes mean the
    /// browser and the host took part in the same handshake, so no one else answered the link.
    pub code: String,
}

impl fmt::Debug for Established {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Established").field("code", &self.code).finish_non_exhaustive()
    }
}

fn established(state: snow::HandshakeState) -> Result<Established, NoiseError> {
    if !state.is_handshake_finished() {
        return Err(NoiseError::Malformed);
    }
    let code = code(state.get_handshake_hash());
    let state = state.into_transport_mode().map_err(|_| NoiseError::Unavailable)?;
    Ok(Established { channel: Channel::new(state), code })
}

/// The browser's half of a handshake: it sends the first frame and reads the host's answer.
pub struct Initiator {
    state: snow::HandshakeState,
}

impl Initiator {
    /// Pair with the host a link names. Sends the channel's first frame.
    pub fn pair(pairing: &Pairing, device: &Secret) -> Result<(Self, Vec<u8>), NoiseError> {
        Self::start(Kind::Pair, &pairing.host_id, &pairing.host_key, device, Some(&pairing.psk), None)
    }

    /// A session with a host this device paired with.
    pub fn connect(host_id: &str, host_key: &PublicKey, device: &Secret) -> Result<(Self, Vec<u8>), NoiseError> {
        Self::start(Kind::Session, host_id, host_key, device, None, None)
    }

    fn start(
        kind: Kind,
        host_id: &str,
        host_key: &PublicKey,
        device: &Secret,
        psk: Option<&Secret>,
        ephemeral: Option<&[u8; KEY_BYTES]>,
    ) -> Result<(Self, Vec<u8>), NoiseError> {
        let mut state = handshake(kind, true, host_id, device, Some(host_key), psk, ephemeral)?;
        let mut first = vec![0u8; FIRST_FRAME_BYTES];
        first[0] = kind.byte();
        let written = state.write_message(&[], &mut first[1..]).map_err(|_| NoiseError::Unavailable)?;
        debug_assert_eq!(written, MESSAGE_1_BYTES);
        Ok((Self { state }, first))
    }

    /// The host's answer. For a pairing, a wrong psk fails here.
    pub fn finish(mut self, answer: &[u8]) -> Result<Established, NoiseError> {
        if answer.len() != SECOND_FRAME_BYTES {
            return Err(NoiseError::Malformed);
        }
        let mut payload = [0u8; SECOND_FRAME_BYTES];
        let read = self.state.read_message(answer, &mut payload).map_err(|_| NoiseError::Refused)?;
        if read != 0 {
            return Err(NoiseError::Malformed);
        }
        established(self.state)
    }
}

/// The host's half: it reads a channel's first frame and answers it.
pub struct Responder {
    state: snow::HandshakeState,
    device: PublicKey,
}

impl Responder {
    /// Read a channel's first frame. `psk` is the open pairing's, for a [`Kind::Pair`] frame
    /// ([`Kind::of`] says which); a session takes none.
    pub fn read(first: &[u8], host_id: &str, host: &Secret, psk: Option<&Secret>) -> Result<Self, NoiseError> {
        Self::read_with(first, host_id, host, psk, None)
    }

    fn read_with(
        first: &[u8],
        host_id: &str,
        host: &Secret,
        psk: Option<&Secret>,
        ephemeral: Option<&[u8; KEY_BYTES]>,
    ) -> Result<Self, NoiseError> {
        let kind = Kind::of(first)?;
        let mut state = handshake(kind, false, host_id, host, None, psk, ephemeral)?;
        let mut payload = [0u8; MESSAGE_1_BYTES];
        let read = state.read_message(&first[1..], &mut payload).map_err(|_| NoiseError::Refused)?;
        if read != 0 {
            return Err(NoiseError::Malformed);
        }
        let device = state
            .get_remote_static()
            .and_then(|key| <[u8; KEY_BYTES]>::try_from(key).ok())
            .ok_or(NoiseError::Malformed)?;
        Ok(Self { state, device: PublicKey(device) })
    }

    /// The browser's static key. Message 1 proves the browser holds its private half.
    pub fn device(&self) -> PublicKey {
        self.device
    }

    /// The channel's second frame, and the finished handshake.
    pub fn answer(mut self) -> Result<(Vec<u8>, Established), NoiseError> {
        let mut answer = vec![0u8; SECOND_FRAME_BYTES];
        let written = self.state.write_message(&[], &mut answer).map_err(|_| NoiseError::Unavailable)?;
        answer.truncate(written);
        Ok((answer, established(self.state)?))
    }
}

/// One end of an established channel: `atomics.v1` frames in, Noise transport messages out, and
/// back. Both directions count their own nonces, so each message must arrive once and in order.
pub struct Channel {
    state: snow::TransportState,
    /// The frame being put together from messages flagged [`MORE`].
    partial: Vec<u8>,
    /// Frame bytes sent since the sending direction's last rekey.
    sent: u64,
    rekey_bytes: u64,
    /// Rekey the sending direction with the next message.
    rekey: bool,
}

impl Channel {
    fn new(state: snow::TransportState) -> Self {
        Self { state, partial: Vec::new(), sent: 0, rekey_bytes: REKEY_BYTES, rekey: false }
    }

    /// One frame, up to `wire::MAX_FRAME_BYTES`, as the messages that carry it, in order. Each is
    /// one data body on the channel.
    pub fn seal(&mut self, frame: &[u8]) -> Result<Vec<Vec<u8>>, NoiseError> {
        if frame.len() > wire::MAX_FRAME_BYTES {
            return Err(NoiseError::TooBig);
        }
        let chunks: Vec<&[u8]> = if frame.is_empty() { vec![frame] } else { frame.chunks(MAX_CHUNK_BYTES).collect() };
        let last = chunks.len() - 1;
        let mut messages = Vec::with_capacity(chunks.len());
        let mut plain = Zeroizing::new(Vec::with_capacity(chunks[0].len() + 1));
        for (at, chunk) in chunks.into_iter().enumerate() {
            let rekey = self.rekey || self.sent + chunk.len() as u64 >= self.rekey_bytes;
            let flags = (if at < last { MORE } else { 0 }) | (if rekey { REKEY } else { 0 });
            plain.clear();
            plain.push(flags);
            plain.extend_from_slice(chunk);
            let mut message = vec![0u8; plain.len() + TAG_BYTES];
            let written = self.state.write_message(&plain, &mut message).map_err(|_| NoiseError::Unavailable)?;
            message.truncate(written);
            messages.push(message);
            self.sent += chunk.len() as u64;
            if rekey {
                self.state.rekey_outgoing();
                (self.sent, self.rekey) = (0, false);
            }
        }
        Ok(messages)
    }

    /// One message as it arrived: a whole frame once the message that ends it is read. Any error
    /// ends the channel; nothing after it can be read.
    pub fn open(&mut self, message: &[u8]) -> Result<Option<Vec<u8>>, NoiseError> {
        if !(TAG_BYTES + 1..=MAX_MESSAGE_BYTES).contains(&message.len()) {
            return Err(NoiseError::Malformed);
        }
        let mut plain = Zeroizing::new(vec![0u8; message.len()]);
        let read = self.state.read_message(message, &mut plain).map_err(|_| NoiseError::Refused)?;
        let flags = plain[0];
        if read == 0 || flags & !(MORE | REKEY) != 0 {
            return Err(NoiseError::Malformed);
        }
        let data = &plain[1..read];
        if self.partial.len() + data.len() > wire::MAX_FRAME_BYTES {
            return Err(NoiseError::TooBig);
        }
        self.partial.extend_from_slice(data);
        if flags & REKEY != 0 {
            self.state.rekey_incoming();
        }
        Ok((flags & MORE == 0).then(|| std::mem::take(&mut self.partial)))
    }

    /// Rekey the sending direction with the next message: what a host does every hour. Byte
    /// counts ([`REKEY_BYTES`]) are kept here.
    pub fn rekey_soon(&mut self) {
        self.rekey = true;
    }

    #[cfg(test)]
    fn rekey_every(&mut self, bytes: u64) {
        self.rekey_bytes = bytes;
    }
}

/// What a paired device may do on this host, as its allowlist entry says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Follows everything and changes nothing.
    View,
    /// May also type into terminals and approve, steer and command.
    Control,
}

impl Access {
    pub fn role(self) -> Role {
        match self {
            Access::View => Role::Viewer,
            Access::Control => Role::Controller,
        }
    }
}

/// The browser's first frame on a pairing channel, once the handshake is done: how the host's
/// prompt names this device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub name: String,
}

/// The host's answer, once its user has compared the codes. Paired: `may` says what this device
/// may do, and the channel goes on as an `atomics.v1` session. Otherwise the host closes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairAnswer {
    pub paired: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub may: Option<Access>,
}

/// A device name a person can read: no control characters, at most [`MAX_DEVICE_NAME`]
/// characters, never empty.
pub fn device_name(raw: &str) -> String {
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name: String = clean.trim().chars().take(MAX_DEVICE_NAME).collect();
    match name.trim() {
        "" => "Unnamed device".into(),
        name => name.to_string(),
    }
}

/// Six groups of four, three groups per line on either screen.
pub fn show_code(code: &str) -> String {
    if code.len() != 24 || !code.is_ascii() {
        return code.to_string();
    }
    format!("{} {} {}\n{} {} {}", &code[..4], &code[4..8], &code[8..12], &code[12..16], &code[16..20], &code[20..])
}

#[cfg(test)]
mod tests;

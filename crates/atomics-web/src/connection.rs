//! One browser channel to a host through a rendezvous broker: the Noise handshake that opens it, the
//! pairing exchange when this device pairs, and `atomics.v1` frames sealed and opened on it
//! (docs/daemon.md, "Pairing and the encrypted channel"). Sans I/O: it takes the broker's binary
//! messages and gives back those to send, so the same code runs in the browser and in tests against
//! a host built from `atomics_core::noise`.

use std::mem;

use atomics_core::noise::{
    self, Access, Channel, Initiator, NoiseError, PairAnswer, PairRequest, Pairing, PublicKey, Secret,
};

/// The longest pairing answer read; the host's is a few dozen bytes.
const MAX_PAIR_ANSWER_BYTES: usize = 1024;

pub struct Connection {
    stage: Stage,
    outgoing: Vec<Vec<u8>>,
}

enum Stage {
    /// The first frame is out; the host's answer completes the handshake. A pairing keeps the
    /// device's name for its [`PairRequest`].
    Handshake {
        initiator: Box<Initiator>,
        pairing: Option<String>,
    },
    /// Pairing: the handshake is done, the code shows, and the host's user compares it.
    Confirming {
        channel: Channel,
    },
    Open {
        channel: Channel,
    },
    /// A direct link to the host's own server (a native client on loopback or through a forward):
    /// `atomics.v1` text frames as they are, the upgrade's bearer token being the authentication.
    Plain,
    Ended,
}

/// What a message from the broker brought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Pairing: the pairing fingerprint to show beside the host's.
    Code(String),
    /// Pairing: the host's user said the codes match; what this device may do there.
    Paired(Option<Access>),
    /// The channel carries `atomics.v1` from now on, starting with the client's `hello`.
    Open,
    /// One `atomics.v1` frame.
    Frame(String),
}

/// Why the channel ended. Any of these ends it: nothing after can be read or sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The host's answer to the first frame (Noise message 2) failed: the host does not hold the key
    /// this browser expects, or (pairing) the link's psk.
    Handshake(NoiseError),
    /// A transport message failed to open or broke the framing, or one could not be sealed. It says
    /// nothing about the keys: it was cut, dropped or forged on the way, and a new channel may
    /// succeed.
    Transport(NoiseError),
    /// The host's pairing answer or a frame was not well formed.
    Malformed,
    /// The host's user refused this device: the codes differed.
    Refused,
    /// The channel had already ended.
    Ended,
}

impl Connection {
    /// Pair this device with the host a pairing link names. `name` is how the host's prompt names it.
    pub fn pair(pairing: &Pairing, device: &Secret, name: &str) -> Result<Self, NoiseError> {
        let (initiator, first) = Initiator::pair(pairing, device)?;
        let pairing = Some(noise::device_name(name));
        Ok(Self { stage: Stage::Handshake { initiator: Box::new(initiator), pairing }, outgoing: vec![first] })
    }

    /// A session with a host this device paired with.
    pub fn session(host_id: &str, host_key: &PublicKey, device: &Secret) -> Result<Self, NoiseError> {
        let (initiator, first) = Initiator::connect(host_id, host_key, device)?;
        Ok(Self { stage: Stage::Handshake { initiator: Box::new(initiator), pairing: None }, outgoing: vec![first] })
    }

    /// A direct link to the host's server, which the WebSocket upgrade authenticated: no handshake, and
    /// each message is one `atomics.v1` frame.
    pub fn plain() -> Self {
        Self { stage: Stage::Plain, outgoing: Vec::new() }
    }

    /// The messages to send, in order: one binary WebSocket message each (text on a plain link).
    pub fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        mem::take(&mut self.outgoing)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.stage, Stage::Open { .. } | Stage::Plain)
    }

    /// A direct link to the host's server, carrying text frames.
    pub fn is_plain(&self) -> bool {
        matches!(self.stage, Stage::Plain)
    }

    /// One binary message from the broker.
    pub fn receive(&mut self, message: &[u8]) -> Result<Vec<Event>, Failure> {
        match mem::replace(&mut self.stage, Stage::Ended) {
            Stage::Handshake { initiator, pairing } => {
                let done = initiator.finish(message).map_err(Failure::Handshake)?;
                let mut channel = done.channel;
                let Some(name) = pairing else {
                    self.stage = Stage::Open { channel };
                    return Ok(vec![Event::Open]);
                };
                let request = serde_json::to_vec(&PairRequest { name }).map_err(|_| Failure::Malformed)?;
                self.outgoing.extend(channel.seal(&request).map_err(Failure::Transport)?);
                self.stage = Stage::Confirming { channel };
                Ok(vec![Event::Code(done.code)])
            }
            Stage::Confirming { mut channel } => {
                let Some(frame) = channel.open(message).map_err(Failure::Transport)? else {
                    self.stage = Stage::Confirming { channel };
                    return Ok(Vec::new());
                };
                if frame.len() > MAX_PAIR_ANSWER_BYTES {
                    return Err(Failure::Malformed);
                }
                let answer: PairAnswer = serde_json::from_slice(&frame).map_err(|_| Failure::Malformed)?;
                if !answer.paired {
                    return Err(Failure::Refused);
                }
                self.stage = Stage::Open { channel };
                Ok(vec![Event::Paired(answer.may), Event::Open])
            }
            Stage::Open { mut channel } => {
                let frame = channel.open(message).map_err(Failure::Transport)?;
                let event = match frame {
                    Some(frame) => Some(Event::Frame(String::from_utf8(frame).map_err(|_| Failure::Malformed)?)),
                    None => None,
                };
                self.stage = Stage::Open { channel };
                Ok(event.into_iter().collect())
            }
            Stage::Plain => {
                self.stage = Stage::Plain;
                let frame = String::from_utf8(message.to_vec()).map_err(|_| Failure::Malformed)?;
                Ok(vec![Event::Frame(frame)])
            }
            Stage::Ended => Err(Failure::Ended),
        }
    }

    /// Seal one `atomics.v1` frame for sending; it may take several messages.
    pub fn send(&mut self, frame: &str) -> Result<(), Failure> {
        if matches!(self.stage, Stage::Plain) {
            self.outgoing.push(frame.as_bytes().to_vec());
            return Ok(());
        }
        let Stage::Open { channel } = &mut self.stage else { return Err(Failure::Ended) };
        match channel.seal(frame.as_bytes()) {
            Ok(messages) => {
                self.outgoing.extend(messages);
                Ok(())
            }
            Err(error) => {
                self.stage = Stage::Ended;
                Err(Failure::Transport(error))
            }
        }
    }
}

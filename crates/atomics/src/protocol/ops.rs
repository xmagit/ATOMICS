//! Handing mutations to whatever carries them out (the runtime thread, in the app) and their
//! outcomes back to the client's thread. The protocol never runs a mutation itself: the runtime
//! thread serialises them with the desktop's own actions, so two clients answering one approval,
//! or a client and the desktop, meet one after the other and exactly one of them wins.

use std::sync::Arc;
use std::sync::mpsc;

use atomics_core::ops::{Command, CommandResult, Outcome};
#[cfg(test)]
use atomics_core::wire::{ErrorCode, WireError};

use super::link::Waker;

/// One mutation on its way to be carried out.
pub struct OpRequest {
    /// The client's own ID, from its `hello`; with `command.op()` (and `device`) it names the
    /// operation.
    pub client: String,
    /// The paired device it came from, if any.
    pub device: Option<super::session::PeerDevice>,
    pub command: Command,
    pub reply: OpReply,
}

/// Where a mutation's outcomes go: the thread of the client that sent it, which it wakes.
#[derive(Clone)]
pub struct OpReply {
    tx: mpsc::Sender<CommandResult>,
    waker: Option<Arc<Waker>>,
}

impl OpReply {
    #[cfg(test)]
    pub(crate) fn for_test(tx: mpsc::Sender<CommandResult>) -> Self {
        Self::new(tx, None)
    }

    pub(super) fn new(tx: mpsc::Sender<CommandResult>, waker: Option<Arc<Waker>>) -> Self {
        Self { tx, waker }
    }

    /// Never blocks. A client that has gone is not an error: its operation stays journaled.
    pub fn send(&self, op: &str, outcome: Outcome) {
        if self.tx.send(CommandResult { op: op.into(), outcome }).is_ok()
            && let Some(waker) = &self.waker
        {
            waker.wake();
        }
    }
}

/// What carries mutations out.
pub trait Commands: Send + Sync {
    /// Separate user-client ingress. Never routed through control or the mutation journal.
    fn attachment(
        &self,
        _owner: u64,
        _request: atomics_core::attachments::Request,
    ) -> Result<atomics_core::attachments::Reply, String> {
        Err("Attachments unavailable on this host".into())
    }
    fn attachment_disconnect(&self, _owner: u64) {}
    fn attachment_tick(&self) {}
    /// Queue a mutation; its outcomes arrive on its reply. False when nothing will run it (the
    /// host is stopping).
    fn submit(&self, request: OpRequest) -> bool;
}

/// For a server with nothing behind it that changes state: every mutation is refused.
#[cfg(test)]
pub struct NoCommands;

#[cfg(test)]
impl Commands for NoCommands {
    fn submit(&self, request: OpRequest) -> bool {
        let error = WireError::new(ErrorCode::Rejected, "this host carries out no commands");
        request.reply.send(request.command.op(), Outcome::refused(error));
        true
    }
}

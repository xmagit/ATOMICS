//! Which host a native client shows, and how each dial finds it: its address and token, found afresh for
//! every dial so that a host that restarted with a new token is reached again.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use super::link::{self, Endpoint};

/// `atomics --connect <address>` and `[client] connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `local`: the `atomics host` on this machine, as its `protocol.json` advertises it (in ATOMICS_HOME).
    Local,
    /// `ws://127.0.0.1:PORT`: a host's server on this machine's loopback, or a forward to one, with the
    /// token this machine paired with it (kept in the OS credential store under the address).
    Address { addr: SocketAddr },
    /// `vm`: the ATOMICS host in this Mac's VM (docs/nixos-vm.md).
    Vm,
}

impl Target {
    pub fn parse(text: &str) -> Result<Target> {
        let text = text.trim();
        match text {
            "local" => return Ok(Target::Local),
            "vm" => return Ok(Target::Vm),
            _ => {}
        }
        let Some(rest) = text.strip_prefix("ws://") else {
            bail!("connect to `local`, `vm` or ws://127.0.0.1:PORT, not {text:?}");
        };
        let addr: SocketAddr =
            rest.trim_end_matches('/').parse().with_context(|| format!("{text} is not ws://IP:PORT"))?;
        if addr.port() == 0 {
            bail!("the port in {text} must be between 1 and 65535");
        }
        if !addr.ip().is_loopback() {
            bail!("{text} is not on this machine's loopback; reach another machine's host through a forward");
        }
        Ok(Target::Address { addr })
    }
}

/// The keychain item a paired address's token is kept in.
pub const PAIRED_SERVICE: &str = "atomics-paired-host-v1";

/// The endpoint of the local host, from its advertisement.
pub fn local() -> Result<Endpoint> {
    let path = atomics_core::paths::atomics_home().join("protocol.json");
    let advert = atomics_core::advert::read(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let addr: SocketAddr = advert
        .url
        .strip_prefix("ws://")
        .and_then(|rest| rest.parse().ok())
        .with_context(|| format!("{} names no ws:// address", path.display()))?;
    Ok(Endpoint { addr: link::Address::Tcp(addr), host: addr.to_string(), token: Zeroizing::new(advert.token.clone()) })
}

/// A paired address's endpoint, its token from the OS credential store.
pub fn address(addr: SocketAddr) -> Result<Endpoint> {
    let keychain = crate::relay::Keychain::current();
    let token = keychain
        .read_item(PAIRED_SERVICE, &format!("ws://{addr}"))?
        .with_context(|| format!("no token is kept for ws://{addr}; pair with that host first"))?;
    Ok(Endpoint { addr: link::Address::Tcp(addr), host: addr.to_string(), token: Zeroizing::new(token) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_local_the_vm_or_a_loopback_address() {
        assert_eq!(Target::parse("local").unwrap(), Target::Local);
        assert_eq!(Target::parse(" vm ").unwrap(), Target::Vm);
        assert_eq!(
            Target::parse("ws://127.0.0.1:47800/").unwrap(),
            Target::Address { addr: "127.0.0.1:47800".parse().unwrap() }
        );
        for bad in [
            "",
            "ws://127.0.0.1:0",
            "ws://[::1]:0",
            "ws://example.com:1",
            "ws://10.0.0.1:47800",
            "http://127.0.0.1:1",
            "wss://127.0.0.1:1",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }
}

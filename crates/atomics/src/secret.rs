//! The CLI accepts values on stdin only. There is deliberately no CLI/protocol copy operation.
use anyhow::{Context, Result};
use atomics_core::secrets::{Auth, Scope, Secret};
use clap::Subcommand;
use std::io::{BufRead, IsTerminal, Read, Write};

#[derive(Subcommand)]
pub enum Command {
    /// Add a secret. Read one value from stdin (masked in a terminal), never an argument.
    Add {
        name: String,
        /// DNS host or known service: openai, anthropic, github, linear, logo.dev, stripe.
        #[arg(long)]
        host: Option<String>,
        /// everywhere, workspace:NAME, or project:/main/checkout (VM path for a VM client)
        #[arg(long, default_value = "everywhere")]
        scope: String,
        /// Send the key in this header instead of Authorization: Bearer.
        #[arg(long, conflicts_with_all = ["basic_user", "query"])]
        header: Option<String>,
        /// Basic auth: public username; stdin supplies the password.
        #[arg(long, conflicts_with = "query")]
        basic_user: Option<String>,
        /// Send the key in this query parameter.
        #[arg(long)]
        query: Option<String>,
        /// Restrict HTTP methods (comma separated); default: all.
        #[arg(long, value_delimiter = ',')]
        method: Vec<String>,
        /// Restrict requests to this path and its descendants.
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// List names, kind, scope, hosts and last relay use. Never prints values.
    List,
    /// Remove a secret by name.
    Rm { name: String },
}

pub fn run(command: Command) -> Result<()> {
    let relay = crate::relay::Service::new(atomics_core::paths::atomics_home(), crate::relay::Keychain::current());
    let store = if cfg!(target_os = "macos") {
        crate::credentials::Store::local(relay.keychain().clone())
    } else {
        crate::credentials::Store::new(relay.keychain().clone())
    };
    match command {
        Command::Add { name, host, scope, header, basic_user, query, method, path_prefix } => {
            let scope = match scope.as_str() {
                "everywhere" => Scope::Everywhere,
                s if s.starts_with("workspace:") => Scope::Workspace(s[10..].into()),
                s if s.starts_with("project:") => {
                    Scope::Project(atomics_core::paths::expand(&s[8..]).to_string_lossy().into_owned())
                }
                _ => anyhow::bail!("Scope must be everywhere, workspace:NAME or project:/main/checkout"),
            };
            let known = host.as_deref().and_then(atomics_core::secrets::service);
            let auth = match (header, basic_user, query) {
                (Some(h), _, _) => Auth::Header(h),
                (_, Some(u), _) => Auth::Basic(u),
                (_, _, Some(q)) => Auth::Query(q),
                _ => known.as_ref().map(|(_, a)| a.clone()).unwrap_or_default(),
            };
            let host = known.map(|(h, _)| h.into()).or_else(|| host.map(|h| h.to_ascii_lowercase()));
            let spec = Secret { name, host, scope, auth, methods: method, path_prefix, ..Default::default() };
            spec.validate().map_err(anyhow::Error::msg)?;
            let vm_client = crate::vm::secrets::is_vm_client()?;
            if spec.host.is_none() {
                eprintln!("{}", atomics_core::secrets::EXPOSED);
            }
            let stdin = std::io::stdin();
            eprint!("Secret value (stdin only): ");
            std::io::stderr().flush()?;
            let terminal = stdin.is_terminal();
            let echo = crate::sandbox::setup::Echo::hide(terminal)?;
            let value = read_value(stdin.lock(), terminal)?;
            drop(echo);
            eprintln!();
            if vm_client {
                store.add_secret_rule(spec, &value)?;
            } else {
                store.add_secret(spec, &value)?;
            }
            sync_vm().context("Secret saved in the local keychain; VM sync is pending")?;
            println!("Secret added. New sessions receive it when in scope.");
        }
        Command::List => {
            for spec in store.secrets()? {
                println!(
                    "{}\t{}\t{}\t{}\t{}\tmethods={}\tpath={}\tauth={}",
                    spec.name,
                    spec.kind(),
                    spec.scope.label(),
                    spec.host.as_deref().unwrap_or("—"),
                    relay.secret_last_use(&spec.name).unwrap_or_else(|| "never".into()),
                    if spec.methods.is_empty() { "*".into() } else { spec.methods.join(",") },
                    spec.path_prefix.as_deref().unwrap_or("/"),
                    serde_json::to_string(&spec.auth)?
                );
            }
        }
        Command::Rm { name } => {
            store.remove_secret(&name)?;
            sync_vm().context("Secret removed from the local keychain; VM removal is pending")?;
            if atomics_core::paths::socket_path().exists()
                && !crate::ipc::request(&atomics_core::control::ControlRequest::SecretRemoved { name })
                    .is_ok_and(|reply| reply.ok)
            {
                eprintln!(
                    "Secret removed from the store; could not notify the running host. Restart its sessions to revoke existing grants."
                );
            }
            println!("Secret removed. Restart existing sessions to clear their environment.");
        }
    }
    Ok(())
}

fn sync_vm() -> Result<()> {
    if let Some(info) = crate::vm::secrets::configured()? {
        crate::vm::secrets::push(&info)?;
    }
    Ok(())
}

fn read_value(reader: impl BufRead, terminal: bool) -> Result<zeroize::Zeroizing<String>> {
    const MAX_VALUE: u64 = 8192;
    // Keep room for CRLF and one extra byte, so overlong input cannot be silently truncated.
    let mut reader = reader.take(MAX_VALUE + 3);
    let mut value = zeroize::Zeroizing::new(String::new());
    if terminal {
        reader.read_line(&mut value)?;
    } else {
        reader.read_to_string(&mut value)?;
    }
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    anyhow::ensure!(value.len() <= MAX_VALUE as usize, "Secret value exceeds 8192 bytes (contents hidden)");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_secret_preserves_all_lines_and_trims_only_one_trailing_newline() {
        for (input, expected) in [
            (
                "-----BEGIN PRIVATE KEY-----\ncanary-first\ncanary-second\n-----END PRIVATE KEY-----\n",
                "-----BEGIN PRIVATE KEY-----\ncanary-first\ncanary-second\n-----END PRIVATE KEY-----",
            ),
            ("line one\nline two\r\n", "line one\nline two"),
            ("line one\n\n", "line one\n"),
            ("line one\nline two", "line one\nline two"),
        ] {
            assert_eq!(read_value(input.as_bytes(), false).unwrap().as_str(), expected);
        }
        assert_eq!(read_value(b"first\nsecond\n".as_slice(), true).unwrap().as_str(), "first");
    }

    #[test]
    fn piped_secret_rejects_oversized_values_without_truncating_them() {
        let boundary = "x".repeat(8192);
        assert_eq!(read_value(format!("{boundary}\r\n").as_bytes(), false).unwrap().as_str(), boundary);
        for extra in ["x", "x\n", "x\r\n", "\ncanary-hidden-tail"] {
            let error = read_value(format!("{boundary}{extra}").as_bytes(), false).unwrap_err().to_string();
            assert!(error.contains("exceeds 8192 bytes"));
            assert!(!error.contains("canary"));
        }
    }
}

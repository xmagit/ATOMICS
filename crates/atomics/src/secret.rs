//! The CLI accepts values on stdin only. There is deliberately no CLI/protocol copy operation.
use anyhow::Result;
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
        /// everywhere, workspace:NAME, or project:/path/to/main/checkout
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
    let store = crate::credentials::Store::new(relay.keychain().clone());
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
            let spec = Secret { name, host, scope, auth, methods: method, path_prefix };
            spec.validate().map_err(anyhow::Error::msg)?;
            if spec.host.is_none() {
                eprintln!("{}", atomics_core::secrets::EXPOSED);
            }
            let stdin = std::io::stdin();
            eprint!("Secret value (stdin only): ");
            std::io::stderr().flush()?;
            let echo = crate::sandbox::setup::Echo::hide(stdin.is_terminal())?;
            let mut value = zeroize::Zeroizing::new(String::new());
            stdin.lock().take(8194).read_line(&mut value)?;
            drop(echo);
            eprintln!();
            if value.ends_with('\n') {
                value.pop();
                if value.ends_with('\r') {
                    value.pop();
                }
            }
            store.add_secret(spec, &value)?;
            println!("Secret added. New sessions receive it when in scope.");
        }
        Command::List => {
            for spec in store.secrets()? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    spec.name,
                    spec.kind(),
                    spec.scope.label(),
                    spec.host.as_deref().unwrap_or("—"),
                    relay.secret_last_use(&spec.name).unwrap_or_else(|| "never".into())
                );
            }
        }
        Command::Rm { name } => {
            store.remove_secret(&name)?;
            if atomics_core::paths::socket_path().exists() {
                if !crate::ipc::request(&atomics_core::control::ControlRequest::SecretRemoved { name })
                    .is_ok_and(|reply| reply.ok)
                {
                    eprintln!(
                        "Secret removed from the store; could not notify the running host. Restart its sessions to revoke existing grants."
                    );
                }
            }
            println!("Secret removed. Restart existing sessions to clear their environment.");
        }
    }
    Ok(())
}

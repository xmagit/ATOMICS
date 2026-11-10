#[cfg(test)]
#[path = "../../../tests/support.rs"]
mod test_support;

#[cfg(test)]
mod platform_tests;
#[cfg(test)]
mod test_raster;

mod app;
mod build_info;
mod client;
mod connections;
mod crash;
mod credentials;
mod diff_worker;
mod doctor;
mod github;
mod governor;
mod headless;
mod highlight;
mod hook_client;
mod host;
mod install;
mod instance;
mod ipc;
mod lifecycle;
mod machine;
mod mcp;
mod mcp_client;
mod open_link;
mod preview;
mod protocol;
mod ref_guard;
mod relay;
mod restart;
mod sandbox;
mod sandbox_ownership;
mod sandbox_usage;
mod secret;
mod secret_copy;
mod setup;
mod statusline;
mod terminal;
mod theme;
mod ticket_signin;
mod tickets;
mod update;
mod usage;
mod vm;
mod voice;
mod watcher;

// Drawing code that reads nothing from the host's model; atomics-ui owns it.
use atomics_ui::{diff_view, markdown, panel_nav};

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
// An update reads the version from the first word after the name: see `update::must_report`.
#[command(
    name = "atomics",
    version = crate::build_info::version(),
    about = "Review-first workspace for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Project (git repository) to open; any folder inside it or its worktrees works. An
    /// `atomics://open?...` link opens ATOMICS at a plan or ticket instead.
    path: Option<PathBuf>,
    /// Open a saved workspace (a set of projects); `setup` and `doctor` take it after their name too
    #[arg(long, short, global = true)]
    workspace: Option<String>,
    /// Start an agent on launch, as AGENT:FOLDER (repeatable), e.g. claude:~/repos/app
    #[arg(long, value_name = "AGENT:FOLDER")]
    open: Vec<String>,
    /// Window size in points, e.g. 1600x1000
    #[arg(long)]
    size: Option<String>,
    /// Save a PNG of the window after --screenshot-delay seconds, then quit
    #[arg(long)]
    screenshot: Option<PathBuf>,
    #[arg(long, default_value_t = 4.0)]
    screenshot_delay: f32,
    /// Open the window as a client of an ATOMICS host: `local` (`atomics host` here), `vm` (the host in
    /// this Mac's ATOMICS VM) or ws://127.0.0.1:PORT; "" for this window as the host. Default:
    /// `[client] connect`
    #[arg(long, value_name = "ADDRESS")]
    connect: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the headless VM secret broker (a separate system user).
    SecretBroker(relay::broker::Options),
    /// Private stdin-only bridge to the broker's administration socket.
    #[command(hide = true)]
    SecretBrokerAdmin,
    /// Store secrets once for sessions in scope; values are accepted on stdin only.
    Secret {
        #[command(subcommand)]
        command: secret::Command,
    },
    /// Guide fresh-machine sandbox setup (safe to run again)
    Setup {
        /// Store one credential; the value is read from stdin only
        #[arg(long, value_parser = ["claude", "openai", "linear", "github", "registry"])]
        credential: Option<String>,
        /// Upstream DNS host for a private registry (no URL or path)
        #[arg(long, requires = "credential")]
        registry_host: Option<String>,
    },
    /// Set up Docker Sandboxes or build warm repository templates
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Check required tools, configuration and integrations
    Doctor,
    /// Install this program for your user: where it goes, its menu entry and icon, and on a Mac `atomics` on PATH
    Install {
        /// Install the Linux fresh-auth Copy policy system-wide (requires root).
        #[arg(long)]
        secret_copy_policy: bool,
    },
    /// Go back to the version the last update replaced; run it again to go forward again
    Rollback,
    /// Sign in to or out of the ticket system's MCP server (browser OAuth)
    Tickets {
        #[command(subcommand)]
        command: TicketsCommand,
    },
    /// Hook handler for Claude Code and Codex (reads the event on stdin)
    Hook {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        chief: bool,
    },
    /// Forward Claude's status line data to ATOMICS, preserving the user's command
    Statusline {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Add ATOMICS's hooks to ~/.claude/settings.json so every Claude session reports to ATOMICS
    InstallHooks,
    /// Print the hook settings ATOMICS passes to the agents it starts
    #[command(name = "print-hook-settings", alias = "hooks-json")]
    HooksJson {
        #[arg(long, default_value = "codex", value_parser = ["codex", "claude"])]
        agent: String,
        /// Pane ID used by Claude hooks; Codex hook definitions do not contain it
        #[arg(long, default_value = "PANE_ID")]
        pane: String,
    },
    /// MCP server (stdio) that lets another agent start, read, review and steer agents in ATOMICS
    Mcp {
        /// Expose reporting and scoped ticket read/comment tools for task agents
        #[arg(long)]
        agent: bool,
        /// Enable chief-only tools (used by the built-in chief session)
        #[arg(long, conflicts_with = "agent")]
        chief: bool,
    },
    /// Run a connected MCP server's command for sbx's gateway, with its stored environment
    #[command(name = "mcp-host", hide = true)]
    McpHost {
        /// Add the environment stored for the server in the OS keychain
        #[arg(long)]
        env: bool,
        server: String,
        /// The server's command, after `--`
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Send one control request to the running ATOMICS, e.g. '{"cmd":"list"}'
    Ctl { json: String },
    /// Run the runtime headless, with no window, display or GPU: agents, hooks, ctl, MCP and
    /// (with `[remote] enabled`) the protocol server. Stays in the foreground, logs to stderr and
    /// stops cleanly on SIGTERM. Takes ATOMICS_HOME for itself: no desktop may share it.
    Host {
        /// Projects (git repositories) to open, as `atomics <path>` does
        projects: Vec<PathBuf>,
        /// Serve `atomics.v1` on this loopback port whatever `[remote]` says (a service's own setting)
        #[arg(long, value_name = "PORT")]
        remote_port: Option<u16>,
    },
    /// The ATOMICS VM on a Mac: set it up from a built image and bundle, start, stop and check it
    /// (docs/nixos-vm.md). macOS only.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    /// Install the newest version; a running ATOMICS restarts into it when agents finish
    Update {
        /// Only say whether a newer version exists
        #[arg(long)]
        check: bool,
    },
    /// Run a heavy command, such as a full test run or a QA instance, once ATOMICS has a free slot
    Queue {
        #[arg(long, default_value = "test", value_parser = ["test", "qa"])]
        class: String,
        /// The command, after `--`
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum VmCommand {
    /// Update the guest system and Mac wrappers in place, keeping the disk and sign-ins
    Update {
        /// Continue even if the VM's host reports busy work
        #[arg(long)]
        yes: bool,
        /// Install and select the next boot, leaving the VM and window running
        #[arg(long)]
        no_restart: bool,
    },
    /// Install the bundle, create the Lima instance from the image, start and check it, list the
    /// sign-ins left and show the config change for this Mac (written only by `vm config --apply`)
    Setup {
        /// Use a local build folder instead of downloading this version's signed VM image
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        #[command(flatten)]
        sizes: vm::download::Sizes,
        /// Delete and recreate an instance of the bundle's name that this setup did not create
        #[arg(long)]
        replace: bool,
        /// Confirm --replace without asking
        #[arg(long, requires = "replace")]
        yes: bool,
    },
    /// Start the VM (waits for a QA slot)
    Start,
    /// Stop the VM
    Stop,
    /// Whether the VM is set up, stopped, starting or running
    Status,
    /// Run the guest check and list the sign-ins left
    Check,
    /// Show this Mac's config change for the VM; --apply writes it, keeping a backup
    Config {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum TicketsCommand {
    /// Sign in with the browser; on a machine without one, open the printed address elsewhere and paste back
    /// the address the browser ends on
    Login,
    /// Revoke the sign-in where the server allows it and forget it on this machine
    Logout,
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Set up sandboxed agents: tools, Docker sign-in, network policy and credentials (over stdin)
    Setup,
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Check that sandboxed Claude answers on the Claude subscription through ATOMICS's relay, with no
    /// token in the VM (makes one throwaway sandbox and removes it)
    RelayCheck,
    /// Run a fixed read-only KQL query through host az login from a throwaway sandbox
    AzureCheck {
        /// Application Insights application ID chosen by the user (not its ARM resource ID)
        app_id: String,
    },
}

#[derive(Subcommand)]
enum TemplateCommand {
    /// Save a warmed local template, using this project's configured template tag
    Build {
        project: String,
        #[arg(long, default_value = "codex", value_parser = ["codex", "claude", "shell"])]
        agent: String,
        /// Repository-relative project to build for .NET repositories
        #[arg(long)]
        dotnet_project: Option<String>,
        /// Rust package to check (defaults to the whole workspace)
        #[arg(long)]
        rust_package: Option<String>,
    },
}

/// Markers of the Claude Code or Codex session ATOMICS may have been started from. Agents
/// that inherit them believe they are sub-processes (Claude then stops saving
/// transcripts, so sessions cannot be resumed).
const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

fn main() -> eframe::Result {
    let cli = Cli::try_parse().unwrap_or_else(|error| {
        if std::env::args_os().any(|arg| arg == "secret" || arg == "secret-broker" || arg == "secret-broker-admin") && !matches!(error.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion) {
            eprintln!("Invalid secret command (arguments hidden). Use atomics secret --help. Values are accepted on stdin only.");
            std::process::exit(2);
        }
        error.exit()
    });
    if matches!(cli.command, None | Some(Command::Host { .. }))
        && let Err(error) = atomics_core::paths::validate_socket_path(&atomics_core::paths::socket_path())
    {
        eprintln!("atomics: {error}");
        std::process::exit(1);
    }
    let mut migration_errors = Vec::new();
    if matches!(cli.command, None | Some(Command::Host { .. })) {
        for var in PARENT_SESSION_VARS {
            // SAFETY: runs first thing in main, before any other thread exists.
            unsafe { std::env::remove_var(var) };
        }
    }
    // Everything a person runs moves the old places first, so that setup (the installer's first
    // step) finds the old config instead of creating a new one. What agents call inside a live
    // session never moves state out from under it.
    let in_session = matches!(
        cli.command,
        Some(
            Command::SecretBroker(_)
                | Command::SecretBrokerAdmin
                | Command::Hook { .. }
                | Command::Statusline { .. }
                | Command::HooksJson { .. }
                | Command::Mcp { .. }
                | Command::McpHost { .. }
                | Command::Queue { .. }
                | Command::Ctl { .. }
                | Command::Update { .. }
        )
    );
    if !in_session {
        let migration = atomics_core::paths::migrate_old_names();
        for moved in migration.moved {
            eprintln!("atomics: {moved}");
        }
        for error in &migration.errors {
            eprintln!("atomics: {error}");
        }
        migration_errors = migration.errors;
    }
    // What ATOMICS runs itself gets the login shell's PATH: git (with its LFS filters and hooks),
    // version probes, MCP and ticket servers, a queued command. What agents call per tool use
    // (hooks, the status line, MCP) runs with their PATH, which ATOMICS gave them, and must stay quick.
    if matches!(
        cli.command,
        Some(
            Command::Host { .. }
                | Command::Doctor
                | Command::Setup { .. }
                | Command::Secret { .. }
                | Command::Sandbox { .. }
                | Command::Tickets { .. }
                | Command::Queue { .. }
                | Command::Update { .. }
                | Command::Install { .. }
        )
    ) {
        // SAFETY: no other thread exists yet.
        unsafe { setup::adopt_login_path() };
    }
    match cli.command {
        Some(Command::SecretBrokerAdmin) => {
            if relay::broker::admin_stdin().is_err() {
                eprintln!("Secret broker administration failed (contents hidden)");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::SecretBroker(options)) => {
            if let Err(error) = relay::broker::run(options) {
                eprintln!("Secret broker failed: {error}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Secret { command }) => {
            if let Err(error) = secret::run(command) {
                eprintln!("Secret operation failed: {error}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Setup { credential: Some(kind), registry_host }) => {
            if let Err(error) = sandbox::setup::credential_setup(&kind, registry_host.as_deref()) {
                eprintln!("Credential setup failed: {error}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(
            command @ (Command::Setup { credential: None, .. } | Command::Sandbox { command: SandboxCommand::Setup }),
        ) => {
            let sandboxes = matches!(command, Command::Sandbox { .. });
            if let Err(err) = sandbox::setup::run(cli.workspace.as_deref(), sandboxes) {
                // Stopping it is no failure: ATOMICS is installed, and this carries on whenever someone wants it.
                if err.is::<sandbox::setup::Stopped>() {
                    println!("{}", sandbox::setup::STOPPED);
                    return Ok(());
                }
                eprintln!("Setup did not finish: {err}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Sandbox {
            command:
                SandboxCommand::Template {
                    command: TemplateCommand::Build { project, agent, dotnet_project, rust_package },
                },
        }) => {
            if let Err(error) = sandbox::template::run(
                &project,
                cli.workspace.as_deref(),
                &agent,
                dotnet_project.as_deref(),
                rust_package.as_deref(),
            ) {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Sandbox { command: SandboxCommand::RelayCheck }) => {
            // A report of PASS and FAIL lines, none holding a value, so the failure goes with them.
            if let Err(error) = sandbox::relay_check::run() {
                println!("FAIL relay check: {error:#}");
                std::process::exit(1);
            }
            println!("PASS relay check");
            return Ok(());
        }
        Some(Command::Sandbox { command: SandboxCommand::AzureCheck { app_id } }) => {
            match sandbox::azure_check::run(&app_id) {
                Ok(count) => println!("PASS Azure relay check: count={count}"),
                Err(error) => {
                    println!("FAIL Azure relay check: {error:#}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some(Command::Tickets { command }) => std::process::exit(ticket_signin::run_cli(
            matches!(command, TicketsCommand::Login),
            cli.workspace.as_deref(),
        )),
        Some(Command::Doctor) => std::process::exit(doctor::run(cli.workspace.as_deref())),
        Some(Command::Hook { socket, pane, chief }) => {
            hook_client::run(socket, pane, chief);
            return Ok(());
        }
        Some(Command::Statusline { socket }) => {
            statusline::run(socket.unwrap_or_else(atomics_core::paths::socket_path));
            return Ok(());
        }
        Some(Command::InstallHooks) => {
            match setup::install_global_hooks() {
                Ok(path) => println!("ATOMICS hooks added to {} (backup next to it)", path.display()),
                Err(err) => eprintln!("installing hooks failed: {err:#}"),
            }
            return Ok(());
        }
        Some(Command::HooksJson { agent, pane }) => {
            let exe = setup::hook_executable().unwrap_or_else(|_| PathBuf::from("atomics"));
            println!(
                "{}",
                serde_json::to_string_pretty(&atomics_core::hook::session_settings(
                    &exe,
                    &atomics_core::paths::socket_path(),
                    &pane,
                    agent == "codex"
                ))
                .unwrap_or_default()
            );
            return Ok(());
        }
        Some(Command::Mcp { agent, chief }) => {
            mcp::run(agent, chief);
            return Ok(());
        }
        Some(Command::McpHost { env, server, command }) => {
            // Stdout belongs to the server's MCP channel: report only on stderr.
            let Err(error) = connections::run_host(&server, env, &command);
            eprintln!("atomics mcp-host: {error:#}");
            std::process::exit(1);
        }
        Some(Command::Update { check }) => std::process::exit(update::run_cli(check)),
        Some(Command::Install { secret_copy_policy: true }) => {
            match secret_copy::install_policy() {
                Ok(()) => println!("Secret Copy authentication policy installed"),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some(Command::Install { secret_copy_policy: false }) => std::process::exit(install::run_cli()),
        Some(Command::Rollback) => std::process::exit(update::run_rollback_cli()),
        Some(Command::Queue { class, command }) => {
            let class =
                atomics_core::governor::SlotClass::parse(&class).unwrap_or(atomics_core::governor::SlotClass::Test);
            std::process::exit(governor::run_queued(&atomics_core::paths::socket_path(), class, &command));
        }
        Some(Command::Host { projects, remote_port }) => {
            let options = app::Options {
                projects: projects.iter().map(|p| atomics_core::paths::expand(&p.to_string_lossy())).collect(),
                workspace: cli.workspace,
                migration_errors,
                remote_port,
                ..app::Options::default()
            };
            std::process::exit(headless::run(options));
        }
        Some(Command::Vm { command }) => {
            let result = match command {
                VmCommand::Setup { from, sizes, replace, yes } => {
                    vm::setup::setup(vm::setup::Setup { from, sizes, replace, yes })
                }
                VmCommand::Update { yes, no_restart } => vm::update::update(vm::update::Update { yes, no_restart }),
                VmCommand::Start => vm::setup::start(),
                VmCommand::Stop => vm::setup::stop(),
                VmCommand::Status => vm::setup::status(),
                VmCommand::Check => vm::setup::check_command(),
                VmCommand::Config { apply } => vm::setup::config(apply),
            };
            if let Err(error) = result {
                eprintln!("atomics vm: {error:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Ctl { json }) => {
            let response = serde_json::from_str::<atomics_core::control::ControlRequest>(&json)
                .map_err(anyhow::Error::from)
                .and_then(|request| ipc::request(&request));
            match response {
                Ok(response) => println!("{}", serde_json::to_string_pretty(&response).unwrap_or_default()),
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        None => {}
    }
    // The window as a client of a host, which owns the agents: nothing here is the host's.
    let connect = cli.connect.clone().unwrap_or_else(|| {
        atomics_core::config::Config::load(&atomics_core::paths::config_file())
            .map(|config| config.client.connect)
            .unwrap_or_default()
    });
    if !connect.trim().is_empty() {
        let target = match client::target::Target::parse(&connect) {
            Ok(target) => target,
            Err(error) => {
                client::report_bad_connect(&connect, cli.connect.is_some(), &error);
                std::process::exit(2);
            }
        };
        let size: Option<[f32; 2]> = cli
            .size
            .as_deref()
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?]));
        let launch = client::Launch {
            open: cli
                .open
                .iter()
                .filter_map(|spec| spec.split_once(':'))
                .map(|(agent, dir)| (agent.to_string(), atomics_core::paths::expand(dir)))
                .collect(),
            screenshot: cli.screenshot.clone().map(|path| (path, cli.screenshot_delay)),
        };
        return client::run(target, launch, native_options(size, load_icon()));
    }
    // A link goes to the ATOMICS that is running, and this process ends; otherwise it is handled
    // once the app has loaded.
    let link =
        cli.path.as_deref().and_then(|path| path.to_str()).filter(|arg| open_link::is_link(arg)).map(open_link::parse);
    if let Some(incoming) = &link
        && let Some(code) = open_link::forward(&atomics_core::paths::socket_path(), incoming.clone())
    {
        std::process::exit(code);
    }
    // After a link is forwarded, which needs no shell, and before the desktop starts any thread.
    // The window never waits for the login shell: it takes on the PATH the shell gave last time,
    // and reads the shell's current one in the background.
    // SAFETY: no other thread exists yet.
    unsafe { setup::start_login_path() };
    // Shared, so desktops still run side by side; refused only while a headless host owns the home.
    let _instance = match instance::Lock::desktop(&atomics_core::paths::atomics_home()) {
        Ok(lock) => Some(lock),
        Err(refused @ instance::Refused::Host(..)) => {
            eprintln!("atomics: {refused}");
            std::process::exit(1);
        }
        // A home that cannot be locked (a filesystem without flock) starts as it always has.
        Err(refused) => {
            eprintln!("atomics: {refused}");
            None
        }
    };
    crash::install(&atomics_core::paths::atomics_home());

    let open = cli
        .open
        .iter()
        .filter_map(|spec| spec.split_once(':'))
        .map(|(agent, dir)| (agent.to_string(), atomics_core::paths::expand(dir)))
        .collect();
    let size: Option<[f32; 2]> =
        cli.size.as_deref().and_then(|s| s.split_once('x')).and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?]));
    let options = app::Options {
        projects: cli
            .path
            .iter()
            .filter(|_| link.is_none())
            .map(|p| atomics_core::paths::expand(&p.to_string_lossy()))
            .collect(),
        link,
        workspace: cli.workspace,
        open,
        screenshot: cli.screenshot,
        screenshot_delay: cli.screenshot_delay,
        migration_errors,
        remote_port: None,
    };
    let icon = load_icon();
    let native = native_options(size, icon.clone());
    // Before the event loop: the Apple event that launches the app may arrive before the window exists.
    #[cfg(target_os = "macos")]
    open_link::macos::register();
    eframe::run_native(
        "ATOMICS",
        native,
        Box::new(|cc| {
            // Again once the window exists: AppKit's finishLaunching installs its own handler for the
            // event, replacing the early one.
            #[cfg(target_os = "macos")]
            open_link::macos::register();
            Ok(Box::new(app::App::new(cc, options, &icon)))
        }),
    )
}

/// The window: without `size` it fills the screen, which the layout is built to use.
fn native_options(size: Option<[f32; 2]>, icon: egui::IconData) -> eframe::NativeOptions {
    let mut native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ATOMICS")
            .with_app_id("atomics")
            .with_icon(icon)
            .with_inner_size(size.unwrap_or([1600.0, 1000.0]))
            .with_maximized(size.is_none())
            .with_min_inner_size([480.0, 320.0]),
        ..Default::default()
    };
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut native.wgpu_options.wgpu_setup {
        let device_descriptor = setup.device_descriptor.clone();
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut descriptor = device_descriptor(adapter);
            // eframe's 8192-pixel default is too small for wide or tall Retina windows.
            descriptor.required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d;
            descriptor
        });
    }
    native
}

fn load_icon() -> egui::IconData {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../../../assets/icon.png")));
    let mut reader = decoder.read_info().expect("embedded icon must be a valid PNG");
    let mut rgba = vec![0; reader.output_buffer_size().expect("embedded icon must fit in memory")];
    let info = reader.next_frame(&mut rgba).expect("embedded icon must decode");
    rgba.truncate(info.buffer_size());
    egui::IconData { rgba, width: info.width, height: info.height }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn named_credentials_setup_has_no_value_argument() {
        for kind in ["claude", "openai", "linear", "github", "registry"] {
            assert!(Cli::try_parse_from(["atomics", "setup", "--credential", kind]).is_ok());
        }
        assert!(Cli::try_parse_from(["atomics", "setup", "--credential", "openai", "--value", "secret"]).is_err());
        assert!(Cli::try_parse_from(["atomics", "setup", "--credential", "openai", "secret"]).is_err());
    }

    #[test]
    fn setup_and_sandbox_setup_parse() {
        let cli = Cli::try_parse_from(["atomics", "setup"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Setup { credential: None, .. })));
        let cli = Cli::try_parse_from(["atomics", "sandbox", "setup"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::Setup })));
        let cli = Cli::try_parse_from(["atomics", "sandbox", "relay-check"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::RelayCheck })));
        let cli =
            Cli::try_parse_from(["atomics", "sandbox", "azure-check", "11111111-1111-1111-1111-111111111111"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::AzureCheck { .. } })));
        assert!(Cli::try_parse_from(["atomics", "sandbox", "azure-check"]).is_err());
        for args in [
            &["atomics", "--workspace", "daily", "setup"][..],
            &["atomics", "setup", "--workspace", "daily"],
            &["atomics", "sandbox", "setup", "-w", "daily"],
            &["atomics", "doctor", "-w", "daily"],
            // What a restart passes: the open workspace as one argument, after the rest.
            &["atomics", "/repo", "--size", "800x600", "--workspace=daily"],
            &["atomics", "host", "/repo", "--workspace=daily"],
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().workspace.as_deref(), Some("daily"), "{args:?}");
        }
        // A queued command keeps its own flags.
        let cli = Cli::try_parse_from(["atomics", "queue", "--class", "test", "--", "make", "-w", "x"]).unwrap();
        assert!(cli.workspace.is_none());
        let Some(Command::Queue { command, .. }) = cli.command else { panic!("queue command") };
        assert_eq!(command, ["make", "-w", "x"]);
    }

    #[test]
    fn a_link_is_taken_for_the_path_argument() {
        let cli = Cli::try_parse_from(["atomics", "atomics://open?plan=a"]).unwrap();
        let path = cli.path.unwrap();
        assert!(open_link::is_link(path.to_str().unwrap()));
    }

    #[test]
    fn a_link_after_the_end_of_options_is_the_path_argument() {
        let cli = Cli::try_parse_from(["atomics", "--", "atomics://open?plan=a"]).unwrap();
        assert!(cli.command.is_none());
        let path = cli.path.unwrap();
        assert_eq!(path.to_str(), Some("atomics://open?plan=a"));
        assert!(open_link::is_link(path.to_str().unwrap()));
    }

    #[test]
    fn host_takes_projects_and_the_workspace_and_plain_atomics_stays_the_desktop() {
        let cli = Cli::try_parse_from(["atomics", "host", "-w", "daily", "~/repos/a", "/repos/b"]).unwrap();
        let Some(Command::Host { projects, remote_port }) = cli.command else { panic!("host command") };
        assert_eq!(remote_port, None);
        assert_eq!(projects, [PathBuf::from("~/repos/a"), PathBuf::from("/repos/b")]);
        assert_eq!(cli.workspace.as_deref(), Some("daily"));
        let cli = Cli::try_parse_from(["atomics"]).unwrap();
        assert!(cli.command.is_none() && cli.path.is_none());
        // A folder named like the subcommand is still opened through `--` or a path.
        for (arg, folder) in [(&["--", "host"][..], "host"), (&["./host"], "./host")] {
            let cli = Cli::try_parse_from(["atomics"].iter().chain(arg)).unwrap();
            assert!(cli.command.is_none(), "{arg:?}");
            assert_eq!(cli.path, Some(PathBuf::from(folder)));
        }
    }

    #[test]
    fn hook_parses_the_chief_flag() {
        for chief in [false, true] {
            let mut args = vec!["atomics", "hook", "--pane", "chief"];
            if chief {
                args.push("--chief");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Some(Command::Hook { chief: parsed, .. }) if parsed == chief));
        }
    }

    #[test]
    fn print_hook_settings_selects_the_launch_settings() {
        let cli = Cli::try_parse_from(["atomics", "print-hook-settings"]).unwrap();
        let Some(Command::HooksJson { agent, pane }) = cli.command else { panic!("settings command") };
        let settings = atomics_core::hook::session_settings(
            std::path::Path::new("/atomics"),
            std::path::Path::new("/socket"),
            &pane,
            agent == "codex",
        );
        assert!(settings["hooks"].get("PermissionRequest").is_some());
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "/atomics hook --socket /socket");
        assert!(Cli::try_parse_from(["atomics", "hooks-json", "--agent", "claude"]).is_ok());
    }
}

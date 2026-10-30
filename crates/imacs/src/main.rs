#[cfg(test)]
#[path = "../../../tests/support.rs"]
mod test_support;

#[cfg(test)]
mod platform_tests;

mod app;
mod crash;
mod diff_view;
mod diff_worker;
mod doctor;
mod governor;
mod highlight;
mod hook_client;
mod host;
mod ipc;
mod lifecycle;
mod mcp;
mod mcp_client;
mod ref_guard;
mod relay;
mod restart;
mod sandbox;
mod sandbox_ownership;
mod sandbox_usage;
mod setup;
mod statusline;
mod terminal;
mod theme;
mod tickets;
mod usage;
mod voice;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "imacs", version = env!("IMACS_BUILD_VERSION"), about = "Review-first workspace for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Project (git repository) to open; any folder inside it or its worktrees works
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
}

#[derive(Subcommand)]
enum Command {
    /// Guide fresh-machine sandbox setup (safe to run again)
    Setup,
    /// Set up Docker Sandboxes or build warm repository templates
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Check required tools, configuration and integrations
    Doctor,
    /// Hook handler for Claude Code and Codex (reads the event on stdin)
    Hook {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        pane: Option<String>,
    },
    /// Forward Claude's status line data to IMACS, preserving the user's command
    Statusline {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Add IMACS's hooks to ~/.claude/settings.json so every Claude session reports to IMACS
    InstallHooks,
    /// Print the hook settings IMACS passes to the agents it starts
    #[command(name = "print-hook-settings", alias = "hooks-json")]
    HooksJson {
        #[arg(long, default_value = "codex", value_parser = ["codex", "claude"])]
        agent: String,
        /// Pane ID used by Claude hooks; Codex hook definitions do not contain it
        #[arg(long, default_value = "PANE_ID")]
        pane: String,
    },
    /// MCP server (stdio) that lets another agent start, read, review and steer agents in IMACS
    Mcp {
        /// Expose reporting and scoped ticket read/comment tools for task agents
        #[arg(long)]
        agent: bool,
        /// Enable chief-only tools (used by the built-in chief session)
        #[arg(long, conflicts_with = "agent")]
        chief: bool,
    },
    /// Send one control request to the running IMACS, e.g. '{"cmd":"list"}'
    Ctl { json: String },
    /// Run a heavy command, such as a full test run or a QA instance, once IMACS has a free slot
    Queue {
        #[arg(long, default_value = "test", value_parser = ["test", "qa"])]
        class: String,
        /// The command, after `--`
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Set up sandboxed agents: tools, Docker sign-in, network policy and credentials (over stdin)
    Setup,
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Check that sandboxed Claude answers on the Claude subscription through IMACS's relay, with no
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
    },
}

/// Markers of the Claude Code or Codex session IMACS may have been started from. Agents
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
    let cli = Cli::parse();
    let mut migration_errors = Vec::new();
    if cli.command.is_none() {
        for var in PARENT_SESSION_VARS {
            // SAFETY: runs first thing in main, before any other thread exists.
            unsafe { std::env::remove_var(var) };
        }
        let migration = imacs_core::paths::migrate_from_cox();
        for moved in migration.moved {
            eprintln!("imacs: {moved}");
        }
        for error in &migration.errors {
            eprintln!("imacs: {error}");
        }
        migration_errors = migration.errors;
    }
    match cli.command {
        Some(Command::Setup | Command::Sandbox { command: SandboxCommand::Setup }) => {
            if let Err(err) = sandbox::setup::run(cli.workspace.as_deref()) {
                eprintln!("Sandbox setup failed: {err}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Sandbox {
            command: SandboxCommand::Template { command: TemplateCommand::Build { project, agent, dotnet_project } },
        }) => {
            if let Err(error) =
                sandbox::template::run(&project, cli.workspace.as_deref(), &agent, dotnet_project.as_deref())
            {
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
        Some(Command::Doctor) => std::process::exit(doctor::run(cli.workspace.as_deref())),
        Some(Command::Hook { socket, pane }) => {
            hook_client::run(socket, pane);
            return Ok(());
        }
        Some(Command::Statusline { socket }) => {
            statusline::run(socket.unwrap_or_else(imacs_core::paths::socket_path));
            return Ok(());
        }
        Some(Command::InstallHooks) => {
            match setup::install_global_hooks() {
                Ok(path) => println!("IMACS hooks added to {} (backup next to it)", path.display()),
                Err(err) => eprintln!("installing hooks failed: {err:#}"),
            }
            return Ok(());
        }
        Some(Command::HooksJson { agent, pane }) => {
            let exe = setup::executable_path().unwrap_or_else(|_| PathBuf::from("imacs"));
            println!(
                "{}",
                serde_json::to_string_pretty(&imacs_core::hook::session_settings(
                    &exe,
                    &imacs_core::paths::socket_path(),
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
        Some(Command::Queue { class, command }) => {
            let class = imacs_core::governor::SlotClass::parse(&class).unwrap_or(imacs_core::governor::SlotClass::Test);
            std::process::exit(governor::run_queued(&imacs_core::paths::socket_path(), class, &command));
        }
        Some(Command::Ctl { json }) => {
            let response = serde_json::from_str::<imacs_core::control::ControlRequest>(&json)
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
    crash::install(&imacs_core::paths::imacs_home());

    let open = cli
        .open
        .iter()
        .filter_map(|spec| spec.split_once(':'))
        .map(|(agent, dir)| (agent.to_string(), imacs_core::paths::expand(dir)))
        .collect();
    let size: Option<[f32; 2]> =
        cli.size.as_deref().and_then(|s| s.split_once('x')).and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?]));
    let options = app::Options {
        projects: cli.path.iter().map(|p| imacs_core::paths::expand(&p.to_string_lossy())).collect(),
        workspace: cli.workspace,
        open,
        screenshot: cli.screenshot,
        screenshot_delay: cli.screenshot_delay,
        migration_errors,
    };
    let icon = load_icon();
    let mut native = eframe::NativeOptions {
        // Without --size IMACS fills the screen: the layout is built to use all of it.
        viewport: egui::ViewportBuilder::default()
            .with_title("IMACS")
            .with_app_id("imacs")
            .with_icon(icon.clone())
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
    eframe::run_native("IMACS", native, Box::new(|cc| Ok(Box::new(app::App::new(cc, options, &icon)))))
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
    fn setup_and_sandbox_setup_parse() {
        let cli = Cli::try_parse_from(["imacs", "setup"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Setup)));
        let cli = Cli::try_parse_from(["imacs", "sandbox", "setup"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::Setup })));
        let cli = Cli::try_parse_from(["imacs", "sandbox", "relay-check"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::RelayCheck })));
        let cli =
            Cli::try_parse_from(["imacs", "sandbox", "azure-check", "11111111-1111-1111-1111-111111111111"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Sandbox { command: SandboxCommand::AzureCheck { .. } })));
        assert!(Cli::try_parse_from(["imacs", "sandbox", "azure-check"]).is_err());
        for args in [
            &["imacs", "--workspace", "daily", "setup"][..],
            &["imacs", "setup", "--workspace", "daily"],
            &["imacs", "sandbox", "setup", "-w", "daily"],
            &["imacs", "doctor", "-w", "daily"],
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().workspace.as_deref(), Some("daily"), "{args:?}");
        }
        // A queued command keeps its own flags.
        let cli = Cli::try_parse_from(["imacs", "queue", "--class", "test", "--", "make", "-w", "x"]).unwrap();
        assert!(cli.workspace.is_none());
        let Some(Command::Queue { command, .. }) = cli.command else { panic!("queue command") };
        assert_eq!(command, ["make", "-w", "x"]);
    }

    #[test]
    fn print_hook_settings_selects_the_launch_settings() {
        let cli = Cli::try_parse_from(["imacs", "print-hook-settings"]).unwrap();
        let Some(Command::HooksJson { agent, pane }) = cli.command else { panic!("settings command") };
        let settings = imacs_core::hook::session_settings(
            std::path::Path::new("/imacs"),
            std::path::Path::new("/socket"),
            &pane,
            agent == "codex",
        );
        assert!(settings["hooks"].get("PermissionRequest").is_some());
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "/imacs hook --socket /socket");
        assert!(Cli::try_parse_from(["imacs", "hooks-json", "--agent", "claude"]).is_ok());
    }
}

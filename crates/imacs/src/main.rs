mod app;
mod diff_view;
mod doctor;
mod highlight;
mod hook_client;
mod ipc;
mod mcp;
mod setup;
mod terminal;
mod theme;
mod voice;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "imacs", about = "Review-first workspace for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Project (git repository) to open; any folder inside it or its worktrees works
    path: Option<PathBuf>,
    /// Open a saved workspace (a set of projects)
    #[arg(long, short)]
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
    /// Check required tools, configuration and integrations
    Doctor,
    /// Hook handler for Claude Code and Codex (reads the event on stdin)
    Hook {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        pane: Option<String>,
    },
    /// Add IMACS's hooks to ~/.claude/settings.json so every Claude session reports to IMACS
    InstallHooks,
    /// Print the hook settings IMACS passes to the agents it starts
    HooksJson,
    /// MCP server (stdio) that lets another agent start, read, review and steer agents in IMACS
    Mcp {
        /// Expose only `imacs_report`, for agents IMACS starts to report on their task
        #[arg(long)]
        agent: bool,
    },
    /// Send one control request to the running IMACS, e.g. '{"cmd":"list"}'
    Ctl { json: String },
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
        Some(Command::Doctor) => std::process::exit(doctor::run()),
        Some(Command::Hook { socket, pane }) => {
            hook_client::run(socket, pane);
            return Ok(());
        }
        Some(Command::InstallHooks) => {
            match setup::install_global_hooks() {
                Ok(path) => println!("IMACS hooks added to {} (backup next to it)", path.display()),
                Err(err) => eprintln!("installing hooks failed: {err:#}"),
            }
            return Ok(());
        }
        Some(Command::HooksJson) => {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("imacs"));
            println!("{}", serde_json::to_string_pretty(&imacs_core::hook::claude_settings(&exe)).unwrap_or_default());
            return Ok(());
        }
        Some(Command::Mcp { agent }) => {
            mcp::run(agent);
            return Ok(());
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
    let native = eframe::NativeOptions {
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

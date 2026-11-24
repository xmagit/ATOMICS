use atomics_core::config::ChiefAgent;
use serde_json::json;
use std::path::Path;

pub(super) struct HookLaunch<'a> {
    pub hooks_exe: &'a Path,
    pub socket: &'a Path,
    pub pane: &'a str,
    pub chief: bool,
}

/// Keep permissions in the CLI and the MCP/hook config in a private directory owned by this PTY, made in `parent`
/// ([`crate::lifecycle::LAUNCH_DIR`]). Never touches `~/.cursor` or `~/.copilot`.
pub(super) fn prepare(
    parent: &Path,
    provider: ChiefAgent,
    args: &mut Vec<String>,
    executable: &str,
    mut env: serde_json::Value,
    instructions: Option<&str>,
    hooks: Option<HookLaunch<'_>>,
) -> std::io::Result<tempfile::TempDir> {
    atomics_core::store::private_dir(parent)?;
    let profile = tempfile::Builder::new().prefix("atomics-cli-").tempdir_in(parent)?;
    let root = profile.path();
    let role = if instructions.is_some() { "--chief" } else { "--agent" };
    if instructions.is_some()
        && let Some(map) = env.as_object_mut()
    {
        map.insert("ATOMICS_CHIEF_PROVIDER".into(), provider.name().into());
    }
    let mut server = json!({ "command": executable, "args": ["mcp", role], "env": env });
    match provider {
        ChiefAgent::Copilot => {
            server["type"] = "local".into();
            server["tools"] = json!(["*"]);
            let mcp = root.join("mcp.json");
            let config = json!({ "mcpServers": { "atomics": server } });
            atomics_core::store::write_private(&mcp, config.to_string().as_bytes())?;
            args.extend(["--additional-mcp-config".into(), format!("@{}", mcp.display())]);
            if instructions.is_some() || hooks.is_some() {
                let mut plugin = serde_json::Map::new();
                plugin.insert("name".into(), "atomics-launch".into());
                if let Some(instructions) = instructions {
                    std::fs::create_dir(root.join("agents"))?;
                    plugin.insert("agents".into(), "agents/".into());
                    std::fs::write(
                        root.join("agents/atomics-chief.agent.md"),
                        format!(
                            "---\nname: atomics-chief\ndescription: Coordinate work in the ATOMICS plan board.\n---\n\n{instructions}"
                        ),
                    )?;
                }
                if let Some(hooks) = hooks {
                    std::fs::create_dir(root.join("hooks"))?;
                    plugin.insert("hooks".into(), "hooks/hooks.json".into());
                    let hooks_json = atomics_core::hook::copilot_hooks_config(
                        hooks.hooks_exe,
                        hooks.socket,
                        hooks.pane,
                        hooks.chief,
                    );
                    atomics_core::store::write_private(
                        &root.join("hooks/hooks.json"),
                        hooks_json.to_string().as_bytes(),
                    )?;
                }
                std::fs::write(root.join("plugin.json"), serde_json::Value::Object(plugin).to_string())?;
                args.extend(["--plugin-dir".into(), root.to_string_lossy().into_owned()]);
                if instructions.is_some() {
                    args.extend(["--agent".into(), "atomics-launch:atomics-chief".into()]);
                }
            }
        }
        ChiefAgent::Cursor => {
            // Cursor requires at least three components in the canonical plugin path.
            let root = root.join("plugins").join("atomics-launch");
            let config = json!({ "mcpServers": { "atomics": server } });
            atomics_core::store::write_private(&root.join("mcp.json"), config.to_string().as_bytes())?;
            std::fs::create_dir(root.join(".cursor-plugin"))?;
            let mut manifest = serde_json::Map::new();
            manifest.insert("name".into(), "atomics-launch".into());
            if let Some(hooks) = hooks {
                std::fs::create_dir(root.join("hooks"))?;
                manifest.insert("hooks".into(), "hooks/hooks.json".into());
                let hooks_json =
                    atomics_core::hook::cursor_hooks_config(hooks.hooks_exe, hooks.socket, hooks.pane, hooks.chief);
                atomics_core::store::write_private(&root.join("hooks/hooks.json"), hooks_json.to_string().as_bytes())?;
            }
            std::fs::write(root.join(".cursor-plugin/plugin.json"), serde_json::Value::Object(manifest).to_string())?;
            if let Some(instructions) = instructions {
                std::fs::create_dir(root.join("rules"))?;
                std::fs::write(
                    root.join("rules/atomics-chief.mdc"),
                    format!(
                        "---\ndescription: Coordinate work in the ATOMICS plan board.\nalwaysApply: true\n---\n\n{instructions}"
                    ),
                )?;
            }
            args.extend(["--plugin-dir".into(), root.to_string_lossy().into_owned()]);
        }
        _ => unreachable!("not a plugin-based CLI"),
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_writes_session_local_hooks_only_when_enabled_and_sets_chief_provider() {
        let exe = Path::new("/bin/atomics");
        let sock = Path::new("/tmp/test.sock");
        for provider in [ChiefAgent::Copilot, ChiefAgent::Cursor] {
            let parent = crate::test_support::tempdir().unwrap();
            let mut args = Vec::new();
            let dir = prepare(
                parent.path(),
                provider,
                &mut args,
                "/bin/atomics",
                json!({"ATOMICS_SOCKET": "/tmp/test.sock"}),
                Some("Chief rules"),
                Some(HookLaunch { hooks_exe: exe, socket: sock, pane: "chief", chief: true }),
            )
            .unwrap();
            let hooks_path = if provider == ChiefAgent::Cursor {
                dir.path().join("plugins/atomics-launch/hooks/hooks.json")
            } else {
                dir.path().join("hooks/hooks.json")
            };
            assert!(hooks_path.exists(), "{provider:?} chief hooks written inside temp profile");
            let hooks_text = std::fs::read_to_string(&hooks_path).unwrap();
            assert!(hooks_text.contains(" --chief"));
            let mcp_path = if provider == ChiefAgent::Cursor {
                dir.path().join("plugins/atomics-launch/mcp.json")
            } else {
                dir.path().join("mcp.json")
            };
            let mcp: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
            assert_eq!(mcp["mcpServers"]["atomics"]["env"]["ATOMICS_CHIEF_PROVIDER"], provider.name());

            // Without hooks (old CLI): no hooks.json written.
            let mut old_args = Vec::new();
            let old_dir =
                prepare(parent.path(), provider, &mut old_args, "/bin/atomics", json!({}), None, None).unwrap();
            assert!(!old_dir.path().join("hooks/hooks.json").exists());
            assert!(!old_dir.path().join("plugins/atomics-launch/hooks/hooks.json").exists());
        }
    }
}

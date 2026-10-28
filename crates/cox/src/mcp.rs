//! `cox mcp`: a stdio MCP server that lets another agent run agents inside cox.
//! Each tool call becomes one control request to the running app.

use std::io::{BufRead, Write};

use cox_core::control::ControlRequest;
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// `agent` serves only `cox_report`: agents cox starts use it to report on their task.
pub fn run(agent: bool) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            write(
                &mut stdout,
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }),
            );
            continue;
        };
        // Notifications carry no id and get no answer.
        let Some(id) = message.get("id").cloned() else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "cox", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": if agent { AGENT_INSTRUCTIONS } else { INSTRUCTIONS },
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": if agent { agent_tools() } else { tools() } })),
            "tools/call" => Ok(if agent { call_report(&params) } else { call(&params) }),
            _ => Err(json!({ "code": -32601, "message": format!("unknown method {method}") })),
        };
        let reply = match response {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        write(&mut stdout, reply);
    }
}

fn write(out: &mut impl Write, value: Value) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

const INSTRUCTIONS: &str = "cox runs coding agents (Claude Code, Codex, shells) in terminals the user watches, each in its own git worktree, and shows every worktree's diff against main. As an orchestrator: write a plan with cox_plan_set and let the user approve it on the board, then cox_plan_dispatch. Loop on cox_wait instead of polling: agents report with cox_report (status reported) or finish turns. Then read cox_result and cox_diff, steer with cox_send or inline review comments (cox_comment + cox_send_review), and move tasks along with cox_plan_update. Merge each accepted task into the plan's integration branch with cox_plan_integrate before dispatching tasks that depend on it: those branch from the integration branch. For one-off work, cox_start starts an agent directly. The user sees everything and can steer too, so keep briefs and reviews readable.";

const AGENT_INSTRUCTIONS: &str = "You run inside cox, where an orchestrator and the user follow your work. Call cox_report when you finish your task (status done), when you are blocked or need a decision (blocked or question), and optionally to report progress.";

fn agent_tools() -> Value {
    json!([{
        "name": "cox_report",
        "description": "Report on your task to the orchestrator and the user: done when finished (say what changed and how you checked it), blocked or question when you need a decision, progress for a milestone.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["done", "blocked", "question", "progress"] },
                "summary": { "type": "string" }
            },
            "required": ["status", "summary"]
        }
    }])
}

fn call_report(params: &Value) -> Value {
    if params.get("name").and_then(Value::as_str) != Some("cox_report") {
        return tool_error("only cox_report is available here".into());
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let request = ControlRequest::Report {
        pane: std::env::var("COX_PANE").ok(),
        status: args.get("status").and_then(Value::as_str).unwrap_or("progress").to_string(),
        summary: args.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
    };
    match crate::ipc::request(&request) {
        Ok(response) if response.ok => {
            json!({ "content": [{ "type": "text", "text": "Reported. The orchestrator and the user can see it." }], "isError": false })
        }
        Ok(response) => tool_error(response.error.unwrap_or_else(|| "failed".into())),
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn tools() -> Value {
    let session = json!({ "type": "string", "description": "Session id, unique id prefix, or title" });
    json!([
        {
            "name": "cox_list",
            "description": "List the sessions in cox with status (working, needs you, turn done, idle, exited), folder, branch and diff size.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "cox_start",
            "description": "Start an agent in cox. Give `folder` to run in an existing folder, or `repo` and `branch` to create a new git worktree for it. `prompt` is its first task; `args` are extra command-line arguments (for example [\"-m\", \"gpt-6.1-terra\"] for Codex).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent name from the cox config: claude, codex or shell" },
                    "folder": { "type": "string" },
                    "repo": { "type": "string", "description": "Repository to branch a new worktree from" },
                    "branch": { "type": "string", "description": "Branch name for the new worktree" },
                    "prompt": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["agent"]
            }
        },
        {
            "name": "cox_plan_set",
            "description": "Write the plan for a goal as tasks. It appears on cox's plan board, where the user reviews, edits and approves it; nothing starts before approval. Each task: id (short, e.g. t1), title, brief (what to do, with context), acceptance (list of checks that mean done), depends_on (task ids), agent (claude or codex), args (e.g. [\"-m\", \"gpt-6.1-terra\"]), repo (project folder), branch. Re-sending a plan keeps the progress of tasks whose id stays. Tasks are merged into the integration branch (default cox/plan-<goal>, kept while the goal stays the same).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string" },
                    "integration": { "type": "string", "description": "Integration branch name, if not cox/plan-<goal>" },
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string" },
                                "brief": { "type": "string" },
                                "acceptance": { "type": "array", "items": { "type": "string" } },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "agent": { "type": "string" },
                                "args": { "type": "array", "items": { "type": "string" } },
                                "repo": { "type": "string" },
                                "branch": { "type": "string" }
                            },
                            "required": ["id", "title", "brief"]
                        }
                    }
                },
                "required": ["goal", "tasks"]
            }
        },
        {
            "name": "cox_plan_get",
            "description": "The plan with every task's status (planned, running, blocked, review, done), its agent session, the agent's last report, notes, whether the user approved it, and which tasks are ready to start.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "cox_plan_update",
            "description": "Change a task's status (e.g. review → done after you verified it, or back to running after sending feedback) and/or add a note the user sees on the board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "status": { "type": "string", "enum": ["planned", "running", "blocked", "review", "done"] },
                    "note": { "type": "string" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "cox_plan_dispatch",
            "description": "Start one task (id) or every ready task: each gets a new worktree (from the integration branch when it has dependencies, else from main) and an agent whose first message is the task brief. Only works once the user approved the plan. Agents report back with cox_report, which wakes cox_wait.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } }
        },
        {
            "name": "cox_plan_integrate",
            "description": "Merge a task (in review or done) into the plan's integration branch: uncommitted changes in its worktree are committed on its branch first (message '<title> (<id>)'), then the branch is merged in a separate integration worktree next to the repository. Tasks with dependencies branch from the integration branch, so integrate accepted tasks before dispatching their dependents. On a conflict the merge is aborted, and the task stays in review with a note listing the conflicted files.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }
        },
        {
            "name": "cox_wait",
            "description": "Block until an agent finishes a turn, needs input, or exits (or other `statuses`), then return those events with a `cursor`. Pass the cursor as `after` next time to only see newer events. Use this instead of polling. `sessions` empty means any; timeout_secs defaults to 120 (max 280).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessions": { "type": "array", "items": { "type": "string" } },
                    "statuses": { "type": "array", "items": { "type": "string", "enum": ["working", "turn done", "needs you", "idle", "exited"] } },
                    "after": { "type": "integer" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 280 }
                }
            }
        },
        {
            "name": "cox_result",
            "description": "What an agent produced: status, its last prompt, its final message (Claude), the tail of its screen, and its diff size. Read this when cox_wait reports a turn done.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "cox_send",
            "description": "Send a message to an agent, as if the user typed it and pressed Enter. With raw=true the text is written as keystrokes instead (escapes: \\r Enter, \\e Escape, \\t Tab, \\xNN byte), for answering menus and prompts.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "text": { "type": "string" }, "raw": { "type": "boolean" } },
                "required": ["session", "text"]
            }
        },
        {
            "name": "cox_read",
            "description": "Read what an agent's terminal shows right now (the last `lines` lines), with its status.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "lines": { "type": "integer", "minimum": 1 } },
                "required": ["session"]
            }
        },
        {
            "name": "cox_diff",
            "description": "The agent's worktree diff against the merge-base with main: committed, uncommitted and untracked changes. patch=true includes the hunks.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "patch": { "type": "boolean" } },
                "required": ["session"]
            }
        },
        {
            "name": "cox_comment",
            "description": "Leave a review comment on a line of an agent's diff. The user sees it inline in cox's diff panel; nothing reaches the agent until cox_send_review.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": { "type": "string", "description": "File path as shown by cox_diff" },
                    "line": { "type": "integer", "description": "Line number in the new file (or old file with removed=true)" },
                    "removed": { "type": "boolean" },
                    "text": { "type": "string" }
                },
                "required": ["session", "path", "text"]
            }
        },
        {
            "name": "cox_send_review",
            "description": "Send all pending review comments for a session's worktree to its agent as one message.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "cox_focus",
            "description": "Bring a session to the front in cox and show its diff to the user.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "cox_open",
            "description": "Show a file of the session's worktree to the user in cox: its diff hunks when changed, the whole file otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "path": { "type": "string" } },
                "required": ["session", "path"]
            }
        },
        {
            "name": "cox_edit",
            "description": "Open a file of the session's worktree in cox's editor (Helix by default) at a line, so the user can edit it right there.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "path": { "type": "string" }, "line": { "type": "integer" } },
                "required": ["session", "path"]
            }
        },
        {
            "name": "cox_close",
            "description": "Stop a session's process and remove it from cox.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        }
    ])
}

fn call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let mut args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let cmd = match name {
        "cox_list" => "list",
        "cox_start" => "start",
        "cox_send" => "send",
        "cox_read" => "read",
        "cox_diff" => "diff",
        "cox_focus" => "activate",
        "cox_open" => "open",
        "cox_edit" => "edit",
        "cox_wait" => "wait",
        "cox_plan_set" => "plan_set",
        "cox_plan_get" => "plan_get",
        "cox_plan_update" => "plan_update",
        "cox_plan_dispatch" => "plan_dispatch",
        "cox_plan_integrate" => "plan_integrate",
        "cox_result" => "result",
        "cox_comment" => "comment",
        "cox_send_review" => "send_review",
        "cox_close" => "close",
        _ => return tool_error(format!("unknown tool {name}")),
    };
    if let Some(map) = args.as_object_mut() {
        map.insert("cmd".into(), json!(cmd));
    }
    let request: ControlRequest = match serde_json::from_value(args) {
        Ok(request) => request,
        Err(err) => return tool_error(format!("invalid arguments: {err}")),
    };
    match crate::ipc::request(&request) {
        Ok(response) if response.ok => {
            let text = match response.result.get("diff").and_then(Value::as_str) {
                Some(diff) => diff.to_string(),
                None => match response.result.get("screen").and_then(Value::as_str) {
                    Some(screen) => {
                        format!("status: {}\n\n{screen}", response.result["status"].as_str().unwrap_or("?"))
                    }
                    None => serde_json::to_string_pretty(&response.result).unwrap_or_default(),
                },
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Ok(response) => tool_error(response.error.unwrap_or_else(|| "failed".into())),
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn tool_error(message: String) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

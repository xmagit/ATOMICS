//! `imacs mcp`: a stdio MCP server that lets another agent run agents inside IMACS.
//! Each tool call becomes one control request to the running app.

use std::io::{BufRead, Write};

use imacs_core::control::ControlRequest;
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Task agents get reporting and session-scoped ticket tools, never the external server.
pub fn run(agent: bool, chief: bool) {
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
                    "serverInfo": { "name": "imacs", "title": "IMACS", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": if agent { AGENT_INSTRUCTIONS } else { INSTRUCTIONS },
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": available_tools(agent, chief) })),
            "tools/call" => Ok(if agent {
                call_agent(&params)
            } else if let Some(name) = params["name"].as_str().filter(|name| !chief && CHIEF_ONLY.contains(name)) {
                tool_error(format!("{name} is only available to the chief"))
            } else {
                call(&params)
            }),
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

const INSTRUCTIONS: &str = "IMACS runs coding agents (Claude Code, Codex, shells) in terminals the user watches, each in its own git worktree, and shows every worktree's diff against main. As an orchestrator: write a plan with imacs_plan_set and let the user approve it on the board, then imacs_plan_dispatch. Loop on imacs_wait instead of polling: agents report with imacs_report (status reported) or finish turns. Then read imacs_result and imacs_diff, steer with imacs_send or inline review comments (imacs_comment + imacs_send_review), and move tasks along with imacs_plan_update. Merge each accepted task into the plan's integration branch with imacs_plan_integrate before dispatching tasks that depend on it: those branch from the integration branch. For one-off work, imacs_start starts an agent directly. Several plans can run at once: retain each returned plan id and pass plan to every plan tool. Start the next plan while one finishes. The chief is never closed. With a ticket system configured, plans and tasks carry ticket links (ticket in imacs_plan_set, or imacs_ticket_link): IMACS keeps their states in step, comments on them, and reports changes made in the ticket system as ticket events in imacs_wait. The user sees everything and can steer too, so keep briefs and reviews readable.";

const AGENT_INSTRUCTIONS: &str = "You run inside IMACS, where an orchestrator and the user follow your work. Call imacs_report when you finish your task (status done), when you are blocked or need a decision (blocked or question), and optionally to report progress. IMACS posts your report to the linked ticket when configured; do not post a second copy. With tickets.comments = agents, imacs_agent_ticket_get reads your task's ticket (plan=true reads its board or plan ticket), and imacs_agent_ticket_comment adds a signed comment to your own task's ticket for questions or implementation details. These tools cannot choose another task or change ticket state.";

fn agent_tools() -> Value {
    json!([{
        "name": "imacs_report",
        "description": "Report on your task to the orchestrator and the user: done when finished (say what changed and how you checked it), blocked or question when you need a decision, progress for a milestone.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["done", "blocked", "question", "progress"] },
                "summary": { "type": "string" }
            },
            "required": ["status", "summary"]
        }
    }, {
        "name": "imacs_agent_ticket_get",
        "description": "Read your task's linked ticket through the configured get operation, including its details. Set plan=true to read the plan's board or ticket instead. Requires tickets.comments = agents. Your session determines the task; no other ticket can be selected.",
        "inputSchema": { "type": "object", "properties": {
            "plan": { "type": "boolean", "description": "Read the plan link instead of your task ticket (default false)" }
        }, "additionalProperties": false }
    }, {
        "name": "imacs_agent_ticket_comment",
        "description": "Add a signed comment to your own task's ticket through the configured comment operation. Requires tickets.comments = agents and a ticket linked to your task. Use imacs_report for reports; IMACS posts those automatically.",
        "inputSchema": { "type": "object", "properties": {
            "text": { "type": "string" }
        }, "required": ["text"], "additionalProperties": false }
    }])
}

/// Tools only the chief's own server offers: restarting IMACS and configuring it.
const CHIEF_ONLY: &[&str] = &["imacs_restart", "imacs_config_get", "imacs_config_propose"];

fn available_tools(agent: bool, chief: bool) -> Value {
    if agent {
        return agent_tools();
    }
    let mut tools = tools();
    if !chief {
        tools.as_array_mut().unwrap().retain(|tool| !CHIEF_ONLY.iter().any(|name| tool["name"] == *name));
    }
    tools
}

fn call_agent(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    if !agent_tools().as_array().unwrap().iter().any(|tool| tool["name"] == name) {
        return tool_error("only reporting and scoped ticket read/comment tools are available here".into());
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let request = match request_for_tool(name, args) {
        Ok(request) => request,
        Err(err) => return tool_error(err),
    };
    match crate::ipc::request(&request) {
        Ok(response) if response.ok && name == "imacs_report" => {
            json!({ "content": [{ "type": "text", "text": "Reported. The orchestrator and the user can see it." }], "isError": false })
        }
        Ok(response) if response.ok => {
            json!({ "content": [{"type":"text", "text": response.result.to_string()}], "isError":false })
        }
        Ok(response) => tool_error(response.error.unwrap_or_else(|| "failed".into())),
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn ticket_schema() -> Value {
    json!({
        "type": "object",
        "description": "A ticket in the configured ticket system. id is what its tools address it by; the rest is filled in from the system.",
        "properties": {
            "id": { "type": "string" },
            "key": { "type": "string", "description": "Human-readable key, if different from id" },
            "url": { "type": "string" },
            "title": { "type": "string" },
            "system": { "type": "string", "description": "Defaults to the configured system" }
        },
        "required": ["id"]
    })
}

fn ticket_tools(plan: &Value) -> Value {
    let ticket = ticket_schema();
    let ticket_ref = |what: &str| json!({ "type": "string", "description": what });
    json!([
        {
            "name": "imacs_ticket_link",
            "description": "Attach a ticket to a plan (no task: the board in board mode, the plan's one ticket in ticket mode) or to a task, then read it from the ticket system. Or create a new ticket with the configured create operation and attach it (create: title and body), or remove the link (unlink). Returns the link as read, with an error if the ticket system failed.",
            "inputSchema": { "type": "object", "properties": {
                "plan": plan,
                "task": { "type": "string", "description": "Task id; omit to link the plan" },
                "ticket": ticket,
                "create": { "type": "object", "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string", "description": "What to do: the ticket's description" }
                }, "required": ["title"] },
                "unlink": { "type": "boolean" }
            } }
        },
        {
            "name": "imacs_ticket_sync",
            "description": "Read the linked tickets of one plan (or every open plan) from the ticket system now, retry failed state updates, and return each plan's links. In board mode this includes the board's tickets and which task each belongs to, so tickets without a task stand out.",
            "inputSchema": { "type": "object", "properties": { "plan": { "type": "string", "description": "Plan id; omit for every open plan" } } }
        },
        {
            "name": "imacs_ticket_comment",
            "description": "Comment on a task's ticket (or, in ticket mode, the plan's ticket), signed as the chief: implementation details, decisions, anything the ticket's readers should know. Refused when comments are off.",
            "inputSchema": { "type": "object", "properties": {
                "plan": plan,
                "task": { "type": "string", "description": "Task id; omit for the plan's ticket" },
                "text": { "type": "string" }
            }, "required": ["text"] }
        },
        {
            "name": "imacs_ticket_event",
            "description": "For external systems and webhook bridges: a ticket (id or key) or a board changed. Given fields (state, title, url, updated_at) apply to the linked tickets at once, and IMACS reads them again; state changes reach the chief as ticket events.",
            "inputSchema": { "type": "object", "properties": {
                "system": ticket_ref("Ticket system name; omit to match any"),
                "id": ticket_ref("Ticket id"),
                "key": ticket_ref("Ticket key"),
                "board": ticket_ref("Board id, to read a board-mode plan's board again"),
                "state": ticket_ref("New state name"),
                "title": ticket_ref("New title"),
                "url": ticket_ref("Ticket URL"),
                "updated_at": ticket_ref("When it changed, as the system says")
            } }
        }
    ])
}

fn tools() -> Value {
    let plan = json!({ "type": "string", "description": "Plan id from imacs_plan_set/get. Required for update/dispatch/integrate when several plans are open." });
    let session = json!({ "type": "string", "description": "Session id, unique id prefix, or title" });
    let ticket = ticket_schema();
    let mut tools = json!([
        {
            "name": "imacs_restart",
            "description": "Chief only: restart this installation after rebuilding it. Saves plans, sessions and layout, then automatically resumes the chief. Refuses while agents work, approvals/questions are pending, queue slots are held, jobs run or a plan edit is open. Wait for blockers instead of forcing. Force bypasses busy checks, never executable validation or saving.",
            "inputSchema": { "type": "object", "properties": {
                "force": { "type": "boolean", "default": false },
                "reason": { "type": "string" }
            } }
        },
        {
            "name": "imacs_list",
            "description": "List the sessions in IMACS with status (working, needs you, turn done, idle, exited), folder, branch and diff size.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "imacs_approve",
            "description": "Answer the oldest pending permission request for the specified session, regardless of the selected plan tab. allow_always remembers this exact command and directory until the session closes. If the hook has returned, answers only a visible matching native menu; retry if it is not visible yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "decision": { "type": "string", "enum": ["allow", "allow_always", "deny"] },
                    "message": { "type": "string" }
                },
                "required": ["session", "decision"]
            }
        },
        {
            "name": "imacs_start",
            "description": "Start an agent in IMACS. Give `folder` to run in an existing folder, or `repo` and `branch` to create a new git worktree for it. `prompt` is its first task; `args` are extra command-line arguments (for example [\"-m\", \"gpt-6.1-terra\"] for Codex).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent name from the IMACS config: claude, codex or shell" },
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
            "name": "imacs_plan_set",
            "description": "Write the plan for a goal as tasks. It appears on IMACS's plan board, where the user reviews, edits and approves it; nothing starts before approval. Each task: id (short, e.g. t1), title, brief (what to do, with context), acceptance (list of checks that mean done), depends_on (task ids), agent (claude or codex), args (e.g. [\"-m\", \"gpt-6.1-terra\"]), repo (project folder), branch, and optionally ticket (its ticket in the configured ticket system). ticket on the plan links its board (board mode) or its one ticket (ticket mode). Omitted tickets keep their links. Without plan, creates a separate new plan; pass its returned plan id to update it, keeping progress of tasks whose id stays, even when rewording the goal. Tasks are merged into the integration branch (default imacs/plan-<plan-id>, kept unless explicitly overridden).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan": plan,
                    "goal": { "type": "string" },
                    "integration": { "type": "string", "description": "Integration branch name, if not imacs/plan-<plan-id>" },
                    "ticket": ticket,
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
                                "branch": { "type": "string" },
                                "ticket": ticket
                            },
                            "required": ["id", "title", "brief"]
                        }
                    }
                },
                "required": ["goal", "tasks"]
            }
        },
        {
            "name": "imacs_plan_close",
            "description": "Close a plan as completed or archived. Completed tabs show ✓; archiving hides the tab. Returns only plan id, status and ready task ids (task is null). Use imacs_plan_get with its id for the full plan and task history.",
            "inputSchema": { "type": "object", "properties": {
                "plan": plan, "status": { "type": "string", "enum": ["completed", "archived"] }
            }, "required": ["plan", "status"] }
        },
        {
            "name": "imacs_plan_get",
            "description": "Without plan, lists open plans with goal, done/total, approval and waiting tasks. With plan, returns the full plan with every task's status (planned, running, blocked, review, done), its agent session, the agent's last report, notes, whether the user approved it, and which tasks are ready to start.",
            "inputSchema": { "type": "object", "properties": { "plan": plan } }
        },
        {
            "name": "imacs_plan_update",
            "description": "Change a task's status (e.g. review → done after you verified it, or back to running after sending feedback) and/or add a note the user sees on the board. Returns only plan id/status, the changed task (id, status, latest note, integrated branch), and ready task ids. Use imacs_plan_get for the full plan.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan": plan,
                    "id": { "type": "string" },
                    "status": { "type": "string", "enum": ["planned", "running", "blocked", "review", "done"] },
                    "note": { "type": "string" },
                    "session": { "type": "string", "description": "Link the task to another agent session (after restarting its agent)" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "imacs_plan_dispatch",
            "description": "Start one task (id) or every ready task: each gets a new worktree (from the integration branch when it has dependencies, else from main) and an agent whose first message is the task brief. Only works once the user approved the plan. Agents report back with imacs_report, which wakes imacs_wait.",
            "inputSchema": { "type": "object", "properties": { "plan": plan, "id": { "type": "string" } } }
        },
        {
            "name": "imacs_plan_integrate",
            "description": "Merge a task (in review or done) into the plan's integration branch: uncommitted changes in its worktree are committed on its branch first (message '<title> (<id>)'), then the branch is merged in a separate integration worktree next to the repository. Tasks with dependencies branch from the integration branch, so integrate accepted tasks before dispatching their dependents. Returns only plan id/status, the changed task (id, status, latest note, integrated branch), the integration branch/worktree and committed/merged flags, and ready task ids. Use imacs_plan_get for the full plan. On a conflict the merge is aborted, and the task stays in review with a note listing the conflicted files.",
            "inputSchema": { "type": "object", "properties": { "plan": plan, "id": { "type": "string" } }, "required": ["id"] }
        },
        {
            "name": "imacs_wait",
            "description": "Block until an agent finishes a turn, needs input, exits or reports, or a linked ticket changes in its system (or other `statuses`), then return those events with a `cursor`. Pass the cursor as `after` next time to only see newer events. Use this instead of polling. Heartbeat events contain fleet findings (an empty list means all good). Ticket events say which plan or task's ticket moved from which state to which, and which IMACS status that state means. Config events say whether the user applied or rejected a proposed config change. Plan-session events include plan and task ids; permission events also include the approval request. `sessions` empty means any; timeout_secs defaults to 120 (max 280).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessions": { "type": "array", "items": { "type": "string" } },
                    "statuses": { "type": "array", "items": { "type": "string", "enum": ["working", "turn done", "needs you", "idle", "exited", "reported", "heartbeat", "ticket", "config"] } },
                    "after": { "type": "integer" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 280 }
                }
            }
        },
        {
            "name": "imacs_result",
            "description": "What an agent produced: status, its last prompt, its final message (Claude), the tail of its screen, and its diff size. Read this when imacs_wait reports a turn done.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "imacs_send",
            "description": "Send a message to an agent, as if the user typed it and pressed Enter. Codex sends wait for a visible transcript/queue receipt or prompt hook, retry, and return an error if delivery cannot be confirmed. With raw=true the text is written as keystrokes instead (escapes: \\r Enter, \\e Escape, \\t Tab, \\xNN byte), for terminal controls. While a permission request is pending, plain messages are held until the request finishes and the response says held=true; raw input is rejected. Use imacs_approve to answer permission menus.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "text": { "type": "string" }, "raw": { "type": "boolean" } },
                "required": ["session", "text"]
            }
        },
        {
            "name": "imacs_read",
            "description": "Read what an agent's terminal shows right now (the last `lines` lines), with its status.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "lines": { "type": "integer", "minimum": 1 } },
                "required": ["session"]
            }
        },
        {
            "name": "imacs_diff",
            "description": "The agent's worktree diff against the merge-base with main: committed, uncommitted and untracked changes. patch=true includes the hunks.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "patch": { "type": "boolean" } },
                "required": ["session"]
            }
        },
        {
            "name": "imacs_comment",
            "description": "Leave a review comment on a line of an agent's diff. The user sees it inline in IMACS's diff panel; nothing reaches the agent until imacs_send_review.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": { "type": "string", "description": "File path as shown by imacs_diff" },
                    "line": { "type": "integer", "description": "Line number in the new file (or old file with removed=true)" },
                    "removed": { "type": "boolean" },
                    "text": { "type": "string" }
                },
                "required": ["session", "path", "text"]
            }
        },
        {
            "name": "imacs_send_review",
            "description": "Send all pending review comments for a session's worktree to its agent as one message.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "imacs_focus",
            "description": "Bring a session to the front in IMACS and show its diff to the user.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        },
        {
            "name": "imacs_open",
            "description": "Show a file of the session's worktree to the user in IMACS: its diff hunks when changed, the whole file otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "path": { "type": "string" } },
                "required": ["session", "path"]
            }
        },
        {
            "name": "imacs_edit",
            "description": "Open a file of the session's worktree in IMACS's editor (Helix by default) at a line, so the user can edit it right there.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "path": { "type": "string" }, "line": { "type": "integer" } },
                "required": ["session", "path"]
            }
        },
        {
            "name": "imacs_close",
            "description": "Stop a session's process and remove it from IMACS.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "required": ["session"] }
        }
    ]);
    if let (Some(all), Value::Array(more)) = (tools.as_array_mut(), ticket_tools(&plan)) {
        all.extend(more);
    }
    if let (Some(all), Value::Array(more)) = (tools.as_array_mut(), config_tools()) {
        all.extend(more);
    }
    tools
}

fn config_tools() -> Value {
    json!([
        {
            "name": "imacs_config_get",
            "description": "Chief only: IMACS's effective configuration, each value with its source (default, main for config.toml, host for config.d/<host>.toml, workspace for the active workspace file). Also lists the files and, with project, that project's settings. Secret values read as ***; token_env and api_key_env stay names. Read this before proposing a change.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "A folder in a project, to include that project's settings" },
                "key": { "type": "string", "description": "Only keys under this dotted prefix, such as tickets or layout.sidebar_width" }
            } }
        },
        {
            "name": "imacs_config_propose",
            "description": "Chief only: propose a change to one config file for the user to apply. scope main is config.toml (shared by every machine that shares it), host is config.d/<host>.toml (this machine only), workspace is the active workspace's file ([tickets] and [chief] prompt only). remove takes dotted keys out first; toml then merges in: tables key by key, arrays and other values replace. IMACS checks that the result loads, then shows the user a diff with Apply and Reject, labeling settings that run commands, send credentials or instruct agents. Nothing is written before the user applies it. Returns a proposal id; the outcome (applied, rejected or failed, with the user's note) arrives as a config event in imacs_wait. Keep secrets out of the config: token_env and api_key_env name environment variables.",
            "inputSchema": { "type": "object", "properties": {
                "scope": { "type": "string", "enum": ["main", "host", "workspace"] },
                "toml": { "type": "string", "description": "The change as TOML, for example \"[layout]\\nsidebar_width = 300.0\"" },
                "remove": { "type": "array", "items": { "type": "string" }, "description": "Dotted keys to take out of the file, such as layout.sidebar_width" },
                "reason": { "type": "string", "description": "Why, in a sentence the user reads above the diff" }
            }, "required": ["scope"] }
        }
    ])
}

fn call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let request = match request_for_tool(name, args) {
        Ok(request) => request,
        Err(err) => return tool_error(err),
    };
    match crate::ipc::request(&request) {
        Ok(response) if response.ok => {
            let text = response_text(name, &response.result);
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Ok(response) => tool_error(response.error.unwrap_or_else(|| "failed".into())),
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn response_text(name: &str, result: &Value) -> String {
    if name == "imacs_diff"
        && let Some(diff) = result.get("diff").and_then(Value::as_str)
    {
        return diff.to_string();
    }
    if name == "imacs_read"
        && let Some(screen) = result.get("screen").and_then(Value::as_str)
    {
        return format!("status: {}\n\n{screen}", result["status"].as_str().unwrap_or("?"));
    }
    serde_json::to_string_pretty(result).unwrap_or_default()
}

fn request_for_tool(name: &str, mut args: Value) -> Result<ControlRequest, String> {
    if name == "imacs_agent_ticket_get" || name == "imacs_agent_ticket_comment" {
        return agent_ticket_request(name, args, std::env::var("IMACS_PANE").ok());
    }
    let cmd = match name {
        "imacs_list" => "list",
        "imacs_restart" => "restart",
        "imacs_start" => "start",
        "imacs_send" => "send",
        "imacs_approve" => "approve",
        "imacs_read" => "read",
        "imacs_diff" => "diff",
        "imacs_focus" => "activate",
        "imacs_open" => "open",
        "imacs_edit" => "edit",
        "imacs_wait" => "wait",
        "imacs_plan_set" => "plan_set",
        "imacs_plan_get" => "plan_get",
        "imacs_plan_close" => "plan_close",
        "imacs_plan_update" => "plan_update",
        "imacs_plan_dispatch" => "plan_dispatch",
        "imacs_plan_integrate" => "plan_integrate",
        "imacs_result" => "result",
        "imacs_comment" => "comment",
        "imacs_send_review" => "send_review",
        "imacs_close" => "close",
        "imacs_report" => "report",
        "imacs_ticket_link" => "ticket_link",
        "imacs_ticket_sync" => "ticket_sync",
        "imacs_ticket_comment" => "ticket_comment",
        "imacs_ticket_event" => "ticket_event",
        "imacs_config_get" => "config_get",
        "imacs_config_propose" => "config_propose",
        _ => return Err(format!("unknown tool {name}")),
    };
    if let Some(map) = args.as_object_mut() {
        map.insert("cmd".into(), json!(cmd));
        if cmd == "report" || cmd == "config_propose" {
            map.insert("pane".into(), json!(std::env::var("IMACS_PANE").ok()));
        }
    }
    serde_json::from_value(args).map_err(|err| format!("invalid arguments: {err}"))
}

fn agent_ticket_request(name: &str, args: Value, pane: Option<String>) -> Result<ControlRequest, String> {
    let allowed: &[&str] = match name {
        "imacs_agent_ticket_get" => &["plan"],
        "imacs_agent_ticket_comment" => &["text"],
        _ => return Err(format!("unknown agent ticket tool {name}")),
    };
    let mut map = args.as_object().cloned().ok_or("arguments must be an object")?;
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unexpected agent ticket argument {key}"));
    }
    map.insert("cmd".into(), json!(name.strip_prefix("imacs_").unwrap()));
    map.insert("pane".into(), json!(pane));
    serde_json::from_value(Value::Object(map)).map_err(|err| format!("invalid arguments: {err}"))
}

fn tool_error(message: String) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_ticket_arguments_cannot_choose_identity_or_operations() {
        use super::*;
        for name in ["imacs_agent_ticket_get", "imacs_agent_ticket_comment"] {
            for key in ["pane", "session", "task", "id", "ticket", "operation", "cmd", "create", "state"] {
                let mut args = if name.ends_with("comment") { json!({"text":"hello"}) } else { json!({}) };
                args[key] = json!("someone-else");
                assert!(agent_ticket_request(name, args, Some("own-session".into())).is_err(), "{name}: {key}");
            }
        }
        assert!(agent_ticket_request("imacs_agent_ticket_comment", json!({"text":"x", "plan":true}), None).is_err());
        assert!(matches!(
            agent_ticket_request("imacs_agent_ticket_get", json!({"plan":true}), Some("own-session".into())).unwrap(),
            imacs_core::control::ControlRequest::AgentTicketGet { pane: Some(id), plan: true } if id == "own-session"
        ));
        let names: Vec<_> =
            agent_tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["imacs_report", "imacs_agent_ticket_get", "imacs_agent_ticket_comment"]);
        for name in [
            "imacs_ticket_comment",
            "imacs_ticket_link",
            "imacs_ticket_sync",
            "imacs_ticket_event",
            "imacs_plan_update",
            "save_issue",
        ] {
            assert_eq!(call_agent(&json!({"name":name, "arguments":{}}))["isError"], true);
        }
    }
    #[test]
    fn restart_is_chief_only_and_maps_optional_force_and_reason() {
        for (agent, chief, available) in [(true, false, false), (false, false, false), (false, true, true)] {
            let tools = super::available_tools(agent, chief);
            for name in super::CHIEF_ONLY {
                assert_eq!(tools.as_array().unwrap().iter().any(|tool| tool["name"] == *name), available, "{name}");
            }
        }
        let default = super::request_for_tool("imacs_restart", serde_json::json!({})).unwrap();
        assert!(matches!(default, imacs_core::control::ControlRequest::Restart { force: false, reason: None }));
        let force =
            super::request_for_tool("imacs_restart", serde_json::json!({"force":true,"reason":"upgrade"})).unwrap();
        assert!(
            matches!(force, imacs_core::control::ControlRequest::Restart {force:true,reason:Some(reason)} if reason == "upgrade")
        );
        assert_eq!(super::call_agent(&serde_json::json!({"name":"imacs_restart"}))["isError"], true);
    }
    #[test]
    fn wait_schema_accepts_heartbeat() {
        let tools = super::tools();
        let wait = tools.as_array().unwrap().iter().find(|t| t["name"] == "imacs_wait").unwrap();
        assert!(
            wait["inputSchema"]["properties"]["statuses"]["items"]["enum"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("heartbeat"))
        );
        let request = super::request_for_tool("imacs_wait", serde_json::json!({"statuses":["heartbeat"]})).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap()["statuses"], serde_json::json!(["heartbeat"]));
    }

    use super::*;

    fn example(schema: &Value, required_only: bool) -> Value {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values[0].clone();
        }
        match schema["type"].as_str().unwrap() {
            "object" => schema["properties"]
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| {
                    !required_only || schema["required"].as_array().is_some_and(|keys| keys.contains(&json!(key)))
                })
                .map(|(key, value)| (key.clone(), example(value, required_only)))
                .collect(),
            "array" => json!([example(&schema["items"], required_only)]),
            "string" => json!("example"),
            "integer" => schema.get("minimum").cloned().unwrap_or(json!(1)),
            "boolean" => json!(true),
            other => panic!("unhandled schema type {other}"),
        }
    }

    // ControlRequest permits extra fields, so deserialization alone would miss
    // a property silently discarded because its schema name drifted.
    fn assert_properties_survive(input: &Value, output: &Value) {
        match input {
            Value::Object(properties) => {
                for (key, value) in properties {
                    assert!(output.get(key).is_some(), "missing property {key}");
                    assert_properties_survive(value, &output[key]);
                }
            }
            Value::Array(values) => {
                assert_eq!(values.len(), output.as_array().unwrap().len());
                for (input, output) in values.iter().zip(output.as_array().unwrap()) {
                    assert_properties_survive(input, output);
                }
            }
            _ => assert_eq!(input, output),
        }
    }

    #[test]
    fn result_preserves_approval_but_read_formats_the_screen() {
        let result =
            json!({"status":"needs you", "screen":"menu", "approval":{"command":"touch fixture", "reason":"test"}});
        let output: Value = serde_json::from_str(&response_text("imacs_result", &result)).unwrap();
        assert_eq!(output, result);
        assert_eq!(response_text("imacs_read", &result), "status: needs you\n\nmenu");
    }

    #[test]
    fn every_tool_schema_deserializes_into_its_control_request() {
        for tool in tools().as_array().unwrap().iter().chain(agent_tools().as_array().unwrap()) {
            let name = tool["name"].as_str().unwrap();
            for required_only in [false, true] {
                let args = example(&tool["inputSchema"], required_only);
                let request = request_for_tool(name, args.clone()).unwrap_or_else(|err| panic!("{name}: {err}"));
                let serialized = serde_json::to_value(request).unwrap();
                let cmd = if name == "imacs_focus" { "activate" } else { name.strip_prefix("imacs_").unwrap() };
                assert_eq!(serialized["cmd"], cmd, "{name}");
                assert_properties_survive(&args, &serialized);
            }
        }
    }
}

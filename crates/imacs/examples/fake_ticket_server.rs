//! A fake ticket system behind a stdio MCP server, for IMACS's ticket tests.
//!
//! `fake_ticket_server <state.json>`
//!
//! It speaks two unrelated tool dialects, so tests can swap ticket systems by config
//! alone: "card_*" tools with nested, structured results, and "issue_*" tools with
//! flat results as JSON text. The state file holds `tickets`, every call made
//! (`calls`), tools that fail (`fail`, with an optional `fail_message`) and tools that never answer (`hang`). Each call
//! rereads and rewrites it, so tests can change tickets behind IMACS's back and check
//! what IMACS pushed.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let path = std::env::args().nth(1).expect("usage: fake_ticket_server <state.json>");
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = message.get("id").cloned() else { continue };
        let answer = match message["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-tickets", "version": "1" },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => list(&path, &message["params"]),
            "tools/call" => call(&path, &message["params"]),
            other => Err(format!("unknown method {other}")),
        };
        let reply = match answer {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(message) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": message } }),
        };
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
}

const TOOLS: &[&str] = &[
    "card_get",
    "card_list",
    "card_move",
    "card_note",
    "card_add",
    "issue_read",
    "issue_search",
    "issue_save",
    "issue_comment",
    "issue_create",
    "get_ticket",
    "list_tickets",
    "update_ticket",
    "add_comment",
    "create_ticket",
    "get_issue",
    "save_issue",
    "save_comment",
];

fn list(path: &str, params: &Value) -> Result<Value, String> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    if state["list_hang"] == true {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    let names = state["offered_tools"].as_array().cloned().unwrap_or_else(|| TOOLS.iter().map(|n| json!(n)).collect());
    let start = params["cursor"].as_str().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let size = state["page_size"].as_u64().unwrap_or(names.len() as u64) as usize;
    let end = (start + size).min(names.len());
    let mut page = json!({"tools": names[start..end].iter().map(|name| json!({
        "name": name, "inputSchema": {"type":"object"}
    })).collect::<Vec<_>>()});
    if end < names.len() {
        page["nextCursor"] = json!(end.to_string());
    }
    Ok(page)
}

fn call(path: &str, params: &Value) -> Result<Value, String> {
    let name = params["name"].as_str().unwrap_or_default();
    let args = &params["arguments"];
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    state["calls"].as_array_mut().unwrap().push(json!({ "tool": name, "arguments": args }));
    let listed = |key: &str| state[key].as_array().is_some_and(|tools| tools.iter().any(|t| t == name));
    if listed("hang") {
        save(path, &state);
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    if listed("fail") {
        save(path, &state);
        // `fail_message` lets tests make a failing backend echo whatever they need.
        let text = state["fail_message"].as_str().map_or_else(|| format!("{name} is broken today"), str::to_string);
        return Ok(json!({ "isError": true, "content": [{ "type": "text", "text": text }] }));
    }
    let text = |value: Value| json!({ "content": [{ "type": "text", "text": value.to_string() }] });
    let structured =
        |value: Value| json!({ "structuredContent": value, "content": [{ "type": "text", "text": "ok" }] });
    let result = match name {
        "card_get" => structured(json!({ "card": card(find(&state, &args["card"])?) })),
        "card_list" => structured(json!({ "cards": on_board(&state, &args["board"]).map(card).collect::<Vec<_>>() })),
        "card_move" => {
            change(&mut state, &args["card"], |t| t["state"] = args["column"].clone())?;
            structured(json!({ "moved": true }))
        }
        "card_note" => {
            change(&mut state, &args["card"], |t| comment(t, &args["text"]))?;
            structured(json!({ "noted": true }))
        }
        "card_add" => structured(json!({ "card": card(&create(&mut state, &args["board"], &args["name"])) })),
        "issue_read" => text(issue(find(&state, &args["id"])?)),
        "issue_search" => text(json!({ "issues": on_board(&state, &args["project"]).map(issue).collect::<Vec<_>>() })),
        "issue_save" => {
            change(&mut state, &args["id"], |t| t["state"] = args["state"].clone())?;
            text(json!({ "success": true }))
        }
        "issue_comment" => {
            change(&mut state, &args["issueId"], |t| comment(t, &args["body"]))?;
            text(json!({ "success": true }))
        }
        "issue_create" => text(issue(&create(&mut state, &args["project"], &args["title"]))),
        "get_ticket" => structured(json!({"ticket": relay(find(&state, &args["ticket"])?) })),
        "list_tickets" => {
            structured(json!({"tickets": on_board(&state, &args["board"]).map(relay).collect::<Vec<_>>() }))
        }
        "update_ticket" => {
            change(&mut state, &args["ticket"], |t| t["state"] = args["status"].clone())?;
            structured(json!({"updated":true}))
        }
        "add_comment" => {
            change(&mut state, &args["ticket"], |t| comment(t, &args["body"]))?;
            structured(json!({"posted":true}))
        }
        "create_ticket" => structured(json!({"ticket": relay(&create(&mut state, &args["board"], &args["title"]))})),
        "get_issue" => text(issue(find(&state, &args["id"])?)),
        "save_issue" if args["id"].is_string() => {
            change(&mut state, &args["id"], |t| t["state"] = args["state"].clone())?;
            text(json!({"success":true}))
        }
        "save_issue" => text(issue(&create(&mut state, &args["team"], &args["title"]))),
        "save_comment" => {
            change(&mut state, &args["issueId"], |t| comment(t, &args["body"]))?;
            text(json!({"success":true}))
        }
        _ => return Err(format!("unknown tool {name}")),
    };
    save(path, &state);
    Ok(result)
}

fn find<'a>(state: &'a Value, reference: &Value) -> Result<&'a Value, String> {
    state["tickets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| *reference == t["id"] || *reference == t["key"])
        .ok_or_else(|| format!("no ticket {reference}"))
}

fn on_board<'a>(state: &'a Value, board: &'a Value) -> impl Iterator<Item = &'a Value> {
    state["tickets"].as_array().into_iter().flatten().filter(move |t| t["board"] == *board)
}

fn change(state: &mut Value, reference: &Value, edit: impl FnOnce(&mut Value)) -> Result<(), String> {
    let clock = state["clock"].as_u64().unwrap_or(0) + 1;
    state["clock"] = json!(clock);
    let ticket = state["tickets"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .find(|t| *reference == t["id"] || *reference == t["key"])
        .ok_or_else(|| format!("no ticket {reference}"))?;
    edit(ticket);
    ticket["rev"] = json!(clock);
    Ok(())
}

fn comment(ticket: &mut Value, body: &Value) {
    if !ticket["comments"].is_array() {
        ticket["comments"] = json!([]);
    }
    ticket["comments"].as_array_mut().unwrap().push(body.clone());
}

fn create(state: &mut Value, board: &Value, title: &Value) -> Value {
    let clock = state["clock"].as_u64().unwrap_or(0) + 1;
    state["clock"] = json!(clock);
    let ticket = json!({
        "id": format!("n{clock}"), "key": format!("NEW-{clock}"), "board": board, "title": title,
        "state": "Todo", "url": format!("https://tickets.example/NEW-{clock}"), "rev": clock, "comments": [],
    });
    state["tickets"].as_array_mut().unwrap().push(ticket.clone());
    ticket
}

/// The "card" dialect: nested fields under other names.
fn card(ticket: &Value) -> Value {
    json!({
        "ref": ticket["id"], "code": ticket["key"], "href": ticket["url"], "name": ticket["title"],
        "column": { "title": ticket["state"] }, "touched": format!("rev-{}", ticket["rev"].as_u64().unwrap_or(0)),
    })
}

/// The "issue" dialect: flat fields, addressed by key.
fn issue(ticket: &Value) -> Value {
    json!({
        "id": ticket["key"], "uuid": ticket["id"], "url": ticket["url"], "title": ticket["title"],
        "status": ticket["state"], "updatedAt": format!("rev-{}", ticket["rev"].as_u64().unwrap_or(0)),
        "description": ticket["description"], "comments": ticket["comments"],
    })
}

fn relay(ticket: &Value) -> Value {
    json!({"id":ticket["id"], "key":ticket["key"], "url":ticket["url"], "title":ticket["title"],
        "status":ticket["state"], "updated_at":format!("rev-{}",ticket["rev"].as_u64().unwrap_or(0)),
        "description":ticket["description"], "comments":ticket["comments"]})
}

fn save(path: &str, state: &Value) {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

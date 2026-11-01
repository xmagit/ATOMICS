//! A fake ticket system behind a stdio MCP server, for ATOMICS's ticket tests.
//!
//! `fake_ticket_server <state.json>`
//! `fake_ticket_server --no-read` or `--stop-reading-after-initialize` stalls stdin for transport tests.
//!
//! It speaks three unrelated tool dialects, so tests can swap ticket systems by config
//! alone: "card_*" tools with nested, structured results, and "issue_*" tools with
//! flat results as JSON text, plus a find/get dialect with scored search.
//! A missing state file is seeded with the demo epic and four direct children. The state file holds `tickets`, every call made
//! (`calls`), tools that fail (`fail`, with an optional `fail_message`, one string or an array of blocks) and tools that never answer (`hang`). Each call
//! rereads and rewrites it, so tests can change tickets behind ATOMICS's back and check
//! what ATOMICS pushed.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let path = std::env::args().nth(1).expect("usage: fake_ticket_server <state.json>");
    if path == "--no-read" {
        loop {
            std::thread::park();
        }
    }
    if !std::path::Path::new(&path).exists() && path != "--stop-reading-after-initialize" {
        save(&path, &demo());
    }
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
        if path == "--stop-reading-after-initialize" && message["method"] == "initialize" {
            loop {
                std::thread::park();
            }
        }
    }
}

const TOOLS: &[&str] = &[
    "card_search",
    "card_semantic_search",
    "card_children",
    "card_related",
    "issue_semantic_search",
    "issue_children",
    "issue_related",
    "find_tickets",
    "find_keyword_tickets",
    "find_related_tickets",
    "get_children",
    "list_ticket_children",
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
    "list_issues",
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
        // `fail_message` lets tests make a failing backend echo whatever they need, as one
        // text block or, given an array, as several.
        let blocks = match &state["fail_message"] {
            Value::String(text) => vec![text.clone()],
            Value::Array(texts) => texts.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => vec![format!("{name} is broken today")],
        };
        let content: Vec<Value> = blocks.into_iter().map(|text| json!({ "type": "text", "text": text })).collect();
        return Ok(json!({ "isError": true, "content": content }));
    }
    let text = |value: Value| json!({ "content": [{ "type": "text", "text": value.to_string() }] });
    let structured =
        |value: Value| json!({ "structuredContent": value, "content": [{ "type": "text", "text": "ok" }] });
    let result = match name {
        "card_search" | "card_semantic_search" => {
            structured(json!({"cards": search(&state, &args["query"]).map(card).collect::<Vec<_>>()}))
        }
        "card_children" => structured(json!({"cards": children(&state, &args["card"]).map(card).collect::<Vec<_>>()})),
        "card_related" => structured(json!({"cards": related(&state, &args["card"])? .map(card).collect::<Vec<_>>()})),
        "issue_semantic_search" => {
            text(json!({"issues": search(&state, &args["query"]).map(issue).collect::<Vec<_>>()}))
        }
        "issue_search" if args["query"].is_string() => {
            text(json!({"issues": search(&state, &args["query"]).map(issue).collect::<Vec<_>>()}))
        }
        "issue_children" => text(json!({"issues": children(&state, &args["id"]).map(issue).collect::<Vec<_>>()})),
        "issue_related" => text(json!({"issues": related(&state, &args["id"])? .map(issue).collect::<Vec<_>>()})),
        "find_tickets" | "find_keyword_tickets" => {
            text(json!({"result":{"tickets": search(&state, args.get("about").unwrap_or(&args["query"]))
            .take(args["take"].as_u64().unwrap_or(25).min(25) as usize).map(found_ticket).collect::<Vec<_>>()}}))
        }
        "find_related_tickets" => text(
            json!({"result":{"tickets": related(&state, &args["ticket"])? .map(found_ticket).collect::<Vec<_>>()}}),
        ),
        "get_children" => {
            text(json!({"result":{"tickets": children(&state, &args["ticket"]).map(found_ticket).collect::<Vec<_>>()}}))
        }
        "list_ticket_children" => text(json!({"children": children(&state, &args["ticket"])
            .take(args["take"].as_u64().unwrap_or(50).min(50) as usize).map(found_ticket).collect::<Vec<_>>()})),
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
        "get_ticket" => {
            let ticket = find(&state, args.get("ticketNumber").unwrap_or(&args["ticket"]))?;
            text(json!({"row": found_ticket(ticket), "ticket": full_ticket(ticket), "url": ticket["url"],
                "comments": ticket["comments"], "childCount": children(&state, &ticket["id"]).count()}))
        }
        "list_tickets" => {
            structured(json!({"tickets": on_board(&state, &args["board"]).map(full_ticket).collect::<Vec<_>>() }))
        }
        "update_ticket" => {
            change(&mut state, &args["ticket"], |t| t["state"] = args["status"].clone())?;
            structured(json!({"updated":true}))
        }
        "add_comment" => {
            change(&mut state, &args["ticket"], |t| comment(t, &args["body"]))?;
            structured(json!({"posted":true}))
        }
        "create_ticket" => {
            structured(json!({"ticket": full_ticket(&create(&mut state, &args["board"], &args["title"]))}))
        }
        "list_issues" if args["parentId"].is_string() => {
            text(json!({"issues": children(&state, &args["parentId"]).map(issue).collect::<Vec<_>>()}))
        }
        "list_issues" => text(json!({"issues": search(&state, &args["query"]).map(issue).collect::<Vec<_>>()})),
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
        .find(|t| matches(t, reference))
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
        .find(|t| matches(t, reference))
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
    let number = state["tickets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ticket| ticket["key"].as_str()?.strip_prefix("DEMO-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let ticket = json!({
        "id": format!("n{clock}"), "key": format!("DEMO-{number}"), "board": board, "title": title,
        "state": "Todo", "url": format!("https://tickets.example/DEMO-{number}"), "rev": clock, "comments": [],
    });
    state["tickets"].as_array_mut().unwrap().push(ticket.clone());
    ticket
}

/// The "card" dialect: nested fields under other names.
fn card(ticket: &Value) -> Value {
    json!({
        "ref": ticket["id"], "code": ticket["key"], "href": ticket["url"], "name": ticket["title"],
        "column": { "title": ticket["state"] }, "touched": format!("rev-{}", ticket["rev"].as_u64().unwrap_or(0)),
        "rank": ticket["score"], "excerpt": ticket["snippet"], "owner": ticket["assignee"],
        "parent": ticket["parent"], "type": ticket["kind"], "body": ticket["description"],
        "notes": ticket["comments"], "development": ticket["developmentLinks"],
    })
}

/// The "issue" dialect: flat fields, addressed by key.
fn issue(ticket: &Value) -> Value {
    json!({
        "id": ticket["key"], "uuid": ticket["id"], "url": ticket["url"], "title": ticket["title"],
        "status": ticket["state"], "updatedAt": format!("rev-{}", ticket["rev"].as_u64().unwrap_or(0)),
        "description": ticket["description"], "comments": ticket["comments"],
        "score": ticket["score"], "snippet": ticket["snippet"], "assignee": ticket["assignee"],
        "parent": ticket["parent"], "kind": ticket["kind"], "links": ticket["developmentLinks"],
    })
}

fn full_ticket(ticket: &Value) -> Value {
    json!({"ticketNumber":ticket["key"], "number":ticket["key"], "subject":ticket["title"], "ticketTypeKey":ticket["kind"],
        "parentTicketId":ticket["parent"], "developmentLinks":ticket["developmentLinks"],
        "id":ticket["id"], "key":ticket["key"], "url":ticket["url"], "title":ticket["title"],
        "status":ticket["state"], "updated_at":format!("rev-{}",ticket["rev"].as_u64().unwrap_or(0)),
        "description":ticket["description"], "comments":ticket["comments"], "fields":ticket["fields"]})
}

fn save(path: &str, state: &Value) {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

fn matches(ticket: &Value, reference: &Value) -> bool {
    *reference == ticket["id"]
        || *reference == ticket["key"]
        || reference.as_str().map(str::to_string).or_else(|| reference.as_u64().map(|n| n.to_string())).is_some_and(
            |number| ticket["key"].as_str().and_then(|key| key.rsplit('-').next()) == Some(number.as_str()),
        )
}

fn search<'a>(state: &'a Value, query: &Value) -> impl Iterator<Item = &'a Value> {
    let query = query.as_str().unwrap_or_default().to_lowercase();
    state["tickets"].as_array().into_iter().flatten().filter(move |ticket| {
        query.split_whitespace().all(|word| {
            ["key", "title", "description", "snippet"]
                .iter()
                .any(|key| ticket[*key].as_str().unwrap_or_default().to_lowercase().contains(word))
        })
    })
}

fn children<'a>(state: &'a Value, reference: &Value) -> impl Iterator<Item = &'a Value> {
    let parent = find(state, reference).ok();
    let id = parent.map(|ticket| ticket["id"].clone()).unwrap_or_else(|| reference.clone());
    state["tickets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |ticket| ticket["parent"] == id || ticket["board"] == id)
}

fn related<'a>(state: &'a Value, reference: &Value) -> Result<impl Iterator<Item = &'a Value>, String> {
    let ticket = find(state, reference)?;
    Ok(state["tickets"].as_array().into_iter().flatten().filter(move |other| {
        other["id"] != ticket["id"]
            && (other["parent"] == ticket["id"] || (!ticket["parent"].is_null() && other["parent"] == ticket["parent"]))
    }))
}

fn found_ticket(ticket: &Value) -> Value {
    json!({"id":ticket["id"], "number":ticket["key"], "subject":ticket["title"], "status":ticket["state"],
        "score":ticket["score"], "snippet":ticket["snippet"], "assignee":ticket["assignee"],
        "assigneeDisplayName":ticket["assignee"], "url":ticket["url"],
        "updatedAtUtc":format!("rev-{}", ticket["rev"].as_u64().unwrap_or(0)),
        "parentTicketId":ticket["parent"], "type":ticket["kind"]})
}

fn demo() -> Value {
    let parent = "00000000-0000-4000-8000-000000000001";
    let titles = [
        (1, "Ticket workspace: end-to-end integration and personal-agent MCP sign-in"),
        (2, "Connect any MCP client with browser OAuth"),
        (3, "MCP: list a ticket's children and record development stages"),
        (4, "Generic ticket-layer support"),
        (5, "Run engineering tasks in isolated workspaces"),
    ];
    let tickets: Vec<Value> = titles.into_iter().enumerate().map(|(i, (number, title))| {
        json!({"id":format!("00000000-0000-4000-8000-{number:012}"),
            "key":format!("DEMO-{number}"), "title":title, "state":if i == 1 {"Deploying"} else {"Backlog"},
            "url":format!("https://tickets.example/{number}"), "rev":1, "score":0.82 - i as f64 * 0.01,
            "snippet":format!("Personal-agent ticket workspace: {title}"), "assignee":"Demo User",
            "kind":if i == 0 {"epic"} else {"task"}, "parent":if i == 0 {Value::Null} else {json!(parent)},
            "board":if i == 0 {Value::Null} else {json!(parent)},
            "description":format!("## Goal\n{title}\n\n## Work\nBuild the ticket workspace with generic MCP operations.\n\n## Acceptance\nDocs and tests pass."),
            "comments":[{"authorDisplayName":"Demo agent", "authorIdentityKind":"Agent", "body":"Ready for planning.", "createdAtUtc":"2026-10-31T12:00:00Z"}],
            "developmentLinks":if i == 1 {json!([{"title":"OAuth support", "pullRequestUrl":"https://code.example/pull/1", "pullRequestStatus":"Open", "continuousIntegrationStatus":"Passed"}])} else {json!([])}
        })
    }).collect();
    json!({"tickets":tickets, "calls":[], "fail":[], "hang":[], "clock":1})
}

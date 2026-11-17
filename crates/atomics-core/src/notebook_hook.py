"""Sandbox hook transport. The gateway's host MCP server owns the session capability."""
import json
import pathlib
import sys
import urllib.request


def deliver():
    payload = json.load(sys.stdin)
    event = payload.get("hook_event_name")
    if event not in ("SessionStart", "UserPromptSubmit", "PostToolUse"):
        return
    # Both adapters mark background/subagent hooks; never consume their parent's inbox.
    if ("transcript_path" in payload and payload["transcript_path"] is None) or payload.get("agent_id"):
        return
    home = pathlib.Path.home()
    if sys.argv[1] == "codex":
        import tomllib
        config = tomllib.loads((home / ".codex/config.toml").read_text())["mcp_servers"]["mcp-gateway"]
        headers = dict(config.get("http_headers", {}))
    else:
        config = json.loads((home / ".claude.json").read_text())["mcpServers"]["mcp-gateway"]
        headers = dict(config.get("headers", {}))
    headers.update({"Accept": "application/json, text/event-stream", "Content-Type": "application/json"})
    sequence = 0

    def rpc(method, params):
        nonlocal sequence
        sequence += 1
        request = urllib.request.Request(config["url"], headers=headers, data=json.dumps({
            "jsonrpc": "2.0", "id": sequence, "method": method, "params": params,
        }).encode())
        with urllib.request.urlopen(request, timeout=3) as response:
            if response.headers.get("Mcp-Session-Id"):
                headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
            body = response.read(65536).decode()
        if body.startswith(("event:", "data:")):
            body = next(line[5:].strip() for line in body.splitlines() if line.startswith("data:"))
        return json.loads(body)["result"]

    def call(arguments):
        result = rpc("tools/call", {"name": "atomics_note_delivery", "arguments": arguments})
        if result.get("isError"):
            raise ValueError("Delivery unavailable")
        return json.loads(result["content"][0]["text"])

    rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "atomics-notebook-hook", "version": "1"}})
    batch = call({"event": event, "main": True})
    if not batch.get("context"):
        return
    # Only additionalContext is emitted: no permission, control or Stop decision fields.
    print(json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": batch["context"]}}), flush=True)
    call({"ack": batch["ids"]})


try:
    deliver()
except Exception:
    # Unsupported templates, missing trust or gateway outages leave messages for an explicit pull.
    pass

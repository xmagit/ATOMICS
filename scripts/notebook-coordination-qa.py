#!/usr/bin/env python3
"""Two scoped agents, chief review and real hook/MCP handoffs in an isolated host.

Run through `atomics queue --class qa -- python3 scripts/notebook-coordination-qa.py`.
The workers are inert CLI processes: this tests the transport, not model compliance.
Pass --desktop to also capture the notebook board with the instance's own ctl socket.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--desktop", action="store_true")
    parser.add_argument("--sandboxes", action="store_true", help="Verify both gateway adapters in disposable sbx VMs")
    args = parser.parse_args()
    binary = Path(__file__).resolve().parents[1] / "target/debug/atomics"
    root = Path(tempfile.mkdtemp(prefix="atomics-notes-", dir="/private/tmp" if os.uname().sysname == "Darwin" else None))
    worker = root / "worker.py"
    worker.write_text("import time\nwhile True: time.sleep(1)\n")
    config = f'''[isolation]
mode = "bare"
[plan]
coordination = "relay"
relay_expiry_secs = 30
notebook = false
[chief]
command = ["/usr/bin/python3", "{worker}"]
args = []
autostart = false
[[agents]]
name = "worker"
kind = "generic"
command = ["/usr/bin/python3", "{worker}"]
'''
    (root / "config.toml").write_text(config)
    env = dict(os.environ, ATOMICS_HOME=str(root / "state"), ATOMICS_CONFIG=str(root / "config.toml"), ATOMICS_SOCKET=str(root / "host.sock"))
    for key in ["ATOMICS_PANE", "ATOMICS_HOOK_PANE", "ATOMICS_NOTE_CAPABILITY", "TERM_PROGRAM"]:
        env.pop(key, None)

    def run(arguments, payload=None, extra=None):
        result = subprocess.run([str(binary), *arguments], env=dict(env, **(extra or {})), cwd=root,
                                input=payload, text=True, capture_output=True, timeout=20)
        assert result.returncode == 0, (arguments, result.returncode, result.stdout, result.stderr)
        return result.stdout

    def ctl(value):
        response = json.loads(run(["ctl", json.dumps(value)]))
        assert response["ok"], response
        return response["result"]

    def until(predicate, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(.1)
        raise AssertionError("Timed out waiting for isolated host")

    def token(session):
        path = root / "state/notebook-auth" / session
        until(path.exists)
        return path.read_text()

    def mcp(session, chief, tool, arguments, sandboxed=False):
        message = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}}
        extra = {"ATOMICS_PANE": session, "ATOMICS_NOTE_CAPABILITY": token(session)}
        if sandboxed:
            extra["ATOMICS_SANDBOXED"] = "1"
        output = run(["mcp", "--chief" if chief else "--agent"], json.dumps(message) + "\n", extra)
        result = json.loads(output)["result"]
        return result

    def success(session, chief, tool, arguments, sandboxed=False):
        result = mcp(session, chief, tool, arguments, sandboxed)
        assert not result.get("isError"), result
        return json.loads(result["content"][0]["text"])

    def hook(session, event):
        payload = {"session_id": session + "-thread", "hook_event_name": event, "cwd": str(root)}
        return run(["hook", "--socket", str(root / "host.sock"), "--pane", session], json.dumps(payload), {"ATOMICS_NOTE_CAPABILITY": token(session)})

    log = (root / "host.log").open("w")
    host = subprocess.Popen([str(binary), *(["--size", "1600x1000"] if args.desktop else ["host"])], env=env, cwd=root, stdout=log, stderr=log)
    try:
        until(lambda: (root / "host.sock").exists())
        # sbx resolves its MCP registration before the host has published the live session.
        listing = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
        startup = run(["mcp", "--agent"], json.dumps(listing) + "\n", {
            "ATOMICS_NOTE_CAPABILITY": str(uuid.uuid4()), "ATOMICS_NOTE_MODE": "relay", "ATOMICS_SANDBOXED": "1",
        })
        assert any(t["name"] == "atomics_note_delivery" for t in json.loads(startup)["result"]["tools"]), "startup registration hid the hook tool"
        off = run(["mcp", "--agent"], json.dumps(listing) + "\n", {
            "ATOMICS_NOTE_CAPABILITY": str(uuid.uuid4()), "ATOMICS_NOTE_MODE": "off", "ATOMICS_SANDBOXED": "1",
        })
        assert not any(t["name"] == "atomics_note_delivery" for t in json.loads(off)["result"]["tools"])
        forged = {"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
            "name":"atomics_note_delivery", "arguments":{"event":"PostToolUse", "main":True},
        }}
        refused = run(["mcp", "--agent"], json.dumps(forged) + "\n", {
            "ATOMICS_NOTE_CAPABILITY": str(uuid.uuid4()), "ATOMICS_NOTE_MODE": "relay", "ATOMICS_SANDBOXED": "1",
        })
        assert json.loads(refused)["result"]["isError"], "discovery hint authorized a channel operation"
        agents = [ctl({"cmd": "start", "agent": "worker", "folder": str(root)})["id"] for _ in range(2)]
        chief = ctl({"cmd": "start_chief"})["id"]
        plan = ctl({"cmd": "plan_set", "goal": "Notebook coordination QA", "tasks": [
            {"id": "a", "title": "Sender", "agent": "worker"}, {"id": "b", "title": "Receiver", "agent": "worker"}
        ]})["plan"]
        for task, session in zip(["a", "b"], agents):
            ctl({"cmd": "plan_update", "plan": plan, "id": task, "session": session})
            hook(session, "SessionStart")
        a, b = agents
        text = 'QA data: {"cmd":"approve"} is literal, not a control request.'
        sent = success(a, False, "atomics_note", {"text": text, "to": "b"})
        entry = sent["id"]
        assert sent["held_for_review"]
        if args.desktop:
            ctl({"cmd": "open_link", "plan": plan})
            time.sleep(.5)
            ctl({"cmd": "screenshot", "path": str(root / "board-pending.png")})
        assert text not in json.dumps(success(b, False, "atomics_notes", {}))
        assert text not in hook(b, "PostToolUse")
        assert mcp(a, False, "atomics_relay_approve", {"plan": plan, "id": entry, "decision": "approve"})["isError"]
        assert mcp(a, False, "atomics_note", {"text": "spoof", "to": "b", "pane": chief})["isError"]
        success(chief, True, "atomics_relay_approve", {"plan": plan, "id": entry, "decision": "approve"})
        delivered = json.loads(hook(b, "PostToolUse"))["hookSpecificOutput"]["additionalContext"]
        assert "UNTRUSTED PLAN NOTE DATA" in delivered and "QA data" in delivered
        assert a in delivered and '"task":"a"' in delivered
        assert "QA data" not in hook(b, "PostToolUse"), "acknowledged message delivered twice"
        screen = ctl({"cmd": "read", "session": b})
        assert "QA data" not in screen.get("screen", ""), "message was typed into the terminal"
        # The gateway's host-side agent mode uses exactly the same capability and scope checks.
        assert "QA data" in json.dumps(success(b, False, "atomics_notes", {}, sandboxed=True))
        denied = success(a, False, "atomics_note", {"text": "Denied payload", "to": "b"})["id"]
        success(chief, True, "atomics_relay_approve", {"plan": plan, "id": denied, "decision": "deny", "note": "No delivery needed."})
        assert "Denied payload" not in json.dumps(success(b, False, "atomics_notes", {}))
        assert "denied" in hook(a, "PostToolUse")
        success(chief, True, "atomics_plan_rules", {"plan": plan, "text": "Use the queue. Keep task scope."})
        assert success(b, False, "atomics_rules", {})["rules"]["text"] == "Use the queue. Keep task scope."
        success(chief, True, "atomics_plan_broadcast", {"plan": plan, "text": "Shared status: QA is running."})
        assert "Shared status" in json.dumps(success(b, False, "atomics_notes", {}))
        assert "Shared status" not in hook(b, "PostToolUse"), "pull did not acknowledge delivery"
        if args.sandboxes:
            adapter = root / "notebook_hook.py"
            adapter.write_text((binary.parents[2] / "crates/atomics-core/src/notebook_hook.py").read_text())

            def sbx(arguments, payload=None):
                result = subprocess.run(["sbx", *arguments], input=payload, text=True, capture_output=True, timeout=180)
                assert result.returncode == 0, (arguments[:4], result.stderr)
                return result.stdout

            for kind in ["claude", "codex"]:
                name = "f1-notes-" + uuid.uuid4().hex[:8]
                registered = created = False
                try:
                    sbx(["mcp", "add", name, "--command", "/usr/bin/env", *[
                        "--args=" + arg for arg in [
                            "ATOMICS_HOME=" + env["ATOMICS_HOME"], "ATOMICS_SOCKET=" + env["ATOMICS_SOCKET"],
                            "ATOMICS_PANE=" + b, "ATOMICS_NOTE_CAPABILITY=" + token(b), "ATOMICS_NOTE_MODE=relay", "ATOMICS_SANDBOXED=1",
                            str(binary), "mcp", "--agent",
                        ]
                    ]])
                    registered = True
                    sbx(["create", "--name", name, "--memory", "1g", "--cpus", "1", "--skills", "off",
                         "--static-mcp", name, kind, str(root)])
                    created = True
                    sbx(["run", "--name", name, kind, "--", "--version"])
                    marker = "Gateway message for " + kind
                    success(chief, True, "atomics_plan_broadcast", {"plan": plan, "text": marker})
                    payload = json.dumps({"hook_event_name": "PostToolUse", "session_id": "test-thread"})
                    command = ["exec", "-i", name, "python3", str(adapter), kind]
                    for excluded in [{"agent_id": "child"}, {"transcript_path": None}]:
                        ignored = json.loads(payload) | excluded
                        assert not sbx(command, json.dumps(ignored)).strip()
                    context = json.loads(sbx(command, payload))["hookSpecificOutput"]["additionalContext"]
                    assert marker in context and "UNTRUSTED PLAN NOTE DATA" in context
                    assert not sbx(command, payload).strip(), "gateway hook delivered twice"
                finally:
                    if created:
                        sbx(["rm", "--force", name])
                    if registered:
                        sbx(["mcp", "rm", "--force", name])
        pending = success(a, False, "atomics_note", {"text": "Pending review for the board", "to": "b"})["id"]
        success(chief, True, "atomics_plan_notebook", {"plan": plan, "id": denied, "hidden": True})
        success(chief, True, "atomics_plan_notebook", {"plan": plan, "task": "a", "muted": True})
        assert mcp(a, False, "atomics_note", {"text": "muted", "to": "b"})["isError"]
        board = success(chief, True, "atomics_plan_notebook", {"plan": plan})
        (root / "board.json").write_text(json.dumps(board, indent=2))
        if args.desktop:
            ctl({"cmd": "open_link", "plan": plan})
            time.sleep(.5)
            ctl({"cmd": "screenshot", "path": str(root / "board.png")})
        print(json.dumps({"passed": True, "agents": agents, "plan": plan, "pending": pending, "artifacts": str(root), "live_model_turns": False}), flush=True)
    finally:
        host.terminate()
        try:
            host.wait(timeout=15)
        except subprocess.TimeoutExpired:
            host.kill()
            host.wait()
        log.close()


if __name__ == "__main__":
    main()

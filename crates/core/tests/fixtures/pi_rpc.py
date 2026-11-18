"""Deterministic protocol fixture, not a model. Never executes tools."""
import json
import os
from pathlib import Path
import sys


def send(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def respond(message, data=None):
    send({"id": message["id"], "type": "response", "command": message["type"], "success": True, "data": data})


def event(kind, **fields):
    send({"type": kind, **fields})


args = sys.argv[1:]
directory = Path(args[args.index("--session-dir") + 1])
(directory / "pid").write_text(str(os.getpid()))
session = Path(args[args.index("--session") + 1]) if "--session" in args else directory / "native.json"
count = json.loads(session.read_text())["count"] if session.exists() else 0
waiting = None
sessionless = False
omp = "rpc-ui" in args
ask_enabled = False


def finish(text):
    event("message_start", message={"role": "assistant"})
    event("message_update", assistantMessageEvent={"type": "text_delta", "delta": text})
    event("message_end", message={"role": "assistant", "content": [{"type": "text", "text": text}], "usage": {"input": 3, "output": 2, "cacheRead": 1}})
    event("agent_end", messages=[])
    event("agent_settled", aborted=False)


for line in sys.stdin:
    m = json.loads(line)
    kind = m["type"]
    with (directory / "wire.jsonl").open("a") as log:
        log.write(json.dumps(m) + "\n")
    if kind == "get_state":
        respond(m, {"sessionFile": None if sessionless else str(session), "model": {"provider": "test", "id": "model"}, "isStreaming": waiting is not None, "thinkingLevel": "low", "sessionName": "named"})
        if waiting == "turn":  # a command started its own turn, as pi.sendUserMessage does
            waiting = None
            finish("turn done")
        elif waiting == "idle-update":
            waiting = None
            event("available_commands_update", commands=[{"name": "settings"}])
    elif kind == "set_ask_dialog":
        ask_enabled = m.get("enabled") is True
        respond(m, {"enabled": ask_enabled})
    elif kind == "get_available_models":
        respond(m, {"models": [{"provider": "test", "id": "model"}]})
    elif kind in ("get_commands", "get_available_commands"):
        if omp:
            respond(m, {"commands": [{"name": "session", "aliases": ["info"]}, {"name": "export"}]})
            event("available_commands_update", commands=[{"name": "session", "aliases": ["info"]}, {"name": "late"}])
        else:
            respond(m, {"commands": [{"name": "hello"}, {"name": "/stats"}, {"name": "greencode-reload"}, {"name": "greencode-export-jsonl"}, {"name": "greencode-changelog"}, {"name": "greencode-hotkeys"}, {"name": "greencode-trust"}]})
    elif kind == "get_available_thinking_levels":
        respond(m, {"levels": ["off", "low", "high"]})
    elif kind == "get_last_assistant_text":
        respond(m, {"text": "last answer"})
    elif kind == "get_fork_messages":
        respond(m, {"messages": [{"entryId": "e1", "text": "hi there"}]})
    elif kind == "get_tree":
        respond(m, {"tree": [{"entry": {"id": "e1", "type": "message", "message": {"role": "user", "content": "hi"}}, "children": [{"entry": {"id": "e2", "type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "yo"}]}}, "children": []}]}], "leafId": "e2"})
    elif kind == "login":
        event("extension_ui_request", id="url", method="open_url", url="about:blank", instructions="paste the code")
        event("extension_ui_request", id="question", method="input", title="Code")
        answer = json.loads(sys.stdin.readline())
        with (directory / "wire.jsonl").open("a") as log:
            log.write(json.dumps(answer) + "\n")
        respond(m, {"providerId": m["providerId"]})
    elif kind == "prompt":
        count += 1
        if m["message"].startswith("/boom"):
            event("extension_error", extensionPath="/ext/boom.ts", event="command", error="exploded")
            respond(m, {"disposition": "handled"})
            continue
        if m["message"] == "/greencode-changelog":
            event("extension_ui_request", id="changelog", method="notify", message="Pi changelog fixture", notifyType="info")
            respond(m, {"disposition": "handled"})
            continue
        if m["message"] == "/greencode-hotkeys":
            event("extension_ui_request", id="hotkeys", method="notify", message="Pi terminal keybindings: app.interrupt: escape", notifyType="info")
            respond(m, {"disposition": "handled"})
            continue
        if m["message"] == "/greencode-trust":
            event("extension_ui_request", id="question", method="select", title="Pi project trust", options=["Trust", "Do not trust"])
            answer = json.loads(sys.stdin.readline())
            with (directory / "wire.jsonl").open("a") as log:
                log.write(json.dumps(answer) + "\n")
            respond(m, {"disposition": "handled"})
            continue
        if m["message"] == "/greencode-reload" or m["message"].startswith("/greencode-export-jsonl "):
            respond(m, {"disposition": "handled"})
            continue
        if m["message"].startswith("/turn"):
            waiting = "turn"
            respond(m, {"disposition": "handled"})
            continue
        if m["message"].startswith("/hello"):
            respond(m, {"disposition": "handled"})
            continue
        session.write_text(json.dumps({"count": count}))
        respond(m, {"disposition": "started"})
        event("message_end", message={"role": "user", "content": [{"type": "text", "text": m["message"]}]})
        if m["message"] == "steer":
            waiting = "steer"
            event("message_update", assistantMessageEvent={"type": "text_delta", "delta": "working"})
            event("agent_end", messages=[], willRetry=True)
        elif m["message"] == "dialog":
            waiting = "dialog"
            event("extension_ui_request", id="question", method="input", title="A question")
        elif m["message"] == "ask" and ask_enabled:
            waiting = "ask"
            event("extension_ui_request", id="question", method="ask", questions=[
                {"id": "a", "question": "Choose options", "multi": True, "options": [{"label": "one"}, {"label": "two"}]},
                {"id": "b", "question": "Explain", "options": []},
            ])
        elif m["message"] == "open":
            event("extension_ui_request", id="url", method="open_url", url="about:blank", instructions="paste the code")
            finish("opened")
        elif m["message"] == "timeout":
            waiting = "timeout"
            event("extension_ui_request", id="slow", method="input", title="Slow", timeout=50)
        elif m["message"] == "ask-timeout":
            waiting = "timeout"
            event("extension_ui_request", id="slow", method="ask", questions=[{"id": "q", "question": "Pick", "options": []}], timeout=50)
        elif m["message"] == "agent-cancels-dialog":
            event("extension_ui_request", id="slow", method="input", title="Cancelled")
            event("extension_ui_request", id="notice", method="notify", message="before cancellation")
            event("extension_ui_request", id="cancel-request", method="cancel", targetId="slow")
            finish("continued")
        elif m["message"] == "cancel":
            waiting = "cancel"
            event("message_update", assistantMessageEvent={"type": "text_delta", "delta": "working"})
        else:
            finish(f"{count}:{m['message']}\u2028日本語")
    elif kind == "extension_ui_response":
        waiting = None
        finish(m.get("value") or ("answered" if "answers" in m else "timedOut" if m.get("timedOut") else "cancelled"))
    elif kind == "steer":
        respond(m, {"disposition": "queued"})
        waiting = None
        finish(m["message"])
    elif kind == "abort":
        respond(m)
        waiting = None
        event("agent_settled", aborted=True)
    elif kind == "schedule_update":
        waiting = "idle-update"
        respond(m)
    elif kind == "set_busy":
        waiting = "busy"
        respond(m)
    elif kind == "drop_session":
        sessionless = True
        respond(m)
    elif kind == "exit_now":
        sys.exit(7)
    else:
        respond(m)

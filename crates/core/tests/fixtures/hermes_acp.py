#!/usr/bin/env python3
"""Hermes CLI/ACP contract fixture. No inference and no credential access."""
import json
import os
from pathlib import Path
import sys

assert sys.argv[1:] == ["--profile", "coder", "acp"], sys.argv
root = Path(os.environ["HERMES_HOME"])
assert root.name == "Hermes home with spaces"
assert (root / "profiles/coder/config.yaml").is_file()


def send(value):
    print(json.dumps(value), flush=True)


for line in sys.stdin:
    message = json.loads(line)
    method, params = message.get("method"), message.get("params", {})
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}
    elif method in ("session/new", "session/load"):
        if method == "session/load":
            assert params["sessionId"] == "hermes-fixture"
        else:
            # Only GreenCode's own delegation server, never the project's or imported ones.
            assert [s["name"] for s in params["mcpServers"]] in ([], ["greencode"]), params["mcpServers"]
        result = {"sessionId": "hermes-fixture", "models": {"currentModelId": "openai-codex:gpt-6-luna"}}
    elif method == "session/prompt":
        assert params["sessionId"] == "hermes-fixture"
        # No replay of the imported transcript: at most GreenCode's delegation guide before the prompt.
        blocks = [b for b in params["prompt"] if not b["text"].startswith("## GreenCode delegation")]
        assert len(blocks) == 1, params["prompt"]
        prompt = blocks[0]["text"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "hermes-fixture", "update": {"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "coder:" + prompt}}}})
        result = {"stopReason": "end_turn"}
    else:
        raise AssertionError("Unexpected RPC method: " + str(method))
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})

#!/usr/bin/env python3
"""Exercise native model routing and CLI resume against local protocol fixtures, without model inference."""
import argparse
import http.server
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/greencode-cli")
    args = parser.parse_args()
    requests = []

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            route = self.path.split("/")[1]
            requests.append((route, body))
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            if route == "anthropic":
                events = [
                    {"type": "message_start", "message": {"usage": {"input_tokens": 1}}},
                    {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "anthropic answer"}},
                    {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}},
                ]
            else:
                is_parent = any(t["function"]["name"] == "task" for t in body.get("tools", []))
                last = body["messages"][-1]
                if is_parent and last["role"] == "user":
                    task = {"prompt": "isolated subtask"}
                    if last["content"] == "override":
                        task["model"] = "anthropic-fixture/gpt-6-luna"
                    delta = {"tool_calls": [{"index": 0, "id": "sub-task", "type": "function", "function": {
                        "name": "task", "arguments": json.dumps(task)}}]}
                else:
                    delta = {"content": "parent final" if is_parent else f"{route} answer"}
                events = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                          {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls" if "tool_calls" in delta else "stop"}]}]
            for event in events:
                self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="greencode-subagent-models-") as temp:
            root = Path(temp)
            home = root / "home"
            home.mkdir()
            config = 'model="parent/gpt-6-luna"\nsubagent_model="child/gpt-6-luna"\n'
            for name, route, kind in [("parent", "parent", "openai"), ("child", "child", "openai"),
                                      ("anthropic-fixture", "anthropic", "anthropic"), ("unavailable", "unused", "openai")]:
                key_env = "GREENCODE_MISSING_FIXTURE_KEY" if name == "unavailable" else "GREENCODE_FIXTURE_KEY"
                config += (f'[providers.{name}]\nkind="{kind}"\n'
                           f'base_url="http://127.0.0.1:{server.server_port}/{route}/v1"\napi_key_env="{key_env}"\n')
            (home / "config.toml").write_text(config)
            env = dict(os.environ, GREENCODE_HOME=str(home), GREENCODE_FIXTURE_KEY="local-fixture")
            env.pop("GREENCODE_MISSING_FIXTURE_KEY", None)
            command = [str(args.cli.resolve()), "--json", "--mode", "full"]
            checks = []

            def run(label, options, routes, answer=None, error=None, prompt="delegate"):
                start = len(requests)
                result = subprocess.run([*command, *options, "-p", prompt], env=env, cwd=root,
                                        capture_output=True, text=True, timeout=30)
                assert result.returncode == 0, result.stderr
                captured = requests[start:]
                assert [route for route, _ in captured] == routes, (label, captured)
                assert all(body["model"] == "gpt-6-luna" for _, body in captured)
                events = [json.loads(line) for line in result.stdout.splitlines()]
                tool_end = next(event for event in events if event["type"] == "tool_end")
                assert tool_end["is_error"] == bool(error), tool_end
                assert (error or answer) in tool_end["output"], tool_end
                if not error:
                    child = captured[1][1]
                    assert child["messages"][-1]["content"] == ("isolated subtask" if routes[1] != "anthropic" else [{"type": "text", "text": "isolated subtask"}])
                    assert not any(t.get("name", t.get("function", {}).get("name")) == "task" for t in child["tools"])
                checks.append(label)
                return re.search(r"session ([\w-]+)", result.stderr)[1]

            session = run("config default", [], ["parent", "child", "parent"], answer="child answer")
            resume = ["--resume", session]
            run("saved default", resume, ["parent", "child", "parent"], answer="child answer")
            run("follow parent", [*resume, "--subagent-model", "parent"], ["parent"] * 3, answer="parent answer")
            run("saved parent selection", resume, ["parent"] * 3, answer="parent answer")
            run("CLI provider selection", [*resume, "--subagent-model", "child/gpt-6-luna"], ["parent", "child", "parent"], answer="child answer")
            run("per-call cross-protocol override", resume, ["parent", "anthropic", "parent"], answer="anthropic answer", prompt="override")
            run("missing credentials", [*resume, "--subagent-model", "unavailable/gpt-6-luna"], ["parent"] * 2, error="is not set")
            run("unknown provider", [*resume, "--subagent-model", "unknown/gpt-6-luna"], ["parent"] * 2, error="unknown provider")
            unsupported = subprocess.run([*command, "--backend", "pi", "--subagent-model", "child/gpt-6-luna", "-p", "unused"],
                                         env=env, cwd=root, capture_output=True, text=True, timeout=30)
            assert unsupported.returncode != 0 and "for native threads" in unsupported.stderr
            checks.append("external backend rejected before startup")
            print(json.dumps({"scope": __doc__, "checks": checks, "requests": len(requests)}, indent=2))
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()

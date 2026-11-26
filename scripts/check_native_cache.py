#!/usr/bin/env python3
"""Capture production native requests on a local fixture server; bytes are not token/cache-hit measurements."""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/greencode-cli")
    parser.add_argument("--baseline-cli", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    requests = []

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(body)
            has_result = any(m["role"] == "tool" for m in body["messages"])
            delta = {"role": "assistant", "content": "verified"} if has_result else {
                "role": "assistant", "tool_calls": [{"index": 0, "id": "read-1", "type": "function",
                    "function": {"name": "read", "arguments": json.dumps({"path": "sample.txt"})}}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for d, finish in [(delta, None), ({}, "stop" if has_result else "tool_calls")]:
                chunk = {"choices": [{"index": 0, "delta": d, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="greencode-native-cache-") as temp:
            root = Path(temp)
            project = root / "project"
            project.mkdir()
            (project / "sample.txt").write_text("fixture content\n")
            (project / ".env").touch()
            (project / "target").mkdir()
            report = {"scope": __doc__, "model": "gpt-6-luna (local protocol fixture, no inference)"}
            arms = [("baseline", args.baseline_cli)] if args.baseline_cli else []
            for name, cli in [*arms, ("optimized", args.cli)]:
                instructions = project / "AGENTS.md"
                instructions.unlink(missing_ok=True)
                added = project / "new_file.rs"
                added.unlink(missing_ok=True)
                home = root / name
                home.mkdir()
                (home / "config.toml").write_text(
                    'model="probe/gpt-6-luna"\ninclude_workspace_tree=true\n'
                    '[features]\nunexpected_stop_detection="none"\n'
                    f'[providers.probe]\nkind="openai"\nbase_url="http://127.0.0.1:{server.server_port}/v1"\n'
                    'api_key_env="GREENCODE_FIXTURE_KEY"\n')
                env = dict(os.environ, GREENCODE_HOME=str(home), GREENCODE_FIXTURE_KEY="local-fixture")
                command = [str(cli.resolve()), "--mode", "full", "--json"]
                requests.clear()
                first = subprocess.run([*command, "-p", "Read sample.txt"], cwd=project, env=env, capture_output=True, text=True, timeout=30)
                assert first.returncode == 0, first.stderr
                session = next(home.glob("sessions/*.jsonl")).stem
                memory = project / ".greencode/MEMORY.md"
                memory.parent.mkdir(exist_ok=True)
                memory.write_text("Changed after the first turn.\n")
                added.touch()
                resumed = subprocess.run([*command, "--resume", session, "-p", "Continue"], cwd=project, env=env, capture_output=True, text=True, timeout=30)
                assert resumed.returncode == 0, resumed.stderr
                assert len(requests) == 3, len(requests)
                first, tool_round, resumed_body = requests
                system = first["messages"][0]["content"]
                stable = system == resumed_body["messages"][0]["content"]
                prefix_len = len(tool_round["messages"])
                prefix_stable = resumed_body["messages"][:prefix_len] == tool_round["messages"]
                if name == "optimized":
                    assert prefix_stable, "cached message prefix changed on resume"
                    assert first["tools"] == tool_round["tools"] == resumed_body["tools"]
                    assert "Changed after the first turn." in resumed_body["messages"][-2]["content"]
                    workspace = first["messages"][1]["content"]
                    assert "# Workspace tree\nsample.txt\n" in workspace
                    assert ".env" not in workspace and "target/" not in workspace
                    assert "new_file.rs" in resumed_body["messages"][-2]["content"]
                    assert "new_file.rs" not in system
                instructions.write_text("Updated project safety instructions.\n")
                updated = subprocess.run([*command, "--resume", session, "-p", "Continue with updated project rules"],
                    cwd=project, env=env, capture_output=True, text=True, timeout=30)
                assert updated.returncode == 0, updated.stderr
                assert len(requests) == 4, len(requests)
                updated_body = requests[-1]
                if name == "optimized":
                    assert updated_body["messages"][:len(resumed_body["messages"])] == resumed_body["messages"]
                    assert updated_body["tools"] == first["tools"]
                    assert "Updated project safety instructions." in updated_body["messages"][-2]["content"]
                else:
                    assert any("Updated project safety instructions." in m["content"] for m in updated_body["messages"])
                report[name] = {"system_bytes": len(system.encode()), "system_stable_on_resume": stable,
                    "message_prefix_stable_on_resume": prefix_stable,
                    "updated_project_instructions_applied": True, "requests": len(requests)}
                memory.unlink()
            if "baseline" in report:
                report["system_byte_reduction"] = report["baseline"]["system_bytes"] - report["optimized"]["system_bytes"]
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report, indent=2))
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()

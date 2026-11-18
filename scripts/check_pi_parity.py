#!/usr/bin/env python3
"""Compare real Pi HTTP requests, direct versus GreenCode, using a deterministic local server.

This checks harness overhead, not model quality or real-provider billing/cache hits.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", default=str(Path(__file__).resolve().parents[1] / "target/debug/greencode-cli"))
    parser.add_argument("--pi", default=shutil.which("pi"))
    parser.add_argument("--protocol", choices=("pi", "omp"), default="pi")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not args.pi:
        parser.error("Pi must be installed, or supplied with --pi")
    requests = []

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(body)
            has_result = any(m.get("role") == "tool" for m in body["messages"])
            approval_probe = any("approval probe" in str(m.get("content", "")) for m in body["messages"] if m.get("role") == "user")
            tool = "bash" if approval_probe else "read"
            arguments = {"command": "touch denied"} if approval_probe else {"path": "sample.txt"}
            delta = {"role": "assistant", "content": "verified"} if has_result else {
                "role": "assistant", "tool_calls": [{"index": 0, "id": "read-1", "type": "function",
                    "function": {"name": tool, "arguments": json.dumps(arguments)}}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for d, finish in [(delta, None), ({}, "stop" if has_result else "tool_calls")]:
                chunk = {"id": "parity", "object": "chat.completion.chunk", "created": 0, "model": "probe",
                    "choices": [{"index": 0, "delta": d, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    proc = None
    try:
        with tempfile.TemporaryDirectory(prefix="greencode-pi-parity-") as temp:
            root = Path(temp)
            project = root / "project"
            project.mkdir()
            (project / "sample.txt").write_text("identical tool result\n")
            agent = root / "pi"
            agent.mkdir()
            # Deliberately local test credentials; no user's auth files are read or changed.
            (agent / "models.json").write_text(json.dumps({"providers": {"probe": {
                "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "api": "openai-completions", "apiKey": "local-test",
                "models": [{"id": "probe", "reasoning": False, "contextWindow": 128000, "maxTokens": 1024}]}}}))
            env = dict(os.environ, PI_CODING_AGENT_DIR=str(agent), PI_CONFIG_DIR=str(root / "config"))
            flags = ["--provider", "probe", "--model", "probe", "--no-context-files", "--no-skills", "--no-extensions"]
            if args.protocol == "omp":
                flags += ["--no-rules", "--no-title", "--no-lsp", "--tools", "read,bash,edit,write"]
            else:
                flags += ["--no-prompt-templates", "--offline"]
            prompt = "Read sample.txt, then say verified."
            mode = "rpc-ui" if args.protocol == "omp" else "rpc"
            proc = subprocess.Popen([args.pi, *flags, "--mode", mode, "--no-session"], cwd=project,
                env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            records = queue.Queue()

            def reader():
                for line in proc.stdout:
                    records.put(json.loads(line))
                records.put(None)

            threading.Thread(target=reader, daemon=True).start()
            proc.stdin.write(json.dumps({"id": "direct", "type": "prompt", "message": prompt}) + "\n")
            proc.stdin.flush()
            while True:
                record = records.get(timeout=60)
                assert record is not None, "direct Pi exited before settling"
                assert not (record.get("type") == "response" and record.get("success") is False), record
                if record.get("type") in ("agent_settled", "session_settled"):
                    break
            proc.terminate()
            proc.wait(timeout=10)
            direct = requests.copy()
            requests.clear()
            home = root / "greencode"
            home.mkdir()
            (home / "config.toml").write_text('[[backends]]\nname="pi"\nprotocol=' + json.dumps(args.protocol) + '\ncommand=' + json.dumps([args.pi, *flags]) + "\n")
            result = subprocess.run([args.cli, "--backend", "pi", "--json", "-p", prompt], cwd=project,
                env=dict(env, GREENCODE_HOME=str(home)), capture_output=True, text=True, timeout=60)
            assert result.returncode == 0, result.stderr
            wrapped = requests.copy()
            assert len(direct) == len(wrapped) == 2, (len(direct), len(wrapped))
            sizes = []
            for i, (a, b) in enumerate(zip(direct, wrapped)):
                for field in ("model", "messages", "tools", "tool_choice", "parallel_tool_calls"):
                    assert a.get(field) == b.get(field), f"request {i + 1}: {field} differs"
                prompt_fields = {key: a[key] for key in ("messages", "tools") if key in a}
                sizes.append(len(json.dumps(prompt_fields, ensure_ascii=False).encode()))
            session = next((home / "sessions").glob("*.jsonl")).stem
            requests.clear()
            resumed = subprocess.run([args.cli, "--backend", "pi", "--json", "--resume", session, "-p", "Continue without tools."],
                cwd=project, env=dict(env, GREENCODE_HOME=str(home)), capture_output=True, text=True, timeout=60)
            assert resumed.returncode == 0, resumed.stderr
            assert len(requests) == 1, "resume made extra model requests"
            resumed_request = requests[0]
            for field in ("model", "tools", "tool_choice", "parallel_tool_calls", "prompt_cache_key", "prompt_cache_retention", "reasoning_effort"):
                assert wrapped[-1].get(field) == resumed_request.get(field), f"resume changed {field}"
            prefix = wrapped[-1]["messages"]
            assert resumed_request["messages"][:len(prefix)] == prefix, "resume rewrote the cached message prefix"
            denied = subprocess.run([args.cli, "--backend", "pi", "--json", "-p", "approval probe"], cwd=project,
                env=dict(env, GREENCODE_HOME=str(home)), input="n\n", capture_output=True, text=True, timeout=60)
            assert denied.returncode == 0, denied.stderr
            events = [json.loads(line) for line in denied.stdout.splitlines()]
            assert any(e.get("type") == "permission" and e.get("tool") == "bash" for e in events), "approval hook was not invoked"
            assert not (project / "denied").exists(), "denied tool executed"
            report = {"pi_version": subprocess.check_output([args.pi, "--version"], text=True).strip(),
                "protocol": args.protocol,
                "provider": "deterministic local OpenAI-compatible test server", "model_requests_each": len(direct),
                "prompt_fields_equal": True, "prompt_bytes_by_request": sizes, "added_prompt_bytes": 0,
                "resume_prefix_unchanged": True, "resume_model_requests": 1,
                "denied_tool_did_not_execute": True,
                "tool_scope": "read,bash,edit,write; LSP disabled" if args.protocol == "omp" else "Pi defaults",
                "scope": "same engine, model, project, flags and read task; includes tool result round trip; not a quality/billing benchmark"}
            if args.output:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            print(json.dumps(report, ensure_ascii=False, indent=2))
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        server.shutdown()


if __name__ == "__main__":
    main()

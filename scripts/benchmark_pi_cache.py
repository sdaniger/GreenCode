#!/usr/bin/env python3
"""Live, read-only cache/token smoke benchmark. Uses only GPT-6 Luna, never a fallback model."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

MODEL = "openai-codex/gpt-6-luna"


def metrics(rows):
    total = {key: sum(row[key] for row in rows) for key in ("input", "cache_read", "cache_write", "output")}
    context = total["input"] + total["cache_read"] + total["cache_write"]
    return {**total, "total_input": context, "cache_hit_rate": total["cache_read"] / context if context else None}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/greencode-cli")
    parser.add_argument("--pi", default=shutil.which("pi"))
    parser.add_argument("--resume-each-turn", action="store_true", help="Restart the CLI and resume the same thread for every follow-up")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not args.pi:
        parser.error("Pi must be installed and signed in for GPT-6 Luna")
    with tempfile.TemporaryDirectory(prefix="greencode-cache-bench-") as temp:
        root = Path(temp)
        project, home = root / "project", root / "greencode"
        project.mkdir()
        home.mkdir()
        (project / "reference.txt").write_text("".join(f"item_{i:04d}=value_{i:04d}\n" for i in range(1, 401)))
        flags = [args.pi, "--provider", "openai-codex", "--model", "gpt-6-luna", "--thinking", "low",
                 "--no-context-files", "--no-skills", "--no-extensions", "--no-mcp", "--no-prompt-templates", "--offline", "--tools", "read"]
        (home / "config.toml").write_text('[[backends]]\nname="pi"\nprotocol="pi"\ncommand=' + json.dumps(flags) + "\n")
        indices = [10, 100, 200, 300, 400]
        prompts = [f"What is item_{i:04d}? Reply only with its exact value, no quotes or explanation. Do not call tools again."
                   for i in indices]
        prompts[0] = "Read reference.txt once using the read tool. " + prompts[0].replace("Do not call tools again.", "")
        started = time.monotonic()
        command = [str(args.cli.resolve()), "--backend", "pi", "--model", MODEL, "--mode", "full", "--json"]
        events, session = [], None
        batches = [[prompt] for prompt in prompts] if args.resume_each_turn else [prompts]
        for batch in batches:
            result = subprocess.run([*command, *(["--resume", session] if session else [])],
                input="\n".join(batch) + "\n", cwd=project, env=dict(os.environ, GREENCODE_HOME=str(home)),
                capture_output=True, text=True, timeout=240)
            records = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
            errors = [e["text"] for e in records if e["type"] == "error"]
            if result.returncode or errors:
                raise RuntimeError(f"benchmark failed, no fallback: {errors or result.stderr[-2000:]}")
            states = [e["data"]["model"] for e in records if e["type"] == "remote" and e["command"] == "get_state" and e["data"].get("model")]
            assert states and all(f"{m['provider']}/{m['id']}" == MODEL for m in states), states
            events.extend(records)
            sessions = list((home / "sessions").glob("*.jsonl"))
            assert len(sessions) == 1 and (session is None or sessions[0].stem == session), "resume created another thread"
            session = sessions[0].stem
        elapsed = time.monotonic() - started
        turns, turn = [], {"text": "", "requests": []}
        for event in events:
            if event["type"] == "text":
                turn["text"] += event["text"]
            elif event["type"] == "usage":
                turn["requests"].append({k: event[k] for k in ("input", "cache_read", "cache_write", "output")})
            elif event["type"] == "done":
                turns.append(turn)
                turn = {"text": "", "requests": []}
        assert len(turns) == len(indices), f"expected {len(indices)} settled turns, got {len(turns)}"
        for i, turn in zip(indices, turns):
            assert turn["text"].strip() == f"value_{i:04d}", turn["text"]
            assert turn["requests"] and all(sum(r.values()) > 0 for r in turn["requests"]), "provider did not report usage"
        calls = [e for e in events if e["type"] == "tool_start"]
        assert len(calls) == 1 and calls[0]["name"] == "read", "expected exactly one read across the thread"
        report = {"model": MODEL, "pi_version": subprocess.check_output([args.pi, "--version"], text=True).strip(),
                  "scope": "live GreenCode/Pi cache smoke test on generated lookup data; not a coding-quality or native-provider A/B benchmark; cache is not forcibly cleared",
                  "resume_each_turn": args.resume_each_turn, "cli_processes": len(batches),
                  "turns_verified": len(turns), "read_tool_calls": len(calls), "elapsed_seconds": round(elapsed, 3),
                  "all_requests": metrics([r for t in turns for r in t["requests"]]),
                  "followup_requests": metrics([r for t in turns[1:] for r in t["requests"]]),
                  "turns": [{"verified": True, **t} for t in turns]}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    assert metrics([{"input": 10, "cache_read": 80, "cache_write": 10, "output": 5}])["cache_hit_rate"] == 0.8
    main()

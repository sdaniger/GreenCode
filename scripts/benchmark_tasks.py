#!/usr/bin/env python3
"""Live coding-task benchmark on GPT-6 Luna. Only when GPT is rate-limited does the whole run switch to Claude Haiku 5.5 (`--model`)."""
import argparse
from datetime import date
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

from benchmark_pi_cache import metrics

# Luna is the benchmark model; Haiku 5.5 is used only when GPT reports a limit. Each entry names the model for the native harness and for Pi/OMP.
MODELS = {
    "luna": {"id": "gpt-6-luna", "native": "openai/gpt-6-luna", "ext": "openai-codex", "thinking": "low",
             "config": 'effort = "low"\n', "login": "Imported the Codex CLI login;"},
    "haiku": {"id": "claude-haiku-5-5", "native": "anthropic/claude-haiku-5-5", "ext": "anthropic", "thinking": "off",
              "config": "", "login": "Imported the Claude Code login;"},
}
# The imported Codex login uses the codex provider, not the OpenAI API preset.
LUNA_NATIVE_PROVIDER = '[providers.openai]\nkind="codex"\nbase_url="https://chatgpt.com/backend-api/codex"\n'
LIMITED = re.compile(r"\b429\b|rate.?limit|usage.?limit|quota|limit reached|too many requests", re.I)
HARNESSES = ("native", "pi", "omp-default", "omp-lean", "omp-advisor")
PROMPTS = {
    "bugfix": "The tests in test_slug.py fail. Fix slug.py so all tests pass. Do not edit the tests.",
    "feature": "Implement parse_duration in util.py as its docstring describes; run the tests until they pass.",
    "refactor": "Rename fetch_user to load_user everywhere in this project and keep the tests passing.",
    "lookup": "What is the value of timeout_ms for region eu? Reply with only the value.",
}

LONG_FILES = ("alpha", "beta", "gamma", "delta", "epsilon", "zeta")
LONG_TASK = "resume"
LONG_TURNS = (
    ("For this session prefix every short answer with `RIVER:` (no space), even after resuming. "
     "Reply with `RIVER:ready` now.", "RIVER:ready"),
    *((f"Read all 34 lines of {name}.txt, then reply with the code on its last line, using the agreed format.",
       f"RIVER:{name}-33") for name in LONG_FILES),
    ("/compact summary", ""),
    ("What is the code on the last line of alpha.txt? Re-read that file if needed. Use the agreed format.",
     "RIVER:alpha-33"),
)


def usage_rows(events):
    return [{key: event[key] for key in ("input", "cache_read", "cache_write", "output")}
            for event in events if event.get("type") in ("usage", "usage_aux")]


def fixture(project, task):
    files = {
        "bugfix": {
            "slug.py": 'def slugify(text):\n    return text.replace(" ", "-")\n',
            "test_slug.py": '''import unittest
from slug import slugify

class SlugTests(unittest.TestCase):
    def test_lowercase(self): self.assertEqual(slugify("Hello World"), "hello-world")
    def test_collapse(self): self.assertEqual(slugify("a  b---c"), "a-b-c")
    def test_edges(self): self.assertEqual(slugify(" -hello- "), "hello")
''',
        },
        "feature": {
            "util.py": '''def parse_duration(text):
    """Parse concatenated integer durations: 1h30m, 45s and 2d; 90 means seconds.

    Units are d, h, m and s. Empty or malformed input raises ValueError.
    """
''',
            "test_util.py": '''import unittest
from util import parse_duration

class DurationTests(unittest.TestCase):
    def test_examples(self):
        for text, expected in [("1h30m", 5400), ("45s", 45), ("2d", 172800), ("90", 90)]:
            self.assertEqual(parse_duration(text), expected)
    def test_invalid(self):
        for text in ("", "1x", "h", "1h30", "-2s", "1h 2m"):
            with self.assertRaises(ValueError): parse_duration(text)
''',
        },
        "refactor": {
            "db.py": 'def fetch_user(user_id):\n    return {"id": user_id, "name": "Ada"}\n',
            "api.py": 'from db import fetch_user\n\ndef user_name(user_id):\n    return fetch_user(user_id)["name"]\n',
            "cli.py": 'from db import fetch_user\n\ndef show_user(user_id):\n    return fetch_user(user_id)["name"]\n',
            "test_api.py": '''import unittest
from api import user_name
from cli import show_user
from db import fetch_user

class ApiTests(unittest.TestCase):
    def test_user(self):
        self.assertEqual(fetch_user(7)["id"], 7)
        self.assertEqual(user_name(7), "Ada")
        self.assertEqual(show_user(7), "Ada")
''',
        },
    }
    if task == "lookup":
        files[task] = {"README": "the timeout key is in whichever file mentions region eu\n"}
        for name, start in (("a.ini", 1), ("b.ini", 151)):
            lines = [f"key_{i:04d}=value_{i:04d}\n" for i in range(start, start + 150)]
            if name == "a.ini":
                lines[49] = "region=us\n"
                lines[99] = "timeout_ms=1000\n"
            else:
                lines[49] = "region=eu\n"
                lines[99] = "timeout_ms=2750\n"
            files[task][name] = "".join(lines)
    for name, content in files[task].items():
        (project / name).write_text(content, encoding="utf-8")
    test = project / {"bugfix": "test_slug.py", "feature": "test_util.py"}.get(task, "__no_test__")
    return test.read_bytes() if test.exists() else None


def check(project, task, original_test, final_text):
    if task == LONG_TASK:
        return final_text.strip() == LONG_TURNS[-1][1]
    if task == "lookup":
        return final_text.strip() == "2750"
    if task in ("bugfix", "feature"):
        test = project / ("test_slug.py" if task == "bugfix" else "test_util.py")
        if not test.exists() or test.read_bytes() != original_test:
            return False
    if task == "refactor":
        sources = [p.read_text(encoding="utf-8") for p in project.rglob("*.py")]
        if any("fetch_user" in source for source in sources) or not any("def load_user(" in source for source in sources):
            return False
    try:
        return subprocess.run(["python3", "-m", "unittest", "-q"], cwd=project, capture_output=True, timeout=30).returncode == 0
    except subprocess.TimeoutExpired:
        return False


def selfcheck():
    events = [
        {"type": "usage", "input": 2, "cache_read": 3, "cache_write": 5, "output": 7},
        {"type": "text", "text": "not usage"},
        {"type": "usage_aux", "input": 11, "cache_read": 13, "cache_write": 17, "output": 19},
    ]
    totals = metrics(usage_rows(events))
    assert (totals["input"], totals["cache_read"], totals["cache_write"], totals["output"]) == (13, 16, 22, 26)
    with tempfile.TemporaryDirectory(prefix="greencode-task-selfcheck-") as temp:
        for task in PROMPTS:
            project = Path(temp) / task
            project.mkdir()
            original = fixture(project, task)
            assert not check(project, task, original, "wrong"), task
            if task == "bugfix":
                (project / "slug.py").write_text('import re\ndef slugify(text):\n    return re.sub(r"[-\\s]+", "-", text.lower()).strip("-")\n')
            elif task == "feature":
                (project / "util.py").write_text('''import re
def parse_duration(text):
    if re.fullmatch(r"[0-9]+", text): return int(text)
    parts = re.findall(r"([0-9]+)([dhms])", text)
    if not parts or "".join(n + u for n, u in parts) != text: raise ValueError(text)
    return sum(int(n) * {"d": 86400, "h": 3600, "m": 60, "s": 1}[u] for n, u in parts)
''')
            elif task == "refactor":
                for path in project.glob("*.py"):
                    path.write_text(path.read_text().replace("fetch_user", "load_user"))
            assert check(project, task, original, "2750"), task
    for text in ("HTTP 429: usage limit reached", "You have hit your usage limit", "rate_limit_exceeded", "Too Many Requests"):
        assert LIMITED.search(text), text
    for text in ("HTTP 500: server error", "invalid api key", "OK"):
        assert not LIMITED.search(text), text
    print("Self-check passed for all fixtures and checkers.")


def claude_token():
    """The Claude Code login, which Pi and OMP read as ANTHROPIC_OAUTH_TOKEN; the native harness imports the same login with `auth import`."""
    try:
        oauth = json.loads((Path.home() / ".claude/.credentials.json").read_text())["claudeAiOauth"]
        if oauth["expiresAt"] / 1000 > time.time():
            return oauth["accessToken"]
    except (OSError, KeyError, ValueError):
        pass
    # shortcut: only the file `auth import` reads; on macOS recent Claude Code keeps its live token in the Keychain, so this file can be stale.
    raise SystemExit("Claude Code login missing or expired in ~/.claude/.credentials.json (the file `auth import` reads); sign in with Claude Code again")


def import_login(cli, home, choice):
    """Imports the login `choice` needs into `home`. Returns the CLI's complaint, or None when it worked."""
    try:
        auth = subprocess.run([str(cli), "auth", "import"], env=dict(os.environ, GREENCODE_HOME=str(home)), capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return str(exc)
    return None if not auth.returncode and choice["login"] in auth.stdout else (auth.stderr or auth.stdout or "no login found")


def probe_luna(cli, root):
    """One prompt to Luna through the native harness. Returns the error text, or None when it answered."""
    home, project = root / "probe-home", root / "probe-project"
    home.mkdir()
    project.mkdir()
    (home / "config.toml").write_text(MODELS["luna"]["config"] + LUNA_NATIVE_PROVIDER)
    complaint = import_login(cli, home, MODELS["luna"])
    if complaint:
        return complaint
    command = [str(cli), "--json", "--mode", "full", "--backend", "native", "--model", MODELS["luna"]["native"], "-p", "Reply with OK."]
    try:
        result = subprocess.run(command, cwd=project, env=dict(os.environ, GREENCODE_HOME=str(home)), capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return str(exc)
    errors = []
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "error":
            errors.append(event.get("text", ""))
    return "\n".join(errors) or (None if result.returncode == 0 else result.stderr.strip() or f"exit {result.returncode}")


def version(command):
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        return result.stdout.strip() if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def prompt_turn(command, project, env):
    try:
        result = subprocess.run(command, cwd=project, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, timeout=300)
        stdout, returncode = result.stdout, result.returncode
        failure = [result.stderr.splitlines()[-1]] if returncode and result.stderr else []
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout or b""
        if isinstance(stdout, bytes):
            stdout = stdout.decode("utf-8", errors="replace")
        returncode, failure = None, ["Run timed out after 300 seconds"]
    except OSError as exc:
        stdout, returncode, failure = "", None, [str(exc)]
    events = []
    for line in stdout.splitlines():
        if line.strip():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                failure.append(f"Invalid JSON event: {line}")
    return events, returncode, failure


def run_one(cli, home, root, harness, task, number, choice, env=None, *, arm=None):
    project = root / f"{arm or harness}-{task}-{number}"
    project.mkdir()
    if task == LONG_TASK:
        for i, name in enumerate(LONG_FILES):
            (project / f"{name}.txt").write_text("".join(
                f"record {j:02}: {i * 8021 + j * 7817:06x} voltage {j % 17} "
                f"at synthetic station {i}/{j % 7}; code {name}-{j:02}\n"
                for j in range(34)
            ))
        original = None
        turns = LONG_TURNS
    else:
        original = fixture(project, task)
        turns = ((PROMPTS[task], None),)
    model = choice["native"] if harness == "native" else f"{choice['ext']}/{choice['id']}"
    command = [str(cli), "--json", "--mode", "auto-edit" if env and harness == "native" else "full",
               "--backend", harness, "--model", model]
    runtime = env if env and harness == "native" else dict(os.environ, GREENCODE_HOME=str(home), **(env or {}))
    started = time.monotonic()
    events, failure, turn_failures = [], [], []
    session_args = []
    final_text = ""
    for turn, (prompt, expected) in enumerate(turns, start=1):
        previous = set(home.glob("sessions/*.jsonl")) if turn == 1 and task == LONG_TASK else set()
        if env and harness == "native" and task != LONG_TASK:
            prompt += " The fixture runner will execute the tests after you finish. Do not use the shell or access files outside this project."
        batch, returncode, problems = prompt_turn([*command, *session_args, "-p", prompt], project, runtime)
        events.extend(batch)
        failure.extend(problems)
        failure.extend(e["text"] for e in batch if e.get("type") == "error")
        if returncode != 0 or failure:
            if not failure:
                failure.append(f"CLI exited with status {returncode}")
            break
        # An under-budget /compact can finish without spending any model tokens.
        needs_usage = not prompt.startswith("/compact ")
        if not any(e.get("type") == "done" for e in batch):
            failure.append(f"Turn {turn} did not finish")
            break
        if needs_usage and not usage_rows(batch):
            failure.append(f"Turn {turn} did not report model usage")
            break
        final_text = ""
        for event in batch:
            if event.get("type") == "tool_start":
                final_text = ""
            elif event.get("type") == "text":
                final_text += event["text"]
        if expected is not None and final_text.strip() != expected:
            turn_failures.append(turn)
        if turn == 1 and task == LONG_TASK:
            created = set(home.glob("sessions/*.jsonl")) - previous
            if len(created) != 1:
                failure.append("Expected exactly one new native session to resume")
                break
            session_args = ["--resume", created.pop().stem]
    seconds = time.monotonic() - started
    states = [e["data"]["model"] for e in events if e.get("type") == "remote" and e.get("command") == "get_state" and e.get("data", {}).get("model")]
    # The first get_state precedes the harness's set_model; the last one is what served the prompt.
    assert harness == "native" or states or failure, f"no get_state event: cannot confirm the harness used {model}"
    assert all(f"{m['provider']}/{m['id']}" == model for m in states[-1:]), f"Unexpected model, no fallback: {states[-1:]}"
    usage = usage_rows(events)
    totals = metrics(usage)
    ok_run = not failure and bool(usage)
    return {"harness": arm or harness, "task": task, "run": number,
            "passed": ok_run and not turn_failures and check(project, task, original, final_text),
            "turn_failures": turn_failures, "seconds": seconds,
            "requests": len(usage), "aux_requests": sum(e.get("type") == "usage_aux" for e in events),
            **totals, "total_tokens": totals["total_input"] + totals["output"],
            "per_request": [metrics([u])["total_input"] for u in usage],
            "first_request_total_input": metrics(usage[:1])["total_input"],
            "tool_calls": sum(e.get("type") == "tool_start" for e in events),
            "final_text": final_text, "errors": failure, "ok_run": ok_run,
            "models": [f"{m['provider']}/{m['id']}" for m in states[-1:]]}


def advisor_usage(home):
    """Sum the advisor transcripts OMP writes beside its sessions; the CLI's usage events omit them."""
    total = {"input": 0, "cache_read": 0, "cache_write": 0, "output": 0}
    for path in Path(home).rglob("__advisor.jsonl"):
        for line in path.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            usage = (event.get("message") or {}).get("usage") or event.get("usage") or {}
            for key, camel in (("input", "input"), ("cache_read", "cacheRead"), ("cache_write", "cacheWrite"), ("output", "output")):
                total[key] += int(usage.get(key) or usage.get(camel) or 0)
    total["total_input"] = total["input"] + total["cache_read"] + total["cache_write"]
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/greencode-cli")
    parser.add_argument("--baseline-cli", type=Path, help="interleave a pinned-model native comparison with this binary")
    parser.add_argument("--candidate-old-read-previews", action="store_true",
                        help="enable old_read_previews only for the candidate native binary")
    parser.add_argument("--harness", choices=HARNESSES, action="append")
    parser.add_argument("--tasks", nargs="+", choices=[*PROMPTS, LONG_TASK])
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--keep-homes", type=Path, help="copy each harness home here before the temp tree is deleted")
    parser.add_argument("--model", choices=("auto", "luna", "haiku"), default="auto",
                        help="auto: gpt-6-luna, or claude-haiku-5-5 only when a probe shows GPT is rate-limited")
    parser.add_argument("--selfcheck", action="store_true")
    args = parser.parse_args()
    if args.selfcheck:
        selfcheck()
        return
    if not args.output:
        parser.error("--output is required unless --selfcheck is used")
    if args.runs < 1:
        parser.error("--runs must be positive")
    if args.candidate_old_read_previews and not args.baseline_cli:
        parser.error("--candidate-old-read-previews requires --baseline-cli")
    harnesses = list(dict.fromkeys(args.harness or (("native",) if args.baseline_cli else HARNESSES)))
    tasks = list(dict.fromkeys(args.tasks or ([*PROMPTS, LONG_TASK] if args.baseline_cli else PROMPTS)))
    isolated = bool(args.baseline_cli or LONG_TASK in tasks)
    if isolated and (harnesses != ["native"] or args.model != "luna" or args.keep_homes):
        parser.error("native comparison and resume tasks require only the native harness, --model luna, and no --keep-homes")
    arms = {"baseline": args.baseline_cli.resolve(), "candidate": args.cli.resolve()} if args.baseline_cli else {
        harness: args.cli.resolve() for harness in harnesses}
    pi, omp = shutil.which("pi"), shutil.which("omp")
    if "pi" in harnesses and not pi:
        parser.error("pi must be installed and signed in")
    if any(h.startswith("omp-") for h in harnesses) and not omp:
        parser.error("omp must be installed and signed in")
    cli = args.cli.resolve()
    lean = ["--tools", "read,bash,edit,write", "--no-lsp", "--no-rules", "--no-skills", "--no-extensions", "--no-context-files", "--no-title"]
    versions = {"pi": version([pi, "--version"]) if pi else None,
                "omp": version([omp, "--version"]) if omp else None,
                "greencode-cli": version(["git", "-C", str(Path(__file__).resolve().parents[1]), "describe", "--always", "--dirty"])}
    runs = []
    with tempfile.TemporaryDirectory(prefix="greencode-task-bench-") as temp:
        root = Path(temp)
        name, reason = args.model, None
        if name == "auto":
            reason = probe_luna(cli, root)
            name = "luna"
            if reason:
                if not LIMITED.search(reason):
                    parser.exit(1, f"Luna probe failed and it is not a rate limit, so there is no fallback: {reason}\n")
                name = "haiku"
                print("GPT is rate-limited; using claude-haiku-5-5", file=sys.stderr)
        choice = MODELS[name]
        # Pi and OMP get the Claude Code login through the environment; the native harness imports it.
        env = {"ANTHROPIC_OAUTH_TOKEN": claude_token()} if name == "haiku" else {}
        # OMP's auxiliary roles (smol/title, advisor, task agents, ...) default to other models in the user's config:
        # pin every one of them to the benchmark model so nothing else is billed.
        pinned = f"{choice['ext']}/{choice['id']}"
        thinking = ["--thinking", choice["thinking"]]
        overlay = root / "omp-pinned.yml"
        overlay.write_text(f"model: {pinned}\nmodelRoles:\n" + "".join(f"  {role}: {pinned}\n" for role in ("default", "smol", "tiny", "plan", "slow", "task", "commit", "advisor"))
                           + "task:\n  agentModelOverrides:\n" + "".join(f"    {agent}: {pinned}\n" for agent in ("task", "reviewer", "sonic", "explore", "plan", "designer")))
        lean = ["--config", str(overlay), *thinking, *lean]
        commands = {
            "pi": [pi, "--provider", choice["ext"], "--model", choice["id"], *thinking, "--no-context-files", "--no-skills", "--no-extensions", "--no-mcp", "--no-prompt-templates", "--offline"],
            "omp-default": [omp, "--model", pinned, "--config", str(overlay), *thinking],
            "omp-lean": [omp, "--model", pinned, *lean],
            "omp-advisor": [omp, "--model", pinned, *lean, "--advisor"],
        }
        homes = {}
        for arm, binary in arms.items():
            harness = "native" if isolated else arm
            home = root / (arm + "-home")
            home.mkdir()
            homes[arm] = home
            config = choice["config"]
            if arm == "candidate" and args.candidate_old_read_previews:
                config += "old_read_previews = true\n"
            if harness == "native":
                if name == "luna":
                    config += LUNA_NATIVE_PROVIDER
            else:
                protocol = "pi" if harness == "pi" else "omp"
                config += f'[[backends]]\nname={json.dumps(harness)}\nprotocol={json.dumps(protocol)}\ncommand={json.dumps(commands[harness])}\n'
            (home / "config.toml").write_text(config)
            if harness == "native":
                complaint = import_login(binary, home, choice)
                if complaint:
                    parser.exit(1, f"Native login import failed; sign in to {'Codex CLI' if name == 'luna' else 'Claude Code'} first: {complaint}\n")
        if isolated:
            user = root / "user"
            user.mkdir()
            ordered = list(arms.items())
            for number in range(1, args.runs + 1):
                for task in tasks:
                    for arm, binary in ordered if number % 2 else reversed(ordered):
                        home = homes[arm]
                        # No inherited developer keys, proxy, user settings or real project context.
                        runtime = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(user),
                                   "GREENCODE_HOME": str(home), "TMPDIR": str(root)}
                        row = run_one(binary, home, root, "native", task, number, choice, runtime, arm=arm)
                        runs.append(row)
                        print(f"{arm} {task}/{number}: {row['total_tokens']} tokens, {'pass' if row['passed'] else 'fail'}",
                              file=sys.stderr, flush=True)
                        if not row["ok_run"]:
                            break
                    if not runs[-1]["ok_run"]:
                        break
                if not runs[-1]["ok_run"]:
                    break
        else:
            for harness, binary in arms.items():
                home = homes[harness]
                for task in tasks:
                    for number in range(1, args.runs + 1):
                        runs.append(run_one(binary, home, root, harness, task, number, choice,
                                            None if harness == "native" else env))
        advisor = {harness: advisor_usage(home) for harness, home in homes.items()} if not isolated else {}
        for home in homes.values():
            if args.keep_homes:
                args.keep_homes.mkdir(parents=True, exist_ok=True)
                shutil.copytree(home, args.keep_homes / home.name, dirs_exist_ok=True)
    summary = {}
    for harness in arms:
        rows = [r for r in runs if r["harness"] == harness]
        if not rows:
            continue
        summary[harness] = {"passed": sum(r["passed"] for r in rows), "of": len(rows),
                            **{f"{key}_sum": sum(r[key] for r in rows) for key in (
                                "input", "cache_read", "cache_write", "total_input", "output", "total_tokens", "requests",
                                "aux_requests", "seconds")},
                            "median_total_tokens_by_task": {
                                task: statistics.median(values) for task in tasks
                                if (values := [r["total_tokens"] for r in rows if r["task"] == task and r["ok_run"]])
                            },
                            "median_first_request_total_input": statistics.median(r["first_request_total_input"] for r in rows)}
    for harness, usage in advisor.items():
        if any(usage.values()):
            summary[harness]["advisor_usage"] = usage
    report = {"model": choice["id"], "model_choice": args.model, "fallback_reason": reason, "date": date.today().isoformat(),
              "harnesses": list(arms), "versions": versions, "runs": runs, "summary": summary}
    if args.candidate_old_read_previews:
        report["candidate_old_read_previews"] = True
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print("| Harness | Passed | Total input | Cache-read share | Output | Requests | Seconds | Median first-request input |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for harness, s in summary.items():
        rows = [r for r in runs if r["harness"] == harness]
        share = sum(r["cache_read"] for r in rows) / s["total_input_sum"] if s["total_input_sum"] else 0
        print(f'| {harness} | {s["passed"]}/{s["of"]} | {s["total_input_sum"]} | {share:.1%} | {s["output_sum"]} | {s["requests_sum"]} | {s["seconds_sum"]:.1f} | {s["median_first_request_total_input"]:g} |')
    if isolated and any(not row["ok_run"] for row in runs):
        parser.exit(1, "Native measurement incomplete; see recorded CLI errors in the report.\n")


if __name__ == "__main__":
    main()

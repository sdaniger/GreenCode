# Cache and token efficiency

Live model benchmarks use **`gpt-6-luna`**; only when GPT reports a rate limit does `scripts/benchmark_tasks.py` switch the whole run to `claude-haiku-5-5` (`--model auto`, decided once by a probe; `--model luna|haiku` forces one). Local deterministic protocol
fixtures do not call any model. Do not confuse prompt bytes, eligible cache boundaries and actual cached tokens.

## Changes

- Shortened the native base instructions and put worktree-dependent paths after reusable instructions.
- Native threads keep the initial workspace context across restarts. Changes to rules, memory, skills, or the
  opt-in `include_workspace_tree` listing append a superseding snapshot rather than rewriting the cached prefix.
  Compaction refreshes the snapshot. A changed tree can add input tokens because the old snapshot remains in history.
- Native tool schemas use the requesting session's settings, not the most recently started session's settings.
  Starting a different session no longer changes an existing agent's shell or extension tool descriptions.
- First-party OpenAI Responses requests for GPT-5.6+ place reusable instructions in the first `developer`
  message with an explicit breakpoint, without disabling implicit caching for conversation history.
  GPT-6.2+ otherwise places top-level `instructions` after cacheable input and bills them as uncached on
  every request. The breakpoint can be reused across threads when the shared prefix reaches 1,024 tokens.
  This leaves the configured lifetime, ChatGPT/Codex requests, and the caching opt-out unchanged.
  Older OpenAI models use a stable application cache key. OpenAI-specific cache fields are not sent to
  compatible third-party servers.
- Anthropic marks the static system prefix and the preceding user/tool-result endpoint, while automatic caching
  advances over the new history. This preserves an earlier lookup boundary through large tool batches.
- [OpenRouter](https://openrouter.ai/docs/guides/best-practices/prompt-caching) sends both a system breakpoint and
  automatic history caching for Claude. Documented Alibaba-backed Qwen and DeepSeek models use five-minute
  explicit breakpoints instead. Explicit cache writes can cost more than uncached input; `cacheRetention = "none"`
  omits these markers. Other OpenRouter models retain their provider's implicit behavior.
- OpenAI output limits are now sent; cache-write tokens are separated from ordinary input. Compaction and
  subagent usage events are included instead of hidden. Between-turn automatic compaction runs before appending
  the incoming user prompt, keeping that new requirement verbatim.
- DeepSeek requests clamp `max_tokens` to 8,192, count `prompt_cache_hit_tokens` as cache reads, keep
  `reasoning_content` as a thinking block, and compact at 60k tokens instead of the 150k default. DeepSeek caches
  prefixes automatically, so the static system prompt stays first and nothing volatile precedes it.
- The native prompt is 4,359 bytes with 12 tools (system 418, schemas 3,941); `native_prompt_stays_lean` fails if it
  grows past 7,000 bytes, so a new tool cannot silently raise every request.
- Pi/OMP still own their agent loop, prompts, and caching; GreenCode never replays its display transcript into
  them. A local before/after capture kept their tool schemas and cached message prefix unchanged through a
  tool round trip and resume. This is not a direct-engine parity or live billing measurement.

Default cache lifetimes are retained; longer premium writes and padding solely to raise hit percentage are not enabled.
No aggressive history eviction or tighter tool-output truncation was introduced.

### Native resume after a workspace tree change

The deterministic native fixture below compares the old and new CLI: after adding a file, the old CLI rewrote
the system message; the new CLI kept both the system message and the request history prefix unchanged.
The new file, changed memory, and later project rules still appeared in context updates.

An isolated Ollama A/B used 80 empty files, added one file between two turns, and unloaded the model between
builds. Both models returned the expected answers in both arms. Values are CLI-reported tokens on the resumed
request, one sample per build and model:

| Model | Cache reads before → after | Uncached input before → after |
| --- | ---: | ---: |
| `qwen2.5-coder:1.5b` | 308 → 2,293 | 2,059 → 1,228 |
| `paritok-4b-v1:latest` | 307 → 2,297 | 2,064 → 1,233 |

These results apply to this tree-change workload, not to every request. No live OpenRouter usage was measured.

## Observed results — 2026-11-12

### Live GreenCode / Pi 1.5.2 / openai-codex/gpt-6-luna

Five sequential exact-value lookups from a generated 400-line reference file, one real `read` call, low thinking.
All five answers matched their expected values. Six model requests completed in 13.378 seconds total.
Provider-reported usage, not an estimate:

| Scope | Total input | Cached input | Ordinary input | Cache writes | Output | Token-weighted cache hit rate |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| All requests | 22,728 | 17,024 | 5,704 | 0 | 66 | 74.90% |
| Four follow-up turns | 17,706 | 17,024 | 682 | 0 | 32 | 96.15% |

`hit rate = sum(cache_read) / sum(input + cache_read + cache_write)`; do not average per-request percentages.
Cache was not forcibly cleared. The first two requests reported zero cache reads. These are Pi backend measurements,
not evidence that the native-provider changes improved live hit rates or that coding quality matches Pi.

### Native production-request A/B on a deterministic local server

The pre-change and post-change CLI each performed a real local `read` tool round trip and cross-process resumes.
The fixture server did not run a model or report simulated billing/cache usage.

| Check | Before | After |
| --- | ---: | ---: |
| Native system prompt bytes (same project path, no catalog/memory) | 692 | 487 |
| System prefix survives resume after memory file changes | No | Yes (thread snapshot) |
| Updated project instructions apply on resume | Yes | Yes (snapshot invalidated) |
| Tool definitions and non-system message content | Identical | Identical |

This removes 205 bytes of base instructions, **not** 205 tokens. The Pi direct-versus-GreenCode protocol check
also observed zero added prompt bytes, with identical model/messages/tools across its two-request tool round trip.
Its permission-denial check confirmed that the denied tool did not execute.

Raw measured results are saved in [cache-2026-11-12.json](benchmarks/cache-2026-11-12.json).

## Reproduce

Build the CLI, then run the protocol checks without provider credentials:

```sh
cargo build -p greencode-core --bin greencode-cli
python3 scripts/check_pi_parity.py --output /path/to/pi-wire.json
python3 scripts/check_native_cache.py --output /path/to/native-wire.json
# Optional pre-change binary for the native A/B:
python3 scripts/check_native_cache.py --baseline-cli /path/to/old-cli --output /path/to/native-ab.json
```

For actual provider usage, Pi must already be signed in and authorized for GPT-6 Luna:

```sh
python3 scripts/benchmark_pi_cache.py --output /path/to/pi-live.json
```

The live script explicitly selects and verifies `openai-codex/gpt-6-luna`, uses an isolated project/session,
disables discovered extensions/skills/MCP, allows only `read`, checks every answer, and fails on missing usage or
provider errors. It uses the existing Pi login without reading or modifying credentials.

The lookup smoke test above alone does not establish a coding-task quality/token distribution.

## Native harness token efficiency — 2026-11-13

Goal: keep the native loop's OMP-style features (sub-agents, LSP, MCP, skills, memory) while spending Pi-like input tokens on the same tasks and model.

Measurement: `python3 scripts/benchmark_tasks.py --harness native --harness pi --runs 3 --output docs/benchmarks/tasks-2026-11-13-caps7.json` (4 deterministic tasks: bugfix, feature, refactor, lookup; GPT-6 Luna only). `GREENCODE_DUMP_REQUESTS=<dir>` writes one JSON line per native request with the byte sizes of the system prompt, the tool schemas, the whole message list and the last message's blocks — that is how the growth was traced (a single `glob` returned 27 KB of paths).

| Change | Effect on native total input |
| --- | ---: |
| Baseline (before any cap) | 198,360 (1.94x Pi) |
| `glob`/`grep` capped at 200 lines **and** 3,000 chars; `read` defaults to 150 lines with a "more lines" note | 144,547 (1.32x) |
| Tool results capped at 8 KB (head+tail 3 KB), one turn's tool output capped at 3 KB (`context::budget`), `computer`/`task` descriptions shortened | 141,029 (1.40x) |
| System prompt asks for the fewest steps, batched calls and `offset`/`limit` reads | **98,966–113,516 (0.94–1.04x)** |
| `read` returns small files whole (up to 400 lines or 2.5 KB, then names the resume offset), one turn's tool output capped at 2 KB and error results at 1.5 KB (spill file keeps the rest), tool descriptions trimmed to 4,113 B, the system prompt tells the model to answer once a result suffices, codex requests carry a stable `prompt_cache_key` and `text.verbosity: low`, and a missing interpreter (exit 127) is named once | see the paired runs below |

Paired runs on gpt-6-luna after those changes (4 tasks x 3 runs per harness, same session each; `docs/benchmarks/tasks-2026-11-13-caps*.json`):

| Run | Native passed / input | Pi passed / input | Native / Pi |
| --- | ---: | ---: | ---: |
| caps8 | 12/12, 107,689 | 12/12, 117,751 | 0.91x |
| caps9 | 11/12, 112,076 | 12/12, 111,723 | 1.00x |
| caps10 | 12/12, 94,677 | 12/12, 114,247 | 0.83x |
| caps11 | 12/12, 117,674 | 11/12, 106,143 | 1.11x |
| caps12 (error cap, before read/prompt change) | 12/12, 136,449 | 12/12, 105,386 | 1.29x |
| caps13 (read whole small files) | 12/12, 113,144 | 12/12, 95,691 | 1.18x |
| caps14 (+ "answer once a result suffices") | 12/12, 93,519 | 12/12, 111,471 | 0.84x |
| caps15 (same prompt, `read` hint only after a cut; final code) | 12/12, 116,561 | 12/12, 112,379 | 1.04x |

Not every run is below Pi: caps11-13 were above it, driven by native feature/refactor/lookup runs that took extra confirming steps or read large outputs, and Pi's own total moves 95k-118k between runs. The caps9 miss was a refactor that stopped after `python` returned 127 (since fixed by the exit-127 hint). The two runs on the final prompt (caps14, caps15) sum to 210,080 vs 223,850 (0.94x), 24/24 each; the eight runs together sum to 891,789 vs 874,791 (1.02x; 95/96 passed each). Treat it as "at or slightly below Pi on the final code, not reliably in every run"; N is 12 per cell and the subscription cache adds noise.

The `lsp` tool (370 B of schema) was added afterwards; trimming the `task` and `computer` descriptions kept the fixed cost flat (tools 4,805 B, system 960 B, first request 1,188 tokens vs Pi's 1,178). Final run with LSP present: native 109,791 vs Pi 116,599 total input (0.94x), 12/12 each (raw report no longer on disk; figures kept from the run log). Native's first request is slightly cheaper than Pi's (1,162 vs 1,179 tokens: 911 B system + ~4.6 KB tool schemas + the prompt); the earlier gap was entirely tool output growth, not the fixed prompt.

Current native behavior: successful `bash`, `grep`, `glob`, and `web_fetch` calls share a 2 KiB preview budget per tool round, with a 512-byte minimum per result for larger batches. Long previews keep both ends and name a file with the original tool result. Targeted `read` windows and errors are not subject to this shared budget. Summary compaction sends no tool schemas and excludes workspace snapshots; the next request uses the latest workspace files. The historical benchmark above does not measure these changes.

## Native vs. Pi — local read and resume

Ollama 0.40.11 (`paritok-4b-v1:latest`) served both the current native CLI and Pi 1.5.2
through the same OpenAI-compatible endpoint. Each ran two fresh sessions on the same 80-line lookup file,
in forward then reverse harness order, with project instructions, skills and extensions absent.
The first turn called `read` with `path=reference.txt`, `offset=1`, and `limit=80`;
each of the two follow-ups launched a new CLI process, resumed the same session, and answered without tools.
These are Ollama usage counts, not byte or price estimates. The cache was not cleared between runs.

| Harness | Correct answers | First request input (per run) | Total input incl. cache | Uncached input after resume | Output |
| --- | ---: | ---: | ---: | ---: | ---: |
| Native, default tools | 6/6 | 2,102 | 22,990 | 156 | 70 |
| Pi, `read,bash,edit,write` | 6/6 | 1,433 | 17,776 | 156 | 84 |
| Native, `read,bash,edit,write` | 6/6 | 749 | 12,187 | 156 | 77 |

Default native input was 29% higher on this short workload; matching the four-tool catalog used 31% less
than Pi. Default native and Pi each used 39 uncached input tokens per resumed request. For a session that
only needs the four core tools, run `greencode-cli --tui --tools read,bash,edit,write`; tools outside the
allowlist are unavailable in that launch. The full catalog remains the default.

The earlier [GPT-6 Luna coding-task run](benchmarks/tasks-2026-11-17-omp-tools.json) recorded
109,477 native versus 123,485 Pi input tokens, with 12/12 passes each. It predates this local run;
neither result establishes parity on every provider or workload. The current wire regression test
separately verifies an unchanged tool schema and cached message prefix across process restarts,
including when updated workspace rules are appended.

## Native defaults vs. Pi — GPT-6 Luna, 2026-11-18

Default native configuration (full tool catalog, the user's custom agents listed) against Pi 1.5.2, both on
`gpt-6-luna` with low effort, four tasks per run. Before: [3 runs](benchmarks/tasks-2026-11-18-default-before.json)
105,879 vs 102,253 total input (1.04x), first request 1,404 vs 1,178 tokens. Session traces showed that every
`grep`/`glob` line carried the absolute project path. Changes: search results are project-relative, the workspace
context preamble is shorter, and custom-agent descriptions are capped at 60 characters.

After: [5 runs](benchmarks/tasks-2026-11-18-default-after.json), 20/20 each: native 188,171 vs Pi 184,938 total
input (1.02x), 114 vs 119 requests, 4,797 vs 6,352 output tokens; first request 1,337 vs 1,177. Per task, native
used less input on refactor and more on bugfix; the bugfix traces show both harnesses spending a request on the
missing `python` binary, so the per-task gaps are within run-to-run variance at this N.

## Native old-read previews — not adopted

In the paired GPT-6 Luna run, both arms used
low effort and three runs per generated task. The candidate sent a 512-byte preview of older successful
`read` results, kept at least the latest two turns verbatim, and saved the original text for retrieval. It did not
shorten the input to summary compaction. Total tokens include uncached input, cache reads and writes,
and output, including the compaction request.

| Task | Baseline median tokens | Candidate median tokens | Change | Baseline passed | Candidate passed |
| --- | ---: | ---: | ---: | ---: | ---: |
| Resume + summary compaction | 61,834 | 50,247 | −18.7% | 2/3 | 1/3 |
| Lookup | 4,740 | 4,468 | −5.7% | 3/3 | 3/3 |
| Bug fix | 7,874 | 7,843 | −0.4% | 3/3 | 3/3 |

All misses were on the final turn after compaction. The candidate omitted the `RIVER:` prefix twice;
the baseline kept it but added a space once. The candidate met the token threshold (at least 10% less
on the long task and no more than 5% extra on the short tasks) but failed the quality gate. The
prototype and its opt-in setting were removed. The short tasks never reached the preview boundary, so
their deltas are run-to-run variation, not measured savings from this feature.

## Native prompt and compaction costs — not adopted

Shorter built-in instructions and tool descriptions left tool names, argument schemas, user messages,
and tool results unchanged. The local wire fixture measured 613 → 453 system bytes and
6,553 → 6,406 tool declaration bytes, with the resumed cache prefix intact. Bytes are not token counts.

An initial trial skipped summary compaction
whenever the whole history fit the retained tail. It recovered all three exact answers after resume,
but the long-task median rose from 60,072 to 65,139 total tokens (+8.4%): the follow-up replayed
thousands of tokens, and some runs performed extra reads.

A second candidate skipped compaction only for text-only histories under 2,000 bytes and asked the
summarizer to preserve exact constraints and formats. GPT-6 Luna used low effort in both arms.
The first batch stopped after two pairs; the third used fresh isolated homes and the same binaries
and fixtures. Counts include cache hits, cache writes, output, and auxiliary requests.

| Task | Baseline runs (tokens) | Candidate runs (tokens) | Median change | Passed (baseline → candidate) |
| --- | ---: | ---: | ---: | ---: |
| Resume + summary compaction | 62,569 / 60,162 / 60,919 | 55,381 / 57,211 / 57,187 | -6.1% | 1/3 → 3/3 |
| Lookup | 2,957 / 2,947 / 4,468 | 8,968 / 4,233 / 2,785 | +43.2% | 3/3 → 3/3 |
| Bug fix | 7,889 / 7,874 / 7,837 | 7,430 / 7,456 / 7,412 | -5.6% | 3/3 → 3/3 |

The narrower candidate missed both the long-task target (at least 10% fewer tokens) and the short-task
limit (at most 5% more). The prompt, tool descriptions, and compaction behavior were restored.

## Native recoverable read previews — opt-in

`old_read_previews = true` shortens older successful reads only on the model wire. It leaves the
original prompts and results in the session, saves exact results for retrieval with `read`, and
adds recovery instructions only to requests containing excerpts. With no workspace rebase pending,
the option also avoids a lossy summary when the whole history fits the tail and the context is below
the compaction threshold. None of this changes the default request prefix.

The paired run used GPT-6 Luna at low
effort. The lookup task took five pairs because its live tool choices varied; the other tasks took
three. Total tokens include cached input, cache writes, output, and auxiliary requests.

| Task | Baseline median | Preview median | Change | Passed (baseline → preview) |
| --- | ---: | ---: | ---: | ---: |
| Resume + summary compaction | 58,497 | 50,233 | −14.1% | 1/3 → 3/3 |
| Lookup | 4,465 | 4,462 | −0.1% | 5/5 → 5/5 |
| Bug fix | 8,016 | 7,882 | −1.7% | 3/3 → 3/3 |

Short tasks did not reach the preview boundary. The local protocol fixture confirmed identical
request bodies for a short tool turn, so their measured deltas are not attributed to the option.
In three additional live recovery cases, the model reread a saved snapshot after the source file
was deleted and returned the exact omitted value with the original required answer format.

Reproduce with `--baseline-cli PATH --cli PATH --model luna --candidate-old-read-previews --tasks resume lookup bugfix --runs 3`.
To extend the short lookup comparison, use `--tasks lookup --runs 5`.

## Provider references

- [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)
- [Chat Completions cache options and usage fields](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)

Run the live coding-task comparison with `python3 scripts/benchmark_tasks.py --output docs/benchmarks/tasks-<date>.json` (`--keep-homes DIR` copies each harness home before the temp tree is deleted; OMP advisor turns are billed outside the CLI usage events, so the script sums them from `__advisor.jsonl` into `summary.<harness>.advisor_usage`). It uses GPT-6 Luna on the user's subscription; with `--model auto` (default) one probe prompt decides whether GPT is rate-limited, and only then the whole run uses Claude Haiku 5.5 through the Claude Code login (`fallback_reason` in the report records why). Results are small-N and should not be treated as conclusive.

Use `--baseline-cli PATH --cli PATH --model luna --runs 3` to compare two native binaries.
The runner alternates them on the same generated tasks in separate homes, without local project
instructions or inherited credentials other than the imported subscription login. `--tasks resume lookup bugfix`
includes a resumed multi-turn session with summary compaction and two short tasks. The report separates
uncached input, cache reads, cache writes and output, counts `usage_aux`, and records each turn that missed
its expected answer. Compare `median_total_tokens_by_task`, not cache-hit percentage alone.

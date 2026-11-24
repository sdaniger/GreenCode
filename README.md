# GreenCode

A native agent workspace for macOS, Linux and Windows: Rust + GPUI, no WebView. Native mode runs in-process; external agents are lazy subprocesses.

T3 Code / OMP feature parity is in progress, not complete. The [feature ledger](docs/parity.md) records the remaining surface and verification gaps.

Start with [Downloads](#downloads), the [native terminal UI](#native-terminal-ui), or
[installed agents](#use-installed-agents). For automation, see [thread scripting](#thread-scripting),
[remote access](#remote-access), and [workflows](#workflows-labs). The
[cache-efficiency report](docs/cache-efficiency.md) contains benchmarks and reproduction steps.
Check `greencode-cli --help` for which automation commands your build includes.

## Downloads

Download a [release archive](https://github.com/sdaniger/GreenCode/releases) for macOS (Apple Silicon or Intel),
Linux x86-64 (built on Ubuntu 24.04), or Windows x86-64. Each contains `greencode` (desktop GUI) and
`greencode-cli` (CLI/TUI), with `.exe` extensions on Windows. `SHA256SUMS` lists the SHA-256 of each archive.

On Linux, the GUI needs X11 or Wayland, Vulkan, Fontconfig and XKB libraries. On Windows, install
Git for Windows to use the native `bash` tool. The Windows embedded terminal uses ConPTY;
Pi/OMP session handoff from the GUI is not available there. GreenCode's built-in Claude and
ChatGPT subscription credential storage and CLI login import require macOS or Linux.
In-app updates remain macOS-only.

## Native terminal UI

```sh
greencode-cli                           # opens the native TUI in a terminal
greencode-cli --continue                # latest native session in this directory
greencode-cli --resume THREAD_ID
make cli                                # build and run from a source checkout
```

This runs GreenCode's Rust agent, not OMP. Use `-p "PROMPT"` for one turn,
`--text` for line-oriented input, and `--tui` to require a terminal. This is not
a drop-in OMP replacement: use the installed OMP backend or its own terminal UI
for OMP-only features. See the [supported mappings and differences](docs/parity.md#omp-execution-features).

The native agent uses Ponytail (full) by default: make the smallest complete change,
reuse existing code, and verify it without cutting safety or error handling. No
plugin installation is required. A custom `system_prompt` in `config.toml` (or
`--system-prompt TEXT` in the CLI) replaces the default, including Ponytail;
`--append-system-prompt TEXT` keeps it. External backends keep their own prompts.

Type `/help` for commands and `/hotkeys` for active application shortcuts.
By default, `Alt+M` or `Ctrl+L` opens the model picker, `Ctrl+P` cycles models,
and `Alt+Shift+P` toggles plan mode. `Ctrl+R` searches the current thread's
prompt history; Enter restores the selection to the composer without sending it.
When no turn is running, `Ctrl+C` clears the draft. `Up`/`Down` can recover
the last 100 cleared drafts alongside submitted prompts, newest first. Cleared
drafts never enter session storage or `Ctrl+R` search. Use
`/settings recall_cleared_drafts false` to stop retaining future clears.
`Alt+Enter` inserts a newline, `Ctrl+S` steers a running turn, and `Esc` interrupts it.
`Ctrl+G` opens the draft in `$VISUAL` (falling back to `$EDITOR`). Save and
exit to bring the edited text back without sending it. A failed editor leaves
the original draft intact.
`/mode`, `/plan`, `/thinking`, `/resume`, `/fork`, and `/compact` are available without leaving the TUI.
`/review [FOCUS]` opens a picker for tracked staged/unstaged changes, a base
branch (merge base to `HEAD`), or a recent commit. It sends the selected diff to
the current model in plan mode. Review the diff for sensitive content first;
untracked files are excluded. Use `/plan` afterward to restore the previous mode.
For a remote diff, use `/review owner/repo#123 [FOCUS]` or pass a
`https://github.com/OWNER/REPO/pull/123` or
`https://gitlab.com/GROUP/REPO/-/merge_requests/123` URL after `/review`.
This uses the locally authenticated `gh` or `glab` to read the diff, then sends
it to the current model without tools or local image attachments. Only specify
diffs you can share with that provider. `Esc` cancels the fetch; no comments are posted.
While a turn runs, Enter queues the next prompt by default. `Ctrl+Q` (or `Ctrl+Enter`
if the terminal distinguishes it) queues explicitly even when Enter is set to steer.
`Alt+Up` or `Shift+Up` recalls the last queued prompt into an empty composer.
After interruption, `/queue run` resumes held prompts and `/queue clear` discards them.
`Ctrl+O` expands or collapses tool output. `Ctrl+Shift+O` toggles tool activity
and `Ctrl+T` toggles thinking traces. Tool failures remain visible.
If your terminal cannot distinguish `Ctrl+Shift+O` from `Ctrl+O`, remap it.

Use `/settings` to find a terminal preference and Enter to fill in the edit
command. For example, `/settings show_usage false` hides token counts and
`/settings show_usage default` resets the setting. UI changes apply immediately;
`auto_resume` is only used at startup. Changes are saved to global `config.toml`,
except when launched with `--config`, where the edits last only for that launch.
Set `show_turn_usage` and/or `show_turn_time` to `true` to show per-turn model
tokens and elapsed time below future successful replies, including time spent
on tools. These rows are not restored with the transcript.

Use `/export [PATH]` in a saved session or `greencode-cli --export-html THREAD_ID [PATH]`
to write a local HTML copy of the Markdown transcript. It contains no executable HTML,
does not upload anything, and refuses to overwrite an existing file. The default
destination is `~/.greencode/exports/THREAD_ID.html`. Review the transcript before sharing it.

## Thread scripting

These commands work with locally saved threads; they do not control an active desktop turn.

```sh
greencode-cli thread list --json
greencode-cli thread start --project . --wait --json "Summarize the README"
greencode-cli thread show THREAD_ID         # Markdown; add --json for structured messages
greencode-cli thread send THREAD_ID --wait "List the setup commands"
```

Replace `THREAD_ID` with the ID returned by `start`. `start` accepts `--backend`
and `--model`; `send` uses the thread's saved backend and model. Both accept `-`
instead of a prompt to read stdin. Without `--wait`, they return the ID and run
in a background process. Do not send to a thread that is also open in the
desktop app: the desktop app does not share the CLI's turn lock.

## Use installed agents

Open ⌘K and select a backend in a new thread: `pi`, `omp`, `opencode`, `codex`, `claude-code`, or `antigravity`.
Pi, OMP and OpenCode are discovered in common installation directories even when Finder's PATH is minimal.
Kiro, Grok Build, fx and Gemini CLI appear as `kiro`, `grok`, `fx` and `gemini` when their executables are installed.
Those directories are also appended to subprocess PATH so script-based agents can find Node/Bun.
They use their own credentials, skills, extensions, MCP setup and native conversation state. Codex/Claude use pinned ACP adapters via npx. Google Antigravity uses the official `antigravity-acp` ACP runtime (downloaded from the ACP registry or using an installed `agy` CLI) with Google OAuth sign-in, matching T3 Code.

```sh
greencode-cli --backends
greencode-cli --backend pi -p "Inspect this project"
greencode-cli --backend opencode -p "Inspect this project"
greencode-cli --backend omp -p "Inspect this project"
greencode-cli --resume THREAD_ID -p "Continue"
greencode-cli --backend pi --rpc '{"type":"get_state"}'
greencode-cli --backend pi --rpc '{"type":"get_available_models"}' --json
```

Typing `/` discovers slash commands before the first prompt without making a model request. Advertised commands,
including Pi/OMP aliases and later catalog updates, appear in completion and take precedence over GreenCode commands.
If discovery times out before a conflicting command can be resolved, its input is restored instead of guessing.
OMP `ask` dialogs accept multiple questions, multiple-choice selections and free-text answers in the native composer.
In `greencode-cli`, answer each question with option numbers (comma-separated for multiple choice) or free text.
Pi `/reload` reloads its extensions, skills, prompts, themes and context files in the current session.
Pi `/changelog` shows the installed Pi's changelog without a model request.
Pi `/hotkeys` lists the installed Pi's configured terminal keybindings, not GreenCode shortcuts or extension shortcuts.
Pi `/trust` shows the current and saved project trust, and confirms the exact paths before saving; restart the Pi session to apply it.
Pi `/export path.jsonl` exports the current branch; replacing an existing file requires confirmation.
OpenCode project and global Markdown commands also appear in completion. Pi/OMP commands with RPC equivalents
are available directly; to use terminal-only commands or custom terminal widgets, choose “Pi / OMP の端末 UI に引き継ぐ”
in the palette on an idle Pi/OMP thread. Exit the terminal UI to return to GreenCode. GreenCode stops its RPC
process before launch and checks the terminal's session and branch before importing changes. If the histories
diverge after a confirmed terminal exit, the same GreenCode instance can adopt the terminal history after
confirmation and a backup. An unclean or unverifiable exit leaves the thread blocked rather than risking lost
history; do not open the same session manually in another Pi/OMP process. Handoff is unavailable in plan mode or
with a GreenCode policy or sandbox enabled.
Configure an alternate executable or extra flags with a backend entry:

```toml
[[backends]]
name = "pi"
protocol = "pi"                 # pi | omp | acp (default)
command = ["/absolute/path/to/pi"]
```

Enter during a run queues a follow-up. ⇧⌘⏎ steers a running Pi/OMP or native turn (when no approval is pending).
Native threads steer at the next model request boundary.
Stopping holds the queue; resume it from the composer or palette. Drafts, held queues, pin/archive state and
per-thread permission modes survive restarts. Use the palette to pin, archive or find archived threads.
Pi/OMP's permission hook changes execution approval only; it adds no system text or tool definitions.

```sh
export ANTHROPIC_API_KEY=...          # and/or OPENAI_API_KEY
make run                              # GUI, debug build with backtraces
make cli ARGS='-p "prompt"'           # headless
make debug                            # GUI under lldb
make test                             # all tests; make test ARGS=name for one
make release                          # optimized binaries in target/release
```

`make` alone lists every target. Set `GREENCODE_HOME=/some/dir` to debug against a scratch home instead of `~/.greencode`.

The model picker (model chip, ⇧⌘M, `/model`, `/models`) works like OpenCode's: a backend rail, models grouped by provider
(`opencode · ollama`, …), recently used first, multi-word search, ⌘1–⌘9, four rows per section plus Show more, and a
思考の強さ column for the thread's own reasoning control. Choosing a model of another backend switches the backend, or opens
a new thread when the current one already has history. Native models come from each provider's `/models` endpoint; external
agents are asked for theirs on first use, cached in `~/.greencode/backend-models.json` (OpenCode per project) and refreshed
with "外部エージェントのモデル一覧を再取得" or `greencode-cli --models BACKEND`.
The reasoning column offers model-specific native effort levels, Pi/OMP thinking levels, and advertised ACP reasoning options; agents that expose none show an explanation.

`/login` works like `opencode auth login`: a searchable picker shows every provider with its sign-in state.

- **Claude Pro/Max**: opens the browser, paste the code shown there. Tokens go to `~/.greencode/anthropic-oauth.json` (600) and refresh automatically.
- **ChatGPT Plus/Pro** (`/login chatgpt`): device flow like Copilot; the native loop then talks to the Codex Responses endpoint as `chatgpt/<model>`. Tokens go to `~/.greencode/chatgpt-oauth.json` (600) and refresh automatically.
- **GitHub Copilot**: device flow. The code is copied to the clipboard and the browser opens; approve there and GreenCode finishes by itself.
- **Mistral Vibe** (`/login vibe`, or the browser chip in `/login mistral`): the same browser sign-in as the Vibe CLI and OMP. Approve in the browser; GreenCode exchanges it for a regular Mistral API key billed against the plan's Vibe quota and stores it as the `mistral` key in `~/.greencode/auth.json`.
- **Google Antigravity** (`/login antigravity`, or Settings → 接続): uses the official `antigravity-acp` runtime (auto-installed from the ACP registry, or installed `agy acp`) and signs in via Google OAuth (`oauth-personal` / `oauth-business`) or `GEMINI_API_KEY`; credentials stay inside the official runtime's storage (`~/.gemini/antigravity-acp/`).
- **API-key providers** (`/login openai`): one masked field. The key is stored in `~/.greencode/auth.json` (600), replacing any earlier one.
- `/logout [provider]` (also in the palette and Settings → 接続) removes a stored key or subscription. `/usage` shows Claude and Copilot subscription limits. Select text in a reply and press ⇧⌘C to quote it into the draft with a comment.

Settings → 接続 shows providers and agents in a two-pane list with status, sign-in, runtime details and enable controls.

The same from a terminal: `greencode-cli auth login [provider]`, `auth list`, `auth logout PROVIDER`. `greencode-cli update --check` checks the configured GitHub Release; `greencode-cli update` downloads and verifies the matching macOS asset before replacing the current binary. Set `update_repo = "OWNER/REPO"` in `~/.greencode/config.toml`, or use `GREENCODE_UPDATE_REPO` / `--repo OWNER/REPO`. Keep several accounts per provider with `auth save PROVIDER NAME` / `auth switch PROVIDER NAME` (or the palette's アカウント切替).
Precedence for one provider: `api_key` written in config.toml > auth.json > subscription > environment variable.
Using a subscription token from a third-party client may conflict with the provider's terms; the account risk is yours.

Other built-in commands: `/new`, `/clear`,
`/mode ask|auto-edit|full|plan`, `/plan`, `/compact`, `/status`, `/diff`, `/login [provider]`, `/logout [provider]`, `/usage`, `/provider`, `/undo`, `/fork`, `/export`, `/attach`,
`/effort`, `/sandbox`, `/settings`, `/init`, `/review`, `/commit`, `/skill NAME`, `/help`; `!command` runs a shell command directly.

`/webhook PROMPT` creates a loopback URL for the current thread while the app is running. Use `{{body.a.b}}`,
`{{headers.name}}`, `{{query.name}}`, `{{body}}` or `{{request}}` to include untrusted request data;
`/webhook` lists URLs and deletion actions, and `/webhook-secret ID SECRET` requires a GitHub-style HMAC-SHA256 signature.
The listener is local only; a public URL requires your own tunnel, such as `cloudflared`. The port defaults to
47321 and can be changed with `webhook_port` in `ui.json`. Without a secret, anyone who can reach the URL can trigger it.

- **Tools**: `read`, `write`, `edit`, `bash` (`background: true` for servers and watchers), `grep`, `glob`, `web_fetch`,
  `web_search` (DuckDuckGo, no key), `todo`, `memory`, `task` (`task`, `scout`, `reviewer`, `security` agents), `mcp`, and `computer` on macOS.
  The native `lsp` tool queries installed Rust, Go, TypeScript/JavaScript and Python language servers.
  The lazy `ext` tool adds OMP-style tools listed by name only: `hread`/`hedit`, `ast`, `eval`, `debug`, `advisor`, `learn`, `ask`,
  `checkpoint`/`rewind`, `new_context`, `recall`, `wait`, `manage_skill`, `find` (files ranked by how many words of a plain-language
  query they contain) and `github` (read-only `gh`: issues, PRs, diffs, checks, searches, Actions runs and failed logs). Each can be turned off in Settings (`ext_off` in `ui.json`).
- **Approvals**: new threads start in `full` (YOLO): nothing asks. Pick `ask`, `auto-edit` or `plan` per thread (⇧⌘A) or as the default in Settings. In the other modes, reads inside the project and shell commands that only look (`ls`, `cat`, `grep`, `find`, `git status|diff|log`, … joined by `|`, `&&`, `;`, with no redirection, `$()` or paths outside the project) never ask, in every mode. Anything else asks unless the mode or an "always allow" rule covers it. Writes to `.git/` always ask, because git hooks and config run commands.
- **Plan mode** is read-only: tools that change anything are refused until you switch modes.
- **Undo** (`/undo`, ⇧⌘Z) removes the last turn, restores files changed by `write`/`edit` (not by `bash`), and puts the prompt
  back in the composer. `/fork` copies the thread; `/export` writes Markdown to `~/.greencode/exports`.
- **Images**: paste with ⌘V, `/attach`, or name an image file in the prompt (`@shot.png`); screenshots and reasoning are shown in the thread.
- **File previews**: the right panel refreshes open files as they change; HTML/SVG/PDF use Quick Look thumbnails (no JavaScript), with browser opening and rendered snapshots attachable to the next prompt.
- **Sandbox** (`sandbox = true`, macOS): `bash` can write only inside the project, temp directories and `~/.greencode`.
- **Cost**: add `[prices]` with `"provider/model" = [input, output, cache_read, cache_write]` in USD per million tokens to see dollars.
- **Composer** (as in T3 Code): ↑/↓ recall sent prompts, ⌘S stashes or restores a draft, ⌥↑ edits the last queued message,
  ⌘⏎ on a new thread sends it and opens the next draft, and "最後の回答を引用" quotes the last reply. The follow-up behavior
  (queue or steer) and "hide tool calls" are palette settings; `/goal CONDITION` keeps a native agent working until it reports the goal met.
- **Threads**: `/rename`, snooze (hidden until the time passes or the thread needs approval), threads without a project
  (⌥⌘N, in `~/.greencode/scratch`), and per-project default model, backend and permission mode. ⇧⌘A cycles the permission mode.
- **Git**: the header shows the branch; the palette switches branches, commits through the agent, pushes and opens a pull request
  with `gh`. `/branch NAME`, `/push`, `/pr` do the same.
- **Terminal** (⌘J): project-scoped PTY tabs with 5,000 lines of history; the terminal resizes with the panel. Close or add tabs in the drawer; selected output can be inserted into the composer. The last 500 lines are saved locally when a tab closes or the app exits.
- **Settings**: open settings with `⌘,`, `/settings`, or the palette. 一般 shows preview cards for
  System, Light and Dark, followed by swatches for Emerald, Emerald Light, Paper, Sand and Graphite.
  Select a card or use `/theme` to switch immediately. The sidebar also offers モデルと権限
  (default model, effort, permission modes, sandbox, allow rules),
  接続 (providers and agents), ツール (automatic compaction, learning, individual `ext` tools), プロジェクト (project defaults, worktrees)
  and キーとファイル (custom keybindings, config files); it always opens on 一般. Values persist in
  `~/.greencode/ui.json`; custom bindings can also be represented as `{"keys": {"cmd-shift-p": "/plan"}}`.
- A notification appears when a turn finishes or needs approval while GreenCode is in the background. ⌘K with `?text` searches conversations.

## Remote access

Run `greencode-cli serve` and open the printed URL in a browser. The server defaults to
`127.0.0.1:47322`; use `--port` to change the port or explicitly pass `--host 0.0.0.0`
to allow network access. For a phone, replace `0.0.0.0` in the printed URL with the
machine's LAN address. The URL contains a secret that grants control over agents and
their tools: anyone who has it can read conversations and operate the machine.
Keep it private, use a trusted network, and avoid exposing the unencrypted HTTP server
to the internet. The secret is stored in `~/.greencode/remote-token`; restart with
`--rotate` to invalidate the old URL.

### Login service

On macOS (launchd) or Linux (systemd user session), install from the final
location of your `greencode-cli` binary: the service records its absolute path.

```sh
greencode-cli service install
greencode-cli service status
greencode-cli service restart
greencode-cli service uninstall
```

`service install --lan` enables network access. `serve` reads `KEY=VALUE`
settings from `~/.greencode/service.env` without shell expansion and the
service logs to `~/.greencode/service.log`. On Windows, run `serve` manually.

### Guest links

In the remote UI, select a thread and click **Share** to create a read-only
link to that thread, valid for 24 hours. A guest can read the transcript and
submit a request, but only the owner can send that request to the agent or
dismiss it. **Unshare** revokes all guest links for the selected thread.

### Outside agents via MCP

Run `greencode-cli mcp` as a local newline-delimited stdio MCP server. For Claude Code,
choose **他のアプリから GreenCode を使う（MCP）** in the palette to copy the registration
command, or run `claude mcp add greencode -- /absolute/path/to/greencode-cli mcp`.
Other MCP clients can launch the same command. The server exposes projects, threads,
text transcripts, models, and synchronous thread turns (10 minute timeout). Approval
requests are denied; actions already allowed by the thread's permission mode still run.

`greencode-cli serve` also accepts single JSON-RPC requests at `POST /mcp` with the
same bearer token as remote access, returning JSON rather than SSE. Notifications return
HTTP 202. This endpoint does not provide OAuth: protect the token and use TLS or a
trusted local network. Anyone with the token can read threads and run agents.

## Workflows (Labs)

Enable **設定 → ラボ → ワークフロー** or set `"labs_workflows": true` in
`~/.greencode/ui.json`. Save a definition as `~/.greencode/workflows/check-in.toml`
(override the definitions directory with `GREENCODE_WORKFLOWS`):

```toml
name = "check-in"

[[steps]]
type = "ask"
id = "review"
question = "Is the draft ready?"
options = ["yes", "no"]
```

Run `greencode-cli workflow start check-in`, then use the returned run ID:

```sh
greencode-cli workflow show RUN_ID
greencode-cli workflow answer RUN_ID review yes
greencode-cli workflow list
```

Runs are saved under `~/.greencode/workflow-runs/`. Use `resume RUN_ID` for
a paused run or `cancel RUN_ID` to stop it, both under `greencode-cli workflow`.
Run only workflow definitions you trust: shell steps execute local commands.

## Keys

| | |
|---|---|
| ⏎ / ⇧⏎ | send / newline |
| ⌘N / ⇧⌘N | new thread / new thread in its own git worktree |
| ⌘1–9 | switch thread |
| ⌘K | palette: commands, models, permission mode, backends, threads |
| ⌘D | uncommitted-changes panel |
| ⌘O | open a project folder |
| ⌘⏎ / ⇧⌘⏎ / esc | allow / always allow / deny an approval; esc also stops a run |
| tab | complete `/command` or `@file` |

## Files

Everything lives in `~/.greencode` (override with `GREENCODE_HOME`).

| Path | Purpose |
|---|---|
| `config.toml` | settings, below |
| `sessions/*.jsonl` | one append-only file per thread |
| `memory/MEMORY.md`, `<project>/.greencode/MEMORY.md` | persistent memory the agent edits with its `memory` tool |
| `skills/<name>/SKILL.md` | skills; the agent writes new ones itself. `~/.claude/skills` and `<project>/.claude/skills` are read too |
| `commands/<name>.md` | `/name` prompt templates; `$ARGUMENTS` is replaced |
| `extensions/*.lua` | Lua extensions; `<project>/.greencode/extensions` loads after you approve it once |
| `mcp.json`, `<project>/.mcp.json` | MCP servers, Claude Code format |
| `allow` | "always allow" grants, one per line |

```toml
# ~/.greencode/config.toml
update_repo = "OWNER/REPO"       # GitHub Releases used by /update and greencode-cli update
models = ["anthropic/claude-sonnet-5-5", "openrouter/openai/gpt-5"]  # extra picker entries
mode = "full"                             # full (default, nothing asks) | auto-edit | ask | plan
effort = "high"                           # Anthropic output_config.effort; unset = API default
compact_at = 250000                       # summarize history above this many context tokens
model_compact_at = { "openai/gpt-5*" = 120000, "openai/gpt-5.4" = 180000 }
compaction_mid_turn = true                 # false delays main-agent auto-compaction until the next user turn
compaction = "summary"                    # summary | snap
old_read_previews = false                 # opt in to replaying older successful reads as recoverable excerpts

[providers.my-llm]                        # custom provider; the GUI palette's "カスタムプロバイダを追加…" appends this block
kind = "openai"                           # openai (any OpenAI-compatible server) or anthropic
base_url = "http://localhost:8080/v1"
api_key_env = "MY_LLM_KEY"                # or api_key = "..." stored here; omit both for keyless local servers
models = ["qwen3-coder"]                  # shown in the picker as my-llm/qwen3-coder

[[backends]]                              # external agents over ACP
name = "claude-code"
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]

[[backends]]
name = "codex"
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
```

Predefined providers (set the key variable, then use `name/model`): anthropic, openai, openrouter, google (`GEMINI_API_KEY`),
xai, groq, deepseek, mistral, together, fireworks, cerebras, deepinfra, perplexity, nvidia, huggingface (`HF_TOKEN`),
github (`GITHUB_TOKEN`), moonshot, zai, and keyless ollama and lmstudio. A `[providers.NAME]` entry of the same name overrides one.
Bedrock, Vertex AI and Azure deployment URLs use their own authentication and are not built in.

On macOS native threads have a `computer` tool with Codex Computer Use's actions: `list_apps`, `get_app_state`, `click`,
`drag`, `paste`, `perform_secondary_action`, `press_key`, `scroll`, `select_text`, `set_value` and `type_text`.
Get the app state first, then use its screenshot coordinates for background clicks (including double-click and
right-click), drags and scrolling. Input is addressed to that window without moving the physical cursor or
asking macOS to bring the app forward. Use `element_index` when available; the app can still ignore a synthetic
event or activate itself. If the target is observed in front afterwards, the reply reports it. `super+a` can select
all text in a focused background text field through accessibility when the menu shortcut is unavailable.
`get_app_state` returns indexed elements, secondary actions and a screenshot; later replies return changed elements
unless `disable_diff` is set. Closed apps are launched in the background. `paste` restores the clipboard afterwards.
Approvals are scoped per app (`computer:<app>`). The tool needs Accessibility permission (Screen Recording for
screenshots), a vision-capable model, and `swiftc` on first use. Coordinate input uses macOS's private SkyLight API
when available; the public fallback may not reach some background apps.

- Claude Code and Codex sessions of the current project appear in the palette as `…のセッションをインポート`.
- Open any saved Claude Code/Codex transcript from the palette action `Claude Code / Codex セッションをファイルから開く…`, or use `/import PATH` in the GUI. Multiple files can be selected at once; each opens as a new native thread and the source remains unchanged.
- The CLI accepts the same formats with `greencode-cli --import-session FILE`; GreenCode exports and OpenCode JSON exports are also detected automatically. Imported conversations keep text only; source tool calls, tool output, and reasoning are not replayed.


Native configuration keeps the prompt lean while exposing the high-value OMP controls:
`model_roles`, `tools`, `max_tool_calls`, `compact_at`, `compaction_enabled`, `compaction`, and `context_files` are read from `~/.greencode/config.toml`. An empty `tools` list preserves the full native catalog. `learning.auto` is on by default, but the default path is local-only: it stores an explicit `Lesson:` or `Durable lesson:` line from the final reply and adds no model request or tokens. Set `learning.model` (or `advisor_model`) only when a separate one-request-per-turn distillation pass is wanted.

```sh
greencode-cli config list
greencode-cli config get task.max_concurrency
greencode-cli config set tui.follow_up steer
greencode-cli config reset tui.follow_up
greencode-cli --config ./work.toml --config ./local.toml
```

`config set` and `config reset` edit the global TOML and preserve unrelated comments.
`--config FILE` explicitly layers TOML over the global file; the last file wins,
tables merge recursively, and arrays are replaced. `config list/get` show the TOML
settings and built-in defaults, with credentials redacted; managed policy and
runtime flags are applied separately. Only pass overlay files you trust.

To migrate settings from an OMP YAML file, generate and review a separate overlay:

```sh
greencode-cli config import-omp ~/.omp/agent/config.yml > omp-settings.toml
greencode-cli --config omp-settings.toml
```

The converter writes only to stdout and imports only these explicitly set OMP keys:
`autoResume`, `autocompleteMaxVisible`, `hideThinkingBlock`,
`display.{hideToolActivity,showTokenUsage,showTurnTime}`, `composer.recallClearedDrafts`, `tui.tight`,
`terminal.showProgress`, `contextFiles.extra`,
`compaction.{enabled,midTurnEnabled,thresholdTokens,modelThresholds,modelThresholdsEnabled}`,
`skills.enabled`, `mcp.enableProjectConfig`, `async.enabled` (native background tasks only), and
`task.{maxConcurrency,maxRecursionDepth,maxRuntimeMs,disabledAgents,structured,isolation.enabled,perCallModel,perCallModelAllow,agentModelOverrides}`.
It does not copy credentials, providers, or executable configuration. To layer
explicit YAML files, pass them in order; later values replace earlier ones.
OMP `compaction.thresholdTokens: -1` maps to GreenCode's default of 250,000.
For `modelThresholds`, the importer accepts fixed triggers such as `f90000` and
per-entry `null` resets; it rejects percentages and numeric base windows rather
than treating them as fixed counts. `model_compact_at` prefers an exact model,
then the longest matching `*` prefix, then `compact_at`. Use `0` to clear an
inherited entry or `model_compact_at_enabled = false` to ignore the entire map.

Alternatively, select an OMP profile by name:

```sh
greencode-cli config import-omp --profile work > omp-settings.toml
greencode-cli --config omp-settings.toml
```

This reads the selected profile's `config.yml` (or `config.yaml`) and adds
absolute paths for its keymap and the inherited default keymap, default first.
Use `--profile default` to import only the default profile. `PI_CODING_AGENT_DIR`
relocates the default profile. Normal startup does not read OMP profiles.

Native run flags include `--mode`, `--thinking`, `--tools read,grep`, `--no-tools`,
`--no-skills`, `--no-rules`, `--no-extensions`, `--no-context-files`, and
`--system-prompt` / `--append-system-prompt`. In TOML, `mcp_project_config = false`
ignores project `.mcp.json` while keeping global MCP settings. `[task]` controls
concurrency, recursion depth, runtime limit, disabled agents, model overrides,
structured results, isolation and background execution. `[tui]` controls
`hide_tools`, `hide_thinking`, `expand_tools`, `tight`, `show_usage` (footer),
`show_turn_usage`, `show_turn_time`, and `follow_up`.

`context_files_extra = ["TEAM.md"]` loads optional instruction files from the
native thread's working directory after its standard instructions. Use filenames
only; symlinks are not followed. `--no-context-files` disables both.

```toml
[tui]
auto_resume = true                  # latest modified native session in this directory; default false
autocomplete_max_visible = 10        # 3–20; short terminals may show fewer
show_terminal_progress = true        # OSC 9;4 for terminals that support it; default false
# expand_tools = true                # show all retained tool output from launch
# tight = true                       # remove the outer one-column margin
# theme = "/absolute/path/to/omp-theme.json"
# keybindings = "/absolute/path/to/keybindings.yml"
```

`--no-resume` bypasses `auto_resume` once. `tui.theme` imports foreground colors
and background colors for the selected item, user message, tool rows, and status
line from an OMP-style JSON palette (`colors`, optional `vars`). It supports hex
RGB, ANSI indices and variable references. It does not import symbol, syntax or
HTML export styling. Relative paths are resolved from the launch directory.

`tui.keybindings` explicitly loads OMP-format YAML or JSON keymaps. Use a single
path or `keybindings = ["base.yml", "profile.yml"]`; the last file wins for each
action. Each action accepts one chord or an array; `[]` disables it:

```yaml
app.model.select: [Alt+B, Ctrl+L]
app.plan.toggle: []
app.history.search: Ctrl+R
tui.editor.cursorWordLeft: [Alt+Left, Ctrl+Left, Alt+B]
```

Supported IDs: `app.model.{select,cycleForward,cycleBackward}`, `app.plan.toggle`,
`app.thinking.{cycle,toggle}`, `app.history.search`, `app.message.{followUp,dequeue}`,
`app.tools.{expand,toggleVisibility}`, and `app.editor.external`.
The `tui.editor` actions `cursor{Up,Down,Left,Right,WordLeft,WordRight,LineStart,LineEnd}`
and `delete{CharBackward,CharForward,WordBackward,WordForward,ToLineStart,ToLineEnd}`
are also configurable. `Ctrl+C` and `Ctrl+D` retain their GreenCode behavior;
picker arrows, editing actions outside this list, and approval bindings are
not imported. Paths are relative to the launch directory unless absolute.

Manual self-improvement uses the lazy native tool: ask the agent to call `ext` with `tool = "learn"`, or use `/learn LESSON`. Lessons are stored through the existing `memory` boundary and remain editable in `.greencode/MEMORY.md` or `~/.greencode/memory/MEMORY.md`.

Example:
```toml
# ~/.greencode/config.toml
model_roles = { task = "openai/gpt-6-luna" }
tools = []                         # empty = all native tools
max_tool_calls = 0                  # zero = unlimited per response
compaction_enabled = true
context_files = true

[learning]
enabled = true
auto = true                        # local Lesson: extraction; no extra request
# model = "provider/model"         # optional: one bounded distillation request per turn
scope = "project"
max_chars = 600
```
ACP adapters use their own login, not the native provider API keys. Reported ACP model/config selectors are used when available.
For OpenCode, choose OpenCode in the `/login` picker (macOS Terminal), or run `greencode-cli --opencode-login`; follow its prompts, then select the OpenCode backend. This signs in to OpenCode, not the native provider.
Arguments after `--opencode-login` are passed to `opencode auth login`, for example `--opencode-login openai --method oauth` (available methods depend on OpenCode).
OpenCode manages its own credentials; this does not sign in the native provider.
For example, after signing in with `codex login`, run
`cargo run -p greencode-core --bin greencode-cli -- --backend codex -p "Hello"`.
Reopening an ACP thread resumes its external session when the agent supports loading sessions; otherwise an explicit error prevents silent context loss.

## Sub-agent models (native mode)

Open ⌘K and search `サブエージェント既定モデル` to select a model for the current thread or `親と同じ`
to follow its parent. Choices come from `model`, `models`, and the configured/current sub-agent default.
The selection is saved with the conversation; changes during a run apply after that turn, without resetting its context.
The optional `subagent_model` setting above sets the default for new threads. CLI overrides also work on resume:

```sh
greencode-cli --model anthropic/claude-opus-5-5 --subagent-model openai/gpt-6-luna -p "Delegate a repository search"
greencode-cli --resume THREAD_ID --subagent-model parent -p "Continue"
```

Native `task` calls accept `tasks[]` for parallel work, with optional shared `context`. Choose `task`, read-only `scout` or `reviewer`, or custom agents from `.greencode/agents/*.md` and `.claude/agents/*.md`. Set `isolated` on a task to retain its changes in a separate git worktree, or `background` on the call to receive results later. List definitions with `greencode-cli --subagents`.
Sub-agents may start sub-agents of their own one level deep; a grandchild cannot.
A task can specify a JSON `schema` to receive a structured result submitted by the sub-agent, checked for required fields and primitive types.
The Agent Hub card shows native sub-agent progress and offers steering and cancellation, including `/to NAME TEXT` for instructions.

An individual task can override the model default with `{"tasks":[{"prompt":"Inspect tests","model":"openai/gpt-6-luna"}]}`.
Order: task override → thread default → parent model. Cross-provider choices use the selected provider's configured credentials;
missing credentials or unknown providers return a tool error, with no fallback to a different model.
Pi/OMP/Hermes and other external backends manage their own sub-agent settings; this selector does not change them.

## Extra native tools (`ext`)

One tool, `ext`, reaches OMP-style tools without adding their schemas to every request: its description is a one-line catalog, `help` returns a tool's arguments only when asked, and `call` runs it (same pattern as `mcp`). The system prompt and tool list stay within the limits in `native_prompt_stays_lean`.

- `hread` / `hedit`: read with line numbers and a 4-digit tag, then edit by line number (`replace`, `insert`, `delete`). A stale tag is refused and nothing is written. Approval, plan mode and undo treat `hedit` as `edit`.
- `ast`: structural search and single-file rewrite through [ast-grep](https://ast-grep.github.io) (`brew install ast-grep`).
- `eval`: a persistent Python or JS kernel per project (state survives between calls; `reset` clears it). Honors `sandbox`.
- `debug`: a Debug Adapter Protocol client (`launch`, `break`, `continue`, `step`, `stack`, `vars`, `eval`, `stop`). `.py` programs use `debugpy` (`python3 -m pip install debugpy`), anything else `lldb-dap`; an `adapter` argv overrides.
- `advisor`: one tool-less request to `advisor_model` with the tail of the conversation; its usage appears with the thread's. It runs only when the agent calls it.

Rules (OMP's TTSR): a markdown file in `~/.greencode/rules/` or `.greencode/rules/` with `trigger: <regex>` in its frontmatter. When the model's own reply (text or tool arguments) matches, the file body is sent as the next message, once per session. Rules cost no prompt bytes until they apply.

## Import a Hermes profile

In the GUI, open ⌘K and search `Hermes`. Choose a discovered profile or
`Hermesプロファイルをフォルダからインポート…`. Importing opens a new thread with that profile;
existing threads and GreenCode settings are left intact.

```sh
greencode-cli --hermes-profiles                    # list local profiles
greencode-cli --import-hermes default              # or a named profile, e.g. coder
greencode-cli --import-hermes ~/.hermes/profiles/coder
greencode-cli --backend hermes:coder -p "Hello"
```

This imports a persistent ACP connection to the installed Hermes, not a conversion to GreenCode's
native agent. Hermes loads its own model, credentials, SOUL, memory, skills, plugins and MCP configuration.
GreenCode does not read or copy credential files or add a second system prompt or tool catalog.
Hermes may update the original profile's memory and session state during use, just as its own UI does.
Existing Hermes chat history is not imported into GreenCode's thread list; new threads have their own sessions.

Install Hermes with ACP support first (`hermes acp --check`). Discovery respects `HERMES_HOME`
(including named-profile homes), otherwise `~/.hermes`. Only the selected name and absolute directory
are saved in `~/.greencode/hermes-profiles/<name>.json`; repeated imports are idempotent and a conflicting
name is never overwritten. Keep the source profile in place. For an exported Hermes archive,
first restore it with `hermes profile import FILE`, then import the resulting profile here.

See the official [Hermes profiles](https://hermes-agent.nousresearch.com/docs/user-guide/profiles)
and [ACP setup](https://hermes-agent.nousresearch.com/docs/user-guide/features/acp) documentation.

## Extensions

```lua
-- ~/.greencode/extensions/example.lua — reloaded when the file changes
gc.tool{
  name = "word_count", description = "Count words in a file",
  schema = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
  run = function(input) local _, n = (gc.read(input.path) or ""):gsub("%S+", ""); return n end,
}
gc.command("standup", function(args) return "Summarize today's commits. " .. args end)  -- returned text is the prompt
gc.on("tool_call", function(c)            -- return { deny = "why" } or { input = {...} }
  if c.tool == "bash" and c.input.command:find("rm %-rf") then return { deny = "no rm -rf" } end
end)
-- other events: session_start {cwd}, prompt_submit {prompt}, tool_result {tool,input,output,is_error}, context {messages}
-- helpers: gc.notify(text), gc.exec(cmd) -> output, code, gc.read(path)
```

## Token use

Native threads keep a stable system prompt and initial workspace snapshot across restarts
unless the configured or built-in system prompt changes.
On the next prompt, changed project rules, memory, or skills are appended as a superseding
workspace snapshot, preserving the existing cache prefix. Compaction refreshes the snapshot;
extension tool changes intentionally change the prefix. OpenAI's supported first-party models and
Anthropic receive explicit static-prefix cache breakpoints alongside automatic history caching. Hits still depend
on model thresholds, cache lifetime and provider behavior; they are not guaranteed on every request.

Native `/compact` uses the configured method; `/compact summary` and `/compact snap` override it.
Snap archives older history as pixel-font PNG images without a model call and preserves recent turns;
it requires an Anthropic or supported first-party OpenAI vision model. Automatic snap compaction falls
back to a summary on unsupported providers. Archives and their source are stored in the session.

For long native sessions, `old_read_previews = true` keeps the latest two or three turns intact
and sends excerpts of older successful `read` results. The original stays in the transcript, and
the excerpt points to a saved copy in `~/.greencode/spill/` that can be read even after the source
changes. Deleting a thread does not remove these spill files.
Below the automatic compaction threshold, `/compact summary` can keep the original history without
a model call if it fits the retained tail, has older reads to preview, and needs no workspace rebase.
When summary compaction runs, it still receives full results.

Skills cost one catalog line until read; MCP schemas are discovered through one `mcp` tool. Long tool output keeps
both ends and a path to the complete text. Pi/OMP retain their own prompts, history and caching without an added
GreenCode system prompt. Native usage includes cache writes, compaction and subagent calls.

See [cache measurements and reproduction](docs/cache-efficiency.md). Live model benchmarks use GPT-6 Luna; only when GPT reports a rate limit does `scripts/benchmark_tasks.py` switch the whole run to Claude Haiku 5.5 (`--model auto|luna|haiku`).
With default settings, a GPT-6 Luna coding-task run measured native at 1.02x Pi's total input with 20/20 passes
each ([details](docs/cache-efficiency.md#native-defaults-vs-pi--gpt-6-luna-2026-11-18)). For simple sessions,
`greencode-cli --tui --tools read,bash,edit,write` trims the fixed prompt further.

The Pi wrapper's deterministic read/denial probe added 0 prompt bytes and no model requests. OMP's wrapper also
added 0, but the OMP engine itself sent more prompt bytes than Pi even with only four tools enabled; this is not
a claim that every backend matches Pi's total token use. See [the parity ledger](docs/parity.md) and its raw reports.

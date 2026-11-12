# GreenCode

A small, native agent harness for macOS: Rust + GPUI, one process, no WebView.

```sh
export ANTHROPIC_API_KEY=...          # and/or OPENAI_API_KEY
cargo run --release -p greencode-app  # GUI
cargo run -p greencode-core --bin greencode-cli -- -p "prompt"   # headless
cargo test -p greencode-core
```

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
model = "anthropic/claude-opus-5-5"      # provider/model
models = ["anthropic/claude-sonnet-5-5", "openrouter/openai/gpt-5"]  # extra picker entries
mode = "ask"                              # ask | auto-edit | full
effort = "high"                           # Anthropic output_config.effort; unset = API default
compact_at = 150000                       # summarize history above this many context tokens

[providers.openrouter]                    # anthropic and openai are predefined
kind = "openai"                           # any OpenAI-compatible server
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[[backends]]                              # external agents over ACP
name = "claude-code"
command = ["npx", "-y", "@zed-industries/claude-code-acp"]
```

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

The system prompt and tool list are fixed for a session and history is append-only, so every request after the
first reads the prompt cache. Skills cost one line each until read; MCP tool schemas are never in the prompt (the
model searches them through one `mcp` tool); long tool output is cut to its head and tail with the rest saved to disk.

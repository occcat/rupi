<div align="center">

# rupi

**A Rust port of the Pi coding agent, with MCP, persistent memory, self-accumulating skills, and a cloud control plane**

<p>
  <a href="https://github.com/occcat/rupi/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/occcat/rupi/ci.yml?branch=main&style=for-the-badge&logo=github&label=CI" alt="CI" /></a>
  <img src="https://img.shields.io/badge/Rust-1.88%2B-B7410E?style=for-the-badge&logo=rust&logoColor=white" alt="Rust 1.88+" />
  <a href="https://github.com/earendil-works/pi"><img src="https://img.shields.io/badge/Upstream-Pi%200.85.x-6E40C9?style=for-the-badge" alt="Upstream Pi 0.85.x" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-3DA639?style=for-the-badge" alt="License MIT OR Apache-2.0" /></a>
</p>

<p>
  <b>English</b> ·
  <a href="README.zh-CN.md">简体中文</a>
</p>

</div>

rupi reimplements [`@earendil-works/pi-coding-agent`](https://github.com/earendil-works/pi) 0.85.x in Rust. It keeps Pi's design: a small agent loop, sessions stored as trees, and a handful of file and shell tools. The pieces Pi leaves to extensions are built in as separate crates: an MCP bridge, Hermes-style memory, and skills that the agent can distill from its own work.

One `AgentLoop` drives every front end. Locally that is the `rupi` binary: a full-screen TUI, a line REPL, a one-shot `run` mode with a JSONL event stream, and a JSONL RPC mode for embedding. In the cloud it is `rupi-server`, a stateless multi-tenant control plane that speaks [AG-UI](https://docs.ag-ui.com/introduction) over SSE and sends every shell command to an external executor.

## Contents

- [Quick Start](#quick-start)
- [Highlights](#highlights)
- [rupi vs Pi](#rupi-vs-pi)
- [Usage](#usage)
- [Configuration](#configuration)
- [Cloud control plane](#cloud-control-plane)
- [Workspace layout](#workspace-layout)
- [Documentation](#documentation)
- [Development](#development)
- [License](#license)

## Quick Start

rupi builds from source on macOS and Linux. CI runs on Ubuntu and macOS with stable Rust and the 1.88 MSRV. Windows uses PowerShell for the `bash` tool and is not covered by CI.

### 1. Install

```bash
git clone https://github.com/occcat/rupi.git
cd rupi

# Option A: build in place; binaries land in target/release/
cargo build --release -p rupi-cli

# Option B: put `rupi` on your PATH (~/.cargo/bin)
cargo install --locked --path crates/rupi-cli
```

The cloud binaries (`rupi-server`, `rupi-execd`, `rupi-sandboxd`) come from `cargo build --release -p rupi-server`. You only need them for the [cloud control plane](#cloud-control-plane).

### 2. Connect a model

Without any API key, rupi falls back to a built-in mock provider and prints a notice on stderr. This is enough to try the commands, but every reply is canned.

```bash
# OpenAI or any OpenAI-compatible gateway
export RUPI_API_KEY=sk-...                      # or OPENAI_API_KEY
export RUPI_BASE_URL=https://api.openai.com/v1  # or OPENAI_BASE_URL
rupi --model gpt-4o-mini

# Anthropic (claude-* models route here automatically)
export ANTHROPIC_API_KEY=...                    # or RUPI_ANTHROPIC_KEY
rupi --model claude-sonnet-4-5

# Gemini (gemini-* models route here automatically)
export GEMINI_API_KEY=...                       # or RUPI_GEMINI_KEY / GOOGLE_API_KEY
rupi --model gemini-2.5-flash

# A local llama.cpp server, no key required (default http://127.0.0.1:8080/v1)
rupi --model llamacpp/qwen2.5-coder
```

`rupi login [provider]` prints the environment variables a provider reads. There is no browser or device-code login.

### 3. Run your first task

```bash
cd your-project

rupi                                   # full-screen TUI (the default)
rupi chat                              # line REPL
rupi -p "summarize README.md"          # one shot, print the answer, exit
rupi run --json "list the TODOs"       # one shot, one JSON AgentEvent per line
rupi @src/main.rs "explain this file"  # attach files with @path
```

The first time rupi runs inside a project that ships its own memory, skills, or commands under `.rupi/` (or Pi and Agent Skills directories), it asks whether to trust that project. Every turn is saved to `~/.rupi/sessions.db`, so `rupi --continue` picks up where you left off.

## Highlights

| Feature | What it does |
|---|---|
| **One loop, every front end** | The TUI, the `chat` REPL, `run`, `--mode rpc`, and the cloud `POST /v1/agent` endpoint all run the same `AgentLoop`: stream the assistant message, run its tool calls, repeat until no tool is called. Esc or Ctrl-C cancels mid-stream and kills the running shell's process group. While a turn runs, Enter steers it and Alt+Enter queues a follow-up. |
| **Sessions are trees** | Every message is a node in SQLite (WAL + FTS5), tool calls and results included. `/tree`, `/goto`, `/rewind`, `/fork`, and `/clone` move around or branch the history. `--resume` restores the full tool context. `/export` writes Pi's JSONL v3 format or a standalone HTML page, and `/import` and `--session file.jsonl` load Pi sessions. |
| **Token-based compaction** | When usage passes `context_window - reserveTokens`, older history is summarized by the model and the most recent `keepRecentTokens` are kept. The summary carries a list of files read and modified. If the provider still reports a context overflow, rupi compacts and resends, at most twice. |
| **Coding tools with a sandbox** | `read`, `write`, `edit`, `bash`, `glob`, `grep`, and `think`, plus optional `find` and `ls`. `edit` requires a unique match, supports multiple edits, and returns a diff. Interactive modes confine the file tools to the launch directory and reject `..`, absolute paths, and symlink escapes. `bash` keeps the last 2000 lines or 50 KB of output. |
| **Many providers, real streaming** | OpenAI-compatible, Anthropic Messages, and Gemini `generateContent`, plus OpenRouter, Azure OpenAI, Bedrock, Vertex AI, and llama.cpp presets. All three wire formats stream over SSE and support tool calls, images, usage, and six thinking levels (`off` to `max`). 429 and 5xx responses are retried with backoff. |
| **MCP built in** | Connect MCP servers over stdio, Streamable HTTP, or WebSocket. Tools register as `{server}_{tool}`. Resources and prompts each get a per-server tool. The bridge answers server requests such as `roots/list` and `ping`, and picks up `tools/list_changed` between turns. |
| **Memory that persists** | Hermes-style `MEMORY.md`, `USER.md`, and `failures.md`, global and per project. A frozen snapshot goes into the system prompt so the prefix cache stays warm. Lines tagged `[core]` are always injected and the rest stay searchable. Writes that look like secrets are refused. |
| **Skills that accumulate** | Discovers [Agent Skills](https://agentskills.io) from rupi, Pi, and `.agents` directories with progressive disclosure: an index, then the full `SKILL.md` on demand, then resources. After each turn a review suggests memories and skill drafts. `--review-apply` writes them and `rupi skill-distill` turns a list of steps into a new skill. |
| **Extensible without recompiling** | Slash commands from Markdown files, `rupi install git:` / `npm:` / local packages (skills, prompts, extensions, themes; package scripts never run), and process extensions over a one-shot or JSON-RPC protocol that can add tools, commands, keybindings, and providers. Extensions hot-reload when their manifest changes. |
| **Cloud control plane** | `rupi-server` keeps sessions and memory in PostgreSQL, uses Redis only as a cache, and serves AG-UI over SSE plus an `/admin` console. User commands run only on `rupi-execd` or `rupi-sandboxd` nodes, never in the API process. |

## rupi vs Pi

Pi keeps its core small and leaves most features to extensions. rupi follows the same split at the crate level and builds the common extensions in. [`docs/UPSTREAM.md`](docs/UPSTREAM.md) maps every subsystem to its upstream counterpart and lists the deliberate differences.

| Capability | Pi | rupi |
|---|:---:|:---:|
| Agent loop, session tree, compaction | ✓ | ✓ |
| Default tools, TUI, print / JSON / RPC modes | ✓ | ✓ |
| `settings.json`, `AGENTS.md`, `SYSTEM.md`, custom prompts | ✓ | ✓ (also reads `~/.pi/agent` and `.pi` read-only) |
| Session storage | JSONL files | SQLite; JSONL for import and export |
| MCP bridge | extension | built in (`rupi-mcp`) |
| Persistent memory | extension | built in (`rupi-memory`) |
| Skill self-accumulation | extension | built in (`rupi-skills` + review) |
| Path sandbox for file tools | — | ✓ |
| Permission gates and plan mode | extension | built in (`--plan`, approval prompts) |
| In-process TypeScript extensions | ✓ | — (process extensions over JSON-RPC instead) |
| Subscription OAuth login | some providers | — (API keys only) |
| Multi-tenant cloud control plane | — | `rupi-server` |

## Usage

### Interfaces

| Command | Use it for |
|---|---|
| `rupi` / `rupi tui` | Full-screen ratatui interface: Markdown with syntax highlighting, collapsible tool and thinking blocks, Mermaid diagrams drawn in Unicode, inline images on Kitty-protocol terminals, a session picker, a tree navigator, and a built-in file editor (`/edit`, Ctrl+E). |
| `rupi chat` | Plain line REPL. Works over SSH and in pipes. |
| `rupi run "..."` / `rupi -p "..."` | Run one prompt and exit. Piped stdin is appended to the prompt. Approval prompts are denied unless you pass `--approve`. |
| `rupi run --json "..."` | Same, but stdout is one `AgentEvent` per line and ends with a `run_result` line (session id, stop reason, final text). Matches `pi --mode json`. |
| `rupi --mode rpc` | JSONL commands on stdin (`prompt`, `steer`, `follow_up`, `abort`, `get_state`, `set_model`, `fork`, ...) and responses plus events on stdout, for embedding rupi in another program. |

### Sessions

```bash
rupi sessions                     # recent sessions
rupi session-show <id>            # one session in detail
rupi session-search "keyword"     # full-text search across sessions
rupi --continue                   # latest session, preferring the current directory
rupi --resume <id>                # a specific session
rupi -r                           # pick a session interactively
rupi --fork <id|file.jsonl>       # continue a copy under a new id
rupi --export <id> out.html       # export and exit (.jsonl for Pi's format)
rupi --no-session                 # do not persist anything
```

### Slash commands

These work in the TUI. Most also work in the `chat` REPL.

| Command | Action |
|---|---|
| `/tree`, `/goto <id>`, `/rewind [id]` | Show the session tree, jump to a node, or rewind |
| `/fork`, `/clone`, `/new`, `/name <name>` | Branch into a new session, copy the whole tree, start fresh, rename |
| `/sessions`, `/resume <id>` | Pick or switch sessions |
| `/export [file]`, `/import <file>`, `/share` | JSONL or HTML export, Pi JSONL import, share as a secret gist |
| `/compact [instructions]` | Compact now, optionally with custom summary instructions |
| `/model [name]`, `/thinking [level]` | Switch model or thinking level for the rest of the session |
| `/plan` | Toggle plan mode (read-only: `write`, `edit`, and `bash` are blocked) |
| `/settings`, `/reload` | Edit settings in place; reload extensions, skills, prompts, and themes |
| `/skills`, `/commands`, `/<skill-name> args` | List skills and custom commands; run a skill as a command |
| `/edit [path]`, `/copy`, `/hotkeys`, `/trust`, `/help`, `/quit` | TUI only: edit a file, copy the last reply, show keys, manage project trust |

Custom commands are Markdown files in `~/.rupi/commands/` or `.rupi/commands/` (Pi's `prompts/` directories also work). `/name args` expands the file body, replacing `$ARGUMENTS`, `$1`, or `{{var}}`.

### Tools and permissions

`--tools read,bash`, `--exclude-tools`, `--no-tools`, and `--no-builtin-tools` choose what the model sees. `--plan` starts in read-only plan mode. `bash` commands that match the blocklist (`rm -rf /`, `mkfs`, `dd if=`) stop for approval. Answering `a` approves that kind of call for the rest of the session. `--approve` and `--no-approve` decide all prompts up front for scripts. `--parallel-tools` runs independent tool calls concurrently and keeps results in order. `--subagents` lets the model delegate subtasks to forked child sessions.

### Providers and models

Pass `--model name` or `--model provider/model[:thinking]`, for example `anthropic/claude-sonnet-4-5:high`. Without a prefix, `claude-*` goes to Anthropic, `gemini-*` goes to Gemini, and everything else uses the OpenAI-compatible client.

| Provider | Key variables | Base URL override |
|---|---|---|
| `openai` | `RUPI_API_KEY`, `OPENAI_API_KEY` | `RUPI_BASE_URL`, `OPENAI_BASE_URL` |
| `anthropic` | `RUPI_ANTHROPIC_KEY`, `ANTHROPIC_API_KEY` | `RUPI_ANTHROPIC_BASE` |
| `gemini` | `RUPI_GEMINI_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY` | `RUPI_GEMINI_BASE` |
| `openrouter` | `RUPI_OPENROUTER_KEY`, `OPENROUTER_API_KEY` | `RUPI_OPENROUTER_BASE` |
| `azure` | `AZURE_OPENAI_API_KEY`, `RUPI_AZURE_KEY` | `AZURE_OPENAI_ENDPOINT` (required) |
| `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`, `BEDROCK_API_KEY` | `RUPI_BEDROCK_BASE`, or `AWS_REGION` |
| `vertex` | `VERTEX_TOKEN`, `GOOGLE_OAUTH_ACCESS_TOKEN` | `GOOGLE_CLOUD_PROJECT`, `VERTEX_LOCATION` |
| `llamacpp` | optional `RUPI_LLAMACPP_KEY` | `RUPI_LLAMACPP_BASE` |

`--api-key` and `--provider` override the variables above. `rupi models` lists the built-in catalog, which `~/.rupi/models.json` or `RUPI_MODELS` can extend.

### MCP servers

```bash
# Probe a server: lists its tools, resources and prompts
rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
rupi mcp-list --url http://127.0.0.1:8000/mcp

# Use servers in a session
cat > mcp.json <<'EOF'
[
  { "name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."], "env": {} },
  { "name": "remote", "command": "", "args": [], "env": {}, "url": "https://example.com/mcp" }
]
EOF
rupi --mcp-config mcp.json
```

A `url` starting with `ws://` or `wss://` uses WebSocket. Any other `url` uses Streamable HTTP.

### Memory

```bash
rupi memory-write add "prefers small focused commits"
rupi memory-write add "[core] answer in English" --scope global
rupi memory-write add "uses pnpm, not npm" --scope project
rupi memory-show
rupi memory-search "pnpm"
```

Files live in `~/.rupi/memories/` (global) and `<repo>/.rupi/MEMORY.md` (project), up to 5000 characters each. When a file overflows, the oldest lines are dropped. During a session the model writes through the `memory` tool and recalls with `memory_search` and `session_search`. `--no-memory` turns the built-in memory off for one run, and `--memory-provider jsonl` adds an external provider.

### Skills, packages, and extensions

```bash
rupi skills-list
rupi skill-load <name>
rupi skill-distill release-notes "Draft release notes from git log" "collect commits since last tag" "group by type"

rupi install git:github.com/user/repo        # also npm:@scope/pkg@1.2.3, https/ssh URLs, local paths
rupi install ./examples/packages/demo
rupi packages
rupi uninstall git:github.com/user/repo
rupi config list                             # enable or disable packages, extensions, skills, prompts, themes

rupi --ext-dir examples/extensions ext-list
```

Skills are discovered in this order, first match wins: bundled `skills/builtin`, `~/.rupi/skills`, `.rupi/skills`, `~/.pi/agent/skills`, `~/.agents/skills`, then `.pi/skills` and `.agents/skills` walking up from the working directory to the repository root.

An extension is a JSON manifest plus a command. With `"protocol": "oneshot"` (the default) the arguments go to stdin and stdout is the result. With `"protocol": "jsonrpc"` the process stays alive and can register commands, keybindings, providers, and event subscriptions. See [`examples/extensions/`](examples/extensions/).

## Configuration

Settings merge in this order, later wins: `~/.rupi/settings.json`, then `.rupi/settings.json` from the working directory upward. Pi's `~/.pi/agent/settings.json` and `.pi/settings.json` are read too, and rupi's own files win on conflicts.

```json
{
  "model": "anthropic/claude-sonnet-4-5",
  "thinking": "medium",
  "theme": "dark",
  "tools": ["read", "write", "edit", "bash", "grep", "find", "ls"],
  "compaction": { "reserveTokens": 16384, "keepRecentTokens": 20000 },
  "steeringMode": "one-at-a-time",
  "defaultProjectTrust": "ask",
  "enabledModels": ["anthropic/claude-sonnet-4-5", "openai/gpt-4o"]
}
```

Other files under `~/.rupi` (or `$RUPI_HOME`):

| Path | Purpose |
|---|---|
| `sessions.db` | All sessions (`--session-dir` or `RUPI_SESSION_DIR` moves it) |
| `memories/` | `MEMORY.md`, `USER.md`, `failures.md` |
| `skills/`, `commands/`, `prompts/`, `extensions/`, `themes/` | User resources; `rupi install` writes here |
| `SYSTEM.md`, `APPEND_SYSTEM.md` | Replace or extend the system prompt (also `--system-prompt`, `--append-system-prompt`) |
| `keybindings.json` | TUI key overrides |
| `models.json`, `providers.json` | Extra models and providers |

Projects add `AGENTS.md` or `CLAUDE.md` files (loaded from the global directory and every ancestor of the working directory; `--no-context-files` skips them) and a `.rupi/` directory with the same layout. Set `RUPI_HOME` to keep a separate home, which is handy for demos and tests.

## Cloud control plane

The cloud deployment has four parts:

- PostgreSQL, the source of truth for sessions, memory, quotas, and audit records
- Redis, used only as a cache; the server keeps serving from PostgreSQL if Redis is down
- one or more executor nodes, `rupi-execd` or the isolating `rupi-sandboxd`
- `rupi-server`, stateless, so you can run several replicas

A minimal loopback setup:

```bash
cargo build --release -p rupi-server
export RUPI_EXEC_TOKEN=change-me RUPI_ADMIN_TOKEN=change-me-too
export DATABASE_URL=postgres://rupi:rupi@127.0.0.1:5432/rupi

./target/release/rupi-server --database-url "$DATABASE_URL" --bootstrap-tenant demo   # prints a tenant key, then exits
./target/release/rupi-execd --listen 127.0.0.1:8090 --root ./rupi-data/execd --token "$RUPI_EXEC_TOKEN"
./target/release/rupi-server --listen 127.0.0.1:8080 \
  --database-url "$DATABASE_URL" --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 --executor-token "$RUPI_EXEC_TOKEN" \
  --admin-token "$RUPI_ADMIN_TOKEN"

# Talk to it with the thin client
rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 prompt "hello"
```

Conversations use AG-UI: `POST /v1/agent` with a `RunAgentInput` body returns an SSE stream. The admin console at `/admin` manages tenants, quotas, settings, sessions, and the executor pool. Defaults are strict: a non-loopback listener needs TLS (`--tls-cert`/`--tls-key`) or an explicit `--insecure`, and an executor with an empty token refuses to start. [`docs/LAUNCH.md`](docs/LAUNCH.md) lists every flag and the production checklist.

## Workspace layout

| Crate | Role |
|---|---|
| [`rupi-core`](crates/rupi-core/README.md) | Messages, session tree, events, tool and extension traits. No LLM and no I/O. |
| [`rupi-llm`](crates/rupi-llm/README.md) | `LlmProvider` trait, the three wire formats, routing, SSE, model catalog |
| [`rupi-tools`](crates/rupi-tools/README.md) | Built-in tools and the path sandbox |
| [`rupi-agent`](crates/rupi-agent/README.md) | `AgentLoop`, compaction, permissions, subagents, review |
| [`rupi-mcp`](crates/rupi-mcp/README.md) | MCP bridge over stdio, HTTP, and WebSocket |
| [`rupi-memory`](crates/rupi-memory/README.md) | Memory files and the SQLite session store |
| [`rupi-skills`](crates/rupi-skills/README.md) | Skill discovery and distillation |
| [`rupi-ext`](crates/rupi-ext/README.md) | Process extensions (one-shot and JSON-RPC) |
| [`rupi-config`](crates/rupi-config/README.md) | `settings.json` merging and resource filters |
| [`rupi-pkg`](crates/rupi-pkg/README.md) | `rupi install` / `uninstall` / `packages` |
| [`rupi-tui`](crates/rupi-tui/README.md) | ratatui interface |
| [`rupi-cli`](crates/rupi-cli/README.md) | The `rupi` binary and the `rupi cloud` thin client |
| [`rupi-runtime`](crates/rupi-runtime/README.md) | Executor trait, `rupi-execd`, `rupi-sandboxd` |
| [`rupi-server`](crates/rupi-server/README.md) | Cloud control plane and `/admin` |
| [`rupi-benches`](crates/rupi-benches/README.md) | Criterion benchmarks (not published) |

## Documentation

| Document | Contents |
|---|---|
| [`docs/UPSTREAM.md`](docs/UPSTREAM.md) | Subsystem-by-subsystem mapping to Pi, deliberate differences, what is not ported |
| [`docs/LAUNCH.md`](docs/LAUNCH.md) | Cloud deployment: every real flag, safe defaults, loopback and public setups, load drills |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Product goal, current state, branches in progress, and what to do next |
| [`docs/backlog/`](docs/backlog/README.md) | Every known issue (258 review findings) and 42 ranked ideas borrowed from upstream Pi v1.4.0 |
| [`AGENTS.md`](AGENTS.md) | Project principles and conventions for contributors and coding agents |
| [`BRANCH_COMPARISON.md`](BRANCH_COMPARISON.md) | Historical review of the three original port branches (2026-09) |
| [`crates/*/README.md`](crates/) | Per-crate responsibilities and commands |
| [`examples/`](examples/) | Sample extensions and an installable package |

Most project documents other than this README are written in Chinese.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --exclude rupi-benches
cargo test --workspace --locked --exclude rupi-benches
cargo bench -p rupi-benches                       # benchmarks
./scripts/bench-startup.sh                        # cold start, needs hyperfine and a release build
```

The `rupi-server` integration tests need PostgreSQL and Redis through `DATABASE_URL` and `REDIS_URL` and skip themselves when those are absent. MCP and extension tests need `python3`. Read [`AGENTS.md`](AGENTS.md) before sending changes.

## License

Dual-licensed under [MIT or Apache-2.0](Cargo.toml), at your option.

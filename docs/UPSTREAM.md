# Upstream alignment

Target: **@earendil-works/pi-coding-agent 0.85.1**  
Source: https://github.com/earendil-works/pi  
Memory/skills extras: [Hermes Agent memory](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory) and [skills](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/skills.md).

## Agent loop (`packages/agent/src/agent-loop.ts`)

Implemented in `crates/rupi-agent-core/src/loop_.rs`:

- `agent_loop` / `agent_loop_continue`
- Events: `agent_start`, `turn_start`, `message_*`, `tool_execution_*`, `turn_end`, `agent_end`
- Steering after a tool batch; follow-up after the agent would otherwise stop
- `shouldStopAfterTurn` exits before polling queues
- `beforeToolCall` `{ block, reason, terminate }`; batch terminate only when **every** result sets terminate
- Parallel vs sequential tool execution (result artifacts stay in assistant source order)
- StreamFn/provider failures become assistant `stopReason=error|aborted` rather than thrown errors

Not ported (TypeScript/TUI-specific): `AssistantMessageStream` transport decoupling, declaration-merging custom messages, OAuth providers, pi-tui differential renderer, TypeScript extension loader (`jiti`).

## Compaction (`packages/agent/src/harness/compaction/compaction.ts`)

- `reserveTokens = 16384`
- `keepRecentTokens = 20000`
- `shouldCompact`: `contextTokens > contextWindow - reserveTokens`
- Token estimate: `ceil(chars / 4)` (pi-ai)

LLM-authored summaries can be plugged in later; the default path is extractive `serialize_conversation`.

## Skills (`packages/agent/src/harness/system-prompt.ts`, `skills.ts`)

- `<available_skills><skill><name/><description/><location/></skill></available_skills>`
- XML escape: `& < > " '`
- `SKILL.md` + root `.md` with description frontmatter
- Name rules (lowercase, digits, hyphens) emit diagnostics, matching Pi's warn-not-reject stance
- Invocation wrapper: `<skill name location>…</skill>`

## MCP (`pi-mcp-extension`)

- Protocol `2025-03-26` with `2024-11-05` accepted from the server
- Transports: stdio (newline JSON-RPC), streamable HTTP, SSE `data:` fallback
- Paginated `tools/list` (cursor, max 100 pages)
- Tool name prefix `mcp_<server>_<tool>`
- Annotations appended to descriptions (`readOnly`, `destructive`, …)
- Layered config: global then project, project wins per server name
- Harness bootstrap reads `~/.rupi/mcp.json` and `.rupi/.pi/mcp.json`, connects non-lazy servers, and registers prefixed tools
- JSON-RPC notifications omit `id` (spec); stdio flushes after notify
- Verified: Python stdio child e2e + local HTTP/SSE e2e + harness `mcp.json` wiring

Resources/prompts/sampling are intentionally out of v1 (same as the extension's v2 backlog).

## Hermes memory

Three durable layers plus an evidence index (Hermes session / user / project):

| Layer | File | Limit | Location |
|---|---|---|---|
| Agent notes (`memory`) | `MEMORY.md` | 2200 chars | `~/.rupi/memories/` |
| User profile (`user`) | `USER.md` | 1375 chars | `~/.rupi/memories/` |
| Project (`project`) | `PROJECT.md` | 2200 chars | `{cwd}/.rupi/memories/` |
| Session evidence | SQLite FTS5 | unbounded | `~/.rupi/state.db` via `session_search` |

- Frozen snapshot at session start (MEMORY core + USER + PROJECT); extended MEMORY stays retrievable via `memory` search
- Actions: `add` / `replace` / `remove` / `search` (substring; unique match required); `target=memory|user|project`
- `[core]` prefix: always injected; without any core tags, all MEMORY entries inject (backward compatible)
- Hard limit: error on overflow, never silent drop
- Duplicate exact entries: success + `no duplicate added`
- Injection/invisible-Unicode scanner
- `session_search` actions: `search` (FTS5 phrase) and `scroll` (session_id + after_ts)

## Permissions, sub-agent, REPL

- `PermissionGate` is wired as `beforeToolCall`: sandboxed cwd for path tools; destructive bash (`rm -rf`, `mkfs`, …) is `Ask` and denied unless `RUPI_ALLOW_DESTRUCTIVE=1`
- `subagent` tool: nested `run_subagent` with read-only `read/grep/find/ls`, isolated summary
- REPL (Pi-shaped commands): `/help /session /tree /compact /memory /mcp /search <q> /skill [name] /skill:name /quit`

## Skill self-accumulation

Hermes reviewer pattern:

1. User-facing turn completes first
2. A sandboxed pass (tool whitelist: `memory`, `skill_manage`) extracts durable facts / workflows
3. Optional `write_approval` stages writes instead of committing

The default reviewer is heuristic (explicit “remember that”, preference lines, workflow/runbook markers) so tests do not need a second LLM. A live provider can be pointed at the same tools.

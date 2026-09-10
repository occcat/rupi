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

Resources/prompts/sampling are intentionally out of v1 (same as the extension's v2 backlog).

## Hermes memory

- Files: `MEMORY.md` (2200 chars), `USER.md` (1375 chars), `§` delimiters
- Frozen snapshot at session start
- Actions: `add` / `replace` / `remove` / `search` (substring; unique match required)
- `[core]` prefix: always injected; without any core tags, all entries inject (backward compatible)
- Hard limit: error on overflow, never silent drop
- Duplicate exact entries: success + `no duplicate added`
- Injection/invisible-Unicode scanner
- Evidence layer: SQLite FTS5 (`session_search`)

## Skill self-accumulation

Hermes reviewer pattern:

1. User-facing turn completes first
2. A sandboxed pass (tool whitelist: `memory`, `skill_manage`) extracts durable facts / workflows
3. Optional `write_approval` stages writes instead of committing

The default reviewer is heuristic (explicit “remember that”, preference lines, workflow/runbook markers) so tests do not need a second LLM. A live provider can be pointed at the same tools.

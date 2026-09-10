# rupi

Rust 复刻的 [Pi coding agent](https://github.com/earendil-works/pi)（上游 `@earendil-works/pi-coding-agent` **0.85.1**），并补齐 Pi 刻意留在扩展层、但现代 agent harness 常见的能力：

- **MCP 2025-03-26**（stdio / streamable HTTP / 分页 `tools/list` / `tools/call`）
- **Hermes 风格记忆**（`MEMORY.md` / `USER.md` / `PROJECT.md` 三层冻结快照、`[core]` 分层、FTS5 `session_search`）
- **Skill 自积累**（AgentSkills 渐进式披露 + `skill_manage` + 回合结束后的 reviewer）

哲学对齐 Pi：核心循环保持最小（默认四个工具 `read` / `write` / `edit` / `bash`），把 MCP、记忆、skill 写成可组合的 harness 层，而不是把上下文窗口塞满协议噪声。

## 对照上游

| Pi 0.85.1 | rupi |
|---|---|
| `@earendil-works/pi-ai` | `rupi-ai`：OpenAI / Anthropic / faux provider、模型目录、usage |
| `@earendil-works/pi-agent-core` agent loop | `rupi-agent-core`：`agent_start/turn_*/tool_execution_*`、steer / follow-up、并行工具、`beforeToolCall` |
| AgentHarness compaction | `reserveTokens=16384`、`keepRecentTokens=20000`、cut-point + 摘要替换 |
| JSONL session tree + durable leaf | `SessionStore`（`session` / `message` / `leaf` / `compaction`） |
| Skills XML in system prompt | 与 `system-prompt.ts` 相同的 `<available_skills>` 结构 |
| 默认工具 read/bash/edit/write | 同名实现；另有 grep/find/ls |
| MCP 作为 extension | 一等公民：`mcp_<server>_<tool>` 前缀桥接 |
| 无内置记忆 | Hermes：session/user/project 三层 + `memory` 工具 + SQLite FTS5 `session_search` |
| Skills 需人工编写 | `skill_manage` + 回合后自积累 |

详细对照见 [`docs/UPSTREAM.md`](docs/UPSTREAM.md)。

## 快速开始

需要 Rust 1.88+（`rust-toolchain.toml` 钉在 `stable`）。

```bash
cargo build --release -p rupi-coding-agent
./target/release/rupi --help
./target/release/rupi version
```

Print 模式（对应 `pi -p`），可用真实 API 或确定性 faux provider：

```bash
export OPENAI_API_KEY=sk-...
rupi -p "列出当前目录并解释 README" --model openai/gpt-4o

# 无网络的确定性演示
rupi --faux --faux-text "hello from rupi" -p "hi"
```

REPL：

```bash
rupi --model anthropic/claude-sonnet-4-5
# /help /session /tree /compact /memory /mcp /search <q> /skill [name] /skill:name /quit
```

## 配置

主目录默认 `~/.rupi`（可用 `RUPI_HOME` 覆盖）：

```
~/.rupi/
  agent/settings.json
  mcp.json
  skills/<name>/SKILL.md
  memories/MEMORY.md
  memories/USER.md
  sessions/<id>.jsonl
  state.db          # FTS5 证据层

{cwd}/.rupi/
  memories/PROJECT.md
  mcp.json          # 项目覆盖全局
  skills/
```

项目级覆盖：`.rupi/` 或 Pi 兼容的 `.pi/`（`SYSTEM.md`、`APPEND_SYSTEM.md`、`skills/`、`mcp.json`）。向上查找 `AGENTS.md`。

`mcp.json` 示例（项目覆盖全局，按 server 名合并，对齐 pi-mcp-extension）：

```json
{
  "mcpServers": {
    "time": { "command": "uvx", "args": ["mcp-server-time"] },
    "docs": { "url": "http://127.0.0.1:3000/mcp" }
  }
}
```

## Harness 行为

1. **系统提示**：身份 + 工具说明 + skills 索引（仅 name/description/location）+ 冻结记忆块 + AGENTS.md。
2. **Agent loop**：流式助手消息 → 校验/权限门 → 并行或顺序执行工具 → steer 注入 → 无 tool call 时排空 follow-up。
3. **记忆**：会话开始时把 USER / MEMORY(core) / PROJECT 打进 prompt 后冻结（保 prefix cache）。当轮 `memory` 写入立刻落盘（`target=memory|user|project`），但要到下一会话才进入 prompt。`[core]` 条目始终注入，其余走 `search`；无界历史走 `session_search`。
4. **Skill 自积累**：用户可见回复先返回；随后 reviewer（默认可仅用 memory / skill_manage）从 transcript 提取 “remember that …” 与非平凡 workflow，写入 `MEMORY.md` 或 `skills/`。
5. **窄缩**：`contextTokens > contextWindow - 16384` 时，保留最近 ~20k tokens，前缀换成 `<compaction_summary>`。
6. **权限**：cwd sandbox（默认拒绝越出工作区的 read/write/edit）；破坏性 bash 走 Ask，需 `RUPI_ALLOW_DESTRUCTIVE=1` 才放行。
7. **子 agent**：`subagent` 工具用只读 ToolSet 跑嵌套 loop，isolated 模式只把摘要交回父 agent。
8. **MCP**：启动时连接 `mcp.json` 里的 stdio/HTTP server，工具名 `mcp_<server>_<tool>`。

## 开发

```bash
cargo test --workspace
cargo run -p rupi-coding-agent -- --faux -p "ping"
```

本仓库是从零实现的 Rust 复刻，不是对 TypeScript 源码的转译。循环语义、事件名、compaction 默认值、skill XML、MCP 协议版本以 Pi 0.85.1 / Hermes 文档为准。

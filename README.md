# rupi — Pi Agent 的 Rust 复刻

最小 Harness + MCP + 记忆（Hermes 风格）+ Skill 自积累。TypeScript 版 Pi 的设计哲学是
**core 极小、一切走扩展组合**；本仓库用 Rust workspace 逐层复刻该思想。

## 对标关系

| Pi / 生态 | rupi |
|---|---|
| `pi-agent-core`（agent loop / state / events，~300 行核心） | `rupi-core`（`Message`/`SessionTree`/`AgentEvent`/`ToolDefinition`/`Extension`） + `rupi-agent`（`AgentLoop`/`PromptBuilder`） |
| `pi-ai`（统一 LLM API，多 provider） | `rupi-llm`（`LlmProvider` trait + `OpenAiCompatProvider` + `MockProvider`，`complete_streaming` 真 SSE） |
| 默认四工具 Read/Write/Edit/Bash | `rupi-tools`（`ToolRegistry::with_builtins`） |
| sessions are trees（branch/rewind/summary） | `SessionTree::branch_from` / `rewind_to` / `prompt_history` 压缩窗口 + `AgentLoop::maybe_compress` |
| 无内置 MCP（立场非缺失），MCP-Direct 扩展：spawn → initialize → tools/list → registerTool，`sanitizeParams`，30s 超时，`promptSnippet` 必填 | `rupi-mcp`（`McpBridge` stdio JSON-RPC + `sanitize_params` + `mcp_tool_to_definition`） |
| Skills（Agent Skills 开放标准，渐进披露） | `rupi-skills`（`SkillRegistry` 三阶段 + `load_skill` 工具） |
| Hermes 记忆：MEMORY.md/USER.md 冻结快照 + `MemoryProvider` 七方法 + `MemoryManager`（单外部）+ SQLite FTS5 session_search + background_review | `rupi-memory`（`MemoryStore::frozen_snapshot` + `MemoryProvider` trait + `MemoryManager` + `SessionStore`） |
| Skill 自积累（后台 review 沉淀） | `SkillAccumulator::propose` + `rupi skill-distill` |
| coding-agent CLI + TUI | `rupi-cli`（`rupi` 二进制；TUI 当前为 REPL，后续接 ratatui） |

## 快速开始

```bash
cargo build --workspace
cargo test --workspace

# 无 Key 演示模式（MockProvider）
./target/debug/rupi --help
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-write add "likes tea"
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-show
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skills-list
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skill-load --name commit-helper
echo "/quit" | RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi chat

# 接真实模型（OpenAI-compatible）
export RUPI_API_KEY=... RUPI_BASE_URL=https://api.openai.com/v1
./target/debug/rupi --model gpt-4o-mini chat
```

## MCP 探活

```bash
./target/debug/rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
echo '[{"name":"fake","command":"python3","args":["crates/rupi-mcp/tests/fake_mcp_server.py"],"env":{}}]' > /tmp/mcp.json
echo "/quit" | ./target/debug/rupi --mcp-config /tmp/mcp.json chat
# → [mcp] 2 tools: fake_echo, fake_fail
```

语义与 `pi-directx` 一致：stdio 上换行分隔 JSON-RPC 2.0，
`tools/list`（cursor 分页）→ 每个工具 `registerTool` 为 `{prefix}_{name}`，
`tools/call` 前按 `inputSchema` 把 string 纠正回 boolean/number/integer，
`prompt_snippet` 必填否则 agent 看不见工具。

## 记忆语义（Hermes 对齐）

- `~/.rupi/memories/MEMORY.md` / `USER.md`，`memory_char_limit=2200` / `user_char_limit=1375`。
- 启动时冻结快照注入系统提示（保 prefix cache）；会话内 `memory` 工具写盘即时生效，
  但快照不变，下个 session 才可见；`tool` 回包永远显示实时状态。
- `MemoryManager` 只允许一个外部 provider，第二个拒绝并 warning；
  `prefetch` 超时/失败只记 debug，`sync` 失败记 warning，主循环不崩。
- `SessionStore`（SQLite + FTS5）提供 `session-search` 跨会话回忆。

## Skills 语义

- 目录 + `SKILL.md`（frontmatter 必含 `name`/`description`），约束与官方规范一致
  （name 小写-数字-连字符 ≤64，description ≤1024，body 建议 <5000 tokens / <500 行）。
- 渐进披露：`skills-list`（metadata 索引）→ `skill-load`（全文）→ `read_resource` 按需读
  `references/`/`assets/`；agent 内以 `load_skill` 工具激活。
- 自积累：`skill-distill --name x --description d --steps s1 s2` 生成新 `SKILL.md` 草稿到
  `~/.rupi/skills/<name>/`，落盘前需校验（已实现 name 校验 + 重名拒绝）。
- 后台 review：`chat --review` 每轮后安静复盘并打印记忆/Skill 建议，
  `--review-apply` 直接落盘（`MEMORY.md` add + skill 草稿；已存在 skill 跳过不覆盖）。

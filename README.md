# rupi

Rust 复刻的 [Pi Agent](https://github.com/earendil-works/pi) harness（对照 `@earendil-works/pi-coding-agent` **0.85.1**），并内置常见 agent harness 能力：

- **MCP**：stdio + streamable HTTP，`mcp.json` 配置，代理工具 `mcp` 与原生 `mcp_<server>_<tool>`
- **记忆**（参考 [Hermes](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory)）：`MEMORY.md` / `USER.md`、字符上限、§ 分隔、冻结快照、`memory` 工具、FTS5 `session_search`
- **Skill 自积累**（agentskills.io 进小披露 + Hermes 审查循环）：`skill_view` / `skills_list` / `skill_manage`，会话后后台审查，read-before-write 守卫

二进制名：`rupi`。

## 快速开始

```bash
cargo build --release -p rupi-cli
./target/release/rupi --help

# 单轮 print 模式
export ANTHROPIC_API_KEY=...
rupi -p "explain this repo"

# 指定提供商 / 模型
rupi --provider openai --model gpt-4.1
rupi --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.1
```

无 API key 时会落到 `faux` 提供商（用于测试）。

## 目录布局

对齐 Pi 的 `~/.pi/agent/`，rupi 使用 `~/.rupi/`（可用 `RUPI_HOME` 覆盖）：

```
~/.rupi/
  settings.json
  mcp.json
  skills/           # 全局 skills（含自积累产物）
  memories/MEMORY.md
  memories/USER.md
  sessions/*.jsonl  # Pi 风格 session v3
  state.db          # FTS5 会话回召
```

项目级：`.rupi/skills/`、`.rupi/mcp.json`，以及 Pi 同款 `AGENTS.md` / `CLAUDE.md` 发现。

## Crate 结构

| Crate | 对应 Pi / Hermes |
| --- | --- |
| `rupi-ai` | `@earendil-works/pi-ai`：OpenAI / Anthropic / Google / OpenAI-compat / faux |
| `rupi-agent` | `@earendil-works/pi-agent-core` + coding-agent 工具：agent loop、read/write/edit/bash/grep/find/ls、JSONL session、compaction |
| `rupi-mcp` | Pi 生态 `pi-mcp-adapter`：MCP client |
| `rupi-memory` | Hermes MEMORY.md / USER.md / session FTS |
| `rupi-skills` | Pi skills + Hermes `skill_view` / `skill_manage` / background review |
| `rupi-cli` | `@earendil-works/pi-coding-agent` CLI：print / REPL / JSON / RPC |

## Agent loop

与 Pi 的 `runLoop` 同构：

1. 注入 steering / follow-up
2. 按需 compaction（保留尾部 turn boundary）
3. 流式调用模型
4. 并行或串行执行 tool calls
5. `stopReason == length` 时拒绝执行可能被截断的工具参数
6. 无工具调用且 inbox 为空时结束

## MCP

`~/.rupi/mcp.json`（Claude Code 同款）：

```json
{
  "mcpServers": {
    "echo": {
      "command": "python3",
      "args": ["path/to/server.py"]
    }
  }
}
```

远程：`{"url": "https://example/mcp", "headers": {}}`。

## 记忆

- `memory` 目标：`memory`（2200 字符）/ `user`（1375 字符）
- 动作：`add` / `replace` / `remove`（`old_text` 唯一子串匹配）
- 超限不自动压缩，返回当前条目让模型自己整理
- 注入格式含用量百分比与 `§` 分隔；**会话开始冻结快照**（盘上立即更新，下一会话才进 system prompt）
- 基础注入/凭据扫描

## Skills

启动时只把 name + description 放进 `<available_skills>`。模型用 `skill_view`（或 `read`）加载全文。

`skill_manage`：`create` / `edit` / `patch` / `delete` / `write_file` / `remove_file`。

后台审查（默认开启）：turn 结束后用同一模型 fork 一轮，白名单仅为 memory + skill 工具。后台 patch 必须先 `skill_view`（Hermes read-before-write）。

## CLI 摘要

```
rupi -p "..."                 print
rupi --mode json -p "..."     JSONL 事件
rupi --continue               继续最近 session
rupi --provider faux -p hi    本地假模型
```

交互 slash：`/help` `/model` `/mcp` `/skills` `/memory` `/session` `/compact` `/quit`

## 测试

```bash
cargo test --workspace
```

## License

MIT

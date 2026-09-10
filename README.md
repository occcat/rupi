# rupi — Pi Agent 的 Rust 复刻

最小 Harness + MCP + 记忆（Hermes 风格）+ Skill 自积累。TypeScript 版 Pi 的设计哲学是
**core 极小、一切走扩展组合**；本仓库用 Rust workspace 逐层复刻该思想。

## 对标关系

| Pi / 生态 | rupi |
|---|---|
| `pi-agent-core`（agent loop / state / events，~300 行核心） | `rupi-core`（`Message`/`SessionTree`/`AgentEvent`/`ToolDefinition`/`Extension`） + `rupi-agent`（`AgentLoop`/`PromptBuilder`） |
| `pi-ai`（统一 LLM API，多 provider） | `rupi-llm`（`LlmProvider` trait + `OpenAiCompatProvider` + `MockProvider`，`complete_streaming` 真 SSE；`--model` 启动指定，REPL `/model [名]` 会话内切换；回包兼容数组 content 与对象式 arguments） |
| 默认四工具 Read/Write/Edit/Bash | `rupi-tools`（`ToolRegistry::with_builtins`；`read` 分页 offset/limit + 大文件截断标注，`bash` 支持 `timeout_secs` + 输出首尾保留中部折叠，上限 12k 字符，上下文有界） |
| sessions are trees（branch/rewind/summary） | `SessionTree::branch_from` / `rewind_to` / `prompt_history` 压缩窗口 + `AgentLoop::maybe_compress` |
| 无内置 MCP（立场非缺失），MCP-Direct 扩展：spawn → initialize → tools/list → registerTool，`sanitizeParams`，30s 超时，`promptSnippet` 必填 | `rupi-mcp`（`McpBridge` stdio JSON-RPC + `sanitize_params` + `mcp_tool_to_definition` + server→client 请求应答 roots/ping + `McpManager` 配对注册） |
| Skills（Agent Skills 开放标准，渐进披露） | `rupi-skills`（`SkillRegistry` 三阶段 + `load_skill` 工具） |
| Hermes 记忆：MEMORY.md/USER.md 冻结快照 + `MemoryProvider` 七方法 + `MemoryManager`（单外部）+ SQLite FTS5 session_search + background_review | `rupi-memory`（冻结快照 + provider/manager + `SessionStore` 触发器同步 FTS + `JsonlProvider` 示例，`--memory-provider jsonl` 即接即用） |
| Skill 自积累（后台 review 沉淀） | `SkillAccumulator::propose` + `rupi skill-distill` |
| 会话持久化 | 每轮落盘 `sessions.db`，`sessions` / `session-show` / `session-search`，`--resume <id>` 断点续聊（REPL + TUI 通用）；`/tree` 全分支视图 + `/goto <短id>` 跨分支时间旅行（节点 id 即库行 id，跨进程稳定） |
| Extension 热重载（自写工具-重载-自测） | `rupi-ext`（manifest + 外部进程契约 stdin JSON→stdout，`ExtensionSet::refresh` mtime 增量重载，`--ext-dir`/`ext-list`，REPL `/reload` + 每轮自动检查） |
| 权限门与计划模式 | `rupi-agent::policy`（`AllowAll`/`RulePolicy`/`ChainPolicy` + `Approver`，拒绝转 tool error；`--plan` + REPL `/plan` 只读侦察；bash 高危子串转人工审批 `[y(es)/a(ll session)/N]`，选 a 的（工具+原因）本会话免打扰；TUI 内同语义全屏暂停问询） |
| 子任务分发 | `rupi-agent::subagent`（`run_subagents` 分叉会话并发扇出 + `subagent` 委托工具，深度 guard 断递归；`--subagents` 开启） |
| coding-agent CLI + TUI | `rupi-cli`（`rupi` 二进制；`chat` REPL + `tui` ratatui 全屏界面：流式渲染/滚动/review 行） |

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

- `~/.rupi/memories/MEMORY.md` / `USER.md` / `failures.md`，`memory_char_limit=5000` / `user_char_limit=5000`。
- 启动时冻结快照注入系统提示（保 prefix cache）；会话内 `memory` 工具写盘即时生效，
  但快照不变，下个 session 才可见；`tool` 回包永远显示实时状态。
- 密钥扫描：`memory` 写入 / `failures.md` 记录含疑似密钥（api key / token / 私钥）一律拒绝落盘。
- 失败记忆：review 纠正检测（用户纠正 / 助手自认失败）→ `failures.md`，随快照注入 `<FailureMemory>`。
- 成功写入即镜像到 `sessions.db`（memories 表 + FTS5），`memory-search` / `memory_search` 工具按需查，不进每轮 prompt。
- 容量压实：写路径超限即按行丢最旧、保最新（`...[compacted]` 标记），磁盘文件永远有界。
- 双层记忆：从 cwd 上溯 `.git` 定项目根，`<root>/.rupi/MEMORY.md` 独立限额、分区注入；`memory` 工具与 `memory-write` 支持 `scope=project`。
- `MemoryManager` 只允许一个外部 provider，第二个拒绝并 warning；
  `prefetch` 超时/失败只记 debug，`sync` 失败记 warning，主循环不崩。
- `SessionStore`（SQLite + FTS5）提供 `session-search` 跨会话回忆。

## Skills 语义

- 目录 + `SKILL.md`（frontmatter 必含 `name`/`description`），约束与官方规范一致
  （name 小写-数字-连字符 ≤64，description ≤1024，body 建议 <5000 tokens / <500 行）。
- 渐进披露：`skills-list`（metadata 索引）→ `skill-load`（全文）→ `read_resource` 按需读
  `references/`/`assets/`；agent 内以 `load_skill` / `read_resource` 工具激活（schema 随工具表发给模型，空注册表时不挂载）。
- 自积累：`skill-distill <name> <description> [steps...]` 生成新 `SKILL.md` 草稿到
  `~/.rupi/skills/<name>/`，落盘前校验（name 规范 + description 压单行 1..=1024 + steps 非空 + 重名拒绝）。
  REPL/TUI 每轮发送前热刷新注册表，会话内新蒸馏 skill 下一轮即对模型可见，无需重启。
- 后台 review：`chat --review` 每轮后安静复盘并打印记忆/Skill 建议（默认离线启发式，`--review-llm`
  用模型做 JSON 复盘，提炼质量更高），
  `--review-apply` 直接落盘（`MEMORY.md` add + skill 草稿；已存在 skill 跳过不覆盖）。

## 自定义斜杠命令

- 文件即命令：`~/.rupi/commands/<name>.md`（或项目级 `.rupi/commands/<name>.md`）
  即 `/name args`，正文为提示模板，`$ARGUMENTS` 替换为用户参数，无占位符则追加到末尾。
- 可选 YAML frontmatter（`description` 等）只做元信息，解析时剥离；空文件不展开。
- 内建命令（`/quit`、`/tree`、`/goto` 等）优先；未知 `/foo` 先查自定义命令，
  命中则展开后发送（REPL 打印 `[command /foo]`，TUI 插一行同名系统提示），查不到才当普通消息。
- 发现：`rupi commands` 子命令与 REPL/TUI 内 `/commands` 列出全部自定义命令
 （description 取自 frontmatter，无则取正文首行）。

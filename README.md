# rupi — Pi Agent 的 Rust 复刻

最小 Harness + MCP + 记忆（Hermes 风格）+ Skill 自积累。TypeScript 版 Pi 的设计哲学是
**core 极小、一切走扩展组合**；本仓库用 Rust workspace 逐层复刻该思想。

## 对标关系

| Pi / 生态 | rupi |
|---|---|
| `pi-agent-core`（agent loop / state / events，~300 行核心） | `rupi-core`（`Message`/`SessionTree`/`AgentEvent`/`ToolDefinition`/`Extension`） + `rupi-agent`（`AgentLoop`/`PromptBuilder`） |
| `pi-ai`（统一 LLM API，多 provider） | `rupi-llm`（`LlmProvider` trait + `OpenAiCompatProvider` + `AnthropicProvider`（`claude-*` 自动路由，system 独立参数/tool_result/user 交替合并/真 SSE）+ `GeminiProvider`（`gemini-*` 自动路由，user/model 角色/functionCall-Response/真流）+ `MockProvider`，`complete_streaming` 真 SSE；`--model` 启动指定，REPL `/model [名]` 会话内切换；回包兼容数组 content 与对象式 arguments；429/5xx + Retry-After 指数退避重试；Anthropic prompt caching（system/末工具断点）；`ThinkingLevel` 四档映射 reasoning_effort/thinkingLevel/thinking+budget（含签名回放：有签原样、无签降文本防 400，400 仍命中则去思考块重试一次），`--thinking` + REPL/TUI `/thinking` 会话内切换；OpenRouter（base_url 含 openrouter.ai）默认带 `x-session-id` 会话亲和头（荷载为 sessions.db 会话 id，`--resume` 同 id 即同一下游；`RUPI_SESSION_AFFINITY=0/1` 显式关/开）） |
| 默认七工具 Read/Write/Edit/Bash/Glob/Grep/Think | `rupi-tools`（`ToolRegistry::with_builtins`；`read` 分页 offset/limit + 大文件截断标注，`bash` 支持 `timeout_secs` + 输出首尾保留中部折叠，上限 12k 字符，上下文有界；REPL/TUI 用 `with_sandboxed_builtins` 把 read/write/edit 约束在启动 cwd 内——`..`/绝对路径/符号链接逃逸拒绝并改写为绝对路径执行，subagent 克隆继承） |
| sessions are trees（branch/rewind/summary） | `SessionTree::branch_from` / `rewind_to` / `prompt_history` 压缩窗口 + `AgentLoop::maybe_compress`（`--compress-threshold/--compress-keep` 全局，`RUPI_COMPRESSION_OVERRIDES` 按 `provider/model` 覆盖，对标上游 `compaction.modelOverrides`；溢出报错强制压实 + 同 turn 重发（`MAX_OVERFLOW_RECOVERIES=2` 封顶），对标上游 overflow recovery） |
| 无内置 MCP（立场非缺失），MCP-Direct 扩展：spawn → initialize → tools/list → registerTool，`sanitizeParams`，30s 超时，`promptSnippet` 必填 | `rupi-mcp`（`McpBridge` stdio JSON-RPC + StreamableHTTP（`url` 配置，POST 单 JSON/SSE 回包，`mcp-session-id` 保持）+ `sanitize_params` + `mcp_tool_to_definition` + server→client 请求应答 roots/ping（stdio 与 HTTP 同语义：POST 回包流增量消费 + 独立 GET 常驻流，反向请求当场 POST 应答，桥 drop 即停流）+ `resources/list→read`（每 server `{server}_read_resource`）+ `prompts/list→get`（每 server `{server}_get_prompt`）+ `McpManager` 配对注册与失败隔离；`mcp-list` 三区段探活，`--url` 直探 HTTP） |
| Skills（Agent Skills 开放标准，渐进披露） | `rupi-skills`（`SkillRegistry` 三阶段 + `load_skill`/`read_resource` 工具 + 每轮 `refresh` 热加载（蒸馏即对模型可见，REPL/TUI 同闭环）+ `SkillAccumulator::propose` 落盘校验（名/描述/steps，非 ascii 回合 hash 兜底命名）；内建 `skills/builtin` 走 exe 锚定发现，cwd 无关） |
| Hermes 记忆：MEMORY.md/USER.md 冻结快照 + `MemoryProvider` 七方法 + `MemoryManager`（单外部）+ SQLite FTS5 session_search + background_review | `rupi-memory`（冻结快照 + `<MemoryGuidance>` 指导块 + `--no-memory` 总开关 + provider/manager + `SessionStore` 触发器同步 FTS（trigram 中英文子串召回 + bm25 排名，老库自动迁移）+ 全局/项目 two-tier + 密钥拒写 + failures.md + `JsonlProvider` 示例，`--memory-provider jsonl` 即接即用） |
| Skill 自积累（后台 review 沉淀） | 启发式复盘默认开（`--no-review` 关，`--review-llm` 切模型版）：记忆/纠正失败/多工具草稿建议只打印，`--review-apply` 才落盘（MEMORY.md/failures.md/skills/）+ `rupi skill-distill` 手工蒸馏 |
| 会话持久化 | 每轮落盘 `sessions.db`，`sessions` / `session-show` / `session-search`，`--resume <id>` 断点续聊（REPL + TUI 通用）；`/tree` 全分支视图 + `/goto <短id>` 跨分支时间旅行（节点 id 即库行 id，跨进程稳定） |
| Extension 热重载（自写工具-重载-自测） | `rupi-ext`（manifest + 外部进程契约 stdin JSON→stdout，`ExtensionSet::refresh` mtime 增量重载，`--ext-dir`/`ext-list`，REPL `/reload` + 每轮自动检查；子进程 stdout 与内置 bash 同口径 12k 折叠，超时 `kill_on_drop` 无僵尸） |
| 权限门与计划模式 | `rupi-agent::policy`（`AllowAll`/`RulePolicy`/`ChainPolicy` + `Approver`，拒绝转 tool error；`--plan` + REPL/TUI `/plan` 只读侦察；bash 高危子串转人工审批 `[y(es)/a(ll session)/N]`，选 a 的（工具+原因）本会话免打扰；TUI 内同语义全屏暂停问询；问询前后派发 `UiPromptStart/End` 事件供宿主区分等待耗时） |
| 子任务分发 | `rupi-agent::subagent`（`run_subagents` 分叉会话并发扇出 + `subagent` 委托工具，深度 guard 断递归；`--subagents` 开启；父会话 thinking 档透传子循环） |
| coding-agent CLI + TUI | `rupi-cli`（`rupi` 二进制；`chat` REPL + `run` 非交互（`--approve/--no-approve`，`--review/--review-apply` 后台沉淀落盘）+ `tui` ratatui 全屏界面：流式渲染/滚动/review 行；TUI 内建命令与 REPL 同语义（`/rewind/plan/thinking/model`，`reload` 明确提示 REPL 专属）+ `/` 开头 Tab 补全，弹窗展示候选） |

## 快速开始

```bash
cargo build --workspace
cargo test --workspace

# 无 Key 演示模式（MockProvider）
./target/debug/rupi --help
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-write add "likes tea"
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-show
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skills-list
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skill-load commit-helper
echo "/quit" | RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi chat

# 接真实模型（OpenAI-compatible）
export RUPI_API_KEY=... RUPI_BASE_URL=https://api.openai.com/v1
./target/debug/rupi --model gpt-4o-mini chat

# 接 Claude（原生 Anthropic Messages API，--model claude-* 自动路由）
export RUPI_ANTHROPIC_KEY=... # 或 ANTHROPIC_API_KEY；网关 RUPI_ANTHROPIC_BASE 可选
./target/debug/rupi --model claude-sonnet-4-5 chat

# 接 Gemini（原生 generateContent，--model gemini-* 自动路由）
export RUPI_GEMINI_KEY=... # 或 GEMINI_API_KEY / GOOGLE_API_KEY；网关 RUPI_GEMINI_BASE 可选
./target/debug/rupi --model gemini-2.5-flash chat
```

## MCP 探活

```bash
./target/debug/rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
echo '[{"name":"fake","command":"python3","args":["crates/rupi-mcp/tests/fake_mcp_server.py"],"env":{}}]' > /tmp/mcp.json
echo "/quit" | ./target/debug/rupi --mcp-config /tmp/mcp.json chat
# → [mcp] 5 tools: fake_echo, fake_fail, fake_roots_probe, fake_read_resource, fake_get_prompt
```

语义与 `pi-directx` 一致：stdio 上换行分隔 JSON-RPC 2.0，
`tools/list`（cursor 分页）→ 每个工具 `registerTool` 为 `{prefix}_{name}`，
`tools/call` 前按 `inputSchema` 把 string 纠正回 boolean/number/integer，
`prompt_snippet` 必填否则 agent 看不见工具。
外加 `resources/list → resources/read`（每 server 一个 `{server}_read_resource`）、
`prompts/list → prompts/get`（每 server 一个 `{server}_get_prompt`），description 自带可用 URI/模板名。
传输除 stdio 外还支持 StreamableHTTP：配置里给 `url` 即走 POST（单 JSON 或 SSE 回包二选一，
`mcp-session-id` 自动保持），例：`[{"name":"h","command":"","args":[],"env":{},"url":"http://127.0.0.1:8000/mcp"}]`。

## 记忆语义（Hermes 对齐）

- `~/.rupi/memories/MEMORY.md` / `USER.md` / `failures.md`，`memory_char_limit=5000` / `user_char_limit=5000`。
- 启动时冻结快照注入系统提示（保 prefix cache）；会话内 `memory` 工具写盘即时生效，
  但快照不变，下个 session 才可见；`tool` 回包永远显示实时状态。
- 模型指导块 `<MemoryGuidance>`（静态文本，不伤 prefix cache）：何存（可复用的偏好/纠正/教训，项目事实用 `scope=project`）、何取（先 `memory_search` 再追问）、不存瞬态与密钥；空库时也注入，否则存→冻→忆的环转不起来。
- `--no-memory`：回合内撤下内建记忆（内容/工具/指导块，外部 provider 不受影响），显式记忆子命令照常可用。
- 密钥扫描：`memory` 写入 / `failures.md` 记录含疑似密钥（api key / token / 私钥）一律拒绝落盘。
- 失败记忆：review 纠正检测（用户纠正 / 助手自认失败）→ `failures.md`，随快照注入 `<FailureMemory>`。
- 成功写入即镜像到 `sessions.db`（memories 表 + FTS5），`memory-search` / `memory_search` 工具按需查，不进每轮 prompt。
- 外部 recall 预检门：寒暄/应答/斜杠命令（`hi!`/`thanks :)`/`/tree`）跳过本轮外部 prefetch（对标 Hermes `is_trivial_prompt`），省后端往返且不带偏单字回复；显式 recall 工具不受影响。
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
- 后台 review：默认每轮离线启发式复盘，非空建议打印（空则零打扰；只建议不落盘，无自主写盘），
  `--review-apply` 落盘（`MEMORY.md` add + skill 草稿；已存在 skill 跳过不覆盖），`--review-llm`
  用模型做 JSON 复盘（提炼质量更高），`--no-review` 关闭。

## 自定义斜杠命令

- 文件即命令：`~/.rupi/commands/<name>.md`（或项目级 `.rupi/commands/<name>.md`）
  即 `/name args`，正文为提示模板，`$ARGUMENTS` 替换为用户参数，无占位符则追加到末尾。
- 可选 YAML frontmatter（`description` 等）只做元信息，解析时剥离；空文件不展开。
- 内建命令（`/quit`、`/tree`、`/goto` 等）优先；未知 `/foo` 先查自定义命令，
  命中则展开后发送（REPL 打印 `[command /foo]`，TUI 插一行同名系统提示），查不到才当普通消息。
- 发现：`rupi commands` 子命令与 REPL/TUI 内 `/commands` 列出全部自定义命令
 （description 取自 frontmatter，无则取正文首行）。

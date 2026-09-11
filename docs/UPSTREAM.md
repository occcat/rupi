# 上游对照（Upstream alignment）

目标：**earendil-works/pi `@earendil-works/pi-coding-agent` 0.85.x** 的 Rust 复刻，外加 Pi 刻意留给扩展层、
但现代 agent harness 常见的三块：MCP、Hermes 风格记忆、Skill 自积累。
本文说明每个子系统对应上游哪一部分、哪些是有意偏离、哪些明确不移植。（形式借鉴 `rupi-pi-agent-9492` 分支的 `docs/UPSTREAM.md`。）

参考：
- Pi 源码：https://github.com/earendil-works/pi （`packages/agent`、`packages/coding-agent`、`packages/ai`）
- Hermes 记忆与 skills：https://hermes-agent.nousresearch.com/docs/user-guide/features/memory

## Agent loop（`packages/agent/src/agent-loop.ts` → `crates/rupi-agent/src/lib.rs`）

| 上游 | rupi | 说明 |
|---|---|---|
| `runLoop`：流式助手消息 → 工具 → 循环直到无工具调用 | `AgentLoop::run` / `run_with_user` | 事件：`TurnStart/TextDelta/ToolStart/ToolEnd/TurnEnd/RunEnd/Usage/CompactionStart/End/UiPromptStart/End/UiHint` |
| `beforeToolCall` / `afterToolCall` | `ToolHook::before/after`（`hooks.rs`） | before 可改写参数或拒绝；after 包住一切结果（含拒绝路径） |
| `toolExecution: parallel \| sequential` | `--parallel-tools`，默认串行 | 并行 `join_all`，事件与结果保原序；审批问询永远串行发生在执行前 |
| 取消（effect gate） | `CancelFlag`（Atomic + Notify） | turn 边界、流中、串行工具间隙三处检查点；bash 进程组 SIGKILL |
| 溢出恢复（`isContextOverflow` → 强制压实重发） | `overflow.rs` + `MAX_OVERFLOW_RECOVERIES=2` | 现在对 OpenAI-compat 也生效：非 2xx 回包读出 body 后再匹配（见"本次合并"） |
| steering / follow-up 队列 | 部分：TUI 运行中输入自动排队为下一轮 | 未实现 Pi 的运行中注入（mid-run steering） |
| `stopReason=error` 转助手消息 | 直接返回 `Err`，调用方决定 | REPL 打印错误并回滚本轮；`run` 非零退出；错误轮**不落盘**（对比 2c40 分支会污染会话） |

## Compaction（`packages/agent/src/harness/compaction` → `AgentLoop::compress_inner`）

- 按 token（`reserveTokens=16384`、`keepRecentTokens=20000`）：`used > context_window - reserve` 触发，切点从尾部累加 `keepRecentTokens`。估算：ASCII ≈ 4 字/token，CJK ≈ 1 字/token，provider `usage` 校准比例。`settings.json` `compaction` 与 `--compress-threshold/--compress-keep`（别名 `--reserve-tokens/--keep-recent-tokens`）可覆盖；`RUPI_COMPRESSION_OVERRIDES` 按 `provider/model` 覆盖，对标 `compaction.modelOverrides`（旧字段 `threshold_chars`/`keep_last` 仍作别名）。
- 摘要由模型生成，失败退回首行拼接；`<read-files>/<modified-files>` 文件足迹跨轮合并（对标上游 file-ops 追踪）。
- `/compact [指令]` 手动压实，对标 `compaction.customInstructions`。
- 有意偏离：摘要不写回会话树节点，只存 `sessions.summary` 一列，resume 时预热窗口。

## 会话（Pi "sessions are trees" → `rupi_core::SessionTree` + `sessions.db`）

- 树：`branch_from` / `rewind_to` / `goto_node` / `tree_view`，节点 id 即库行 id，`/goto 短id` 跨进程稳定。
- 持久化：SQLite（WAL）而非上游 JSONL。**本次合并后每个节点完整落盘**：`content` 列存纯文本（FTS/展示），
  `blocks` 列存整条 `Message` JSON（工具调用、工具结果、思考块），`--resume` / TUI `/resume` 按结构回填，模型续聊时看到完整工具上下文。老库自动补列（`user_version=4`：`name`/`cwd`/`updated_at`/`parent_session`）。每轮 3–4 条 INSERT 走 `persist_turn` 一个事务；CLI/TUI 落盘与记忆文件写进 `spawn_blocking`/`tokio::fs`。`memory_search`/`session_search`/`mirror_memory` 复用 `MemoryStore` 里的 `sessions.db` 连接。
- 互操作：`--continue`/`-c` 最近会话（优先同 cwd）、`--no-session`、`--name`/`/name`、`/export`（Pi JSONL v3 + 简易 HTML）、`/import`、`/fork`（当前路径新 id）、`/clone`（全树 remap）。存储仍是 SQLite；JSONL 是进出口，不是主存。
- 不移植：`--session <path>` 直接打开上游文件当主存、`/share`。

## 工具（`packages/coding-agent/src/core/tools` → `crates/rupi-tools`）

| 上游 | rupi |
|---|---|
| `read`（`offset/limit`，头截 2000 行/50KB，含图片） | 先 `metadata` 再分页；图片整读（上限 10MB）挂 `ToolImage`；`@file` 图片上限 5MB |
| `edit`（`edits[]` 按原文匹配、必须唯一、不得重叠、返回 diff） | **同语义**：`old_string/new_string`（+`replace_all`）或 `edits[]`（兼容 `oldText/newText`），唯一性校验、重叠拒绝、统一 diff 回包。此前是 `replacen(...,1)` 静默改首个匹配 |
| `bash`（尾截 2000 行/50KB、超时、进程组 kill） | 同默认（`truncate.rs` 自 2c40 分支搬运），默认 30s 超时可调，`execute_with_cancel` 进程组 SIGKILL、取消返回部分输出 |
| `grep` / `find` / `ls` | `grep` / `glob`；`ls` 用 bash |
| — | `think`（Anthropic 风格）、`load_skill` / `read_resource`、`memory` / `memory_search` / `session_search`、`search_tools`（渐进式发现） |
| 无路径沙箱 | `SandboxedTool`：read/write/edit/glob/grep 约束在启动 cwd 内（canonicalize，拒绝 `..`/绝对路径/符号链接逃逸）；bash 不受限（与上游一致） |
| 同文件写串行化（`withFileMutationQueue`） | `FileMutationQueue` |

## Provider 层（`packages/ai` → `crates/rupi-llm`）

- OpenAI 兼容 / Anthropic Messages / Gemini `generateContent`，按模型名前缀路由（`claude-*` / `gemini-*` / 其余）。
- 真 SSE 流式。**本次合并后三家共用 `sse.rs`**（自 2c40 搬运并加强）：跨 chunk UTF-8 增量解码、多行 `data:`、`event:`、CRLF；流中 `error` 事件/对象转错误而非静默空回。
- `usage`：OpenAI `stream_options.include_usage`、Anthropic `message_start/message_delta.usage`、Gemini `usageMetadata` → `StreamEvent::Usage` → `AgentEvent::Usage`（REPL/TUI/`run` 显示，`--json` 输出为事件）。
- 重试：429/5xx + `Retry-After`，指数退避 500ms·2ⁿ 上限 8s，3 次。
- thinking 六档 `off|low|medium|high|xhigh|max` 映射 `reasoning_effort` / `thinkingLevel` / `thinking.budget_tokens`；开启 Anthropic 思考时 `max_tokens = max(请求, budget+4096)`，medium/high 不再因默认 4096 静默失效。
- 路由：`provider/model[:thinking]`（`--provider` / `--api-key` 可覆盖）；显式 `openai` / `anthropic` / `gemini` / `openrouter` / `azure` / `bedrock` / `vertex`。Bedrock 走 Anthropic Messages + `anthropic_version`（Bearer，`AWS_BEARER_TOKEN_BEDROCK`）；Vertex 走 Gemini `generateContent`（Bearer，`VERTEX_TOKEN`）；Azure 走 deployment URL + `api-key`；OpenRouter 加 Referer/Title 与默认亲和。
- 模型目录：内置 `crates/rupi-llm/models.json`，可被 `RUPI_MODELS` 或 `~/.rupi/models.json` 覆盖合并；`--list-models` / `rupi models`。
- 图片：`ContentBlock::Image` 映射三家（OpenAI `image_url` data URL / Anthropic `image` source / Gemini `inline_data`）；`read` 先 `metadata` 再分页；`@file` 图片先判大小再整读（上限 5MB）。
- 成本：内建粗粒度价目（`TokenMeter` footer `↑↓ tokens / context% / $`）。
- OAuth：`rupi login [provider]` **仅 stub**（打印 API key 用法，无浏览器/设备码）。未落地：Claude Pro/Max、ChatGPT Codex、GitHub Copilot 订阅登录；Bedrock SigV4。

## MCP（Pi 生态 `pi-mcp-adapter` / 官方规范 2024-11-05 + Streamable HTTP）

- stdio：换行 JSON-RPC，按 id 路由 oneshot，30s 超时，server→client `roots/list` / `ping` 应答，`notifications/tools/list_changed` 差量刷新。
- Streamable HTTP：POST 单 JSON / SSE 回包，`mcp-session-id` 保持，独立 GET 常驻流，反向请求当场 POST 应答。
- `tools/list`（cursor 分页）→ `{server}_{tool}`；`resources/list→read` / `prompts/list→get` 各生成一个 per-server 工具；`sanitize_params` 按 schema 把 string 纠回 boolean/number。
- 不移植：legacy HTTP+SSE 双端点、JSON-RPC batch、sampling。

## 记忆（Hermes）

- `MEMORY.md` / `USER.md` / `failures.md`，各 5000 字符，超限按行丢最旧；启动冻结快照（保 prefix cache），会话内写盘即时、下个 session 可见。
- 项目域：从 cwd 上溯 `.git`，`<root>/.rupi/MEMORY.md` 独立限额分区注入；`scope=project`。
- **`[core]` 分层（本次合并，借鉴 9492 分支 / Hermes core-extended）**：任一行带 `[core]` 时只注入标记行，其余为 extended 层，`memory_search` 按需召回（FTS 镜像 + 文件子串兜底）；无标记全量注入，向后兼容。
- 密钥扫描拒写；`MemoryProvider` 外部接口（单 provider）+ `JsonlProvider` 示例；prefetch 3s 超时 + 寒暄门。

## Skills（Agent Skills 开放标准 + Hermes 自积累）

- 递归发现（honor `.gitignore/.ignore`），`SKILL.md` frontmatter 校验，三阶段渐进披露，`/skillname args` 即斜杠命令（兼容 `/skill:name`），每轮热刷新。
- 发现目录对齐 Pi / Agent Skills：全局 `~/.pi/agent/skills` 与 `~/.agents/skills`；项目 `.pi/skills` 与 `.agents/skills` 从 cwd 上溯（有 `.git` 停在仓库根，否则到文件系统根）。另保留 `skills/builtin`、`~/.rupi/skills`、`.rupi/skills`。
- 自积累：默认启发式复盘只建议不落盘，`--review-apply` 落盘，`--review-llm` 用模型复盘；`skill-distill` 手工蒸馏。

## 扩展（Pi 进程内 TS → `crates/rupi-ext`）

- oneshot：manifest + 子进程 stdin JSON → stdout（原行为）。
- **jsonrpc**：长连接双向 JSON-RPC（复用 `rupi_mcp::StdioRpc` 帧）。扩展可 `initialize` 注册斜杠命令、订阅 `tool_call` / `turn_end` / `session_*`，`tools/call` 可回 `ui` hint；运行中 `registerCommand` / `subscribe` / `ui/hint`。
- 不移植：WASM、注册 provider、自定义编辑器/渲染器、快捷键/flag。

## CLI / TUI（`packages/coding-agent` CLI）

| 上游 | rupi |
|---|---|
| `pi` 交互 | `rupi chat`（REPL）与 `rupi tui`（ratatui：Tab 补全、`@path`、`/sessions` `/resume`、运行中排队） |
| `pi -p` | `rupi run "..."` |
| `pi --mode json` | `rupi run --json "..."`（本次合并）：stdout 每行一个 `AgentEvent`（`{"type":"text_delta",...}`），末行 `run_result`，出错 `error` 行 + 非零退出 |
| `pi --continue/--resume` | `--continue`/`-c` 最近会话、`--resume <id>`，`rupi sessions` / `session-show` |
| `/export` `/import` `/fork` `/clone` `/name` | 同名（JSONL 贴 Pi session-format v3；HTML 为简易独立页） |
| `/tree` `/compact` `/model` `/thinking` | 同名；另有 `/rewind` `/goto` `/plan` `/reload` `/skills` `/commands` |
| `settings.json` + `SYSTEM.md` | `rupi-config`：`~/.rupi/settings.json` + 上溯 `.rupi/settings.json`；`--tools/--exclude-tools/--no-tools`、`--system-prompt/--append-system-prompt` |
| 自定义命令 | `~/.rupi/commands/*.md` 与 `.rupi/commands/*.md`；`$ARGUMENTS` / `$1` / `{{var}}`；JSON-RPC 扩展命令走 `commands/execute` |
| `pi install git:/npm:` | `rupi install` / `uninstall` / `packages`（`rupi-pkg`；物化 skill/command/`*.json` 扩展，不跑 npm 脚本） |
| 不移植 | `--mode rpc`、会话文件选择器 UI、Pi 的主题热重载/差分渲染器 |

## 本次合并（2026-09-11，main ← `rupi-pi-agent-port-23a6`）新增/修复

1. REPL 遇 provider 错误不再退出进程：回滚本轮节点、打印原因、回到提示符（TUI 同步回滚）。
2. `edit` 唯一匹配校验 + `replace_all` + `edits[]` 多处编辑 + 统一 diff（对标 Pi，搬运 2c40 的 `apply_edits` 语义）。
3. OpenAI-compat 非 2xx 回包读出 body：用户看到网关原因，溢出恢复对该 provider 生效。
4. 会话落盘完整节点（`blocks` JSON），`--resume` 恢复工具上下文；老库自动迁移。
5. 搬运：`truncate.rs`（bash 尾截 2000 行/50KB）、`sse.rs`（多行 data、跨 chunk UTF-8）、`run --json`、usage 统计。
6. 借鉴：`[core]` 记忆分层、本文档。

已知未做：完整 OAuth 设备码流；子 agent 继承 policy/approver；悬空符号链接写入逃逸沙箱；扩展 WASM。

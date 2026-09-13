# rupi

[`@earendil-works/pi-coding-agent`](https://github.com/earendil-works/pi) 的 Rust 复刻。
循环、会话树、工具、MCP、记忆和 skill 按 crate 分层。入口有两个：

| 入口 | 做什么 |
|---|---|
| `rupi` | 本机。无子命令默认 TUI；`chat` 是 REPL；`run` / `-p` 非交互；`--mode rpc` 是 JSONL。会话在 `$RUPI_HOME/sessions.db`（SQLite WAL + FTS5）。 |
| `rupi-server` | 无状态云控制面。会话与记忆在 PostgreSQL；Redis 只缓存（挂了读库）；bash 和会执行代码的工具只打在外部 `Executor`（`rupi-execd` 或 `rupi-sandboxd`）。对话是 AG-UI：`POST /v1/agent` → SSE。管理面 `/admin`。 |

云不链 `rupi-tui`。本机继续用 SQLite。鉴权是租户 API Key + BYOK。`rupi login` 只打印 key 用法，没有浏览器/设备码，也没有订阅登录或商店付费。

上游对照与未移植：[`docs/UPSTREAM.md`](docs/UPSTREAM.md)。  
三分支历史评审：[`BRANCH_COMPARISON.md`](BRANCH_COMPARISON.md)。

## Crate

| crate | 职责 |
|---|---|
| [`rupi-core`](crates/rupi-core/README.md) | `Message` / `SessionTree` / `AgentEvent` / `ToolDefinition` / `Extension`。无 LLM、无 IO。 |
| [`rupi-agent`](crates/rupi-agent/README.md) | `AgentLoop`、`PromptBuilder`、权限门、subagent。 |
| [`rupi-llm`](crates/rupi-llm/README.md) | `LlmProvider`：OpenAI 兼容 / Anthropic / Gemini / OpenRouter / Azure / Bedrock / Vertex。真 SSE。 |
| [`rupi-tools`](crates/rupi-tools/README.md) | Read / Write / Edit / Bash / Glob / Grep / Think；REPL/TUI 用 `with_sandboxed_builtins` 箍 cwd。 |
| [`rupi-mcp`](crates/rupi-mcp/README.md) | stdio 与 Streamable HTTP 的 MCP 桥。 |
| [`rupi-memory`](crates/rupi-memory/README.md) | Hermes 风格 `MEMORY.md` / `USER.md` + `SessionStore`。云路径权威在 Postgres，不在这份文件。 |
| [`rupi-skills`](crates/rupi-skills/README.md) | Agent Skills：发现、渐进披露、`skill-distill`。 |
| [`rupi-ext`](crates/rupi-ext/README.md) | 外部进程扩展（oneshot / JSON-RPC），mtime 热重载。 |
| [`rupi-config`](crates/rupi-config/README.md) | `~/.rupi/settings.json` 与项目 `.rupi/settings.json` 合并。 |
| [`rupi-pkg`](crates/rupi-pkg/README.md) | `rupi install` / `uninstall` / `packages`。不跑 npm 脚本。 |
| [`rupi-tui`](crates/rupi-tui/README.md) | ratatui 界面。云不依赖它。 |
| [`rupi-cli`](crates/rupi-cli/README.md) | `rupi` 二进制：本机装配 + `rupi cloud` 瘦客户端。 |
| [`rupi-runtime`](crates/rupi-runtime/README.md) | `Executor` trait、`rupi-execd`、`rupi-sandboxd`。 |
| [`rupi-server`](crates/rupi-server/README.md) | 云控制面 + `/admin`。 |
| [`rupi-benches`](crates/rupi-benches/README.md) | Criterion：SSE / edit / compaction / `SessionStore`。 |

## 本机快速开始

```bash
cargo build --workspace
cargo test --workspace

# 无 Key 走 MockProvider
./target/debug/rupi --help
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-write add "likes tea"
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi memory-show
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skills-list
RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi skill-load commit-helper
echo "/quit" | RUPI_HOME=/tmp/rupi-demo ./target/debug/rupi --continue chat
# ~/.rupi/settings.json 与项目 .rupi/settings.json（后覆盖前）
# {"model":"gpt-4o-mini","thinking":"low","theme":"dark","tools":["read","bash"],"compaction":{"reserveTokens":16384,"keepRecentTokens":20000}}

# OpenAI 兼容
export RUPI_API_KEY=... RUPI_BASE_URL=https://api.openai.com/v1
./target/debug/rupi --model gpt-4o-mini chat

# 非交互 + JSONL（对标 pi --mode json；每行一个 AgentEvent，末行 run_result）
./target/debug/rupi --model gpt-4o-mini run --json "summarize README.md"

# Anthropic Messages（--model claude-* 自动路由）
export RUPI_ANTHROPIC_KEY=... # 或 ANTHROPIC_API_KEY；网关 RUPI_ANTHROPIC_BASE 可选
./target/debug/rupi --model claude-sonnet-4-5 chat

# Gemini generateContent（--model gemini-* 自动路由）
export RUPI_GEMINI_KEY=... # 或 GEMINI_API_KEY / GOOGLE_API_KEY；网关 RUPI_GEMINI_BASE 可选
./target/debug/rupi --model gemini-2.5-flash chat
```

无子命令默认 TUI。`chat` 仍是 REPL。`--session` / `-r` / `--fork` / `--resume` / `--continue` 续聊；`--no-session` 不落盘。位置参数 `@file`。`--tools` / `--exclude-tools` / `--no-tools` / `--no-builtin-tools`。`--no-context-files` 跳过 `AGENTS.md` / `CLAUDE.md`。状态栏：`↑↓ tokens / context% / $`。

## 云控制面

需要 PostgreSQL、Redis，以及至少一个外部执行节点。控制面进程里不跑用户 `sh -c`，也不把本机 Docker 当默认执行面。

```bash
# 执行节点（二选一或并存；可写 region=url）
./target/debug/rupi-execd --listen 127.0.0.1:8090 --root /tmp/rupi-execd
./target/debug/rupi-sandboxd --listen 127.0.0.1:8190 --root /tmp/rupi-sandboxd

# 控制面。--bootstrap-tenant / --bootstrap-admin 只打印一把明文 Key 后退出，给本地和 CI。
./target/debug/rupi-server \
  --listen 127.0.0.1:8080 \
  --database-url postgres://rupi:rupi@127.0.0.1:5432/rupi \
  --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 \
  --sandbox-urls http://127.0.0.1:8190 \
  --admin-token "$RUPI_ADMIN_TOKEN"

# 租户 Key：Authorization: Bearer <rupi_...>
curl -sS http://127.0.0.1:8080/health
curl -sS -H "Authorization: Bearer $RUPI_CLOUD_KEY" http://127.0.0.1:8080/v1/me

# 本机瘦客户端：只发 AG-UI/HTTP，bash 在 Executor 上跑
./target/debug/rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 me
./target/debug/rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 prompt "hello"
```

管理面：浏览器打开 `http://127.0.0.1:8080/admin`，凭证是 `RUPI_ADMIN_TOKEN` 或库内 `rupi_admin_*` Key。页面管租户、配额、settings、会话目录、执行池容量；对话仍走 `POST /v1/agent` 或 `rupi cloud prompt`。

薄 REST 补租户目录、建仓、fork/clone、进出口。`--mode rpc` 只留本机。

## MCP 探活

```bash
./target/debug/rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
echo '[{"name":"fake","command":"python3","args":["crates/rupi-mcp/tests/fake_mcp_server.py"],"env":{}}]' > /tmp/mcp.json
echo "/quit" | ./target/debug/rupi --mcp-config /tmp/mcp.json chat
# → [mcp] 5 tools: fake_echo, fake_fail, fake_roots_probe, fake_read_resource, fake_get_prompt
```

与 `pi-directx` 同语义：stdio 上换行分隔 JSON-RPC 2.0，
`tools/list`（cursor 分页）→ 每个工具 `registerTool` 为 `{prefix}_{name}`，
`tools/call` 前按 `inputSchema` 把 string 纠正回 boolean/number/integer，
`prompt_snippet` 必填否则 agent 看不见工具。
另有 `resources/list → resources/read`（每 server 一个 `{server}_read_resource`）、
`prompts/list → prompts/get`（每 server 一个 `{server}_get_prompt`），description 带可用 URI/模板名。
传输除 stdio 外还支持 Streamable HTTP：配置里给 `url` 即走 POST（单 JSON 或 SSE 回包，
`mcp-session-id` 自动保持），例：`[{"name":"h","command":"","args":[],"env":{},"url":"http://127.0.0.1:8000/mcp"}]`。

## 记忆（本机 Hermes）

- `~/.rupi/memories/MEMORY.md` / `USER.md` / `failures.md`，`memory_char_limit=5000` / `user_char_limit=5000`。
- `[core]` 分层：条目以 `[core]` 开头即固定注入每个 session；一旦存在任何 `[core]` 条目，未标记条目成为 extended 层，只在 `memory_search`（FTS 镜像 + 文件子串兜底）时召回。无标记时全量注入。
- 启动时冻结快照注入系统提示（保 prefix cache）；会话内 `memory` 工具写盘即时生效，快照不变，下个 session 才可见；`tool` 回包永远显示实时状态。
- `<MemoryGuidance>`：何存（可复用的偏好/纠正/教训，项目事实用 `scope=project`）、何取（先 `memory_search` 再追问）、不存瞬态与密钥。空库也注入。
- `--no-memory`：回合内撤下内建记忆（内容/工具/指导块，外部 provider 不受影响）。显式记忆子命令照常可用。
- 密钥扫描：`memory` 写入 / `failures.md` 记录含疑似密钥（api key / token / 私钥）一律拒绝落盘。
- 失败记忆：review 纠正检测（用户纠正 / 助手自认失败）→ `failures.md`，随快照注入 `<FailureMemory>`。
- 成功写入即镜像到 `sessions.db`（memories 表 + FTS5），`memory-search` / `memory_search` 按需查，不进每轮 prompt。
- 外部 recall 预检门：寒暄/应答/斜杠命令（`hi!`/`thanks :)`/`/tree`）跳过本轮外部 prefetch（对标 Hermes `is_trivial_prompt`）。显式 recall 工具不受影响。
- 容量压实：写路径超限即按行丢最旧、保最新（`...[compacted]` 标记）。
- 双层记忆：从 cwd 上溯 `.git` 定项目根，`<root>/.rupi/MEMORY.md` 独立限额、分区注入；`memory` 与 `memory-write` 支持 `scope=project`。
- `MemoryManager` 只允许一个外部 provider，第二个拒绝并 warning；`prefetch` 超时/失败只记 debug，`sync` 失败记 warning，主循环不崩。
- `SessionStore` 提供 `session-search` 跨会话回忆。

云上的记忆权威是 PostgreSQL 行（带 `tenant_id`）。sandbox 卷上的 `MEMORY.md` 不是主存。

## Skills

- 目录 + `SKILL.md`（frontmatter 必含 `name`/`description`）。name 小写-数字-连字符 ≤64，description ≤1024，body 建议 <5000 tokens / <500 行。
- 渐进披露：`skills-list`（metadata）→ `skill-load`（全文）→ `read_resource` 读 `references/` / `assets/`。agent 内以 `load_skill` / `read_resource` 激活；空注册表不挂载。
- 发现目录（先发现者胜）：`skills/builtin`（exe 锚定）→ `~/.rupi/skills` → 项目 `.rupi/skills` → `~/.pi/agent/skills` → `~/.agents/skills` → 从 cwd 上溯的 `.pi/skills` / `.agents/skills`（有 `.git` 停在仓库根）。项目级目录与记忆同走信任门。
- 斜杠直调：`/skillname args`；兼容 Pi 的 `/skill:name`。
- 自积累：`skill-distill <name> <description> [steps...]` 生成草稿到 `~/.rupi/skills/<name>/`，落盘前校验（name 规范 + description 压单行 1..=1024 + steps 非空 + 重名拒绝）。REPL/TUI 每轮发送前热刷新，会话内新蒸馏的 skill 下一轮即对模型可见。
- 后台 review：默认每轮离线启发式复盘，非空建议才打印，不落盘。`--review-apply` 才写 `MEMORY.md` / skill 草稿（已存在 skill 不覆盖）。`--review-llm` 改用模型做 JSON 复盘（多一次 LLM 调用）。`--no-review` 关闭。

## 自定义斜杠命令

- `~/.rupi/commands/<name>.md`（或项目 `.rupi/commands/<name>.md`）即 `/name args`。占位符：`$ARGUMENTS` / `$1` / `${1:-default}`，以及 `{{var}}` / `{{var:-default}}`（`name=value` 或 `--name value`）；无占位符则追加到末尾。
- YAML frontmatter（`description` 等）只做元信息；空文件不展开。
- 内建命令（`/quit`、`/tree`、`/goto` 等）优先；未知 `/foo` 先查自定义命令，命中则展开后发送（REPL 打印 `[command /foo]`，TUI 插一行系统提示）。
- `rupi commands` 与 REPL/TUI `/commands` 列出全部自定义命令（description 取 frontmatter，无则取正文首行）。JSON-RPC 扩展注册的斜杠命令同表列出，未命中 `.md` / skill 时再走 `commands/execute`。

## 包管理（`rupi install`）

对标 `pi install`：从 npm / git / 本地路径装 skill、斜杠命令、`*.json` 扩展。
不执行 `npm install` 或包内脚本；TypeScript 扩展会跳过并提示。

```bash
rupi install git:github.com/user/repo
rupi install git:github.com/user/repo@v1
rupi install npm:@foo/bar@1.2.3
rupi install ./examples/packages/demo
rupi install -l ./vendor/pkg
rupi packages
rupi uninstall npm:@foo/bar
```

包布局：`package.json` 的 `pi.skills` / `pi.prompts` / `pi.extensions`，或约定目录
`skills/`、`prompts|commands/`、`extensions/`。物化到 `~/.rupi/skills|commands|extensions`
（`-l` 则 `.rupi/`），锁文件 `packages.json`。

仓内基准：`cargo bench -p rupi-benches`。启动脚本 `scripts/bench-startup.sh`（需 hyperfine + release 二进制）。

## Provider 与图片

```bash
# provider/model[:thinking]；--api-key 覆盖当前家的密钥
./target/debug/rupi --model anthropic/claude-sonnet-4-5:high --api-key "$KEY" chat
./target/debug/rupi --model openrouter/openai/gpt-4o-mini --provider openrouter chat
./target/debug/rupi --list-models   # 或 `rupi models`
./target/debug/rupi login anthropic # 打印 API key 环境变量，无浏览器流
```

显式路由：`openai` / `anthropic` / `gemini` / `openrouter` / `azure` / `bedrock` / `vertex`。
思考档 `off|low|medium|high|xhigh|max`（也可写在模型后缀）。Anthropic 开启思考时会抬高
`max_tokens`，避免 medium/high 因默认 4096 静默失效。

`read` 与 `@file` 支持 png/jpg/gif/webp/bmp/svg：先 `metadata` 判大小再读；文本按行分页。
图片进 `ContentBlock::Image`，各 provider 各自映射。

OpenRouter（`base_url` 含 openrouter.ai）默认带 `x-session-id`（荷载为 `sessions.db` 会话 id，`--resume` 同 id 即同一下游；`RUPI_SESSION_AFFINITY=0/1` 显式关/开）。

压实：`reserveTokens` / `keepRecentTokens` 默认 16384 / 20000；`--compress-threshold` / `--compress-keep` 或 settings `compaction`；`RUPI_COMPRESSION_OVERRIDES` 按 `provider/model` 覆盖。溢出报错强制压实 + 同 turn 重发（`MAX_OVERFLOW_RECOVERIES=2`）。

## JSON-RPC 扩展（长连接）

默认仍是 oneshot（stdin JSON → stdout）。`protocol: jsonrpc` 拉起双向 JSON-RPC 子进程
（复用 `rupi-mcp` 换行帧）：`initialize` 可注册斜杠命令、预订阅 `tool_call` / `turn_end` /
`session_*`；`tools/call` 结果可带 `ui` hint；运行中可 `registerCommand` / `subscribe` /
`ui/hint`。WASM 不在范围。示例：`examples/extensions/echo-rpc.json`。

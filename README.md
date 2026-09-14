# rupi

[`@earendil-works/pi-coding-agent`](https://github.com/earendil-works/pi) 的 Rust 复刻。循环、会话树、工具、MCP、记忆和 skill 按 crate 分层。

本机入口是 `rupi`。云入口是 `rupi-server`。两者共用 `rupi-agent`。

## Quick Start

需要 Rust 1.88+（见 [`Cargo.toml`](Cargo.toml) `rust-version`）。从源码编，没有安装包。

### 1. Build

```bash
cargo build --workspace
cargo test --workspace
```

二进制在 `target/debug/`：`rupi`、`rupi-server`、`rupi-execd`、`rupi-sandboxd`。

### 2. Run locally

无 API key 走 `MockProvider`。家目录用 `RUPI_HOME` 隔开即可。

**2.1 无 key**

```bash
export RUPI_HOME=/tmp/rupi-demo
./target/debug/rupi --help
./target/debug/rupi memory-write add "likes tea"
./target/debug/rupi memory-show
./target/debug/rupi skills-list
./target/debug/rupi skill-load commit-helper
echo "/quit" | ./target/debug/rupi --continue chat
```

**2.2 接真实模型**

```bash
# OpenAI 兼容
export RUPI_API_KEY=... RUPI_BASE_URL=https://api.openai.com/v1
./target/debug/rupi --model gpt-4o-mini chat

# Anthropic（--model claude-* 自动路由）
export RUPI_ANTHROPIC_KEY=...   # 或 ANTHROPIC_API_KEY
./target/debug/rupi --model claude-sonnet-4-5 chat

# Gemini（--model gemini-* 自动路由）
export RUPI_GEMINI_KEY=...      # 或 GEMINI_API_KEY / GOOGLE_API_KEY
./target/debug/rupi --model gemini-2.5-flash chat
```

**2.3 第一次任务**

无子命令默认进 TUI。`chat` 是 REPL。`run` 跑一轮就退出。

```bash
./target/debug/rupi --model gpt-4o-mini run --json "summarize README.md"
```

`--json` 对标 `pi --mode json`：stdout 每行一个 `AgentEvent`，末行 `run_result`。

配置写 `~/.rupi/settings.json`，项目级 `.rupi/settings.json` 后覆盖前：

```json
{
  "model": "gpt-4o-mini",
  "thinking": "low",
  "theme": "dark",
  "tools": ["read", "bash"],
  "compaction": { "reserveTokens": 16384, "keepRecentTokens": 20000 }
}
```

`rupi login` 只打印该 provider 的 API key 环境变量。没有浏览器/设备码，也没有订阅登录。

### 3. Run the cloud control plane

需要 PostgreSQL、Redis，以及至少一个外部执行节点。控制面进程里不跑用户 `sh -c`。

**3.1 起执行节点**

回环也要带 `--token`。空 token 仅 `127.0.0.1` 且必须 `--insecure`。`0.0.0.0` 禁止空 token。生产步骤见 [`docs/LAUNCH.md`](docs/LAUNCH.md)。

```bash
export RUPI_EXEC_TOKEN=...
./target/debug/rupi-execd --listen 127.0.0.1:8090 --root /tmp/rupi-execd --token "$RUPI_EXEC_TOKEN"
# 或
./target/debug/rupi-sandboxd --listen 127.0.0.1:8190 --root /tmp/rupi-sandboxd --token "$RUPI_EXEC_TOKEN"
```

URL 可写 `region=url`。两种后端可以并存。`sandboxd` 是句柄根 jail，不是微 VM。

**3.2 起控制面**

`--bootstrap-tenant` / `--bootstrap-admin` 打印一把明文 Key 后退出，给本地和 CI。正式跑服务不要带这两个 flag。

```bash
./target/debug/rupi-server \
  --listen 127.0.0.1:8080 \
  --database-url postgres://rupi:rupi@127.0.0.1:5432/rupi \
  --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 \
  --sandbox-urls http://127.0.0.1:8190 \
  --executor-token "$RUPI_EXEC_TOKEN" \
  --admin-token "$RUPI_ADMIN_TOKEN"
```

Redis Cluster：`--redis-cluster` / `REDIS_CLUSTER=1`，或 `--redis-url redis-cluster://host:port`（逗号分隔多个 seed）。

未指定 `--snapshot-dir` 时写到 `./rupi-data/snapshots`，不要依赖 `/tmp/rupi-snapshots`。多副本用 `RUPI_SNAPSHOT_URI`。

**3.3 发第一轮**

```bash
curl -sS http://127.0.0.1:8080/health
curl -sS -H "Authorization: Bearer $RUPI_CLOUD_KEY" http://127.0.0.1:8080/v1/me

./target/debug/rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 me
./target/debug/rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 prompt "hello"
```

对话是 AG-UI：`POST /v1/agent` → SSE。租户 Key 放 `Authorization: Bearer`。也可以设 `RUPI_CLOUD_KEY`。

管理面：`http://127.0.0.1:8080/admin`，凭证是 `RUPI_ADMIN_TOKEN` 或库内 `rupi_admin_*` Key。页面管租户、配额、settings、会话目录、执行池。对话仍走 `POST /v1/agent` 或 `rupi cloud prompt`。

## Highlights

| Feature | What it does |
|---|---|
| 本机 `rupi` | 无子命令默认 TUI。`chat` 是 REPL。`run` / `-p` 非交互。`--mode rpc` 是本机 JSONL。会话在 `$RUPI_HOME/sessions.db`（SQLite WAL + FTS5）。 |
| 云 `rupi-server` | 无状态控制面。会话和记忆在 PostgreSQL。Redis 只缓存，挂了读库。对话 `POST /v1/agent`。管理面 `/admin`。不链接 `rupi-tui`。 |
| 外部 Executor | bash 和会执行代码的工具打在 `rupi-execd` 或 `rupi-sandboxd`。控制面不把本机 Docker 当默认执行面。 |
| Agent loop | `AgentLoop`：流式助手消息 → 工具 → 再循环。TUI、`run`、`--mode rpc`、云 AG-UI 共用。 |
| 会话树 | `branch_from` / `rewind_to` / `/goto`。`--session` / `-r` / `--fork` / `--resume` / `--continue`。`/export` JSONL 或 HTML。 |
| 七工具 + 沙箱 | Read / Write / Edit / Bash / Glob / Grep / Think。REPL/TUI 把文件工具箍在启动 cwd。本机 `bash` 是 `sh -c`。 |
| Providers | OpenAI 兼容 / Anthropic / Gemini / OpenRouter / Azure / Bedrock / Vertex / llama.cpp（本机 OpenAI-compat 预设）。真 SSE。思考档 `off\|low\|medium\|high\|xhigh\|max`。 |
| MCP | stdio 换行 JSON-RPC，或 Streamable HTTP。`rupi mcp-list`。`prompt_snippet` 必填。 |
| 记忆 | 本机 Hermes：`MEMORY.md` / `USER.md` / `failures.md`。云权威是 Postgres 行，不是 sandbox 卷上的 markdown。 |
| Skills / 命令 / 包 | Agent Skills 渐进披露。`~/.rupi/commands/<name>.md`。`rupi install git:` / `npm:`，不跑 npm 脚本。 |
| 权限 | `--plan` 禁 write/edit/bash。危险 bash 子串转人工审批。`--approve` / `--no-approve`。 |

## Local vs cloud

| | `rupi`（本机） | `rupi-server`（云） |
|---|---|---|
| 会话 / 记忆权威 | `sessions.db` + `MEMORY.md` | PostgreSQL（`tenant_id`） |
| 缓存 | 无 Redis | Redis，可降级读库 |
| bash / 代码工具 | 本机 `sh -c` | 外部 `Executor` |
| 对话协议 | TUI / REPL / `run --json` / `--mode rpc` | AG-UI `POST /v1/agent` → SSE |
| 鉴权 | 环境变量或 `--api-key`（BYOK） | 租户 API Key；管理面独立口令 |
| UI | ratatui TUI | `/admin` 运营页。没有聊天 IDE |

`rupi cloud` 是瘦客户端：只发 HTTP，bash 在 Executor 上跑。`--mode rpc` 只留本机。

## vs Pi

对标 [`@earendil-works/pi-coding-agent` 0.85.x](https://github.com/earendil-works/pi)。逐项偏离见 [`docs/UPSTREAM.md`](docs/UPSTREAM.md)。

| Capability | Pi | rupi |
|---|---|---|
| Agent loop / 会话树 / 压实 | ✓ | ✓ |
| 默认七工具 + TUI / `run` / `--mode json` / `--mode rpc` | ✓ | ✓ |
| MCP 桥 | 扩展层 | `rupi-mcp`（stdio + Streamable HTTP） |
| Hermes 记忆 / Skill 自积累 | 扩展层 | `rupi-memory` / `rupi-skills` |
| 会话主存 | JSONL | 本机 SQLite；云 Postgres。JSONL 是进出口 |
| 云控制面 / AG-UI / `/admin` | — | `rupi-server` |
| 订阅 OAuth（Claude Pro/Max、Codex、Copilot） | 部分有 | 无。`rupi login` 只打印 key 用法 |

## Docs

| 文档 | 内容 |
|---|---|
| [`docs/LAUNCH.md`](docs/LAUNCH.md) | 云上线：真实 flag、默认安全值、回环与对外听 |
| [`docs/UPSTREAM.md`](docs/UPSTREAM.md) | 上游对照、有意偏离、未移植 |
| [`BRANCH_COMPARISON.md`](BRANCH_COMPARISON.md) | 三分支历史评审 |
| [`crates/*/README.md`](crates/) | 各 crate 职责与命令 |
| [AG-UI](https://docs.ag-ui.com/introduction) | 云对话事件与 `RunAgentInput` |

仓内基准：`cargo bench -p rupi-benches`。启动耗时：`scripts/bench-startup.sh`（hyperfine + release）。没有对外公布的竞品对比数字。

## Crate

| crate | 职责 |
|---|---|
| [`rupi-core`](crates/rupi-core/README.md) | `Message` / `SessionTree` / `AgentEvent` / `ToolDefinition`。无 LLM、无 IO。 |
| [`rupi-agent`](crates/rupi-agent/README.md) | `AgentLoop`、权限门、subagent。 |
| [`rupi-llm`](crates/rupi-llm/README.md) | `LlmProvider` + 真 SSE。 |
| [`rupi-tools`](crates/rupi-tools/README.md) | 七工具与 cwd 沙箱。 |
| [`rupi-mcp`](crates/rupi-mcp/README.md) | MCP 桥。 |
| [`rupi-memory`](crates/rupi-memory/README.md) | 本机 Hermes 记忆 + `SessionStore`。 |
| [`rupi-skills`](crates/rupi-skills/README.md) | Skill 发现与 `skill-distill`。 |
| [`rupi-ext`](crates/rupi-ext/README.md) | 进程扩展，oneshot 或 JSON-RPC。 |
| [`rupi-config`](crates/rupi-config/README.md) | `settings.json` 合并。 |
| [`rupi-pkg`](crates/rupi-pkg/README.md) | `rupi install` / `uninstall` / `packages`。 |
| [`rupi-tui`](crates/rupi-tui/README.md) | ratatui。云不链接。 |
| [`rupi-cli`](crates/rupi-cli/README.md) | `rupi` 与 `rupi cloud`。 |
| [`rupi-runtime`](crates/rupi-runtime/README.md) | `Executor`、`rupi-execd`、`rupi-sandboxd`。 |
| [`rupi-server`](crates/rupi-server/README.md) | 云控制面 + `/admin`。 |
| [`rupi-benches`](crates/rupi-benches/README.md) | Criterion。 |

## Reference

下面是本机路径和开关，细节在对应 crate README。

**CLI 常用 flag。** `--session` / `-r` / `--fork` / `--resume` / `--continue` 续聊；`--no-session` 不落盘。位置参数 `@file`。`--tools` / `--exclude-tools` / `--no-tools` / `--no-builtin-tools`。`--no-context-files` 跳过 `AGENTS.md` / `CLAUDE.md`。`--plan`、`--subagents`、`--parallel-tools`、`--thinking`。状态栏：`↑↓ tokens / context% / $`。

**模型。** `--model provider/model[:thinking]`。`rupi models` / `--list-models`。显式路由：`openai` / `anthropic` / `gemini` / `openrouter` / `azure` / `bedrock` / `vertex` / `llamacpp`（本机 `127.0.0.1:8080/v1`，无 key 也可）。压实默认 reserve 16384 / keep 20000；`--compress-threshold` / `--compress-keep` 或 settings `compaction`。

**MCP。**

```bash
./target/debug/rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
```

配置数组里给 `url` 即走 Streamable HTTP。工具名 `{server}_{name}`。`resources/list→read`、`prompts/list→get` 各生成一个 per-server 工具。

**记忆。** `~/.rupi/memories/MEMORY.md` / `USER.md` / `failures.md`，各 5000 字符。`[core]` 行固定注入；有 `[core]` 时其余走 `memory_search`。`--no-memory` 撤本轮内建记忆。密钥子串拒写。云权威是 Postgres，不是卷上的 `MEMORY.md`。

**Skills。** 发现顺序：`skills/builtin` → `~/.rupi/skills` → 项目 `.rupi/skills` → `~/.pi/agent/skills` → `~/.agents/skills` → 上溯 `.pi/skills` / `.agents/skills`。`skill-distill` 写草稿。`--review-apply` 才落盘；`--review-llm` 多一次 LLM 调用。

**斜杠命令。** `~/.rupi/commands/<name>.md`。占位符 `$ARGUMENTS` / `$1` / `{{var}}`。

**包。**

```bash
rupi install git:github.com/user/repo
rupi install npm:@foo/bar@1.2.3
rupi install ./examples/packages/demo
rupi packages
rupi uninstall npm:@foo/bar
```

不执行 `npm install` 或包内脚本。

**扩展。** 默认 oneshot。`protocol: jsonrpc` 长连接。示例：`examples/extensions/echo-rpc.json`。WASM 不在范围。

## License

`MIT OR Apache-2.0`。见根 [`Cargo.toml`](Cargo.toml)。

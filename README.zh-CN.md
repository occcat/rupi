<div align="center">

# rupi

**Pi coding agent 的 Rust 复刻：内置 MCP、持久记忆、Skill 自积累与云控制面**

<p>
  <a href="https://github.com/occcat/rupi/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/occcat/rupi/ci.yml?branch=main&style=for-the-badge&logo=github&label=CI" alt="CI" /></a>
  <img src="https://img.shields.io/badge/Rust-1.88%2B-B7410E?style=for-the-badge&logo=rust&logoColor=white" alt="Rust 1.88+" />
  <a href="https://github.com/earendil-works/pi"><img src="https://img.shields.io/badge/Upstream-Pi%200.85.x-6E40C9?style=for-the-badge" alt="Upstream Pi 0.85.x" /></a>
  <a href="#许可证"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-3DA639?style=for-the-badge" alt="License MIT OR Apache-2.0" /></a>
</p>

<p>
  <a href="README.md">English</a> ·
  <b>简体中文</b>
</p>

</div>

rupi 用 Rust 重新实现 [`@earendil-works/pi-coding-agent`](https://github.com/earendil-works/pi) 0.85.x，沿用 Pi 的设计：很小的 agent 循环、树形会话、少量文件与 shell 工具。Pi 留给扩展层的部分在这里是独立 crate：MCP 桥、Hermes 风格记忆，以及 agent 能从自己的工作中提炼出来的 skill。

所有前端共用同一个 `AgentLoop`。本机入口是 `rupi`：全屏 TUI、行式 REPL、带 JSONL 事件流的一次性 `run`，以及供嵌入的 JSONL RPC 模式。云端入口是 `rupi-server`：无状态的多租户控制面，对外讲 [AG-UI](https://docs.ag-ui.com/introduction)（SSE），所有 shell 命令都发给外部执行节点。

## 目录

- [快速上手](#快速上手)
- [特性](#特性)
- [与 Pi 对比](#与-pi-对比)
- [使用](#使用)
- [配置](#配置)
- [云控制面](#云控制面)
- [Workspace 结构](#workspace-结构)
- [文档索引](#文档索引)
- [开发](#开发)
- [许可证](#许可证)

## 快速上手

rupi 在 macOS 和 Linux 上从源码构建。CI 覆盖 Ubuntu 与 macOS，工具链为 stable 和最低支持版本 1.88。Windows 下 `bash` 工具改走 PowerShell，CI 未覆盖。

### 1. 安装

```bash
git clone https://github.com/occcat/rupi.git
cd rupi

# 方式 A：就地构建，二进制在 target/release/
cargo build --release -p rupi-cli

# 方式 B：把 `rupi` 装进 PATH（~/.cargo/bin）
cargo install --locked --path crates/rupi-cli
```

云端二进制（`rupi-server`、`rupi-execd`、`rupi-sandboxd`）用 `cargo build --release -p rupi-server` 构建，只有部署[云控制面](#云控制面)时才需要。

### 2. 接入模型

没有任何 API key 时，rupi 退回内置的 mock provider，并在 stderr 打印提示。这足够试用各个命令，但回复都是固定文本。

```bash
# OpenAI 或任意 OpenAI 兼容网关
export RUPI_API_KEY=sk-...                      # 或 OPENAI_API_KEY
export RUPI_BASE_URL=https://api.openai.com/v1  # 或 OPENAI_BASE_URL
rupi --model gpt-4o-mini

# Anthropic（claude-* 模型自动路由）
export ANTHROPIC_API_KEY=...                    # 或 RUPI_ANTHROPIC_KEY
rupi --model claude-sonnet-4-5

# Gemini（gemini-* 模型自动路由）
export GEMINI_API_KEY=...                       # 或 RUPI_GEMINI_KEY / GOOGLE_API_KEY
rupi --model gemini-2.5-flash

# 本机 llama.cpp，无需 key（默认 http://127.0.0.1:8080/v1）
rupi --model llamacpp/qwen2.5-coder
```

`rupi login [provider]` 打印该 provider 读取的环境变量。没有浏览器或设备码登录。

### 3. 第一个任务

```bash
cd your-project

rupi                                   # 全屏 TUI（默认）
rupi chat                              # 行式 REPL
rupi -p "summarize README.md"          # 一次性执行，打印回答后退出
rupi run --json "list the TODOs"       # 一次性执行，每行一个 JSON AgentEvent
rupi @src/main.rs "explain this file"  # 用 @path 附带文件
```

第一次在自带 `.rupi/` 记忆、skill 或命令（或 Pi / Agent Skills 目录）的项目里运行时，rupi 会询问是否信任该项目。每一轮都存进 `~/.rupi/sessions.db`，`rupi --continue` 可以接着上次聊。

## 特性

| 特性 | 说明 |
|---|---|
| **一个循环，所有前端** | TUI、`chat` REPL、`run`、`--mode rpc` 和云端 `POST /v1/agent` 都跑同一个 `AgentLoop`：流式输出助手消息，执行工具调用，直到不再调用工具。Esc 或 Ctrl-C 可在流中取消，并杀掉正在运行的 shell 进程组。运行中按 Enter 转向当前轮，Alt+Enter 排队下一轮。 |
| **会话是树** | 每条消息都是 SQLite（WAL + FTS5）里的一个节点，工具调用和结果也在内。`/tree`、`/goto`、`/rewind`、`/fork`、`/clone` 在历史中移动或分叉。`--resume` 恢复完整的工具上下文。`/export` 写出 Pi 的 JSONL v3 格式或独立 HTML 页，`/import` 与 `--session file.jsonl` 导入 Pi 会话。 |
| **按 token 压实** | 用量超过 `context_window - reserveTokens` 时，由模型摘要较早的历史，保留最近 `keepRecentTokens`。摘要附带读过和改过的文件清单。provider 仍报上下文溢出时，rupi 压实后重发，最多两次。 |
| **带沙箱的编码工具** | `read`、`write`、`edit`、`bash`、`glob`、`grep`、`think`，另有可选的 `find` 和 `ls`。`edit` 要求唯一匹配，支持多处编辑并返回 diff。交互模式下文件工具限定在启动目录内，拒绝 `..`、绝对路径和符号链接逃逸。`bash` 输出保留最后 2000 行或 50 KB。 |
| **多 provider，真流式** | OpenAI 兼容、Anthropic Messages、Gemini `generateContent`，另有 OpenRouter、Azure OpenAI、Bedrock、Vertex AI、llama.cpp 预设。三种协议都走 SSE 流式，支持工具调用、图片、用量统计和六档思考强度（`off` 到 `max`）。429 和 5xx 自动退避重试。 |
| **内置 MCP** | 通过 stdio、Streamable HTTP 或 WebSocket 连接 MCP server。工具注册为 `{server}_{tool}`，resources 和 prompts 各生成一个 per-server 工具。桥会应答 `roots/list`、`ping` 等反向请求，并在轮次之间处理 `tools/list_changed`。 |
| **持久记忆** | Hermes 风格的 `MEMORY.md`、`USER.md`、`failures.md`，分全局和项目两层。系统提示里注入冻结快照，保住 prefix cache。带 `[core]` 的行始终注入，其余按需检索。疑似密钥的内容拒绝写入。 |
| **Skill 自积累** | 从 rupi、Pi 和 `.agents` 目录发现 [Agent Skills](https://agentskills.io)，渐进披露：先给索引，按需加载完整 `SKILL.md`，再按需读资源。每轮结束后的复盘会建议记忆和 skill 草稿，`--review-apply` 落盘，`rupi skill-distill` 把步骤清单写成新 skill。 |
| **不重新编译也能扩展** | Markdown 文件即斜杠命令；`rupi install git:` / `npm:` / 本地包（skill、prompt、扩展、主题，从不运行包脚本）；进程扩展支持一次性或 JSON-RPC 协议，可注册工具、命令、快捷键和 provider。manifest 改动后自动热重载。 |
| **云控制面** | `rupi-server` 把会话和记忆放在 PostgreSQL，Redis 只做缓存，对外提供 AG-UI（SSE）和 `/admin` 运营台。用户命令只在 `rupi-execd` 或 `rupi-sandboxd` 节点上跑，从不在 API 进程里执行。 |

## 与 Pi 对比

Pi 的核心很小，大部分功能交给扩展。rupi 在 crate 层面保持同样的切分，并把常用扩展做成内置。[`docs/UPSTREAM.md`](docs/UPSTREAM.md) 逐个子系统对照上游，并列出有意的偏离。

| 能力 | Pi | rupi |
|---|:---:|:---:|
| Agent 循环、会话树、压实 | ✓ | ✓ |
| 默认工具、TUI、print / JSON / RPC 模式 | ✓ | ✓ |
| `settings.json`、`AGENTS.md`、`SYSTEM.md`、自定义 prompt | ✓ | ✓（也只读加载 `~/.pi/agent` 和 `.pi`） |
| 会话存储 | JSONL 文件 | SQLite；JSONL 用于导入导出 |
| MCP 桥 | 扩展 | 内置（`rupi-mcp`） |
| 持久记忆 | 扩展 | 内置（`rupi-memory`） |
| Skill 自积累 | 扩展 | 内置（`rupi-skills` + 复盘） |
| 文件工具路径沙箱 | — | ✓ |
| 权限门与计划模式 | 扩展 | 内置（`--plan`、审批问询） |
| 进程内 TypeScript 扩展 | ✓ | —（改用 JSON-RPC 进程扩展） |
| 订阅 OAuth 登录 | 部分 provider | —（只支持 API key） |
| 多租户云控制面 | — | `rupi-server` |

## 使用

### 交互方式

| 命令 | 用途 |
|---|---|
| `rupi` / `rupi tui` | 全屏 ratatui 界面：Markdown 与代码高亮、可折叠的工具和思考块、Unicode 绘制的 Mermaid 图、Kitty 协议终端内联图片、会话选择器、树导航器，以及内置文件编辑器（`/edit`、Ctrl+E）。 |
| `rupi chat` | 纯行式 REPL，适合 SSH 和管道。 |
| `rupi run "..."` / `rupi -p "..."` | 执行一次后退出。管道输入的 stdin 会拼到 prompt 后面。审批问询默认拒绝，加 `--approve` 放行。 |
| `rupi run --json "..."` | 同上，但 stdout 每行一个 `AgentEvent`，末行是 `run_result`（会话 id、停止原因、最终文本）。对标 `pi --mode json`。 |
| `rupi --mode rpc` | stdin 收 JSONL 命令（`prompt`、`steer`、`follow_up`、`abort`、`get_state`、`set_model`、`fork` 等），stdout 输出响应和事件，用于把 rupi 嵌进其他程序。 |

### 会话

```bash
rupi sessions                     # 最近的会话
rupi session-show <id>            # 查看单个会话
rupi session-search "keyword"     # 跨会话全文检索
rupi --continue                   # 最近一次会话，优先当前目录
rupi --resume <id>                # 指定会话
rupi -r                           # 交互式选择会话
rupi --fork <id|file.jsonl>       # 复制为新 id 后继续
rupi --export <id> out.html       # 导出后退出（.jsonl 为 Pi 格式）
rupi --no-session                 # 不落盘
```

### 斜杠命令

下表命令在 TUI 中可用，大部分在 `chat` REPL 中也可用。

| 命令 | 作用 |
|---|---|
| `/tree`、`/goto <id>`、`/rewind [id]` | 查看会话树、跳到节点、回退 |
| `/fork`、`/clone`、`/new`、`/name <name>` | 分叉为新会话、复制整棵树、新建、改名 |
| `/sessions`、`/resume <id>` | 选择或切换会话 |
| `/export [file]`、`/import <file>`、`/share` | 导出 JSONL 或 HTML、导入 Pi JSONL、以秘密 gist 分享 |
| `/compact [指令]` | 立即压实，可附带自定义摘要指令 |
| `/model [name]`、`/thinking [level]` | 本会话内切换模型或思考强度 |
| `/plan` | 切换计划模式（只读：禁止 `write`、`edit`、`bash`） |
| `/settings`、`/reload` | 就地修改设置；重载扩展、skill、prompt 和主题 |
| `/skills`、`/commands`、`/<skill-name> args` | 列出 skill 和自定义命令；把 skill 当命令执行 |
| `/edit [path]`、`/copy`、`/hotkeys`、`/trust`、`/help`、`/quit` | 仅 TUI：编辑文件、复制上一条回复、查看快捷键、管理项目信任 |

自定义命令是 `~/.rupi/commands/` 或 `.rupi/commands/` 下的 Markdown 文件（Pi 的 `prompts/` 目录也可以）。`/name args` 展开文件正文，并替换 `$ARGUMENTS`、`$1` 或 `{{var}}`。

### 工具与权限

`--tools read,bash`、`--exclude-tools`、`--no-tools`、`--no-builtin-tools` 决定模型能看到哪些工具。`--plan` 以只读计划模式启动。匹配黑名单（`rm -rf /`、`mkfs`、`dd if=`）的 `bash` 命令会停下来等待审批，回答 `a` 表示本会话内同类调用都放行。脚本场景用 `--approve` 或 `--no-approve` 预先决定所有问询。`--parallel-tools` 并发执行互不依赖的工具调用，结果保持原顺序。`--subagents` 允许模型把子任务派给分叉出的子会话。

### Provider 与模型

用 `--model name` 或 `--model provider/model[:thinking]`，例如 `anthropic/claude-sonnet-4-5:high`。不带前缀时，`claude-*` 走 Anthropic，`gemini-*` 走 Gemini，其余走 OpenAI 兼容客户端。

| Provider | Key 环境变量 | Base URL 覆盖 |
|---|---|---|
| `openai` | `RUPI_API_KEY`、`OPENAI_API_KEY` | `RUPI_BASE_URL`、`OPENAI_BASE_URL` |
| `anthropic` | `RUPI_ANTHROPIC_KEY`、`ANTHROPIC_API_KEY` | `RUPI_ANTHROPIC_BASE` |
| `gemini` | `RUPI_GEMINI_KEY`、`GEMINI_API_KEY`、`GOOGLE_API_KEY` | `RUPI_GEMINI_BASE` |
| `openrouter` | `RUPI_OPENROUTER_KEY`、`OPENROUTER_API_KEY` | `RUPI_OPENROUTER_BASE` |
| `azure` | `AZURE_OPENAI_API_KEY`、`RUPI_AZURE_KEY` | `AZURE_OPENAI_ENDPOINT`（必填） |
| `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`、`BEDROCK_API_KEY` | `RUPI_BEDROCK_BASE` 或 `AWS_REGION` |
| `vertex` | `VERTEX_TOKEN`、`GOOGLE_OAUTH_ACCESS_TOKEN` | `GOOGLE_CLOUD_PROJECT`、`VERTEX_LOCATION` |
| `llamacpp` | 可选 `RUPI_LLAMACPP_KEY` | `RUPI_LLAMACPP_BASE` |

`--api-key` 和 `--provider` 优先于上述变量。`rupi models` 列出内置模型目录，可用 `~/.rupi/models.json` 或 `RUPI_MODELS` 扩充。

### MCP server

```bash
# 探活：列出 server 的 tools、resources、prompts
rupi mcp-list python3 crates/rupi-mcp/tests/fake_mcp_server.py
rupi mcp-list --url http://127.0.0.1:8000/mcp

# 在会话中使用
cat > mcp.json <<'EOF'
[
  { "name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."], "env": {} },
  { "name": "remote", "command": "", "args": [], "env": {}, "url": "https://example.com/mcp" }
]
EOF
rupi --mcp-config mcp.json
```

`url` 以 `ws://` 或 `wss://` 开头时走 WebSocket，其余 `url` 走 Streamable HTTP。

### 记忆

```bash
rupi memory-write add "prefers small focused commits"
rupi memory-write add "[core] answer in English" --scope global
rupi memory-write add "uses pnpm, not npm" --scope project
rupi memory-show
rupi memory-search "pnpm"
```

文件在 `~/.rupi/memories/`（全局）和 `<repo>/.rupi/MEMORY.md`（项目），每个最多 5000 字符，超出时丢弃最旧的行。会话内模型通过 `memory` 工具写入，用 `memory_search` 和 `session_search` 回忆。`--no-memory` 在单次运行中关闭内建记忆，`--memory-provider jsonl` 接入外部 provider。

### Skill、包与扩展

```bash
rupi skills-list
rupi skill-load <name>
rupi skill-distill release-notes "Draft release notes from git log" "collect commits since last tag" "group by type"

rupi install git:github.com/user/repo        # 也支持 npm:@scope/pkg@1.2.3、https/ssh URL、本地路径
rupi install ./examples/packages/demo
rupi packages
rupi uninstall git:github.com/user/repo
rupi config list                             # 启停 packages、extensions、skills、prompts、themes

rupi --ext-dir examples/extensions ext-list
```

Skill 按以下顺序发现，先发现者优先：内置 `skills/builtin`、`~/.rupi/skills`、`.rupi/skills`、`~/.pi/agent/skills`、`~/.agents/skills`，然后从工作目录向上直到仓库根查找 `.pi/skills` 和 `.agents/skills`。

扩展由一个 JSON manifest 和一条命令组成。`"protocol": "oneshot"`（默认）时参数写入 stdin，stdout 即结果；`"protocol": "jsonrpc"` 时进程常驻，可以注册命令、快捷键、provider 和事件订阅。示例见 [`examples/extensions/`](examples/extensions/)。

## 配置

设置按以下顺序合并，后者覆盖前者：`~/.rupi/settings.json`，然后从工作目录向上的 `.rupi/settings.json`。Pi 的 `~/.pi/agent/settings.json` 和 `.pi/settings.json` 也会读取，同名键以 rupi 自己的文件为准。

```json
{
  "model": "anthropic/claude-sonnet-4-5",
  "thinking": "medium",
  "theme": "dark",
  "tools": ["read", "write", "edit", "bash", "grep", "find", "ls"],
  "compaction": { "reserveTokens": 16384, "keepRecentTokens": 20000 },
  "steeringMode": "one-at-a-time",
  "defaultProjectTrust": "ask",
  "enabledModels": ["anthropic/claude-sonnet-4-5", "openai/gpt-4o"]
}
```

`~/.rupi`（或 `$RUPI_HOME`）下的其他文件：

| 路径 | 用途 |
|---|---|
| `sessions.db` | 所有会话（`--session-dir` 或 `RUPI_SESSION_DIR` 可改位置） |
| `memories/` | `MEMORY.md`、`USER.md`、`failures.md` |
| `skills/`、`commands/`、`prompts/`、`extensions/`、`themes/` | 用户资源；`rupi install` 写到这里 |
| `SYSTEM.md`、`APPEND_SYSTEM.md` | 替换或追加系统提示（也可用 `--system-prompt`、`--append-system-prompt`） |
| `keybindings.json` | TUI 快捷键覆盖 |
| `models.json`、`providers.json` | 额外的模型和 provider |

项目里可以放 `AGENTS.md` 或 `CLAUDE.md`（从全局目录和工作目录的每一级祖先加载；`--no-context-files` 跳过），以及结构相同的 `.rupi/` 目录。设置 `RUPI_HOME` 可以使用独立的家目录，演示和测试时很方便。

## 云控制面

云端部署由四部分组成：

- PostgreSQL：会话、记忆、配额和审计记录的权威存储
- Redis：只做缓存，Redis 挂掉时服务继续从 PostgreSQL 读取
- 一个或多个执行节点：`rupi-execd`，或带隔离的 `rupi-sandboxd`
- `rupi-server`：无状态，可以多副本运行

最小回环部署：

```bash
cargo build --release -p rupi-server
export RUPI_EXEC_TOKEN=change-me RUPI_ADMIN_TOKEN=change-me-too
export DATABASE_URL=postgres://rupi:rupi@127.0.0.1:5432/rupi

./target/release/rupi-server --database-url "$DATABASE_URL" --bootstrap-tenant demo   # 打印租户 key 后退出
./target/release/rupi-execd --listen 127.0.0.1:8090 --root ./rupi-data/execd --token "$RUPI_EXEC_TOKEN"
./target/release/rupi-server --listen 127.0.0.1:8080 \
  --database-url "$DATABASE_URL" --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 --executor-token "$RUPI_EXEC_TOKEN" \
  --admin-token "$RUPI_ADMIN_TOKEN"

# 用瘦客户端对话
rupi --api-key "$RUPI_CLOUD_KEY" cloud --url http://127.0.0.1:8080 prompt "hello"
```

对话走 AG-UI：`POST /v1/agent`，请求体为 `RunAgentInput`，返回 SSE 流。`/admin` 运营台管理租户、配额、设置、会话和执行池。默认值从严：非回环监听必须配 TLS（`--tls-cert`/`--tls-key`）或显式 `--insecure`，执行节点 token 为空时拒绝启动。全部 flag 和上线检查项见 [`docs/LAUNCH.md`](docs/LAUNCH.md)。

## Workspace 结构

| Crate | 职责 |
|---|---|
| [`rupi-core`](crates/rupi-core/README.md) | 消息、会话树、事件、工具与扩展 trait。不依赖 LLM，不做 I/O。 |
| [`rupi-llm`](crates/rupi-llm/README.md) | `LlmProvider` trait、三种协议、路由、SSE、模型目录 |
| [`rupi-tools`](crates/rupi-tools/README.md) | 内置工具与路径沙箱 |
| [`rupi-agent`](crates/rupi-agent/README.md) | `AgentLoop`、压实、权限、subagent、复盘 |
| [`rupi-mcp`](crates/rupi-mcp/README.md) | MCP 桥（stdio、HTTP、WebSocket） |
| [`rupi-memory`](crates/rupi-memory/README.md) | 记忆文件与 SQLite 会话库 |
| [`rupi-skills`](crates/rupi-skills/README.md) | Skill 发现与提炼 |
| [`rupi-ext`](crates/rupi-ext/README.md) | 进程扩展（一次性与 JSON-RPC） |
| [`rupi-config`](crates/rupi-config/README.md) | `settings.json` 合并与资源过滤 |
| [`rupi-pkg`](crates/rupi-pkg/README.md) | `rupi install` / `uninstall` / `packages` |
| [`rupi-tui`](crates/rupi-tui/README.md) | ratatui 界面 |
| [`rupi-cli`](crates/rupi-cli/README.md) | `rupi` 二进制与 `rupi cloud` 瘦客户端 |
| [`rupi-runtime`](crates/rupi-runtime/README.md) | Executor trait、`rupi-execd`、`rupi-sandboxd` |
| [`rupi-server`](crates/rupi-server/README.md) | 云控制面与 `/admin` |
| [`rupi-benches`](crates/rupi-benches/README.md) | Criterion 基准（不发布） |

## 文档索引

| 文档 | 内容 |
|---|---|
| [`docs/UPSTREAM.md`](docs/UPSTREAM.md) | 逐子系统对照 Pi、有意偏离、未移植项 |
| [`docs/LAUNCH.md`](docs/LAUNCH.md) | 云部署：全部真实 flag、安全默认值、回环与对外监听、容量演练 |
| [`AGENTS.md`](AGENTS.md) | 面向贡献者和编码 agent 的项目原则与规范 |
| [`BRANCH_COMPARISON.md`](BRANCH_COMPARISON.md) | 三个初始移植分支的历史评审（2026-09） |
| [`crates/*/README.md`](crates/) | 各 crate 的职责与命令 |
| [`examples/`](examples/) | 扩展示例与可安装的示例包 |

## 开发

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --exclude rupi-benches
cargo test --workspace --locked --exclude rupi-benches
cargo bench -p rupi-benches                       # 基准
./scripts/bench-startup.sh                        # 冷启动耗时，需要 hyperfine 和 release 构建
```

`rupi-server` 的集成测试通过 `DATABASE_URL` 和 `REDIS_URL` 连接 PostgreSQL 与 Redis，缺失时自动跳过。MCP 和扩展测试需要 `python3`。提交改动前请先读 [`AGENTS.md`](AGENTS.md)。

## 许可证

[MIT 或 Apache-2.0](Cargo.toml) 双许可，任选其一。

# rupi 三分支对比评审

日期：2026-09-11 · 评审对象：`origin/thoxvi/rupi-pi-agent-9492`、`origin/thoxvi/rupi-pi-agent-port-23a6`、`origin/thoxvi/rust-pi-agent-2c40`
三个分支都是 Pi coding agent（`@earendil-works/pi-coding-agent` 0.85.x）的 Rust 复刻，目标一致：最小 agent loop + MCP + Hermes 风格记忆 + Skill 自积累。

> **后续（同日）**：`rupi-pi-agent-port-23a6` 已 fast-forward 合并进 `main`；下文列出的 23a6 前四个缺陷（REPL 遇错退出、`edit` 无唯一性校验、错误体被丢弃、会话不存工具消息）已修复，并搬运了 2c40 的 `truncate.rs` / SSE 解析器 / `run --json` / usage，借鉴了 9492 的 `[core]` 记忆分层与上游对照文档。变更清单见 [`docs/UPSTREAM.md`](docs/UPSTREAM.md) 末节。

## 结论

**推荐以 `rupi-pi-agent-port-23a6` 为主线继续。** 它是唯一一个在"能接真实模型 + 流式 + 会话续聊 + 权限/沙箱 + MCP 全功能 + TUI"上都跑通的分支，255 个测试全部通过，也是三者中唯一一个在 provider 出错后不会污染会话的实现。它的主要短板（chat 模式遇错退出、edit 无唯一性校验、错误体被丢弃、会话持久化不存工具消息）都是局部修补，不影响骨架。

`rust-pi-agent-2c40` 是对 Pi 核心最忠实的移植（SSE 解析器、多处 edit + diff、Pi 默认截断参数、`--mode json/rpc`），有几个模块值得直接搬到 23a6，但它有一个实测复现的致命缺陷：任何一次 provider 错误都会把空 assistant 消息写进会话，之后 `--continue` 永远 400。默认开启的"后台"review 其实是同步的第二次 LLM 调用，把每轮延迟从约 2 秒拉到约 14 秒。

`rupi-pi-agent-9492` 骨架最干净、crate 边界最清楚，但作为产品是不可用的：CLI 从未接入真实 provider（`options.live` 恒为 `None`），`-p` 在 20 毫秒内返回空文本；"并行工具"实为顺序 await；bash 输出截断按字节切片会在多字节字符处 panic。

| 维度（1–10） | 9492 | 23a6 | 2c40 |
|---|---:|---:|---:|
| 实现完整度 | 3 | **9** | 6 |
| 性能 | 3 | **7** | 5 |
| 功能完善 / 健壮性 | 3 | **6** | 3 |
| 设计优雅度 | 6 | 6 | **6** |
| 测试覆盖 | 5 | **9** | 3 |

## 评审方法

1. 三个分支在 `.worktree/` 下各建 worktree（已加入 `.gitignore`，同时忽略 `.env`）。
2. `cargo build --release` 全部成功、零编译警告；`cargo test --workspace` 全部通过；`cargo clippy --all-targets` 三者各 23–30 条建议级警告、无错误。
3. 我通读了三个分支的 agent loop、LLM provider、bash/edit 工具、CLI 装配代码，并派三个子 agent 逐文件通读全部约 3.5 万行源码，交叉核对。
4. 用 `.env` 里的网关做真实调用。注意：`.env` 中 `MODEL=deepseek-flash` 已失效，该 key 实际可用 `deepseek-v4-flash` / `deepseek-v4-pro` / `kimi-k2.7-code`，测试时用 `deepseek-v4-flash` 覆盖（未修改 `.env`）。
5. `rust-pi-agent-2c40` 把工具链钉在 1.88.0（本机未安装），改用 `cargo +stable`（1.96.1）编译，无兼容问题。MCP 端到端测试依赖 `python3`。

### 各分支如何接 `.env`

| 分支 | 用法 |
|---|---|
| 9492 | 无法接入。没有 base URL 的入口（无 flag、无环境变量），且 `main.rs` 从不构造真实 provider |
| 23a6 | `RUPI_BASE_URL=$BASE_URL RUPI_API_KEY=$API_KEY rupi --model deepseek-v4-flash run "..."`（也接受 `OPENAI_BASE_URL` / `OPENAI_API_KEY`） |
| 2c40 | `RUPI_PROVIDER=openai-compat RUPI_BASE_URL=$BASE_URL RUPI_API_KEY=$API_KEY RUPI_MODEL=deepseek-v4-flash rupi -p "..."`（或用 `--provider/--base-url/--api-key/--model`） |

两者缺 key 时都会静默降级到 mock/faux provider，表面看似正常，需留意 stderr 的提示。

## 编译与体量

| | 9492 | 23a6 | 2c40 |
|---|---:|---:|---:|
| crate 数 | 6 | 10 | 6 |
| 源码行数（不含测试） | 6 338 | 12 021 | 7 509 |
| 测试行数 | 1 068 | 7 822 | 487 |
| 测试通过数 | 44 | 255 | 20 |
| release 编译耗时 | 2m15s | 2m32s | 2m16s |
| 二进制体积 | 7.2 MB | 12 MB | 9.1 MB |
| 非测试代码 `unwrap/expect` | 22 | 57 | 27 |
| `#[allow(dead_code)]` / `todo!` | 1 | 0 | 9 |
| 依赖包数（Cargo.lock） | 235 | 273 | 235 |
| clippy 警告 | 23 | 30 | 24 |

## 功能矛阵

✓ 实现且验证 · △ 部分实现或有明显缺口 · ✗ 缺失

| 能力 | 9492 | 23a6 | 2c40 |
|---|:-:|:-:|:-:|
| CLI 接真实模型 | ✗ 从未接线 | ✓ | ✓ |
| 流式输出（SSE） | ✗ 非流式 | ✓ | ✓ |
| 429/5xx 退避重试 | ✗ | ✓ 含 Retry-After | ✓ |
| Provider 覆盖 | OpenAI-compat、Anthropic（均非流式）；Gemini 路由错到 OpenAI | OpenAI-compat、Anthropic（thinking 签名回放、prompt caching）、Gemini | OpenAI-compat、Anthropic、Google；`--thinking` 解析后未使用 |
| 并行工具执行 | ✗ 伪并行（顺序 await） | ✓ `--parallel-tools`，结果保序 | ✓ 默认 `join_all` |
| 取消（Ctrl-C / Esc） | ✗ | ✓ 流中、工具间隙、进程组真抢占 | ✗ abort 通道未接线 |
| Compaction | 抽取式，切点不看角色 | LLM 摘要 + 文件足迹 + 溢出恢复重发 | LLM 摘要，不落盘，阈值后每轮重摘要 |
| 会话持久化 / 续聊 | JSONL 树，无 resume | SQLite + `--resume`、`/tree` `/goto` `/rewind`；不存工具消息 | JSONL v3 + `--continue`；无分支 |
| 错误轮不污染会话 | — | ✓ 实测 | ✗ 实测复现 400 |
| `edit` 唯一性校验 | ✓ | ✗ 静默替换首个匹配 | ✓ 多处编辑 + overlap 检查 + diff |
| `bash` 超时 / 进程组 | 超时 ✓；只 kill 子进程；先读 stdout 再读 stderr 会死锁 | 进程组 SIGKILL，取消返回部分输出 | 进程组 SIGKILL；无默认超时 |
| 路径沙箱 | △ `..` 可绕过 | ✓ canonicalize；悬空符号链接可逃逸 | ✗ 实测可读 `/etc/hosts` |
| 权限 / 审批 / 计划模式 | △ 仅环境变量 | ✓ policy + approver + `--plan` | ✗ |
| MCP stdio | △ 无 id 匹配，任何通知即失败 | ✓ id 路由、30s 超时、roots/ping、list_changed | ✓ id 路由；stderr 不排空；无超时 |
| MCP Streamable HTTP | △ | ✓ session-id、GET 常驻流 | △ 丢弃 session-id |
| MCP resources / prompts | ✗ | ✓ | ✗ |
| 记忆 | MEMORY/USER/PROJECT + `[core]` + FTS5 | MEMORY/USER/failures + 项目域 + FTS5 trigram + 外部 provider | MEMORY/USER + FTS5 |
| Skills | 发现 + skill_manage + 启发式积累 | 忽略文件 + 热重载 + distill + 启发式/LLM review | view/list/manage + LLM review（同步） |
| 子 agent | ✓ 只读工具集 | ✓ `--subagents`；绕过 policy | ✗ |
| 外部扩展 | ✗ | ✓ manifest + 外部进程 + 热重载 | ✗ |
| 交互界面 | stdin REPL | REPL + ratatui TUI（Tab 补全、@path、会话选择） | rustyline REPL、`--mode json`、`--mode rpc` |
| 自定义斜杠命令 | ✗ | ✓ `commands/*.md` | ✗ |

## 真实网关实测

工作目录是一个含 `hello.txt`、`main.rs`、`README.md` 的小项目，每个分支独立 `RUPI_HOME`。

| 任务 | 9492 | 23a6 | 2c40（review 关） | 2c40（默认 review 开） |
|---|---:|---:|---:|---:|
| 回复 PONG | 0.02s，空文本，未发请求 | 2.1–2.5s | 1.6–2.3s | 13.8s |
| read 工具读文件 | — | 4.1–4.8s | 3.6–3.8s | 10.1s |
| edit 替换并确认 | — | 8.1s（read→edit→read） | — | 11.3s |
| bash `ls` | — | 3.8s | 4.1s | 11.1s |
| MCP 工具调用（自带 python 假服务） | 未测（无模型） | 4s ✓ | 4s ✓ | — |
| 读 `/etc/hosts` | — | read 被沙箱拒绝，bash 绕行 | read 直接成功 | — |
| 三文件并行读 | — | ✓ 三个 read 并发，结果保序 | — | — |
| 无网络启动开销（中位数） | 22 ms | 9 ms | 12 ms | — |

延迟主要由模型决定，23a6 与 2c40 在同任务上处于同一量级；唯一显著差异是 2c40 默认开启的同步 review 使每轮多一次完整模型调用。

### 错误恢复实测

先用一个 key 无权访问的模型名触发 403，再用正确模型续聊：

- 23a6：`--resume` 后正常回答 "OK"，会话中没有残留错误消息。
- 2c40：`--continue` 后网关返回 400 `Invalid assistant message: content or tool_calls must be set`。会话文件中出现两条 `content: []` 的 assistant 消息，`convert_to_llm` 是直通实现，不做 Pi 那样的过滤。

另外：2c40 把 403 的完整 body 打印出来（能看到"无权访问该模型"），23a6 用 `error_for_status()` 只留状态码，这也让它的溢出恢复对 OpenAI-compat 失效（匹配不到 "maximum context length"）。

### Compaction 实测

- 23a6：`--compress-threshold 200 --compress-keep 2`，第二轮起出现 `[compacting]… [compacted: summarized 1, kept 2]`，第四轮从 LLM 摘要中正确答出第一轮的事实。
- 2c40：`reserve_tokens` 调到 127 900 后，第五轮起每轮都打印 `(context compacted)`，事实召回正确；但 `append_compaction` 没有任何调用者，摘要不落盘，`--continue` 每次重载全量历史再重新摘要。

## 性能分析

**23a6** 复用 `reqwest::Client`、真 SSE、按 index 拼装 tool_calls、默认启发式 review 零成本、启动 9 ms。弱点：每次 memory 写入/检索都重新 `SessionStore::open` 并跑全套 DDL；每轮重新扫描 skills 与 AGENTS.md 链；压缩阈值按字符而非 token；TUI 每帧克隆全部行。

**2c40** 真 SSE + 正确的多行 `data:` 解析 + `stream_options.include_usage`。弱点：会话 `append` 每次全文件读写（O(n²) 且非原子）；同步 review 默认开；compaction 不落盘导致阈值后每轮多一次摘要调用；grep/find 每个文件重新编译 regex；`bash -l` 每次加载 login profile。

**9492** 非流式、每次请求 `Client::new()`（新连接池 + TLS 握手）、伪并行、每轮克隆整个 context 两次。

## 设计与代码质量

**9492**：crate 分层最清晰（`rupi-ai → agent-core → {mcp, memory, skills} → coding-agent`），文件短小、事件模型与 Pi 一一对应。但 `Agent` 结构体里内嵌 `FauxProvider` 与 `use_faux` 标志，测试替身泄漏进核心；`rupi_ai::Context` 与 `AgentContext` 字段完全相同却靠拷贝转换；MCP 工具名靠两层包装；错误几乎全是 `String`。

**23a6**：分层同样合理，且有几个设计得很好的原语——`SseFramer`、`CancelFlag`（Atomic + Notify）、`SandboxedTool` 装饰器、`FileMutationQueue`、两阶段 `PendingCall` 门控/执行拆分、TUI 的 `Builtin` 枚举。代价是 `AgentLoop::run` 约 390 行，`run_once/run_chat/run_tui` 三处重复约 100 行装配代码，REPL 与 TUI 斜杠命令各写一份，`memory/lib.rs` 与 `mcp/lib.rs` 各塞四种职责，`thiserror` 声明了却没用。注释密集、中文、大量"对标上游"说明，对读代码的人是双刃剑。

**2c40**：`Provider` 与 `Tool` 两个 trait 小而对象安全，`edit.rs`、`truncate.rs`、`retry.rs`、`sse.rs` 是三个分支里最整洁的模块。但手写 argv 解析器、14 个参数的函数、6 处 `#[allow(dead_code)]` 与 `let _ = sid;` 式压警告、三个 provider 各复制一份约 60 行的重试/spawn/channel 样板、`convert_to_llm` 是直通函数。

三者共同点：错误类型普遍字符串化；`async fn` 内直接做 `std::fs` 与 rusqlite 阻塞调用，没有 `spawn_blocking`；所有 provider 都对每个 HTTP chunk 做 `from_utf8_lossy`，理论上多字节字符跨 chunk 会产生 U+FFFD（6 次中文长文本实测均未复现，网关按事件边界分块）。

## 缺陷清单（按严重度）

### 9492
1. CLI 从未接入真实 provider：`main.rs` 只设置 `options.faux`，`options.live` 恒 `None`，`Agent::new` 默认空 `FauxProvider`。实测 `-p` 0.02s 返回空字符串。
2. "并行"工具是顺序执行：`loop_.rs:411-422` 逐个 `await` 惰性 future。
3. `truncate_bytes` 用 `&s[..max]` 按字节切片（`bash.rs:98`），第 32 000 字节落在多字节字符内即 panic。
4. bash 先 `read_to_end` stdout 再读 stderr（`bash.rs:57-62`），stderr 超过管道缓冲即死锁到超时；超时只 `child.kill()` 不杀进程组。
5. 路径沙箱 `resolved.starts_with(&self.cwd)` 是按组件的词法比较（`permissions.rs:47`），`cwd/../../etc/passwd` 通过。
6. MCP stdio 写一行读一行、无 request-id 匹配（`stdio.rs:50-68`），服务端任何通知都会被当作响应并反序列化失败。
7. compaction 切点不看角色，可能留下没有 `tool_use` 的 `tool_result` 导致 API 400。
8. Gemini 路由到 OpenAI endpoint；无 resume；无重试。

### 23a6
1. chat REPL 遇 provider 错误直接退出进程（`main.rs:1249` `res?`），实测输入一行后 403 即退出，没走到 `/quit`。TUI 同场景能恢复。
2. `edit` 无唯一性检查，`replacen(old, new, 1)` 静默替换首个匹配（`rupi-tools/lib.rs:420-423`）；空 `old_string` 会把新文本插到文件头。
3. OpenAI-compat 非 2xx 走 `error_for_status()?`（`rupi-llm/lib.rs:354, 371`）丢弃响应体，用户看不到原因，溢出恢复对该 provider 永远匹配不上。
4. 会话持久化有损：每轮只存用户文本和最终 assistant 文本，`--resume` 丢失全部工具调用上下文。
5. `SubagentTool` 新建 `AgentLoop` 时不带 policy/approver/hooks，委托任务绕过全部权限门。
6. 沙箱对不存在的目标只 canonicalize 父目录，悬空符号链接写入可逃逸；`bash` 本身不受沙箱约束，模型可自行创建链接。
7. Anthropic `--thinking medium|high` 静默失效：`max_tokens` 默认 4096 不大于预算，只有 `low` 生效。
8. 秘钥扫描把任何含 `sk-` 的文本拒写（`task-runner`、`risk-based`）。
9. 不解析 `usage`；`run` 模式无 JSON 输出；缺 key 静默降级 mock。
10. Gemini 流式会把同名的两个并行调用合并成一个。

### 2c40
1. provider 错误污染会话：`AssistantMessage::error` 的空 content 消息被持久化，`convert_to_llm` 直通不过滤，`--continue` 后 400。实测复现。
2. "后台" review 是每轮同步 `.await` 的第二次完整模型调用（`run.rs:374-391`），默认开启；实测把 2 秒的任务拉到 14 秒。首轮 review 还写了一段 SKILL.md。
3. compaction 不落盘（`append_compaction` 零调用者），`Session::messages()` 忽略 `Compaction` 条目；阈值后每轮重摘要。
4. 无路径沙箱、无权限系统，实测 `read` 直接读出 `/etc/hosts`。
5. MCP 子进程 stderr 设为 `piped` 却从不读取（`client.rs:71`），服务端多打日志即死锁；`initialize` 与 `tools/call` 无超时；服务端崩溃后 pending oneshot 永不返回。
6. `--thinking` 解析后没有任何 provider 读取；`abort` 通道存在但 CLI 从不创建 watch channel，无 Ctrl-C 处理。
7. 会话 `append` 每次读全文件再整体写回，O(n²) 且非原子。
8. MCP Streamable HTTP 读到 `mcp-session-id` 后丢弃（代码注释自承）。
9. `bash` 无默认超时，输出全部读入内存后再截断；`read` 带 offset/limit 时无字节上限。
10. OpenAI usage 被加两次（`Usage` 事件与 `Done` 同时累加）；RPC 模式一行坏 JSON 即退出。

## 测试

- 9492：44 个测试，全部通过。有 MCP 的 Python 子进程与手写 TCP HTTP/SSE 端到端，记忆语义单测充分；provider 层零测试，并行/steering/沙箱穿越均无测试覆盖其当前失效。
- 23a6：255 个测试，全部通过。`AgentLoop` 在 mock provider 上跑取消、压缩、溢出、hooks、policy、并行保序等端到端脚本；三个 provider 各有 TCP stub；MCP 对真 Python 服务测 tools/resources/prompts/反向请求/list_changed；TUI 用 `TestBackend` 做渲染断言；`smoke.rs` 驱动编译后的二进制。`main.rs` 无内联测试；测试里用 `set_var` 改环境变量与固定临时目录名，存在并行 flaky 风险。
- 2c40：20 个测试，全部通过。faux provider 冒烟、工具往返、会话 JSONL、stdio MCP、skills read-before-write。SSE 解析器、重试、provider 映射、错误路径、HTTP MCP、compaction 持久化全无测试；上面列出的每个缺陷都在测试盲区。

## 建议

1. **以 23a6 为主线**，先修四件事：REPL 捕获 provider 错误而不退出；`edit` 加唯一性校验（可直接移植 2c40 的 `apply_edits`：多处编辑、overlap 检查、diff 输出）；OpenAI-compat 读出错误体再报错，同时让溢出恢复对其生效；会话持久化补齐工具调用与结果。
2. **从 2c40 择优搬运**：`truncate.rs`（Pi 的 2000 行 / 50KB 默认）、`sse.rs`（多行 `data:` 与 `event:` 解析）、`--mode json` 事件流、`stream_options.include_usage`。
3. **从 9492 借鉴**：`docs/UPSTREAM.md` 的上游对照文档形式，以及 `[core]` 记忆分层的思路。
4. 三个分支都应把 `from_utf8_lossy` 改成跨 chunk 的增量 UTF-8 解码，并把 `std::fs`/rusqlite 调用挪进 `spawn_blocking`。
5. 修正 `.env` 里的 `MODEL`（当前 key 可用 `deepseek-v4-flash`）。

## 附：本次产生的文件

- `.worktree/rupi-pi-agent-9492`、`.worktree/rupi-pi-agent-port-23a6`、`.worktree/rust-pi-agent-2c40`：三个 worktree，各自 `target/release/rupi` 已编译。
- `.gitignore`：新增，忽略 `.worktree/` 与 `.env`。
- 本文件。

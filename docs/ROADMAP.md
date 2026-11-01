# 路线图与现状

记录时间：2026-11-01，基线 `main` @ `c293c78`。本文记下目标、已知问题、做到哪一步、下次从哪里接着做。逐条证据见 [`backlog/`](backlog/README.md)。

## 目标

rupi 要做成 Agent SaaS：在云端大规模执行，服务很多租户和用户。排优先级时，云端路径（`rupi-server`、`rupi-runtime`、共用的 `rupi-agent` / `rupi-llm`）排在本机 CLI/TUI 打磨前面。只影响本机单用户的问题优先级低，除非改的是两边共用的代码。

## 已完成

| PR | 内容 |
|---|---|
| [#43](https://github.com/occcat/rupi/pull/43) | 中英双语 README、`AGENTS.md`、补全 `.gitignore` |
| [#44](https://github.com/occcat/rupi/pull/44) | 升级 rustls、quinn-proto 等依赖，修复 `cargo audit` 报出的 10 个漏洞 |

仓库元数据已设置：description、15 个 topic、17 个 label（`area: *`、`security`、`performance`、`refactor`、`platform: *` 等）。

## 审查结论

2026-10-31 做了两轮分析，结果在 [`backlog/`](backlog/README.md)：

1. **代码审查**：8 个子系统（agent 核心、llm、工具/扩展/包/skills、MCP/记忆/配置、CLI、TUI、云控制面）加上一轮二进制端到端实测，共 258 条发现（P0 44 条，P1 141 条，P2 73 条），每条都附了代码行号或复现命令。不同审查报过的同一问题保留了重复条目。`rupi-runtime` 的专项审查没有完成，它的问题只覆盖到云控制面审查顺带发现的那几条。
2. **对标上游 Pi v1.4.0**：0.85 之后上游新增了 `pi-durable`（先提交后展示的持久状态机）、`pi-env`（Rust 远程执行守护进程）、`pi-codemode`、`pi-mcp`、`pi-telemetry`。分析给出 42 条按 SaaS 价值排序的借鉴条目，分 5 个阶段，见 [`backlog/upstream-pi/ranked.md`](backlog/upstream-pi/ranked.md)。目标架构见 [`backlog/upstream-pi/architecture.md`](backlog/upstream-pi/architecture.md)。`pi-env` 执行协议那一块的研究没有完成，相关条目（B3、B5、B33）的设计依据来自其他领域研究里的引用。

对 SaaS 影响最大的问题：

- **云端一轮的中间状态不落盘。** 会话树只在 run 结束时整体 DELETE 再 INSERT（`rupi-server/src/db.rs` `persist_tree`），SSE 先于持久化发出。副本崩溃会丢掉整轮，工具副作用却已经发生；租约没有 fence，僵尸副本可能用旧树覆盖新数据。对应 B22–B26。
- **transcript 会坏到无法恢复。** 压实切点拆散 tool_call 与 tool_result；同一轮有多个工具调用时，云端 interrupt 只挂起第一个；`AGENTS.md` 被当作 System 节点写进树。这三种情况都会让后续每次请求 400。对应 F226、F228、F193、F198 和 B1。
- **租户之间相互影响。** bash 输出全量读进 execd 内存（B3）；`PoolScheduler::abort` 是空实现（B5）；平台 OpenAI key 会被发给其他 provider（B2）；租户级 frozen 记忆的缓存按租户键却按会话取数，300 秒内同租户的其他会话会读到这个会话的私有记忆，而 scope=tenant 的记忆反而被绑到写入它的会话上，其他会话看不到（F194、B6）。
- **计费漏账。** 取消、出错、压实产生的 token 都不计入；断开 SSE 即可免费拿到输出（B8）。
- **对新一代模型的请求会被拒。** 总是带 `temperature` 和 `budget_tokens`；DeepSeek 的 `reasoning_content` 没有回放；Gemini 3 的 `thoughtSignature` 没有回放；Anthropic 空工具输出触发 400（F101–F106）。

## 进行中的分支

第一轮把发现和阶段 0/1 的借鉴条目按互不重叠的文件范围拆成 7 个 track，任务说明在 [`backlog/tracks/`](backlog/tracks/)。实现到一半时，执行用的模型服务达到月度请求上限，全部中断。下表是中断时各分支的实际状态：

| 分支 | 状态 | 已有提交 |
|---|---|---|
| `thoxvi/tools-ext-fixes` | 已推送，3 个提交，可以继续 | `9468cf1` 截断按字节、`read` 按 50KB 整行分页（F3、F4、F24）；`91e2415` bash 不被后台进程挂住、stdin 接 `/dev/null`、超时保留输出（F5、F6、F25、F149）；`976cc13` `ToolRegistry` 有序存放，工具顺序稳定（F16、B9 的一部分） |
| `thoxvi/cloud-run-safety` | 已推送，1 个**未完成**提交，不能直接合入 | `55b7ea8`：只改了 `rupi-agent`（工具门整批判完、多 interrupt 收集、截断调用不执行、计划模式白名单、bash 黑名单按 argv 判）。`rupi-server/src/run.rs` 仍只取第一个 interrupt，track 里的其余 server 侧条目都没做 |
| `thoxvi/agent-loop-integrity`、`thoxvi/executor-output-bounds`、`thoxvi/llm-wire-compat`、`thoxvi/mcp-memory-sessions`、`thoxvi/local-frontends-fixes` | 未开始 | 只在本机 `.worktree/` 下建过同名分支，没有提交，没有推送 |

已验证：`tools-ext-fixes` 上 `rupi-tools` 与 `rupi-cli` 的测试全部通过，fmt 通过，`rupi-tools` 没有新增 clippy 警告；`cloud-run-safety` 上 `rupi-agent` 的 87 个测试全部通过。

## 下一步

按顺序：

1. **合并 `thoxvi/tools-ext-fixes`。** 先 rebase 到最新 `main`，跑完整 CI，再开 PR 合并。
2. **补完 `thoxvi/cloud-run-safety`。** server 侧改用 `take_pending_interrupts()`，每个挂起调用落一行 interrupt，resume 时逐个回填并补上端到端测试；接着做 track 里其余的 P0：租约泄漏 F192、跨会话记忆 F194、凭证匹配 B2、persist 守卫 B4、计费 B8。
3. **按 track 说明逐个实现其余 5 个 track。** 建议顺序：`llm-wire-compat` → `agent-loop-integrity` → `executor-output-bounds` → `mcp-memory-sessions` → `local-frontends-fixes`。
4. **阶段 2 持久化内核**（B22–B26）：按步增量提交、`runs` 表与 epoch fence、reconciler、effect sandwich、run 幂等。这几项是支撑大规模多副本的核心，依赖第 2 步的 B4。
5. 阶段 3、4 按 [`backlog/upstream-pi/ranked.md`](backlog/upstream-pi/ranked.md) 推进。

[`backlog/upstream-pi/architecture.md`](backlog/upstream-pi/architecture.md) 末尾列了需要产品方拍板的问题，包括目标并发规模、计费口径、平台 key 与 BYOK 的边界、崩溃后默认续跑还是失败、是否开放自动放行、codemode 是否做。

## 本机验证环境

macOS CI 上没有 PostgreSQL 和 Redis，`rupi-server` 的集成测试会静默显示 ok，等于没跑（F213）。改动云端代码时要在本机起这两个服务：

```bash
PGD=$(mktemp -d)
initdb -D "$PGD" -U rupi --auth=trust -E UTF8 --locale=en_US.UTF-8
printf "port = 55432\nlisten_addresses = '127.0.0.1'\nunix_socket_directories = '$PGD'\nfsync = off\nmax_connections = 300\n" >> "$PGD/postgresql.conf"
pg_ctl -D "$PGD" -l "$PGD/log" -w start
createdb -h 127.0.0.1 -p 55432 -U rupi rupi
redis-server --port 6399 --bind 127.0.0.1 --save '' --appendonly no --daemonize yes

export DATABASE_URL=postgresql://rupi@127.0.0.1:55432/rupi REDIS_URL=redis://127.0.0.1:6399/0
cargo test --locked -p rupi-server -p rupi-runtime
```

几点注意：

- 数据库必须是 UTF-8 locale。`--locale=C` 时 `cloud_fts` 的中文检索断言会失败。
- 几个 track 并行时，每个 track 用独立的数据库和 Redis db 编号，测试会 TRUNCATE 和 FLUSHDB。
- `crates/rupi-cli/tests/smoke.rs` 里有两个测试依赖仓库内的 `target/` 才能找到 `skills/builtin`，自定义 `CARGO_TARGET_DIR` 时会失败。根因与 `cargo install` 装出的二进制找不到内建 skill 相同，见 `local-frontends-fixes` track。
- 在 `main`（`c293c78`）上，`cloud_hardening::sse_drop_aborts_inflight_bash` 在 macOS 本机必定失败（F214），与第一轮的改动无关。

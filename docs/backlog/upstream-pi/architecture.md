# 目标架构（来自对标 Pi v1.4.0 的综合结论）

上游 Pi 在 0.85 到 1.4 之间把原来的单进程 CLI 拆成了分层的包。pi-agent-core 现在只剩 agent loop。pi-durable 是一个持久化状态机，规则是先提交后展示，提供步骤 checkpoint、effect sandwich、restart_limit 和 group commit。pi-env 是用 Rust 写的远程执行守护进程，取消时杀掉整个进程组，输出过大时写到文件，连接断开 30 秒后自行清理。pi-codemode 把 QuickJS/WASM 虚拟机放在会话宿主之外运行。pi-telemetry 只定义诊断用的接口，不记录内容。对 rupi 最重要的一点是：权威状态必须是已经提交的步骤日志，不能只存在副本内存里。rupi 目前先发 SSE 后落盘，run 结束时把整棵树 DELETE 再 INSERT，租约也没有 fencing。崩溃丢掉整轮、僵尸副本覆盖新数据、PG 写放大、断开 SSE 就漏计费，这几个问题都出在这里。上游靠单进程独占存储保证一致性，rupi 是多副本，做不到这一点，只能在 Postgres 里用 epoch 围栏加步骤提交实现同样的保证。动手之前，云端主路径上有一批 S 级缺陷要先修，我已在代码里逐一确认：工作区有 AGENTS.md 时，审批后 resume 必然返回 400；bash 输出没有上限；平台的 OpenAI key 可能被发给其他 provider；PoolScheduler 的 abort 是空实现；frozen 记忆会泄露到同租户的其他会话。

会话所有权保持现状：rupi-server 是无状态多副本，Postgres 是唯一权威，rupi-execd 和 rupi-sandboxd 作为外部执行面。上游 durable 依赖单进程独占存储（durable.README.md:881），Chord 只有一个 server，这两种拓扑都不照搬。会话按 run 激活，run 期间唯一的 owner 由 PG 证明：runs.fence（过渡期用 sessions.run_epoch）单调递增。crates/rupi-server/src/db.rs 的每次写入，都要在同一事务里先对 runs 或 sessions 行执行一条带 fence 条件的 UPDATE 拿到行锁，影响 0 行就停止写入并 cancel；Redis NX 只保留为快路径提示。副本存活由 instances 心跳表判定，接管时间可以从 180 秒缩到 15 至 45 秒。reconciler、dispatcher、闲置回收和配额 reconcile 都放进 reclaim::loop_forever，用 FOR UPDATE SKIP LOCKED 和 pg_try_advisory_lock 分片，全集群只做一份扫描，不再由每个副本各自遍历全部租户。提交模型借用 durable 的先提交后展示，落在 PG 上就是按步骤提交。rupi-agent 定义 RunJournal trait，AgentLoop 在五个点上 await 提交：用户消息、assistant 完成、工具 intent、工具结果、压实。rupi-server 的 PgJournal 每一步只开一个短事务：messages 只追加并带会话内 seq，sessions 记录 leaf_id，runs 记录 phase、checkpoint、attempt 和 restarts。TEXT_MESSAGE_END、TOOL_CALL_RESULT 和 RUN_FINISHED 在对应提交完成后才发出，文本 delta 仍然立即发。persist_tree 只保留给 import、fork 和 clone，调用前必须持有租约。高频低价值的数据不进 PG：流式 partial 和工具输出尾部最多每秒写一次 Redis run_live，合并后的 AG-UI 事件写进按 run 划分的 Redis Stream，配额增量和租约心跳按副本合并成批量语句。单条提交由 PG 自带的 WAL group commit 处理，应用层不再自建 mutation line。Redis 降级时只影响展示和断线续传，不影响正确性。SSE 断开只移除这个观看者；取消走显式接口，由 owner 的 CancelFlag 执行；任何副本都能用 runs 表和 Stream 重建视图。工具副作用用 effect sandwich 处理：tool_calls 先写入 executing 状态和最终参数，结果节点和 done 状态在同一事务里提交。崩溃后只重跑 Replay::Safe 的工具，其余调用补一条 interrupted 结果；restart_limit 让能稳定打崩进程的输入最终表现为单个 run 的失败，不会在副本间反复接管。rupi-llm 在发送前统一规范化 transcript，修复 tool 配对、把插在中间的 system 消息挪走、剔除 aborted partial，作为所有路径的最后防线，每次修复都计数并告警。执行器保留 Executor trait 和 HTTP 接口（rupi-runtime/src/lib.rs:234-258），语义向 pi-env 对齐：请求带 call_key，执行与连接解绑，cancel 杀进程组，输出在 engine.rs 内有界读取并 spill 到工作区，ExecResult 带 truncated、duration_ms 和 timed_out，资源由 cgroup v2 加卷配额约束，执行面自己暴露 /metrics。codemode 的 VM 只能放在执行面或独立的 rupi-codemoded 池里，控制面只按 start/deliver 发起步进；嵌套工具调用回到控制面，走同一个 ToolGate、策略和配额。codemode 默认关闭，eval 证明收益后再放量。观测和计量分开：计费与配额只认 PG 的 usage_events（服务端生成 run_key 保证幂等，按月分区，Redis 做实时累计并定期与 PG 对账）。tracing 和 OTel 只做诊断，Prometheus 只用低基数标签。trace carrier 存在 sessions 和 interrupts 上，用来关联跨 run 的 trace。租户审计单独建表，与业务写入同事务。降低单会话成本最有效的三项是：确定性的 prompt 前缀、Anthropic 历史缓存断点、增量压实。安全方面，凭证严格按 provider 匹配并做信封加密；图片、MCP、base_url 等出站请求统一过 SSRF 守卫；控制面不执行任何租户代码或模型生成的代码。

## 阶段
- 阶段 0：止血，1 到 2 周。都是 S 或 M 级、互相没有依赖的改动，可以并行推进，包括第 1 到 8 项：transcript 配对、凭证隔离、bash 输出上限、persist 守卫、abort 生效、frozen 记忆泄露、图片 SSRF、计费漏洞。这批改动修复今天就能稳定复现的故障、成本损失和安全问题。
- 阶段 1：成本与可运维的底子，含第 9 到 21 项。这一阶段做缓存前缀、LLM 超时、PG 固定负载、优雅下线与 panic 隔离、快照 claim、格式版本、length 截断、增量压实、可观测性基础、并行只读工具、工作区资源、请求卫生、模型目录与套餐。除了依赖阶段 0 的少数几项外，其余都可以各自独立上线。
- 阶段 2：持久化内核，含第 22 到 26 项。依次落地按步增量提交、runs 表加 fence、reconciler 与存活表、effect sandwich、run 幂等，以及 attempt 计数和 restart_limit。上线顺序是先发兼容读的版本，再用开关切换写路径；chaos 测试需要补上 run 中途被杀的场景。
- 阶段 3：会话体验与计费产品化，含第 27 到 33 项。包括断线续传与窗口快照、用量账本与租户用量 API、租户审计、跨副本控制与收件箱、执行器资源计量、回合边界交接，以及执行面 call_key。
- 阶段 4：企业能力与扩展，含第 34 到 42 项。包括凭证加密与 ProviderConfig、OTel 追踪、principal scope、region 放置、模型路由与故障转移、多租户 MCP，以及经 eval 验证后才做的 codemode 路线，最后是 JSONL 互操作和统一会话运行时。

## 不借鉴
- 上游的单进程 mutation line、group-commit writer 和 per-key gate（pkg/durable/src/session/writer.ts，docs/group-commit.md §4-6）。它们的前提是单个进程独占存储（durable.README.md:881）。rupi 有多个副本共写 PG，并发控制交给 PG 行锁和 fence，应用层只合并高频的小写入。
- 上游规定存储一出错整个 Harness 就失败、需要重新 open（durable.README.md:914）。rupi 一个副本同时服务成千上万个会话，PG 出错时只应该让当前 run 失败或重试。
- 把高频的 deferred 进度和 SSE 事件日志写进权威存储。partial、工具输出尾部和事件流放 Redis（run_live 和 Stream），PG 只接收步骤级提交，避免和 PG 写吞吐目标冲突。
- Chord 的拓扑（一个 server 按需 fork session worker，再加 relay 树）、它的链路协议、进程内热重载，以及链路不能续传的行为。rupi 保持无状态多副本、AG-UI SSE 和滚动发布，只借用 snapshot+delta、溢出时 reset、控制帧优先这几条语义。
- 可插拔的 Storage 后端、Chord 文档模型、通用的扩展自定义持久 task 框架、结构化并发任务图、Cloudflare Durable Object 的 alarm/onWake。rupi 只需要 PG 加内建的 run/tool 状态机，控制面也不执行租户 task 代码。
- v2 不按数量拒绝请求、改为排队的做法（design.md:374）。多租户 SaaS 必须保留 429 和每个租户的订阅者上限，用来防滥用。
- codemode 的确定性重放 journal（pkg/durable/src/codemode/codemode.ts），以及在控制面进程内运行 VM。每个嵌套结果都要提交一次，写入会成倍放大；VM 跑在会话宿主里会拖慢同宿主上的其他租户。VM 只放在执行面，第一阶段不做重放。
- 全部改成 transcript system message 模型（ai 0.86.0 breaking），以及 provider 原生的延迟加载工具（beta）。改动面太大，先借用前缀稳定和历史断点这两点。
- OAuth 订阅登录（ChatGPT、Anthropic 订阅、Copilot 等）和用 provider 登录 token 作为 MCP bearer。消费级订阅不能在多租户 SaaS 里转售或代用，把平台或租户的密钥转发给第三方 MCP 必须禁止。
- 空闲时的 cache warming、virtual model 的 JS route()、分类器模型、models.dev 全量目录。前两项在海量空闲会话上投机发请求，或者执行租户代码；后两项和编码 agent 的主路径无关，或者维护成本过高。
- 把 telemetry 指标当作计费来源、chord 的 telemetry-sqlite 本地存储，以及原样记录最多 500 字符的 provider 错误消息。计费只认 PG 账本；trace 走 OTLP；错误状态只记类别和 HTTP status，避免 prompt 片段进入共享观测后端。
- TUI、终端、安装更新、/bug 和 /share 上传第三方、project trust 交互提问、Windows/Termux 适配、用 JSONL 做主存储。这些都只服务本机单用户，云端不会用到。

## 待决问题
- 目标规模：峰值并发 run 数、活跃会话数、租户数和 region 数分别是多少。这些数决定 messages 是否按 tenant 哈希分区，事件流放 Redis Stream 还是 PG，Redis Cluster 的分片数，以及 reconcile 和心跳的批量参数。
- 计费模型：按 token、按美元成本还是按套餐额度计费，cache_read 和 cache_write 怎么折算。按口径修正后，现有租户的 max_tokens_per_day 实际会被收紧，需要决定是公告还是换算。另外，估算出来的用量（estimated）在条款里怎么写。
- 平台 key 与 BYOK 的边界：哪些套餐可以使用平台托管的 key、开放哪些 provider，BYOK 租户是否计成本、怎么展示。还要定 KMS 选型和密钥轮换流程。
- 崩溃后的默认策略：on_crash 默认 fail 还是 continue（自动续跑会在没人看时继续花 token），restart_limit 设多少，detach_grace_secs 设多久，无人观看期间的运行时长怎么计入配额和账单。
- 是否开放自主模式，即 write、edit、bash 自动放行。这会改变 interrupt 的语义、write 的 replay 分类、能否并行执行，以及沙箱隔离要做到什么强度（cgroup、磁盘配额、出网策略）。
- AG-UI 对外 API 的演进策略：多个 interrupt 同时存在、窗口化的 MESSAGES_SNAPSHOT、断线续传接口、CUSTOM 的 retry/compaction 事件、错误 code 枚举。这些都需要客户端配合，要定版本号和兼容期。
- codemode 是否要做，用哪个运行时（execd 内的 wasmtime+QuickJS、独立的 rupi-codemoded 池，或者先不做，改为在 sandboxd 中自动放行只读 bash）。还要定用哪套 eval 和哪些模型（包括 deepseek-v4-flash）来证明收益。
- 数据驻留与合规目标：region 必须严格不出区，还是允许跨区兜底；是否追求 SOC2 或等保；审计日志、usage_events 和会话数据保留多久，租户删除后审计是否保留。
- 可观测性后端选择（Prometheus、OTLP collector、Tempo/Jaeger 等）、采样比例，以及跨 region 的 collector 部署和出网成本预算。
- 是否需要多 provider 故障转移，以及平台签约的 provider 和模型清单。这决定第 38 项模型路由的优先级，以及模型目录和价格表的维护归谁负责。

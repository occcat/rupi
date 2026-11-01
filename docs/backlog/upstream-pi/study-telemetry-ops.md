# telemetry-ops

## 上游
上游在 0.84 新增 `@earendil-works/pi-telemetry`，1.4.0 时成形。它只定义与厂商无关的契约，不带 exporter，也不依赖任何后端（telemetry.README.md:5-15）。span 只能用回调 `startSpan(options, fn)` 开启，没有 `end()`，也没有 ambient current span。父 span 必须作为参数显式传入，未配置时用 `NOOP_TELEMETRY_CONTEXT`（README:69-121、240-259）。adapter 契约规定：回调正常返回即 ok，抛错即 error；aborted 保持 ok；settle 之后的调用一律忽略（README:123-139）。1.4.0 加入不透明的字符串 map carrier（W3C `traceparent` 可直接放进去）、`inject/extract` 和 span links（README:141-183；pkg/telemetry/src/index.ts:10-30）。carrier 只用于诊断，不能参与授权，大小上限由传输边界负责（README:167）。同版本加入 counter、upDownCounter、histogram，histogram 可带 exemplar（pkg/telemetry/src/metrics.ts:10-41）。metric 属性只允许低基数，不能放 request id、user id、prompt 或路径（README:209、236）。schema 元数据可以标 `sensitive` 和 `cardinality`（README:472-495）。安全约定是 telemetry 只做诊断，不承载业务状态；prompt、参数、输出、header 和凭据都不进属性（README:513-519）。pi-ai 的 `AI_TELEMETRY_SCHEMA` 定义 `pi.ai.request` 和 `pi.ai.poll` 两种 span。开始属性是 provider、api、model、reasoning；结束属性是 stop、usage.input/output/cache_read/cache_write、cost.total、response.id。事件有 `pi.ai.first_chunk`，以及只记字符数的 `pi.ai.block`。错误状态固定为 `provider_error`，消息截到 500 字符。指标有 request.duration、first_chunk.duration（TTFT）、按 kind 分的 tokens 和 cost（pkg/ai/src/telemetry.ts:15-105、121-210；ai.README.md:1118-1150）。pi-durable 的 `DURABLE_TELEMETRY_SCHEMA` 定义 `pi.durable.submit` 和 `pi.durable.phase` 两种 span，后者带 `resumed` 和 outcome；指标有 reports 计数、phase.duration、tasks.active（pkg/durable/src/harness/telemetry.ts:50-139）。durable 把 carrier 存进 submission 和 task 的 JSON（submissions.ts:241），崩溃重开后 phase span 从存储里 extract 出原 carrier，仍落在同一条 trace 上（scheduler.ts:2219-2240）；steer 用 link 连到正在运行的 trace，不改父子关系（submissions.ts:260；durable.README.md:82-91）。telemetry adapter 抛错只上报 `host_error`，不会拒绝 commit（durable.CHANGELOG.md:181）。计量方面，durable 每会话维护一份 `pi.usage` 账本，按 `provider/model` 和工具名分桶，与 assistant entry 在同一个 commit 写入，compaction 的用量也计入（usage.ts:6-56；generation.ts:1145-1153；compaction.ts:200-206）。成本由 `calculateCost` 按目录价算出，支持阶梯价和 1h cache write，并有 `totalFromProvider` 标记（ai/src/models.ts:1236-1256；types.ts:465-487）。OpenAI 兼容 usage 会从 input 里减去 cacheRead 和 cacheWrite，cacheRead 依次读 `prompt_tokens_details.cached_tokens`、DeepSeek 的 `prompt_cache_hit_tokens`、Kimi 顶层 `cached_tokens`，保证 input 指未缓存的输入（gh 拉取的 packages/ai/src/api/openai-completions.ts:1521-1546）。另外，chord 提供 SQLite 本地 trace 存储和 `renderTrace`（chord.README.md:99-125），coding-agent 有安装统计 ping（coding-agent.CHANGELOG.md:2818-2842），这两项都属于本机或单用户功能。

## rupi 现状
指标：只有 `Metrics{runs, reject_429}` 两个进程内 AtomicU64（crates/rupi-server/src/lib.rs:36-40）。`/metrics` 用 `format!` 手写 4 个值的 Prometheus 文本，没有 histogram，也没有按 provider、model、tool 拆分（crates/rupi-server/src/http.rs:76-89）。这个路由不鉴权（http.rs:28）。每次抓取都会串行请求所有执行节点的 `/v1/stats`（http.rs:77 → crates/rupi-runtime/src/pool.rs:277-299），`/ready` 也一样（http.rs:47），admin overview 同样会调（admin.rs:404）。reqwest 客户端用的是 `Client::new()`，没设超时（crates/rupi-runtime/src/http.rs:22、sandbox_http.rs:23）。每次 alloc 还要串行把全部节点的 stats 拉一遍（pool.rs:134-145）。PG 连接池只设了 `max_size(32)`，没设 wait 超时（db.rs:103-113，deadpool 0.13 的 Timeouts 默认不限时）。`reject_429` 把日配额、并发、QPS 几种原因混在一起，因为 `quota::Admit` 只有 `Ok/TooMany` 两个值（quota.rs:5-9、33-86；run.rs:130-141）。SSE 队列和 agent 事件队列满了就丢事件，只打 warn 日志，不计数（run.rs:48-58）。

Tracing：全仓库只有 HTTP 访问日志这一个 span（crates/rupi-runtime/src/access_log.rs:38-46）。它是 info 级 span，会把全部请求头作为字段，只脱敏 authorization、proxy-authorization、cookie、set-cookie（access_log.rs:8-13）。`TraceLayer::new_for_http()` 默认把 on_request 和 on_response 设成 DEBUG、on_failure 设成 ERROR。所以在默认 info 级别下，成功请求一行访问日志都没有，header 只出现在 span 内 ≥info 的事件里。run、turn、LLM 调用、工具、执行器调用都没有 span。`tokio::spawn` 都没有 `.instrument`（run.rs:168、234、246、337）。控制面调执行器的请求不带 traceparent 或 request id（rupi-runtime/src/http.rs:26-47、sandbox_http.rs:26-50）。日志是纯文本 `tracing_subscriber::fmt()`（rupi-server/src/main.rs:83-88；bin/rupi-execd.rs:29-34），workspace 的 tracing-subscriber 只开了 env-filter（Cargo.toml:42）。Cargo.lock 里没有 opentelemetry。

计量与配额：只有日粒度的 `quota_ledger(tenant_id, day, tokens, runs)`（db.rs:270-276、1550-1577）。run.rs:345 只把 input+output 计入 tokens。OpenAI 路径的 input 含 cached，Anthropic 的不含（rupi-llm/src/lib.rs:169-209，测试 1697-1702），所以 cache_read 在 OpenAI 下按全价 input 计入，在 Anthropic 下完全不计，cache_write 两边都不计。`bump_quota_tokens` 在 AG-UI mapper 任务里 await Postgres（run.rs:337-348），而 Usage 事件走满了就丢的 `try_send` 通道（run.rs:26、54-58、359-361），写入错误被 `let _ =` 吞掉。流中取消（rupi-agent/src/lib.rs:753-758）和 stop_reason=error（lib.rs:785）都在发 Usage（lib.rs:801）之前返回，这些轮次不计量。compaction 调用 `provider.complete()`（lib.rs:1289），而 `ChatResponse` 没有 usage 字段（rupi-llm/src/lib.rs:148-151），所以压实的用量从不计入。价格方面，rupi-agent/src/tokens.rs:11-90 有一张按子串匹配的硬编码 `rates_for` 价目表。它只有 input/output 两项，用 f64，只给 CLI/TUI 状态栏用（tokens.rs:194-196）。模型目录 `ModelEntry` 没有价格字段（rupi-llm/src/catalog.rs:29-35），会话导出的 cost 写死为 0（rupi-memory/src/session_io.rs:253-261）。云端 `persist_tree` 只序列化 Message，不写 `usage_by_node`（db.rs:1088），`load_tree` 也不恢复（db.rs:1121-1166）。`persist_tree` 只在 run 结束或中断时调用（run.rs:451、724）。配额只在入院时检查一次（quota.rs:34-40），单个 run 最多 12 轮（run.rs:363），中途超预算拦不住。租户只能查到今天的 tokens 和 runs（http.rs:153-168）；quota_history 只给管理面用（db.rs:1754-1772；admin.rs:536、775）。存储计量只有 `workspace_snapshots.bytes`（db.rs:277-284）。

审计：只有管理面的 `admin_audit`（db.rs:341-349），由 `write_audit` 写入，写失败也被忽略（admin.rs:224-241）。租户侧的审批、BYOK 改键、删除和导出会话都不留审计。`interrupts` 表只有 status，`set_interrupt_status` 只更新 status（db.rs:258-269、1524-1535），不记录由谁、在何时决定。执行器的 `ExecResult` 只有 stdout、stderr、exit_code（crates/rupi-runtime/src/lib.rs:132-136）。超时只体现为 exit_code=124 加 stderr "timed out"（engine.rs:340-346），既没有耗时和 CPU，也没有独立的超时标记。jail 里没有 cgroup 或 rlimit（jail.rs 只有 unshare 和 --die-with-parent）。

## 提案
### 计费级用量账本：每次模型调用一行，以 Postgres 为权威，不经有损通道 [critical/L]
上游：pkg/durable/src/harness/usage.ts:6-56（pi.usage 按 provider/model 和 tools 分桶）；generation.ts:1145-1153（REMINDER：所有 assistant 写入都走同一个函数，账本才完整）；compaction.ts:200-206（压实用量也记账）；telemetry.README.md:513-515（telemetry 只做诊断，不承载业务状态）

差距：run.rs:337-348 在 mapper 任务里 await bump_quota_tokens。Usage 事件走 AGENT_EVENT_CAP=256 的 try_send 通道，满了就丢（run.rs:26、54-58、359-361）。PG 连接池没设 wait 超时（db.rs:103-113，deadpool 默认 Timeouts 为空），池满时 mapper 的 `pool.get()` 会一直等。这期间下一轮的 TextDelta 会把队列塞满，之后的事件被丢，包括下一轮的 Usage，于是少计费。写入错误被 `let _ =` 吞掉。流中取消（rupi-agent/src/lib.rs:753-758）和 stop_reason=error（lib.rs:785）都在发 Usage（lib.rs:801）之前 return，provider 已经收费的这一轮完全不记。客户端断开 SSE 会经 CancelOnDrop（http.rs:103-107）触发取消，所以在流尾 usage chunk 到达之前断开，就能绕过计量。run.rs:345 只计 input+output，口径因 provider 而异：OpenAI 的 input 含 cached，Anthropic 不含，cache_write 两边都不计。compaction 的 `provider.complete()`（lib.rs:1289）拿到的 ChatResponse 没有 usage（rupi-llm/src/lib.rs:148-151），压实花费完全不计。账本只有 tenant×day 的聚合（db.rs:270-276），没有按 model、session、run 的明细。persist_tree 不写 usage_by_node（db.rs:1088），load_tree 也不恢复（db.rs:1121-1166）。配额只在入院时检查（quota.rs:34-40），一个 run 的 12 轮内超了预算也拦不住。

设计：1) rupi-llm：给 `ChatResponse` 加 `#[serde(default)] usage: Option<TokenUsage>`。各 provider 的 `complete` 都要填，`complete_streaming` 的默认实现（lib.rs:250-264）在拿到 resp.usage 后补发一次 StreamEvent::Usage。这样 compaction 和 review（rupi-agent/src/review.rs:323）的调用都能拿到用量。云端目前不用 subagent 和 reviewer，这两种 kind 先预留。2) rupi-agent：新增不丢记录的 `UsageSink` trait，挂在 `AgentLoop::with_usage_sink(Arc<dyn UsageSink>)` 上。在 lib.rs:801-808 发 Usage 的地方、compress_inner 拿到压实结果的地方，以及取消和错误这两个提前返回的分支（lib.rs:753、785），同步调用 `sink.record(UsageRecord{kind: generation|compaction|aborted|error, provider, model, turn, seq, usage, source})`。取消分支拿不到 provider usage，就用估算值：input 取本轮请求历史的 `calibrate` 估算，output 取 partial 文本的估算，并标 `source='estimate'`。Anthropic 在 message_start 就给出 input（anthropic.rs:507-519），可以提前发一次只含 input 的 Usage，取消时就有真实的 input 数。AG-UI 的 STATE_DELTA 照旧走原通道，只用于展示。3) rupi-server/db.rs：迁移 v5，新建 `usage_events(id TEXT PK, tenant_id, session_id, run_key UUID, client_run_id TEXT, turn INT, seq INT, kind, provider, model, input BIGINT, output BIGINT, cache_read BIGINT, cache_write BIGINT, reasoning BIGINT, cost_nanos BIGINT, cost_source TEXT, price_version TEXT, created_at)`，按 created_at 做月分区。幂等键必须由服务端生成：在 start_run（run.rs:163 附近）为每次 run 生成 `run_key = Uuid::new_v4()`，唯一约束定为 `UNIQUE(run_key, turn, seq, kind)`，写入时 `ON CONFLICT DO NOTHING`。不能用客户端传来的 run_id（agui.rs:11）当幂等键：租户每次都传同一个 run_id 的话，之后所有的用量行都会因冲突被静默丢弃，等于免费使用。同一事务里 upsert `usage_daily(tenant_id, day, provider, model, …)`。`quota_ledger.tokens` 的口径统一为 input+output+cache_write 加按权重折算的 cache_read，或者直接改为按成本计配额。4) 写入路径：sink 的实现是专用的 `mpsc::unbounded_channel` 加一个后台 writer。每 run 的记录数受 max_turns=12 和 MAX_OVERFLOW_RECOVERIES=2（lib.rs:129）约束，天然有界。writer 每条记录尽快提交（最多攒 200ms 一批），失败带退避重试。`finish()` 和中断分支都要先 flush 再释放 lease。注意云端的转录只在 run 结束时落盘（run.rs:451、724），没有每轮的 persist_tree 可以挂靠。崩溃时账本只丢在途记录，转录则丢整个 run，所以计费会领先于转录。按 provider 实际花费计费，这是可以接受的。同时给 deadpool 设 `Timeouts.wait`。5) 运行中预算：AgentLoop 增加 `with_turn_gate(Arc<dyn Fn() -> bool + Send + Sync>)`，在每轮调用 provider 之前检查（lib.rs:673 循环头）。gate 读本 run 的本地累计值，加上 Redis 的 `INCRBY rl:tok:{tenant}:{day}`；Redis 不可用时退回 PG 的 today_quota。PG 仍是权威，定期 reconcile 纠偏。超限时返回新的 StopReason::Budget，服务端发 `RUN_ERROR code=quota_exhausted`，再走 finish() 落盘转录、释放 lease。

风险：月分区和 rollup 都要迁移和回填，旧的 quota_ledger 需要并行写一段时间。`ChatResponse {` 字面量全仓有约 38 处（含测试），加字段要逐个补，或者给 ChatResponse 加 Default。估算用量和真实计费有偏差，cost_source='estimate' 的行要在条款里写明口径。逐行写 PG 在高 QPS 下会放大写入，要批量提交。改了配额口径之后，现有租户的 max_tokens_per_day 实际上被收紧了，需要提前公告或做换算。

### 统一 token 语义，模型价格并入目录，以整数纳美元记成本 [high/M]
上游：pkg/ai/src/types.ts:465-487（Usage.cost、cacheWrite1h、reasoning、totalFromProvider）；pkg/ai/src/models.ts:1236-1256（calculateCost 阶梯价，1h cache write 按 2 倍 input 价）；packages/ai/src/api/openai-completions.ts:1521-1546（input = prompt − cacheRead − cacheWrite；cacheRead 兼容 prompt_cache_hit_tokens 和顶层 cached_tokens；reasoning 取 completion_tokens_details，经 gh api 获取）

差距：parse_provider_usage 对 OpenAI 返回的 input 含 cached（rupi-llm/src/lib.rs:169-209；测试 lib.rs:1697-1702 断言 prompt=50、cached=40 时 input=50）。Anthropic 的 input_tokens 本身不含 cached（anthropic.rs:507-545）。同一个 `TokenUsage.input` 在不同 provider 下含义不同。`cache_hit_pct` 用 cache_read/input 计算（rupi-core/src/lib.rs:159-164；rupi-agent/src/tokens.rs:205-210），在 Anthropic 下可能超过 100%。cacheRead 的取值路径里没有 DeepSeek 的 `prompt_cache_hit_tokens`（lib.rs:183-191）。如果网关不返回 `prompt_tokens_details`，目前主力的 deepseek 模型的 cache_read 就恒为 0，全部按未缓存计。reasoning tokens 在 rupi-llm 里完全没有解析，1h cache write 也没有区分。价格方面，目录 ModelEntry 只有 id、provider、name、context（catalog.rs:29-35）。另有一张硬编码价目表 `rates_for`（rupi-agent/src/tokens.rs:11-90），按子串匹配，只有 input/output 两档，用 f64，没有 cache 价；不认识的模型（包括 deepseek）一律落到默认的 $1/$5（tokens.rs:83-88）。它只给 CLI/TUI 状态栏用（tokens.rs:194-196），云端完全不用。session_io.rs:253-261 的 cost 写死为 0。

设计：1) rupi-llm：把 `TokenUsage.input` 统一定义为未缓存的输入。OpenAI 兼容路径做 `input = prompt.saturating_sub(cache_read + cache_write)`，cache_read 依次读 `prompt_tokens_details.cached_tokens`、`prompt_cache_hit_tokens`、顶层 `cached_tokens`，与上游顺序一致。给 TokenUsage 加 `reasoning: u64` 和 `cache_write_1h: u64`，都用 serde default，解析 `completion_tokens_details.reasoning_tokens` 和 Gemini 的 `thoughtsTokenCount`。修正 lib.rs:1697 的测试，cache_hit_pct 的分母改为 input+cache_read+cache_write，rupi-core 和 TokenMeter 两处都要改。2) catalog.rs：ModelEntry 增加 `cost: Option<ModelCost{input, output, cache_read, cache_write, tiers: Vec<{input_tokens_above, …}>}>`，单位 USD/Mtok，并在 models.json 里补价格。`rates_for` 改为从目录读，删掉硬编码表，保证 CLI 和云端只有一处价格。新增 `fn cost_nanos(entry, &TokenUsage) -> (i64, price_version)`，用整数纳美元（1e-9 USD）。纳美元的 i64 上限约 92 亿美元，足够。微美元对便宜模型的单次调用会舍入成 0。3) 价格的 SaaS 来源：运营可以在 Postgres 表 `model_prices(provider, model, effective_from, …)` 里覆盖，管理面可编辑并写审计。记账时查生效价格，把 `price_version` 写进 usage_events。4) BYOK 租户的 cost 只做展示（cost_source='byok_estimate'），平台 key 租户才进计费。provider 返回了真实计费（如 OpenRouter 的 usage.cost）时直接采用，标 `cost_source='provider'`。

风险：改 input 语义会影响 TokenMeter 的校准：`note_usage_cache` 用 input/estimated_input 算 ratio（tokens.rs:161-186）。改成只算未缓存的输入后，缓存命中高的会话 ratio 会骤降，可能低于 0.1 被丢弃，上下文百分比就会算错，所以校准要改用 input+cache_read+cache_write。CLI（main.rs:2097-2104、2626-2636）和 TUI（app.rs:1719、1870-1876）的展示公式要同步检查。价格表需要持续维护，错价直接变成错账，要有 effective_from 和审计。DeepSeek 这类聚合网关的字段五花八门，要逐家写回归测试。

### 基于 tracing + OpenTelemetry 的分布式追踪：内容无关的 span 模型，上下文一直传到执行器 [high/L]
上游：telemetry.README.md:69-139（显式父 span，状态语义，aborted=ok）、141-183（carrier 和 links，traceparent 可直接放入）、513-519（不记 prompt、参数、输出、header）；pkg/ai/src/telemetry.ts:15-105（pi.ai.request 的属性和 first_chunk 事件）；pkg/durable/src/harness/telemetry.ts:77-115（phase span 带 resumed 和 outcome）；durable.CHANGELOG.md:181（adapter 抛错不影响业务）

差距：全仓库只有一个 info_span，就是 HTTP 访问日志（access_log.rs:38-46）。run、turn、LLM、工具、执行器都没有 span。tokio::spawn 不传递 span（run.rs:168、234、246、337）。HttpExecutor 和 SandboxExecutor 的请求不带 traceparent（rupi-runtime/src/http.rs:26-47、sandbox_http.rs:26-50）。execd 和 sandboxd 都只有访问 span（execd.rs:108；sandbox.rs:120），关联不到 run。没有 OTel 依赖（Cargo.lock 里 opentelemetry 出现 0 次），日志只有纯文本 fmt（main.rs:83-88）。

设计：1) 依赖：在 rupi-runtime 加一个可选 feature `otel`，引入 `opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`、`tracing-opentelemetry`。新建模块 `rupi-runtime/src/otel.rs`，供 rupi-server、rupi-execd、rupi-sandboxd 三个 bin 共用。rupi-cli 只把 rupi-runtime 当 dev-dependency（rupi-cli/Cargo.toml:37-38），CLI 的产物不受影响。只有设置了 `RUPI_OTEL_ENDPOINT` 才启用，否则退回现在的 fmt 输出，相当于 NOOP。使用 BatchSpanProcessor，限定队列长度，满了就丢弃并计数；导出超时 5s；初始化失败就降级为 noop，绝不阻塞 run。2) span 词表（名称和属性统一定义，见词表提案）：
- `rupi.run`：root span，在 run.rs 的 drive 里开启，用 `.instrument()` 包住 spawn；map_task、心跳、abort 这几个子任务也要 instrument。属性有 tenant.id、session.id、run.id、region、instance.id、resumed、parent_run.id。结束时记 outcome（success/error/interrupt/aborted/quota）和 turns。
- `rupi.turn`：属性 turn。
- `rupi.llm.request`：属性 provider、model、reasoning；结束时记 stop、usage.*、cost、retries；事件 `first_chunk`；错误状态取固定类别 provider_error 加 HTTP status。
- `rupi.tool`：属性 tool.name、tool.call_id、policy.decision（allow/ask/deny）、is_error，不记参数和输出。并行工具走 join_all（lib.rs:1028），每个 future 单独 instrument。
- `rupi.compaction`：结束时记 summarized、kept。
- `rupi.workspace.ensure_hot`：source 取 hot、snapshot 或 empty，另记 bytes。
- `rupi.snapshot`：属性 bytes。
- `rupi.executor.call`：client span，属性 op、node.id、node.kind、region、http.status。
3) 传播：在 HttpExecutor::post 和 SandboxExecutor::send 里用 `TraceContextPropagator` 把 `traceparent`/`tracestate` 注入 header。execd（execd.rs:108）和 sandboxd（sandbox.rs:120）的 router 在 `access_trace_layer` 之前加一个 extract 层，生成 server span `rupi.execd.<op>`，子 span 为 `rupi.jail.spawn`（exit_code、timed_out、duration）。执行器只信任已通过 bearer 认证的请求带来的 carrier。4) 客户端入站的 `traceparent` 只作为 link，不作为 parent，长度上限 512 字节，防止租户把数据注入运营方的 trace。5) 采样：parent-based 头部采样，比例由 `RUPI_OTEL_SAMPLE_RATIO` 配置；collector 端做 tail sampling，错误和慢 run 全部保留。按租户强制采样的开关放在 tenants 表的管理员专用列，不放在租户可以 PATCH 的 settings 里（http.rs:189 起），免得租户把采样拉满、抬高观测成本。

风险：AgentLoop 在 rupi-agent 里。给它加 span 只用 `tracing` 宏，exporter 只在云端 bin 初始化，不会把 OTel 依赖带进 CLI 和 TUI。span 属性里有 tenant.id，基数高，但 trace 后端可以接受，只要不进 metric label。跨区域时要评估 collector 的部署和出网成本。`tracing-opentelemetry` 和 `opentelemetry` 的版本必须严格配套。

### 重做指标：低基数的 Prometheus 或 OTel 指标，执行器状态改为后台缓存，并保护 /metrics [high/M]
上游：pkg/ai/src/telemetry.ts:70-104（pi.ai.request.duration、first_chunk.duration、tokens{kind}、cost）；pkg/durable/src/harness/telemetry.ts:117-138（phase.duration、tasks.active、reports{type}）；telemetry.README.md:209、236（只允许低基数属性，禁止 request id 和 user id）

差距：指标只有 4 个手写值（http.rs:76-89；lib.rs:36-40）。429 不区分原因（run.rs:130-141；quota.rs:5-9 的 Admit 只有 Ok/TooMany）。下面这些都没有指标：LLM 时延、TTFT、重试（rupi-llm/src/lib.rs:907-935 的重试不留任何记录）、工具时延、执行器操作时延、热恢复与冷恢复、SSE 丢事件（run.rs:48-58 只打 warn）、Redis 降级（cache.rs:55-70 只打日志）、PG 连接池等待。连接池本身也没设 wait 超时（db.rs:103-113）。/metrics 不鉴权（http.rs:28）。每次抓取都串行请求所有执行节点的 /v1/stats，而且不设超时（http.rs:77 → pool.rs:277-299；rupi-runtime/src/http.rs:22、321-333）。/ready（http.rs:47）和 admin overview（admin.rs:404）也做同样的扇出。每次 alloc 的 `ranked()` 也串行拉一遍全部节点（pool.rs:134-145），alloc 时延因此随节点数线性增长。N 个控制面副本、M 个执行器，每个抓取和探活周期就有 N×M 次 HTTP；任何一个节点卡住，抓取、探活和 alloc 会一起卡住。/metrics 不需要凭据，公网上任何人都能反复请求它，每次触发 M 次出站请求，这是一个放大攻击面，同时还暴露了集群容量。

设计：1) 选用 `metrics` crate 加 `metrics-exporter-prometheus`，或者 OTel metrics。如果已经接了 OTel（见追踪提案），统一走 OTLP，同时保留 Prometheus 拉取。2) 指标清单（label 都是有界取值）：
- rupi_runs_total{outcome}
- rupi_run_rejects_total{reason=quota_runs|quota_tokens|concurrency|qps|lease_conflict|pool_exhausted}，需要把 `quota::Admit::TooMany` 改成带 reason
- rupi_run_duration_seconds{outcome}
- rupi_llm_request_duration_seconds{provider,model,stop}
- rupi_llm_ttft_seconds{provider,model}
- rupi_llm_tokens_total{provider,model,kind}
- rupi_llm_retries_total{provider,status_class}
- rupi_tool_duration_seconds{tool,outcome}
- rupi_tool_decisions_total{tool,decision}
- rupi_executor_op_duration_seconds{op,node_kind,region,status}
- rupi_workspace_hydrate_seconds{source}
- rupi_snapshot_bytes（histogram）
- rupi_events_dropped_total{stage=agent|sse}
- rupi_cache_ops_total{op,result}
- rupi_redis_up
- rupi_pg_pool_size、rupi_pg_pool_available、rupi_pg_pool_waiting：直接取 deadpool 的 `Pool::status()`，同时给池设 `Timeouts.wait`
- rupi_active_runs（本实例）
- rupi_reclaim_total{result}
- rupi_executor_used/capacity/warm{node,region,kind}：节点数随集群规模有界，可以接受
禁止 tenant、session、run 作 label。成本这类计费数据以账本为准，指标只做趋势参考。按租户聚合用 usage_daily 的 SQL，或在 collector 用 spanmetrics 按 plan/tier 汇总。3) 执行器状态改由后台 poller 每 5s 并发拉一次（join_all），缓存在 `PoolScheduler` 里（pool.rs）。`ranked()`（pool.rs:134-145）、`node_stats()`、/ready 和 /metrics 都读缓存。reqwest 客户端设 connect 超时和总超时。4) execd 和 sandboxd 各自暴露 /metrics（exec 时延、jail spawn 失败、租户校验 Forbidden 次数），由 Prometheus 直接抓，不再经控制面代理。5) /metrics 挂到独立的内部监听端口（`RUPI_METRICS_BIND`），或者要求 admin token，不放在公网 listener 上。

风险：model label 的取值随目录增长，需要白名单，未知值归为 `other`；BYOK 的自定义模型名可能无限多。后台 poller 让 ranked() 看到的是最多 5s 前的数据，alloc 可能多遇到几次 pool_exhausted，现有的回退逻辑（pool.rs:176-193）可以兜住。/ready 目前在执行面全挂时返回 503（LAUNCH.md 验收约定），所有副本会同时被摘除，连只读 API 也不可用。改成读缓存的同时，要评估是否把执行面状态从 readiness 里拆出去。/metrics 换端口后要同步更新 LAUNCH.md:162 的验收命令。

### 请求 ID、结构化 JSON 日志、访问日志瘦身和对外错误收敛 [high/S]
上游：telemetry.README.md:517（不记 header 和凭据）、167（carrier 大小上限由传输边界负责）；pkg/ai/src/telemetry.ts:192-196（错误只记固定类别加截断后的消息）

差距：没有生成、回传或传播 request id，全仓 grep x-request-id 只在 access_log.rs:124 的测试里出现。日志是纯文本（main.rs:83-88；rupi-runtime/src/bin/rupi-execd.rs:29-34），tracing-subscriber 只开了 env-filter（Cargo.toml:42）。访问 span 在 info 级把全部请求头作为字段（access_log.rs:39-45），脱敏表只有 4 项（access_log.rs:8-13），x-api-key、x-goog-api-key、x-amz-security-token 都不在里面。不过 `TraceLayer::new_for_http()` 默认 on_request 和 on_response 是 DEBUG 级，on_failure 是 ERROR 级，所以默认 info 级别下成功请求一行访问日志都没有。header 只会随 5xx 或处理函数里 ≥info 的事件输出；drive 是另起的任务，也不在这个 span 里。所以主要问题是缺少有用的访问日志，日志量大并不是现在的问题。对外错误会泄露内部信息：RUN_ERROR 直接带 `e.to_string()`（run.rs:172、427）；执行器错误把对端的响应体原样拼进去（rupi-runtime/src/http.rs:43、sandbox_http.rs:48）；http.rs 里还有 13 处把 `e.to_string()` 作为 500 的响应体，PG 错误文本会直接回给租户。这些错误都没有可供客服查询的关联 ID（agui.rs:107-122）。

设计：1) tower-http 开启 `request-id` feature。在 http.rs:40、execd.rs:108、sandbox.rs:120 加 `SetRequestIdLayer::x_request_id(MakeRequestUuid)` 和 `PropagateRequestIdLayer`。访问 span 只记 method、route（MatchedPath，不用原始 path）、request_id、trace_id，去掉 headers 字段；确实需要 header 时降到 debug 级并改用白名单。on_response 调到 INFO，只记 status 和 latency；对 /health、/ready、/metrics 降级或采样。2) workspace 给 tracing-subscriber 加 `json` feature，用 `RUPI_LOG_FORMAT=json|text` 选择，默认 text，保持本地体验。3) 在 rupi.run span 上带 tenant.id、session.id、run.id，drive 里的日志会自动带上这些字段。4) 控制面调执行器时，带上 `x-request-id`，以及只用于诊断的 `x-rupi-tenant`、`x-rupi-session`、`x-rupi-run`。执行器的鉴权仍以 body 里的 tenant_id 为准（execd.rs:136-141），这些 header 不参与授权。5) RUN_ERROR 和所有 4xx/5xx 的 JSON body 改为固定枚举的 `code` 加 `requestId`（agui.rs:107 已经支持 code）。原始错误只写进带 request_id 的服务端日志，不回给客户端。6) 脱敏表扩充 x-api-key、x-goog-api-key、x-amz-security-token、proxy-authorization 的变体，并补单测。

风险：改用 MatchedPath 需要 axum 的提取器，访问日志层要放在路由之后。on_response 调到 INFO 后，海量请求下日志量会明显上升，需要对探活路由降级并采样。JSON 日志会改变运维现有的 grep 习惯，所以默认保持 text。诊断 header 一旦被误用于授权就会变成越权点，要用代码注释和测试锁住。收敛错误文本后，现有依赖错误原文的测试和客户端需要调整。

### 把 trace carrier 持久化到会话和中断记录，重启或审批恢复后仍能关联 [medium/S]
上游：durable.README.md:91（carrier 存在 submission 和 task 的 JSON 里，无需迁移；重开后 resumed=true，仍在同一 trace；steer 用 link）；pkg/durable/src/harness/submissions.ts:241、260；scheduler.ts:2219-2240；telemetry.README.md:515（carrier 只用于诊断关联，不是状态）

差距：审批中断会结束本次 run，并把 interrupt 落库（run.rs:439-501；db.rs:258-269）。之后的 resume 是一个全新的 HTTP 请求和新 run（run.rs:384-389、513-698）。数据层面只有 interrupts.run_id（db.rs:262）和客户端可选传入的 parentRunId（agui.rs:12-13）可以按属性关联，trace 层面没有任何 link。实例崩溃后，lease 被 reap（db.rs:936-947；quota.rs:122），客户端重试也是一次孤立的 run。fork 和 clone（http.rs:34-35）只在数据里记了 parent_session（db.rs:229），父子会话之间没有可追踪的关联。

设计：1) 迁移：`sessions` 加 `last_trace JSONB`，`interrupts` 加 `trace JSONB`。都可空，旧行不受影响。2) rupi.run 开始时，用 OTel propagator 把当前 span inject 成 `{traceparent, tracestate}`，在 `set_run_id`（quota.rs:128）的同一条 UPDATE 里写入 sessions.last_trace。`insert_interrupt`（run.rs:440）写入 interrupts.trace。3) resume 时，`rupi.run` 新开一个 root span（保持一 run 一 trace，避免长 trace），并对每个 pending interrupt 的 trace 执行 extract 后 `add_link`，link 属性记 reason=resume、interrupt.id。属性 `resumed=true`。4) 崩溃后的同 thread 新 run 也 link 到 sessions.last_trace（reason=retry_after_reap）。fork 和 clone 的新会话 link 到源会话。5) 遵循上游原则：carrier 读不出来或格式未知时直接退回 root，不报错；carrier 绝不参与任何授权或路由决策。

风险：依赖追踪提案的 OTel 先接入，否则没有东西可 inject。traceparent 很小，但需要限制 JSONB 大小（例如 1KB）。更换追踪后端会导致旧 carrier 失效，这符合上游说的“changing adapters may lose correlation”，可以接受。

### 统一的、带版本、内容无关的 telemetry 词表模块，用测试守住基数和 PII 约束 [medium/S]
上游：telemetry.README.md:343-495（defineTelemetrySchema 的 sensitive、cardinality 元数据，metric 属性不允许 high cardinality）；pkg/ai/src/telemetry.ts:64-105；pkg/durable/src/harness/telemetry.ts:49-139（content-free，ID 只指向记录）

差距：现在没有任何 telemetry 命名规范。加 span 和指标的改动会分散到 rupi-server、rupi-runtime、rupi-agent、rupi-llm、rupi-tools 这些 crate，没有统一约束的话，很快会出现 label 里塞 session_id、span 里塞工具参数之类的问题（access_log.rs:39-45 已经是把全部 header 打进 span 的先例）。

设计：1) 在 rupi-core 新增 `telemetry` 模块（不引入新依赖），用 `pub const` 定义 span 名、属性键和指标名，再用一个静态表 `METRICS: &[MetricDef{name, kind, unit, labels: &[LabelDef{key, values: Option<&[&str]>}]}]` 和 `SPAN_ATTRS: &[AttrDef{key, sensitive, high_cardinality}]` 描述它们。版本号为 `TELEMETRY_SCHEMA_VERSION`，作为 OTel Resource 属性 `rupi.telemetry.schema` 导出。2) 测试：扫描 crates/*/src，断言所有 `metrics::counter!`、`histogram!` 的 label 键都在 METRICS 白名单里；断言 `tracing` span 字段不出现 prompt、content、arguments、args、stdout、api_key 等禁用键，做法类似 lib.rs:367-383 已有的源码扫描测试。3) 用这个表生成 docs/TELEMETRY.md，供运维和下游接入者参考。

风险：源码扫描测试只能覆盖字面量，动态拼出来的名字会漏网，需要约定只能用 const。维护这张表会增加每次加指标的步骤。

### 租户用量查询 API 与会话级用量 [high/M]
上游：pkg/durable/src/harness/harness.ts:369-376（Harness.usage() 汇总）；usage.ts:6-11（按 provider/model 和工具分桶）；durable.CHANGELOG.md:111（usage_changed 视图事件）

差距：/v1/me 只返回今天的 tokensToday 和 runsToday（http.rs:153-168）。quota_history 只按天聚合，而且只有管理面调用（db.rs:1754-1772；admin.rs:536、775）。拿不到按模型、会话、run 的拆分，也没有成本。GET /v1/sessions/{id} 只返回节点数和估算的 history_tokens（http.rs:378-392），没有真实用量。云端 persist_tree 不写 usage_by_node（db.rs:1088），load_tree 也不恢复（db.rs:1121-1166），所以云端导出（http.rs:569-594）里的 usage 全是 0。AG-UI 的 MESSAGES_SNAPSHOT 一直不带每条消息的用量（agui.rs:471-476 的 message_to_agui 不读 usage），这和 load_tree 无关，本来就没有实现。

设计：依赖账本提案的 usage_events 和 usage_daily。1) `GET /v1/usage?from=&to=&groupBy=day|model|session&limit=&cursor=`：读 usage_daily（session 维度读 usage_events），走 `app.reader()` 只读副本（lib.rs:133-135），时间窗最多 93 天，用 keyset 分页。2) GET /v1/sessions/{id} 增加 `usage{input, output, cacheRead, cacheWrite, costNanos}`，由 `SUM(usage_events) WHERE session_id` 算出，结果在 Redis 缓存 60s。3) persist_tree 改用现成的 `rupi_memory::message_blocks_json`（session_io.rs:290）写 blocks，把非零的 usage 一起落盘；load_tree 再按 session_io.rs:358 的方式恢复到 usage_by_node，与 CLI 路径对齐，云端导出的用量也就有了。如果需要，可以在 message_to_agui 里附上每条消息的用量。4) 管理面增加 `/admin/api/usage?tenantId=&groupBy=`，并在 overview 里展示今日成本 Top-N 租户（admin.rs:401-423）。5) 可选：预算阈值（tenants.budget_nanos_month）跨线时发 webhook。

风险：在 usage_events 上按 session 聚合需要 `(tenant_id, session_id, created_at)` 索引，大租户的查询要限流。只读副本有延迟，账单页需要标注“数据有几十秒延迟”。blocks 里加 usage 字段后，旧行反序列化要靠 serde default 兼容。

### 执行器资源计量：工作区占用秒数优先，其次是 exec 耗时，CPU 依赖 cgroup [high/M]
上游：pkg/durable/src/harness/usage.ts:9-11（tools 桶，按工具名记用量）；durable.CHANGELOG.md:260（NestedToolExecutionResult 带 durationMs、usage）；pkg/durable/src/harness/telemetry.ts:123-131（phase.duration）

差距：ExecResult 只有 stdout、stderr、exit_code（rupi-runtime/src/lib.rs:132-136）。Engine::exec_for 在子进程退出后不记耗时和 CPU（engine.rs:295-330）。超时只表现为 exit_code=124 加 stderr "timed out"（engine.rs:340-346），和命令自己返回 124 分不开。工作区从 alloc 到 release 或 snapshot 的占用时长没有记录，只有快照的 bytes（db.rs:277-284；reclaim.rs:60-89）。热工作区默认空闲 30 分钟才回收（lib.rs:49-57），在这段时间里一直占着执行器的槽位，这是计算成本的大头。jail 没有 cgroup 或 rlimit（jail.rs 只有 unshare 和 --die-with-parent），所以既测不出单租户的 CPU 和内存，也限不住。

设计：第一阶段（S，价值最高）：1) 新建 `workspace_leases(id, tenant_id, session_id, node_id, region, kind, allocated_at, released_at, release_reason)`。在所有 alloc 入口开启一条记录：http.rs:680-695 的 alloc，以及 reclaim::ensure_hot 里的 alloc_pref（reclaim.rs:123 附近）。在 snapshot_and_release（reclaim.rs:60-89）、teardown_session（http.rs:394 起）和 preempt 时关闭。定时 rollup 出 workspace_seconds，再按存储 GB·天算快照费用。2) ExecResult 增加 `#[serde(default)] duration_ms: u64, timed_out: bool, stdout_bytes: u64, stderr_bytes: u64`，旧版 execd 反序列化时这些字段为 0 或 false。execd 暴露 histogram rupi_execd_exec_seconds{status}。控制面在 ExecBashTool（tools.rs:40）返回后，把这些数据写入单独的 `compute_events` 表，不和 LLM 的 usage_events 混在一起。第二阶段（M，和资源限制一起做）：3) CPU 和内存要靠 cgroup v2：execd 为每个工作区建一个 cgroup，读 cpu.stat 和 memory.peak，同时可以设 cpu.max、memory.max、pids.max。不要用 `wait4` 取 rusage：tokio::process 自己负责回收子进程，再调 wait4 会和它抢；`getrusage(RUSAGE_CHILDREN)` 是整个进程的累计值，execd 上并发执行的多个租户的命令无法区分；rusage 也统计不到 bash 派生的孙进程。macOS 只填 duration。4) 滥用信号：单租户的 exec 秒数或 workspace 秒数在 5 分钟窗口内突增时告警，并可联动调低 max_concurrent_runs。

风险：cgroup v2 需要 execd 有对应的委派权限，部署时容器或 systemd 要配合；bwrap 和 seatbelt 的口径也不一样，计费口径要明确以 Linux 生产环境为准。workspace_leases 的写入频率和 alloc 次数相当，远小于消息量，可以接受。alloc 入口有好几处，漏掉任何一处都会让占用时长偏少，需要集中到一个辅助函数里。

### 租户侧安全审计日志：审批决定、凭据变更、会话导出和删除 [high/M]
上游：上游没有对应功能。上游明确把 telemetry 定位为诊断、不是业务状态（telemetry.README.md:513-515），审计应该作为 PG 权威数据另存，不放进 trace

差距：只有管理面的 admin_audit（db.rs:341-349），由 admin.rs:224-241 写入，失败被 `let _ =` 忽略（admin.rs:232）。租户侧的高风险动作都没有审计记录：工具审批的通过、拒绝和改参（run.rs:536-594）；interrupts 只改 status（db.rs:1524-1535），不记录决定者和时间；editedArgs 会直接替换原参数并执行（run.rs:584-594）。PATCH /v1/settings 修改 BYOK key（http.rs:189 起）、会话的导出、导入、删除、fork、clone（http.rs:33-37）也都不留记录。`tenant_of` 只返回 Tenant，不带是哪把 API key（http.rs:109-134），即使想记也不知道操作者是谁。

设计：1) 迁移：新建 `tenant_audit(id, tenant_id, actor_key_id, action, target_type, target_id, detail JSONB, request_id, trace_id, created_at)`，按月分区，索引 `(tenant_id, created_at DESC)`。detail 只放元数据，例如工具名、参数的 sha256、改了哪些设置键，不放明文 key 和参数。2) `tenant_by_key_hash` 和 `tenant_of`（http.rs:109-134）同时返回 api_key id，作为 actor。3) 写入点：resume 时每个 interrupt 的决定（approved、denied、cancelled、edited，加上参数哈希）；settings 修改（只记键名）；export、import、delete、fork、clone；可选：API key 首次使用或从新 IP 使用。4) 写入失败的处理：审批、改键这类高风险动作，审计写入要和业务写入在同一个事务里，失败就拒绝操作，不能像 admin.rs:232 那样吞掉错误。interrupts 顺带加 decided_at 和 decided_by 两列。5) `GET /v1/audit` 给租户自查，`/admin/api/tenants/{id}/audit` 给运营查。设置保留期，到期按整个分区删除。

风险：同事务写审计会增加审批路径的延迟和 PG 写入量。需要明确保留期与合规要求，以及删除租户时审计是否保留。参数哈希无法还原内容，调查时还要对照 messages 表里的工具调用。

## 不借鉴
- chord 的 telemetry-sqlite 本地 trace 存储和 `TelemetryStore`、`renderTrace`（chord.README.md:99-125；telemetry.README.md:287-308）：SaaS 应该经 OTLP 导出到 collector 和 Tempo/Jaeger 一类的后端。把 span 写进 Postgres 会压垮作为权威数据源的数据库，而且无法水平扩展。
- 上游回调式的 `TelemetryContext`/`TelemetrySpan` API 本身（没有 `end()`，没有 ambient context，telemetry.README.md:69-121、519）：Rust 的 `tracing` 已经有 span、guard 和 `Instrument`，再移植一套平行的 trait 只会造成两套体系。只借它的语义：跨 spawn 显式传递，ok/error/aborted 的状态规则，settle 之后不再记录。
- TypeScript 编译期的 schema 类型推导（pkg/telemetry/src/metrics.ts:54-145、index.ts:175-452）：Rust 里用 const 词表加源码扫描测试就够了，不值得为此写宏。
- `InMemoryTelemetryContext` 的确定性 ID 和自定义 carrier 编码（telemetry.README.md:261-285）：它只服务于测试。rupi 已经有用 tracing-subscriber 测试 layer 的写法（access_log.rs:93-144），生产中的 carrier 统一用 W3C traceparent。
- 安装和更新统计 ping 以及 provider 归因 header（pkg/coding-agent/src/core/telemetry.ts；coding-agent.CHANGELOG.md:2818-2842；docs/settings.md:166-171）：这是本机 CLI 的功能。云端执行租户的 run 时不能向第三方回报，OpenRouter 等平台的归因 header 应该由运营方配置决定。
- durable 的 `pi.usage` 实现方式：每个会话存一份 JSON 文档，`Harness.usage()` 再扫描全部会话求和（harness.ts:365-376）。租户量大时这是 O(会话数) 的全量扫描，SaaS 应该用关系型明细表，加分区和 rollup。
- 用 telemetry 指标（如 `pi.ai.cost` counter）作为计费来源：指标和 trace 会被采样，导出时也可能丢数据，上游自己也说 telemetry 只做诊断（telemetry.README.md:513-515）。计费和配额只能来自 Postgres 账本。
- 原样照搬 span 错误状态里最多 500 字符的 provider 错误消息（pkg/ai/src/telemetry.ts:192-196；ai.README.md:1150）：provider 的错误体可能回显 prompt 片段。多租户把 trace 导出到共享观测后端时，状态里只应放固定类别和 HTTP status，原文只进租户范围内的日志。
- 默认给每个内容块发 `pi.ai.block` 事件（pkg/ai/src/telemetry.ts:42-56、164-179）：在超大规模下，每个 span 的事件量会抬高观测成本。只保留 first_chunk 事件，块数和字符数作为结束属性汇总即可。
- durable 的 phase/task 调度 span 模型整体（pkg/durable/src/harness/telemetry.ts:77-115）：它依赖 durable 的任务和 checkpoint 架构，而 rupi 的 run 是单次 HTTP 驱动加 lease 的模型。只借 `resumed` 属性和 outcome 分类的思路，不借 phase 这个概念。
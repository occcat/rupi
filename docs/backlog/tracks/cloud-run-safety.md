# Track: cloud-run-safety

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/cloud-run-safety`，worktree：`.worktree/cloud-run-safety`

## 目标
云端一次 run 从准入到收尾不泄漏租约、不丢工具结果、不串租户数据、不漏计费、不泄露凭证；控制面能优雅下线。

## 负责文件
- crates/rupi-server/src/**（除 tools.rs）
- crates/rupi-server/tests/**
- crates/rupi-server/Cargo.toml
- 根 Cargo.toml 的 [profile.*]
- crates/rupi-agent/src/lib.rs 中的工具门循环（hook/policy/on_ask/plan 判定、pending_interrupt、执行前过滤）
- crates/rupi-agent/src/policy.rs

不在本 track：rupi-server/src/tools.rs 与 rupi-runtime 归 executor-output-bounds；AgentLoop 其余部分归 agent-loop-integrity；rupi-llm 归 llm-wire-compat。

## 特别说明
- 所有修复都要有回归测试；能用本地 Postgres 跑的集成测试一定要跑（步骤见 [`ROADMAP.md`](../../ROADMAP.md)「本机验证环境」）。
- 优先级：凭证泄露、租约泄漏、tool_call 配对、跨会话记忆泄露、计费 > 其余。

## 借鉴上游 Pi 的条目（来自对标 Pi v1.4.0 的分析，按 SaaS 价值排序）

### B1. 云端 transcript 配对止血：guidance 不再写入树、压实切点对齐、多调用 interrupt、发送前修复 [critical/M]
本 track 范围：做 (a) guidance/context 不再写入树（老会话里已有的 [workspace ...] System 节点在投影时过滤）和 (d) 多 interrupt：pending_interrupt 改为 Vec，整批判完，已放行的照常执行，云端每个 interrupt 一行，resume 全部回填后才 continue；保留 take_pending_interrupt() 兼容（返回第一个）并新增复数接口

上游参考：packages/ai/src/api/transform-messages.ts:179-250（补 No result provided 并挪走夹在中间的 system）；pkg/durable/docs/spec.md §2.1 规则 7-9（325-338）；docs/compaction.md:136；coding-agent.CHANGELOG.md:93（#11227）

差距：已抽查核实。drive 在 resume 分支之前调用 load_remote_guidance（crates/rupi-server/src/run.rs:312，resume 在 :384），把 AGENTS.md 和 commands 作为 Role::System push 进树（run.rs:883、956）。历史因此变成 assistant(tool_use) 后接 system、再接 tool，OpenAI 和 Anthropic 都会返回 400。同样的内容还会拼进 system prompt（run.rs:370-375），每个 run 都多落盘一份。AgentLoop 遇到第一个 Interrupt 就 return（crates/rupi-agent/src/lib.rs:922-932），同一批的其他调用永远拿不到结果。compaction_cut 只按 token 切分（crates/rupi-core/src/lib.rs:284-302），保留窗口的开头可能是一条孤立的 Tool。这些 400 不匹配 is_overflow_error，会话会一直卡住。

设计：(a) load_remote_guidance 和 input.context 只写入 builder.append，不再 tree.push；老会话里重复的 [workspace ...] 节点在 prompt_history 投影时过滤掉。(b) rupi-llm 新增纯函数 repair_tool_pairs，在 to_openai_messages 和 to_anthropic_messages 之前调用：夹在 tool_use 和结果之间的非 Tool 消息挪到结果后面，缺结果的调用合成 is_error 结果，丢弃孤立的结果，并累加 rupi_transcript_repairs_total。(c) compaction_cut 回退到 Assistant 或 User 边界。(d) pending_interrupt 改成 Vec，第一阶段把整批调用都判完，interrupts 表一个调用一行（run.rs:117-125 已经要求 resume 覆盖全部 interrupt）。(e) 云端集成测试改用会校验配对的 mock，并在工作区放一个 AGENTS.md。

### B2. 凭证严格按 provider 匹配，云端禁用 mock 兜底 [critical/S]
本 track 范围：全部

上游参考：ai.README.md「How Auth Resolves」「Credential Store」（存储的凭证属于对应 provider，刷新失败时不回落到 env）；pkg/ai/src/types.ts:164-176

差距：已核实。resolve_byok 的兜底分支对 openrouter、bedrock、azure、vertex 都去取 openai_api_key 或 api_key，再回落到 RUPI_API_KEY/OPENAI_API_KEY（crates/rupi-server/src/run.rs:819-834），然后作为 override 传入；pick_key 会优先用 override（crates/rupi-llm/src/route.rs:80-86）。所以租户只要把 model 设成 openrouter/x，平台的 OpenAI key 就会作为 Bearer 发到第三方。缺 key 时 provider_or_mock 静默返回 demo 回答（route.rs:290-297），这个 run 照样扣次数。Tenant 用 derive(Debug) 打印 settings（crates/rupi-server/src/db.rs:437-440），明文 BYOK key 可能进日志。

设计：resolve_byok 改为 provider 到设置键的严格映射，不跨 provider 回落，通用的 api_key 只给 openai。平台 key 只用于套餐允许的 provider，并且只读该 provider 自己的 env。云端构造 provider 时不再走 provider_or_mock：缺凭证在 preflight 阶段返回 4xx，code=missing_credentials，不扣 run 次数。Tenant 改为手写 Debug，隐藏 settings。usage 记录 billing=byok|platform。加密存储放在第 34 项。

### B4. 持久化止血：persist_tree 加 run_id 守卫，写入失败不再报 success，连接池设超时 [critical/S]
本 track 范围：全部

上游参考：pkg/durable/docs/spec.md §1 不变式 1-3（65-82）、§10.1（6121-6160）；chord packages/chord/docs/next/design.md:2272-2274（忽略旧 incarnation 的上报）

差距：已核实。finish 用 let _ = 吞掉 persist_tree 的错误（crates/rupi-server/src/run.rs:724），然后照样把内存里的树写进 Redis（run.rs:725-730），再发 RUN_FINISHED success。persist_tree 先 DELETE 整个会话，再全量 INSERT，全程不校验租约（db.rs:1065-1119）。心跳丢失后，旧 owner 进入 finish 时会用旧树覆盖接管者写入的内容。连接池只设了 max_size(32)，没有 wait 超时（db.rs:103-107），池满时心跳会一直等下去，这恰好会触发上面的接管。import_session 也不检查租约（crates/rupi-server/src/http.rs:598-626）。

设计：persist_tree 在事务开头执行 SELECT run_id, instance_id FROM sessions ... FOR UPDATE，与调用方传入的值比较，不一致就回滚。finish、interrupt、import 三条路径都要传入 run_id。心跳返回 false 之后，finish 不再持久化。持久化失败时发 RUN_ERROR，code=persist_failed，并且不写缓存。deadpool 配置 wait、create、recycle 三个超时。这是第 23 项 fence 落地前的过渡方案。

### B6. frozen 记忆的缓存键改为按会话区分，修复同租户内跨会话泄露 [high/S]
本 track 范围：全部

上游参考：上游没有直接对应；这是 coding-agent 研究中发现的跨领域问题，已在代码中核实

差距：已核实。PostgresMemory::frozen 按会话范围查询，条件是 session_id IS NULL OR session_id=$2（crates/rupi-server/src/db.rs:1349-1353），但结果缓存在租户级的 key mem_frozen_key(tenant) 下（crates/rupi-server/src/run.rs:274-299，cache.rs:150，TTL 300 秒）。于是同一租户里，A 会话的会话级记忆会出现在 B 会话的 system prompt 里。cache.rs:153 已经定义了 mem_frozen_session_key，但没有被使用。

设计：run.rs 改用 mem_frozen_session_key(tenant, session)。写入租户级记忆时，让该租户所有会话级缓存失效，做法是在缓存值里带一个租户版本号。加一个测试：两个会话各写一条会话级记忆，彼此读不到对方的。

### B7. 图片 URL 抓取的 SSRF 修复（第一步），之后把图片移到外部存储 [high/S]
本 track 范围：第一步（URL 校验、禁重定向、私网/回环/元数据拒绝、限制数量与大小）

上游参考：docs/models.md:81-102（入口统一缩放并只编码一次）；ai.CHANGELOG.md 1.4.0 #11016

差距：已核实。fetch_image_url 只做前缀检查 starts_with(http://127.0.0.1)（crates/rupi-server/src/agui.rs:391），http://127.0.0.1.attacker.example 也能通过，控制面还能访问本机回环地址上的服务。reqwest 默认跟随重定向，攻击者可以借此访问内网，属于盲 SSRF。图片以 base64 内联写进 messages.blocks 和 Redis 里缓存的会话树，每一轮都要重放。

设计：第一步（S）：先解析 URL，只允许 https；解析出的 IP 如果是私网、回环、链路本地或元数据地址就拒绝；禁止重定向；限制图片数量。第二步（M）：在入口处按目录里的 image_limits 用 spawn_blocking 缩放一次，按 sha256 存入 S3ObjectStore，ContentBlock::Image 增加 object_ref 字段。所有 413 都当作上下文溢出处理。

### B8. 计费漏洞止血：取消和出错也要记账，压实计费，统一 token 口径，运行中检查预算 [critical/M]
本 track 范围：只做 server 侧：用量记账不再经过有损通道（同步累加或独立无损通道），drive 任何出口都 flush；每回合前检查当日预算

上游参考：pkg/durable/docs/spec.md §8.6（5171-5203，失败和中止的 attempt 也计入）；pkg/durable/src/harness/usage.ts:6-56；compaction.ts:200-206；packages/ai/src/api/openai-completions.ts:1521-1546（input 等于 prompt 减去 cacheRead 和 cacheWrite，并兼容 prompt_cache_hit_tokens）

差距：已核实。Usage 只在流正常结束时才发出（crates/rupi-agent/src/lib.rs:801-808），取消分支（:753-757）和错误分支都会提前 return。断开 SSE 会触发 CancelOnDrop（crates/rupi-server/src/http.rs:103-107），租户可以在流快结束时断开连接，拿到内容却不被计 token。ChatResponse 没有 usage 字段（crates/rupi-llm/src/lib.rs:148-151），压实时调用 provider.complete 的消耗从不计入。run.rs:339-348 只累加 input+output，而且是在一个有损通道的消费循环里 await PG，这个通道容量 256，满了就丢（run.rs:54-58）。parse_provider_usage 对 OpenAI 和 Gemini 把包含缓存的 prompt 记为 input，对 Anthropic 则不包含（rupi-llm/src/lib.rs:169-209），也不认 DeepSeek 的 prompt_cache_hit_tokens。配额只在准入时检查一次（crates/rupi-server/src/quota.rs:33-40）。

设计：rupi-agent 增加 with_usage_sink，由同步回调直接累加用量，不经过事件通道。取消和错误分支用 tokens.rs 估算用量，并标记 estimated；Anthropic 在 message_start 就先上报一次 input。ChatResponse 增加 usage 字段（serde default），压实的消耗以 kind=compaction 上报。TokenUsage.input 统一定义为不含缓存的输入；cache_read 依次读取 cached_tokens、prompt_cache_hit_tokens、顶层 cached_tokens；cache_hit_pct 和 TokenMeter 的校准都改用 input+cache_read+cache_write。drive 无论从哪个分支退出，都把用量 flush 到幂等的 run_usage(run_key, turn, attempt, kind)，run_key 由服务端生成。每一回合开始前用 Redis INCRBY 检查当日预算，超出就 cancel，并发 RUN_ERROR code=quota_exhausted。按套餐限制 thinking 档位（run.rs:376-382）。

### B11. 降低 PG 和执行面的固定负载：准入时不再全表 reap，reconcile 分片，执行器 stats 改为后台缓存 [high/S]
本 track 范围：只做 server 侧：准入不再全表 reap、reconcile 改成一条 GROUP BY、加 advisory lock；执行器 stats 缓存归 executor-output-bounds

上游参考：pkg/durable/docs/group-commit.md §1、§4-5（合并低价值的小写入）；telemetry.README.md:209

差距：已核实。每次 acquire_lease 都会跑一次全局的 reap_stale_runs（quota.rs:122，db.rs:936-947）。每个副本每 15 秒遍历一遍所有租户做 reconcile，每个租户一次 count(*)（crates/rupi-server/src/reclaim.rs:15-20）；10 万租户、50 个副本时约 33 万 QPS，而且和业务流量无关。/metrics、/ready 和每次 alloc 都要串行请求所有执行节点的 stats，客户端没有设超时（http.rs:76-89，pool.rs:134-145，crates/rupi-runtime/src/http.rs:22）。/metrics 也不鉴权。

设计：acquire 只检查自己会话的 run_id 是否过期；全局 reap 只在 reclaim 循环里做，加 LIMIT，并用 pg_try_advisory_lock 保证同一时间只有一个副本在跑。reconcile 改为每个 tick 执行一条 GROUP BY tenant_id，只修正计数不为零的租户。PoolScheduler 起一个后台 poller，每 5 秒并发拉一次 stats 并缓存，ranked、/ready、/metrics 都读缓存。reqwest 设连接超时和总超时。/metrics 移到 RUPI_METRICS_BIND 指定的内网端口。

### B12. 优雅下线（阶段 0）与 panic 隔离 [high/S]
本 track 范围：全部（profile 改动只加一个 server 专用 profile，不改默认 release）

上游参考：pkg/durable/docs/spec.md §5.1 close（2359-2370）、§5.4（2531-2536，close 不算崩溃）；chord spec.md §6-7（gate deadline）

差距：已核实。rupi-server 没有处理任何信号，也没有调用 with_graceful_shutdown；drive 是一个没人管的 tokio::spawn（run.rs:169）。滚动发布时，正在跑的 run 和崩溃一样整轮丢失，会话还会被锁住 180 秒。release profile 设置了 panic=abort（Cargo.toml:29），一个 run 里的 panic 会让整个副本退出，副本上所有租户正在跑的 run 都跟着丢失。

设计：App 增加 draining 标志和 RunRegistry，后者记录每个 run 的 CancelFlag 和 JoinSet。收到 SIGTERM 后：/ready 返回 503；/v1/agent 返回 503 并带 Retry-After；等待正在跑的 run 结束，最多等 drain_secs，terminationGracePeriodSeconds 要设得比它大；超时后 cancel，走现有的 finish 流程落盘并释放租约。新增 profile.release-server，继承 release 并改为 panic=unwind，同时修改 docs/LAUNCH.md:111。drive 外层 await JoinHandle，或者用 Drop guard，确保 release_lease、release_run 执行并发出 RUN_ERROR。排查 Mutex lock().unwrap() 在锁被 poison 后连锁 panic 的问题（例如 crates/rupi-agent/src/lib.rs:922）。

### B13. 闲置快照先 claim：引入 snapshotting 中间态 [high/S]
本 track 范围：全部

上游参考：chord pkg/chord/docs/spec.md §7 Gate、§12 Decisions「Gate admission」（exclusive 资源先关旧再开新，迟到的请求收到可重试的 upgrade_conflict）

差距：list_reclaim_candidates 既不加锁也不 claim（db.rs:1014-1063）。mark_snapshotted 无条件清空 handle（db.rs:949-964）。ensure_hot 用的是 start_run 在拿到租约之前读出的旧行（run.rs:82、230）。preempt 选候选时用的是 Duration::ZERO（reclaim.rs:41、49）。执行池紧张时，run 可能跑在已经被释放的卷上，多个副本也可能同时给同一个会话做快照。

设计：用一个短事务完成 claim：UPDATE sessions SET workspace_state=snapshotting WHERE run_id IS NULL AND workspace_state=hot AND runtime_handle=$h RETURNING，然后在事务之外做快照。mark_snapshotted 和回滚都要带上 snapshotting 状态和 handle 作为条件。set_run_id 增加条件 workspace_state 不等于 snapshotting，不满足时返回可重试的 409。ensure_hot 在拿到租约后重新读一次行。处于 snapshotting 超过 10 分钟的会话由 reaper 回滚。

### B14. 给持久化格式加版本：遇到不认识的格式时拒绝，不做降级 [high/S]
本 track 范围：只做 server 侧：load_tree 反序列化失败时报错而不是降级成纯文本，降级结果绝不写回

上游参考：chord pkg/chord/docs/spec.md §2、§7（schema 变了但版本号没升，就拒绝 reload）、§9 PROTOCOL_VERSION

差距：ContentBlock 是 tag=type 的枚举，没有表示未知类型的变体（crates/rupi-core/src/lib.rs:29-30）。load_tree 反序列化失败时，会静默把消息退化成纯文本（db.rs:1151-1155），工具调用的 id 随之丢失，调用和结果对不上。随后 persist_tree 把整棵树写回，损坏就永久留下了；Redis 里的树缓存还会把这个降级版本继续提供 300 秒。滚动发布、新旧副本同时在线时就会触发。

设计：messages 表增加 format 列，rupi-core 定义 MESSAGE_FORMAT 常量。读到比本副本更高的版本时返回 upgrade_required（503 并带 Retry-After），让客户端重试；另一个办法是给 ContentBlock 增加 Unknown(Value)，原样保留，但不发给模型。Redis 树缓存的 key 里带上版本号。上线顺序是先发布能读新格式的代码，再打开写新格式的开关。

### B15. 处理 length 截断：参数是坏 JSON 的调用不执行，max_tokens 按模型设置 [high/S]
本 track 范围：只做工具门：stop_reason 为 length（同时接受 "length"/"max_tokens"/"MAX_TOKENS"）或参数不是 JSON 对象（llm-wire-compat 会把坏 JSON 以 Value::String(raw) 交上来）的调用不执行、不送审批，合成 is_error 结果保持配对

上游参考：pkg/agent/src/agent-loop.ts:273-279、488；ai.CHANGELOG.md 1.3.0 #10897（argumentsError）、1.4.0 #9512

差距：AgentLoop 调用时 max_tokens 总是 None（crates/rupi-agent/src/lib.rs:707-714），所以 Anthropic、Bedrock、Gemini 都退回到 4096 的输出上限（anthropic.rs:84，已核实）。输出被截断后参数解析失败，结果变成 {}，但调用照样进入执行；在云端，参数为空的 write 还会触发一次审批。主循环只判断 stop_reason 是否为 error。

设计：把 stop_reason 归一为枚举 Stop、Length、ToolUse、Error、Aborted；ToolCall 增加 arguments_error 字段。遇到 Length 且消息里带工具调用时，这些调用不执行，也不送审批，而是各合成一个 is_error 结果，提示模型分块写入，这样调用和结果仍然一一配对。models.json 增加 maxOutput 字段，默认 max_tokens 取 maxOutput 与「上下文窗口减去估算输入再留余量」两者中的较小值，并按套餐设上限。

### B18. 只读工具并行执行 [medium/S]
本 track 范围：P2：云端只读工具并行

上游参考：pkg/durable/docs/spec.md:3678-3681（executionMode：一批里只要有一个 sequential，整批就串行）；pkg/durable/src/tools/file-mutation-queue.ts:28-40

差距：云端没有调用 with_tool_execution，用的是默认的 Sequential（run.rs:363-369；crates/rupi-agent/src/lib.rs:119-124）。模型一次发出 5 个 read 或 grep，控制面就要串行地向 execd 发 5 次 HTTP 请求。

设计：云端设置为 ToolExecution::Parallel，并发上限为 8。ToolDefinition 增加 execution_mode 字段，一批调用里只要有一个是 Sequential，整批就串行执行，防止租户把写操作设为自动放行后出现并发写。

### B19. 工作区资源发现：只扫白名单目录、标记为不可信、每个热工作区只加载一次 [medium/S]
本 track 范围：P1：资源发现白名单、标记不可信、每个热工作区缓存一次

上游参考：docs/security.md:25-57（AGENTS.md 一律当作不可信输入）；docs/skills.md 中的发现目录

差距：每次 run，load_remote_guidance 都会 glob **/SKILL.md 和 commands/*.md，最多要和执行面往返约 54 次，结果也不缓存（run.rs:862-966）。glob 默认使用 standard_filters，会跳过隐藏目录，所以 .rupi/commands、.agents/skills、.pi/skills 在云端都找不到；反过来，仓库里任何非隐藏路径下的 SKILL.md 都会被当成 skill 加载。注入时也不标注来源。

设计：skill 只在白名单目录里查找；给 Executor 增加一个受限的列目录接口，只允许白名单前缀，跳过符号链接和 ..。注入内容用 workspace-instructions trust=untrusted 包起来。按 (handle, 文件指纹) 缓存到 Redis，并提供 reload。commands 只在用户输入 /name 时才展开。

### B21. 模型目录记录能力，套餐限定可用模型，租户和会话设置真正生效 [high/M]
本 track 范围：P1：会话 model 参与路由（套餐白名单校验）、settings 里写了没生效的键要么生效要么从白名单删掉、max_turns 读设置并设上限

上游参考：pkg/ai/src/types.ts:840-887；pkg/ai/src/models.ts:1259-1290（clampThinkingLevel）；docs/models.md:81-112；pkg/coding-agent/src/core/sdk.ts:442-453

差距：已核实。ALLOWED_SETTING_KEYS 接受 enabledModels、compaction、tools、base_url 等键（crates/rupi-server/src/admin.rs:24-45），但服务端只读其中少数几个，其余写进去不起作用。会话上的 model 只用于回显，不参与路由（run.rs:323）。租户可以把 model 设成任意字符串，用平台 key 调用最贵的模型。目录里没有 maxOutput、模态、thinking 档位等信息。

设计：ModelEntry 增加 max_tokens、input、thinking_map、cost、prompt_cache、image_limits、compat 字段。新建 PG 表 model_catalog(provider, id, version, data, enabled, plans)，以内置 JSON 作为初始数据，副本在内存缓存，通过 pub/sub 失效。实际使用的模型依次取 sess.model、tenant.model、默认值，且必须在套餐白名单内，否则返回 400。compaction、tools、excludeTools、max_turns 从租户设置读取并设上限。白名单里没有云端含义的键删掉。增加 clamp_thinking；不支持视觉的模型改为收到占位文本。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F192 [P0] drive() 早退不释放 run 租约与并发槽：空 messages 一次请求即可把租户锁 ~3 分钟
类型：bug，工作量：S，文件：crates/rupi-server/src/run.rs

证据：start_run 在 run.rs:130-161 先 admit_run（Redis 并发 +1、日账本 +1）再 acquire_lease（PG sessions.run_id）。随后 run.rs:168-175 spawn 的任务对 drive 的 Err 只 emit RUN_ERROR，不调 finish/release_lease/release_run。drive 内多处 `?` 在租约之后：apply_state(195)、load_tree(205/214/217/220)、ensure_hot(230)、pending_interrupts(315)、`last_user_from_input(...).ok_or_else(...)?`(391-393)、insert_interrupt/persist_tree/load_tree(450-455)。实测：POST /v1/agent 带 `messages: []` 时依次返回 RUN_STARTED、MESSAGES_SNAPSHOT、STATE_SNAPSHOT、RUN_ERROR "messages must end with a user turn"；PG 里 sessions.run_id='leak1' 残留；同租户另一个会话在 +0s/+20s/+40s 都收到 429 {"error":"quota"}（当时 Redis 降级，quota.rs:79 收紧到 1 并发）。有 Redis 时则泄漏一个并发槽，Redis 计数要靠 reclaim 每 15s reconcile、PG 要靠 reap_stale_runs(180s) 才恢复。

建议修法：把消息校验（末条必须是 user，或 resume 非空）提前到 start_run 里、admit 之前，返回 400。drive 用 guard/finally 结构包起来，任何退出路径都执行 finish 或至少 release_lease + release_run + hb.abort()。

### F193 [P0] 一条助手消息含多个工具调用且其一需审批：其余调用永不执行、无结果，续跑历史断裂
类型：bug，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-server/src/run.rs

证据：AgentLoop 第一阶段逐个检查策略。Ask+Interrupt 时 lib.rs:922-932 直接 `return Ok(StopReason::Aborted)`：同批里已放行的调用（排在前面的 read）和排在后面的调用都不执行，也不写 Tool 消息。PendingInterrupt 只存一个（lib.rs:923），服务端只落一个 interrupt（run.rs:439-450）。resume_and_continue 只为该 interrupt 推一条 ToolResult（run.rs:667-677），然后 continue_session。实测剧本是一条消息 [read c-read, write c-write]：先后发出 TOOL_CALL_START c-read 与 c-write，但只有 c-write 产生 interrupt；批准后 DB 里只有 tool_result c-write，c-read 没有执行也没有结果。真实 OpenAI/Anthropic 要求每个 tool_call 都有对应结果，这段历史在后续每一轮都会被拒（400），会话永久不可用。两个 ask 工具同批时，第二个直接丢失。

建议修法：AgentLoop 遇到 Interrupt 时先跑完本批其余已放行的调用，把需要审批的调用全部收集为多个 PendingInterrupt 一次性返回。服务端为每个调用落一条 interrupt，resume 时按原序合成同一条 Tool 消息，保证每个 tool_call 都有 ToolResult（未批准的写 denied）。

### F228 [P0] 云端 on_ask Interrupt 遇到同一轮多个工具调用时，只挂起第一个，其余调用（包括已放行的）丢失且没有 ToolResult，resume 后请求 400
类型：bug，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-server/src/run.rs

证据：一轮里的 assistant 消息（含全部 ToolCall）在 lib.rs:814 先入树。第一阶段循环（882-987）遇到 AskAction::Interrupt 时，往 pending_interrupt 里写一个值，然后立刻 return Ok(Aborted)（922-933）。此前已经放进 pending 的已放行调用不会执行，之后的调用也不再处理。服务端只取一个 interrupt（run.rs:439 take_pending_interrupt → insert_interrupt），resume 时只为这些 interrupt 补 ToolResult（528-684），然后 continue_session（685-697）。探针 P1：两个 write 调用加 Interrupt，结果 stop=Aborted，pending=Some("c0")，tool_calls=2，tool_results=0，第二次 take 为 None。resume 之后 c1 没有结果，会话里留下悬空的 tool_use，OpenAI/Anthropic 会直接拒绝请求。

建议修法：第一阶段先完整跑完所有调用的门：Allow/Deny 照常处理，Ask+Interrupt 的调用都收集起来。pending_interrupt 改成 Vec，已放行的调用照常执行并把结果入树。云端为每个 interrupt 插一行，resume 时逐个回填，全部回填后才 continue。另外在 continue_session 入口加一道检查：发现悬空 ToolCall 就补上 "cancelled" 结果。

### F194 [P0] 云端 tenant 记忆在其他会话的冻结提示里不可见；冻结缓存按租户键却按会话取数
类型：bug，工作量：S，文件：crates/rupi-server/src/memory.rs, crates/rupi-server/src/db.rs, crates/rupi-server/src/run.rs, crates/rupi-server/src/cache.rs

证据：memory 工具 add 无论 scope 都写 `session_id = Some(&self.session_id)`（memory.rs:120-131）。list_memories 过滤条件是 `session_id IS NULL OR session_id = $2`（db.rs:1348-1355），所以别的会话永远读不到。实测：会话1 用 memory 工具写 `[core] tenant-wide fact oolong`（scope=tenant），行的 session_id=会话1；按会话2 查冻结记忆返回空串。工具回包却写着 "Takes effect in prompt next session"（memory.rs:163）。另外，冻结缓存键是租户级的 `{tenant}:mem:frozen`（run.rs:275-301），内容却来自 `pgmem.frozen()` 的会话级查询，同租户并发会话 300s 内会读到彼此的会话私有记忆。按会话设计的 `mem_frozen_session_key`（cache.rs:153）只在 invalidate 里删（cache.rs:230），从来没有写。现有测试只断言 search_memories 能命中（cloud_first_cut.rs:367-370），没有覆盖下个会话的提示词。

建议修法：scope=tenant/global 的记忆写入时 session_id 置 NULL，只有显式 session/workspace 作用域才绑定会话。冻结缓存改用会话级键，或改成只缓存租户级记忆。补一条测试：会话 A 写入后，会话 B 的 system prompt 应包含该记忆。

### F206 [P1] 云记忆 replace/remove 语义与本机不一致且易误删：无 id 时覆盖最新一行
类型：bug，工作量：S，文件：crates/rupi-server/src/memory.rs, crates/rupi-server/src/db.rs

证据：replace 不带 id 时执行 `UPDATE … WHERE id = (SELECT id … ORDER BY created_at DESC LIMIT 1)`（db.rs:1240-1247），覆盖的是租户最新的一条记忆（可能来自别的会话），与 entry 内容无关。本机约定是 `OLD ||| NEW`（rupi-memory/src/lib.rs:406-412），模型按这个约定调用会把最新一行改成字面量 "OLD ||| NEW"。remove 要求全文精确匹配（db.rs:1266-1270），本机是子串删除（lib.rs:414-416）。工具输出从不暴露 id（memory.rs:155-163，而且用 "" 拼接，没有分隔符），schema 里的 id 字段模型无从得知。layer 只在 insert 时按 [core]/[user]/[failure] 计算（db.rs:1202-1210），replace 后不会重算。

建议修法：支持 `OLD ||| NEW` 并按子串匹配定位行，replace 时重算 layer。列表输出带 id 和换行分隔。没有匹配时返回错误，不能盲改最新一行。

### F234 [P1] hook 已经拒绝的调用仍会走 policy 和审批（Option::or 急切求值）：用户被问一个不会执行的调用，云端还会因此挂起
类型：bug，工作量：S，文件：crates/rupi-agent/src/lib.rs

证据：lib.rs:909-910 是 hook_denied.or(match self.policy.decide(&name,&args) { ... Decision::Ask => approver.approve / on_ask ... })。or 的参数在调用前就求值，hook 拒绝之后依然会执行 policy.decide、UiPromptStart、approve() 和 UiPromptEnd，云端 on_ask 返回 Interrupt 时还会直接挂起。探针 P16：DenyToolsHook([bash]) 加 ask_tools=[bash] 加计数审批器，结果 approver_called=1，事件序列为 [TurnStart, ToolStart, UiPromptStart, UiPromptEnd, ToolEnd, ...]。现有测试 hook_deny_short_circuits_before_execution 只断言没有执行，没有断言没问审批。

建议修法：改成 `let denied = match hook_denied { Some(d) => Some(d), None => match self.policy.decide(..) {..} };` 或 or_else 加闭包，并补测试断言 approver 没被调用、没有 UiPrompt 事件。

### F235 [P1] 工具过滤只隐藏定义，不拦截执行：--no-tools 和 --exclude-tools memory 下 memory/skill 路由工具照样能执行，提示里还在引导模型写记忆
类型：bug，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-memory/src/lib.rs

证据：run_turns 只对 all_tools 做 retain(tool_visible)（lib.rs:653-656），execute_allowed（465-524）不检查可见性。memory、memory_search、session_search 走 mem.handle_tool_call（514），load_skill/read_resource 走 skills（491-510），都绕过了 apply_tool_filter（那只对 ToolRegistry 生效）。PromptBuilder 无条件拼 mem.system_block，其中包含 MemoryGuidance（memory lib.rs:709-715）。探针 P17：with_tool_filter(Some([]), ["memory"]) 下模型看到的工具数为 [0,0]，可脚本里的 memory 调用照样把 INJECTED-FACT 写进了 $TMPDIR/rupi-core-probe-p17-*/memories/MEMORY.md，system 里仍有 MemoryGuidance。

建议修法：在第一阶段门里加一步：!self.tool_visible(&name) 时返回 ToolOutput::err("tool {name} is disabled")（search_tools 和发现机制只隐藏 schema 的情况另行处理）。memory 工具被过滤时，PromptBuilder 不拼 MemoryGuidance。

### F198 [P1] AGENTS.md / commands / RunAgentInput.context 每轮作为 System 节点追加进持久会话树（且同时进 system prompt）
类型：bug，工作量：S，文件：crates/rupi-server/src/run.rs

证据：load_remote_guidance 对每个工作区说明文件既 `tree.push(Message::text(Role::System, block))`（run.rs:880-889），又把同一段拼进 builder.append（run.rs:370-375）。commands/*.md 整段 push（run.rs:956-959），context 每轮 push（run.rs:394-401）。实测两轮之后，库里有 `[workspace AGENTS.md]`×2、`[workspace agents.md]`×2、`[context: ui]`×2：macOS 大小写不敏感，AGENTS.md 和 agents.md 读的是同一个文件，被算了两次。Anthropic/Gemini 的历史转换把 System 映射为 user 轮（anthropic.rs:147、gemini.rs:111），等于每轮注入伪造的用户消息；这些节点还会出现在 MESSAGES_SNAPSHOT 和 session_search 里。

建议修法：说明文件和 context 只放进本轮 system prompt（对齐本机 PromptBuilder.context_dirs 每轮重读），不入树。文件名按大小写不敏感去重。commands 改为按 `/name` 展开，而不是整段注入。

### F199 [P1] `.rupi/commands/*.md` 与隐藏目录下的 SKILL.md 永远发现不到（glob 跳过隐藏目录）
类型：bug，工作量：S，文件：crates/rupi-server/src/run.rs, crates/rupi-tools/src/lib.rs

证据：run.rs:891-899 用 glob `**/SKILL.md`，run.rs:925 用 glob `.rupi/commands/*.md`，都经执行面的 glob 工具。该工具用 `WalkBuilder::standard_filters(true)`（rupi-tools/src/lib.rs:1176-1179），会过滤隐藏目录。实测：在工作区放 `.rupi/commands/hello.md`（内容 cmd-marker），两轮后库里 cmd-marker 计数为 0。UPSTREAM.md:79 记载的发现目录 `.pi/skills`、`.agents/skills`、`.rupi/skills` 在云端因此全部失效，只有非隐藏的 `skills/` 能用（cloud_hardening.rs:637-640 的测试正好用的是这个）。

建议修法：给 glob 请求加 hidden 选项，或由执行面提供专用的 list-skills/commands 接口，按本机 rupi-skills 的发现目录列出。

### F200 [P1] 云端 skills 只做了一半：read_resource 不可用，控制面每轮写 temp 目录且不清理，无斜杠命令和自积累
类型：incomplete，工作量：M，文件：crates/rupi-server/src/run.rs, crates/rupi-skills/src/lib.rs

证据：run.rs:311 用 `SkillRegistry::default()`，再逐个 `skills.ingest_markdown(path, &md.content)`（run.rs:919）。ingest_markdown 只把 SKILL.md 正文写到控制面本机的 `temp_dir()/rupi-remote-skills/{pid}-{nanos}/{name}/SKILL.md`（rupi-skills/src/lib.rs:247-264），每次 run 每个 skill 都新建一个目录，从不删除（本机 TMPDIR 里已有残留），这与无状态控制面的定位相悖。references/、scripts/ 不会被复制，read_resource（lib.rs:573-585）读到的是控制面临时目录，必然失败。云端没有 `/skillname args` 展开（run_with_user 直接用原文，run.rs:402-415），没有 with_reviewer 自积累。

建议修法：实现一个以 Executor 为后端的 SkillSource：load_skill/read_resource 通过 fs_read 读工作区，不落控制面磁盘。在 run_with_user 前做斜杠命令展开。

### F201 [P1] MESSAGES_SNAPSHOT 中一条 Tool 消息只带第一个 ToolResult，同批其余结果丢失
类型：bug，工作量：S，文件：crates/rupi-server/src/agui.rs

证据：AgentLoop 把同一轮的所有结果放进一条 Role::Tool 消息（rupi-agent/src/lib.rs:1088-1094）。message_to_agui 用 `.find()` 只取第一个 ToolResult 设置 toolCallId/content（agui.rs:539-550）。实测：一轮里有 glob g1 和 grep g2，下一轮快照里 tool 消息只有 [('g1','')]，助手的 toolCalls 却是 ['g1','g2']。is_error 也丢了。从快照恢复 UI 的 AG-UI 客户端会看到一个没有结果的工具调用。

建议修法：tree_to_agui_messages 把含多个 ToolResult 的消息拆成多条 role=tool 的 AG-UI 消息，每条带自己的 toolCallId（可选附 error）。补快照内容测试。

### F196 [P1] 会话 model / AG-UI state.model 被接受、回显但不参与选 provider
类型：bug，工作量：S，文件：crates/rupi-server/src/run.rs, crates/rupi-server/src/lib.rs, crates/rupi-server/src/http.rs

证据：POST /v1/sessions 的 model 写入 sessions.model（http.rs:264-275），state.model 经 apply_state → update_session_meta 写库（run.rs:700-721）。但 provider 只看租户：`app.provider_for(&tenant)`（run.rs:313）→ default_provider/resolve_byok 只读 tenant.settings.model 或 tenant.default_model（run.rs:790-797）。sess.model 只回显到 STATE_SNAPSHOT（run.rs:323、462）。客户端看到的是切换后的模型，实际跑的是租户默认模型。thinking 档位走的是 sess.thinking_level（run.rs:376-382），这一项是生效的。

建议修法：resolve_byok 接收可选的会话 model（优先级：state/会话 > 租户 settings.model > default_model），provider_factory 签名改为 (tenant, session)。

### F197 [P1] load_tree 忽略 parent、按 created_at 拍平 current_path：顺序错乱，分支被交错
类型：bug，工作量：M，文件：crates/rupi-server/src/db.rs

证据：persist_tree 按 Message.created_at 排序插入（db.rs:1078-1079），load_tree 按 `ORDER BY created_at` 读出后把每一行都 push 进 current_path（db.rs:1130-1168），完全不用 parent 列。Message.created_at 是构造时间而不是入树时间，mock 剧本（cached_mock 构造于 provider 创建时）里的助手消息早于用户消息。实测 DB 顺序为 assistant 'turn-one'、assistant 'turn-two'、user 'first'、user 'second'，后续 MESSAGES_SNAPSHOT roles=['assistant','user']。context system 消息在 user 消息之后构造却先 push（run.rs:391-401），重载后也会倒序。导入带分支的 Pi JSONL 时，所有分支节点会被拍成一条线性历史送给模型。

建议修法：按 parent 链重建树：持久化当前叶子（如 sessions.leaf_id），load 时从叶子回溯得到 current_path，其余节点只作为分支。排序依据改成入树序号列，而不是 Message.created_at。

### F203 [P1] SSE 队列满时静默丢任意事件（含 TEXT_MESSAGE_END / RUN_FINISHED）
类型：bug，工作量：M，文件：crates/rupi-server/src/run.rs

证据：emit 用 try_send，Full 时只打 warn（run.rs:23-26、48-58）。AgentLoop → mapper 也一样（run.rs:54-58、359-361），容量各 256。单测 sse_emit_drops_when_backlog_full 把丢事件当作预期行为（run.rs:1073-1081）。模型 token 级 delta 很快，慢客户端（移动网络）可能丢失 TEXT_MESSAGE_END、TOOL_CALL_END、RUN_FINISHED，流结构被破坏，客户端永远等不到结束。

建议修法：生命周期事件（START/END/RESULT/RUN_*）用 await send（配超时，超时就取消 run）。只对 *_CONTENT delta 做合并或丢弃，并在丢弃后补发整段内容。

### F204 [P1] GET /v1/settings 明文返回 git_token（与管理面掩码列表不一致）
类型：bug，工作量：S，文件：crates/rupi-server/src/http.rs, crates/rupi-server/src/admin.rs

证据：租户 get_settings 只掩码 openai_api_key/api_key/anthropic_api_key/gemini_api_key（http.rs:175-180），管理面的 SECRET_SETTING_KEYS 还包含 git_token（admin.rs:49-55）。实测 PATCH {"git_token":"ghs_example…"} 后，GET /v1/settings 原样返回明文。任何持有该租户 API Key 的集成都能读出 git 凭证。

建议修法：get_settings 直接调用 `crate::admin::mask_settings`，删掉重复的列表。

### F205 [P1] git bootstrap 失败被吞：file:// 或不存在的仓库仍返回 201 ready
类型：bug，工作量：S，文件：crates/rupi-server/src/http.rs

证据：create_session 里 `let _ = app.executor.bootstrap(&wh, boot).await;`（http.rs:345），同样忽略了 set_runtime/mark_hot 的错误（http.rs:309-320）。实测 `bootstrap:"git", git_url:"file:///etc"` 和一个不存在的 GitHub 仓库都返回 201，runtime.status="ready"，客户端无从得知 clone 失败（执行面 git.rs:43 实际会拒绝 file://）。

建议修法：bootstrap 返回 Err 或 ToolText.is_error 时 teardown 会话并返回 400/502，附上执行面的错误文本。mark_hot 失败时也应回滚。

### F207 [P1] settings 白名单里 13 个键被接受却无任何效果；max_turns 写死 12
类型：incomplete，工作量：M，文件：crates/rupi-server/src/admin.rs, crates/rupi-server/src/run.rs

证据：ALLOWED_SETTING_KEYS（admin.rs:24-47）允许 thinking、theme、tools、defaultTools、excludeTools、exclude_tools、compaction、steeringMode、followUpMode、defaultProjectTrust、externalEditor、enabledModels、base_url。grep 结果显示服务端源码除白名单本身外没有读取任何一个。实测 PATCH {"excludeTools":["bash"],"thinking":"high","compaction":{"enabled":false}} 返回 200 ok，但没有效果。base_url 尤其关键：ProviderOptions 只有 api_key/provider（rupi-llm/src/route.rs:10-13），租户无法 BYOK 到自建网关。AgentLoop::new(12) 写死（run.rs:363），本机默认 20（rupi-cli/src/main.rs:93-94）且可配。

建议修法：把 excludeTools/tools 映射到 with_tool_filter，thinking 映射到 with_thinking，compaction 映射到 with_compression，base_url 传入 ProviderOptions（需要扩展 rupi-llm），新增 maxTurns。其余纯 UI 键（theme/externalEditor）从云端白名单移除，或明确拒绝。

### F208 [P1] 图片 URL 抓取可经重定向 SSRF 到内网 http，且先读完整个 body 才判大小
类型：bug，工作量：S，文件：crates/rupi-server/src/agui.rs

证据：fetch_image_url 只校验初始 URL 是 https 或回环 http（agui.rs:391-393）。reqwest Client 没有设置 redirect 策略（agui.rs:394-396），默认最多跟随 10 跳，https 地址可以 302 到 http://169.254.169.254/ 或内网服务，由控制面发出请求。`resp.bytes().await?` 读完整个响应后才检查 2MB（agui.rs:413-416），也没有检查 Content-Length，超大响应会占满内存（只有 5s 超时）。每张图都新建一个 Client。

建议修法：设置 `redirect::Policy::none()`（或在每一跳校验 https 和非私网 IP），解析出的 IP 拒绝私网和链路本地段。先查 Content-Length，再用 chunk 流式累计，超过上限立即中止。复用一个共享 Client。

### F211 [P1] Redis 会话树缓存命中仍查库（unwrap_or 急求值）；persist_tree 每轮全删全插
类型：perf，工作量：S，文件：crates/rupi-server/src/run.rs, crates/rupi-server/src/db.rs

证据：run.rs:202-206：`serde_json::from_str(&raw).ok().unwrap_or(db::load_tree(...).await?)`。unwrap_or 的参数会先求值，缓存命中时也会完整执行 load_tree，而且库报错时整轮失败，缓存形同虚设。persist_tree 每次 run 都 `DELETE FROM messages WHERE session…` 再逐行 INSERT 全部节点（db.rs:1071-1105），finish 每轮都调用（run.rs:724），触发 content_tsv 生成列和两个 GIN（trgm/tsv）索引的全量重建。长会话的写放大随轮数成二次增长。

建议修法：改成 `match … { Some(t) => t, None => db::load_tree(...).await? }`。persist_tree 改为增量：`INSERT … ON CONFLICT (id) DO NOTHING` 只写新节点，另外单独更新 summary 列。

### F212 [P1] 历史快照对象、workspace_snapshots 与 interrupts 行泄漏；workspace_snapshots 只写不读
类型：bug，工作量：M，文件：crates/rupi-server/src/reclaim.rs, crates/rupi-server/src/http.rs, crates/rupi-server/src/db.rs

证据：每次闲置回收都用新的 handle id 生成一个对象 key `{region}/ws/{tenant}/{session}/{handle}.tgz`（reclaim.rs:75-79），并插入 workspace_snapshots（reclaim.rs:80-87）。删除会话时只删当前的 row.snapshot_key（http.rs:407-409），更早的快照对象永久残留。grep 显示 workspace_snapshots 在 src 里没有任何 SELECT。interrupts、workspace_snapshots、quota_ledger 没有外键（db.rs:258-285），删除会话或租户后会留下孤儿行。

建议修法：回收时删除旧的 snapshot_key 对象（或固定按会话一个 key 覆盖写）。删除会话/租户时按 workspace_snapshots 清理全部对象和行，interrupts 加 ON DELETE CASCADE。如果不打算读 workspace_snapshots，就删掉这张表。

### F217 [P2] 请求体字段风格不一致导致静默忽略：fork 只认 entry_id，未知 backend 静默回落
类型：bug，工作量：S，文件：crates/rupi-server/src/http.rs

证据：ForkBody 没有 rename（http.rs:425-429），其他接口返回 camelCase，RPC 用的是 "entryId"（rupi-cli/src/rpc.rs:50）。实测 {"entryId":"nope"} 返回 200（被忽略，fork 了整条路径），{"entry_id":"nope"} 返回 400。CreateSession 的 git_url/git_token/git_hosts 也是 snake_case（http.rs:235-253）。`backend:"docker"` 经 BackendKind::parse 得到 None 后用默认后端，实测返回 201。

建议修法：统一 `#[serde(rename_all = "camelCase")]` 并为 snake 写法保留 alias。无法解析的 backend/region 返回 400。

### F220 [P2] run 结果语义粗糙：MaxTurns/Aborted 报 success，editedArgs 不回写历史，resume 时附带的新消息被丢
类型：bug，工作量：S，文件：crates/rupi-server/src/run.rs

证据：agent 返回 Ok(MaxTurns) 或 Ok(Aborted)（心跳失租导致 cancel）时都走 run_finished_success（run.rs:502-508）。批准时带的 editedArgs 用于执行（run.rs:584-594），但树里助手的 tool_call 参数仍是原值，模型看到的是原参数加上新结果。resume 分支完全不读 input.messages（run.rs:384-390），客户端在 resume 时附带的新 user 消息被静默丢弃。同一个 runId 重试会重复追加 user 轮（没有按 message id 或 runId 去重）。

建议修法：MaxTurns/Aborted 用 RUN_ERROR 或在 outcome 里附 reason。editedArgs 回写到对应 ToolCall 块。resume 带新 user 消息时，在工具结果之后追加。按 runId 或 message id 去重。

### F221 [P2] 租户 mock provider 缓存永不失效（改剧本或切回真实 provider 都不生效）
类型：bug，工作量：S，文件：crates/rupi-server/src/lib.rs

证据：provider_for 先查 mock_providers 缓存，命中就返回（lib.rs:137-153），从不对照当前 settings。实测：剧本跑完后管理面 PATCH 一个新 mock_script，下一轮仍返回耗尽后的默认 "done"，而不是 "second-script"。租户一旦用过 mock，即使改成真实 provider 也要重启进程才生效。

建议修法：缓存键加上 mock_script 的哈希，或者在 patch_settings 时清掉该租户的缓存项。

### F222 [P2] 后台与入院路径的全表扫描：每 15s 全租户 reconcile，每次入院全局 reap；会话列表无分页
类型：perf，工作量：M，文件：crates/rupi-server/src/reclaim.rs, crates/rupi-server/src/quota.rs, crates/rupi-server/src/db.rs

证据：loop_forever 每个 tick 都对 list_tenant_ids 的所有租户执行 reconcile（一次 count 查询加一次 Redis SET，reclaim.rs:14-20），每个副本独立执行，开销 O(租户数×副本数)/15s。reconcile 直接用 PG 计数覆盖 Redis 计数，与“admit 已 incr、set_run_id 尚未写入”的窗口竞争，会短暂多放行。acquire_lease 每次都执行全局 `UPDATE sessions … WHERE run_id IS NOT NULL AND updated_at < …`（quota.rs:122、db.rs:936-947）。GET /v1/sessions 一次返回全部会话（db.rs:711-722）。tenant_of 每个请求查一次库（http.rs:109-134），没有缓存。

建议修法：reconcile 只处理活跃租户（有 run_id 的 DISTINCT tenant_id），并用 advisory lock 保证单副本执行。reap 只在后台循环里做。/v1/sessions 加 limit/cursor。API Key 查询结果在 Redis 短 TTL 缓存。

### F225 [P2] 会话存在性泄露：别租户的会话 id 返回 403、不存在返回 404
类型：bug，工作量：S，文件：crates/rupi-server/src/http.rs, crates/rupi-server/src/run.rs

证据：session_guard 在查不到本租户会话时再用 session_owner 查全表，属于其他租户就返回 403（http.rs:136-151），AG-UI 预检逻辑相同（run.rs:91-105）。实测租户 B 访问租户 A 的会话 id 返回 403 {"error":"forbidden"}，随机 id 返回 404。数据本身没有泄露，但构成了跨租户的存在性探测；id 是 UUIDv4，实际风险低。

建议修法：对外一律返回 404，403 只记日志，同时省掉一次全表查询。

### F252 [P2] 计划模式只拦 write/edit/bash 三个名字，memory 写入、MCP/扩展工具和可选工具照常执行
类型：incomplete，工作量：S，文件：crates/rupi-agent/src/lib.rs

证据：is_mutating 只匹配 write | edit | bash（lib.rs:527-529），plan 兜底只看这个函数（972-980）。memory(op=add/replace/remove)、MCP 工具（任意副作用）、外部扩展工具、subagent 在 plan 模式下都不受限制（subagent 虽然继承了 plan，但子 agent 照样能调 MCP）。PlanMode 提示（703）只点名了这三个工具。

建议修法：把逻辑改成白名单：plan 模式下只放行只读工具（read/glob/grep/find/ls/think/load_skill/read_resource/memory_search/session_search/search_tools），以及在 settings 里显式声明为只读的 MCP 工具，其他一律拒绝。

### F143 [P1] bash 危险命令名单用子串匹配：`rm -rf /tmp/x` 被误拦，`rm -fr /` 和 `rm -rf  /` 绕过
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/policy.rs

证据：main.rs:1434 默认名单 `["rm -rf /", "mkfs", "dd if="]`，policy.rs:98 用 `cmd.contains(b)` 做子串匹配。实测：`rupi --model deepseek-v4-flash run "Run this exact bash command: rm -rf /tmp/rupi-e2e/does-not-exist-dir && echo removed"` → `[bash error]`，模型转述 `matched the substring rm -rf / inside rm -rf /tmp/...`。正常删除临时目录被拦下，run 模式默认直接拒绝。反过来，`rm -fr /`、`rm -r -f /`、多一个空格都不在名单里。`dd if=/dev/zero of=/dev/null count=1` 这种无害命令也被拦（chat 里出现 `denied (approval required): bash command matched blocklist: dd if=`）。

建议修法：命令先做 shell 分词（shlex），再按 argv 规则判定：rm 同时带 -r 和 -f（顺序不限），且目标是 `/`、`/*`、`~` 或 `$HOME`；dd 只在 of= 指向块设备时才拦。为误拦和漏拦两类各补单测。

### F216 [P1] 关键行为没有测试覆盖
类型：test-gap，工作量：M，文件：crates/rupi-server/tests

证据：grep tests/*.rs 没有任何用例涉及：/import、MESSAGES_SNAPSHOT 内容、fork 的 entry_id/entryId、state 中的 thinkingLevel/autoCompaction/model、debug-run、图片输入、format=html 导出。也没有覆盖多工具调用加 interrupt、早退释放租约、跨会话冻结记忆。本报告中的高危问题（租约泄漏、多工具中断、跨会话记忆）都落在这些空白里。

建议修法：为上述每个高、中危问题各补一个集成用例（mock 剧本已支持多 tool_call），尤其是早退后同租户立即可以再跑、多工具中断后历史完整、会话 B 能看到会话 A 的 tenant 记忆。

## 补充分配的问题

### F202 [P1] AG-UI 流与快照 id 不一致；reasoning 在正文之后整段伪分片；工具参数不流式
类型：incomplete，工作量：L，文件：crates/rupi-server/src/agui.rs, crates/rupi-agent/src/lib.rs

证据：TEXT_MESSAGE_START 的 messageId 是新生成的 uuid（agui.rs:199），TOOL_CALL_RESULT 的 messageId 也是随机值（agui.rs:248），都与持久化的 Message.id 不同，而 MESSAGES_SNAPSHOT 用的是 Message.id（agui.rs:535）。按 id 合并的客户端（CopilotKit 等）会出现重复气泡。TOOL_CALL_START 没有 parentMessageId。StreamEvent 只有 TextDelta/Usage 两个变体（lib.rs:730-746、788-799 是穷尽匹配），思考内容在整轮完成后才由 emit_thinking 发出（lib.rs:818），所以 REASONING_* 总是排在 TEXT_MESSAGE_* 之后，按 16 字符切成伪增量（agui.rs:136-137、258-275）。TOOL_CALL_ARGS 同样是完整参数切片（agui.rs:225-234）。

建议修法：让 AgentEvent 携带助手消息 id，并在 TurnStart 前确定。TEXT/TOOL 事件使用持久化 id，并补 parentMessageId。provider 层增加 ThinkingDelta/ToolArgsDelta 流事件。

### F209 [P1] 管理 UI：带查询串的 hash 路由全部落到“概览”（会话筛选、租户会话链接、详情链接失效）
类型：bug，工作量：S，文件：crates/rupi-server/static/admin.js

证据：parseHash 直接按 '/' 切分 location.hash，没有先去掉 '?…'（admin.js:91-100）。用 node 跑同一函数：'#/sessions' → sessions，'#/sessions?tenantId=abc' → overview，'#/sessions?open=xyz&status=running' → overview。受影响的地方：会话筛选表单提交（admin.js:434-442）、租户页“查看该租户会话 →”（admin.js:309）、会话 id 链接 `#/sessions?open=`（admin.js:398）都会跳回概览，筛选功能完全不可用。

建议修法：parseHash 先执行 `h.split('?')[0]` 再切分路径；hashQuery 保持不变。

### F210 [P1] 管理审计只覆盖部分写操作，UI 也没有审计页
类型：incomplete，工作量：S，文件：crates/rupi-server/src/admin.rs, crates/rupi-server/static/admin.js

证据：write_audit 只在 delete_tenant、create_key、patch_quota、patch_settings、debug_run 中调用（admin.rs:629、674、801、856、992）。create_tenant（480-520）、patch_tenant（567-604）、revoke_key（693-712）、delete_session（964-977）、create_admin_key（1111-1141）、revoke_admin_key（1143-1164）都没有审计。实测创建租户、颁发 key、改名之后，/admin/api/audit 只有 ['create_key','debug_run']。admin.js 里没有任何 audit 字样，/admin/api/audit 在 UI 里不可见。debug_run 在校验之前就写审计（admin.rs:992-1004）。

建议修法：所有 require_write/require_manage_keys 的成功路径都写审计（尤其是管理 Key 的颁发和吊销），debug_run 在校验通过后再写。UI 增加审计列表页。

### F213 [P1] 云集成测试缺依赖时以 ok 静默“通过”；Postgres 探测是死代码；macOS CI 假绿
类型：test-gap，工作量：S，文件：crates/rupi-server/tests/cloud_first_cut.rs, crates/rupi-server/tests/common/mod.rs, crates/rupi-server/src/db.rs, crates/rupi-server/src/cache.rs, .github/workflows/ci.yml

证据：9 个测试文件都会回退到硬编码 URL（例如 cloud_first_cut.rs:19-30）。lock_harness 连不上就返回 None（common/mod.rs:11-29），Redis 用 get_connection 探测（cloud_first_cut.rs:37-40），测试随即 `return`，结果记为 ok 而不是 ignored。`db::connect(...).is_err()` 分支（cloud_first_cut.rs:33-35）不可能触发：deadpool 的 `Pool::builder().build()` 不建连接（db.rs:103-107），所以 "skip: no postgres" 永远不会打印。ci.yml 的 macOS 矩阵执行 `cargo test --workspace` 时没有 DB，约 35 个云用例全部报绿，实际什么都没跑。cache.rs:451-491 的两个 Redis 测试同样静默返回。测试还会对 DATABASE_URL 指向的库执行 TRUNCATE（db.rs:1579-1586），而 LAUNCH.md:122 让用户在同一个 shell 里 export 同名变量来起服务。

建议修法：增加 RUPI_REQUIRE_CLOUD=1（cloud job 设置），设置时缺依赖直接 panic。否则用 eprintln 打印跳过原因，或改为 #[ignore] 并在 cloud job 用 --ignored 跑。测试库改用专用变量（如 RUPI_TEST_DATABASE_URL），避免误清开发库。

### F214 [P1] 本机实测：3 个云用例在 macOS 必败或不稳，且有一个在 macOS 上断言恒真
类型：test-gap，工作量：S，文件：crates/rupi-server/tests/cloud_fts.rs, crates/rupi-server/tests/cloud_hardening.rs, crates/rupi-server/tests/cloud_first_cut.rs

证据：环境：临时 PG16 + redis:7.4，`CARGO_TARGET_DIR=/tmp/rupi-ro-target cargo test --locked --no-fail-fast -p rupi-server`。(1) cloud_fts.rs:132 失败，"pg_trgm word_similarity 对中文词为 0"：initdb --locale=C 下 show_trgm('乌龙茶')={}，word_similarity=0；同一测试在 UTF-8 库上通过（ILIKE 和 n-gram 仍能命中，只是 db.rs:1408 的 trgm 分支失效），LAUNCH.md 没有说明 locale 要求。(2) cloud_hardening.rs:605 失败：macOS 上命令写在 `.rupi-job.sh` 文件里（jail.rs:448-453），token 不出现在任何进程 argv 中，前置条件不可能成立；失败后留下一个 ppid=1 的 `while` 孤儿作业运行了 20 分钟（已由我清理）。(3) cloud_first_cut.rs:596/618 在 cargo test 下 3/3 失败、直接运行测试二进制 8/8 通过：在 cargo 环境里拉起二进制很慢（实测 `rupi-server --help` 在 cargo 下 2.0s、外面 0.013s），而健康等待只有 40×50ms（cloud_first_cut.rs:578-590），也没有等 execd 就绪。另外 process_cmdline_tree 读 /proc（cloud_first_cut.rs:683-703），macOS 上永远为空，断言恒真。

建议修法：健康等待改成 10s 并同时轮询 execd。macOS 用 `ps -ax -o command=` 加 `.rupi-job.sh` 内容匹配，或用工作区的 started 文件判断。fts 测试对 trgm 那一条在非 UTF-8 locale 下跳过并给出提示，LAUNCH.md 写明 `initdb --locale=C.UTF-8`/`en_US.UTF-8`。

### F215 [P1] 云端缺 MCP、AG-UI 前端工具、租户记忆管理接口与显式取消接口
类型：missing-feature，工作量：L，文件：crates/rupi-server/Cargo.toml, crates/rupi-server/src/run.rs, crates/rupi-server/src/http.rs

证据：Cargo.toml:21-28 不依赖 rupi-mcp，cloud_tools 写死 read/write/edit/glob/grep/bash/think（tools.rs:13-43），README.md:104 的 MCP 能力只在本机可用。RunAgentInput.tools（客户端工具）和 forwardedProps 被忽略（run.rs:61-70，其中 `input.tools.iter().any(|_| true)` 是空分支），AG-UI 前端工具调用模式不可用。路由表里（http.rs:24-42）没有 /v1/memories 的列表和删除接口，用户无法查看或删除 agent 存下的长期记忆（隐私诉求），管理面也没有。取消 run 只能断开 SSE（CancelOnDrop，http.rs:91-107），没有 POST /v1/runs/{id}/cancel，断线重连后也不能续流。

建议修法：按租户 settings 配置远程 MCP（Streamable HTTP/WS，执行面不需要 stdio）。把客户端工具注册为 Interrupt 型工具，结果走 resume 回传。增加 GET/DELETE /v1/memories 和取消接口。

### F218 [P2] 管理面会话详情给出的 `rupi cloud` 命令无法解析
类型：doc-drift，工作量：S，文件：crates/rupi-server/src/admin.rs

证据：admin.rs:957-959 生成 `rupi cloud --url <cp> --api-key <key> prompt --session …`。--api-key 是顶层参数（rupi-cli/src/main.rs:84-86，没有 global=true）。实测返回 "error: unexpected argument '--api-key' found"。README.md:300 的写法 `rupi --api-key … cloud --url …` 是对的。

建议修法：改成 `rupi --api-key <tenant-key> cloud --url <control-plane> prompt --session {id} 'hello'`。

### F219 [P2] 管理 UI 违反自身 CSP（内联 style 被拦）；canWrite 未使用；只能颁发 admin 角色
类型：bug，工作量：S，文件：crates/rupi-server/static/admin.js, crates/rupi-server/src/admin.rs

证据：CSP 为 `style-src 'self'`，没有 'unsafe-inline'（admin.rs:21）。admin.js 有 7 处 `style="…"`（183、215、271、278、300、475、535），包括执行池进度条的 `width:${pct}%`，浏览器会拦截这些内联样式，进度条永远为 0，表单网格布局失效。canWrite() 定义了（admin.js:4-6）却从未调用，viewer 角色仍能看到新建、删除、调配额、试跑等按钮，点击后只得到 403 toast。颁发管理 Key 固定为 role admin（admin.js:580），UI 里不能颁发 operator/viewer。

建议修法：把内联样式移到 admin.css 的类，进度用 `<progress>` 或 data 属性加 JS 设置 style 属性（CSSOM 不受 CSP 限制）。按 canWrite 隐藏写操作按钮。颁发 Key 时提供角色选择。

### F223 [P2] 死代码与重复代码可删约 300+ 行
类型：simplify，工作量：M，文件：crates/rupi-server/src/run.rs, crates/rupi-server/src/http.rs, crates/rupi-server/src/admin.rs, crates/rupi-server/src/agui.rs, crates/rupi-server/src/db.rs, crates/rupi-server/src/cache.rs, crates/rupi-server/tests

证据：死代码：agui_user_to_message（agui.rs:352-354，无调用方），count_hot_handles（db.rs:737-749）和 set_tenant_region（db.rs:551-559）无调用方；`let ids … let _ = ids;`（run.rs:117-128）；空的 `if input.tools.iter().any(|_| true) {}`（run.rs:61-63）；`let _ = (&self.thread_id, &self.run_id);`（agui.rs:335）；AgentEvent::Error 的映射（agui.rs:322-330）因为全仓库没有发射方而不可达；mem_frozen_session_key 和 get_session_meta 只出现在删除和测试中；测试里的 `let _ = script;`（cloud_first_cut.rs:511-543）。重复代码：resume 手写的 write/edit/bash 分支（run.rs:595-666，约 70 行，而且 bash 输出格式与 tools.rs:271-283 不一致，实测 '\ntimed out'）可以直接用 `tools.execute(&p.tool, args)`；create_session 与 duplicate 的“入库、分配、mark_hot、回滚”流程（http.rs:255-359 和 448-561）可以合并约 60 行；SSE 响应构造（http.rs:639-646 和 admin.rs:1034-1041）；http.rs 里 12 处相同的 500 错误闭包可改用 admin::internal；insert_session 与 insert_session_if_under_cap 的 SQL 重复（db.rs:654-686 和 790-805）；三处租户 SELECT 列表（db.rs:511-514、525-530、1678-1682）；9 个测试文件各自复制 env_urls/ping_deps（每份约 25 行，可移到 tests/common）。

建议修法：按上述清单删除死代码，抽取 alloc_session_row、sse_response、internal 等公共 helper，resume 改走 ToolRegistry，测试 helper 移到 common/mod.rs。

### F224 [P2] 文档与实际行为的小出入：测试“skip”实为 ok，CJK 检索依赖 UTF-8 locale，云端能力边界未列出
类型：doc-drift，工作量：S，文件：README.md, docs/LAUNCH.md, docs/UPSTREAM.md

证据：README.md:348 说集成测试缺 DATABASE_URL/REDIS_URL 时“skip themselves”，实际是回退到硬编码 URL，连不上就 return 并记为 ok。LAUNCH.md:113-118 只提到 pg_trgm 需要 contrib，没有提到 C locale 下 pg_trgm 对中文完全失效（实测 show_trgm('乌龙茶')={}）。UPSTREAM.md:116 的“已知未做”没有列出云端缺 MCP、skills 资源与斜杠命令、settings 键无效、max_turns 固定等差异。README.md:99 说云端与本机跑的是同一个 AgentLoop，但没有说明云端没接 steering/inbox/hooks/discovery。

建议修法：README 改为“缺依赖时提前返回（显示为 ok）”，或配合 test-gap 的修复改成真正的 ignored。LAUNCH.md 增加 locale 要求。UPSTREAM.md 增加一个“云端与本机差异”小节。

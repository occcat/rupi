# 借鉴上游 Pi v1.4.0 的条目

按 SaaS 价值与依赖排序。每条包含上游出处、rupi 差距（行号基于 `c293c78`）与设计。

### B1. 云端 transcript 配对止血：guidance 不再写入树、压实切点对齐、多调用 interrupt、发送前修复 [critical/M]
依赖：无

上游参考：packages/ai/src/api/transform-messages.ts:179-250（补 No result provided 并挪走夹在中间的 system）；pkg/durable/docs/spec.md §2.1 规则 7-9（325-338）；docs/compaction.md:136；coding-agent.CHANGELOG.md:93（#11227）

差距：已抽查核实。drive 在 resume 分支之前调用 load_remote_guidance（crates/rupi-server/src/run.rs:312，resume 在 :384），把 AGENTS.md 和 commands 作为 Role::System push 进树（run.rs:883、956）。历史因此变成 assistant(tool_use) 后接 system、再接 tool，OpenAI 和 Anthropic 都会返回 400。同样的内容还会拼进 system prompt（run.rs:370-375），每个 run 都多落盘一份。AgentLoop 遇到第一个 Interrupt 就 return（crates/rupi-agent/src/lib.rs:922-932），同一批的其他调用永远拿不到结果。compaction_cut 只按 token 切分（crates/rupi-core/src/lib.rs:284-302），保留窗口的开头可能是一条孤立的 Tool。这些 400 不匹配 is_overflow_error，会话会一直卡住。

设计：(a) load_remote_guidance 和 input.context 只写入 builder.append，不再 tree.push；老会话里重复的 [workspace ...] 节点在 prompt_history 投影时过滤掉。(b) rupi-llm 新增纯函数 repair_tool_pairs，在 to_openai_messages 和 to_anthropic_messages 之前调用：夹在 tool_use 和结果之间的非 Tool 消息挪到结果后面，缺结果的调用合成 is_error 结果，丢弃孤立的结果，并累加 rupi_transcript_repairs_total。(c) compaction_cut 回退到 Assistant 或 User 边界。(d) pending_interrupt 改成 Vec，第一阶段把整批调用都判完，interrupts 表一个调用一行（run.rs:117-125 已经要求 resume 覆盖全部 interrupt）。(e) 云端集成测试改用会校验配对的 mock，并在工作区放一个 AGENTS.md。

### B2. 凭证严格按 provider 匹配，云端禁用 mock 兜底 [critical/S]
依赖：无

上游参考：ai.README.md「How Auth Resolves」「Credential Store」（存储的凭证属于对应 provider，刷新失败时不回落到 env）；pkg/ai/src/types.ts:164-176

差距：已核实。resolve_byok 的兜底分支对 openrouter、bedrock、azure、vertex 都去取 openai_api_key 或 api_key，再回落到 RUPI_API_KEY/OPENAI_API_KEY（crates/rupi-server/src/run.rs:819-834），然后作为 override 传入；pick_key 会优先用 override（crates/rupi-llm/src/route.rs:80-86）。所以租户只要把 model 设成 openrouter/x，平台的 OpenAI key 就会作为 Bearer 发到第三方。缺 key 时 provider_or_mock 静默返回 demo 回答（route.rs:290-297），这个 run 照样扣次数。Tenant 用 derive(Debug) 打印 settings（crates/rupi-server/src/db.rs:437-440），明文 BYOK key 可能进日志。

设计：resolve_byok 改为 provider 到设置键的严格映射，不跨 provider 回落，通用的 api_key 只给 openai。平台 key 只用于套餐允许的 provider，并且只读该 provider 自己的 env。云端构造 provider 时不再走 provider_or_mock：缺凭证在 preflight 阶段返回 4xx，code=missing_credentials，不扣 run 次数。Tenant 改为手写 Debug，隐藏 settings。usage 记录 billing=byok|platform。加密存储放在第 34 项。

### B3. bash 输出设上限，超出部分 spill 到工作区 [critical/M]
依赖：无

上游参考：pkg/env/daemon/src/exec.rs:129-190（Spill）；pkg/env/docs/protocol.md「Scheduling」（未发送输出超过 4 MiB 就施加背压）；coding-agent.CHANGELOG.md:8、27（#11360 maxBuffer 和 truncated）；pkg/durable/src/tools/bash.ts:20-26

差距：已核实。Engine::exec_for 用 wait_with_output 把 stdout 和 stderr 全部读进内存（crates/rupi-runtime/src/engine.rs:320-323），超时上限 300 秒，execd 和 sandboxd 都走这条路径。HttpExecutor 用 resp.text() 读完整响应体（crates/rupi-runtime/src/http.rs:36-38）。ExecBashTool 把输出原样拼接（crates/rupi-server/src/tools.rs:255-284）。一条 yes 命令就能让共享的 execd OOM，同节点的其他租户一起受影响；这些输出还会进入模型上下文、SSE、Redis，以及 PG 的两个 GIN 索引。

设计：Engine 改为并发、有界地读取管道（复用 rupi-tools 的 PipeTail），只保留尾部 50KB 或 2000 行。超过阈值后，把完整输出追加写到工作区 .rupi/out/<call>.log，权限 0600，每个会话最多 8 个文件、共 32 MiB，快照打 tar 时排除这个目录。超过硬上限（例如 64 MiB）时，先对进程组发 SIGTERM，再发 SIGKILL。ExecResult 增加 truncated、total_bytes、full_output_path、duration_ms、timed_out 字段，全部 serde default，新旧版本可以混跑。控制面的 ExecBashTool 和 resume 路径再套一层 bound_bash_output。exec 和 fs 端点各自读取有上限的响应体，不要在 snapshot 共用的 post() 上加全局上限。磁盘写满的问题另做卷配额。

### B4. 持久化止血：persist_tree 加 run_id 守卫，写入失败不再报 success，连接池设超时 [critical/S]
依赖：无

上游参考：pkg/durable/docs/spec.md §1 不变式 1-3（65-82）、§10.1（6121-6160）；chord packages/chord/docs/next/design.md:2272-2274（忽略旧 incarnation 的上报）

差距：已核实。finish 用 let _ = 吞掉 persist_tree 的错误（crates/rupi-server/src/run.rs:724），然后照样把内存里的树写进 Redis（run.rs:725-730），再发 RUN_FINISHED success。persist_tree 先 DELETE 整个会话，再全量 INSERT，全程不校验租约（db.rs:1065-1119）。心跳丢失后，旧 owner 进入 finish 时会用旧树覆盖接管者写入的内容。连接池只设了 max_size(32)，没有 wait 超时（db.rs:103-107），池满时心跳会一直等下去，这恰好会触发上面的接管。import_session 也不检查租约（crates/rupi-server/src/http.rs:598-626）。

设计：persist_tree 在事务开头执行 SELECT run_id, instance_id FROM sessions ... FOR UPDATE，与调用方传入的值比较，不一致就回滚。finish、interrupt、import 三条路径都要传入 run_id。心跳返回 false 之后，finish 不再持久化。持久化失败时发 RUN_ERROR，code=persist_failed，并且不写缓存。deadpool 配置 wait、create、recycle 三个超时。这是第 23 项 fence 落地前的过渡方案。

### B5. 让取消真正生效：PoolScheduler::abort 和进程组清理 [high/S]
依赖：无

上游参考：pkg/env/docs/protocol.md「Cancellation」「Liveness」（cancel 杀进程组；连续 30 秒收不到字节就杀掉所有进程组并退出）

差距：已核实。云端的 app.executor 是 PoolScheduler，它的 impl Executor（crates/rupi-runtime/src/pool.rs:156 起）没有实现 abort，用的是 trait 的默认空实现（crates/rupi-runtime/src/lib.rs:256-258）。所以 run.rs:230-237 里的 abort 在云端什么也不做：SSE 断开、心跳丢失或 cancel 之后，bash 会一直跑到 300 秒超时，还可能在下一个 run 里继续写同一个工作区。kill_on_drop 只杀直接子进程 sh（crates/rupi-runtime/src/jail.rs:183），命令运行在独立的进程组里（jail.rs:195），孙进程会变成孤儿进程继续运行。

设计：PoolScheduler 实现 abort，按 pick(h) 转发；加集成测试，断言取消后 execd 的进程组已被杀掉。Engine 在 exec 超时或被 drop 时对进程组调用 killpg，不再只依赖 kill_on_drop。把执行和 HTTP 连接解绑的工作放在第 33 项。

### B6. frozen 记忆的缓存键改为按会话区分，修复同租户内跨会话泄露 [high/S]
依赖：无

上游参考：上游没有直接对应；这是 coding-agent 研究中发现的跨领域问题，已在代码中核实

差距：已核实。PostgresMemory::frozen 按会话范围查询，条件是 session_id IS NULL OR session_id=$2（crates/rupi-server/src/db.rs:1349-1353），但结果缓存在租户级的 key mem_frozen_key(tenant) 下（crates/rupi-server/src/run.rs:274-299，cache.rs:150，TTL 300 秒）。于是同一租户里，A 会话的会话级记忆会出现在 B 会话的 system prompt 里。cache.rs:153 已经定义了 mem_frozen_session_key，但没有被使用。

设计：run.rs 改用 mem_frozen_session_key(tenant, session)。写入租户级记忆时，让该租户所有会话级缓存失效，做法是在缓存值里带一个租户版本号。加一个测试：两个会话各写一条会话级记忆，彼此读不到对方的。

### B7. 图片 URL 抓取的 SSRF 修复（第一步），之后把图片移到外部存储 [high/S]
依赖：无

上游参考：docs/models.md:81-102（入口统一缩放并只编码一次）；ai.CHANGELOG.md 1.4.0 #11016

差距：已核实。fetch_image_url 只做前缀检查 starts_with(http://127.0.0.1)（crates/rupi-server/src/agui.rs:391），http://127.0.0.1.attacker.example 也能通过，控制面还能访问本机回环地址上的服务。reqwest 默认跟随重定向，攻击者可以借此访问内网，属于盲 SSRF。图片以 base64 内联写进 messages.blocks 和 Redis 里缓存的会话树，每一轮都要重放。

设计：第一步（S）：先解析 URL，只允许 https；解析出的 IP 如果是私网、回环、链路本地或元数据地址就拒绝；禁止重定向；限制图片数量。第二步（M）：在入口处按目录里的 image_limits 用 spawn_blocking 缩放一次，按 sha256 存入 S3ObjectStore，ContentBlock::Image 增加 object_ref 字段。所有 413 都当作上下文溢出处理。

### B8. 计费漏洞止血：取消和出错也要记账，压实计费，统一 token 口径，运行中检查预算 [critical/M]
依赖：无（账本表和价格见第 28 项）

上游参考：pkg/durable/docs/spec.md §8.6（5171-5203，失败和中止的 attempt 也计入）；pkg/durable/src/harness/usage.ts:6-56；compaction.ts:200-206；packages/ai/src/api/openai-completions.ts:1521-1546（input 等于 prompt 减去 cacheRead 和 cacheWrite，并兼容 prompt_cache_hit_tokens）

差距：已核实。Usage 只在流正常结束时才发出（crates/rupi-agent/src/lib.rs:801-808），取消分支（:753-757）和错误分支都会提前 return。断开 SSE 会触发 CancelOnDrop（crates/rupi-server/src/http.rs:103-107），租户可以在流快结束时断开连接，拿到内容却不被计 token。ChatResponse 没有 usage 字段（crates/rupi-llm/src/lib.rs:148-151），压实时调用 provider.complete 的消耗从不计入。run.rs:339-348 只累加 input+output，而且是在一个有损通道的消费循环里 await PG，这个通道容量 256，满了就丢（run.rs:54-58）。parse_provider_usage 对 OpenAI 和 Gemini 把包含缓存的 prompt 记为 input，对 Anthropic 则不包含（rupi-llm/src/lib.rs:169-209），也不认 DeepSeek 的 prompt_cache_hit_tokens。配额只在准入时检查一次（crates/rupi-server/src/quota.rs:33-40）。

设计：rupi-agent 增加 with_usage_sink，由同步回调直接累加用量，不经过事件通道。取消和错误分支用 tokens.rs 估算用量，并标记 estimated；Anthropic 在 message_start 就先上报一次 input。ChatResponse 增加 usage 字段（serde default），压实的消耗以 kind=compaction 上报。TokenUsage.input 统一定义为不含缓存的输入；cache_read 依次读取 cached_tokens、prompt_cache_hit_tokens、顶层 cached_tokens；cache_hit_pct 和 TokenMeter 的校准都改用 input+cache_read+cache_write。drive 无论从哪个分支退出，都把用量 flush 到幂等的 run_usage(run_key, turn, attempt, kind)，run_key 由服务端生成。每一回合开始前用 Redis INCRBY 检查当日预算，超出就 cancel，并发 RUN_ERROR code=quota_exhausted。按套餐限制 thinking 档位（run.rs:376-382）。

### B9. 稳定缓存前缀：工具顺序确定、历史加缓存断点、cache key 和会话亲和 [critical/M]
依赖：第 1 项（guidance 先不写入树，否则每个 run 的历史都不一样）

上游参考：packages/ai/src/api/anthropic-messages.ts:1526-1550（在最后一条 user 消息上打断点）；packages/ai/src/api/openai-prompt-cache.ts；pkg/durable/src/tool-search/tool-search.ts:14-15；ai.CHANGELOG.md 0.86.0 #9102 #9629、1.4.0 #10940；pkg/coding-agent/src/core/cache-stats.ts

差距：已核实。ToolRegistry 用 HashMap 存储，definitions() 按哈希顺序输出（crates/rupi-tools/src/lib.rs:91-94、117-121），而云端每个 run 都新建注册表（run.rs:310）。结果是不同 run 之间 tools 和 AvailableTools 的顺序随机，OpenAI 兼容网关（包括 deepseek）的前缀缓存基本命中不了。Anthropic 只在 system 和最后一个工具上打了断点（crates/rupi-llm/src/anthropic.rs:72-94），历史消息上没有；Bedrock 一个断点都没有。云端也从不设置 session_id 和 prompt_cache_key。

设计：阶段 1（S）：definitions() 按 name 排序，AgentLoop 合并工具后再整体排序一次；加单测，断言两次构建的序列化结果逐字节相同；之后新发现的工具一律追加到末尾。阶段 2（M）：ChatRequest 增加 session_id 和 cache_retention，云端填 hash(tenant_id, thread_id)。Anthropic 和 Bedrock 在最后一条消息上打断点，并保留上一轮末尾的断点，总数不超过 4 个。OpenAI 官方端点发 prompt_cache_key，兼容网关加 compat 开关。按 provider 导出缓存命中率指标，标签里不放 tenant。

### B10. LLM HTTP 超时、Retry-After 上限、jitter 和错误分类 [high/S]
依赖：无

上游参考：packages/ai/src/utils/provider-retry.ts:1-68（x-should-retry；408/409/429/5xx；等待超过 60 秒就失败；最多 25% jitter）；packages/ai/src/utils/retry.ts；pkg/coding-agent/src/core/http-dispatcher.ts:4；ai.CHANGELOG.md 1.2.0 #10609

差距：已核实。shared_http_client 没有设置连接超时和读超时（crates/rupi-llm/src/lib.rs:453-463）。上游连接卡住时，run 会一直占着租约、并发槽和执行器 handle。parse_retry_after 没有上限（lib.rs:865-868），服务端返回 3600 就真的睡一个小时，心跳期间还会照常续约。退避没有加 jitter（lib.rs:860-862），多个租户共用平台 key 时，重试会同步扎堆。insufficient_quota 这类错误也会被重试。

设计：设置 connect_timeout 10 秒；read_timeout 按块间空闲计算，取 120 到 300 秒；不设总超时。Retry-After 最多等 60 秒（可配置），超过就返回 Retryable 交给上层处理。退避加 0 到 25% 的 jitter。识别 x-should-retry、retry-after-ms、408 和 409；配额耗尽和鉴权错误不重试。sleep 和 CancelFlag 用 select 竞速。新增 LlmError{kind, status, retry_after_ms}，通过 anyhow 的 downcast 读取。

### B11. 降低 PG 和执行面的固定负载：准入时不再全表 reap，reconcile 分片，执行器 stats 改为后台缓存 [high/S]
依赖：无

上游参考：pkg/durable/docs/group-commit.md §1、§4-5（合并低价值的小写入）；telemetry.README.md:209

差距：已核实。每次 acquire_lease 都会跑一次全局的 reap_stale_runs（quota.rs:122，db.rs:936-947）。每个副本每 15 秒遍历一遍所有租户做 reconcile，每个租户一次 count(*)（crates/rupi-server/src/reclaim.rs:15-20）；10 万租户、50 个副本时约 33 万 QPS，而且和业务流量无关。/metrics、/ready 和每次 alloc 都要串行请求所有执行节点的 stats，客户端没有设超时（http.rs:76-89，pool.rs:134-145，crates/rupi-runtime/src/http.rs:22）。/metrics 也不鉴权。

设计：acquire 只检查自己会话的 run_id 是否过期；全局 reap 只在 reclaim 循环里做，加 LIMIT，并用 pg_try_advisory_lock 保证同一时间只有一个副本在跑。reconcile 改为每个 tick 执行一条 GROUP BY tenant_id，只修正计数不为零的租户。PoolScheduler 起一个后台 poller，每 5 秒并发拉一次 stats 并缓存，ranked、/ready、/metrics 都读缓存。reqwest 设连接超时和总超时。/metrics 移到 RUPI_METRICS_BIND 指定的内网端口。

### B12. 优雅下线（阶段 0）与 panic 隔离 [high/S]
依赖：第 5 项

上游参考：pkg/durable/docs/spec.md §5.1 close（2359-2370）、§5.4（2531-2536，close 不算崩溃）；chord spec.md §6-7（gate deadline）

差距：已核实。rupi-server 没有处理任何信号，也没有调用 with_graceful_shutdown；drive 是一个没人管的 tokio::spawn（run.rs:169）。滚动发布时，正在跑的 run 和崩溃一样整轮丢失，会话还会被锁住 180 秒。release profile 设置了 panic=abort（Cargo.toml:29），一个 run 里的 panic 会让整个副本退出，副本上所有租户正在跑的 run 都跟着丢失。

设计：App 增加 draining 标志和 RunRegistry，后者记录每个 run 的 CancelFlag 和 JoinSet。收到 SIGTERM 后：/ready 返回 503；/v1/agent 返回 503 并带 Retry-After；等待正在跑的 run 结束，最多等 drain_secs，terminationGracePeriodSeconds 要设得比它大；超时后 cancel，走现有的 finish 流程落盘并释放租约。新增 profile.release-server，继承 release 并改为 panic=unwind，同时修改 docs/LAUNCH.md:111。drive 外层 await JoinHandle，或者用 Drop guard，确保 release_lease、release_run 执行并发出 RUN_ERROR。排查 Mutex lock().unwrap() 在锁被 poison 后连锁 panic 的问题（例如 crates/rupi-agent/src/lib.rs:922）。

### B13. 闲置快照先 claim：引入 snapshotting 中间态 [high/S]
依赖：无

上游参考：chord pkg/chord/docs/spec.md §7 Gate、§12 Decisions「Gate admission」（exclusive 资源先关旧再开新，迟到的请求收到可重试的 upgrade_conflict）

差距：list_reclaim_candidates 既不加锁也不 claim（db.rs:1014-1063）。mark_snapshotted 无条件清空 handle（db.rs:949-964）。ensure_hot 用的是 start_run 在拿到租约之前读出的旧行（run.rs:82、230）。preempt 选候选时用的是 Duration::ZERO（reclaim.rs:41、49）。执行池紧张时，run 可能跑在已经被释放的卷上，多个副本也可能同时给同一个会话做快照。

设计：用一个短事务完成 claim：UPDATE sessions SET workspace_state=snapshotting WHERE run_id IS NULL AND workspace_state=hot AND runtime_handle=$h RETURNING，然后在事务之外做快照。mark_snapshotted 和回滚都要带上 snapshotting 状态和 handle 作为条件。set_run_id 增加条件 workspace_state 不等于 snapshotting，不满足时返回可重试的 409。ensure_hot 在拿到租约后重新读一次行。处于 snapshotting 超过 10 分钟的会话由 reaper 回滚。

### B14. 给持久化格式加版本：遇到不认识的格式时拒绝，不做降级 [high/S]
依赖：无

上游参考：chord pkg/chord/docs/spec.md §2、§7（schema 变了但版本号没升，就拒绝 reload）、§9 PROTOCOL_VERSION

差距：ContentBlock 是 tag=type 的枚举，没有表示未知类型的变体（crates/rupi-core/src/lib.rs:29-30）。load_tree 反序列化失败时，会静默把消息退化成纯文本（db.rs:1151-1155），工具调用的 id 随之丢失，调用和结果对不上。随后 persist_tree 把整棵树写回，损坏就永久留下了；Redis 里的树缓存还会把这个降级版本继续提供 300 秒。滚动发布、新旧副本同时在线时就会触发。

设计：messages 表增加 format 列，rupi-core 定义 MESSAGE_FORMAT 常量。读到比本副本更高的版本时返回 upgrade_required（503 并带 Retry-After），让客户端重试；另一个办法是给 ContentBlock 增加 Unknown(Value)，原样保留，但不发给模型。Redis 树缓存的 key 里带上版本号。上线顺序是先发布能读新格式的代码，再打开写新格式的开关。

### B15. 处理 length 截断：参数是坏 JSON 的调用不执行，max_tokens 按模型设置 [high/S]
依赖：无

上游参考：pkg/agent/src/agent-loop.ts:273-279、488；ai.CHANGELOG.md 1.3.0 #10897（argumentsError）、1.4.0 #9512

差距：AgentLoop 调用时 max_tokens 总是 None（crates/rupi-agent/src/lib.rs:707-714），所以 Anthropic、Bedrock、Gemini 都退回到 4096 的输出上限（anthropic.rs:84，已核实）。输出被截断后参数解析失败，结果变成 {}，但调用照样进入执行；在云端，参数为空的 write 还会触发一次审批。主循环只判断 stop_reason 是否为 error。

设计：把 stop_reason 归一为枚举 Stop、Length、ToolUse、Error、Aborted；ToolCall 增加 arguments_error 字段。遇到 Length 且消息里带工具调用时，这些调用不执行，也不送审批，而是各合成一个 is_error 结果，提示模型分块写入，这样调用和结果仍然一一配对。models.json 增加 maxOutput 字段，默认 max_tokens 取 maxOutput 与「上下文窗口减去估算输入再留余量」两者中的较小值，并按套餐设上限。

### B16. 压实改为增量摘要，按投影后的上下文大小触发，并使用结构化模板 [critical/M]
依赖：第 1 项（切点对齐）、第 8 项（压实计费）

上游参考：docs/compaction.md:29-49、83、100-138、234-291；pkg/coding-agent/src/core/compaction/compaction.ts:507-579、617-636、700-735；coding-agent.CHANGELOG.md 0.87.1 #9908、1.2.0（序列化时去掉 thinking）

差距：触发条件用的是整条原始分支的 history_tokens()（crates/rupi-agent/src/lib.rs:1228），第一次压实之后这个值一直高于阈值，实际起作用的只有 2×keep 那个启发式。每次摘要都从下标 0 开始重新摘（:1260-1263），总成本是 O(N²)。原始历史超过摘要模型的窗口后，会退化成只取最旧 10 条消息的首行，并且覆盖掉旧摘要（:1289-1314）。提示词只有一句 Be brief，序列化时把 thinking 全文都带上了，工具结果也不截断。

设计：触发量改为投影后的大小：用上一轮 provider 报告的 input+cache_read+cache_write，加上之后新增消息的估算值。摘要区间改为从 summary_through 之后到切点，旧摘要放进 previous-summary，用 update 模板合并；超出预算时分块串行 update。摘要失败时保留旧摘要，再追加 N messages elided 标记，并发出 CompactionEnd 错误事件。新增 serialize_for_summary：加角色标签，去掉 Thinking 和 Image，工具结果截到 2000 字符，整体用 conversation 标签包起来。用户消息段被切开时生成两份摘要。

### B17. 可观测性基础：request id、收敛对外错误、日志脱敏、低基数指标 [high/M]
依赖：无

上游参考：telemetry.README.md:167、209、236、513-519；pkg/ai/src/telemetry.ts:70-104、192-196；pkg/durable/src/harness/telemetry.ts:117-138

差距：/metrics 只有 4 个手写的指标值（http.rs:76-89，已核实）。429 不区分原因（quota.rs:5-9）。事件被丢弃时只打一条 warn（run.rs:48-58）。请求没有 request id，日志是纯文本。访问日志的 span 记录了全部请求头，而脱敏表只覆盖 4 项（crates/rupi-runtime/src/access_log.rs:8-13、39-45）。RUN_ERROR 和多处 500 响应直接返回 e.to_string()，会把执行器 URL 和 SQL 错误暴露给客户端。

设计：在 server、execd、sandboxd 三处接入 tower-http 的 request-id 中间件；访问日志只记录 MatchedPath、status 和 latency；脱敏表补上 x-api-key 等头。加 RUPI_LOG_FORMAT=json 选项。对外错误统一为固定的 code 加 requestId，原始错误只写服务端日志。用 metrics crate 导出：runs{outcome}、rejects{reason}、llm_request_duration、ttft、tokens{provider,model,kind}、llm_retries、tool_duration{tool}、executor_op_duration、events_dropped{stage}、pg_pool_waiting、lease_fence_lost、compactions{reason,outcome}。标签不带 tenant、session、run；model 标签只允许白名单值，其余归为 other。在 rupi-core 里建一个 telemetry 常量词表，并加一个扫描源码的测试，防止引入高基数标签或 PII。

### B18. 只读工具并行执行 [medium/S]
依赖：无

上游参考：pkg/durable/docs/spec.md:3678-3681（executionMode：一批里只要有一个 sequential，整批就串行）；pkg/durable/src/tools/file-mutation-queue.ts:28-40

差距：云端没有调用 with_tool_execution，用的是默认的 Sequential（run.rs:363-369；crates/rupi-agent/src/lib.rs:119-124）。模型一次发出 5 个 read 或 grep，控制面就要串行地向 execd 发 5 次 HTTP 请求。

设计：云端设置为 ToolExecution::Parallel，并发上限为 8。ToolDefinition 增加 execution_mode 字段，一批调用里只要有一个是 Sequential，整批就串行执行，防止租户把写操作设为自动放行后出现并发写。

### B19. 工作区资源发现：只扫白名单目录、标记为不可信、每个热工作区只加载一次 [medium/S]
依赖：第 1 项

上游参考：docs/security.md:25-57（AGENTS.md 一律当作不可信输入）；docs/skills.md 中的发现目录

差距：每次 run，load_remote_guidance 都会 glob **/SKILL.md 和 commands/*.md，最多要和执行面往返约 54 次，结果也不缓存（run.rs:862-966）。glob 默认使用 standard_filters，会跳过隐藏目录，所以 .rupi/commands、.agents/skills、.pi/skills 在云端都找不到；反过来，仓库里任何非隐藏路径下的 SKILL.md 都会被当成 skill 加载。注入时也不标注来源。

设计：skill 只在白名单目录里查找；给 Executor 增加一个受限的列目录接口，只允许白名单前缀，跳过符号链接和 ..。注入内容用 workspace-instructions trust=untrusted 包起来。按 (handle, 文件指纹) 缓存到 Redis，并提供 reload。commands 只在用户输入 /name 时才展开。

### B20. 请求卫生：错误结果中的图片、空工具输出、终端用户标识 [medium/S]
依赖：无

上游参考：ai.CHANGELOG.md 1.3.0 #10794、1.4.0 #8720；packages/ai/src/api/anthropic-messages.ts:1306-1311（metadata.user_id）

差距：anthropic.rs:203-224 在 is_error 为 true 时也把图片放进 tool_result，会被 Anthropic 返回 400。空工具输出或只有空白的输出原样发送（crates/rupi-llm/src/lib.rs:312），没有输出的成功 bash 就会触发这种情况。请求不带终端用户标识，一个租户滥用就可能导致整个平台 key 被封。

设计：Anthropic 和 Bedrock 在 is_error 时把图片移到 tool_result 之后。空白输出统一替换成占位文本。ChatRequest 增加 identity 字段，其中 end_user 取 HMAC(tenant_id)，映射到 Anthropic 的 metadata.user_id 和 OpenAI 的 safety_identifier。User-Agent 设为 rupi/版本号。

### B21. 模型目录记录能力，套餐限定可用模型，租户和会话设置真正生效 [high/M]
依赖：第 2 项

上游参考：pkg/ai/src/types.ts:840-887；pkg/ai/src/models.ts:1259-1290（clampThinkingLevel）；docs/models.md:81-112；pkg/coding-agent/src/core/sdk.ts:442-453

差距：已核实。ALLOWED_SETTING_KEYS 接受 enabledModels、compaction、tools、base_url 等键（crates/rupi-server/src/admin.rs:24-45），但服务端只读其中少数几个，其余写进去不起作用。会话上的 model 只用于回显，不参与路由（run.rs:323）。租户可以把 model 设成任意字符串，用平台 key 调用最贵的模型。目录里没有 maxOutput、模态、thinking 档位等信息。

设计：ModelEntry 增加 max_tokens、input、thinking_map、cost、prompt_cache、image_limits、compat 字段。新建 PG 表 model_catalog(provider, id, version, data, enabled, plans)，以内置 JSON 作为初始数据，副本在内存缓存，通过 pub/sub 失效。实际使用的模型依次取 sess.model、tenant.model、默认值，且必须在套餐白名单内，否则返回 400。compaction、tools、excludeTools、max_turns 从租户设置读取并设上限。白名单里没有云端含义的键删掉。增加 clamp_thinking；不支持视觉的模型改为收到占位文本。

### B22. 按步骤增量提交 transcript（先提交后展示的 PG 实现） [critical/L]
依赖：第 4 项；与第 23 项同时上线（依赖 fence）

上游参考：pkg/durable/README.md:5、94-113；pkg/durable/docs/spec.md §1 不变式 1-8（65-98）、§4（1823-1981）、§4.1 deferred（1983-2065）、§8.2 pi.live（4545-4775）；pkg/coding-agent/src/core/session-manager.ts:1172-1197

差距：已核实。会话树只在 run 结束时（run.rs:724）或 interrupt 时（:451）整体 DELETE 再 INSERT（db.rs:1065-1119），所以每个 run 的写入量和 trgm、tsv、CJK n-gram 生成列等索引的维护量都与会话长度成正比。SSE 在持久化之前就发出，副本崩溃时用户输入、partial 和工具结果全部丢失，而 token 已经花掉。load_tree 按各副本本地时钟写入的 created_at 排序，不保存 leaf，多个分支会被拼成一条线。

设计：rupi-agent 新增 RunJournal trait，包含 user、assistant、tool_intent、tool_result、compaction 和延迟提交的 progress，通过 AgentLoop::with_journal 注入，在 lib.rs:582、:814、工具执行前和 :1088 处 await。rupi-server 实现 PgJournal：每一步一个事务，INSERT 新节点时 ON CONFLICT DO NOTHING，同时更新 runs 的 checkpoint，并带上 fence。messages 表增加 seq 和 run_id，sessions 表增加 leaf_id 和 next_seq；load_tree 按 seq 读取，从 leaf 沿 parent 重建路径。TEXT_MESSAGE_END 和 TOOL_CALL_RESULT 在对应的提交完成后才发出；partial 写入 Redis run_live，不写 PG。搜索文本只保留 user 和 assistant 的文本。上线时先发布兼容读取的版本，再用开关切换写路径；Redis 树缓存带上 seq。

### B23. runs 表、epoch fence 租约、恢复用的 reconciler、副本存活表 [critical/L]
依赖：第 22 项

上游参考：pkg/durable/docs/spec.md §5.1（2351-2370）、§5.4（2530-2573）、§10.1；pkg/durable/src/harness/scheduler.ts:428-466；chord packages/chord/docs/next/design.md:2272-2274；chord spec.md §5、§10

差距：已核实。租约由 Redis NX 加 sessions.run_id 上的乐观锁实现，TTL 180 秒，没有 epoch（quota.rs:93-135，db.rs:864-905）。副本崩溃后要等 180 秒 reap，这期间重试一律返回 409，并发槽一直被占着，run 既不会续跑也不会结束。release_lease 只比较客户端提供的 run_id，不比较 instance。

设计：新建 runs 表(tenant_id, id, session_id, status, phase, checkpoint, attempt, restarts, owner_instance, lease_until, fence, failure_reason, code_version)，用部分唯一索引保证每个会话最多一个活动 run。acquire 用一条 UPDATE ... RETURNING fence 完成。所有写操作在事务里先执行 UPDATE runs WHERE fence=$f 拿到行锁。心跳按副本批量发送：UPDATE runs SET lease_until WHERE owner_instance=$me AND id=ANY($ids) RETURNING id, abort_requested。instances 表每 5 秒心跳一次，超过 15 秒没有心跳的副本，其 run 可以被接管，fence 加 1。reconciler 用 SKIP LOCKED 接管过期的 run：处于 request 阶段的，把 partial 转成 aborted 节点后按退避重试；处于 tools 阶段的，按第 24 项的规则处理。租户可设置 on_crash: continue|fail。

### B24. 工具调用先提交 intent，并声明 replay 策略（effect sandwich） [high/M]
依赖：第 22、23 项；第 1 项中的多 interrupt

上游参考：pkg/durable/docs/spec.md §5.2（2372-2388）、§7.3（3995-4006、4146-4152）、§8.4（5035-5118）；pkg/durable/src/harness/tool.ts:186-226；pkg/durable/README.md:277

差距：Tool trait 没有 replay 语义（crates/rupi-tools/src/lib.rs:74-89），执行前不记录 intent，结果也只保存在内存里。崩溃后没办法知道 bash 或 edit 有没有执行，只能二选一：静默丢掉，或者盲目重跑。

设计：Tool trait 增加 replay() 方法，默认返回 Unsafe。read、glob、grep、think 声明为 Safe；write 只在 Sequential 模式下算 Safe。新建 tool_calls 表(tenant_id, run_id, call_id, name, args, replay, status, result_node, output_tail)。assistant 消息提交时，把整批调用写成 pending；执行前改为 executing；结果和 done 在同一个事务里写入。恢复时，只有存储的 replay 和当前声明都是 Safe 才重跑，其他情况追加一个 is_error 结果，说明该调用可能已部分执行，并附上 output_tail。interrupts 表降级为兼容层。MCP 工具一律按 Unsafe 处理。

### B25. run 级幂等准入 [high/S]
依赖：第 23 项

上游参考：pkg/durable/README.md:129-138；pkg/durable/docs/spec.md §6 2928；pkg/durable/src/harness/submissions.ts:232-241

差距：已核实。set_run_id 只检查 run_id IS NULL（db.rs:872-876）。如果 run 成功了但响应丢失，客户端用同一个 runId 重试，就会再跑一遍、再计一次费。第 22 项上线后用户消息会提前落盘，崩溃后的重试会产生重复的用户轮次。复核结论：chord 研究认为客户端消息 id 会和全局主键冲突，这不成立。节点 id 由 push 生成 uuid（crates/rupi-core/src/lib.rs:215-217），import 和 clone 也通过 remap_tree 重新分配 id。

设计：start_run 第一步执行 INSERT INTO runs ... ON CONFLICT DO NOTHING RETURNING。已存在时按状态处理：running 就 attach 上去，或返回 409 并带上状态；done 就返回快照和 RUN_FINISHED；failed 就返回 RUN_ERROR 和失败原因。runId 只在租户和会话范围内有效，完成的记录保留 7 天。

### B26. attempt 计数、restart_limit 和 agent 层重试 [high/M]
依赖：第 10、23 项

上游参考：pkg/durable/docs/spec.md §5.4（2530-2573）、§8.3（4890-4921）；pkg/durable/src/harness/generation.ts:262-266、495-546；packages/ai/src/utils/retry.ts；docs/settings.md:128-136

差距：流传输中途出错（crates/rupi-llm/src/lib.rs:621，anthropic.rs:726-731）会直接让整个 run 失败，已生成的 partial 被丢弃，也没有 attempt 计数。如果开启自动续跑，一个能稳定让进程崩溃的输入会在副本之间来回接管，把副本一个个打崩。

设计：在 LLM 调用阶段（本轮的工具还没执行），遇到可重试的错误就追加一个 aborted 节点，这个节点不进入后续上下文，然后发出 RetryScheduled 并退避重试。默认重试 3 次，从 2 秒开始，上限 60 秒。需要让出副本时，用 status=sleeping 加 wake_at 记录退避状态。因 drain 被打断的不计入失败。restarts 达到 3 到 5 次就把 run 置为 failed，reason=restart_limit，同时写指标和审计。AG-UI 侧先关闭当前打开的 TEXT_MESSAGE，再发送 CUSTOM 事件 rupi.retry。

### B27. 断线不取消 run、事件流可续传、快照按窗口截取 [high/L]
依赖：第 22、23 项

上游参考：pkg/durable/README.md:78、477-527；pkg/durable/docs/spec.md §9.1-9.3（ConversationTranscript.resume）；chord pkg/chord/docs/spec.md §9、§12（合并事件，溢出时发 reset，控制帧不排在状态帧后面）

差距：已核实。CancelOnDrop 在连接断开时直接取消整个 run（http.rs:91-107）。SSE 事件没有 id，不支持 Last-Event-ID，也没有按 run 重新连接的接口（http.rs:24-42）。被取消的 run 仍然发出 RUN_FINISHED success。MESSAGES_SNAPSHOT 每次都下发完整历史，包括 base64 编码的图片（run.rs:317）。终止事件用 try_send 发送，可能丢失。

设计：连接断开只移除这个观看者。最后一个观看者离开后，run 再继续跑 detach_grace_secs，这段时间计入并发配额。owner 把合并后的事件写入 Redis Stream，key 为 {tenant}:ev:{run}，设置 MAXLEN 和 TTL；同一副本内的观看者走进程内 broadcast。新增 GET /v1/sessions/{id}/runs/{runId}/events?after=，任意副本都能处理：先从 PG 发快照，再发 run_live 里的 partial，然后用 XREAD BLOCK 继续推送；请求的位置超出窗口时只发快照。快照只带最近 N 条，附 historyBefore 游标和分页接口，图片改为签名 URL。终止事件和控制事件保证送达，取消的 run 发 RUN_ERROR，code=aborted。

### B28. 计费级用量账本、价格目录和租户用量 API [high/L]
依赖：第 8 项

上游参考：pkg/ai/src/models.ts:1236-1257（calculateCost）；pkg/ai/src/types.ts:459-487；pkg/durable/src/harness/usage.ts:6-56；pkg/durable/src/harness/harness.ts:369-376

差距：quota_ledger 只按 tenant×day 记录 tokens 和 runs（db.rs:270-276），没有金额，也没有按 model、session、run 的明细。价格只有一张按子串匹配的硬编码表（crates/rupi-agent/src/tokens.rs:11-90），而且云端没用它；ModelEntry 没有价格字段（crates/rupi-llm/src/catalog.rs:29-35）。租户只能查到今天的总量（http.rs:153-168）。

设计：新建 usage_events 表，按月分区，唯一键为 (run_key, turn, attempt, kind)，字段包括 cost_nanos、cost_source、billing、price_version，另建 usage_daily 汇总表。实时累计放在 Redis，权威数据以 usage_events 为准。新建 model_prices(provider, model, effective_from)，在管理面编辑，修改要写审计；rates_for 改为从目录读取。BYOK 产生的成本只做展示，不计费。新增 GET /v1/usage?groupBy=day|model|session，查询走只读副本；会话详情返回该会话的 usage；管理面增加按成本排序的租户 Top-N。

### B29. 租户侧安全审计日志 [high/M]
依赖：第 17 项（request_id）

上游参考：上游没有对应实现；telemetry.README.md:513-515（telemetry 不承载业务状态，所以审计需要单独存储）

差距：现在只有管理面的 admin_audit，而且写失败会被忽略（admin.rs:224-241）。工具审批（包括修改参数，run.rs:584-594）、BYOK 改键、导出、导入、删除、fork 都不留记录。interrupts 只存 status，没有记录谁、在什么时候做的决定。tenant_of 也不返回请求用的是哪把 key（http.rs:109-134）。

设计：新建 tenant_audit 表(tenant_id, actor_key_id, action, target, detail, request_id, trace_id, created_at)，按月分区；detail 只存参数哈希和键名。审批、改键这类高风险操作，审计和业务写入放在同一个事务里，审计写失败就拒绝整个操作。interrupts 增加 decided_by 和 decided_at。新增 GET /v1/audit 和对应的管理面接口。

### B30. 跨副本控制：cancel、steer、follow-up 和持久收件箱 [medium/M]
依赖：第 5、23 项

上游参考：pkg/durable/README.md:529-591（whenBusy steer/followUp/reject）；pkg/durable/docs/spec.md §6（2847-3066）；agent.README.md「Steering and Follow-up」；docs/rpc-commands.md:7-90

差距：路由里没有 cancel 和 input 接口（http.rs:24-42）。会话正忙时一律返回 409（run.rs:146-161）。MessageInbox 只有 CLI 在用，云端没有接上（crates/rupi-agent/src/queue.rs；lib.rs:283-310）。

设计：新增 POST /v1/sessions/{id}/runs/{runId}/cancel 和 .../input。权威记录写在 runs.abort_requested 和 run_inputs 表里，client_request_id 唯一；写入后 PUBLISH 到 ctl:{owner_instance}。owner 在回合边界注入 steer，在 drive 循环里处理 follow-up。批量心跳的 RETURNING 里带回 abort_requested 作为兜底，最晚 15 秒生效。另外增加 continue 型 run，用于 interrupted 之后接着跑。

### B31. 执行器资源计量：工作区占用秒数和 exec 耗时 [medium/S]
依赖：第 3 项

上游参考：pkg/durable/src/harness/usage.ts:9-11（tools 桶）；durable.CHANGELOG.md:260（durationMs）

差距：ExecResult 只有 stdout、stderr、exit_code 三个字段（crates/rupi-runtime/src/lib.rs:132-136），超时只能从 exit 124 看出来。热工作区默认空闲 30 分钟才回收，期间一直占着槽位，但没有记录占用了多久。jail 没有使用 cgroup。

设计：第一阶段：新建 workspace_leases 表，所有 alloc 入口收拢到一个辅助函数里，统一写记录；ExecResult 增加 duration_ms 和 timed_out，写入 compute_events。第二阶段：execd 给每个工作区建一个 cgroup v2，读取 cpu.stat 和 memory.peak，并设置 cpu.max、memory.max、pids.max。

### B32. 平滑交接（阶段 1）：在回合边界移交 run，不计为崩溃 [high/L]
依赖：第 22、23、24、27 项

上游参考：pkg/durable/docs/spec.md §5.4「A close is not a crash」（2531-2536）、§8.3（4900-4921）；chord pkg/chord/docs/spec.md §6-7

差距：第 12 项只能等 run 自然结束，或者直接 cancel。运行时间超过宽限期的长 run，发布时仍然会被打断。

设计：drain 超时后，在下一个回合边界返回 StopReason::Handoff。drive 完成步骤提交后把 run 置为 queued，restarts 不加 1，fence 加 1。其他副本通过 dispatcher 或客户端重新 attach 认领这个 run，然后调用 continue_session。正在进行的模型请求按 close 语义重发；正在执行的工具通过 call_key 重新接上，接不上就补一个 interrupted 结果。客户端收到 RUN_ERROR，code=handover，然后自动重连。接手速度要限流。

### B33. 执行面幂等 call_key，连接断开不杀命令 [medium/M]
依赖：第 5、24 项

上游参考：pkg/durable/docs/spec.md §7.3（3727-3740）、§5.2（2380-2383）；pkg/env/docs/protocol.md「Cancellation」（cancel 按请求 id 寻址，先登记后执行）

差距：ExecRequest 没有幂等键（crates/rupi-runtime/src/lib.rs:124-129），execd 的 handler 直接 await 子进程（crates/rupi-runtime/src/execd.rs:246-262）。控制面换副本后，新副本不知道命令有没有跑完。

设计：handler 用 tokio::spawn 执行命令，使执行和 HTTP 连接解耦。ExecRequest、FsWrite、FsEdit 增加 call_key 字段，格式为 {tenant}/{run}/{call}；Engine 按 handle 保存一张结果表，设 TTL 和条数上限。新增 POST /v1/exec/status，要求先通过租户校验。reconciler 遇到状态为 executing 且 Unsafe 的调用时先查 status：已完成的，用真实结果提交；查不到的，才补 interrupted。

### B34. 凭证加密存储，并按请求传入 ProviderConfig [high/L]
依赖：第 2 项

上游参考：ai.README.md「Credential Store」（modify 串行执行刷新）、「Provider-Scoped Environment Overrides」；ai.CHANGELOG.md 1.0.3

差距：BYOK key 以明文存在 tenants.settings JSONB 里（db.rs:208）。endpoint、region、project 全部从进程环境变量读取，租户无法配置；base_url 写进去也不生效。Vertex 用的静态 token 过期后不会刷新。

设计：新建 provider_credentials 表，tenant_id 为 NULL 表示平台凭证，用 KMS 做信封加密；只在 drive 内存里解密，不写 Redis，也不写日志。rupi-llm 新增 ProviderConfig，provider_from_spec 改为接收它作为参数，云端路径不再读环境变量。OAuth 刷新用 Redis 短锁配合 version 做 CAS。base_url 只允许 https，解析后拒绝私网和元数据地址，并禁止重定向。

### B35. 分布式追踪（OTel）与 trace carrier 持久化 [medium/L]
依赖：第 17 项

上游参考：telemetry.README.md:69-183；pkg/ai/src/telemetry.ts:15-105；pkg/durable/src/harness/telemetry.ts:77-115；pkg/durable/src/harness/submissions.ts:241；pkg/durable/src/harness/scheduler.ts:2219-2240

差距：全仓库只有 HTTP 访问日志这一个 span（crates/rupi-runtime/src/access_log.rs:38-46）。run、turn、LLM 调用、工具和执行器都没有 span，执行器请求也不带 traceparent。

设计：在 rupi-runtime 里加一个可选 feature otel，只有设置了 RUPI_OTEL_ENDPOINT 时才启用。需要的 span 有 rupi.run、rupi.turn、rupi.llm.request（含 first_chunk 事件）、rupi.tool、rupi.compaction、rupi.executor.call，外加 execd 侧的 server span。span 只记 ID 和固定的分类值，不记内容；出错时只记类别和 HTTP status。HttpExecutor 往请求里注入 traceparent；客户端传进来的 traceparent 只作为 link，不作为 parent。sessions.last_trace 和 interrupts.trace 存 carrier，resume 时加 link。采样用 parent-based，再配合 tail sampling。

### B36. principal scope 与即时撤销 [medium/M]
依赖：第 27 项

上游参考：chord pkg/chord/docs/spec.md §4（principal 由链路盖章、invalidatePrincipal）；packages/chord/docs/next/design.md §4（revokedBy）

差距：api_keys 表没有 scope 字段（db.rs:213-219），同一租户内分不出观看者和操作者。鉴权只在请求开始时检查一次（http.rs:109-134），key 被撤销后，用它打开的 SSE 和 run 会一直持续到结束。

设计：api_keys 增加 scope（owner、participant 或 viewer）和 grant_rev 两个字段；新建 session_grants 表，用于共享会话。tenant_of 改为返回 Principal，runs 记录发起的 key_id。撤销 key 时 PUBLISH revoke:{key_id}，各副本关闭用这把 key 打开的流，并发送 RUN_ERROR，code=revoked。

### B37. 按 region 放置 run [medium/M]
依赖：第 23 项（instances 表）

上游参考：chord pkg/chord/docs/spec.md §2 placement（提供方重新计算放置位置，放错就拒绝，返回 invalid/placement）

差距：会话的 region 在创建时就定了，执行面也按 region 选节点（pool.rs:108-121），但 start_run 从不比较 sess.region 和 app.region。所以一个 eu 会话可能由 us 副本驱动，每次工具往返都要跨洋，也不符合数据驻留的要求。

设计：preflight 阶段如果发现 region 不一致，并且会话所在 region 有健康的副本，就返回 421 或 307，带上该 region 的 endpoint，或者由当前副本反向代理 SSE。会话所在 region 没有健康副本时，允许跨区兜底。

### B38. 逻辑模型路由与跨 provider 故障转移 [medium/L]
依赖：第 21、26、34 项

上游参考：docs/virtual-models.md；pkg/coding-agent/src/core/virtual-models.ts:50-118；ai.CHANGELOG.md 1.3.0 #10995

差距：provider 只在 run 开始时按租户选一次（run.rs:790-860），没有备用。单个上游出故障，所有租户都会受影响。assistant 消息也不记录实际调用的物理模型。

设计：新建 model_routes 表，用声明式配置描述路由，不执行任何租户代码。ModelRouter 实现 LlmProvider 接口：continuation 继续用上一个物理目标，保住缓存；retry 时切换到下一个健康目标；压实请求走 direct_target。熔断状态放在 Redis，粒度为 (provider, model, credential)。Message 增加 model 字段；换模型时丢弃其他模型产生的 thinking。

### B39. 多租户远程 MCP：只支持 HTTP，默认 deferred [medium/L]
依赖：第 9、24、34 项

上游参考：docs/mcp.md:46-77、191-212（工具命名 mcp__server__tool、冲突时加 hash 后缀、exposure 分级、结果超过 20KB 截断）；mcp.CHANGELOG.md（RFC 9207 iss 校验）；pkg/durable/src/search.ts:15-164（BM25）

差距：云端没有 MCP，rupi-server 不依赖 rupi-mcp。本机 rupi-mcp 用 {server}_{tool} 命名，名字冲突时谁先注册谁生效（crates/rupi-mcp/src/lib.rs:1395-1399）；structuredContent 被丢掉。discovery.rs 按子串匹配打分，中文不分词，发现结果也不持久化。

设计：新建 tenant_mcp_servers 表，只接受 streamable HTTP，禁止 stdio。出站请求经过 SSRF 守卫；tools/list 的结果缓存在 Redis。工具命名改为 mcp__server__tool，冲突时加 hash 后缀。默认 exposure 为 deferred，由 search_tools 返回给模型；search_tools 改为 BM25 排序，中文用 2-gram 切分，发现的工具写进会话。结果超过 20KB 时截掉中间部分，全文写入工作区。带 destructiveHint 的工具走 Ask 再 Interrupt 的流程，并且 replay 一律视为 Unsafe。OAuth 用托管的 callback。

### B40. codemode 路线：嵌套工具管线、结构化输出、在执行面运行 QuickJS/WASM（先做 eval） [medium/XL]
依赖：第 3、9、24 项，以及 evals

上游参考：codemode.README.md:3-5、47-78、138-145、316-374；pkg/codemode/src/types.ts:94-243；pkg/coding-agent/src/core/nested-tool-calls.ts:26-31、161-250；pkg/durable/src/codemode/output.ts:98-172

差距：rupi 目前没有任何脚本或虚拟机能力，云端每次只能调一个工具，多步读取要靠多轮 LLM 往返。ToolOutput 只有文本。subagent 的子循环不继承 hooks（crates/rupi-agent/src/subagent.rs:250-256）。

设计：前置工作：抽出 ToolGate 和 NestedRunner，嵌套调用走同一套 hooks 和策略，嵌套过程中遇到 Ask 一律转成错误；ToolOutput 增加 structured 字段，工具声明增加 output_schema；subagent 继承 hooks（只需一行改动，可以先做）。运行时：QuickJS 编译到 wasm32-wasip1，在 wasmtime 上跑，内存上限 64 MiB，用 fuel 和 epoch 控制执行时间，只导入 host_call，时钟和随机数都做成确定性的。VM 放在执行面，通过 start 和 deliver 两个步进接口驱动，不放在控制面。云端默认只放行只读工具。开关默认关闭，先在 eval 中证明每个任务的 token 和往返次数确实下降，再逐步放量。

### B41. 修复与 Pi JSONL v3 的互操作 [low/M]
依赖：第 22 项

上游参考：docs/session-format.md:62-248；pkg/coding-agent/src/core/session-manager.ts _buildIndex

差距：import_jsonl 只认识 4 种条目（crates/rupi-memory/src/session_io.rs:307-437），其他条目被丢掉后，它们的子节点也断了链，现代 Pi 会话导入后是空的。导出时 compaction 条目被放在文件末尾，Pi 打开后会丢掉压实后保留的尾部消息。

设计：导入时，不进上下文的条目当作透明节点处理，leaf 取最后一条。导出时，compaction 条目挂在 leaf 之后，并写入真实的 model 和 usage。用上游的 fixtures 做往返 golden 测试，并给导入加大小上限。

### B42. 统一会话运行时，不再使用进程全局环境变量 [low/L]
依赖：第 22 项

上游参考：docs/sdk.md:24-60；coding-agent.CHANGELOG.md 0.87.0 Breaking（SessionManager 成为唯一权威）

差距：export_session_id 等函数会调用 std::env::set_var（crates/rupi-tools/src/lib.rs:1421-1441），一个进程里跑多个会话时，环境变量会互相覆盖。云端 drive 自己组装 AgentLoop（run.rs:363-416），和 AgentSession 是两套实现，正在逐渐分叉。

设计：把 SessionEnv 显式传给工具，bash 改用 Command::envs 注入。定义 SessionBackend trait，本机用 SQLite 实现，云端用 PG 实现。云端 drive 改为构造 AgentSession。分三步逐步完成。

# chord-scale-architecture

## 上游
Chord（chord.README.md, pkg/chord/docs/spec.md §1）是一个对象/actor 运行时。对象就是某个地址上的状态加代码，身份由 (type, canonicalInput, principalKey) 唯一确定。对象按引用计数存活，默认 lifetime 为 "referenced"，最后一个引用释放后再保留一段 linger（spec §3 Lifetime）。remote 契约必须声明放置规则 hostNamed/hostFamily("session:", "session")：调用方不写主机名，提供方会再算一遍放置，并拒绝放错地方的 hold（invalid/placement，spec §2；v2 的 packages/chord/docs/next/design.md §3 把它叫 hostAddress）。主体（principal）由链路盖章，调用方不能自报。principalKey 包含连接来源和授权版本。同一对象可以按主体返回不同 variant（viewer/participant），撤销通过 invalidatePrincipal 强制释放对象并沿链路传播（spec §4，guide.md "Principals and variants"；v2 design §4 用 revokedBy）。状态是可观察的 JSON cell：远端引用先拿 snapshot，再收 delta ops；重连或 reload 后收 reset，不回放历史（guide "State"，spec §9）。链路流控是端到端 ack 窗口（默认 256 KiB）。state 在发送方按订阅合并，超过 pendingCap 后把最旧的流换成当前值的 reset；call、result 和控制帧永远不排在 state 后面（spec §9 与 §12 Decisions "Flow control holds back state only"，guide.md:744，chord.CHANGELOG 1.3.0）。v2 改为每个 origin 最多 K=2 条未确认的 bound/state 消息，并且不因数量拒绝请求（v2 design §3，design.md:356、374）。root 是短调用树，携带 codeVersion；规则是"长等待是 state，不是挂着的调用"（guide.md:501、811）。热重载流程是 load → prepare → gate（admission cut，upgrade_conflict 可重试）→ commit → retire，遵循两版本规则，过期 root 收到 superseded（spec §6-7、§12 Decisions）。版本机制包括契约的 version/fingerprint/accepts、cell 的 version 加 migrate、链路 PROTOCOL_VERSION（spec §2/§7/§9）。spec §12 列出的已知局限：链路不续传，提交决定不持久化，Node loader 会泄漏，发布队列是进程级同步的；v2 明确 server 只有一个（design.md:80）。Durable 中每个 Session 一条变更线，提交原子，entries 和 ID 不可变（durable spec §1 invariants 1/5/7）；watch 折叠 pending ops，超过 10,000 条改发 [["r", value]]（spec §9.2）；ConversationTranscript 在客户端合并窗口并用 transcript.resume 续传（spec §9.3）；deferred commit 的 withinMs 默认 1 s（spec §4.1），存储往返用 group commit 合并（docs/group-commit.md）。存储由单进程独占，没有跨进程锁（durable.README.md:881）。v2 把调用方的 incarnation 当作围栏：server 忽略旧 worker incarnation 的上报（v2 design §25 R20，design.md:2272-2274）。

## rupi 现状
控制面是无状态多副本，Postgres 是权威，Redis 只做缓存，挂了降级直读库（cache.rs:1-9）。run 的生命周期：任意副本收到 POST /v1/agent（http.rs:628-648），start_run 先读会话行（run.rs:82），再过 admit_run（PG 日账本加 Redis 并发计数，quota.rs:33-86），再拿租约。租约是 Redis SET NX 加 PG `run_id IS NULL` 乐观锁，TTL 180 s，心跳 15 s（quota.rs:11-12、93-135、159-195；db.rs:864-922）；release 里的 clear_run_id 只比较客户端提供的 run_id，不比较 instance_id（db.rs:890-905）。心跳失败只在本地 cancel（run.rs:240-267）。reap_stale_runs 在每次 acquire 和每个 reclaim tick 上运行（quota.rs:122，reclaim.rs:15，db.rs:936-947），走 sessions_tenant_run 部分索引（db.rs:315），开销与活跃 run 数成正比，不是全表。PG 连接池 max_size 32，没有 wait timeout（db.rs:103-107）。run 在接收请求的副本里以 tokio 任务运行，事件走两条容量 256 的有界 mpsc，满了 try_send 直接丢（run.rs:23-58、163-176、333-361）。SSE 用 CancelOnDrop，断开即取消 run（http.rs:91-107），KeepAlive 已开（http.rs:644）。取消后调 executor.abort（run.rs:231-237，tools.rs:214-229），但 app.executor 永远是 PoolScheduler（server lib.rs:292），它没有实现 abort（rupi-runtime/src/pool.rs:156 起的 impl 里没有），走 trait 默认空实现（rupi-runtime/src/lib.rs:255-258）。所以在云路径上 abort 什么都不做，bash 会一直跑到超时（最长 300 s，engine.rs:303）。事件没有 SSE id，没有 Last-Event-ID，只有发起请求的连接能看到 run；会话正在跑时再 POST 直接 409（run.rs:156-160）。没有 cancel、steer、watch 路由（http.rs:24-42，admin.rs:59-96）。rupi-agent 的 MessageInbox（queue.rs:91-210）和 with_inbox（lib.rs:283-310）只有 rupi-cli 在用，follow-up 的消费在 AgentSession（session.rs:360），server 直接用 AgentLoop。云端策略把 write/edit/bash 都设成 AskAction::Interrupt（run.rs:363-368），遇到就以 Aborted 结束 run（rupi-agent/src/lib.rs:922-932），所以今天云 run 被审批切成短段，"长等待是 state"已经通过 interrupts 表实现（db.rs:258-269）。每个 run 开始时 MESSAGES_SNAPSHOT 下发整棵树，包括 base64 图片（run.rs:317，agui.rs:471-499），interrupt 路径再发一次（run.rs:456）。每个 run 都把 AGENTS.md、commands/*.md、input.context 作为 System 节点再 push 进树（run.rs:394-401、883、956-959）并持久化，同时还把 guidance 拼进 system prompt（run.rs:370-375）；System 历史会发给模型（rupi-llm/src/lib.rs:272，anthropic.rs:147）。持久化只发生在 run 结束或 interrupt：persist_tree 先 DELETE 该会话全部 messages 再全量 INSERT，不带 run/epoch 条件（db.rs:1065-1119，run.rs:451、724），错误被 `let _ =` 吞掉。用户消息 id 由客户端提供（agui.rs:421-423），messages.id 是全局主键（db.rs:237-238）。load_tree 遇到反序列化失败的 blocks 退化成纯文本（db.rs:1151-1155）。计费：Usage 事件在 mapper 任务里调 bump_quota_tokens（run.rs:339-348）；流中取消不发 Usage（rupi-agent/src/lib.rs:753-757）；压实调用的 ChatResponse 没有 usage（rupi-llm/src/lib.rs:148-151，rupi-agent/src/lib.rs:1289）。闲置回收：每个副本都跑 reclaim::loop_forever（lib.rs:344-347），每个 tick 对全部租户 reconcile（reclaim.rs:16-20，db.rs:604-608）；候选行不加锁、不 claim（db.rs:1014-1063），快照后 release，mark_snapshotted 无条件执行（reclaim.rs:60-94，db.rs:949-964）；ensure_hot 用 start_run 早先读到的行，看到 hot 就复用 handle（reclaim.rs:96-112，run.rs:230）。import_session 不检查租约（http.rs:598-626）。region 在建会话时确定（http.rs:650-665），执行面按 region 选节点（pool.rs:108-121），start_run 不比较会话 region 和副本 region。listen.rs:181-199 的 axum::serve 没有 with_graceful_shutdown，rupi-server 里没有 signal 处理。Metrics 只有 runs、reject_429 和执行面 used/capacity（lib.rs:36-40，http.rs:76-89）。api_keys 没有 scope，只有 revoked_at（db.rs:213-219、333），鉴权只在请求开始时做一次（http.rs:109-134，db.rs:521-533）。

## 提案
### 所有权模型：按 run 激活的虚拟 actor（PG 租约加单调 epoch 围栏） [critical/M]
上游：chord spec.md §1（对象身份）、§2（placement hostFamily("session:")）；v2 packages/chord/docs/next/design.md §25 R20（design.md:2272-2274，'ignores reports from an older worker incarnation'）；durable spec.md §1 invariant 7（一条变更线）；durable.README.md:881（'One process owns a storage at a time; there is no cross-process locking'）

差距：租约 payload 只有 runId 和 instanceId（quota.rs:20-22），sessions 没有 epoch 列（db.rs:220-235、296）。心跳失败只在本地 cancel（run.rs:246-267）。被取消的 run 返回 Ok(Aborted)，照样进入 finish（run.rs:421-437、503）。finish 调 persist_tree，先 `DELETE FROM messages WHERE tenant_id=$1 AND session_id=$2` 再全量 INSERT，没有 run/epoch 条件（db.rs:1073-1105，run.rs:724）。insert_interrupt（run.rs:440-450）和 mark_hot（db.rs:977-981）也不带围栏。release_lease 里的 clear_run_id 只比较客户端提供的 run_id，不比较 instance_id（db.rs:897-901）：客户端用同一 runId 在 B 上重试后，A 收尾时会把 B 的 PG 租约清掉。触发条件需要更正，原文写的 GC 卡顿不适用于 Rust。现实的触发是 A 的心跳 180 s 写不进 PG，例如 deadpool 只设了 max_size 32、没有 wait timeout（db.rs:103-107），高负载下 pool.get() 会无限等待；或者 A 到 PG 的单向分区、VM 冻结。这时 reap_stale_runs 清掉 run_id（db.rs:936-947），B 接手；A 恢复后心跳返回 false，A 取消并持久化，用旧树覆盖 B 写的会话树。今天这是尾部事件，但后果是静默丢数据。P3 的 detach、P7 的 handoff、P10 的 steal 都会让两个副本同时自认 owner 变成常态路径，所以围栏是这些提案的前提。

设计：最小改动：db.rs 的 v5 迁移只加 `sessions.run_epoch BIGINT NOT NULL DEFAULT 0`，runs 表放到 P3/P6 再建。acquire 用一条语句完成：`UPDATE sessions SET run_id=$r, instance_id=$i, run_epoch=run_epoch+1 WHERE id=$s AND tenant_id=$t AND (run_id IS NULL OR run_id='') RETURNING run_epoch`，Redis NX 降级为可选快路径。新增 `RunFence{tenant, session, run_id, epoch}`，run.rs 的所有写入都带上它，包括消息、interrupt、mark_hot 和用量。围栏的写法要注意：messages 表没有 epoch 列，不能只在 SQL 末尾加 `AND run_epoch=$e`。READ COMMITTED 下 `INSERT ... WHERE EXISTS(...)` 不加锁，检查和提交之间仍可能被抢占。正确做法是在同一事务里先执行 `UPDATE sessions SET updated_at=now() WHERE id=$s AND tenant_id=$t AND run_epoch=$e`，它会拿到行锁，抢占方的 epoch 加一必须等这个事务提交；影响 0 行就回滚、本地 cancel，并停止一切写入。touch_run、clear_run_id 改为 `AND run_epoch=$e AND instance_id=$i`。心跳返回 false 后，finish 不再持久化。同时给 deadpool 配 wait/create/recycle timeout，避免心跳无限挂起。

风险：滚动升级期间老副本不认 epoch，需要分两步：先上线只写 epoch、不强制，全量后再强制。围栏 UPDATE 会让 persist 事务短暂持有 sessions 行锁，所以 persist 事务必须短（依赖 P4 的追加写）。要逐个排查只信 Redis 租约的代码路径。

### 无损的控制和计费事件：只合并文本 delta，不丢 Usage 和终止事件 [critical/M]
上游：chord spec.md §12 Decisions 'Flow control holds back state only... Results must not wait behind state'；guide.md:744（'calls, results ... are never held back'）；chord.README.md PendingOps（只折叠叶子 append/set）；durable spec.md §9.2（leaf fold）

差距：AgentLoop 到 mapper 的队列满时，offer_agent_event 丢弃任意 AgentEvent（run.rs:54-58）。bump_quota_tokens 在同一个消费循环里 await PG（run.rs:337-349），PG 慢的时候 map_task 卡住，下一回合的 Usage 可能被丢。原文说'慢客户端故意慢读就能少付费'不成立：发往 SSE 的 emit 是 try_send（run.rs:48-52），队列满了只丢 SSE 事件，不阻塞 map_task，计费也发生在 emit 之前，所以客户端读得慢不影响计费。真正能被利用的计费漏洞在另外三处。第一，流中取消时 AgentLoop 直接走 finish_aborted，不发 Usage（rupi-agent/src/lib.rs:753-757）。客户端在模型输出快结束时断开 SSE（CancelOnDrop，http.rs:91-107），这一回合的输入和输出 token 就都不入账。第二，压实摘要调用 provider.complete（rupi-agent/src/lib.rs:1289），而 ChatResponse 没有 usage 字段（rupi-llm/src/lib.rs:148-151），摘要 token 从不计费。第三，配额只在 admit 时检查（quota.rs:34-40），一个 run 最多 12 回合（run.rs:363），中途不检查，可以超出 max_tokens_per_day。终止事件丢失属实：RUN_FINISHED、RUN_ERROR 和 interrupt 结局都走会丢的 emit（run.rs:170-173、424-432、481-508），SSE 队列满时客户端收不到终止事件。测试 sse_emit_drops_when_backlog_full 和 agent_event_offer_drops_when_backlog_full（run.rs:1073-1100）把丢弃当成了预期行为。

设计：把计费和事件管道解耦。on_event 闭包是同步的（run.rs:359-361），在 offer 之前把 Usage 累加到每个 run 的 AtomicU64，计费不再经过任何有界队列。drive 在所有退出分支（含 Err）flush 一次，另有周期任务每回合或每 N 秒 flush，减少崩溃时的损失。为了让重试安全，写入幂等的 `run_usage(run_id PK, tokens)` 再汇总到 quota_ledger，因为 bump_quota_tokens 是累加，不幂等（db.rs:1550-1562）。rupi-agent 改动：取消和错误路径补发 Usage，provider 没给用量时用 estimate_tokens（rupi-core/src/lib.rs:719）估算请求和已输出部分，并标记 estimated；ChatResponse 加 `usage: Option<TokenUsage>`，压实调用也发 Usage。在 TurnEnd 时用累加值检查日配额，超了就 cancel。事件通道：控制事件（ToolStart/End、TurnStart/End、Usage、Error、RunEnd）不丢；TextDelta 和 Thinking 在 mapper 按 messageId 合并。终止事件在 drive（async）里用带超时的 send().await 发出。改掉上述两个测试。

风险：估算值和 provider 账单会有偏差，需要对账字段。flush 失败要能重试。新增 ChatResponse 字段要改所有 provider。

### 持久事件日志（每会话 seq）+ SSE id + 断线续传/按 runId attach；断开不等于取消 [high/L]
上游：chord spec.md §12 'Links do not resume'（反例）；v2 design.md §1 principle 5 'Errors look like disconnects. References re-hold by themselves'；durable spec.md §9.3 ConversationTranscript.resume；durable spec.md §4.1 deferred commits（withinMs）与 docs/group-commit.md

差距：SSE 事件没有 id（http.rs:640-641）。CancelOnDrop 断开即取消 run（http.rs:91-107，run.rs:231-237）。KeepAlive 已经开启（http.rs:644），LB 空闲超时一般不会断开，但网络切换、客户端休眠和部署仍会断开，一断就取消。同一 runId 重发时：run 还在跑就 409（run.rs:156-160）；run 已结束时 set_run_id 只要求 run_id 为空（db.rs:872-876），没有去重，客户端网络重试会开出第二个 run，计费也重复。用户消息 id 由客户端提供（agui.rs:421-423），而 messages.id 是全局主键（db.rs:237-238）。跨会话复用同一个 id 会让 persist_tree 的 INSERT 违反唯一约束，finish 里的 `let _ =` 把错误吞掉（run.rs:724），整次 run 的历史静默丢失，而且之后每次 run 都会失败。云 CLI 把整段 SSE body 缓冲完才解析（rupi-cli/src/cloud.rs:174-180）。价值需要下调：云端策略把 write/edit/bash 都设为 Interrupt（run.rs:363-368，rupi-agent/src/lib.rs:922-932），遇到就结束 run，所以今天的 run 是被审批切成的短段，断开的损失有上界。开放自主模式之后才会出现长 run。因此从 critical 降为 high。

设计：分两步。第一步是 S，不需要事件日志：(a) runId 幂等：新建 `runs(tenant_id, session_id, run_id, epoch, owner_instance, state, error, started_at, finished_at)`，主键 (tenant_id, session_id, run_id)。同一 runId 重发时，如果 run 已结束，返回结局（或 409 run_finished），不开新 run。(b) 断开改为 detach：最后一个订阅者离开后 run 继续跑，宽限期由 tenant.settings.detach_grace_secs 控制（0 表示保持现在的行为），结束后照常持久化，客户端用 GET /v1/sessions/{id} 取结果。(c) 消息 id 改为服务端生成，或把主键改成 (tenant_id, session_id, id)，客户端 id 只作去重键。第二步是 L：可续传的事件流。事件日志只是传输层，正式记录是 messages，所以建议放 Redis Streams（`{tenant}:ev:{session}`，XADD MAXLEN 约几千条加 EXPIRE），不放 PG。按每个活跃 run 每秒约 10 条合并后的事件估算，1 万个并发 run 就是每秒 10 万行写入全体租户共享的权威库，还要分区清理。Redis 不可用时退化为不可续传，也就是现在的行为，符合 cache.rs:1-9 的降级约定。PG 只存 runs 的终止状态和按回合追加的 messages（P4）。客户端重连时如果 Redis 里没有，就用 MESSAGES_SNAPSHOT 加 runs.state 恢复。如果坚持把事件放 PG，有两点要改：接手的 owner 在 epoch 加一之后必须从 `max(seq)` 取起点，不能从 sessions.last_seq 取，因为旧 owner 崩溃前可能已写入超过 last_seq 的事件；按月分区时主键必须包含分区键。SSE 使用 `Event::id(seq)`，新增 `GET /v1/sessions/{id}/events?after=` 并认 Last-Event-ID。cloud.rs 改为流式读取。owner 丢失时 reaper 把 runs.state 标为 lost。

风险：detach 宽限期内无人观看的 run 继续花钱，必须计入并发配额。Redis Streams 内存要靠 MAXLEN 限制。key 沿用 {tenant} hash tag（cache.rs:4）。回放的 seq 只在单个会话内连续。

### 按回合 append-only 持久化，取代每次 run 都 DELETE+INSERT 整棵树 [critical/M]
上游：durable spec.md §1 invariant 1/5（提交原子；entries 和 ID 不可变、不复用）、§3.5 Bases and checkpoints、§4.1 deferred commits；docs/group-commit.md；chord README 'Tracking JSON deltas'

差距：每次 run 结束，persist_tree 都删掉会话全部 messages 再逐条 INSERT（db.rs:1065-1119），写放大与历史长度成正比。blocks JSONB 里还带着 base64 图片。每行都要重建 GIN pg_trgm 和 tsvector 索引，tsvector 的生成列还要对每行调用 plpgsql 函数 rupi_cjk_ngrams（db.rs:311-312、368-422）。持久化只在 run 结束或 interrupt 时发生（run.rs:451、724），副本崩溃会丢掉整个 run，包括本轮用户输入，但这期间的 token 已经计费（run.rs:347）。有一个放大因素：每个 run 都把 AGENTS.md、commands/*.md 和 input.context 作为新的 System 节点 push 进树（run.rs:394-401、883、956-959）并持久化，而这些 System 历史会发给模型（rupi-llm/src/lib.rs:272，anthropic.rs:147），guidance 还另外拼进了 system prompt（run.rs:370-375）。所以会话每跑一次，历史和每回合的 token 成本都会多一份 AGENTS.md。另外 import_session 整树替换时不检查租约（http.rs:598-626），会和正在跑的 run 互相覆盖。

设计：messages 加 `seq BIGINT`（会话内单调递增）和 `format SMALLINT`。主键必须先改：不能直接用原文的 `ON CONFLICT (id) DO NOTHING`，因为 id 是全局主键，又来自客户端（agui.rs:421-423），冲突时用户消息会被静默丢掉，跨租户时这一行还会留在别人的会话里。应把主键改为 (tenant_id, session_id, id)，或由服务端生成节点 id。新增 `db::append_nodes(fence, nodes)`：只插入本 run 新增的节点，`INSERT ... SELECT FROM unnest(...)`，带 P1 的围栏。原文写'drive 在收到 TurnEnd 时追加'，但这做不到：run_with_user 在整个 run 期间以 &mut 借用树，TurnEnd 只送到同步 on_event 和 map_task，drive 看不到树。需要在 rupi-agent 加一个可 await 的回合钩子，例如 `on_turn_committed(&SessionTree, new_ids)`，或者发出 AgentEvent::NodeAppended{id, parent, message}。用户消息在 run 开始、调用 LLM 之前就追加，这样崩溃不会丢输入。summary 和 summary_through 只在变化时单独 UPDATE。树节点本来就不可变（rupi-core/src/lib.rs:215-232，全仓库只有 push_with_id 插入节点）。注入的 guidance 和 context 节点标记为临时，不持久化，也不重复 push。fork 和 clone 在新 session_id 上一次性插入；import 是替换现有会话，要先拿租约和 epoch。

风险：需要在 rupi-agent 增加回合钩子，并维护已持久化节点集合。load_tree 把所有行都塞进 current_path（db.rs:1167），分支语义本来就不对，改成按 seq 排序时要一并确认。回合中途的部分文本不写 messages，只放在事件流或 live view 里。修改主键需要在大表上做在线迁移。

### 多端实时观看：snapshot+delta，每个订阅者单独合并，溢出时发 reset [high/M]
上游：chord spec.md §9 Flow control（window、pendingCap，最旧的流变成 reset）与 §12 'coalesced rather than queued... the next frame it gets is current'；v2 design.md §3 Links、§19 Presence；durable spec.md §9.2（PendingOps 折叠，超过 10,000 条发 r）、§9.3（窗口 min/max/before 与 ConversationTranscript gap/loadEarlier）

差距：每个 run 只有一条 mpsc，只有发起 POST 的连接能看（run.rs:163-176）。慢客户端会被直接丢事件，没有重同步（run.rs:48-52），客户端拼出来的文本是错的。MESSAGES_SNAPSHOT 每次都下发整棵历史，包括 base64 图片（run.rs:317，agui.rs:486-494），interrupt 路径还会再发一次（run.rs:456），长会话每个 run 都要付这笔带宽。

设计：分三步。第一步（S，无依赖）：MESSAGES_SNAPSHOT 只带最新 N 条，STATE 里放 historyBefore 游标，新增 `GET /v1/sessions/{id}/messages?before=&limit=` 分页，图片换成对象存储的签名 URL。第二步（S）：在现有单订阅者路径上做合并加 reset。同一 id 的 TEXT_MESSAGE_CONTENT、TOOL_CALL_ARGS、REASONING_MESSAGE_CONTENT 拼接，STATE_DELTA 对同一路径后写覆盖。按字节设上限（约 256 KiB），超过时丢掉待发内容，改发 STATE_SNAPSHOT、窗口化 MESSAGES_SNAPSHOT 和合成的 TEXT_MESSAGE_START/CONTENT（当前部分文本）。第三步（M，依赖 P3）：每个副本一个 SessionHub，`session_id -> broadcast::Sender<Arc<EventBatch>>`。跨副本优先直接读 P3 的 Redis Stream（XREAD BLOCK），不要单独开 pub/sub。owner 维护 LiveView 并定期写入 Redis 缓存。STATE 里带 viewers 计数。只用官方 AG-UI 事件，不用 CUSTOM/RAW（agui.rs:1）。按租户限制订阅者并发。

风险：AG-UI 的 MESSAGES_SNAPSHOT 语义是替换客户端的整个消息列表，标准客户端收到窗口快照会以为更早的历史没了，前端必须识别 historyBefore。如果用 pub/sub，Redis Cluster 下普通 PUBLISH 会广播到全部分片，需要 Redis 7 的 sharded pub/sub，并确认 redis 0.27 cluster_async 是否支持；用 Streams 可以避开这个问题。reset 会让客户端整体重绘。

### 跨副本控制：cancel、steer、follow-up 路由到 run owner [high/M]
上游：v2 design.md §20 'Input into running work: flows kept by their owner'（answer 拒绝过期步骤）；guide.md:501、811（'Keep roots short; long waits are state'）；chord spec.md §9 relay 转发 call/cancel 帧；durable spec.md §6 Submissions and inbox

差距：路由里没有取消或输入接口（http.rs:24-42，admin.rs:59-96）。唯一的取消方式是断开 SSE（只在 owner 副本有效）或心跳丢租约。MessageInbox（queue.rs:91-210）和 with_inbox/inject_steering（rupi-agent/src/lib.rs:283-310）只有 rupi-cli 在用，server 里没有接上。follow-up 的消费在 AgentSession（session.rs:360），不在 AgentLoop。run 进行中再 POST 直接 409（run.rs:156-160）。还有一个前置缺陷：原文说'发起请求的副本可以直接调 executor.abort(handle)，bash 立刻被杀'，这在今天不成立。app.executor 永远是 PoolScheduler（server lib.rs:292），它没有实现 abort（rupi-runtime/src/pool.rs:156 起），走 trait 默认的空实现（rupi-runtime/src/lib.rs:255-258）。所以 run.rs:236 和 tools.rs:225 的 abort 在云路径上什么都不做。execd 启动子进程时没有设置 kill_on_drop（engine.rs:313），只有 abort 才会杀进程组（engine.rs:270-284）。结果是：断开连接、心跳丢失或取消之后，bash 仍会一直跑到超时（最长 300 s，engine.rs:303），继续消耗执行面，还可能在下一个 run 开始后继续写同一个工作区。interrupt 已经是 PG 里的状态（db.rs:258-269），符合'长等待是 state'。

设计：第 0 步（S，必须先做）：在 PoolScheduler 里实现 `async fn abort(&self, h) { self.pick(h)?.abort(h).await }`，并加测试，断言取消后 execd 的进程组被杀。然后新增 `POST /v1/sessions/{id}/runs/{runId}/cancel` 和 `POST .../input {mode: steer|followUp, message, clientRequestId}`，admin 也加强制取消。权威写入在 PG：`runs.cancel_requested_at`（runs 表来自 P3），以及新表 `run_inputs(run_id, seq, mode, body, client_request_id UNIQUE, consumed_at)`。写入后 PUBLISH 到 `ctl:{owner_instance}`，每个副本订阅自己的控制频道。owner 收到 cancel 就调 CancelFlag::cancel，收到 input 就推进 MessageInbox；steering 在回合边界注入（rupi-agent/src/lib.rs:822、1105）。followUp 需要在 drive 里仿照 AgentSession::prompt（session.rs:340-365）加一层循环。兜底路径：心跳顺带读 cancel_requested_at，最多 15 s 一定生效。收到请求的副本可以直接调 executor.abort(handle)（第 0 步之后才有效），不用等 owner。run 已结束时返回 409 run_finished。

风险：PG 和 Redis 都不可用时，取消延迟最长等于心跳周期。steer 只在回合边界生效，工具执行中途不会，需要写进文档。权限要按 scope 校验（见 P12）。abort 会杀掉该 handle 上的所有子进程，这依赖'一个会话同时只有一个 run'。

### 滚动发布的平滑排空与回合边界交接（Chord safe-point 的 SaaS 版） [high/L]
上游：chord spec.md §6-7（root 在启动时的代码版本上跑完；gate 有 deadline；超时整体中止；两版本规则下 superseded 可重试）、§10 host manager liveness；durable.README.md（phase 重启后从头重跑，abandonOnRestart、restart limit）

差距：listen.rs:181-199 的 axum::serve 没有 with_graceful_shutdown，rupi-server 里没有 SIGTERM 处理。发布时副本上所有进行中的 run 被直接杀掉，而树只在结束时持久化（run.rs:433、503、724），本回合的输入和产出全部丢失。被杀的会话要等租约 TTL 和 reap 阈值（都是 180 s，quota.rs:11、122，reclaim.rs:15）过去，这期间一直返回 409。/ready 没有 draining 状态（http.rs:44-74）。执行面上的 bash 也不会停（abort 是空实现，见 P6）。

设计：分阶段做，前两步都是 S，而且不依赖其他提案。第一步：main.rs 注册 tokio::signal SIGTERM，listen.rs 改成 `.with_graceful_shutdown(drain)`。App 加 `draining: Arc<AtomicBool>`，进入排空后 /ready 返回 503，start_run 返回 503 并带 Retry-After，同时新增 RunRegistry 记录本副本的活跃 run 和它们的 CancelFlag。第二步：等 run 自然结束，最多 drain_secs，这个值要小于 k8s 的 terminationGracePeriodSeconds（默认 30 s，需要调大）。到点后对剩余 run 调 cancel：现有路径会以 Aborted 进入 finish，持久化并释放租约（run.rs:503、745-754），会话不会再被锁 3 分钟。这一步依赖 P6 第 0 步才能真正停住 bash。第三步（L，依赖 P1/P3/P4）：用 HandoffFlag 代替 cancel，AgentLoop 在回合边界返回新增的 StopReason::Handoff，drive 先完成按回合持久化，再写 `runs.state='handoff'`、释放租约。新副本在客户端 attach 时 claim，或者由后台用 `FOR UPDATE SKIP LOCKED` 扫描后调用 continue_session（rupi-agent/src/lib.rs:600）继续跑，epoch 加一。硬截止时还在跑的工具通过 abort 中止，并写入一条工具结果 'interrupted by deploy'。runs 记录 code_version。

风险：非幂等工具（bash）在硬截止时只能中断，不能续跑。交接依赖按回合持久化。大量 run 同时交接会形成接手风暴，需要限速。在没有客户端观看时自动续跑会继续花钱，需要按租户策略决定。

### 闲置快照和后台任务必须 claim 会话（exclusive gate），并按工作分片 [high/S]
上游：chord spec.md §7 Gate 与 §12 Decisions 'Gate admission'（admission cut；exclusive 资源先关旧再开新；迟到的请求收到可重试的 upgrade_conflict）、§3 resource 的 exclusive 声明

差距：每个副本都跑 reclaim::loop_forever（lib.rs:344-347，reclaim.rs:8-23）。list_reclaim_candidates 不加锁也不 claim（db.rs:1014-1063），全仓库只有 db.rs:768 一处 FOR UPDATE。snapshot_and_release 检查的是查询时那份旧行的 run_id（reclaim.rs:64），接着做一次耗时的快照和上传，然后 release，最后 mark_snapshotted（reclaim.rs:74-89），而 mark_snapshotted 无条件把 runtime_handle 置空（db.rs:956-961）。run 一侧的竞态窗口比原文说的更大：start_run 在拿租约之前就读了会话行（run.rs:82），ensure_hot 用的还是这份旧行（run.rs:230），看到 hot 就复用 handle（reclaim.rs:103-112）。所以从 preflight 到 ensure_hot 这段时间里完成的回收，都会让 run 在已被释放的卷上运行。池满时 preempt_one 用 Duration::ZERO 选候选（reclaim.rs:41、49），任何没有 run_id 的会话都可能被选中，包括正在 preflight 的会话，所以在执行池压力大时最容易触发。多个副本也会并发快照同一个会话。每个副本每 15 s 对全部租户 reconcile 一次（reclaim.rs:16-20，db.rs:604-608），开销是 O(租户数 × 副本数)。reconcile 用 SET 覆盖 Redis 计数（quota.rs:198-204），而 admit_run 先 INCR，之后才 set_run_id（quota.rs:49-52，run.rs:146），覆盖发生在这中间时计数会偏低，导致超额准入。

设计：claim 是一次短事务内的状态转换，不能在快照期间一直持有行锁。具体做法：`UPDATE sessions SET workspace_state='snapshotting', snapshotting_since=now() WHERE id=$s AND tenant_id=$t AND (run_id IS NULL OR run_id='') AND workspace_state='hot' AND runtime_handle=$h RETURNING ...`，返回 0 行就跳过。候选查询可以用 `FOR UPDATE SKIP LOCKED LIMIT 8` 加上同一事务里的这条 UPDATE，提交后再去做快照。mark_snapshotted 和失败回滚到 hot 都要带 `workspace_state='snapshotting' AND runtime_handle=$h` 条件。set_run_id 加 `AND workspace_state<>'snapshotting'`，返回可重试的 409，对应 Chord 的 upgrade_conflict。ensure_hot 必须在拿到租约之后重新读会话行，不能用 preflight 时的旧行。上线 P1 后，以上条件可以改为 epoch。周期 reconcile 改为只处理当天有 run 的租户，或者删除它，改在 admit_run 发现漂移时按需纠正；reconcile 和 admit 的竞态可以用 Lua 脚本或在 PG 里计数来消除。snapshotting 状态超时（例如 10 分钟）由 reaper 回滚到 hot。

风险：新增的 snapshotting 中间态需要崩溃回收。preempt_one 走 claim 之后，入院时可能要多试几个候选，延迟会增加。

### 持久化格式版本化：混合版本集群里拒绝而不是降级 [high/S]
上游：chord spec.md §2 compatibility rule（version、fingerprint、contract_mismatch）、§7 State compatibility（'Schema changed without a raised version: reload refused'，cell version 加 migrate）、§9 PROTOCOL_VERSION（hello 不一致就断开）、guide.md Errors（upgrade_required、superseded 可重试）

差距：ContentBlock 是 `#[serde(tag="type")]`，没有未知变体兜底（rupi-core/src/lib.rs:29-30），出现一个未知 block 就会让整条 Message 反序列化失败。load_tree 遇到失败会静默退化成 Message::text（db.rs:1151-1155）。滚动发布期间，老副本读到新副本写入的新 block 变体，会把整条消息压成纯文本，工具调用 id 随之丢失，tool_call 和 tool_result 配对断开，provider 可能直接拒绝请求，会话就卡死了。run 结束时 persist_tree 再用 DELETE+INSERT 把整棵树写回（db.rs:1074-1105），损坏变成永久的。Redis 树缓存会扩散这个问题：finish 把降级后的树写进缓存，TTL 300 s（run.rs:725-729），新副本 drive 时优先读缓存（run.rs:202-205），读到的是降级版本。runs 没有记录 code_version，AG-UI 流也不声明协议版本。

设计：messages 加 `format SMALLINT`，在 rupi-core 定义 `pub const MESSAGE_FORMAT: i16`。load_tree 发现 format 高于本副本支持的版本时，返回 UpgradeRequired，start_run 回 503，带 Retry-After 和 code upgrade_required。原文写'LB 会重试到新副本'不成立，LB 通常不会重试非幂等的 POST，所以必须由客户端或 CLI 按 Retry-After 重试。另一种做法是让 ContentBlock 用自定义反序列化保留未知 block 的原始 JSON（Unknown(Value)），老副本原样透传、不发给模型。两种做法都不再回退成纯文本，真正损坏的行报错并计入指标。Redis 树缓存的 key 带上 MESSAGE_FORMAT（Cache::sess_tree_key），或者缓存里的树带版本，不匹配就丢弃。上线 P4 的追加写之后，副本不会改写自己没碰过的行。runs.code_version 记录构建 sha。STATE_SNAPSHOT 带 protocolVersion。

风险：发布顺序必须先上能读新格式的代码，再用功能开关开启写新格式，否则新老副本会互相拒绝。

### 副本存活注册表：owner 死亡时快速接管租约 [medium/S]
上游：chord spec.md §5（links ping/timeout，'a dead link releases everything held through it'）、§10 host manager liveness；v2 design.md §3 Links heartbeat（15 s ping，30 s 关闭）

差距：副本崩溃后，Redis 租约要等 180 s TTL，PG 的 run_id 要等 reap_stale_runs 的 180 s 阈值（quota.rs:11、122，db.rs:936-947）。这段时间里会话的每次 POST 都是 409，客户端没有任何办法提前恢复。

设计：新表 `instances(id, region, started_at, heartbeat_at, draining)`，或用 Redis 键加 PG 兜底，副本每 5 s 心跳一次。acquire 遇到 run_id 被占用时，检查 runs.owner_instance 的心跳：超过 15 s 未更新，或者 draining 且租约已过期，就直接 steal，即 epoch 加一并把旧 run 标为 lost，再往事件日志写 RUN_ERROR owner_lost。因为所有写入都有 epoch 围栏，steal 是安全的。run 租约 TTL 可以缩到 30 s。

风险：依赖所有权模型提案的 epoch 围栏，否则 steal 会造成双写。时钟偏差要求统一使用 PG 的 now()。

### 按 region 放置：run 只由会话所在 region 的副本驱动 [medium/M]
上游：chord spec.md §2 placement（hostNamed/hostFamily；'A host refuses a hold whose input its own contract places elsewhere (invalid/placement)'）；v2 design.md §3 hostAddress（'The caller computes the address... The provider computes it again'）

差距：会话 region 只在创建时确定（http.rs:650-665），执行面按 region 选节点（pool.rs:108-132）。但 start_run 从不比较 sess.region 和 app.region（run.rs 里 region 只出现在 cloud_state 的 328、467 行）。一个 eu-west 会话可能由 us-east 副本驱动，每次工具调用和 fs 读写都要跨大西洋，快照 key 也是在别的 region 读取。

设计：在 start_run 和 attach 的前置检查里加放置规则：sess.region 不等于 app.region，并且注册表（见副本存活提案）里有该 region 的健康副本时，返回 307 或 421 Misdirected，带 `Location: {region_endpoint}/v1/agent`；或者配置为由本副本反向代理 SSE。region 到 endpoint 的映射放在配置或 instances 表。只读的观看可以就近，从本区 PG 读副本读取事件日志。

风险：客户端需要跟随 307 并重发 POST body。没有该 region 的副本时必须允许跨区兜底，否则可用性下降。全局 LB 加 cookie 粘性可以减少重定向次数。

### 主体与作用域：viewer/participant variant，撤销时关闭活跃流 [medium/M]
上游：chord spec.md §4（principal 由链路盖章，principalKey 含 grant revision，variant，invalidatePrincipal 传播）；guide.md 'Principals and variants'（'A variant isn't complete authorization'）；v2 design.md §4 revokedBy 与 gone forbidden/revoked

差距：api_keys 只有 tenant_id 和 revoked_at，没有 scope（db.rs:213-219、333），租户内无法区分只读观看者和可发指令者，也没有按会话的分享授权。tenant_of 只在请求开始时校验一次（http.rs:109-134），revoke_api_key 只设置 revoked_at（db.rs:1821-1830），用被撤销的 key 打开的 SSE 和 run 会一直持续到 run 结束。

设计：迁移：api_keys 加 `scope TEXT DEFAULT 'owner'`（owner|participant|viewer）和 `grant_rev INT`；新表 `session_grants(session_id, key_id|share_token_hash, scope, expires_at)`。tenant_of 返回 Principal{tenant_id, key_id, scope}。viewer 只能 GET events/messages，participant 可以 POST agent 和 input，owner 才能 cancel 或删除。runs.key_id 记录发起者。SessionHub 的订阅者按 key_id 建索引；撤销时 PUBLISH `revoke:{key_id}`，各副本关闭匹配的流，先发 RUN_ERROR code forbidden/revoked。由被撤销 key 发起的 run，按租户策略决定继续还是取消。

风险：API 语义变化，需要保留默认 owner 以兼容现有 key。分享 token 有暴力枚举风险，需要足够熵并限速。

### 这套架构的可运维指标和追踪 [medium/S]
上游：v2 design.md §29 Telemetry、§30 Cost model（'Cost guards: ... reported as ordinary telemetry metrics, always on'）；chord.README.md 'Explicit Context telemetry'

差距：Metrics 只有 runs、reject_429 和执行面 used/capacity 共四项（lib.rs:36-40，http.rs:76-89）。丢弃事件只打 warn 日志（run.rs:48-58），无法统计。租约冲突、reclaim、心跳失败都没有计数，run 级 tracing span 里也没有 run_id 和 epoch。

设计：在 Metrics 里加计数器和量表：rupi_active_runs、rupi_sse_subscribers、rupi_sse_coalesced_total、rupi_sse_resets_total、rupi_agent_events_dropped_total（控制事件拆分后应为 0）、rupi_lease_fence_lost_total、rupi_lease_steals_total、rupi_handoffs_total、rupi_reclaim_claimed_total、rupi_reclaim_conflicts_total，以及 rupi_event_log_flush_seconds 和 rupi_persist_rows_per_turn 两个直方图，统一以 Prometheus 文本暴露。drive 开一个 tracing span，字段包括 tenant、session、run_id、epoch、instance、code_version，并传给执行面调用。

风险：标签不要带 tenant 或 session，避免高基数；租户维度放进日志或抽样追踪。

## 不借鉴
- Chord 的拓扑：一个 server 加按需 fork 的 session worker，再加 relay 树（guide.md 'Hosts and links'，v2 design.md:80 'server: exactly one'，spec.md §10 host manager）。它不能横向扩展，每个会话一个进程的内存和 fd 成本在海量会话下不可接受。rupi 应保持无状态多副本加外部执行面。
- 把 Chord 链路协议（hold/release/shape/state/ack 帧、WebSocket JSON、每链路窗口）当作对外的客户端协议。rupi 已经承诺 AG-UI SSE，只借它的语义：snapshot+delta、按订阅合并、溢出时发 reset、控制帧不排在 state 后面。
- 进程内热重载：带 cache-busting 的 dynamic import、prepare/gate/commit 投票、两版本 retire（spec.md §6-7、§12 'The Node loader leaks'）。Rust 副本靠滚动发布换代，在进程内热换 dylib 既不安全也没必要；safe-point 的思想改用排空加回合边界交接来实现。
- Chord 的'链路不续传'行为：断开即释放全部 hold，in-flight 调用结局未知（spec.md §12）。这正是 SaaS 要避免的，rupi 应该用 seq 加 Last-Event-ID 让订阅可以续传。
- 提交决定和 reconciliation 只存内存，server 重启后忘掉子节点的模块列表（spec.md §12）。rupi 的一切协调状态必须在 Postgres。
- Durable 靠'单进程独占存储，没有跨进程锁'获得单写者（durable.README.md:881）。rupi 多副本必须用 PG 租约加 epoch 围栏实现同一个不变式，不能依赖进程所有权。
- 为每个内部类型维护 TypeBox 契约、fingerprint 和 accepts 适配器（spec.md §2）。Rust 单仓库里的类型由编译器检查；只需要给持久化格式和对外线协议加版本。
- v2 拒绝'因数量拒绝调用或 hold'，改用排队（v2 design.md:374 K16、§31 Rejected 'count limits that refuse calls or holds'）。上游面向单用户可信环境；多租户 SaaS 必须保留 429 拒绝（quota.rs:33-86）和每租户订阅者上限，用于防滥用。
- Cloudflare Durable Object 专属的机制：alarm、onWake、blockConcurrencyWhile（durable.README.md:166-178）和 v2 的 DO 相关集成（design.md §24），与平台强绑定。
- 进程级同步发布队列（spec.md §12）：一个慢观察者会拖慢所有通知。rupi 应该给每个订阅者独立的异步有界队列。
- chord/telemetry-sqlite 本地遥测存储（telemetry.README.md:289）：SaaS 应该用 OTLP 或 Prometheus。
- 基于 linger 的共享绑定缓存（spec.md:180：按类型配置 linger 毫秒数，总数上限 cap 默认 4，最近最少释放的先淘汰）：rupi 已经用 Redis 缓存树和元数据（run.rs:197-229），不需要再加一层进程内绑定缓存。
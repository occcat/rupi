# durable-harness

## 上游
pi-durable 从 v1.0.0 起是新包（durable.CHANGELOG.md:366-370）。它把 agent harness 做成持久化状态机，核心规则是“先提交后展示”：一次 Session commit 原子写入不可变 entry、完整 task 记录和文档，外部只能观察到已提交的状态（durable.README.md:5,94；spec.md:42-46，§1 不变式 1-8 在 spec.md:65-98）。一轮回答拆成几个内建 task：pi.generation 负责请求、重试和分类，每个工具调用是一个 pi.tool task，pi.compaction 负责压缩。每个 phase 的 checkpoint 整份替换写入，进程重启后从最后一个 checkpoint 继续（README:99-113；spec §5.1 2258-2299，§8 4489-4521）。外部副作用走 effect sandwich：先提交 intent，再执行副作用，最后提交结果；重开时如果停在 intent phase，说明副作用可能已经发生（spec §5.2 2372-2388）。工具在 execute 前持久化最终参数和 replay 策略，默认 unsafe。崩溃后只有存储的策略和当前声明都是 safe 才重跑，否则给模型一条带已提交部分输出的 interrupted 错误结果（README:277；spec §7.3 3995-4006、4146-4152，§8.4 5061-5079；src/harness/tool.ts:186-226）。上游的内建 read/write/edit/bash（pkg/durable/src/tools/*.ts）都没有声明 replay，所以全部是 unsafe；声明 safe 的只有 tool-search、codemode 和 subagent 示例（tool-search.ts:62，codemode.ts:247，spec.md:4057）。open 时把残留的 running task 改回 pending，restarts 加 1（src/harness/scheduler.ts:428-466）。close 不算崩溃，写回 pending 但不计数。task 提交新状态时计数清零；达到 restartLimit（默认 5）的 task 以可见方式结束，不会无限崩溃下去（spec §5.4 2530-2573；CHANGELOG:117）。模型请求进行中宿主死亡时，这次 attempt 按 retry 策略记为失败，不重发：partial 转成 aborted entry，退避用持久 sleep 实现，次数用尽则 runFailure=interrupted（spec §8.3 4890-4921；generation.ts:262-266,495-546；CHANGELOG:118,182）。流式 partial 和工具输出走 deferred commit：watch 立即可见，最迟 persistWithinMs（默认 1s）或者下一个读取 pi.live 的 commit 时落盘。32 路流在 SQLite 上，存储事务数因此降到原来的 1/5.7 到 1/27，代价是崩溃最多丢约 1 秒进度（spec §4.1 1983-2065；CHANGELOG:49）。group commit 让一个 Session 的唯一 mutation line 在存储往返期间不再被占住：在途期间到达的 commit 合并成一次 Storage.commit()，每个 commit 保留自己的 seq 和结果，读到未发布 key 的 job 等待 gate。128 会话、4ms RTT 下吞吐从约 5 run/s 提到约 440 run/s（CHANGELOG:160；docs/group-commit.md §1、§4-5；src/session/writer.ts:71-121）。submission 带 requestId 去重，submit 和 wait 与执行解耦，取消 wait 不会取消工作；watch 和 late-join 从已提交的 view 加窗口恢复（README:78,129-138,477-527；spec §6 2928）。从 transcript 推导上下文时，会给缺结果的 tool call 合成错误结果，丢弃没有对应调用的结果，并剔除 aborted/error 的 assistant 消息（spec §2.1 规则 7-9，spec.md:325-338）。usage 文档把失败和中止的 attempt 也计进去（spec §8.6 5171-5203）。存储错误会让整个 Harness 失败、需要 reopen；同一份存储同一时刻只由一个进程拥有，没有跨进程锁（README:881,910-926；spec §10.1 6121-6160）。

## rupi 现状
【运行路径】POST /v1/agent 进入 start_run。先做配额入院 admit_run：读 today_quota，再 count_active_runs，然后 Redis INCR，最后 bump_quota_run（quota.rs:33-86）。接着 acquire_lease：先对全表执行一次 reap_stale_runs（quota.rs:122），再 Redis NX（TTL 180s），再在 Postgres 上用 `sessions.run_id IS NULL` 乐观锁（quota.rs:93-135；db.rs:864-888）。之后 spawn drive（run.rs:163-176）。drive 从 Redis 缓存或 PG 读入整棵 SessionTree（run.rs:197-229），每 15s 心跳一次（run.rs:240-267；quota.rs:159-195）。AgentLoop 全程在内存里改 tree：用户消息在 rupi-agent/src/lib.rs:582 push，流式 partial 只存在局部 String 里（lib.rs:720），assistant 在 :814 push，工具结果在 :1088-1094 push。事件立即经 mapper 发成 AG-UI SSE（run.rs:333-357），也就是先展示后提交。只有 run 结束时 finish 才调用 persist_tree（run.rs:723-724）。persist_tree 在一个事务里 DELETE 整个会话的 messages，再逐行 INSERT 全部节点（db.rs:1065-1119）。messages 上有 trgm 和 tsv 两个 GIN 索引（db.rs:312,328,421），所以每个 run 的写入量是 O(会话长度)。finish 用 `let _ =` 吞掉 persist_tree 的错误（run.rs:724），随后照样把内存中的 tree 写进 Redis（run.rs:725-730）并发出 RUN_FINISHED success（run.rs:505-507）。PG 写失败对客户端完全不可见。load_tree 按 created_at 排序，并把所有行串成 current_path（db.rs:1134,1167）；created_at 来自各副本的本地时钟，没有单调 seq。每个 run 都会把 [workspace AGENTS.md] 和 commands 作为 System 节点 push 进 tree 并落盘（run.rs:883,956），同一段 guidance 还会追加进 system prompt（run.rs:375-380）。结果是每次请求的 token 随 run 次数线性增长。interrupt 路径先 insert_interrupt，再单独 persist_tree，两步不在同一事务（run.rs:440-451）。resume 先把 interrupt 标成 resolved，再执行工具，到 finish 才落盘（run.rs:563,595-684）。

【副本在 model stream 中途死亡】本 run 的用户消息、partial 以及此前的 assistant 和 tool 节点全部丢失，PG 仍是 run 开始前的 tree，客户端已经看到的文本在重载后消失。Usage 只在流完整结束后发出（lib.rs:801-808），取消分支（lib.rs:753-758）和错误分支（lib.rs:761-780）都不发。租户主动断开 SSE 会触发 CancelOnDrop（http.rs:92-107），本轮 token 就不会经 bump_quota_tokens 记账（run.rs:339-348），而 delta 已经送到了客户端。provider 层只在建连前重试 3 次（rupi-llm/src/lib.rs:614），流中断时 `chunk?`（:621）直接让整个 run 失败。

【副本在 tool call 中途死亡】执行面是外部的 execd/sandboxd，已经改过的文件和执行过的 bash 保留下来，但 transcript 里没有任何记录。execd 的 handler 直接 await 子进程（execd.rs:246-262）。子进程设了 kill_on_drop（jail.rs:183），但它只杀直接子进程 sh；命令跑在自己的进程组里（jail.rs:195），孙进程会成为孤儿继续运行，直到下一次 abort 或 destroy。

【恢复】没有续跑。reap_stale_runs 要在 180s 无心跳之后才清 run_id（db.rs:936-947；reclaim.rs:15）。这段时间内客户端重试一律得到 409（run.rs:146-161），租户的并发槽泄漏约 180-195s。没有 run 级幂等：set_run_id 只检查 `run_id IS NULL`（db.rs:874-876）。也没有 fencing：finish 的 persist_tree 不校验租约。心跳失败后 cancel，agent 返回 Aborted，drive 照样进入 finish（run.rs:253-264 → 502-503 → 724），僵尸副本会用旧 tree 覆盖接管者写入的新 transcript。release_lease 有 run_id 守卫（db.rs:891-905），persist_tree 没有。

【会话变砖】没有补全缺失工具结果的逻辑，全仓检索不到。云端策略对 write/edit/bash 走 Interrupt（run.rs:363-368）。AgentLoop 第一阶段遇到第一个需要审批的调用就直接 return（lib.rs:922-932）：排在它前面、已经放行的调用不会执行，排在后面的调用连策略都不判，resume 只补被 interrupt 的那一个（run.rs:530-684）。compaction_cut 只按 token 切，不对齐 call/result 配对（rupi-core/src/lib.rs:284-301），保留窗口可能以一条失去对应调用的 Tool 消息开头。to_openai_messages 原样输出（rupi-llm/src/lib.rs:268-334）。取消时 partial 作为普通 assistant 文本 push（lib.rs:753-756），之后会被当作正常回答送进上下文。

【运维】release profile 是 panic = "abort"（Cargo.toml:25-29），LAUNCH 用 `cargo build --release -p rupi-server` 构建（docs/LAUNCH.md:111），一个 run 的 panic 会杀掉整个副本。serve 没有 graceful shutdown（listen.rs:181-199；lib.rs:303-351），全仓也找不到信号处理。drive 是脱离管理的 tokio::spawn（run.rs:169），进程退出时直接消失。/metrics 只有 runs_total、429 和执行池计数（http.rs:76-90）。reclaim 循环每 15s 在每个副本上遍历所有租户调用 reconcile，每个租户一次 count(*)（reclaim.rs:16-20；db.rs:604-608）。

【已有可复用的部分】PG 权威加无状态副本、外部执行器、Redis 租约和心跳框架，AgentLoop::continue_session（lib.rs:600-632），以及 MessageInbox/steering（queue.rs；lib.rs:283-289）。chaos 测试只覆盖“副本死后会话和快照仍可读”（tests/cloud_chaos.rs:245-350），不覆盖 run 中途死亡。CLI 有按 turn 写入的 persist_turn（rupi-memory/src/lib.rs:1343；rupi-cli/src/main.rs:1561-1589），但它是在 run 之后调用，云端也没有用上。

## 提案
### 按步增量提交 transcript 和 runs 检查点（commit-before-show 的 PG 版） [critical/L]
上游：durable.README.md:5,94-113；spec.md §1 不变式 1-3（65-82）、§4（1823-1981）、§8.2 pi.live（4545-4775）；src/harness/generation.ts（request 和 classify 各提交一次）

差距：已核实。tree 只在 run 结束时由 finish 调 persist_tree 整体 DELETE+INSERT（run.rs:723-724；db.rs:1065-1119），中途所有节点只在内存里（agent lib.rs:582,814,1088-1094），SSE 先于持久化发出（run.rs:333-357）。补充：finish 用 `let _ =` 吞掉写入错误（run.rs:724），仍然把内存 tree 写进 Redis（run.rs:725-730）并发出 RUN_FINISHED success（run.rs:505-507），PG 写失败对客户端不可见。messages 没有 seq，按副本时钟写的 created_at 排序，并把所有行串成 current_path（db.rs:1134,1167）；一个副本时钟偏慢就会让跨 run 的消息顺序错乱、call/result 配对断开。每个 run 都重复把 guidance 作为 System 节点 push（run.rs:883,956），而 guidance 已经进了 system prompt（run.rs:375-380），所以每次请求的 token 随 run 次数线性增长。

设计：阶段 0（S，不依赖其他提案，可以先上）：(a) load_remote_guidance 不再往 tree 里 push System 节点，只进 system prompt（run.rs:883,956）；(b) finish 的 persist_tree 失败时发 RUN_ERROR，不再发 success，并且不把未落盘的 tree 写进缓存。阶段 1：rupi-agent 新增 `trait RunJournal: Send+Sync`，方法有 `user(&SessionNode)`、`assistant(&SessionNode, usage, stop)`、`tool_intent(call_id, name, args, replay)`、`tool_result(&SessionNode)` 和 deferred 的 `progress(partial)`。通过 `AgentLoop::with_journal` 注入，分别在 lib.rs:582、:814、execute_allowed 之前（:1001/:1032）和 :1088 调用。journal 返回 Err 时只让当前 run 以存储错误失败，不影响副本上的其他 run。CLI 可以用现有 SQLite persist_turn 实现同一个 trait，属于可选项。rupi-server 实现 `PgJournal{pool, tenant, session, run_id, fence}`，每步一个事务：`INSERT INTO messages(id, tenant_id, session_id, run_id, seq, ...) ON CONFLICT (id) DO NOTHING`，加上 `UPDATE runs SET phase, checkpoint, updated_at WHERE id=$1 AND fence=$2`，返回 0 行说明租约已丢，立即 cancel。迁移 v5：新增 `runs(tenant_id, id, session_id, status queued|running|sleeping|interrupted|done|failed|aborted, phase, checkpoint JSONB, attempt, restarts, owner_instance, lease_until, fence BIGINT, failure_reason, failure_detail, first_node, answer_node, created_at, updated_at, PRIMARY KEY(tenant_id, id))`；`messages` 增加 `seq BIGINT` 和 `run_id`。seq 用 `UPDATE sessions SET next_seq = next_seq + k RETURNING` 分配；每个会话同时只有一个活动 run，这一行不会成为跨 run 热点。load_tree 改为 ORDER BY seq，老行按 created_at 回填 seq。AG-UI 的 TEXT_MESSAGE_END 和 TOOL_CALL_RESULT 等 journal 提交后再发，delta 照旧立即发。finish 只更新 runs.status 和摘要列。persist_tree 只保留给 import/fork/clone 使用（http.rs:539,615）。compaction 的 summary/summary_through 和对应节点放在同一事务里写。

风险：每轮多出 2+2k 次小事务（k 为工具调用数），同 region 内单次约 1-3ms，相对 LLM 延迟可以忽略。滚动升级期间，老副本的 DELETE-all 会冲掉新副本写入的行，需要先上线能兼容读的版本，再用开关切换写路径。Redis 缓存 sess_tree_key（run.rs:197-229,725-730）和增量写会不一致：缓存要带 seq 版本，或者在取得租约时失效。阶段 0 去掉 System 节点之后，老会话里已经累积的重复 guidance 节点需要一次性清理，或者在 prompt_history 里过滤掉。

### run 租约 fencing 加恢复 reconciler：残留 running 的 run 可续跑或可见地结束 [critical/L]
上游：spec.md §5.1 open 时 reconcile（2351-2370）、§5.4 Restarts（2530-2573）、§10.1（6121-6160）；src/harness/scheduler.ts:428-466；README.md:214-222

差距：已核实。副本死后只能等 reap_stale_runs 在 180s 后清 run_id（db.rs:936-947；quota.rs:11,122；reclaim.rs:15），run 本身既不结束也不续跑，这段时间重试一律 409（run.rs:146-161）。租约没有 fence。release_lease 有 run_id 守卫（db.rs:891-905），但 finish 的 persist_tree 没有。心跳失败时 cancel，agent 返回 Aborted，drive 照样进入 finish（run.rs:253-264 → 502-503 → 724），会用旧 tree 覆盖接管者的 transcript。还有一种情况：旧 run 在下一次心跳之前正常结束，也会照样覆盖。

设计：阶段 0（S，不依赖 runs 表，应当立刻做）：persist_tree 在事务开头执行 `SELECT run_id FROM sessions WHERE id=$1 AND tenant_id=$2 FOR UPDATE`，与调用方的 run_id 比较，不一致就回滚并报错；interrupt 和 finish 两条路径都传入 run_id。这样就能堵住今天的僵尸覆盖。阶段 1：以 PG 为权威取得租约：`UPDATE runs SET owner_instance=$me, fence=fence+1, lease_until=now()+interval '45s', status='running' WHERE tenant_id=$t AND id=$r AND (owner_instance IS NULL OR lease_until<now()) RETURNING fence`。用部分唯一索引 `runs(session_id) WHERE status IN ('queued','running','sleeping')` 保证每个会话只有一个活动 run，Redis NX 降级为快路径提示。TTL 建议 45-60s，30s 在 PG 抖动时容易误判租约丢失。所有 journal 写和终态写都带 `AND fence=$f`。心跳改成每个副本批量发送（见吞吐提案），RETURNING 里缺失的 id 就 cancel 对应的 run。reconciler 放进 reclaim::loop_forever，任何副本都能跑：`SELECT ... FROM runs WHERE status='running' AND lease_until<now() ORDER BY lease_until LIMIT 64 FOR UPDATE SKIP LOCKED`，restarts+=1，然后按 phase 处理。request 阶段：把 Redis run_live 里最后一次保存的 partial 转成 aborted assistant 节点（不进后续上下文），attempt+1；策略允许时置 sleeping，wake_at=now()+backoff，否则置 failed，reason=interrupted。tools 阶段：逐行处理 tool_calls（见 effect sandwich 提案），safe 的重跑，unsafe 的补 interrupted 错误结果。restarts 达到 restart_limit 时置 failed，reason=restart_limit，并补齐缺失的结果。dispatcher 用 SKIP LOCKED 抢 `status IN ('queued','sleeping') AND (wake_at IS NULL OR wake_at<=now())` 的 run，派发时遵守租户 max_concurrent_runs，然后 ensure_hot（reclaim.rs）并用 PG 里的 tree 执行 continue_session（lib.rs:600-632）。租户设置 `on_crash: continue|fail`，决定是否在无人值守时自动续跑，默认 fail。

风险：自动续跑会在没人观看时继续花 token，必须受 retry 和 restart_limit 约束，并且由租户开关控制。reconciler 只在 lease_until 已过期时接管，并且接管时 fence+1，避免和心跳迟到但仍然存活的 owner 抢同一个 run。恢复时需要重建 MemoryManager、skills 和 guidance，这些不是确定性状态，必须从 PG 和执行面重新加载。阶段 1 依赖增量提交提案。

### 工具 intent 提交与 replay 策略（effect sandwich） [high/M]
上游：spec.md §5.2（2372-2388）、§7.3 replay（3680,3995-4006,4146-4152）、§8.4（5035-5118）；src/harness/tool.ts:186-226；README.md:277；上游内建工具不声明 replay（pkg/durable/src/tools/*.ts）

差距：已核实。Tool trait 没有 replay 语义（rupi-tools/src/lib.rs:74-89）。执行前不记录 intent，lib.rs:989-1048 执行之后结果也只在内存里。云端 interrupt 记在单独的 interrupts 表里，只记第一个需要审批的调用（lib.rs:922-932；db.rs:258-269,1465-1493）。更准确地说，第一阶段遇到 Ask 就直接 return：排在它前面、已经放行的调用不会执行，ToolStart 已经发出却没有 ToolEnd；排在它后面的调用连策略都不判。resume 先把 interrupt 标成 resolved 再执行工具，两步不在同一事务（run.rs:563,595-684）。

设计：Tool trait 增加 `fn replay(&self) -> Replay { Replay::Unsafe }`。云端 read、glob、grep、think 声明 Safe（tools.rs:13-43）。write 声明 Safe 是对上游的偏离，上游内建 write 默认 unsafe；理由是整文件覆盖在 Sequential 执行下是幂等的（lib.rs:260 为默认），但在 ToolExecution::Parallel 下必须当作 Unsafe。edit、bash 和 memory 写入声明 Unsafe。新增表 `tool_calls(tenant_id, run_id, call_id, assistant_node, name, args JSONB（hook 改写后的最终参数）, replay, status pending|awaiting_approval|executing|done, result_node, output_tail, started_at, ended_at, PRIMARY KEY(tenant_id, run_id, call_id))`。assistant 节点提交时，把整轮调用一次性写成 pending。执行前在同一事务里写 `status='executing'` 和 runs.phase='tools'；结果节点和 `status='done'` 在同一事务里提交。AgentLoop 第一阶段改为判完全部调用再返回：PendingInterrupt 改成 Vec，被问询的调用标 awaiting_approval，其余保持 pending。run.rs 已经要求 resume 覆盖所有 interrupt（run.rs:117-125），可以直接复用。interrupts 表降级为兼容层。resume 执行所有非 done 的行，这也修复了多调用 interrupt 只续一个的问题。恢复规则沿用上游：存储的 replay 和当前声明都是 Safe 才重跑，否则追加 is_error 结果“Tool X was interrupted and may have partially run”，并附上 output_tail。恢复时不重跑 before hook，直接使用存储的参数。

风险：replay 分类一旦出错，后果是真实的重复副作用，所以默认必须是 Unsafe。MCP 和外部工具无法判断时一律当 Unsafe。tool_calls 表随调用量增长，需要在 run 结束若干天后按分区或批量清理。

### 执行面幂等 call_key 与断连不杀：控制面换副本后可重新接上在途命令 [medium/M]
上游：spec.md §7.3 nested key 复用（3727-3740）、§5.2 “retries safely, polls an external handle”（2380-2383）；README.md:328（replay-safe 工具仍需自己的幂等）

差距：已核实。ExecRequest 只有 command 和 timeout_secs，没有幂等键（rupi-runtime/src/lib.rs:124-129；Executor trait 在 :234-256）。execd 的 /v1/exec handler 直接 await engine.exec_for（execd.rs:246-262），子进程带 kill_on_drop（jail.rs:183）。控制面连接一断，直接子进程 sh 就会被杀；命令跑在自己的进程组里（jail.rs:195），孙进程会成为孤儿继续运行，直到下一次 abort 或 destroy（engine.rs:270-285）。新副本无从知道命令是否跑完。HttpExecutor 用的是不带超时的 reqwest::Client::new()，也没有重试（http.rs:22,162-173）。

设计：第一步（S）：execd 和 sandboxd 的 handler 用 tokio::spawn 把执行和 HTTP 连接解绑，两个守护进程共用 Engine（sandbox.rs:92），改一处即可。第二步：ExecRequest、FsWriteRequest、FsEditRequest 增加 `call_key: Option<String>`，值为 `{tenant}/{run_id}/{call_id}`。Engine 为每个 handle 维护 `HashMap<call_key, Shared<JoinHandle<ExecResult>>>`，带 TTL（例如 10 分钟）和每个 handle 的上限（例如 16 条；stdout 已有长度上限）。带重复 key 的请求等待同一个执行，或者直接返回缓存的结果。新增 `POST /v1/exec/status {handle, call_key}`，返回 running、done(result) 或 unknown，先过 tenant_of 和 auth_ok 校验（execd.rs:253-254）。客户端侧需要改 HttpExecutor（http.rs:11）、SandboxExecutor（sandbox_http.rs:12）和转发用的 PoolScheduler（pool.rs:79）。reconciler 遇到 executing 且 Unsafe 的调用时先查 status：done 就用真实结果提交，running 就接着等，unknown（执行面重启过）才补 interrupted。显式 abort 照旧按 handle 杀进程组。

风险：断连不再杀命令，意味着没人等待的命令会一直跑到 timeout（最长 300s，engine.rs:303）。run 被 cancel 时必须显式调用 abort，run.rs:230-237 已经这样做。结果缓存占内存，需要上限和 TTL。call_key 必须带租户前缀，并在 execd 端与 handle 的租户校验，防止跨租户读到别人的结果。依赖 effect sandwich 提案提供 call_id 和 tool_calls 行。

### attempt 计数、崩溃循环守卫（restart_limit）与中止和失败 attempt 的 usage 入账 [high/M]
上游：spec.md §5.4 Restarts（2530-2573）、§8.3 interrupted attempt（4890-4921）、§8.6（5171-5203，failed and aborted attempts count）；src/harness/generation.ts:262-266,495-546；src/harness/scheduler.ts:454-456；CHANGELOG Unreleased:117-118,182

差距：已核实。没有 run 级 attempt 计数。流中错误 `chunk?`（rupi-llm/src/lib.rs:621）经 lib.rs:779 直接让 run 失败，partial 被丢弃。Usage 只在流完整结束后发出（lib.rs:801-808），取消分支（lib.rs:753-758）和错误分支（:761-780）都不入账。断开 SSE 会触发 CancelOnDrop（http.rs:103-107），而 delta 已经送到客户端，所以租户可以在流快结束时断开，拿到内容却不计入 token 配额（quota_ledger 只累加 Usage 事件，run.rs:339-348）。用平台 key 时（resolve_byok 回退到 RUPI_API_KEY 等环境变量），这直接就是平台成本；用租户自己的 BYOK key 时，只是配额失真。release 构建是 panic=abort（Cargo.toml:29），一个 run 的 panic 会杀掉整个副本。

设计：拆成两部分。(1) usage 入账（S，现在就能做）：AgentLoop 在取消和错误分支上，用已经收到的 StreamEvent::Usage 发出 Usage 事件；没有收到时，用 tokens.rs 估算 input 和 partial 的 output，并标记 estimated。Anthropic 在 message_start 就给出 input（anthropic.rs:400,507-540），但目前只在 message_delta 时推送，应当在 message_start 时就推一次。admit_run 只在入院时检查日 token（quota.rs:34-41），可以改成“预占再结算”：发请求前预扣估算的 input，结束、取消、出错时按 provider 回报的数字结算。新增 `session_usage(tenant_id, session_id, model, input, output, cache_r, cache_w, aborted_attempts)`。(2) restart_limit（依赖 reconciler 提案）：runs 表增加 attempt 和 restarts 列，提交新 phase 或新节点时 restarts 清零。request 阶段被打断算一次失败 attempt，partial 转成 aborted 节点，用 status=sleeping 加 wake_at 实现持久退避；graceful drain 打断的不计数，直接重发。可重试的流中错误（429、5xx、连接重置）同样追加 aborted 节点并退避，不再让整轮失败。restarts 达到 restart_limit（默认 3-5）时置 failed，reason=restart_limit，写 admin_audit 和指标。指标：`rupi_run_restarts_total`、`rupi_runs_failed_total{reason}`、`rupi_attempts_interrupted_total`。(3) panic 隔离：新建 `[profile.release-server] inherits="release" panic="unwind"`，同步修改 LAUNCH.md:111。改成 unwind 之后，tokio::spawn 本身就会把 panic 收敛在 drive 任务内（run.rs:169），不需要 catch_unwind。真正缺的是收尾：drive 里放一个 Drop guard，或者由外层 await JoinHandle 检查 JoinError，负责 release_lease、release_run、发 RUN_ERROR，并把 run 标成 faulted。否则 panic 之后租约要等 180s 才回收，并发计数一直泄漏到下一次 reconcile。

风险：估算的 token 和 provider 实际计费会有偏差，需要事后对账。restart_limit 太小会误杀正常的长任务，因为同一副本上所有在途 run 会一起被计入重启次数，上游有同样的取舍。改成 unwind 后，`Mutex::lock().unwrap()`（例如 lib.rs:280）在 poison 时会连锁 panic，需要排查。unwind 让二进制略大、略慢，需要压测确认。

### run 级幂等入院：AG-UI run_id 和消息 id 去重 [high/S]
上游：durable.README.md:129-138（requestId 返回同一 submission）；spec.md §6 2928（requestId 先于任何写去重）、§10.1 6130-6132（重开后复用 requestId）；src/harness/submissions.ts:232-241

差距：已核实。start_run 不查 run_id 历史，set_run_id 只检查 `run_id IS NULL`（db.rs:874-876）。客户端在 run 完成后用同一个 run_id 重放 POST，会再推一次用户消息并重跑一遍。用户消息的节点 id 每次随机生成（core lib.rs:215-217）；客户端传来的 AguiMessage.id 只用作 Message.id（agui.rs:421-424），没有用作节点 id，也不参与去重。修正：L7 负载均衡在已经发出字节之后一般不会自动重试 POST，重复请求主要来自客户端 SDK 的超时重试。今天崩溃的 run 在 PG 里什么都不留，所以崩溃后的重试实际上是一次正确的重跑；进行中的重复请求会被租约拦成 409。今天真正有害的场景只有“成功之后响应丢失、客户端重试”。

设计：基于 runs 表主键 (tenant_id, id=run_id)，start_run 第一步执行 `INSERT INTO runs ... ON CONFLICT DO NOTHING RETURNING`。如果已存在：running 时挂接到进行中的 run（见解耦 SSE 提案），或者返回 409 并带上 `{runId, status}`；done 时直接返回 MESSAGES_SNAPSHOT 加 RUN_FINISHED，不重跑；failed 时返回 RUN_ERROR 并带 failure_reason。用户消息用 `push_with_id`（core lib.rs:220-231），写入时 `ON CONFLICT (id) DO NOTHING`。messages.id 是全局 TEXT 主键（db.rs:238），客户端给的 id 必须加 tenant/session 前缀或者改成复合键，否则一个租户可以抢占别的租户的 id，造成插入失败。

风险：依赖 runs 表，所以只有在增量提交或 fencing 阶段 1 之后才是 S。客户端生成的 run_id 不可信，必须限定在租户范围内，并且只接受与会话匹配的记录。done 的 run 要保留一段时间（例如 7 天）供重放查询。

### 优雅下线与 run 移交：SIGTERM 时重新入队续跑，不计入崩溃 [high/M]
上游：spec.md §5.1 close（2359-2370）、§5.4 “A close is not a crash”（2531-2536）、§8.3 close 后重发（4900-4921）；README.md:216

差距：已核实。axum::serve 没有 with_graceful_shutdown（listen.rs:181-199），rupi-server 里没有任何信号处理（lib.rs:303-351）。drive 是脱离管理的 tokio::spawn（run.rs:169），没有在途 run 的登记，进程退出时直接消失。/ready 只检查 PG 和执行面（http.rs:44-73），没有 draining 状态。滚动发布收到 SIGTERM 时，在途 run 和崩溃一样整轮丢失（finish 才落盘，run.rs:723-724），租约要 180s 后才回收（quota.rs:11）。

设计：阶段 0（S，不依赖其他提案）：App 增加 draining 标志和在途 drive 的登记（JoinSet 或计数器加 Notify）。收到 SIGTERM 后：/ready 返回 503，让负载均衡停止导流；/v1/agent 返回 503 并带 Retry-After；等在途 run 自然结束，最长 drain_secs（例如 120s）；超时后再 cancel 并退出。注意 axum 的 graceful shutdown 会等所有连接关闭，而 SSE 是长连接，所以必须由 drain 逻辑主动结束，不能只依赖 axum。k8s 的 terminationGracePeriodSeconds 要大于 drain_secs，并配合 preStop。仅这一步就能消除大部分短于宽限期的 run 被打断的问题。阶段 1（依赖增量提交和 reconciler）：超时后在下一个安全点（turn 边界，或者工具 intent 已提交之后）cancel，提交检查点，置 `status='queued'`，restarts 不加，释放租约（fence+1），让其他副本立刻接手。在途的模型请求按 close 语义重发，不算失败 attempt；在途的工具由新副本靠 call_key 重新接上。SSE 客户端收到 RUN_ERROR，code=handover；解耦 SSE 落地之后可以无感重连。

风险：run 卡在长工具调用上时 drain 等不完，阶段 0 只能 cancel，阶段 1 要靠 call_key 重新接上。被移交的 run 要重新加载 guidance、memory 和 skills，可能和原副本上的状态略有差异。drain 期间副本容量下降，需要 maxSurge 留出余量。

### 上下文推导规范化：补齐缺失工具结果、剔除 aborted partial（防会话变砖） [high/S]
上游：spec.md §2.1 规则 7-9（325-338）；§8.1（4540-4543，aborted 和 error 的 assistant 不进后续请求）

差距：已核实，并且比原文描述的更广。rupi-core、rupi-agent 和 rupi-llm 都没有修复缺结果的 tool call（to_openai_messages 在 rupi-llm/src/lib.rs:268-334 原样输出，prompt_history 在 core lib.rs:261-279 也不处理）。interrupt 时同一条 assistant 消息里只续被问询的那个调用（lib.rs:922-932；run.rs:530-684），其余调用永远缺结果，provider 返回 400，会话无法继续。新增证据：compaction_cut 只按 token 从尾部累加来切（core lib.rs:284-301），不对齐 call/result 配对；保留窗口可能以一条失去对应调用的 Tool 消息开头，OpenAI 会拒绝这种 role=tool 消息。崩溃，以及 interrupt 写入不一致（run.rs:440-451,563），也会留下悬空调用。取消时 partial 作为普通 assistant 文本 push（lib.rs:753-756），之后会被当作正常回答送进上下文。

设计：在 rupi-core 的 SessionTree::prompt_history 返回前，或者在 rupi-agent 构造 ChatRequest 前（lib.rs:705-714），加 `normalize_tool_pairs(history)`。规则：(1) 对每条 assistant 的 ToolCall，在下一条 assistant 之前收集对应的 ToolResult，移到紧跟该 assistant 的位置；(2) 缺失的结果合成 `ToolResult{is_error:true, content:"No result: the call was interrupted before it finished"}`；(3) 丢弃找不到对应调用的结果，这条同时修复 compaction 切断配对的问题；(4) 标记为 aborted 或 error 的 assistant 节点不进上下文。另外让 compaction_cut 把切点对齐到配对边界，不把 Tool 消息留在窗口开头。stop_reason 标记用 SessionNode 或 Message 上新增的 `#[serde(default)]` 字段，不要挪用 Message.provider（它表示 provider 名，core lib.rs:63）；节点整条序列化在 blocks JSONB 里，向后兼容。存储不改，只影响 prompt。interrupt 路径的根本修复见 effect sandwich 提案。

风险：合成的错误结果可能让模型重复发出同一个调用，这是可以接受的行为。aborted 标记要兼容旧数据，旧节点一律视为正常。

### 写路径吞吐：PG 版的 group commit 与 deferred commit（热点合并加进度下沉） [high/M]
上游：docs/group-commit.md §1、§4-§7；CHANGELOG Unreleased:160（128 会话、4ms RTT 下从约 5 run/s 到约 440 run/s）、:49（deferred 让事务数降到 1/5.7 到 1/27）、:163；spec.md §4 1843-1873、§4.1 1983-2065

差距：已核实，并补充两个更大的固定负载。(1) 每次 acquire_lease 都对全表执行 reap_stale_runs（quota.rs:122；db.rs:936-947），也就是一条扫描所有租户全部活动 run 的 UPDATE；入院 QPS 和活动 run 数一起增长时，这条语句的开销是两者的乘积。(2) reclaim 循环每 15s 在每个副本上遍历所有租户，对每个租户做一次 count(*)（reclaim.rs:16-20；db.rs:604-608）；10 万租户、50 个副本时约 33 万次查询每秒，跟有没有流量无关。另外：bump_quota_tokens 每个 turn 对热点行 quota_ledger PK(tenant_id, day) 做一次 UPSERT（db.rs:270-276,1550-1562；run.rs:347），bump_quota_run 也写同一行（db.rs:1538-1548；quota.rs:75），同一租户所有副本上的并发 run 在这一行上排队等锁并产生死元组；每个 run 每 15s 一次 touch_run（db.rs:907-922）；流式进度没有持久化通道。

设计：不移植上游的单 mutation line 和 per-key gate。(0) 先修两个固定负载（S）：acquire_lease 只检查自己这个会话是否过期，改成 `UPDATE sessions ... WHERE id=$1 AND tenant_id=$2 AND run_id IS NOT NULL AND updated_at < ...`，全局 reap 只留给 reclaim 循环做并加 LIMIT；reconcile 改为每个 tick 一条 `SELECT tenant_id, count(*) FROM sessions WHERE run_id IS NOT NULL GROUP BY tenant_id`（命中 sessions_tenant_run 部分索引，db.rs:315），只修正有计数的租户，并用 `pg_try_advisory_lock` 保证同一时刻只有一个副本在跑。(a) 每一步把节点 INSERT、runs 检查点和 usage 合进一个事务、一次往返，可以用多语句或 CTE。(b) 每个副本跑一个 Coalescer 后台任务：quota token 增量按租户在内存中累加，每 250-1000ms 用一条多行 `INSERT ... VALUES (...),(...) ON CONFLICT (tenant_id, day) DO UPDATE SET tokens = quota_ledger.tokens + EXCLUDED.tokens` 刷盘，run 结束或 drain 时同步刷一次；心跳合并为每个 tick 一条 `UPDATE runs SET lease_until=now()+ttl WHERE owner_instance=$me AND id = ANY($ids) RETURNING id, abort_requested`；touch_session 同样合并。(c) 流式 partial 和工具输出尾部按上游 deferred 语义处理：约 100ms 推给客户端，最迟 1s 写入 Redis `run_live:{t}:{run}`（HSET 加 TTL），只在步骤提交时把需要的部分转进 PG。如果一定要放在 PG，这类事务用 `SET LOCAL synchronous_commit = off`。(d) 立即提交依赖 PG 自带的 WAL group commit（commit_delay、commit_siblings），应用层不再自己攒批。admit_run 的日配额读取改走 Redis 计数，定期按 PG 对账。指标：每步提交的延迟直方图、Coalescer 批大小和刷盘延迟、租约丢失次数。

风险：Coalescer 两次刷盘之间副本崩溃，会丢最多一个刷盘间隔的 token 计数，可以靠预占再结算兜底。配额判断会略微滞后，存在超额窗口，需要产品上接受或者留出余量。Redis 不可用时进度无处可写，只影响展示，不影响正确性。

### run 与 SSE 连接解耦：断线不取消、任意副本可重连续看 [high/L]
上游：durable.README.md:78（cancelling a wait only cancels that wait, never the work）、:477-527（watch、窗口、ConversationTranscript.resume）；spec.md §9.1-9.3

差距：已核实。SSE 流被 drop 时，CancelOnDrop 直接 cancel 整个 run（http.rs:92-107,639-646），cancel 之后执行面 abort 会杀掉进程（run.rs:230-237）。没有按 run 订阅或重连事件的接口（路由见 http.rs:24-42）。取消的 run 仍然发 RUN_FINISHED success（run.rs:502-508）；心跳丢失导致的 cancel 发生时客户端还连着，会把被中止的 run 看成成功。

设计：断线只摘掉 watcher。显式取消改为 `POST /v1/sessions/{id}/runs/{run_id}/cancel`，写 runs.abort_requested=true；owner 副本通过批量心跳的 RETURNING 或 Redis pubsub 发现后触发 CancelFlag。owner 把 AG-UI 事件写进 Redis Stream `run_ev:{tenant}:{run}`，用 `{tenant}` 做 hash tag，和 cache.rs 现有 key 规则一致（cache.rs:395）。TextDelta 先按 50-100ms 合并再 XADD，否则十万个并发流会产生百万级 XADD/s。watcher 和 owner 在同一副本时直接走进程内 broadcast，Redis Stream 只服务跨副本的重连。新增 `GET /v1/sessions/{id}/runs/{run_id}/events?after=<stream-id>`，任意副本都能处理：先从 PG 发 MESSAGES_SNAPSHOT，再发 run_live 快照里的 partial 和运行中的工具，然后 XREAD BLOCK 跟进。超出窗口时只发快照，不重放，与上游一致。租户可以配置 `detach_cancel_after`（无人观看 N 分钟后取消）来控制成本。被取消的 run 发 RUN_ERROR，code=aborted，不再发 success。

风险：无人观看的 run 会继续花钱，需要 detach 超时和配额兜底。Redis Stream 的内存随在途 run 数线性增长，要用 MAXLEN 和 TTL 限住。AG-UI 客户端需要支持重连协议，属于对外 API 变更。依赖 runs 表和增量提交，重连时的快照才是真的。

### 忙碌会话的持久收件箱：steer 和 follow-up 排队，替代 409 [medium/M]
上游：durable.README.md:529-591（whenBusy steer/followUp/reject、continue）；spec.md §6（2847-3066，pi.inbox、boundary 放置、afterFailedRun）

差距：会话已有 run 时，新的 POST 一律返回 409（run.rs:146-161）。AgentLoop 支持 MessageInbox 和 steering（rupi-agent/src/queue.rs；lib.rs:283,822），但云端从未接入（rupi-server/src 里没有 with_inbox）。没有 continue 型 run，RunAgentInput 必须以 user 消息结尾（run.rs:391-393）。

设计：新增 `run_inputs(tenant_id, id, session_id, mode steer|follow_up, content JSONB, request_id, status queued|placed|withdrawn, created_at)`。POST 时如果会话正忙，按 forwardedProps.whenBusy 处理：steer 或 follow_up 写入队列并返回 202 和 inputId，reject 返回 409。owner 副本在 turn 边界（lib.rs:822 inject_steering 附近）从 PG 拉取 steer 并放入；run 结束时取出第一个 follow-up，在同一事务里创建下一个 run。新增 `forwardedProps.continue=true`，或者独立接口 `POST /v1/sessions/{id}/continue`，对 runs.failure_reason 非空的会话发起不带用户消息的 run。

风险：boundary 语义（何时放置、重置后失效）容易出错，测试矩阵较大。需要对每个会话的队列长度设上限以防滥用。AG-UI 协议本身没有排队语义，属于 rupi 扩展。

## 不借鉴
- 单进程单 mutation line 加 group-commit writer 和 per-key gate（src/session/writer.ts、src/session/keys.ts、docs/group-commit.md §4-§6）：上游的前提是同一份存储同一时刻只由一个进程拥有，没有跨进程锁（durable.README.md:881）。rupi 是多个无状态副本共写 PG，并发控制交给 PG 行锁和 fencing，应用层只合并高频小写。
- Chord 文档模型（op/delta/base、可回退的历史、fork asOf 拷贝，spec §3、§10 的 document.* 写入）：复杂度高。JSONB 列加 append-only 的 messages 已经够用，UI 增量用 AG-UI STATE_DELTA。
- 可插拔 Storage 后端（SQLite、JSONL、IndexedDB、Durable Object、StoreStorage）和 conformance 套件：rupi 只有 PG 这一个权威存储。
- 上游“存储错误让整个 Harness 失败、需要 reopen”的规则（README:914；spec §10.1）：rupi 一个副本同时服务成千上万个会话，PG 错误只应让当前 run 失败或重试，不能连坐整个副本。
- 把高频 deferred 进度写进同一个权威库（上游写在同一个 Storage）：rupi 应把 partial 和工具输出尾部放在 Redis，PG 只收步骤级提交，否则和 PG 写吞吐目标冲突。
- 通用的扩展自定义持久 task 框架（phases、version、migrate、registry handover、missing_task/task_too_old 阻塞，spec §5.1、§5.4 2517-2528,2657-2667）：现阶段属于 YAGNI。而且在控制面执行租户提供的 task 代码，违反“控制面不执行用户代码”的边界（rupi-server/src/lib.rs:368-380 的测试检查）。只需要内建的 run 和 tool 状态机。
- Cloudflare Durable Object 的 onWake/alarm 唤醒协议（README:166-212）：rupi 用 sweeper 加 wake_at 列加 FOR UPDATE SKIP LOCKED 即可，不需要每个会话一个 alarm。
- 完整的结构化并发任务图（ownership 树、failFast/allSettled 等待、completing hold、自底向上 abort，spec §5.5）：云端现在没有 subagent，等需要时只借“子会话归属于工具调用，abort 向下级联”这一条。
- codemode 的确定性重放 journal（src/codemode/codemode.ts，QuickJS 虚拟时钟）：不属于本领域，而且应该在执行面运行，不放在控制面。
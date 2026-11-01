# Track: agent-loop-integrity

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/agent-loop-integrity`，worktree：`.worktree/agent-loop-integrity`

## 目标
让 AgentLoop 在长会话和异常路径上保持 transcript 合法：压实正确且增量、请求参数适配新模型、取消/出错也记 usage、事件完整。

## 负责文件
- crates/rupi-core/src/lib.rs
- crates/rupi-agent/src/lib.rs（除工具门循环外：请求构建、流式与响应处理、压实、事件、PromptBuilder、工具定义合并）
- crates/rupi-agent/src/subagent.rs
- crates/rupi-agent/src/review.rs
- crates/rupi-agent/src/tokens.rs
- crates/rupi-agent/src/context.rs
- crates/rupi-agent/src/discovery.rs
- crates/rupi-agent/src/hooks.rs

不在本 track：工具门循环（逐个调用判 hook/policy/on_ask/plan 并执行的那段，以及 pending_interrupt）归 cloud-run-safety；session.rs 归 local-frontends-fixes；rupi-llm 归 llm-wire-compat。

## 特别说明
- 232 只做 AgentLoop 侧的 max_tokens/temperature 参数；stop_reason 归一与坏 JSON 标记由 llm-wire-compat 做，坏参数调用的合成错误结果由 cloud-run-safety 在工具门里做。
- stop_reason 判断要同时接受 "length"/"max_tokens"/"MAX_TOKENS"（llm-wire-compat 会把它们归一成 "length"，但你的分支里还没有）。
- 233 只做 subagent.rs 的 inherit_from 补齐 tool_allow/tool_deny/hooks/on_ask/tool_execution/discovery；CLI 里先过滤再构造 SubagentTool 归 local-frontends-fixes。
- 251 删除死 API 前先 grep 全仓（含 rupi-server、rupi-cli、rupi-tui、rupi-ext、rupi-benches、tests），有调用方就保留。

## 借鉴上游 Pi 的条目（来自对标 Pi v1.4.0 的分析，按 SaaS 价值排序）

### B1. 云端 transcript 配对止血：guidance 不再写入树、压实切点对齐、多调用 interrupt、发送前修复 [critical/M]
本 track 范围：只做 (b) 发送前 repair_transcript 兜底（放在 rupi-core 纯函数，AgentLoop 构建请求前调用）和 (c) compaction_cut 对齐；(a)(d) 归 cloud-run-safety

上游参考：packages/ai/src/api/transform-messages.ts:179-250（补 No result provided 并挪走夹在中间的 system）；pkg/durable/docs/spec.md §2.1 规则 7-9（325-338）；docs/compaction.md:136；coding-agent.CHANGELOG.md:93（#11227）

差距：已抽查核实。drive 在 resume 分支之前调用 load_remote_guidance（crates/rupi-server/src/run.rs:312，resume 在 :384），把 AGENTS.md 和 commands 作为 Role::System push 进树（run.rs:883、956）。历史因此变成 assistant(tool_use) 后接 system、再接 tool，OpenAI 和 Anthropic 都会返回 400。同样的内容还会拼进 system prompt（run.rs:370-375），每个 run 都多落盘一份。AgentLoop 遇到第一个 Interrupt 就 return（crates/rupi-agent/src/lib.rs:922-932），同一批的其他调用永远拿不到结果。compaction_cut 只按 token 切分（crates/rupi-core/src/lib.rs:284-302），保留窗口的开头可能是一条孤立的 Tool。这些 400 不匹配 is_overflow_error，会话会一直卡住。

设计：(a) load_remote_guidance 和 input.context 只写入 builder.append，不再 tree.push；老会话里重复的 [workspace ...] 节点在 prompt_history 投影时过滤掉。(b) rupi-llm 新增纯函数 repair_tool_pairs，在 to_openai_messages 和 to_anthropic_messages 之前调用：夹在 tool_use 和结果之间的非 Tool 消息挪到结果后面，缺结果的调用合成 is_error 结果，丢弃孤立的结果，并累加 rupi_transcript_repairs_total。(c) compaction_cut 回退到 Assistant 或 User 边界。(d) pending_interrupt 改成 Vec，第一阶段把整批调用都判完，interrupts 表一个调用一行（run.rs:117-125 已经要求 resume 覆盖全部 interrupt）。(e) 云端集成测试改用会校验配对的 mock，并在工作区放一个 AGENTS.md。

### B8. 计费漏洞止血：取消和出错也要记账，压实计费，统一 token 口径，运行中检查预算 [critical/M]
本 track 范围：只做 agent 侧：取消/错误分支也发 Usage（估算值要标记，例如在 AgentEvent::Usage 已有字段之外不加新字段时用 tokens.rs 估算），压实摘要调用改用 complete_streaming 捕获 Usage 事件并以压实名义上报；不要给 ChatResponse 加字段

上游参考：pkg/durable/docs/spec.md §8.6（5171-5203，失败和中止的 attempt 也计入）；pkg/durable/src/harness/usage.ts:6-56；compaction.ts:200-206；packages/ai/src/api/openai-completions.ts:1521-1546（input 等于 prompt 减去 cacheRead 和 cacheWrite，并兼容 prompt_cache_hit_tokens）

差距：已核实。Usage 只在流正常结束时才发出（crates/rupi-agent/src/lib.rs:801-808），取消分支（:753-757）和错误分支都会提前 return。断开 SSE 会触发 CancelOnDrop（crates/rupi-server/src/http.rs:103-107），租户可以在流快结束时断开连接，拿到内容却不被计 token。ChatResponse 没有 usage 字段（crates/rupi-llm/src/lib.rs:148-151），压实时调用 provider.complete 的消耗从不计入。run.rs:339-348 只累加 input+output，而且是在一个有损通道的消费循环里 await PG，这个通道容量 256，满了就丢（run.rs:54-58）。parse_provider_usage 对 OpenAI 和 Gemini 把包含缓存的 prompt 记为 input，对 Anthropic 则不包含（rupi-llm/src/lib.rs:169-209），也不认 DeepSeek 的 prompt_cache_hit_tokens。配额只在准入时检查一次（crates/rupi-server/src/quota.rs:33-40）。

设计：rupi-agent 增加 with_usage_sink，由同步回调直接累加用量，不经过事件通道。取消和错误分支用 tokens.rs 估算用量，并标记 estimated；Anthropic 在 message_start 就先上报一次 input。ChatResponse 增加 usage 字段（serde default），压实的消耗以 kind=compaction 上报。TokenUsage.input 统一定义为不含缓存的输入；cache_read 依次读取 cached_tokens、prompt_cache_hit_tokens、顶层 cached_tokens；cache_hit_pct 和 TokenMeter 的校准都改用 input+cache_read+cache_write。drive 无论从哪个分支退出，都把用量 flush 到幂等的 run_usage(run_key, turn, attempt, kind)，run_key 由服务端生成。每一回合开始前用 Redis INCRBY 检查当日预算，超出就 cancel，并发 RUN_ERROR code=quota_exhausted。按套餐限制 thinking 档位（run.rs:376-382）。

### B9. 稳定缓存前缀：工具顺序确定、历史加缓存断点、cache key 和会话亲和 [critical/M]
本 track 范围：只做 AgentLoop 合并工具定义后按 name 稳定排序

上游参考：packages/ai/src/api/anthropic-messages.ts:1526-1550（在最后一条 user 消息上打断点）；packages/ai/src/api/openai-prompt-cache.ts；pkg/durable/src/tool-search/tool-search.ts:14-15；ai.CHANGELOG.md 0.86.0 #9102 #9629、1.4.0 #10940；pkg/coding-agent/src/core/cache-stats.ts

差距：已核实。ToolRegistry 用 HashMap 存储，definitions() 按哈希顺序输出（crates/rupi-tools/src/lib.rs:91-94、117-121），而云端每个 run 都新建注册表（run.rs:310）。结果是不同 run 之间 tools 和 AvailableTools 的顺序随机，OpenAI 兼容网关（包括 deepseek）的前缀缓存基本命中不了。Anthropic 只在 system 和最后一个工具上打了断点（crates/rupi-llm/src/anthropic.rs:72-94），历史消息上没有；Bedrock 一个断点都没有。云端也从不设置 session_id 和 prompt_cache_key。

设计：阶段 1（S）：definitions() 按 name 排序，AgentLoop 合并工具后再整体排序一次；加单测，断言两次构建的序列化结果逐字节相同；之后新发现的工具一律追加到末尾。阶段 2（M）：ChatRequest 增加 session_id 和 cache_retention，云端填 hash(tenant_id, thread_id)。Anthropic 和 Bedrock 在最后一条消息上打断点，并保留上一轮末尾的断点，总数不超过 4 个。OpenAI 官方端点发 prompt_cache_key，兼容网关加 compat 开关。按 provider 导出缓存命中率指标，标签里不放 tenant。

### B15. 处理 length 截断：参数是坏 JSON 的调用不执行，max_tokens 按模型设置 [high/S]
本 track 范围：只做 AgentLoop 侧：请求里 max_tokens 不再写死 None（交给 provider 默认或按模型目录取值）、temperature 不再写死 0.2（传 None）。length 截断时“不执行坏参数调用”的逻辑归 cloud-run-safety

上游参考：pkg/agent/src/agent-loop.ts:273-279、488；ai.CHANGELOG.md 1.3.0 #10897（argumentsError）、1.4.0 #9512

差距：AgentLoop 调用时 max_tokens 总是 None（crates/rupi-agent/src/lib.rs:707-714），所以 Anthropic、Bedrock、Gemini 都退回到 4096 的输出上限（anthropic.rs:84，已核实）。输出被截断后参数解析失败，结果变成 {}，但调用照样进入执行；在云端，参数为空的 write 还会触发一次审批。主循环只判断 stop_reason 是否为 error。

设计：把 stop_reason 归一为枚举 Stop、Length、ToolUse、Error、Aborted；ToolCall 增加 arguments_error 字段。遇到 Length 且消息里带工具调用时，这些调用不执行，也不送审批，而是各合成一个 is_error 结果，提示模型分块写入，这样调用和结果仍然一一配对。models.json 增加 maxOutput 字段，默认 max_tokens 取 maxOutput 与「上下文窗口减去估算输入再留余量」两者中的较小值，并按套餐设上限。

### B16. 压实改为增量摘要，按投影后的上下文大小触发，并使用结构化模板 [critical/M]
本 track 范围：全部

上游参考：docs/compaction.md:29-49、83、100-138、234-291；pkg/coding-agent/src/core/compaction/compaction.ts:507-579、617-636、700-735；coding-agent.CHANGELOG.md 0.87.1 #9908、1.2.0（序列化时去掉 thinking）

差距：触发条件用的是整条原始分支的 history_tokens()（crates/rupi-agent/src/lib.rs:1228），第一次压实之后这个值一直高于阈值，实际起作用的只有 2×keep 那个启发式。每次摘要都从下标 0 开始重新摘（:1260-1263），总成本是 O(N²)。原始历史超过摘要模型的窗口后，会退化成只取最旧 10 条消息的首行，并且覆盖掉旧摘要（:1289-1314）。提示词只有一句 Be brief，序列化时把 thinking 全文都带上了，工具结果也不截断。

设计：触发量改为投影后的大小：用上一轮 provider 报告的 input+cache_read+cache_write，加上之后新增消息的估算值。摘要区间改为从 summary_through 之后到切点，旧摘要放进 previous-summary，用 update 模板合并；超出预算时分块串行 update。摘要失败时保留旧摘要，再追加 N messages elided 标记，并发出 CompactionEnd 错误事件。新增 serialize_for_summary：加角色标签，去掉 Thinking 和 Image，工具结果截到 2000 字符，整体用 conversation 标签包起来。用户消息段被切开时生成两份摘要。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F226 [P0] 压实切点会拆散 ToolCall/ToolResult，压实后发出的请求带孤儿 tool 消息，provider 返回 400，会话卡死
类型：bug，工作量：S，文件：crates/rupi-core/src/lib.rs, crates/rupi-agent/src/lib.rs, crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs

证据：SessionTree::compaction_cut（core lib.rs:284-302）从尾部累加 token，只按条数切，不看消息角色。工具结果通常是最大的消息，最容易成为越过 keep 阈值的那一条，于是保留窗口的第一条就是 Role::Tool，而与它配对的 assistant ToolCall 被摘要掉了。compress_inner 直接使用这个切点（agent lib.rs:1257-1264）。探针 P10：构造 [user, assistant(ToolCall call_1), tool(ToolResult 400 字)]，force_compress 之后 compaction_cut(50)=Some(2)，prompt roles=[System, Tool]，to_openai_messages 输出的 roles=[system, system, tool]，tool 消息前面没有带 tool_calls 的 assistant 消息。OpenAI 遇到这种请求返回 400（要求 tool 消息必须响应前面的 tool_calls）；Anthropic 映射时把 tool_result 并进 user 消息（anthropic.rs:199-235），也找不到对应的 tool_use。这类错误不匹配 is_overflow_error，所以 run 直接 Err。REPL 回滚本轮，但 summary/summary_through 不回滚，下一轮 prompt 依然以孤儿 Tool 开头，每轮都失败。一个 Tool 消息里有多条结果（并行调用、CJK 内容、MCP 大输出）时很容易触发。

建议修法：compaction_cut 求出切点后再做对齐：如果 hist[cut] 是 Role::Tool，就把切点向前移到对应的 assistant ToolCall 之前；如果这样做导致 cut==0，就向后移到下一条非 Tool 消息。另外在 prompt_history（或发请求前）加一道兜底，丢弃 tool_call_id 找不到配对 ToolCall 的 ToolResult。补一个单测：大工具结果越过 keep 阈值之后，prompt 的第一条非 System 消息不能是 Tool。

### F227 [P0] 第二次及以后的压实从第 0 条开始重发整段前缀，且按全量历史判断触发；超窗后退回只取前 10 条首行的兜底，旧摘要丢失
类型：bug，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-core/src/lib.rs

证据：触发条件用 session.history_tokens()（lib.rs:1228），算的是整棵树当前路径的全量 token，压实之后这个数也不会下降，所以一旦越过阈值，后续每轮都会进入压实，只有 tail<=2*keep 这道门（1236-1254）限一下频率。摘要输入是 chunk=session.history()[..cut]（1260-1263），从第 0 条开始，而不是从 summary_through 之后开始，旧摘要只是额外拼在前面（1270-1275）。摘要失败时的兜底是 chunk.iter().take(10) 每条取首行（1293-1306），不包含旧摘要。探针 P12/P13（window=200, reserve=50, keep=24）：第 13 条时第一次压实，此后每 2 条压实一次，60 条消息共压实 24 次；最后一次摘要输入包含 "message number 000"，长度 2829 字符，随会话增长没有上限。按默认参数（128k 窗口、16k reserve、20k keep）推算：第二次压实的输入大约 130k token，超过窗口后 provider 报错，摘要退化成会话最早 10 条消息的首行，中间的所有上下文都丢了。

建议修法：摘要只喂 history()[pos(summary_through)+1..cut] 这段新增内容，加上旧摘要（对标 Pi 的增量合并）。触发改用实际要发出的窗口来估算：prompt_history 的 token，加上 system 和工具 schema 的估算，最好再用上一条 assistant 的 usage.input 校准。兜底文本必须保留旧摘要，只追加新段的首行。补一个多轮长会话测试：每次压实时，摘要请求的输入都小于 context_window。

### F242 [P1] 压实触发与真实用量脱节：没算 system 和工具 schema，TokenMeter 校准系数恒为 1，窗口表与 models.json 是两套数据
类型：incomplete，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-agent/src/tokens.rs, crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-llm/models.json

证据：used 只算 session.history_tokens()（lib.rs:1228），不包括 system（记忆、AGENTS.md、skill 索引）和工具 schema。provider 回报的 usage_by_node 已经入树（815-817），但没被使用。所有调用方都把 input_tokens 同时当作 estimated_input 传给 note_usage_cache（main.rs:2097-2103、2631-2637；app.rs:1719-1725），ratio 恒为 1，tokens.rs:2 和 UPSTREAM.md:24 所说的“provider usage 校准比例”并不存在。context_window_for 用子串硬编码（tokens.rs:27-94），没读 models.json：o3/o4-mini 在目录里是 200000，函数返回 128000，扩展注册的 provider 的 context 也被忽略。

建议修法：用最近一条 assistant 节点的 usage.input 加上之后新增消息的估算，作为 used（对标 Pi contextTokens）。没有 usage 时用估算，并加上 system 和 tools 的估算。窗口优先查 load_models() 的 context，查不到再用 rates_for。TokenMeter 的 estimated_input 改传本轮请求的估算值。

### F236 [P1] /rewind 或 /goto 到摘要覆盖点之前，prompt 仍带着描述已废弃后续的旧摘要；keep_recent_tokens 还被当成消息条数传入
类型：bug，工作量：S，文件：crates/rupi-core/src/lib.rs, crates/rupi-agent/src/lib.rs

证据：branch_from 会在摘要越界时清掉 summary（core lib.rs:307-321），rewind_to 和 goto_node（324-354）不会。prompt_history 找不到 summary_through 时退回 all.len().saturating_sub(keep_last)（266-271），而 agent 传入的是 keep_recent_tokens（lib.rs:706，默认 20000），结果从 0 开始取。探针 P14：摘要覆盖 msg0..msg3 后 rewind 到 msg1，prompt 变成 ["[Conversation summary so far] SUMMARY covering msg0..msg3", "msg 0", "msg 1"]，模型同时看到描述“未来”的摘要和全量历史。

建议修法：rewind_to 和 goto_node 之后，如果 summary_through 不在新路径上，就清空 summary/summary_through（与 branch_from 同一规则，抽一个 fn reconcile_summary 共用）。prompt_history 去掉 keep_last 这个回退参数，没有覆盖点就返回全量。

### F237 [P1] resume 后摘要覆盖点被重置为首条消息，压实效果丢失，与文档“resume 时预热窗口”不符
类型：bug，工作量：M，文件：crates/rupi-memory/src/session_io.rs, crates/rupi-core/src/lib.rs, docs/UPSTREAM.md

证据：sessions 表只存 summary，不存 summary_through（persist_turn 在 memory lib.rs:1380-1385 只更新 summary）。tree_from_records 恢复时把 summary_through 设成第一个节点（session_io.rs:877-882）。探针 P15：5 条历史加摘要，resume 之后 prompt_history 长度为 5，即摘要加上第 1 条之后的全部消息。长会话 --resume 后第一轮就会发出接近全量的历史，大概率先触发溢出恢复。UPSTREAM.md:27 写的是“摘要……只存 sessions.summary 一列，resume 时预热窗口”。

建议修法：sessions 表加 summary_through 列（迁移 user_version+1），persist_turn 和 set_summary 一并写入，tree_from_records 读回。旧数据缺这一列时，保守地重新压实，或者只保留最后 keep 窗口。

### F239 [P1] 错误路径不发 TurnEnd/RunEnd；AgentEvent::Error 和 StopReason::ProviderError 从未构造；溢出重试会重复发 TurnStart
类型：incomplete，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-core/src/lib.rs, crates/rupi-server/src/agui.rs, crates/rupi-tui/src/view.rs

证据：provider 出错时，run_turns 在 lib.rs:779 return Err(e)，error stop_reason 也直接 bail（784-786），两条路径都不发 TurnEnd/RunEnd，扩展收不到 run_end/session_end。探针 P4 的事件序列只有 ["TurnStart"]。grep 全仓：AgentEvent::Error 只出现在 TUI view.rs:225 和 agui.rs:322 的 match 分支里，没有任何地方构造它；StopReason::ProviderError（core lib.rs:659）也从未构造，序列化形状是 {"provider_error":"x"}（P7），与其他字符串变体不一致。溢出恢复用 turn -= 1; continue（776-777），同一个 turn 号会发两次 TurnStart，中间没有 TurnEnd，AG-UI 会出现两个 STEP_STARTED turn-1。

建议修法：在 run_turns 外层统一收尾：Err 时先发 Error{message}，再发 TurnEnd{ProviderError} 和 RunEnd{ProviderError}，然后返回 Err。溢出重试前补发 TurnEnd，或者不重复发 TurnStart。如果不打算使用 ProviderError，就删掉这个变体。

### F247 [P1] 主循环写死 temperature 0.2，OpenAI 推理模型（o3/o4-mini/gpt-5，目录内置）会拒绝这个参数
类型：bug，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-llm/src/lib.rs, crates/rupi-llm/models.json

证据：ChatRequest 里 temperature: Some(0.2)（lib.rs:712），openai_body 无条件插入 temperature（rupi-llm lib.rs:385）。models.json 内置了 o3 和 o4-mini。按 OpenAI 的文档，o 系列和 gpt-5 只接受默认 temperature（1），其他值返回 400。这一点没有对真实 API 实测，只核对了代码路径；thinking 档位走 reasoning_effort 的那条路径同样受影响。

建议修法：temperature 默认 None（交给 provider 默认值），由 settings 显式配置；openai_body 对推理模型（o1/o3/o4/gpt-5 前缀，或者设置了 reasoning_effort 时）省略 temperature。

### F253 [P2] 最后一轮工具执行后仍会注入转向消息，但随即以 MaxTurns 退出，这条消息永远得不到回答；review 和记忆 sync 只在 Done 出口运行
类型：bug，工作量：S，文件：crates/rupi-agent/src/lib.rs

证据：工具结果入树后，无条件调用 inject_steering（lib.rs:1104-1105），然后 while 条件 turn < max_turns 不成立，直接发 RunEnd{MaxTurns}（1111-1120）。用户的转向消息写进了会话，也广播了 SteeringInjected，模型却没有机会回应。mem.sync_all 和 run_review 只在无工具调用的 Done 分支执行（837-839），MaxTurns 和 Aborted 的回合不会同步外部记忆，也不会复盘。探针 P5：max_turns=1 时，执行完工具直接 RunEnd，模型看不到工具结果。

建议修法：turn==max_turns 时不注入转向（把消息留在信箱给下一次 prompt）；或者转向存在时允许额外跑一轮。MaxTurns 出口同样执行 sync_all 和 review（带上最后一条 assistant 文本）。

### F245 [P1] 启发式 reviewer 默认开启且误报严重，只要用了两种工具就产生垃圾 skill 草稿
类型：bug，工作量：S，文件：crates/rupi-agent/src/review.rs, crates/rupi-cli/src/main.rs

证据：review_enabled 写成 !no_review || review（main.rs:225-227），默认开启。memory_entry 用子串匹配 "my "、"i am"（review.rs:76-82），"economy " 里也包含 "my "。failure_entry 匹配 "incorrect"、"错了"（99-109）。skill_draft 只要求 distinct>=2（129-131）。探针 P11："fix my code please"、"the economy module is slow"、"I am getting an error in main.rs" 都产生 memory_ops=1；"the test prints incorrect output, fix it" 被记成一条 user correction；一个普通的问答（glob+read）产生了 skill auto-what-readme，步骤是 ["use tool `glob`","use tool `read`"]。加上 --review-apply 时，这些内容会直接写进 MEMORY.md、failures.md 和 skills/。

建议修法：memory 改为按词边界匹配，并且只在显式“记住/remember”或第一人称偏好句式时触发；failure 要求句首否定或者上一轮有工具失败；skill 草稿要求有工具失败后成功，或者 ≥3 个不同工具且本轮成功，名称也不能只从问题句提取。或者默认关闭启发式 review，只在 --review 时开启。

### F146 [P1] 启发式 review 和 --review-apply 把整句用户输入写进记忆，并生成没有价值的 auto-* skill
类型：incomplete，工作量：M，文件：crates/rupi-agent/src/review.rs, crates/rupi-cli/src/main.rs

证据：review.rs:67-89 的 memory_entry 只要用户输入里出现 remember、`my `、`i am`、我的 之类的词，就把整句输入（最多 300 字符）当作记忆。skill_draft(review.rs:122-153) 只要一轮用了 2 个以上不同工具就起草 skill，名字取 prompt 前 4 个词，步骤是 `use tool X`。实测：`rupi --model deepseek-v4-flash --review-apply run "Remember: this project always uses tabs. Read README.md and tell me its title."` 之后，MEMORY.md 多了一行原文 `Remember: this project always uses tabs. Read README.md and tell me its title.`，并生成 skills/auto-remember-this-project-always/SKILL.md，步骤是 `1. use tool memory 2. use tool read`。这个 skill 会进入后续每个会话的 AvailableSkills。普通的 read/edit/bash 任务也会给出 `skill=auto-read-notes-txt-with` 的建议。`my ` 这个子串会命中 "fix my code" 这类常见请求。

建议修法：memory_entry 只提取 remember 之后的子句，或干脆只依赖 LLM reviewer。`my `/`i am` 这类宽泛触发词去掉，或要求句式明确。skill 草稿至少要求 3 个以上步骤且有参数差异，名字由模型生成。--review-apply 默认只写草稿目录（如 skills/.drafts），用户确认后再启用。

### F244 [P1] 子 agent 上下文贫瘠：没有 AGENTS.md 和 --append-system-prompt，没有扩展事件，压实与发现配置用默认值
类型：incomplete，工作量：S，文件：crates/rupi-agent/src/subagent.rs

证据：run_subagent 新建 AgentLoop::new(self.max_turns)（subagent.rs:250），builder 是默认的一行 base（lib.rs:244-246）：没有 context_dirs 就不读 AGENTS.md，也没有 append 和 SYSTEM.md 替换。extensions 传 &[]（subagent.rs:270），子 agent 的工具调用扩展完全看不到。reserve/keep/window/overrides/compaction_enabled/discovery/tool_execution 都是默认值。事件回调是 noop（260），父 UI 看不到子 agent 的进度。

建议修法：inherit_from 直接 clone 父 AgentLoop，只改 max_turns、清空 inbox 和 reviewer，不再逐字段复制；SubagentTool 持有父 extensions。子事件可以加前缀转发（可选的 on_event 透传）。

### F233 [P0] --subagents 子 agent 绕过 --tools/--exclude-tools、hook 和 tool 可见性过滤
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/subagent.rs

证据：run_once、run_chat、run_tui 三处都是先 SubagentTool::new(provider, Arc::new(tools.clone()), ...)，再调用 apply_tool_filter(&mut tools, &rt)（main.rs:2000-2013、2290-2303、2786-2799），所以子注册表是过滤之前的完整副本。子循环 AgentLoop::new(self.max_turns)（subagent.rs:250-258）只继承 policy/approver/plan_mode/thinking（inherit_from 在 171-177），不继承 tool_allow/tool_deny/hooks/on_ask/discovery。效果：rupi --exclude-tools bash --subagents 时父模型看不到 bash，但可以调用 subagent(goal)，子 agent 依然有 bash 可用。

建议修法：在 apply_tool_filter 之后再构造 SubagentTool（先过滤，再克隆，再注册 subagent）。inherit_from 补充复制 tool_allow、tool_deny、hooks、on_ask、tool_execution、discovery 配置。补一个单测：父循环 exclude bash 时，子回执里出现 "bash!" 或 "unknown tool"。

### F246 [P1] 系统提示没有 cwd、日期、平台和工具使用指引，base 只有一句话
类型：missing-feature，工作量：S，文件：crates/rupi-agent/src/lib.rs

证据：默认 base 是 "You are rupi, a minimal coding agent (Rust port of Pi). Use tools to act. Be concise."（lib.rs:245）。grep 全仓没有任何 working directory 或 current date 注入，PromptBuilder（75-113）也不加环境段。模型不知道沙箱根目录（读写都被约束在启动 cwd），不知道今天日期和 OS（macOS 与 Linux 的命令差异），也没有 edit 与 write 的取舍、先读再改之类的指引。Pi 上游的系统提示包含这些内容。

建议修法：PromptBuilder 增加 environment 段：cwd（沙箱根）、日期、OS/arch/shell、git 仓库与否；内置一份更完整的默认 base（工具选择、先读再编辑、批量编辑、不要臆造路径）。SYSTEM.md 仍可整体替换。

### F121 [P1] 成本和窗口表（rupi-agent/tokens.rs）过时且不计缓存价
类型：bug，工作量：S，文件：crates/rupi-agent/src/tokens.rs

证据：claude-opus 按 $15/$75 计价（tokens.rs:29-34），而 Opus 4.6+ 是 $5/$25，Opus 5.5 是 $4/$20；claude-haiku 按 $0.80/$4 计（tokens.rs:41-46），Haiku 4.5 实际是 $1/$5。所有 claude 窗口都按 200k（4.6+ 是 1M）。gpt-5 和 gpt-4.1 共用 1,047,576 的窗口（tokens.rs:71-76），但 gpt-5 只有约 400k，阈值压实在溢出前根本不会触发，只能靠溢出恢复兜底。ModelRates::cost 完全没有缓存读写价（tokens.rs:20-23）。

建议修法：价格迁到 models.json（见上一条），补上 cache_read 和 cache_write 单价；启发式只作为兜底，并按当前价目更新。

### F249 [P2] run_turns 约 490 行，12 处事件广播重复；5 个压实包装和 3 个 10 参数入口可以收敛
类型：simplify，工作量：M，文件：crates/rupi-agent/src/lib.rs

证据：run_turns 在 lib.rs:635-1121。非测试代码里有 12 处 `on_event(ev.clone()); for e in extensions { e.on_event(&ev).await?; }` 形式的块（106、827、844、855、905、946、955、1062、1072、1114、1136、1144 行附近）。扩展返回 Err 时，第三阶段（1062）的 `?` 会在工具已执行、结果还没入树时中断 run。压实有 maybe_compress、maybe_compress_with_event、force_compress、force_compress_with_event、force_compress_with_prompt 五个包装（1151-1210），其中两个只在测试和 bench 里用。run、run_with_user、continue_session 各带 10 个参数，全都 #[allow(clippy::too_many_arguments)]。

建议修法：加 async fn broadcast(&self, ev, on_event, exts)（扩展错误只 warn），预计省约 60 行。第一阶段的门抽成 fn gate(&self, ..) -> Gate，第三阶段抽成 fn record_results，run_turns 可以降到 200 行以内。压实只保留 compress(provider, session, mem, CompressMode{Auto|Force{prompt}}, on_event)，约省 40 行。tools/mem/frozen/skills/extensions 打包成 TurnCtx<'a>，三个入口和各前端调用点的参数各减 5 个。

### F251 [P2] 死 API 和死依赖：run_subagents（只有测试用，且有 bug）、ToolExtension、RulePolicy::plan_mode/readonly/ChainPolicy、重复的 is_mutating、peek_*、SessionNode.summary，rupi-mcp/thiserror 依赖
类型：simplify，工作量：S，文件：crates/rupi-agent/src/subagent.rs, crates/rupi-agent/src/lib.rs, crates/rupi-agent/src/policy.rs, crates/rupi-agent/src/queue.rs, crates/rupi-core/src/lib.rs, crates/rupi-agent/Cargo.toml, crates/rupi-core/Cargo.toml

证据：grep 全仓结果如下。run_subagents（subagent.rs:36-93）只有测试使用，而且它 clone 的父 AgentLoop 带着同一个 inbox，子任务会抢走父的转向消息（探针 P6：父 inbox 预置 1 条，扇出后 pending=0）；tool_calls 统计的是新增消息条数（83 行，P8 中没调工具却得到 2）。ToolExtension（lib.rs:1479-1504）没有调用方。RulePolicy::plan_mode/readonly 和 ChainPolicy 生产上不用，plan 由 AgentLoop.plan_mode 另行拦截，is_mutating 在 lib.rs:527 和 policy.rs:78 各有一份。MessageInbox 的 peek_steering、peek_follow_up、take_steering 只在测试里用。SessionNode.summary（core lib.rs:173）从不写入。history_chars 只有 bench 用。rupi-agent 的 Cargo.toml 依赖了 rupi-mcp，但 src 里没有任何 rupi_mcp 引用；rupi-core 依赖了 thiserror，src 里同样没用。

建议修法：删掉 run_subagents，或者把它修好（清空 inbox/reviewer，按 ToolCall 计数）；删除 ToolExtension、peek_*、SessionNode.summary（serde 用 default 兼容旧数据）、ChainPolicy、RulePolicy::readonly 和 plan_mode（plan 统一由 AgentLoop 处理）；去掉两个未用依赖。预计省约 200 行，编译也会更快。

### F255 [P2] 每轮同步重读 AGENTS.md 祖先链，并多次全量克隆和估算历史
类型：perf，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-agent/src/context.rs, crates/rupi-core/src/lib.rs

证据：PromptBuilder::build 每个 turn 都调用 load_context_files（lib.rs:93-96）。从 cwd 一直走到根，每层对 5 个候选文件做 symlink_metadata、read 和 canonicalize（context.rs:28-82），全是阻塞 fs，跑在 async 里。每轮的 prompt_history 把全部消息克隆两遍（core lib.rs:262 先 cloned 一次，再 extend），maybe_compress 里的 history_tokens 对全部消息做 full_text 字符串拼接后再估算（core lib.rs:248-250；agent 1228、1244-1249），一轮至少调用两次。一千条消息的长会话里，每轮要做 O(总字符数) 的分配。

建议修法：上下文文件按 (path, mtime) 缓存，mtime 变化才重读。SessionTree 维护一个增量的 token 估算缓存（push 时累加，路径变化时重算）。prompt_history 只克隆需要的切片。

### F232 [P0] 主循环从不设置 max_tokens（Anthropic/Gemini 输出被限在 4096），也不处理 max_tokens/length 停止原因，截断的工具调用参数被解析成 {} 照样执行
类型：bug，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/lib.rs

证据：ChatRequest 里写死 max_tokens: None（lib.rs:711），配置和 CLI 里都没有对应项（grep rupi-config/cli/tui 无 max_tokens）。Anthropic 用 req.max_tokens.unwrap_or(4096)（anthropic.rs:84），Gemini 也是 maxOutputTokens 4096（gemini.rs:68-71）。主循环只把 stop_reason=="error" 当失败（lib.rs:784），"max_tokens"/"length"/"MAX_TOKENS" 都按正常结束处理。流式累积器在参数 JSON 不完整时 serde_json::from_str(...).unwrap_or(json!({}))（anthropic.rs:589；rupi-llm lib.rs:742），把截断的 ToolCall 当作空参数调用交给执行器。结果是：在默认的 Claude 路径上，一次 write 超过约 4096 token（约 16KB 代码）就必然失败，模型拿到参数错误后往往原样重试，直到 max_turns 用完。

建议修法：给 AgentLoop 加 max_output_tokens 配置，默认取模型目录值或一个较大的值（例如 Claude 用 16k–32k），接入 settings/--max-tokens。stop_reason 是 max_tokens/length 并且含有参数不完整的 ToolCall 时，不执行该调用，改为回一条 tool error（"output truncated, split the write"），或者调大上限重试一次。累积器对参数不完整的 JSON 打标记，不要静默变成 {}。

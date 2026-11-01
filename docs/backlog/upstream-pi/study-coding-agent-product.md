# coding-agent-product

## 上游
0.85 到 1.4.0 加 Unreleased 之间，上游 coding-agent 和云 SaaS 有关的改动集中在会话、压实和协议三块。(1) 会话格式仍是 v3 JSONL 树，但条目更多了：`context_edit`（只追加，从后续模型上下文中省略或替换某条，原始历史、计费、导出不动，0.87.0）、`usage`（不进上下文的用量，比如 cache_warm）、带 `systemMessage` 检查点和 `usage` 的 `compaction`，另外系统提示和工具集改成按 section 打补丁的 system message 存进会话（0.86.0 "transcript-backed"），resume 和换分支都能还原，前缀缓存也不会失效（docs/session-format.md:80-147，pkg/coding-agent/src/core/session-manager.ts:41-197）。新会话一开始就写 `model_change`/`thinking_level_change`（sdk.ts:442-453）；SessionManager 改为上下文的唯一权威来源（0.87.0 Breaking），leaf 取最后一条条目（session-manager.ts:1103-1111 `_buildIndex`），可以 `inMemory()` 恢复外部存储的条目（0.85.0）。(2) 压实：切点不能落在 toolResult 上（docs/compaction.md:136）；用户消息段被切开时生成两段摘要再合并（compaction.md:100-130）；重复压实从上一次的 firstKept 边界接着摘，不重摘整段历史；摘要用结构化模板（Goal/Constraints/Progress/Decisions/Next Steps/Critical Context）；序列化时去掉 thinking（1.2.0），工具结果截到 2000 字符（compaction.md:291）；摘要请求设 `cacheRetention:"none"` 并带重试；输出上限改用模型自己的 output limit（1.4.0）；compaction.modelOverrides 支持按模型覆盖（0.86.0）。(3) 恢复：上下文溢出或 `length` 截断时只做一次压实再重试（compaction.md:39），失败的那次尝试用 context_edit 省略；截断 tool call 后孤儿 tool result 让后续每次请求都被拒的问题修了（coding-agent.CHANGELOG.md:93，1.4.0 #11227）；参数是坏 JSON 的工具调用不再执行（CHANGELOG.md:149，1.3.0 #10897）。(4) 协议：事件加了 `agent_settled{aborted}`、`compaction_start{reason}`、`compaction_end{result,willRetry,aborted}`、`tool_execution_end.durationMs`、auto_retry 和 summarization_retry 事件（docs/json.md:44-180）；RPC 的 prompt/steer/follow_up 响应带 `disposition`（0.99.0）；重试有总延迟上限 `retry.maxAgentDelayMs`，`Retry-After` 也有上限 `retry.provider.maxRetryDelayMs`（settings.md:128-136）。(5) 成本：prompt cache warming（0.86.0，cache-warmer.ts:14-30，预期节省 ≥$0.05 才发，idle 继续概率常数 0.15）；Anthropic 在最后一条 user 消息上加缓存断点（ai/src/api/anthropic-messages.ts:1526-1550）；各网关按 sessionId 带会话亲和头（0.86.0 多条 inherited fix）。(6) MCP 在 0.99 变成内置能力：工具名统一为 `mcp__server__tool`，冲突时加 hash 后缀（docs/mcp.md:77）；有 direct/deferred/codemode/hidden 四种暴露方式，toolExposure 支持通配（mcp.md:191 起）；文本结果超过 20KB 截中间；OAuth 加固（RFC 9207 iss 校验，拒绝非 http(s) 的 metadata URL）；mcp_servers 这一段 system 只在变化时追加，不影响缓存。(7) 安全模型：project trust 只决定 .pi 资源是否加载，AGENTS.md 永远加载，但一律当不可信输入（docs/security.md:25-57）；真正的隔离要靠容器、VM 或远端 sandbox（docs/containerization.md）。(8) exec 输出上限：`pi.exec()` 默认 256MiB 和运行时字符串上限取小，超出后终止并标 `truncated`（CHANGELOG.md:8、27，#11360）。其余大部分条目属于 TUI、终端、安装、OAuth 订阅登录、codemode、分类器这类本机或其他领域内容。

## rupi 现状
会话：内存里是 `SessionTree`（crates/rupi-core/src/lib.rs:177-190），每个会话只有一份 `summary`/`summary_through`，压实不是树上的条目。云端持久化在 crates/rupi-server/src/db.rs:1065-1119（`persist_tree`）：每次 run 结束（run.rs:724）和每次 interrupt（run.rs:451）都 `DELETE FROM messages` 再把所有节点重新 INSERT 一遍；`load_tree`（db.rs:1121-1170）按 created_at 排序后把全部节点塞进 current_path（1167 行），不存 leaf，也不看 parent。finish 还把整棵树序列化成一个 Redis 值写回（run.rs:725-730）。压实逻辑在 crates/rupi-agent/src/lib.rs:1212-1324：触发条件用原始历史的 `history_tokens()`（1228）加上一个"新增不足 2×keep 就跳过"的启发式（1236-1254）；摘要输入是 `history()[..cut]`，每次都从下标 0 开始（1260-1263）；提示词只有一句 "Summarize ... Be brief."（1279-1281）；调用失败就退回"取最旧 10 条的首行"，并覆盖旧摘要（1289-1314）。切点 `compaction_cut`（rupi-core/src/lib.rs:284-302）只按 token 从尾部累加，不管 tool 配对。prompt 组装（lib.rs:696-714）把每轮的 `<Recalled>` 拼进 system，但云端唯一的外部记忆 PostgresMemory（rupi-server/src/memory.rs:73-200）没有实现 prefetch，走默认空串（rupi-memory/src/lib.rs:608），所以云端 recalled 恒为空。Anthropic 只在 system 和最后一个工具上打 cache_control（rupi-llm/src/anthropic.rs:74-93），默认 max_tokens 4096（84 行）；Bedrock 复用 Anthropic 消息映射但完全没有 cache_control（bedrock.rs:58-75）；OpenAI 不设 max_tokens（lib.rs:374-398）。云端 run 在 crates/rupi-server/src/run.rs：provider 只按租户解析（313，resolve_byok 790-847），session.model 不参与，session.thinking_level 生效（376-382）；每次 run（包括 resume run）都在 `resume_and_continue` 之前把 AGENTS.md 和 commands/*.md 当 System 节点 push 进树并落盘（312、883、956），同时又拼进 builder.append（370-375）；用量只在收到 `AgentEvent::Usage` 时记账，且只累加 input+output（339-348）。云端没有挂 reviewer，也没有 subagent 工具。SDK 在 crates/rupi-agent/src/session.rs，CLI 的 RPC 在 crates/rupi-cli/src/rpc.rs，云端没走这一层。JSONL 进出口在 crates/rupi-memory/src/session_io.rs：导入只认 message/custom_message/compaction/session_info（307-437），导出的 compaction 条目放在文件末尾，parentId 是 through（67-92）。AG-UI 只把压实映射成 STEP_STARTED/FINISHED{summarized,kept}（agui.rs:290-300）。MCP 只在本机（crates/rupi-mcp），工具名是 `{server}_{tool}`（lib.rs:1055、1395）；云端工具集只有 read/write/edit/glob/grep/bash/think（rupi-server/src/tools.rs:13-43），rupi-server 里 grep mcp 零命中。云端 glob 走 ignore_walker(standard_filters(true))（rupi-tools/src/lib.rs:1176-1180），隐藏目录被过滤；read 截到 MAX_TOOL_OUTPUT=12000 字符（lib.rs:1398、572），bash 不截断。审查中另发现一个不属于本领域的问题，供 memory 领域跟进：frozen 记忆按会话范围查询（db.rs:1348-1355，session_id IS NULL OR session_id=$2），却缓存在租户级 key `mem_frozen_key(tenant)` 下（run.rs:275-299，TTL 300 秒），同租户内一个会话的 session 级记忆会被另一个会话读到。

## 提案
### 压实切点不得把 tool result 和它的 tool call 分开（含切开用户消息段时的双摘要） [critical/S]
上游：docs/compaction.md:100-138（Split user-message spans、Cut Point Rules：Never cut at tool results）；CHANGELOG 0.87.1 #9908（split-turn 摘要）；compaction.ts:929-1078 TURN_PREFIX_SUMMARIZATION_PROMPT

差距：crates/rupi-core/src/lib.rs:284-302 的 `compaction_cut` 只按 token 从尾部累加条数，从不检查 `history[cut]` 是不是 Role::Tool（已核对源码）。prompt_history（lib.rs:261-280）随后产出 [System 摘要, Tool(tool_result), ...]。Anthropic 映射（anthropic.rs:147-150、200-240）把 System 和 Tool 都转成 user，合并成一条 user 消息，里面先是摘要文本再是 tool_result，前面没有对应 tool_use；OpenAI 映射（rupi-llm/src/lib.rs:272、303-310）产出 system 后紧跟 role=tool。两家都返回 400。编码会话里大块 read/bash 结果最容易成为越过 keep 阈值的那一条，所以切点落在 Tool 上很常见。更正原提案一处：这个 400 不匹配 `is_overflow_error`（rupi-llm/src/overflow.rs:69），lib.rs:766-778 的溢出恢复不会触发，直接 Err。summary_through 已落盘（db.rs:1105-1113），而下一次压实又被"新增不足 2×keep 就跳过"（lib.rs:1236-1254）挡住，失败的 run 只追加一条 user 消息，所以会话会长期卡死。

设计：rupi-core `SessionTree::compaction_cut`：算出初始 cut 后，如果 `hist[cut].role == Role::Tool`，向前回退到产生这些 tool call 的 Assistant，让 Assistant 和它的全部 Tool 结果一起保留；如果回退后 kept 覆盖全部历史，则向后推进到下一个非 Tool 边界。优先停在 User 边界；按上游规则，只有单个用户消息段超过 keep_recent_tokens 时才允许切在 Assistant 上。切开用户消息段时（cut 落在某个 User 之后的 Assistant 上），compress_inner（rupi-agent/src/lib.rs:1256-1314）把 `history()[span_start..cut]` 再分成 history 和 turn_prefix 两段，分别摘要后合并（上游 TURN_PREFIX prompt）。已有会话的 summary_through 可能已落在 Tool 中间，由下一条提案的发送前修复（repair_tool_pairs）兜底，不需要改数据。补单测：tool 结果过大、并行多 tool result、Tool 后带图片块、切点恰好在 Assistant{tool_call} 和 Tool 之间等形状。

风险：回退会让保留部分略大于 keep_recent_tokens，极端情况下单个巨大 tool 结果本身就超窗，需要配合 bash 输出上限那一条一起解决。

### tool call/result 配对不变量：interrupt 的并发批次、resume 时插入的 guidance、取消和截断在任何路径上都不能把孤立或错位的调用发给 provider [critical/M]
上游：CHANGELOG 1.4.0 #11227（截断的 tool call 被省略后 tool result 仍保留，后续每次请求都被拒，coding-agent.CHANGELOG.md:93）；1.3.0 #10897（坏 JSON 参数不执行）；0.87.0 context_edit 恢复省略（agent-session.ts:1260-1290 `_omitRecoveryAttempt` 同时省略 assistant 和它的 toolResult）；durable/src/harness/tool.ts 中 "No result provided" 合成结果

差距：(1) rupi-agent/src/lib.rs:922-933：第一阶段遇到 `AskAction::Interrupt` 就直接 `return Ok(StopReason::Aborted)`，同一条 assistant 里排在前面的已放行调用（已进 pending 但未执行）和排在后面的调用都没有结果。`pending_interrupt` 是单个 Option（lib.rs:237），resume（run.rs:528-684）只给这一个 interrupt 补结果，其余调用一直孤立，`continue_session` 的下一次请求会被 OpenAI 和 Anthropic 拒绝。(2) 审查新增、比 (1) 更常见：drive() 在 resume 分支之前调用 load_remote_guidance（run.rs:312），把 `[workspace AGENTS.md]` 和 `[command …]` System 节点 push 到 Assistant{tool_use} 之后，resume_and_continue 再 push Tool 结果。scratch 复现（/tmp/rupi-review-resume，用真实的 to_anthropic_messages/to_openai_messages）：Anthropic 得到 user[text "[workspace AGENTS.md]…", tool_result]，tool_result 不在首位；OpenAI 得到 assistant(tool_calls) → system → tool。两家都要求 tool 结果紧跟 tool_use（Anthropic 还要求 tool_result 排在该 user 消息内容最前），都会 400。也就是说，只要工作区有 AGENTS.md，哪怕只批准一个 write，resume 也会失败。云端测试用 MockProvider（cloud_first_cut.rs:663、cloud_hardening.rs:620-668），不校验消息形状，所以没被发现。(3) provider 层没有任何配对检查（rupi-llm/src/lib.rs:267-333、anthropic.rs:128-240 原样转发）。

设计：(a) rupi-agent：Interrupt 不再中途 return。第一阶段把整批调用的门都过完，所有 Ask→Interrupt 的调用都记成 pending（`pending_interrupt` 改成 Vec，interrupts 表本来就一调用一行，preflight 已要求 resume 覆盖全部 interrupt，run.rs:117-126）。已放行的调用照常执行，结果作为一条 Tool 消息立即入树；需要审批的调用不写占位，resume 时按原始调用顺序追加剩余结果。两条相邻的 Tool 消息对 Anthropic（同角色合并）和 OpenAI（多条 role=tool）都合法，且不修改已有节点，和只追加 entries 那一条兼容。最小改动版：Interrupt 时给同批其余调用合成 `ToolResult{is_error:true, content:"not executed: batch interrupted for approval"}`，让模型重发。(b) guidance 不再写进树（见缓存那一条），至少不能在 resume run 里插到 tool_use 和 tool_result 之间；修复前可以让 load_remote_guidance 在树尾是带未完成调用的 Assistant 时跳过 tree.push。(c) rupi-llm 增加纯函数 `repair_tool_pairs(&[Message]) -> Vec<Message>`，在各 provider 转换前调用：把夹在 Assistant{tool_calls} 和其结果之间的非 Tool 消息挪到结果之后，没有结果的 call 合成错误结果，没有 call 的 result 丢弃；计数打 tracing 日志和指标 `rupi_transcript_repairs_total`。只修发出去的请求，不改树。(d) run.rs `resume_and_continue` 按 tool_call_id 精确对齐。(e) 云端集成测试换成会校验配对和顺序的 mock。

风险：多 interrupt 需要 AG-UI 前端一次处理多条审批（RUN_FINISHED.outcome.interrupts 本身是数组，协议兼容）。先执行已放行调用、后审批其余调用，会改变"整批一起批"的语义，需要在文档里说明。合成结果会让模型重试被中断的动作，要在文案里写清楚。repair 如果掩盖了真实 bug，必须靠指标告警发现。

### 压实改为增量摘要，触发条件改用投影后的 prompt 大小（修复 O(N²) 摘要成本和退化） [critical/M]
上游：docs/compaction.md:29-49、83（重复压实从上一个 firstKeptEntryId 开始，tokensBefore 按投影重算）；compaction.ts:700-735（previousSummary 走 UPDATE_SUMMARIZATION_PROMPT）；CHANGELOG 0.86.0 #9740（轮中阈值压实）、1.4.0 #9512（摘要输出上限）

差距：已核对源码。rupi-agent/src/lib.rs:1228：触发条件用 `session.history_tokens()`，即整条原始分支，不扣除已被摘要的部分；第一次压实之后这个值永远高于阈值，真正起作用的只剩 1236-1254 行的"新增尾部 > 2×keep"启发式。1260-1263 行 `history()[..cut]` 每次从下标 0 开始，把已经摘要过的历史和旧摘要一起再送一遍，单次摘要输入随会话长度线性增长，压实次数也随长度线性增长，总成本是 O(N²)。原始历史一旦超过摘要模型窗口，`provider.complete` 返回溢出错误，1293-1314 行退回"最旧 10 条消息的首行"（`chunk.iter().take(10)`），并通过 set_summary 覆盖掉之前的好摘要，之后每次压实都产出这种垃圾摘要。原提案的 1/100 比例模拟（61 次压实、摘要器累计输入约为内容量的 33 倍）方向和代码一致。

设计：rupi-agent compress_inner：(1) 触发量改为 `projected = estimate(system+tools) + Σ estimate(prompt_history())`，优先用最后一轮 provider 报告的 `input+cache_read+cache_write`（usage_by_node 已存，rupi-core/src/lib.rs:188-212）加上之后新增消息的估算（上游 contextTokens 的做法），删掉 2×keep 启发式。(2) 摘要区间改为 `[pos(summary_through)+1 .. cut)`，旧摘要放进 `<previous-summary>` 用 update prompt 合并。(3) 区间超过摘要器预算（窗口减输出上限减提示词）时分块串行 update，不再走首行兜底。(4) provider 出错时保留旧摘要，再追加 `[N messages elided]` 标记，绝不覆盖旧摘要；同时发 compaction failed 事件（见生命周期事件那一条）。(5) 云端 tokensBefore 和 tokensAfter 记进压实条目和指标。相关文件：rupi-agent/src/lib.rs、rupi-core（prompt_history 现在把 keep_recent_tokens 当条数兜底传入，lib.rs:705-706 对应 rupi-core/src/lib.rs:261-271，要一并修正签名）。

风险：触发变晚以后，单步大工具结果更容易直接冲破窗口，所以轮中压实（lib.rs:1102）要保留，并配合溢出恢复。增量摘要会累积误差，可以每 K 次做一次全量重摘（异步、低优先级模型）。已有会话的 summary_through 可以直接沿用。

### 会话持久化改为增量写入 + 显式 leaf（第一阶段），再演进到只追加的 entries 表（第二阶段） [critical/L]
上游：session-manager.ts:1172-1197（`_persist` 只 appendFileSync 新条目）、1103-1111 _buildIndex（leaf = 最后一条）；docs/session-format.md:49-60、209-248（id/parentId 树、从 leaf 建上下文）；CHANGELOG 0.87.0 Breaking（SessionManager 是上下文权威）、0.85.0 #8990（fork 丢失压实边界）；durable/docs/group-commit.md（批量提交）

差距：已核对。crates/rupi-server/src/db.rs:1071-1117：每次 run 结束（run.rs:724）和每次 interrupt（run.rs:451）都在一个事务里 `DELETE FROM messages WHERE session` 再逐行 INSERT 全部节点，写入量是 O(会话长度)，会话生命周期内是 O(N²)。每行都要维护 `messages_trgm`（gin_trgm_ops）、`content_tsv`（STORED 生成列，含 rupi_cjk_ngrams）、`messages_tenant_created`、`messages_session` 等索引（db.rs:245、311-330、411-420），content 是 full_text()，包括 tool 输出和 thinking。finish 还把整棵树序列化成一个 Redis 值（run.rs:725-730），大会话就是一个大 key。db.rs:1130-1168 的 `load_tree` 按 `created_at` 排序（无 tiebreak），把所有节点按顺序塞进 current_path，不存 leaf；clone（http.rs:446、473 `remap_tree(..., false)`）和 import（http.rs:613）会写入全部分支，再次加载时所有分支被拼成一条线性历史送给模型。import_session（http.rs:598-626）不检查 run lease，可能和正在运行的 run 互相覆盖。

设计：第一阶段（M，不改 SessionTree 结构）：`sessions` 加 `leaf_id`、`last_seq`，`messages` 加 `seq BIGINT` 并建 `(tenant_id, session_id, seq)` 唯一索引。run 开始时记下已加载的节点 id 集合，finish 时只把新节点用一次多行 INSERT（或 COPY）写入，然后 `UPDATE sessions SET leaf_id=$1, last_seq=$2, summary=…, summary_through=… WHERE id=$3 AND tenant_id=$4 AND last_seq=$expected`，影响 0 行就返回 409。load_tree 按 seq 读出，从 leaf 沿 parent 建 current_path。import 和 clone 走同一个带 expected_seq 的写入，并检查 lease。search 用的 content 只放 user/assistant 文本，tool 输出截断或不进。第二阶段（L）：迁移到 `session_entries(tenant_id, session_id, seq, id, parent_id, kind /*message|compaction|model_change|thinking_change|context_edit|usage|label|session_info*/, role, body JSONB, search_text, created_at, PRIMARY KEY(tenant_id, session_id, seq))`，压实写成 `kind=compaction` 条目（summary、first_kept_entry_id、tokens_before、usage、details{read_files, modified_files}），`sessions.summary` 列只读兼容后废弃；rupi-core SessionTree 增加非消息条目（包一层 Entry enum），本机 SQLite 复用同一模型。按 tenant_id 哈希分区放到真正出现单表瓶颈之后再做。Redis 缓存 key 带 last_seq，只缓存元数据或增量。

风险：迁移要回填已有 messages 的 seq（按 parent 链和 created_at 顺序）和 leaf_id；新索引要按租户分批在线建。第二阶段的 SessionTree 结构变化会波及 TUI、导出、remap_tree。fork 可以在新会话里存 parent_session 引用加起点 seq，避免复制所有行。

### 缓存友好的 prompt 组装：给历史加缓存断点、按会话缓存路由，并停止每次 run 重复写入 AGENTS.md [critical/M]
上游：CHANGELOG 0.86.0 #9548（transcript-backed 系统提示和工具变更，保住缓存前缀）、0.99.2（mcp_servers 段变化时只追加，不改前缀）、1.0.2（Anthropic 中途重定义工具时内联，保住缓存）、1.4.0 #10940、1.2.0 #10267（运行中改 prompt 会两次打破缓存）；ai/src/api/anthropic-messages.ts:1526-1550（最后一条 user 消息加 cache_control）；0.86.0 多条 x-session-id / session affinity 修复；docs/session-format.md:80-87

差距：(1) rupi-llm/src/anthropic.rs:74-93 只在 system 和最后一个工具上打 cache_control，messages 一个断点都没有，多轮对话历史每轮都按全价输入计费；Bedrock（bedrock.rs:58-75）连 system 和 tools 的断点都没有。(2) run.rs:312 加 862-966：每次 run（包括 resume run）都把 `[workspace AGENTS.md]` 和 `[command …]`（最多 16 个文件，每个最多约 12K 字符）作为 Role::System 节点 push 进树，finish 落盘后下次 run 再 push 一次，历史里的副本数等于 run 次数；同样的内容还拼进了 builder.append（370-375），在 system 里再出现一次。这些节点还会插到 resume 的 tool_use 和 tool_result 之间，造成 400（见配对不变量那一条）。(3) 云端从不调用 `apply_session_settings`（rupi-llm/src/lib.rs:423-430，rupi-server 内 grep 无调用），provider 每次 run 由 provider_factory 新建（rupi-server/src/lib.rs:137-152），session_id 随机生成（anthropic.rs:28），OpenRouter 等网关的会话亲和失效，OpenAI 也没有 `prompt_cache_key`。更正原提案：lib.rs:699-701 把 `<Recalled>` 拼进 system 这一点在云端不成立，云端 PostgresMemory 没有 prefetch，recalled 恒为空，只影响本机。

设计：(1) Anthropic 和 Bedrock：在最后一条 user/tool_result 消息的最后一个块上加 `cache_control`，再在上一轮的最后一条消息上保留一个断点（Anthropic 只回看约 20 个块，单轮追加大量块时上一个断点可能落到回看窗口外；总断点不超过 4 个：tools、system、上一轮末尾、本轮末尾）。Bedrock 改为和 Anthropic 一样的 system 数组形状并打断点。OpenAI 发 `prompt_cache_key=thread_id`。云端在 run.rs:313 之后用 thread_id 设置 provider 的 session_id（provider_factory 返回 Arc，需要在构造时传入，或给工厂加 session 参数）。(2) 工作区 guidance 只放进 system，不写进树；按工作区快照或文件指纹缓存在 Redis，每个热工作区只加载一次。entries 表落地后再按上游 sections 补丁思路记一条 `system_checkpoint`。已有会话里重复的 `[workspace …]` 和 `[command …]` 节点在投影时去重（只保留最后一份或全部省略），不改历史。(3) 本机：`<Recalled>` 从 system 移到当前 user 消息之前（只影响本机 CLI）。(4) 指标：按 provider 统计 `rupi_prompt_cache_read_ratio`（cache_read/(input+cache_read)，标签不放 tenant_id），以及每个 run 的 system hash 变化次数。

风险：缓存写有溢价（Anthropic 5 分钟 1.25 倍），很短的会话可能略亏；低于最低可缓存长度（视模型 512 到 4096 token）的前缀不会被缓存。投影时去重会改变老会话看到的上下文，要灰度。

### 结构化摘要模板 + 序列化去掉 thinking、截断 tool 结果 + 摘要请求处理溢出并不覆盖旧摘要 [high/S]
上游：docs/compaction.md:234-291（Summary Format；thinking 省略；tool result 2000 字符）；compaction.ts:507-579（SUMMARIZATION_PROMPT/UPDATE_SUMMARIZATION_INSTRUCTIONS）、617-636（completeSummarization：cacheRetention none + retryAssistantCall）；utils.ts:94、161；CHANGELOG 1.2.0 serializeConversation 省略 thinking、1.4.0 #11058（/compact 指令在切开的 turn 上被忽略）、1.3.0 #10965（摘要首次失败也应重试）

差距：rupi-agent/src/lib.rs:1276-1288：system 只有一句 "Summarize this conversation prefix concisely ... Be brief."，输入是 `m.full_text()` 用 `---` 拼接，没有角色标签，也没有 `<conversation>` 包裹，模型容易把它当作要续写的对话。rupi-core/src/lib.rs:100-118 的 `full_text` 带 `[thinking] …` 全文；工具结果不再截断（云端 read 已在工具层截到 12000 字符，rupi-tools/src/lib.rs:572，但 bash 没有上限）。更正原提案两处：摘要请求本身有 HTTP 级重试（complete 走 post_json_with_retry，429/5xx/传输错误重试 3 次，rupi-llm/src/lib.rs:597、anthropic.rs:634），缺的是对溢出错误的处理；摘要请求 tools 为空、system 只有几十 token，低于最低可缓存长度，现在并不付缓存写溢价。

设计：rupi-agent 新增 `compaction::serialize_for_summary(&[&Message])`：输出 `[User]: …` / `[Assistant]: …` / `[Assistant tool calls]: read(path=…)` / `[Tool result]: <截到 2000 字符>…[N chars truncated]`，跳过 Thinking/RedactedThinking/Image，整体包在 `<conversation>` 里。提示词照搬上游结构化模板和 update 模板，`/compact <指令>` 作为 Additional focus 附加（split-turn 两段都带上）。摘要请求遇到溢出错误时缩小区间重试（配合增量压实的分块），并检查 cancel；任何失败都保留旧摘要。ChatRequest 增加 `cache: CachePolicy::None`，等缓存那一条给 messages 加断点之后，摘要请求不打断点，避免为一次性大输入付 1.25 倍写入。保留现有的 `<read-files>/<modified-files>` 合并逻辑（lib.rs:1343-1420），它和上游一致。

风险：结构化模板会让摘要更长（大约 1 到 3K token），需要在 keep 预算里留出空间。截断工具结果会丢掉一些细节，靠 modified-files 列表和 keep 窗口弥补。

### 压实等内部模型调用纳入用量和配额，并把 cache 读写分开记账 [high/M]
上游：docs/session-format.md:113-137（UsageEntry、CompactionEntry.usage 计入会话总计）；CHANGELOG 0.99.0（`ctx.executeTool()` 的用量曾从会话成本中丢失，已修）、1.1.0（分层定价少算）、0.86.0（Bedrock 1h cache 按 5m 计价，已修）；compaction.md:165

差距：rupi-llm/src/lib.rs:148-151 的 `ChatResponse` 没有 usage 字段，`complete()`（lib.rs:593-600）丢掉了用量。compress_inner（rupi-agent/src/lib.rs:1289）用的是 complete，不发 `AgentEvent::Usage`，而云端只在收到 Usage 事件时 `bump_quota_tokens`（run.rs:339-348），所以压实请求的 token 不计入租户配额（quota.rs:38 按 tokens 判断日配额），也不出现在账单里。run.rs:345 只累加 input+output，忽略 cache_read 和 cache_write：Anthropic 的 input_tokens 不含缓存部分，缓存读写完全没有计入；OpenAI 的 prompt_tokens 已含缓存读，按全价记会多算成本。更正原提案：review.rs:323 和 subagent.rs:260-271 同样不报用量，但云端目前没有挂 reviewer，也没有注册 subagent 工具（rupi-server 内 grep 无结果），云端今天只有压实一处漏计；这两处在云端接入它们之前要一并修好。

设计：(1) rupi-llm：`ChatResponse { usage: Option<TokenUsage> }`（serde default，兼容 mock_script 里的旧 JSON），各 provider 的 complete 和 complete_streaming 都填上（流式路径已经拿到了用量）。(2) rupi-core：`AgentEvent::Usage` 增加 `kind: UsageKind {Turn, Compaction, Review, Subagent, CacheWarm}`（serde 默认 Turn）；compress_inner、review、subagent 都发这个事件。(3) 云端：quota_ledger 加 `input, output, cache_read, cache_write, cost_micros` 列，用模型目录价格计算成本（models.json 现在只有 context，要补价格和 cache 价）；`admit_run` 按 cost 或总 token 判断。parse_provider_usage（rupi-llm/src/lib.rs:169-209）里统一成"input 不含缓存"的口径。(4) 压实条目（entries 落地后）带 usage；导出 JSONL 时写进 compaction.usage。(5) 指标 `rupi_tokens_total{kind,provider}`。

风险：计价表要维护（建议目录化，参考上游 models.json 的 cost 字段）。改 ledger 结构需要迁移。各 provider 的 input 是否包含缓存部分语义不一致（Anthropic 不含，OpenAI 含），需要逐个 provider 归一并写测试。

### 让会话和租户设置真正生效：按会话选模型、应用租户 compaction/tools/base_url 设置、记录 model_change [high/M]
上游：sdk.ts:442-453（新会话写 model_change/thinking_level_change，resume 时还原）；docs/session-format.md:97-111；CHANGELOG 1.3.0 #10785（resume 保留 active tools）、1.3.0 #10995（换模型时丢弃其他模型的 thinking）、1.0.2（Bedrock 在 system/tools 变化后签名失配）、0.86.0 compaction.modelOverrides；docs/settings.md:38-82

差距：run.rs:313 `app.provider_for(&tenant)` 只按租户 settings.model / default_model 解析（resolve_byok，run.rs:790-847）。客户端通过 AG-UI state 改了 `model`（apply_state，run.rs:700-721 写入 sessions.model），但那个值只在 STATE_SNAPSHOT 里回显（run.rs:323），实际用的还是租户默认模型。admin.rs:24-50 的 ALLOWED_SETTING_KEYS 接受 `thinking/tools/defaultTools/excludeTools/compaction/steeringMode/followUpMode/enabledModels/base_url` 等键，但 rupi-server 只读取 `mock_script`、`provider`、`model`、几个 api key 和 `git_token`（run.rs:759-840、http.rs:326），其余键写进去都不起作用；其中 `base_url` 也没有进 ProviderOptions，租户配置的自有网关地址被静默忽略。AgentLoop 的 max_turns 在云端写死为 12（run.rs:363）。更正原提案：按会话的 thinking 已经生效（sess.thinking_level，run.rs:376-382），不生效的是租户级 `thinking` 键。

设计：run.rs drive：effective_model = sess.model（需在 tenant.enabledModels 白名单内，否则 400）→ tenant.settings.model → default；resolve_byok 改为接收 (tenant, model)，并把 `base_url` 传进 ProviderOptions（要和出站 SSRF 守卫一起做）；`AgentLoop::with_compression(...)` 和压实覆盖从 tenant.settings.compaction 解析（复用 rupi-agent parse_compression_overrides 的 schema）；tools 和 excludeTools 走 `with_tool_filter`；max_turns 改为租户可配并设上限。模型或 thinking 变化时在会话追加 `model_change` 条目（依赖 entries 那一条，之前可以先写 sessions 列）。换模型后对 Anthropic 回放其他模型的 signed Thinking 块：新模型会忽略读不了的块，但网关和旧模型可能 400，转换层按 provider/model 来源过滤更稳。没有云端含义的键（`theme`、`externalEditor`、`defaultProjectTrust`）从白名单移除，或明确标注为未使用。

风险：允许按会话换模型以后，配额要按模型价格计算（依赖用量记账那一条）。BYOK 下不同 provider 的密钥缺失要给出明确错误，不能静默回退到 mock（route.rs:290-296 的 provider_or_mock 在云端不应该使用，default_provider 现在就在用它，run.rs:858）。

### 云端 bash 输出设上限：执行面有界读取，控制面截断后再进上下文和 Postgres [critical/S]
上游：CHANGELOG Unreleased #11360（coding-agent.CHANGELOG.md:8、27：`pi.exec()` maxBuffer，默认 256MiB 与运行时字符串上限取小，超出后终止并标记 `truncated`）；0.99.0（bash 结构化结果上限 1 MiB，保留首尾各 512KiB，加 `truncated`/`full_output_path`）；1.0.3（输出文件只对本用户可读）；docs/mcp.md（文本结果超过 20KB 截中间）

差距：已核对。rupi-runtime/src/engine.rs:313-334 用 `child.wait_with_output()` 把 stdout/stderr 全部读进内存，没有上限，超时上限 300 秒（303 行）。execd（execd.rs:246-262）和 sandboxd（sandbox.rs:264-281）把整个结果用 JSON 返回。控制面 HTTP 执行面客户端用 `resp.text()` 一次读完响应体（rupi-runtime/src/http.rs:36-38）。ExecBashTool（rupi-server/src/tools.rs:255-271）直接拼接 stdout 和 stderr 作为 ToolOutput，resume 路径的 bash（run.rs:640-665）也一样，都没有调用 `truncate_tail`。整段输出进入模型上下文、SSE（TOOL_CALL_RESULT，agui.rs:240-256）、Redis 树缓存和 Postgres messages.content 及其索引。rupi-runtime 里没有任何 rlimit/cgroup 内存限制（grep 无结果）。一个 execd 节点承载多个租户的工作区（engine.require_tenant 按 handle 校验租户），所以这是跨租户的影响面。对照：只有 fs 工具走 sandboxed builtins 有截断（read 截到 12000 字符）；本机 BashTool 已有有界读取 `read_pipe_tail`（rupi-tools/src/lib.rs:883-884），可以直接复用。

设计：rupi-runtime engine：spawn 时 stdout/stderr 设为 piped，各起一个 task 有界读取（复用 rupi-tools 的 read_pipe_tail 思路），保留 head 256KiB 和 tail 256KiB，同时统计 total_bytes；超过硬上限（如 64MiB，可配）就对进程组 SIGTERM（jail 已设 process_group(0)，jail.rs:195、312），5 秒后 SIGKILL。`ExecResult` 增加 `truncated`、`total_bytes`、`full_output_path`（serde default，兼容旧执行面）。完整输出可选写进工作区 `.rupi/out/<call>.log`（0600，计入卷配额）。控制面 ExecBashTool 和 resume 路径统一用 `truncate_tail(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)`，提示模型用 read 加 offset 分页读完整日志。执行面客户端读取响应体时设上限。persist 前对单条 ToolResult 设上限（如 64KB），search 文本不放 tool 输出。另外给沙箱进程加内存 rlimit 或 cgroup 限制，作为纵深防御。

风险：截断会让模型偶尔看不到关键信息，需要提示它去读完整日志文件。按字节切分要落在 UTF-8 边界上（复用 rupi-tools/src/utf8.rs）。

### 处理 length/max_tokens 截断：参数是坏 JSON 的工具调用不执行，输出上限按模型目录设置 [high/S]
上游：CHANGELOG 1.3.0 #10897（coding-agent.CHANGELOG.md:149，最终参数是坏 JSON 时校验失败，并把原始参数回给模型）、1.4.0 #11227（截断的 tool call 被省略）、1.4.0 #9512（使用模型 output limit）、1.2.0 #10799（print 模式 length 时非零退出）；docs/compaction.md:39（早停的 length 可以触发一次压实重试）；docs/cli-integration.md:30

差距：rupi-llm/src/anthropic.rs:591 和 lib.rs:743 对流式累积的参数做 `serde_json::from_str(&buf).unwrap_or(json!({}))`：被 max_tokens 截断的 tool call 以空参数 `{}` 执行，模型拿到的是 "command required" 之类的误导性错误，不知道是自己被截断了。stop_reason 里的 "length"/"max_tokens"/"MAX_TOKENS" 没有任何处理，只有 "error" 会中止（rupi-agent/src/lib.rs:784-786）。Anthropic 默认 max_tokens=4096（anthropic.rs:84），Bedrock 同样 4096（bedrock.rs:61），Gemini 同样 4096（gemini.rs:69-70），写一个大文件的 write/edit 调用很容易超过这个上限。OpenAI 路径不发 max_tokens（lib.rs:374-398），用 provider 默认值，受影响较小。models.json 只有 `context` 字段，没有输出上限。

设计：rupi-llm：在 ToolCall 块上保留 `raw_arguments: Option<String>`（解析失败时填充），stop_reason 归一化为枚举 `{Stop, ToolUse, Length, Error}`。rupi-agent run_turns：Length 加 tool call 时，参数解析失败的调用不执行，合成 `ToolResult{is_error, "arguments truncated at output limit (N chars): <前 200 字符>…; split the write into smaller edits"}`（保持配对不变量）；Length 加纯文本作为最终回答时，在 RUN_FINISHED 带 `truncated:true`。models.json 增加 `maxOutput` 字段，Anthropic、Bedrock、Gemini 默认用 min(maxOutput, window-input-余量)，参考上游 3.5 字符/token 的估算（1.1.0 #10497）；大 max_tokens 时这些 provider 已走流式，不会撞 HTTP 超时。

风险：提高输出上限会让单次请求的最坏延迟和成本上升，需要按租户设上限。不同 provider 的 length 语义不同，需要逐个测试。

### 修正 Pi JSONL v3 互操作：导入现代 Pi 会话、导出时 compaction 放在正确位置 [medium/M]
上游：docs/session-format.md:62-207（全部条目类型）、224-248（从 leaf 建上下文，compaction 必须在路径上）；session-manager.ts _buildIndex（leaf = 文件最后一条）；sdk.ts:448-452（新会话以 model_change/thinking_level_change 开头）；0.86.0 system message 条目

差距：rupi-memory/src/session_io.rs:307-437 的 import_jsonl 只认 message/custom_message/compaction/session_info，`model_change`、`thinking_level_change`、`branch_summary`、`context_edit`、`label`、`usage`、`custom` 都被丢掉，它们的子节点因此断链。432-435 行用 goto_node(最后一条 message) 沿 parent 回溯，遇到缺失的父节点就返回 false。scratch 复现：一个典型的现代 Pi 会话（model_change→thinking_level_change→system message→user→assistant）导入后 nodes=3、current_path=0、history 为空；含 branch_summary 的分支会话同样为空。role=system（sections/toolsAdded）被映射成空文本 User（488 行）。导出（67-92）把 compaction 条目放在文件末尾、parentId=through：复现显示最后一条就是 compaction 且 parentId 等于 through 节点，Pi 以最后一条为 leaf，打开后会从 through 处另起分支，丢掉全部保留的尾部消息。导出的 assistant 写的是 `"model": ""`、`"api": "openai-completions"`、`stopReason "stop"` 占位值（session_io.rs:219-226）。

设计：导入：先把所有带 id/parentId 的条目建成 parent 映射；非上下文条目（model_change、thinking_level_change、label、session_info、usage、custom）作为透明节点，把子节点重新挂到最近的上下文祖先上（或在只追加 entries 落地后作为带类型的条目保存）；role=system 带 sections 的条目存成 system_checkpoint，不进 user 历史；branch_summary 和 compaction 按 Pi 规则只取 leaf 路径上最新的一条；context_edit 应用省略；leaf 取文件最后一个条目。导出：compaction 条目挂在 leaf 之后（parentId=leaf，firstKeptEntryId=through 的后继），session_info 挂在 compaction 之后；assistant 写真实的 provider/model/stopReason/usage；开头写 model_change。增加 golden 测试，用上游仓库 test fixtures 里的 JSONL 做往返测试，并在 http.rs import 时给出被丢弃条目类型的统计。

风险：Pi 格式还在演进（例如 system message 的 sections），需要固定版本号并对未知类型宽容处理。导入不可信 JSONL 要设大小和条数上限，防止上传很大的文件。

### 云端工作区资源的发现和加载策略：显式目录、标注为不可信、每个热工作区只加载一次 [high/S]
上游：docs/security.md:25-57（project trust 只控制 .pi 资源；AGENTS.md 总会加载，但要当不可信输入）、docs/containerization.md:18-28；CHANGELOG Unreleased #10947（保存的 Do not trust 优先）、1.2.0 #10681（symlink AGENTS.md）；docs/skills.md 发现目录（.pi/skills、.agents/skills）

差距：run.rs:862-966 的 load_remote_guidance 每次 run 都会：fs_read 3 个上下文文件；`glob **/SKILL.md` 遍历整个工作区并读取最多 32 个文件注入 SkillRegistry；glob `commands/*.md` 和 `.rupi/commands/*.md` 并把最多 16 个文件作为 System 节点写进历史。首个 token 前最多约 54 次执行面 HTTP 往返，结果不缓存。这些内容来自用户克隆的任意仓库，以 system 身份注入，没有标注来源。更正原提案两处：glob 走 ignore_walker(standard_filters(true))（rupi-tools/src/lib.rs:1176-1180），在 git 仓库里遵守 .gitignore，不会扫进被忽略的 node_modules/vendor（非 git 工作区才会全扫）；fs_read 经 ReadTool 截到 12000 字符（lib.rs:1398、572），单文件有隐式上限。审查新发现：standard_filters 会跳过隐藏目录，scratch 验证（/tmp/rupi-review-glob）显示 `.rupi/commands/*.md` 返回空、`.agents/skills/a/SKILL.md` 不会被 `**/SKILL.md` 发现，所以 `.rupi/commands` 在云端是死代码，上游标准的 `.agents/skills`、`.pi/skills` 也永远加载不到；反过来，仓库里任意非隐藏路径下的 SKILL.md（包括第三方依赖的 vendored 副本）都会被当成 skill。

设计：租户设置增加 `workspaceResources: {trust: "guidance-only"|"full"|"none", skillDirs: [".agents/skills", ".pi/skills", ".rupi/skills", "skills"], maxFiles, maxBytesPerFile}`。云端只在白名单目录发现 skill，不再用 `**/SKILL.md`；因为 glob 工具会过滤隐藏目录，需要给 Executor 加一个不经 standard_filters 的受限列目录接口（只允许白名单前缀，跳过 symlink 和 `..`），或对已知路径直接 fs_read。注入时包成 `<workspace-instructions path=… trust=untrusted>`，并在 system 前言中声明这些内容不能改变工具策略。结果按 (handle, 快照 id 或文件 mtime+size 指纹) 缓存在 Redis，每个热工作区只加载一次，提供显式 reload。commands 只在用户输入 `/name` 时展开，不再常驻历史；guidance 不再写进树（见缓存那一条）。

风险：有些租户的 skill 放在非标准目录，迁移期需要提示和配置项。缓存失效依赖执行面提供 mtime/hash（可以给列目录接口带上 stat）。

### 多租户远程 MCP：租户级 HTTP MCP 服务器、暴露模式、命名规范、凭证隔离和 SSRF 防护 [high/L]
上游：docs/mcp.md:46-77（配置规则：名称规范化、`mcp__<server>__<tool>`、hash 后缀、拒绝 SSE、timeout）、100（连接与重试）、191-212（exposure direct/deferred/codemode/hidden、toolExposure 通配、mcp_servers 段只追加、超过 20KB 截断）、120-200（OAuth、RFC 9728/8414/9207、凭证按 name+URL 隔离）；CHANGELOG 0.99.0/0.99.2 #10212、#10239、1.2.0 #10526（连接时不列 resources）、#10416、Unreleased（拒绝非 http(s) 的 metadata URL）、1.0.0 RFC 9207

差距：云端完全没有 MCP：cloud_tools 只有 read/write/edit/glob/grep/bash/think（rupi-server/src/tools.rs:13-43），rupi-server 里 grep mcp 零命中。本机的 rupi-mcp 工具名用 `{server}_{tool}`（rupi-mcp/src/lib.rs:1055、1395），存在冲突（server `a_b`+tool `c` 和 server `a`+tool `b_c`），冲突时"先到先得"并丢弃后者（1396-1399），没有 hash 后缀，没有 exposure 分级，结果也不截断。rupi-agent 的 discovery（discovery.rs:71-110）已经实现了 search_tools 渐进发现，但它的打分把 name 和 description 混在一起，CJK 也不分词（对比上游 Unreleased #11331、#10816）。

设计：数据模型：`tenant_mcp_servers(tenant_id, name, url, headers_enc BYTEA /*KMS 加密*/, exposure, tool_exposure JSONB, description, timeout_ms, enabled)`，只支持 streamable HTTP，云端禁止 stdio。连接：控制面按 (tenant, server) 懒连接，`mcp-session-id` 和 tools/list 结果（带 ETag/TTL）缓存在 Redis，任何实例都能复用列表，调用时在本实例建连接，空闲后回收；也可以评估把 MCP 出站放到执行面沙箱的受控出口，让控制面不直接连租户 URL。出站经过 SSRF 守卫（拒绝私网、链路本地、metadata IP，DNS 解析后再校验，最多 3 次重定向）。命名：`mcp__<server>__<tool>`，非 [A-Za-z0-9_] 字符替换为 `_`，冲突时加 hash 后缀（rupi-mcp 一并修改）。暴露：默认 deferred，经 rupi-agent discovery 的 search_tools 声明，已声明的工具记进会话条目，resume 后保持；direct 需要显式配置，并且要保证工具列表顺序确定，避免打破前缀缓存。结果：文本超过 20KB 截中间并保存到工作区文件。审批：MCP annotations 里的 destructiveHint/openWorldHint 走 Ask→Interrupt。OAuth：用托管 callback（非 loopback），复用上游的 iss 和 metadata URL 校验规则。

风险：第三方 MCP 返回内容是 prompt 注入的主要来源，需要配合审批和输出标注。控制面对外部服务的长连接会占用资源，要设每租户并发和超时上限。OAuth 托管流程本身需要另做安全评审。

### 可观测的生命周期事件：带原因和结果的压实事件、settled/aborted、重试事件，Retry-After 设上限 [medium/S]
上游：docs/json.md:48-56（agent_settled{aborted}）、107（tool_execution_end.durationMs）、128-180（compaction_start{reason} / compaction_end{result,aborted,willRetry,errorMessage}、auto_retry_*、summarization_retry_*）；docs/compaction.md:370-383（session_compact_failed）；docs/settings.md:128-136（retry.maxAgentDelayMs 60000、provider.maxRetryDelayMs 60000）；CHANGELOG 0.86.0 #8826、0.99.2 #9571、1.1.0 #10607

差距：agui.rs:290-300 把压实映射成 STEP_STARTED/FINISHED{summarized,kept}，没有 reason（threshold/overflow/manual）、tokensBefore/After、失败或中止状态。rupi-core 定义了 AutoRetryStart/End（lib.rs:561-577），但 grep 显示没有任何地方产生这个事件：重试藏在 `post_json_with_retry` 里（rupi-llm/src/lib.rs:907-935），sleep 期间不检查 cancel。`parse_retry_after`（865-868）没有上限，provider 返回 `Retry-After: 3600` 时这个 run 就挂一小时，同时占着 run lease、执行面 handle 和 SSE。`AgentSession.auto_retry_enabled`（rupi-agent/src/session.rs:192-194）只存不读。/metrics（http.rs:76-88）只有 runs、429、executor 三个指标。

设计：rupi-core：`CompactionStart{reason}`、`CompactionEnd{reason, tokens_before, tokens_after, usage, aborted, error, will_retry}`；ToolEnd 增加 `duration_ms`。rupi-llm 的重试改成可观测：传入回调或 event sender 发 AutoRetryStart/End；`Retry-After` 封顶 60 秒（可配），总重试预算封顶；sleep 和 cancel 用 select 并发等待；尊重 auto_retry 开关。AG-UI：压实和重试用 `CUSTOM` 事件（name 为 `rupi.compaction` 或 `rupi.retry`），RUN_FINISHED outcome 带 `aborted`/`truncated`。Prometheus 增加 `rupi_compactions_total{reason,outcome}`、`rupi_compaction_tokens`、`rupi_provider_retries_total{provider,status}`、`rupi_tool_duration_seconds{tool}` 直方图，并配上 tracing span。

风险：AG-UI 的 CUSTOM 事件需要客户端识别，未知事件按协议会被忽略，因此兼容。指标标签要控制基数（不放 tenant_id）。

### 云端 steering 和 follow-up：运行中追加的消息在工具间隙注入，响应带 disposition [medium/M]
上游：docs/rpc-commands.md:7-90（prompt/steer/follow_up 的 disposition: started/queued/handled）；docs/sdk.md:62-70（运行中的 prompt 必须指明 steer 或 followUp）；CHANGELOG 0.99.0 #9098/#9803、1.2.0 #10739（启动中也算 busy，abort 在启动后生效）、0.86.0 #8718

差距：本机已有 MessageInbox 和 steering 注入（rupi-agent/src/queue.rs，lib.rs:288-310、1105）。云端 run.rs:363 构造的 AgentLoop 没有 inbox；会话正在运行时再次 POST /v1/agent 直接返回 409 "run already in progress"（run.rs:146-161），也没有 steer 端点。

设计：新增 `POST /v1/sessions/{id}/steer {text, mode: steer|followUp}`：会话持有 lease 时，写入 Redis 列表 `rupi:{tenant}:steer:{thread}`（并写一份 Postgres 留档），返回 `{disposition:"queued"}`；没有 lease 时返回 `{disposition:"start"}`，提示客户端走 /v1/agent。持有 lease 的实例给 AgentLoop 挂一个 Redis 版的 `MessageInbox`，在工具间隙或心跳时 LPOP 取消息（inject_steering 已经存在）；run 结束前检查 followUp 队列，有消息就在同一个 run 里继续，参考 AgentSession::prompt_queued（session.rs:320-368）。注入的消息作为 SteeringInjected 映射成 AG-UI 的 user 消息事件。配额：steer 计入 QPS。

风险：跨实例投递依赖 Redis 可用，降级方案是 Postgres 轮询。lease 交接（实例崩溃）时队列消息不能丢，要用 at-least-once 加去重 id。

### 统一会话运行时：CLI、SDK、云端共用一套 AgentSession 和存储抽象，去掉进程全局环境变量 [medium/L]
上游：docs/sdk.md:24-60（SessionManager 是权威、inMemory 恢复外部条目、AgentSessionRuntime 替换会话、dispose）；CHANGELOG 0.85.0 #8980（SessionManager.inMemory 恢复外部存储）、0.87.0 Breaking（SessionManager 是上下文权威）、1.2.0 移除实验性 server/client 栈

差距：rupi-tools/src/lib.rs:1421-1441 的 `export_session_id`、`export_model` 等函数通过 `std::env::set_var` 修改进程全局环境变量，AgentSession::set_model 和 adopt_session 都会调用（rupi-agent/src/session.rs:395、461）。一个进程里嵌入多个会话（SDK 嵌入、未来服务端复用）时，这些变量会串号，而且在多线程下调用 set_var 本身不安全。云端没有用 AgentSession，而是在 run.rs:363-416 自己组装 AgentLoop，所以 inbox、auto_retry、扩展事件、follow-up 都有两套实现，逐渐分叉。AgentSessionBuilder 默认的 home 是共享的 temp_dir/rupi-embed（session.rs:117-121），没有 key 时静默落到 Mock（session.rs:114-116）。

设计：rupi-core 定义 `trait SessionBackend { async fn load(&self, id) -> Entries; async fn append(&self, id, &[Entry], expected_seq) -> Result<Seq>; async fn set_leaf(...) }`，本机 SQLite（rupi-memory）和 Postgres（rupi-server db）各实现一份，配合只追加 entries 那一条。AgentSession 持有 `Arc<dyn SessionBackend>` 和 `SessionEnv {session_id, model, provider, reasoning}`，把 SessionEnv 显式传给工具（ToolContext），bash 子进程用 `Command::envs` 注入，不再调用 set_var。云端 drive() 改为构造 AgentSession（on_ask=Interrupt、外部 inbox、云端 tools），保留 run.rs 里的 lease 和 SSE 逻辑。builder 在没有 provider 时返回 Err，不再静默 Mock。

风险：重构面大，会碰到 CLI、TUI、RPC、云端四条路径，需要分阶段：先去掉全局 env，再抽 backend，最后迁移云端。测试矩阵要覆盖四种入口。

### prompt cache warming（只在运行中保温，按成本判断） [low/M]
上游：CHANGELOG 0.86.0 #9668；pkg/coding-agent/src/core/cache-warmer.ts:14-60（在 TTL 的 90% 时刷新、预期节省 ≥$0.05 才发、流式保温最长 60 分钟、idle 保温 30 分钟、IDLE_CONTINUATION_PROBABILITY=0.15、预算型 thinking 的 Anthropic 请求不可重放）；docs/settings.md:21-23；session-format.md UsageEntry kind=cache_warm

差距：rupi 没有缓存保温。云端 bash 超时上限 300 秒（engine.rs:303），Anthropic 默认缓存 TTL 5 分钟，长测试或构建之后的下一轮可能整段 cache miss；而且 rupi 目前连历史缓存断点都没有（见缓存那一条），这一条依赖它先落地。

设计：控制面在长工具执行期间（工具耗时超过 TTL×0.9），对持有 lease 的会话用最后一次请求的上下文发 max_tokens=1 的重放请求，记 `Usage{kind: CacheWarm}`。只在 Anthropic/Bedrock 非预算型 thinking 下启用，按 (上次 cache_read 量 × 价差 × 继续概率) 判断是否发。默认关闭，按租户开启。不做 idle 模式。

风险：判断失误就是纯成本；重放请求被 provider 计入速率限制；预算型 thinking 下不可重放（上游 isReplayable 已有说明）。

### 多租户下的 packages 和扩展：只物化声明式资源，代码类扩展只能在执行面沙箱里运行 [low/M]
上游：docs/packages.md:21-23（项目包在 trust 之后才装；包可以执行代码）、112（禁止自动安装 peer 依赖）；docs/security.md:84-93；CHANGELOG 0.99.0 #9863、Unreleased #11375（-e 包每次启动都检查更新）

差距：云端调用 run_with_user 时 extensions 传 `&[]`（run.rs:411、693），不加载任何扩展或包。rupi-pkg（crates/rupi-pkg）只在本机物化 skill、command 和 JSON-RPC 扩展。目前没有租户级的包分发方式。

设计：新增租户级 `tenant_packages(tenant_id, source, pinned_digest, kind: skills|prompts)`，安装时在隔离 builder 里拉取并校验 digest，只物化 Markdown 资源，作为只读层挂进工作区（或在执行面 bootstrap 时解压）。JSON-RPC 扩展如果要开放，只能作为执行面沙箱内的子进程，通过 Executor 协议转发事件，不能在控制面里运行。禁止浮动版本，必须固定 digest。

风险：包内的 skill 依然是 prompt 注入面，需要和工作区信任那一条的不可信标注一起使用。执行面扩展进程会消耗沙箱资源配额。

### search_tools 打分和 CJK 分词改进 [low/S]
上游：CHANGELOG Unreleased #11331（名称命中权重高于描述；命中更多查询词的排前；整个名称都在查询里有大加成；服务器说明不再稀释打分）、#10816/#11037（任意文字可匹配；无空格中文分词；Unicode 规范化）

差距：rupi-agent/src/discovery.rs:71-110：`terms()` 按 `!is_alphanumeric` 切分，连续的汉字成为一个整 token，查询 "搜索记录" 只能匹配包含这四个连续字的 token。打分只是命中的查询词数量，name 和 description 权重相同，没有名称精确匹配加成。

设计：terms 对 CJK 段生成重叠的 2-gram（和 db.rs 里 rupi_cjk_ngrams 的思路一致），先做 NFC 规范化（unicode-normalization crate）。打分改为 name 命中权重 3、description 命中权重 1、整个名称出现在查询里加 10，排序依据是命中词数，其次是加权分数。

风险：n-gram 会增加误匹配，需要用排序阈值压住。

## 不借鉴
- REFUTED: 缓存那一条原第 (1) 点「把 `<Recalled>` 移出 system，保住云端前缀缓存」。云端唯一注册的外部记忆是 PostgresMemory（run.rs:268-274），它没有实现 prefetch，走 rupi-memory/src/lib.rs:608 的默认空串，所以云端每轮 recalled 恒为空，system 不会因此变化。这一点只对本机 JsonlMemory 有意义，云端收益为零，已降为本机附带项。
- REFUTED: 结构化摘要那一条原文「摘要请求在 Anthropic 下为一次性请求付缓存写溢价」。摘要请求 tools 为空、system 只有几十 token，前缀低于最低可缓存长度（视模型 512 到 4096 token），cache_control 被静默忽略，不产生写入费用。只有在缓存那一条给 messages 加断点之后，才需要 CachePolicy::None。
- REFUTED: 结构化摘要那一条原文「摘要失败没有单独的重试」。complete() 走 post_json_with_retry（rupi-llm/src/lib.rs:597、anthropic.rs:634），429/5xx/传输错误会重试 3 次。真正缺的是溢出错误处理和失败时不覆盖旧摘要。
- REFUTED: 用量记账那一条原文「review.rs:323 和 subagent.rs:260-271 的用量漏计属于云端计费缺口」。rupi-server 没有挂 reviewer，也没有注册 subagent 工具（crates/rupi-server/src 内 grep with_reviewer/subagent 无结果），云端今天只有压实一处漏计。
- REFUTED: 会话设置那一条原文「按会话选 thinking 不生效」。sess.thinking_level 已经应用到 AgentLoop（run.rs:376-382），不生效的是 sess.model 和租户级 thinking/compaction/tools/base_url 等键。
- REFUTED: 工作区资源那一条原文「`**/SKILL.md` 会扫进 node_modules 和 vendor、读文件没有单文件大小上限」。glob 走 ignore_walker(standard_filters(true))（rupi-tools/src/lib.rs:1176-1180），git 仓库里遵守 .gitignore；fs_read 经 ReadTool 截到 12000 字符（lib.rs:1398、572）。实际问题相反：隐藏目录被过滤，`.rupi/commands`、`.agents/skills`、`.pi/skills` 在云端永远发现不了（scratch 验证 /tmp/rupi-review-glob）。
- REFUTED: 切点那一条原文「溢出恢复（lib.rs:766-778）会再 force_compress 一次，可能再切到 Tool 上」。孤立 tool_result 的 400 不匹配 is_overflow_error（rupi-llm/src/overflow.rs:69），直接返回 Err，不进恢复；后果是会话长期卡死，而不是多压一次。
- TUI 和终端相关：默认全屏、OSC 7501 程序状态、OSC 52 剪贴板、Rex/Herdr/Kitty 内联图片、system 主题和 OKHSL 配色、keyText()/keyHint() 注入、/model 和 /resume 模糊搜索排序、HTML 导出空白页和 CJK 粗体修复、Shift+Enter 和 Apple Terminal 适配。纯本机交互层，云端前端走 AG-UI，不共享这些代码。
- 分发和安装：pi update、selfUpdateCommand/PI_SELF_UPDATE_COMMAND、managed install、Nix flake、扩展包 changelog 展示、npm lockfile 嵌套修复。SaaS 用镜像和制品发布，与用户本机升级流程无关。
- /bug 上传到 Radius 和 ~/.pi/agent/crashes.json：会把用户转录发给第三方。SaaS 用自己的 tracing 和错误上报，用户数据不出平台。
- /share 和 sessionShareDestination（gist/Radius）：在服务器上调 `gh gist` 或向第三方上传会话不可接受。云端分享应该是平台内的签名链接加 export 端点。
- Sign in with ChatGPT、Anthropic 订阅 OAuth、Copilot、Meta 设备码登录：这些是个人订阅凭证，多租户 SaaS 不能代用户使用（合规和 ToS 风险），只接受 BYOK API key 或平台签约的 key。
- `auth: {provider}` 把 agent 自己的 provider 登录 token 当作 MCP bearer（0.99.2）：在 SaaS 里等于把平台或租户的 LLM 密钥转发给第三方 MCP 服务器，必须禁止。
- MCP OAuth 的 loopback callback、以 pi.dev Client ID Metadata Document 作 client_id、`pi mcp login` 浏览器流程：云端需要托管 callback 和自有的 client 注册；只借鉴 iss 和 metadata URL 的校验规则。
- 进程内 TypeScript 扩展、registerToolRenderer、扩展 UI 对话框、before_provider_headers、provider_stream_event：租户代码不能在无状态控制面里运行。
- `-e npm:`/`git:` 每次启动或 /reload 自动拉新版（Unreleased #11375）：结果不可复现，有供应链风险。云端包必须固定 digest。
- codemode（QuickJS worker、PI_CODEMODE_MAX_WORKERS、models.generateImages/classify）：在控制面执行模型写的 JS 会破坏隔离边界。如果要用，属于 codemode/执行面领域，只能在 executor 沙箱内运行。
- Virtual models 和分类器路由（Jev/Clef/GPT-6 Luna）：上游通过扩展做路由，属于模型路由领域。如需降本路由，应做成服务端策略，不能靠租户扩展。
- project trust 的交互式提问和按目录保存的 trust.json：云端没有"当前目录的人类所有者"，用租户级策略替代（见工作区资源那一条）。
- JSONL 作为主存储、sessionDir/--session-dir/PI_CODING_AGENT_SESSION_DIR：Postgres 仍是权威存储，JSONL 只作为进出口格式。
- cache warming 的 idle 模式：在大量空闲会话上定时刷新缓存，成本随会话数线性增长。最多只借鉴运行中保温，并做成按租户开启。
- 上游已移除的实验性 server/client/plugin 栈（1.2.0 Removed）：上游自己放弃了，没有可借鉴的设计。
- Windows、WSL、Termux、PowerShell 路径和 shell 探测修复，fd/rg 自动下载：执行面镜像是固定的 Linux，不需要。
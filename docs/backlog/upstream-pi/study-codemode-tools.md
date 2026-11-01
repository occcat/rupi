# codemode-tools

## 上游
上游 1.x 新增了独立包 pi-codemode。它把模型写的 JavaScript 放进编译成 WASM 的 QuickJS VM 里执行，VM 唯一的能力是调用注入的工具。嵌套工具调用不进入 LLM 上下文，只有脚本的输出和返回值进入（codemode.README.md:3-5, 47-56）。脚本环境提供 tools.<name>(args)（返回 Promise，参数和结果做一次 JSON 往返）、text/image/console/exit/store/load 和 ALL_TOOLS；没有 timer、fetch、require 和 WebAssembly，内建对象在脚本运行前冻结（README:47-56；codemode.CHANGELOG 1.0.4）。资源边界有：timeoutMs（默认 300s）、memoryLimitBytes、输出上限 16 Mi 字符或 10 万项、store 单值 256 Ki 且总量 1 Mi；执行失败时 store 写入不生效（types.ts:111-132；README:80-92, 347-356）。在 Node/Bun 上，每次 execute 起一个 worker 线程（约 20ms），deadline 和 abort 由宿主持有；acquireWorker/onWorkerStarted 让宿主把 worker 一直计入并发额度，直到它真正退出（README:64-78, 362-368；CHANGELOG 1.3.0）。上游在 Cloudflare 上实测：QuickJS 同步计算 10 秒，会把同一个会话 Durable Object 的其它请求全部卡住 10 秒。因此 RemoteCodemodeSandbox 只把 VM 放进单独部署的 Worker，工具和调用记录留在会话一侧，每次工具调用通过 RPC 回到会话（README:138-145, 214-232）。sandbox.start() 提供确定性运行：虚拟时钟、seed 随机数、固定 UTC、没有 WeakRef，宿主按序号投递结果，VM 指纹可校验，用于崩溃后重放（types.ts:165-243；README:316-343）。durable 包在此之上把 codemode 做成可恢复：run 文档记录启动数据，journal 每投递一个结果提交一次，嵌套调用以 key c<id> 复用已提交的结果；脚本走了不同路径、版本或指纹变化时拒绝重放（durable/src/codemode/codemode.ts:1-36, 410-449, 964-1004）。工具结果的映射方式：声明了 structuredOutputSchema 的工具给脚本结构化值，例如 bash 给 {output, truncated, fullOutputPath, exitCode}，MCP 给 CallToolResult；其它工具给文本。脚本拿到的 bash 输出最多 1 MiB，而模型只看到 2000 行或 50KB（codemode.ts:280-293；tools/bash.ts:20-26, 105；docs/codemode.md:43-48）。给模型的结果由 Script completed/failed 头、墙钟时间、分段文本和 console 块组成；超过 max_output_tokens（默认 10000）时保留头尾，全文写入临时文件；失败时列出已经执行、不会撤销的调用（output.ts:98-126, 154-172）。codemode 的工具描述固定不变。可调用工具以 TypeScript 声明列在一个 prompt section 里，预算 3000 token，每个命名空间轮流放入最便宜的工具，放不下的靠 searchTools/describeTool/describeNamespace 查找（describe.ts:22-23, 96-157）。MCP 默认 exposure 为 codemode，不向模型声明，所以 MCP 服务器连接或断开不会改写缓存前缀（docs/mcp.md:193-206；coding-agent CHANGELOG:422, 437-439）。tool_search 使用 BM25（k1 1.2，b 0.75），名字命中权重 3、命名空间权重 0.5，整名命中额外加分，支持 CJK 分词和简易词干；它的描述也固定，命中的工具写入会话的 modelTools，压缩和分支后仍然保留（search.ts:15-164；tool-search.ts:1-5, 14-15, 63-86）。嵌套调用走完整的工具管线：经过 hooks，事件带 parentToolCallId，用量计入父调用。调用记录有上限：256 次、单次参数 8KB、总计 32KB（coding-agent/src/core/nested-tool-calls.ts:26-31, 161-250；durable docs/spec.md:3684-3740）。callers 字段决定一个工具能否被模型调用、能否被其它工具调用（spec.md:3649-3656）。并发方面，一个脚本内的 models.* 付费调用最多 4 个同时进行；一个进程内的 codemode worker 默认最多 2 个，排队最长 60 秒（models.ts:43；docs/codemode.md:93, 228）。另外，pi-ai 在发请求前统一做一次消息变换：给没有结果的 tool call 补一条 isError 的 "No result provided" 结果，并把夹在 tool call 和结果之间的 system 消息挪到结果之后（packages/ai/src/api/transform-messages.ts:179-250，经 gh api 取得，评审时已核对）。

## rupi 现状
rupi 没有 codemode 或任何脚本工具，Cargo.lock 里也没有 rquickjs、boa、wasmtime 或 javy。docs/UPSTREAM.md:86 写了"不移植：WASM"，那条针对的是扩展。

工具抽象：Tool trait 只有 execute 和 execute_with_cancel（crates/rupi-tools/src/lib.rs:74-89）。ToolOutput 只有文本 content、is_error、images 和 ui_hint（:37-43）。ToolDefinition 没有 output schema（crates/rupi-core/src/lib.rs:736-743）。注册表是 HashMap（rupi-tools/src/lib.rs:91-94），definitions() 按哈希顺序输出（:119-121）。

渐进发现：discovery.rs 按查询词在名字和描述 token 里的子串命中次数排序（:71-112），常驻工具 9 个（:10-26）。命中的工具名只存在 AgentLoop 内存里的 HashSet（crates/rupi-agent/src/lib.rs:229-230, 477-481），不写入会话树。云端没有开启发现（crates/rupi-server/src/run.rs:363-369）。

截断：本机 bash 用 PipeTail 尾环加 bound_bash_output，上限 2000 行或 50KB（rupi-tools/src/lib.rs:883-884, 908, 1020-1056, 1500-1513）；read 用 truncate_middle，上限 12000 字符（:574, 1398）；超出部分不落盘。execd 的 read/glob/grep 走 with_sandboxed_builtins（crates/rupi-runtime/src/engine.rs:371；rupi-tools/src/lib.rs:197-222），已有上限。云端 bash 完全不截断：ExecBashTool 直接拼接 stdout 和 stderr（crates/rupi-server/src/tools.rs:255-284），Engine::exec_for 用 wait_with_output 把输出全部读进内存（engine.rs:320-323），execd 和 sandboxd 都走这条路径（execd.rs:246-261；sandbox.rs:264-279），控制面 HTTP 客户端用 resp.text() 读完整响应体（crates/rupi-runtime/src/http.rs:36-45）。snapshot/restore 也走同一个 post()，把整个 tar 包 base64 后放在 JSON 里（http.rs:284-319）。

持久化：persist_tree 每次先 DELETE 整个会话的 messages 再全部重插，content 和 blocks 各存一份（crates/rupi-server/src/db.rs:1065-1104）；finish 还把整棵树 JSON 写进 Redis（run.rs:722-729）。每个 run 都调 load_remote_guidance，把 AGENTS.md/CLAUDE.md 和 commands/*.md 作为 Role::System 消息追加进会话树（run.rs:312, 883, 958），同时拼进系统提示（run.rs:370-375）。树会落库，所以每个 run 都会多存一份这些内容。

MCP：McpManager 把远端工具注册成名为 {server}_{tool} 的原生工具（crates/rupi-mcp/src/lib.rs:1389-1400）。call_tool 解析了 structuredContent（:879），但 McpToolExecutor 把它丢掉（:1295-1304）；list_tools 不读 outputSchema。rupi-server 不依赖 rupi-mcp（crates/rupi-server/Cargo.toml:21-28），云端没有 MCP。

云端工具执行：每次 run 新建一个 cloud_tools 注册表，里面是 read/write/edit/glob/grep/bash/think 共 7 个工具，没有 subagent（run.rs:310；tools.rs:13-43）。工具默认串行（rupi-agent/src/lib.rs:119-124），云端没有设置 Parallel。每次 fs 或 bash 调用都是一次控制面到 execd 的 HTTP 请求。write、edit、bash 走 Ask 再 Interrupt（run.rs:364-368），AgentLoop 只有一个 PendingInterrupt 槽（rupi-agent/src/lib.rs:237, 921-932）。数据库和 AG-UI 协议本身支持一次挂起多个 interrupt（run.rs:108-124 要求 resume 覆盖全部；:486 是数组）。请求组装前没有任何"补齐孤立 tool_use"的清洗逻辑（rupi-llm、rupi-core、rupi-agent 里都没有）。

缓存与计量：Anthropic 只在 system 和最后一个工具上挂 cache_control（crates/rupi-llm/src/anthropic.rs:72-94），消息上没有断点；Bedrock 没有任何断点，system 是字符串（bedrock.rs:57-75）。配额只累加 input+output（run.rs:339-348），丢掉 Usage 里的 cache_read/cache_write。云端 PostgresMemory 没有覆盖 prefetch（crates/rupi-server/src/memory.rs 无该方法，默认返回空，rupi-memory/src/lib.rs:608-610），所以云端系统提示里的 <Recalled> 段是空的。

观测：Metrics 只有 runs 和 reject_429 两项（crates/rupi-server/src/lib.rs:36-40）。ToolStart 事件没有 parent 字段（rupi-core/src/lib.rs:466-473），AG-UI 的 TOOL_CALL_START 也没有（crates/rupi-server/src/agui.rs:218-224），并且 agui.rs:1 规定不得用 CUSTOM/RAW 事件。AgentEvent 通道容量 256，满了就丢事件（run.rs:26, 54-57）。

## 提案
### 云端 bash 输出限长，超出部分写入工作区文件 [critical/M]
上游：/tmp/pi-upstream-1793465092/pkg/env/daemon/src/exec.rs:129-190（Rust 实现的 Spill：超过阈值后把完整原始输出追加写文件）；pkg/durable/src/tools/bash.ts:20-26, 105, 127-135；pkg/durable/src/truncate.ts:12-13；docs/codemode.md:43；coding-agent.CHANGELOG.md:521

差距：已核实。crates/rupi-server/src/tools.rs:271-283 的 ExecBashTool 原样拼接 stdout+stderr，不做截断；interrupt 恢复路径 run.rs:640-663 也一样。crates/rupi-runtime/src/engine.rs:320-323 用 wait_with_output 把全部输出读进内存，超时上限 300s（:303）；execd 和 sandboxd 都调这个函数（execd.rs:246-261；sandbox.rs:264-279）。crates/rupi-runtime/src/http.rs:38 的 resp.text() 不限大小。云端只有 bash 无界：execd 的 read/glob/grep 走 with_sandboxed_builtins（engine.rs:371），已有 12000 字符和 200 条上限。放大效应比原文写的更大：persist_tree 每次 finish 都先 DELETE 整个会话再全部重插，content 和 blocks 各一份（db.rs:1073-1104），所以一次大输出在之后每个 run 都会被重写两份；finish 还把整棵树写进 Redis（run.rs:725-729）；每个 run 开始时把整棵树作为 MESSAGES_SNAPSHOT 推给客户端（run.rs:317）。本机 BashTool 有 PipeTail 尾环（crates/rupi-tools/src/lib.rs:883-884, 1020-1056），云端路径没有用上。

设计：(1) 在 rupi-runtime 的 Engine::exec_for 里改成 spawn 后并发读 stdout/stderr 管道。复用 rupi-tools 的 PipeTail（改为 pub，或移到 rupi-runtime），只保留最后 50KB。这一处同时覆盖 execd 和 sandboxd。照上游 Spill 的语义，输出越过 50KB 或 2000 行后，把完整原始输出追加写到工作区的 `.rupi/out/<uuid>.log`。注意快照会 tar 整个目录（engine.rs:442-449），而且整个 tar 包要在两端内存里 base64 后走 JSON（http.rs:284-319），所以落盘文件不能无限累积：每会话最多保留最近 8 个文件、总量不超过 32 MiB，超出时删最旧的；单文件超过上限后停写并在结果中标注。快照时用 `tar --exclude=./.rupi/out` 排除这个目录，结果里注明路径在闲置回收后会失效。(2) ExecResult 增加 truncated、total_bytes、full_output_path 三个字段，都加 serde default，新旧 execd 和控制面可以混跑。(3) 控制面 ExecBashTool 和恢复路径的 bash 再套一层 bound_bash_output，防止旧 execd 返回超长结果；结果末尾写 `[full output: <path>, read with offset/limit]`。(4) 响应体上限只能按端点加，不能加在通用的 post() 里：snapshot/restore 用的是同一个 post()（http.rs:284-319），全局 4 MiB 上限会让任何超过约 3 MiB 的工作区无法闲置快照。做法是给 exec 和 fs 端点单独实现一个带上限的读取（bytes_stream 累加，超过 4 MiB 就报错）。(5) Metrics 增加 bash_output_truncated_total 计数。不需要改 Postgres schema。

风险：落盘文件占用租户磁盘，需要按会话清理，或计入工作区配额。jail 目前没有 rlimit 或 cgroup（rupi-runtime/src 里搜不到 RLIMIT/cgroup），`yes > f` 一样能写满磁盘，这一项修不了这个问题，要另外做磁盘配额。路径必须在 jail 根目录内，并防止符号链接逃逸（复用 SandboxedTool 的规则）。保留尾部会丢掉开头的报错上下文；上游同样保留尾部，结果里会写明总行数。

### 中断恢复会留下没有结果的 tool_use：批内多调用和 AGENTS.md 两种触发方式 [critical/M]
上游：/tmp/pi-upstream-1793465092/pkg/agent/src/agent-loop.ts:492-510, 620-663（每个 toolCall 都产出 toolResult，中止的调用也给错误结果）；packages/ai/src/api/transform-messages.ts:179-250（发请求前给孤立 tool call 补 "No result provided"，并把夹在中间的 system 消息挪到结果之后，经 gh api 取得）；docs/codemode.md:18

差距：已核实，而且问题比原文写的大。触发方式一：crates/rupi-agent/src/lib.rs:882-987 的第一阶段逐个调用过门，遇到 AskAction::Interrupt 就在 :921-932 直接 return。这时助手消息已经入树（:814），在它之前已放行的调用不执行、没有 ToolResult；在它之后的调用连 ToolStart 都没发。PendingInterrupt 只有一个槽（:237），run.rs:438-451 只插一条 interrupt 并落库。恢复时 run.rs:528-684 只给这一个调用补 ToolResult，然后 continue_session。触发方式二：即使只有一个调用，恢复也会出错。每个 run（包括恢复 run）在 run.rs:312 调 load_remote_guidance，它把 AGENTS.md/CLAUDE.md 和 commands/*.md 作为 Role::System 消息 push 进树（:883, :958），这一步发生在 resume_and_continue 补 ToolResult 之前（:384）。于是历史变成 assistant(tool_use) → system(AGENTS.md) → tool(result)。OpenAI 映射会在中间插一条 system 消息（rupi-llm/src/lib.rs:272）；Anthropic 映射把 System 转成 user 文本再和 tool_result 合并（anthropic.rs:134-153），合并后的 content 是 [text, tool_result]，文本在前。按 OpenAI 和 Anthropic 文档的配对规则，这两种请求都会被拒绝（tool 消息必须紧跟 tool_calls；tool_result 必须排在 user 消息 content 的最前面）。rupi 发请求前没有任何清洗逻辑（rupi-llm、rupi-core、rupi-agent 里都没有）。tests/cloud_first_cut.rs:290-350 的恢复测试没有写 AGENTS.md，而且 MockProvider 不校验配对，所以测试能过。以上结论来自读代码，没有对真实 provider 复现。顺带一个问题：load_remote_guidance 每个 run 都往树里追加一份 AGENTS.md，同时又把它拼进系统提示（run.rs:370-375），N 个 run 之后提示里会有 N+1 份。

设计：分三层修。(a) 发请求前清洗，放在 rupi-llm，照搬上游 transform-messages.ts:179-250：对每条带 tool_use 的助手消息，把它和下一条 user 消息之间的 System/User 文本挪到 tool 结果之后，给缺结果的调用补 is_error 的 "No result provided"。这一层兜住所有路径，也能修复 Postgres 里已经坏掉的会话，不用人工处理。(b) AgentLoop：pending_interrupt 改成 Vec<PendingInterrupt>。第一阶段遇到 Interrupt 时记录下来并 continue，不再 return。批内其它调用统一合成错误结果，例如 "not executed: this batch is paused for approval; re-issue if still needed"，不执行任何调用，保持"中断时不动工作区"的现有语义。这些结果写成一条 Role::Tool 消息，紧跟在助手消息后面，然后 return Aborted。run.rs:438 改为每个挂起的调用插一行 interrupt。数据库和 AG-UI 已经支持多个 interrupt（run.rs:108-124, :486），恢复路径的循环本来就遍历全部。(c) load_remote_guidance 不再往树里 push System 消息，只拼进 builder.append。这样既去掉了每个 run 一份的重复，也不会把消息插到 tool_use 和 tool_result 之间。测试：仿照 tests/cloud_first_cut.rs，Mock 返回 [read, write, bash]，并在工作区写一个 AGENTS.md；给 MockProvider 加配对校验断言。

风险：合成的错误结果会让模型重发 read 之类的调用，多花一轮 token。也可以选择先执行批内的只读调用，但这样要按 is_mutating 区分，而且会改变中断语义，建议先不做。清洗层会掩盖 loop 里的新 bug，所以每次补结果都要记 warn 日志并计数（metrics: orphan_tool_calls_repaired_total）。去掉 guidance 入树后，依赖树里 "[workspace AGENTS.md]" 消息的测试和 UI 需要调整。

### 让缓存前缀稳定：工具按确定顺序排列，新发现的工具追加在末尾 [high/S]
上游：/tmp/pi-upstream-1793465092/pkg/durable/src/tool-search/tool-search.ts:14-15（描述固定，不随 MCP 服务器连接而变）；docs/mcp.md:204（服务器摘要变化时追加到会话，不改工具声明）；coding-agent.CHANGELOG.md:437-438；ai.CHANGELOG.md:69（重写工具列表导致丢缓存）、:741（addedToolNames 标记新增工具）

差距：已核实。ToolRegistry 用 HashMap（crates/rupi-tools/src/lib.rs:91-94），definitions() 按哈希顺序输出（:119-121）。std 的 RandomState 每个实例的种子不同，所以同一进程里新建的两个 HashMap 迭代顺序也可能不同。云端每次 run 新建注册表（crates/rupi-server/src/run.rs:310）。这个顺序同时决定请求里的 tools 数组（anthropic.rs:276-287；rupi-llm/src/lib.rs:360-370）和系统提示的 <AvailableTools> 段（crates/rupi-agent/src/lib.rs:98-104）。同一个 run 内 all_tools 只计算一次（:653），所以顺序变化发生在 run 与 run 之间。原文说系统提示里每轮都会变的 <Recalled> 段（rupi-agent/src/lib.rs:697-699）也是问题，这在云端不成立：PostgresMemory 没有覆盖 prefetch，默认返回空（rupi-memory/src/lib.rs:608-610），云端这一段始终为空，只影响本机 CLI。云端另一个会改动系统提示的来源是冻结记忆：它每个 run 从 Redis 或 Postgres 重新读（run.rs:273-300），finish 时又会失效（run.rs:744），租户写入记忆后的下一个 run 前缀会变。这属于记忆区，这里只记录。

设计：ToolRegistry 改用 BTreeMap，或者 definitions() 和 names() 返回前按 name 排序。AgentLoop 组装 all_tools 时保持"内建、记忆、skill"的分组，组内按名字排序。开启发现时，新发现的工具按发现顺序追加到末尾（discovered 改成保序的 Vec），不再按 all_tools 原有位置插入。search_tools 的描述保持固定。加一个单测：两个独立构建的注册表，definitions() 序列化后逐字节相同。观测：Usage 事件已经带 cache_read（run.rs:339-344），map_task 里顺手按租户累加 cache_read/input，导出缓存命中率指标，否则以后前缀被改坏也没人发现。

风险：依赖现有顺序的快照测试需要更新。Anthropic 的断点目前只在 tools 和 system 上，单做这一项对 Anthropic 的收益有限，要配合下一条。

### 给 Anthropic/Bedrock 的消息加缓存断点，并修正缓存 token 的配额计量（跨 rupi-llm 区） [high/S]
上游：/tmp/pi-upstream-1793465092/ai.CHANGELOG.md:636（缓存断点随 tool result 向后推进）、:183（inline-tools beta 在工具变化时保住缓存）、:262, :976（缓存写入计价修正）

差距：已核实。crates/rupi-llm/src/anthropic.rs:72-94 只在最后一个工具和 system 上挂 ephemeral 断点；to_anthropic_messages（:128-239）不给任何消息块挂 cache_control。crates/rupi-llm/src/bedrock.rs:57-75 没有任何断点，system 是字符串。原文留下的计量问题可以回答：Anthropic 的 input_tokens 只是未命中缓存的部分，不含 cache_read 和 cache_creation；而 run.rs:339-348 只累加 input+output，用 `..` 丢掉了 cache_read/cache_write。所以开启消息缓存后，Anthropic 租户被缓存的 token 在配额里记为 0，缓存写入的 1.25 倍溢价也不计。OpenAI 的 prompt_tokens 包含 cached_tokens，会按全价计入。两类 provider 的计量口径不一致。

设计：(1) 在 anthropic.rs 的 body() 里保留 tools 和 system 两个显式断点，再给 messages 最后一条消息的最后一个 content item 加 {"cache_control":{"type":"ephemeral"}}。也可以改用请求顶层的自动 cache_control，它会自动放在最后一个可缓存块上并随会话前移，占 4 个断点中的 1 个。工具循环每轮只追加 2 到 3 个位置（tool_use 和 tool_result 各自合并成一个位置），不会超出 20 块回看窗口。(2) Bedrock 同样处理，把 system 从字符串改为带断点的数组。旧版 Bedrock 集成（Opus 4.6 及更早）不接受顶层 cache_control，那里只能用显式断点。(3) 计量：quota_ledger 增加 cache_read、cache_write 两列，或者按权重折算后计入，例如 input + 0.1×cache_read + 1.25×cache_write + output。OpenAI 路径要从 prompt_tokens 里减去 cached_tokens，把口径统一成"未缓存输入 + 缓存读取 + 缓存写入"三项。用量解析已经支持 cache_read 和 cache_write（rupi-llm/src/lib.rs:168-209）。

风险：缓存写入有约 1.25 倍溢价，极短的会话可能略贵。断点最多 4 个。配额口径变化会影响已有租户的额度判断，需要和计费一起上线。这条属于 rupi-llm 区，可能和其它区的提案重复。

### 嵌套工具调用管线（codemode 的前置条件） [medium/M]
上游：/tmp/pi-upstream-1793465092/pkg/coding-agent/src/core/nested-tool-calls.ts:26-31（记录上限）, 47-101, 161-250；pkg/durable/docs/spec.md:3649-3656（replay/callers）、3684-3740；codemode.README.md:312（直接调用 tool.execute() 会绕过 hooks）；coding-agent.CHANGELOG.md:489, 539（嵌套调用的用量计入会话成本）

差距：缺口属实，但云端今天没有任何"工具调工具"的路径：cloud_tools 只有 7 个工具，没有 subagent，也没有 MCP（crates/rupi-server/src/tools.rs:13-43；Cargo.toml:21-28）。所以云端目前不存在原文说的权限绕过，这一项的价值完全取决于是否做 codemode。本机已有一个现成的绕过实例：subagent 的子循环继承了 policy 和 approver，但没有继承 hooks（crates/rupi-agent/src/subagent.rs:250-256）。放行判断（before 钩子、策略、Ask/Interrupt、计划模式）内联在 run_turns 第一阶段（crates/rupi-agent/src/lib.rs:882-987），执行在 execute_allowed（:455-525），after 钩子和事件在第三阶段（:1050-1087）。Tool::execute_with_cancel 只拿到参数和 CancelFlag（crates/rupi-tools/src/lib.rs:82-88）。ToolStart/ToolEnd 事件没有 parent 字段（crates/rupi-core/src/lib.rs:466-473），AG-UI 的 TOOL_CALL_START 只有 toolCallId 和 toolCallName（crates/rupi-server/src/agui.rs:218-224）。

设计：rupi-agent 新增 nested.rs。把 hooks、policy、plan_mode 的判断从 run_turns 抽成 ToolGate，返回 Allow/Deny/Ask。新增 NestedRunner，持有 gate、tools、mem、skills、recorder、parent_id、cancel 和 limits，提供 async fn call(name, args) -> NestedOutcome。嵌套调用的 id 为 `<parent>/<n>`。嵌套上下文里的 Ask 一律转成错误（"needs approval; call it directly"），不触发 Interrupt。工具定义增加 callers 字段，取值 model、tools 或 both，默认 both；search_tools、codemode、subagent 设为只允许模型调用，防止递归。AgentEvent::ToolStart/ToolEnd 增加 parent_tool_call_id: Option<String>（serde default 加 skip_serializing_if，兼容旧的 RPC 事件）。AG-UI 侧不要把嵌套调用映射成 TOOL_CALL_START：AG-UI 的 parentMessageId 指的是消息，不是父工具调用，客户端还会把这些调用当成模型发出的调用，到消息历史里找它们的结果；agui.rs:1 又禁止 CUSTOM 事件。建议只在父调用结束时把汇总挂进父结果，过程用已有的 STEP_STARTED/STEP_FINISHED（agui.rs:276-296）表示，细节走 tracing span。Recorder 按上游上限（256 次、单次参数 8KB、总计 32KB、错误文本 500 字符）把调用记录挂在父 ToolResult 上：ContentBlock::ToolResult 增加可选的 details 字段，persist_tree 本来就把整块存成 JSON（db.rs:1089）。subagent 顺手改成继承 hooks。

风险：要改 run_turns 的核心路径，回归面大。ContentBlock::ToolResult 加字段会影响所有按字段解构它的 provider 映射和旧会话 JSON 的兼容。事件结构变化会影响 TUI 和 RPC 的消费方。嵌套事件可能远多于顶层事件，而 AGENT_EVENT_CAP=256 的有界通道满了就丢事件（run.rs:26, 54-57），丢掉 ToolEnd 会让 AG-UI 映射器留下未关闭的调用，所以嵌套事件要合并或降采样。

### codemode 沙箱脚本工具：QuickJS 编译成 WASM，VM 放在执行面或独立进程池，由控制面按 start/deliver 步进驱动 [medium/L]
上游：/tmp/pi-upstream-1793465092/codemode.README.md:3-5, 47-62, 64-78, 138-145, 316-343, 347-374；docs/codemode.md:15-58, 224-230；pkg/codemode/src/types.ts:14-41, 94-132, 165-243；pkg/durable/src/codemode/codemode.ts:230-231, 280-293, 824-847, 899-909；describe.ts:96-157；output.ts:98-172；codemode.CHANGELOG.md 1.0.1、1.0.4、1.3.0

差距：rupi 没有任何脚本或 VM 能力：Cargo.lock 里没有 rquickjs、boa、wasmtime，docs/UPSTREAM.md:86 写明不移植 WASM。云端模型只能一次调一个 read/grep/glob，每个 read 结果最多 12000 字符进入历史（crates/rupi-tools/src/lib.rs:574, 1398）；bash 需要人工审批（crates/rupi-server/src/run.rs:364-368）；工具串行执行（run.rs:363-369 没有设 Parallel）。所以"循环读 20 个文件再过滤"这种任务在云端要么多轮 LLM 往返，要么多次人工审批。

设计：新建 crate rupi-codemode，guest 与宿主协议和运行位置分开设计。
(1) 引擎：把 QuickJS 编译成 wasm32-wasip1（用 rquickjs 写一个小 guest，或复用上游的 quickjs-wasi 模块），由 wasmtime 托管：每次执行新建 Store/Instance（pooling allocator，预编译 Module 进程级缓存）；StoreLimits 把内存限在 64 MiB；consume_fuel 计量 CPU，同时作为计费单位；epoch_interruption 做墙钟截止。只导入一个 host_call，外加确定性的时钟和随机数（固定 seed 和 startedAt，给将来的重放留接口）；不提供 WASI 的 fs、net、env。脚本运行前冻结内建对象（上游 1.0.4）。
(2) 协议：guest 导出 start(code)、deliver(seq, id, json)、drain()，语义照上游 types.ts:165-243。VM 跑到所有 Promise 都在等宿主结果时，返回一批待处理调用；宿主并发执行（每个脚本同时最多 8 个），按序号投递结果，再继续。这样保留 Promise.all 的并行。
(3) 运行位置：原文把 VM 放在控制面进程内，这和上游自己的结论相反。上游实测 VM 计算会卡住同一会话宿主上的其它请求（README:140-145），而 rupi 的控制面就是会话宿主，同时服务多个租户的 SSE。spawn_blocking 只能避免卡住 tokio，避免不了多租户之间抢 CPU 和内存（64 MiB × 并发脚本数），控制面也会从 I/O 型变成要按 CPU 扩容。建议把 VM 放在执行面：execd/sandboxd 增加 /v1/codemode/start 和 /v1/codemode/deliver，每次响应返回 {pending_calls, output?, done}，VM 状态挂在会话已有的 handle/lease 上。原文否定执行面的理由是"需要给 execd 一条带凭证、能回调控制面的通道"，这个理由不成立：步进协议由控制面发起每一次请求，execd 只在响应里返回待处理调用，不需要回调；放行判断、钩子、记忆和配额仍然全部在控制面。这正是上游 RemoteCodemodeSandbox 的划分（VM 在远端，工具留在会话一侧）。每批嵌套调用多一次同区域 HTTP 往返，毫秒级。如果不想把 VM 放进租户工作区节点，也可以用独立的 rupi-codemoded 进程池，协议相同。
(4) 工具面：codemode 工具只允许模型调用，参数为 {code}。脚本里的 tools.<id>(args) 交给嵌套管线提案里的 NestedRunner，走同一套 hooks 和策略。云端默认只放行 read、glob、grep、think、memory_search 等不改动工作区的工具；write、edit、bash 返回"需要审批，请直接调用"。结构化结果来自"结构化工具输出"那条提案。全局提供 text、console、return、exit、ALL_TOOLS，以及 searchTools/describeTool（复用 tool search 那条提案里的 BM25）。禁止嵌套调用 codemode、search_tools 和 subagent。
(5) 限额：脚本墙钟默认 60s，上限 300s，与 exec 的 clamp 一致（engine.rs:303）。嵌套调用总数不超过 256。VM 内输出不超过 1 MiB。给模型的输出不超过 max_output_tokens（默认 10000），超出时保留头尾，全文写到工作区 `.rupi/out/codemode-<id>.txt`，复用第一条提案的清理和快照排除规则。每节点 VM 槽位用 semaphore 限制，每租户并发脚本数记在 Redis，沿用 rl_conc_key 的做法（cache.rs:162）；槽位一直占用到 VM 真正退出为止（上游 acquireWorker 的原则）。
(6) 结果格式照搬 output.ts：Script completed/failed 头、墙钟时间、分段文本、console 块；失败时列出已执行的调用，并说明这些调用不会撤销。
(7) 观测：每次执行一个 tracing span（带 tenant、session、run、tool_call_id），嵌套调用各一个子 span。Metrics 增加 codemode_runs_total{outcome}、nested_calls_total{tool}、script_fuel、script_wall_ms、output_truncated_total。fuel 和嵌套调用次数写进父 ToolResult 的 details，并进审计。
(8) 开关：租户 settings 增加 codemode: off|on，默认 off。先在内部租户和 eval 上比较每任务的 token 数、往返次数和成功率，再逐步放量。boa 是纯 Rust 实现，可以作为备选：它内存安全，但据我所知没有堆内存上限，性能也更低。

风险：成败取决于模型写脚本的质量，非前沿模型尤其如此，需要先过 eval。工作量 L 不含两项前置（嵌套管线 M、结构化输出 M），合计接近 XL。QuickJS 是解释器，重计算慢（上游 README:374）。fuel 和 epoch 检查有运行开销。guest 的构建链（wasm32 目标、用 C 编译 QuickJS）会增加 CI 复杂度。VM 放在执行面后，执行节点重启或 lease 丢失会让脚本中断，已执行的嵌套调用不会撤销；这和现在 run 中途崩溃的语义一致（run 由 reap_stale_runs 回收，reclaim.rs:15），第一阶段不做重放。步进协议让 VM 在执行面有状态，必须绑定到会话已有的 handle，并在 abort 或闲置回收时销毁。嵌套事件可能大量涌向 AG-UI。采用后要更新 docs/UPSTREAM.md:86 里"不移植 WASM"的说明。另有一个替代方案需要决策：按租户策略在 sandboxd 隔离下自动放行只读 bash。它成本更低，但无法静态判断一条 bash 是否只读。

### 结构化工具输出：bash 和 MCP 给脚本返回结构化值 [medium/M]
上游：/tmp/pi-upstream-1793465092/pkg/durable/docs/spec.md:3649-3660（structuredOutputSchema）；pkg/durable/src/tools/bash.ts:20-26, 127-135；docs/codemode.md:43-48；docs/extensions.md:2275；pkg/durable/src/codemode/codemode.ts:280-293；codemode.CHANGELOG.md 1.2.0

差距：ToolOutput 只有文本（crates/rupi-tools/src/lib.rs:37-43）。ToolDefinition 没有 output_schema（crates/rupi-core/src/lib.rs:736-743）。McpTool 不读 outputSchema（crates/rupi-mcp/src/lib.rs:55-59, 804-842）。McpToolResult.structured 已经解析出来（:879），但 McpToolExecutor 把它丢掉（:1300-1304）。bash 非零退出时被折叠成 "exit N: ..." 文本错误（rupi-tools/src/lib.rs:912；rupi-server/src/tools.rs:278-280）。

设计：ToolOutput 增加 structured: Option<serde_json::Value>，构造器默认 None，现有调用不受影响。ToolDefinition 增加 output_schema: Option<Value>，加 #[serde(default, skip_serializing_if)]；给 ToolDefinition 实现 Default，存量字面量批量改成 `..Default::default()`。McpTool 读取 outputSchema，McpToolExecutor 透传 structured。云端 ExecBashTool 产出 {output, exit_code, truncated, full_output_path}，依赖第一条提案。模型侧仍然只看 content；structured 只给嵌套调用方和 details 用。

风险：ToolDefinition 的字面量遍布全仓，迁移改动面大。structured 不进 LLM，但如果写进 Postgres，会让 messages 行变大；只在嵌套结果或 details 里保留，不进顶层 ToolResult。

### 云端只读工具并行执行 [medium/S]
上游：/tmp/pi-upstream-1793465092/pkg/durable/docs/spec.md:3678-3681（executionMode：一轮里只要有一个 sequential 调用，整轮就串行）；pkg/durable/src/tools/file-mutation-queue.ts:28-40（同一文件的 edit/write 串行）；pkg/coding-agent/src/core/nested-tool-calls.ts:202-213

差距：云端 AgentLoop 没有调用 with_tool_execution（crates/rupi-server/src/run.rs:363-369），默认是 Sequential（crates/rupi-agent/src/lib.rs:119-124, 260）。Parallel 已经实现（:1016-1047），云端没用。在云端，能走到第二阶段执行的只有非 Ask 的工具，因为 write、edit、bash 都在第一阶段就 Interrupt 了（run.rs:364-368）。

设计：云端加 agent.with_tool_execution(ToolExecution::Parallel)。为了防止将来租户自动审批放行会改动工作区的工具，在 ToolDefinition 或注册表上加 execution_mode: Parallel|Sequential，照上游语义：同一批里只要有一个 Sequential 工具，整批串行。execd 处理同一 handle 的 fs 请求时，每次都新建注册表（engine.rs:360-392），请求之间没有共享状态，可以直接并发。给并发度设上限，例如 8，防止单个会话打满一个执行节点。

风险：如果以后租户策略把 write、edit、bash 设为自动放行，必须同时上 execution_mode，否则对同一文件的并发 edit 会互相覆盖。执行节点的 QPS 峰值会上升。

### tool search 升级：BM25 排序、搜索参数文本、命名空间，并把发现状态存进会话 [medium/M]
上游：/tmp/pi-upstream-1793465092/pkg/durable/src/search.ts:15-70（BM25 k1 1.2、b 0.75；NAME_WEIGHT 3、NAMESPACE_WEIGHT 0.5；整名加分；精确名排第一）、:141-164（Unicode/CJK 分词、停用词、词干）；pkg/durable/src/tool-search/tool-search.ts:1-5（加载的工具写入会话的 modelTools，压缩后保留）、:31-53（搜索参数名和描述）；coding-agent.CHANGELOG.md:48-49；docs/mcp.md:193-206, 228

差距：crates/rupi-agent/src/discovery.rs:71-112 只按查询词在名字和描述 token 里的子串命中次数排序。没有 IDF，像 file 这样的通用词会命中一大片；不搜参数 schema；没有命名空间。原文说"terms 丢掉长度为 1 的 token（:74），单个汉字的查询会变成空查询"，这一点不对：:74 的 `t.len() > 1` 按字节计，一个汉字在 UTF-8 里占 3 字节，不会被丢掉。真正的问题是 CJK 字符算作 alphanumeric，按非字母数字切分时整句中文会变成一个长 token，只有完整子串命中才算分，实际上几乎匹配不到。命中的工具名只存在 AgentLoop 内存里（crates/rupi-agent/src/lib.rs:229-230, 477-481）；云端每次 run 新建 AgentLoop（run.rs:363），发现状态不跨 run。云端没有启用发现，也没有 MCP（crates/rupi-server/Cargo.toml:21-28）。

设计：discovery.rs 换成 BM25，移植 search.ts。Rust 里用 unicode-segmentation 切词，对没有空格的 CJK 文本按单字或双字切分。搜索文本包含参数名和参数描述。工具名按 `__` 或 `_` 切出最后一段作为本地名，整名命中时加分。命名空间取 MCP server 前缀，即 rupi-mcp/src/lib.rs:1395 的 `{server}_`。已发现的工具集合写进会话：可以给 SessionTree 加 discovered_tools: Vec<String>（serde default），也可以放进 search_tools 结果的 details，run 开始时沿当前路径回放，这样分支和压缩后都一致。新发现的工具按第三条提案追加到工具列表末尾。codemode 脚本里的 searchTools 共用同一个排序器。

风险：SessionTree 结构变化需要兼容旧的会话 JSON。新增工具仍会改写工具前缀，造成一次缓存失效；上游靠 provider 原生的延迟加载规避，rupi 暂时不做。工具少的时候 BM25 的提升不明显。

## 不借鉴
- durable 的确定性重放 journal（pkg/durable/src/codemode/codemode.ts:1-36, 964-1004；docs/spec.md 第 7.6 节）：每个嵌套结果要 1 到 2 次存储提交，照搬到 Postgres 会成倍放大写入。rupi 的 run 本身中途不可恢复：会话树只在 finish 或 interrupt 时落盘（run.rs:451, 723-724），崩溃后由 reap_stale_runs 回收（reclaim.rs:15）。在 run 能恢复之前，单独让脚本可重放没有意义。先保留固定 seed 和时钟的接口，等 run 级持久化做完再议。
- Cloudflare 专用的形态：Durable Object、用 interrupt budget 代替计时器、InlineCodemodeSandbox（codemode.README.md:126-248）。rupi 是 Rust 进程，wasmtime 的 fuel 和 epoch 就能做同样的事。RemoteCodemodeSandbox 的划分原则（VM 不和会话宿主放在一起）要借鉴，见 codemode 提案的运行位置一节。
- Node/Bun 每次执行起一个 worker 线程，加上整个进程最多 2 个 worker 的限制（codemode.README.md:362；docs/codemode.md:228）：wasmtime 实例创建只需微秒到毫秒，用节点级 semaphore 加租户级 Redis 计数管理即可。只借鉴一条原则：额度一直占用到 worker 真正退出为止（acquireWorker）。
- 脚本里的 models.classify() 和 models.generateImages()（docs/codemode.md:60-222；pkg/durable/src/codemode/models.ts）：rupi 没有分类器或图像模型目录，脚本内的付费调用还需要单独设计配额和计费。
- Lark 语法约束采样（codemode.ts:245 的 CODEMODE_SOURCE_GRAMMAR）：只有 OpenAI 的 lark 变体支持，rupi 的 provider 层没有。
- image() 和超长输出全文写进本机临时目录（output.ts:128-152；codemode.ts:584-615）：控制面无状态，本机临时文件会随实例消失。应该写到执行面的工作区或对象存储。
- 上游偏宽的上限：VM 内存 256 MB、输出 16 Mi 字符（docs/codemode.md:15, 226），对多租户共享的节点太大。
- codemode.mode: "only"，即把所有工具都藏到脚本后面（docs/settings.md:43）：完全依赖模型写脚本的能力，先只做 on 模式。
- provider 原生的延迟加载工具（Anthropic inline-tools beta 的 tool_addition、OpenAI 的 additional_tools，见 ai.CHANGELOG.md:69, 183, 741）：仍是 beta，且每个 provider 要单独适配，维护成本高。rupi 先靠稳定的工具前缀和 codemode 来避免工具列表变动。
- MCP 的五档 exposure（direct、model-only、codemode、deferred、hidden，docs/extensions.md:2728-2736）：云端还没有 MCP，先只做 direct 和 deferred 两档。
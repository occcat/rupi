# Track: llm-wire-compat

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/llm-wire-compat`，worktree：`.worktree/llm-wire-compat`

## 目标
三种协议对当前一代模型的请求合法、流式解析不丢调用、usage 口径统一、网络异常有超时与可控重试。

## 负责文件
- crates/rupi-llm/**

不在本 track：其他 crate 不要改；如果公开类型（ChatRequest/ChatResponse/ContentBlock）必须变，优先用不破坏字面量构造的方式（例如 Thinking 块携带签名），实在要加字段就同时修完所有调用点并在报告里写明。

## 特别说明
- 103 Gemini 3 thoughtSignature：不要改 ContentBlock::ToolCall 结构（全仓 45 处字面量）。可以把签名存成紧挨在该 ToolCall 前面的 ContentBlock::Thinking{text:"", signature:Some(sig)}，回放时把签名写回对应 functionCall part。
- 104 Anthropic Opus 4.6+/Sonnet 5 系用 adaptive thinking + output_config.effort，旧模型才发 budget_tokens；模型不支持采样参数时不发 temperature；OpenAI 只对推理模型发 reasoning_effort。以官方文档为准（可用 WebSearch/exa 查证），查不到的不要臆造参数。
- 105 本仓 .env 的网关模型是 deepseek-v4-flash（OpenAI 兼容），可以用真实网关验证 reasoning_content 回放：source .env 后用 RUPI_BASE_URL=$BASE_URL RUPI_API_KEY=$API_KEY，绝不打印 key。
- 107 TokenUsage.input 统一为不含缓存的输入，cache_read 识别 cached_tokens、prompt_cache_hit_tokens。
- 125/126 合并 Gemini 与 Vertex、抽公共 SSE 循环属于精简，放在 bug 修完之后做，行为不变并保持测试全绿。

## 借鉴上游 Pi 的条目（来自对标 Pi v1.4.0 的分析，按 SaaS 价值排序）

### B9. 稳定缓存前缀：工具顺序确定、历史加缓存断点、cache key 和会话亲和 [critical/M]
本 track 范围：只做 provider 侧缓存：Anthropic/Bedrock 历史断点（总数 ≤4）；OpenAI prompt_cache_key 仅在 ChatRequest 已有的会话 id 或 provider 构造参数可用时发送

上游参考：packages/ai/src/api/anthropic-messages.ts:1526-1550（在最后一条 user 消息上打断点）；packages/ai/src/api/openai-prompt-cache.ts；pkg/durable/src/tool-search/tool-search.ts:14-15；ai.CHANGELOG.md 0.86.0 #9102 #9629、1.4.0 #10940；pkg/coding-agent/src/core/cache-stats.ts

差距：已核实。ToolRegistry 用 HashMap 存储，definitions() 按哈希顺序输出（crates/rupi-tools/src/lib.rs:91-94、117-121），而云端每个 run 都新建注册表（run.rs:310）。结果是不同 run 之间 tools 和 AvailableTools 的顺序随机，OpenAI 兼容网关（包括 deepseek）的前缀缓存基本命中不了。Anthropic 只在 system 和最后一个工具上打了断点（crates/rupi-llm/src/anthropic.rs:72-94），历史消息上没有；Bedrock 一个断点都没有。云端也从不设置 session_id 和 prompt_cache_key。

设计：阶段 1（S）：definitions() 按 name 排序，AgentLoop 合并工具后再整体排序一次；加单测，断言两次构建的序列化结果逐字节相同；之后新发现的工具一律追加到末尾。阶段 2（M）：ChatRequest 增加 session_id 和 cache_retention，云端填 hash(tenant_id, thread_id)。Anthropic 和 Bedrock 在最后一条消息上打断点，并保留上一轮末尾的断点，总数不超过 4 个。OpenAI 官方端点发 prompt_cache_key，兼容网关加 compat 开关。按 provider 导出缓存命中率指标，标签里不放 tenant。

### B10. LLM HTTP 超时、Retry-After 上限、jitter 和错误分类 [high/S]
本 track 范围：全部

上游参考：packages/ai/src/utils/provider-retry.ts:1-68（x-should-retry；408/409/429/5xx；等待超过 60 秒就失败；最多 25% jitter）；packages/ai/src/utils/retry.ts；pkg/coding-agent/src/core/http-dispatcher.ts:4；ai.CHANGELOG.md 1.2.0 #10609

差距：已核实。shared_http_client 没有设置连接超时和读超时（crates/rupi-llm/src/lib.rs:453-463）。上游连接卡住时，run 会一直占着租约、并发槽和执行器 handle。parse_retry_after 没有上限（lib.rs:865-868），服务端返回 3600 就真的睡一个小时，心跳期间还会照常续约。退避没有加 jitter（lib.rs:860-862），多个租户共用平台 key 时，重试会同步扎堆。insufficient_quota 这类错误也会被重试。

设计：设置 connect_timeout 10 秒；read_timeout 按块间空闲计算，取 120 到 300 秒；不设总超时。Retry-After 最多等 60 秒（可配置），超过就返回 Retryable 交给上层处理。退避加 0 到 25% 的 jitter。识别 x-should-retry、retry-after-ms、408 和 409；配额耗尽和鉴权错误不重试。sleep 和 CancelFlag 用 select 竞速。新增 LlmError{kind, status, retry_after_ms}，通过 anyhow 的 downcast 读取。

### B15. 处理 length 截断：参数是坏 JSON 的调用不执行，max_tokens 按模型设置 [high/S]
本 track 范围：只做 provider 侧：stop_reason 归一成 "stop"/"tool_use"/"length"/"error"，参数 JSON 解析失败时 arguments 用 Value::String(原始文本) 而不是 {}，models.json 增加 maxOutput，Anthropic/Gemini 默认 max_tokens 取模型值

上游参考：pkg/agent/src/agent-loop.ts:273-279、488；ai.CHANGELOG.md 1.3.0 #10897（argumentsError）、1.4.0 #9512

差距：AgentLoop 调用时 max_tokens 总是 None（crates/rupi-agent/src/lib.rs:707-714），所以 Anthropic、Bedrock、Gemini 都退回到 4096 的输出上限（anthropic.rs:84，已核实）。输出被截断后参数解析失败，结果变成 {}，但调用照样进入执行；在云端，参数为空的 write 还会触发一次审批。主循环只判断 stop_reason 是否为 error。

设计：把 stop_reason 归一为枚举 Stop、Length、ToolUse、Error、Aborted；ToolCall 增加 arguments_error 字段。遇到 Length 且消息里带工具调用时，这些调用不执行，也不送审批，而是各合成一个 is_error 结果，提示模型分块写入，这样调用和结果仍然一一配对。models.json 增加 maxOutput 字段，默认 max_tokens 取 maxOutput 与「上下文窗口减去估算输入再留余量」两者中的较小值，并按套餐设上限。

### B20. 请求卫生：错误结果中的图片、空工具输出、终端用户标识 [medium/S]
本 track 范围：Anthropic/Bedrock is_error 时图片挪出 tool_result、空白输出占位；终端用户标识 P2

上游参考：ai.CHANGELOG.md 1.3.0 #10794、1.4.0 #8720；packages/ai/src/api/anthropic-messages.ts:1306-1311（metadata.user_id）

差距：anthropic.rs:203-224 在 is_error 为 true 时也把图片放进 tool_result，会被 Anthropic 返回 400。空工具输出或只有空白的输出原样发送（crates/rupi-llm/src/lib.rs:312），没有输出的成功 bash 就会触发这种情况。请求不带终端用户标识，一个租户滥用就可能导致整个平台 key 被封。

设计：Anthropic 和 Bedrock 在 is_error 时把图片移到 tool_result 之后。空白输出统一替换成占位文本。ChatRequest 增加 identity 字段，其中 end_user 取 HMAC(tenant_id)，映射到 Anthropic 的 metadata.user_id 和 OpenAI 的 safety_identifier。User-Agent 设为 rupi/版本号。

### B21. 模型目录记录能力，套餐限定可用模型，租户和会话设置真正生效 [high/M]
本 track 范围：只做目录：ModelEntry 加能力字段（maxOutput、reasoning、thinking 方式、sampling）

上游参考：pkg/ai/src/types.ts:840-887；pkg/ai/src/models.ts:1259-1290（clampThinkingLevel）；docs/models.md:81-112；pkg/coding-agent/src/core/sdk.ts:442-453

差距：已核实。ALLOWED_SETTING_KEYS 接受 enabledModels、compaction、tools、base_url 等键（crates/rupi-server/src/admin.rs:24-45），但服务端只读其中少数几个，其余写进去不起作用。会话上的 model 只用于回显，不参与路由（run.rs:323）。租户可以把 model 设成任意字符串，用平台 key 调用最贵的模型。目录里没有 maxOutput、模态、thinking 档位等信息。

设计：ModelEntry 增加 max_tokens、input、thinking_map、cost、prompt_cache、image_limits、compat 字段。新建 PG 表 model_catalog(provider, id, version, data, enabled, plans)，以内置 JSON 作为初始数据，副本在内存缓存，通过 pub/sub 失效。实际使用的模型依次取 sess.model、tenant.model、默认值，且必须在套餐白名单内，否则返回 400。compaction、tools、excludeTools、max_turns 从租户设置读取并设上限。白名单里没有云端含义的键删掉。增加 clamp_thinking；不支持视觉的模型改为收到占位文本。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F101 [P0] 空工具输出在 Anthropic/Bedrock 侧生成空 text 块，下一次请求 400，会话无法自愈
类型：bug，工作量：S，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/bedrock.rs, crates/rupi-tools/src/lib.rs

证据：anthropic.rs:209-211 不管 content 是否为空，总是构造 `{"type":"text","text": content}` 放进 tool_result.content。工具侧空输出很常见：bash 成功且无 stdout/stderr 时返回 `ToolOutput::ok(s)`，s 为空串（rupi-tools/src/lib.rs:903-910）；glob 无命中返回 `hits.join("\n")`，即空串（lib.rs:1238）；grep 单文件不匹配返回 `ok("")`（lib.rs:1294,1300）。Anthropic Messages API 对空文本块返回 400 "text content blocks must be non-empty"，社区复现见 BerriAI/litellm#19061、anthropics/claude-code#62396。Bedrock 直接复用 `to_anthropic_messages`（bedrock.rs:70）。这条错误文本既不命中 `is_signature_mismatch` 也不命中 `is_overflow_error`，主循环直接返回 Err（rupi-agent/src/lib.rs:758-779）。复现：claude-* 模型执行 `bash mkdir -p build` 之后的下一次请求必定 400。

建议修法：在 to_anthropic_messages 里，content.trim() 为空且没有图片时改发 `"content": "(no output)"`（is_error 时发 "(error, no output)"），不再生成空 text 块；同时补一个空输出的单测。Gemini 的 functionResponse 可以照样带空串，不受影响。

### F102 [P0] Gemini/Vertex 流式累积器把同一轮里同名的并行工具调用合并成一个，前面的调用丢失
类型：bug，工作量：S，文件：crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs

证据：gemini.rs:352-362 用 `self.calls.iter_mut().find(|c| c.name == name && !c.merged)` 找槽位，找到就 `merge_args`（gemini.rs:407-416 按键覆盖）。`merged` 只在 `finish()` 构造完 blocks 之后才置位（gemini.rs:384），所以同一个响应里所有同名调用都会合并。Gemini streamGenerateContent 每个 functionCall part 都是完整调用，参数不会拆片。于是一轮里的 `read{path:a}` 和 `read{path:b}` 最终只剩 `read{path:b}`，第一个调用无声消失。单测 accumulator_merges_fragmented_function_call（gemini.rs:668-703）把这个错误假设写死了。Vertex 复用同一个累积器（vertex.rs:156,189），主循环始终走流式（rupi-agent/src/lib.rs:716-717）。

建议修法：每遇到一个 functionCall part 就新增一个 CallFrag，fc.id 存在时用它做 id。删除按名合并逻辑，或仅在显式开启 Vertex partialArgs 流时保留。单测改为断言同名两次调用得到两个 ToolCall。

### F103 [P0] Gemini 3 的 thoughtSignature 既不保存也不回放，多步工具调用从第二次请求起 400
类型：bug，工作量：M，文件：crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs, crates/rupi-core/src/lib.rs

证据：全仓 grep 不到 thoughtSignature 或 thought_signature。parse_gemini_response（gemini.rs:262-286）和 GeminiAccumulator::apply_response（gemini.rs:345-364）都忽略 part 级的 thoughtSignature；to_gemini_contents 回放时只发 `{"functionCall":{name,args}}`（gemini.rs:126-132）；`ContentBlock::ToolCall` 也没有字段能携带签名（rupi-core/src/lib.rs:34-38）。Google Vertex 文档原文：Gemini 3 使用函数调用时 "you must return the thought signatures from previous responses"，缺失时 "the model will return a 400 error"，校验对象是当前 turn 每一步的第一个 functionCall。所以任何 gemini-3* 模型第一步工具调用正常，同一 turn 的第二次请求必定 400。内置目录只列了 gemini-2.5（models.json:45-56），2.5 上签名可选，因此测试没暴露。

建议修法：给 ToolCall 增加 `#[serde(default)] signature: Option<String>`，或加一个通用的 provider_meta。流式和非流式都从 part 上取 thoughtSignature，回放时写回 functionCall part。对跨 provider 历史里没有签名的调用，按文档填 `skip_thought_signature_validator`。

### F104 [P0] 请求体与当前一代模型不兼容：总发 temperature、只会 budget_tokens、reasoning_effort 乱发
类型：bug，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/bedrock.rs, crates/rupi-llm/models.json

证据：OpenAI-compat：openai_body 无条件写入 temperature，缺省 0.2（lib.rs:385），主循环本身也传 `Some(0.2)`（rupi-agent/src/lib.rs:712）。o3、o4-mini、gpt-5 系对非默认温度返回 400 "Only the default (1) value is supported"，而 o3/o4-mini 就在内置目录里（models.json:16-26），从 `rupi models` 选中后每次请求都失败。设置了 thinking 就发 reasoning_effort（lib.rs:394-396），连 gpt-4o 这类非推理模型也照发，会 400；Max/XHigh 映射成 "xhigh"（lib.rs:111-112），o3/o4-mini 不接受这个值。Anthropic：body 总带 temperature（anthropic.rs:113-121），思考只会 `{"type":"enabled","budget_tokens"}`（anthropic.rs:108-111）。按 Anthropic 当前文档，Opus 4.7/4.8/5/5.5 和 Sonnet 5/5.5 拒收采样参数和 budget_tokens（400），adaptive thinking 和 `output_config.effort` 完全不支持，因此 `--model claude-opus-4-8` 或 `claude-sonnet-5-5` 每次都 400。Bedrock 同样问题（bedrock.rs:76-86）。

建议修法：给模型加能力描述：models.json 增加 `sampling`、`reasoning`、`thinking: budget|adaptive|none`、`max_output` 字段，查不到时按名字前缀兜底。req.temperature 为 None 或模型不支持采样时不发 temperature。Anthropic 对 4.6+ 改用 adaptive + `output_config.effort`，更老的模型才用 budget。OpenAI 只对推理模型发 reasoning_effort，并按模型裁剪取值。

### F105 [P0] OpenAI-compat 丢弃 reasoning_content/reasoning：DeepSeek V4 思考模式加工具时按文档会 400，TUI 也看不到推理过程
类型：bug，工作量：M，文件：crates/rupi-llm/src/lib.rs

证据：SseAccumulator::apply_chunk 只读 delta.content 和 delta.tool_calls（lib.rs:696-720）；parse_openai_response 只读 content 和 tool_calls（lib.rs:806-832）；to_openai_messages 回放助手消息只带 content 和 tool_calls（lib.rs:277-301）。全仓没有 reasoning_content。DeepSeek thinking_mode 文档写明：思考模式默认开启，带 tools 时 "reasoning_content must be fully passed back to the API in all subsequent requests... the API will return a 400 error"；opencode#24190、hermes-agent#17212 都复现了这个问题。本仓 .env 的 MODEL=deepseek-v4-flash。这条走的是 本地 .env 配置的网关 网关，网关是否代为补全未能实测（磁盘满，无法运行），因此标为待实测。另外这条路径永远不产出 ContentBlock::Thinking，llama.cpp `--reasoning-format` 和 OpenRouter 推理模型的推理内容也全部丢失，和 Anthropic 路径不一致。

建议修法：流式和非流式都把 delta/message 里的 `reasoning_content`（以及 OpenRouter 的 `reasoning`）累积成 `ContentBlock::Thinking{signature:None}`。回放给 OpenAI-compat 时，在 assistant 消息上带回 `reasoning_content`。加一个 stub 测试：两轮工具调用，断言第二轮请求体里有 reasoning_content。

### F106 [P0] 输出上限固定 4096 且无配置项，截断后的工具调用被静默当成 `{}` 参数执行
类型：bug，工作量：M，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/lib.rs, crates/rupi-agent/src/lib.rs

证据：主循环始终传 `max_tokens: None`（rupi-agent/src/lib.rs:711）。Anthropic 和 Bedrock 缺省 4096（anthropic.rs:84、bedrock.rs:61），Gemini/Vertex 的 maxOutputTokens 缺省 4096（gemini.rs:68-71、vertex.rs:78-81）。rupi-config 里没有任何 maxTokens 设置。openai_body 完全不发 max_tokens（lib.rs:374-398），所以 ChatRequest.max_tokens 在 OpenAI-compat 上不起作用，review.rs:319 的 `Some(600)` 白设。输出触顶后的 stop_reason（max_tokens/MAX_TOKENS/length）没有归一化，gemini.rs:221-224 的注释也承认这一点；主循环只检查 "error"（rupi-agent/src/lib.rs:784）。被截断的 tool_use JSON 解析失败后被替换成 `{}`（anthropic.rs:591、lib.rs:743、lib.rs:794），工具照常执行。复现：Claude 用 write 工具写一个约 400 行的文件，input_json 在 4096 token 处被截断，变成 `write{}`，工具报缺参，模型重试同样的调用，直到 max_turns。

建议修法：增加 settings `maxOutputTokens`，按目录给每个模型设缺省值（流式建议 16k–32k），OpenAI-compat 也发 max_tokens/max_completion_tokens。把 stop_reason 归一成共享枚举（stop/tool_use/length/refusal/error）。遇到 length 且工具 JSON 不完整时不执行工具，回一个 error tool_result 提示分段；JSON 解析失败时把错误告诉模型，不再静默换成 `{}`。

### F107 [P1] usage 口径各家不一致：Anthropic 的 input 不含缓存，导致成本、命中率、配额失真；Gemini 思考 token 未计入
类型：bug，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-agent/src/tokens.rs, crates/rupi-server/src/run.rs

证据：parse_provider_usage 把 prompt_tokens、input_tokens、promptTokenCount 当成同一含义（lib.rs:170-173）。OpenAI 的 prompt_tokens 和 Gemini 的 promptTokenCount 包含缓存命中部分；Anthropic 的 input_tokens 只是未缓存的剩余部分，总 prompt = input + cache_read + cache_write。AnthropicAccumulator 原样上报（anthropic.rs:537-545）。下游都把 input 当成总 prompt：TokenMeter::cost_usd 只算 input+output（tokens.rs:194-196），Anthropic 的缓存读写永不计费，OpenAI 的缓存 token 按全价计；cache_hit_pct = cache_read/input（tokens.rs:205-210），在 Anthropic 上会超过 100%（例如 input 2k、cache_read 150k 时显示 7500%）；云配额只累加 input+output（rupi-server/src/run.rs:339-348），Anthropic 带缓存的会话被少计。Gemini 的 thoughtsTokenCount 没读（lib.rs:174-182），2.5 系默认开思考，output 被少报。单测 parse_provider_usage_anthropic_and_openai（lib.rs:1688-1703）把这种混用固化了。

建议修法：约定 StreamEvent::Usage.input 表示含缓存的总 prompt。Anthropic 累积器和 Bedrock 改为上报 input_tokens+cache_read+cache_write；Gemini 的 output 加上 thoughtsTokenCount。成本按 (input-cache_read-cache_write)×输入价 + cache_read×读价 + cache_write×写价 计算，价格取自目录。

### F108 [P1] 工具结果图片挂到下一个 tool_result 上（Anthropic/Bedrock）
类型：bug，工作量：S，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-agent/src/lib.rs

证据：主循环的顺序是先 push ToolResult、再 push 该工具的图片（rupi-agent/src/lib.rs:1076-1086）。to_anthropic_messages 先把 Image 存进 pending_images，再 append 到下一个 ToolResult 的 content 里（anthropic.rs:209-213、223-225），只有剩下的图片才挂到最后一项（229-237）。所以调用 [read(a.png), bash] 时 a.png 进了 bash 的结果；调用 [read(a.png), read(b.png)] 时两张图都在第二个结果里，第一个结果没有图。Bedrock 复用同一段（bedrock.rs:70）。单测 maps_user_and_tool_images（anthropic.rs:836-885）只覆盖了单个工具的情况。

建议修法：遇到 Image 时挂到 items 里最近一个已生成的 tool_result（last_mut），而不是下一个；补一个两个工具、只有一个带图的单测。

### F109 [P1] 错误回包处理不统一：非 JSON 错误丢状态码，Bedrock 错误文案全丢，溢出无法识别，(no body) 正则永不命中
类型：bug，工作量：S，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs, crates/rupi-llm/src/bedrock.rs, crates/rupi-llm/src/overflow.rs, crates/rupi-llm/src/lib.rs

证据：Anthropic、Gemini、Vertex、Bedrock 的 complete 都先 `resp.json().await?` 再看状态码（anthropic.rs:635-636、gemini.rs:442-443、vertex.rs:117-118、bedrock.rs:105-106）。代理返回 502 HTML 或 413 纯文本时，用户只看到 "error decoding response body"，状态码和正文都丢了。流式路径用 `unwrap_or(json!({}))`，只剩 "anthropic request failed" 这类泛化文案（anthropic.rs:677-680、gemini.rs:473-475、vertex.rs:150-152）。Bedrock 用 anthropic::error_text 读 `/error/message`（bedrock.rs:108 → anthropic.rs:290-292），而 Bedrock runtime 的错误体是 `{"message":...}`，所以所有 Bedrock 错误都显示 "bedrock request failed"，"Input is too long for requested model" 到不了 is_overflow_error，与 overflow.rs:6 "三家原生 provider 溢出都走显式报错" 的说法相矛盾。overflow.rs:35 的 `^4(?:00|13)\s*(?:status code)?\s*\(no body\)` 锚定在行首，但 ensure_success 生成的是 "openai-compat HTTP 413 Payload Too Large (no body)"（lib.rs:884-886），永远匹配不上。只有 OpenAI-compat 用了 ensure_success/extract_error_message（lib.rs:598,615）。

建议修法：所有 provider 统一走 ensure_success。extract_error_message 已经兼容 error.message、error、message 和原文，Bedrock 也就修好了。签名失配重试改为基于带 status+body 的错误类型来判断。overflow 正则改成 `HTTP 4(?:00|13)\b.*\(no body\)`。

### F110 [P1] 流中错误和过载不重试，注释声称主循环会重试，实际直接 abort
类型：incomplete，工作量：M，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/lib.rs, crates/rupi-agent/src/lib.rs

证据：post_json_with_retry 只在拿到响应头之前重试（lib.rs:907-935）。流中的 `event: error`（如 overloaded_error）会直接 bail（anthropic.rs:726-731），OpenAI 流中的 error 对象（lib.rs:637-644）和 chunk 传输错误（lib.rs:621 `chunk?`）也一样。anthropic.rs:724-725 的注释写着"转错误让主循环走重试/溢出恢复路径"，但主循环只对溢出做恢复（rupi-agent/src/lib.rs:762-778），其余错误一律 `return Err(e)`。Anthropic 高峰期常在流中下发 overloaded，整轮因此中止。

建议修法：在主循环或一个 provider 包装层里，对可重试的流错误（overloaded、rate_limit、5xx、connection reset）做退避重试，条件是本轮还没有向会话提交 TextDelta 或工具调用（或先回滚已推送的 partial）。同时修正这条注释。

### F111 [P1] HTTP 客户端没有任何超时，Retry-After 也不设上限，可能无限挂起
类型：bug，工作量：S，文件：crates/rupi-llm/src/lib.rs

证据：shared_http_client 只调了 `reqwest::Client::builder().build()`（lib.rs:457-460），没设 connect_timeout 或 read_timeout，reqwest 默认也没有超时。网关建连后不发数据或流半开时，`rupi run`、`-p`、云端 run 会一直挂着（这几条路径没有取消入口），TUI/REPL 只能靠 Ctrl-C。parse_retry_after 接受任意秒数（lib.rs:865-868），收到 `Retry-After: 3600` 就会睡一小时；退避也没有 jitter（lib.rs:860-862）。

建议修法：客户端加 connect_timeout(10s) 和 read_timeout（如 120s，可用 RUPI_HTTP_READ_TIMEOUT 覆盖，不设总超时以免切断长流）。Retry-After 封顶 60s，退避加随机 jitter。

### F112 [P1] 无工具请求也发空 tools 数组：OpenAI 官方 API 拒收，压实摘要和 LLM 复盘静默失效
类型：bug，工作量：S，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs, crates/rupi-agent/src/lib.rs

证据：openai_body 无条件写入 `"tools": [...]`，为空也照写（lib.rs:381-384）；Gemini 和 Vertex 总是发 `tools:[{"functionDeclarations":[]}]`（gemini.rs:62-65、vertex.rs:72-75）。压实摘要和 LLM 复盘传的都是 `tools: vec![]`（rupi-agent/src/lib.rs:1284、review.rs:318）。OpenAI 官方 API 对空 tools 返回 400 "Invalid 'tools': expected an array with at least one element"（Portkey 错误库有收录）。于是接 api.openai.com 时压实永远失败，退回每条消息取首行的启发式摘要，只打一行 tracing::warn（rupi-agent/src/lib.rs:1289-1307）；复盘永远为空。Gemini 空 functionDeclarations 可能同样被拒，未验证。Anthropic 这边已经正确省略（anthropic.rs:99-101）。

建议修法：OpenAI-compat、Gemini、Vertex 在 tools 为空时省略该字段，与 Anthropic 一致；补单测。

### F113 [P1] `/thinking off` 与"不干预"发出的请求完全相同，默认开思考的模型关不掉
类型：bug，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/gemini.rs

证据：lib.rs:37-39 注释承诺 "Some(Off) = 显式关闭（能关的 provider 省 token）"。实际 openai_effort(Off)、gemini_level(Off)、anthropic_budget(Off) 都返回 None（lib.rs:107,119,128），请求体与 thinking=None 一模一样。按各家文档，DeepSeek V4、Gemini 2.5 Flash/Pro 和 Claude 4.6+ 的部分型号默认开着思考，`--thinking off` 和 `/thinking off` 对它们不起作用。Gemini 只会用 thinkingLevel 表达档位（gemini.rs:72-77），2.5 Flash 关思考需要 `thinkingBudget: 0`；2.5 是否接受 thinkingLevel，官方文档没说清，有第三方报告 400，未能实测。

建议修法：Off 按 provider 和模型映射：DeepSeek 发 `thinking:{type:"disabled"}`，OpenAI gpt-5 发 `reasoning_effort:"minimal"`，Gemini 2.5 发 `thinkingBudget:0`（仅 flash 可关），Gemini 3 发 `thinkingLevel:"minimal"`。依赖上面能力描述那一条的模型能力表。

### F114 [P1] 扩展登记的 provider 缺自身 key 时会回落到用户的 OpenAI/Anthropic/Gemini 主 key，凭据泄露给扩展指定的 base_url
类型：bug，工作量：S，文件：crates/rupi-llm/src/route.rs, crates/rupi-llm/src/catalog.rs, crates/rupi-ext/src/rpc.rs

证据：provider_from_extra 的 key 候选是 [extra.api_key_env, RUPI_API_KEY, OPENAI_API_KEY]，anthropic/gemini 协议同理回落到对应主 key（route.rs:251-279）。扩展通过 RPC registerProvider 可以填任意 base_url（rupi-ext/src/rpc.rs:436-495 → catalog.rs:99-129），登记结果写入 providers.json（catalog.rs:127），每次启动都重新加载（rupi-cli/src/main.rs:1812,1918,2178,2721）。若 ACME_KEY 未设，`--model acme/x` 会把用户的 OPENAI_API_KEY 作为 Bearer 发到 https://acme.example。另一方面，本机 server 类扩展 provider 无法免 key，pick_key 失败后回退成 mock。

建议修法：只认 extra.api_key_env 和 --api-key，绝不回落第一方 key。允许声明 `keyless: true` 用于本机 server。可以考虑首次使用新登记的 provider 时提示用户确认。

### F117 [P1] Gemini 合成的调用 id 每轮都从 call-0 起，跨轮重复，回放的 functionResponse 名字被张冠李戴
类型：bug，工作量：S，文件：crates/rupi-llm/src/gemini.rs

证据：流式 finish 的 id 是 `call-{i}`，每个响应从 0 起（gemini.rs:378-380），且流式不读 fc.id（gemini.rs:352-354）；非流式读 fc.id，缺失时从 call-1 起编号（gemini.rs:270-277），两条路径不一致。to_gemini_contents 扫描整段历史建一张 id→name 表（gemini.rs:86-93），后面的轮次会覆盖前面同名的 id。例如第 1 轮 call-0 是 read、第 2 轮 call-0 是 bash，回放时第 1 轮的 functionResponse 会被标成 "bash"。会话树里也出现跨轮重复的 tool_call_id。

建议修法：有 fc.id 就用它，没有就生成唯一 id（如 `call-{uuid}`）。name 表改为按紧邻的那条 assistant 消息查找，不再全局建表。

### F118 [P1] Anthropic 只在 tools 和 system 上设缓存断点，对话历史每轮按全价重算；Bedrock 完全不缓存
类型：perf，工作量：S，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/bedrock.rs, crates/rupi-agent/src/lib.rs

证据：body() 只给 system 块和最后一个工具加 cache_control（anthropic.rs:76-79,90-94），messages 上没有断点。agent 循环里不断变长的对话（包括大段工具输出）每轮都按全价计输入。Anthropic 文档对多轮对话的建议是在最近一轮的最后一个块上放断点，或使用顶层自动缓存。此外，主循环把每次用户输入的 `<Recalled>` 记忆和 PlanMode 文本拼进 system（rupi-agent/src/lib.rs:694-702），换一个用户问题 system 就变，system 断点随之失效。Bedrock body 不带任何 cache_control（bedrock.rs:57-88）。

建议修法：在最后一条消息的最后一个内容块上加 cache_control（总数不超过 4 个），或使用顶层 `cache_control`。把 recall 和 plan 这类易变文本移到最后一条 user 消息里。Bedrock 同步加上断点。用 usage.cache_read_input_tokens 验证第二轮的命中。

### F120 [P1] 模型目录只用于展示：context 覆盖不影响压实，也没有价格和能力字段；内置条目已过时
类型：incomplete，工作量：M，文件：crates/rupi-llm/src/catalog.rs, crates/rupi-llm/models.json, crates/rupi-llm/README.md

证据：ModelEntry 只有 id、provider、name、context（catalog.rs:28-35）。context 只用在展示、RPC 和 HTTP 列表（catalog.rs:221-226、rupi-agent/src/session.rs:414、rupi-server/src/http.rs:228）。压实窗口和 TokenMeter 用的是 tokens::context_window_for 的子串匹配（rupi-agent/src/lib.rs:393、tokens.rs:27-94）。因此 crate README:16 说的 `~/.rupi/models.json` 覆盖改不了窗口、价格、输出上限或能力。内置条目方面：claude-opus-4（models.json:33-38）对应的 Claude Opus 4 已于 2026-06-15 退役，"claude-opus-4" 本身也不是有效别名；claude-sonnet-4-5 已弃用，2026-11-30 退役；Bedrock 的 anthropic.claude-3-5-sonnet-20241022-v2:0（models.json:75-80）是已退役的模型族。目录里没有 Claude 4.6+/5.x、Gemini 3、gpt-5。

建议修法：ModelEntry 增加 max_output、cost{input,output,cache_read,cache_write}、thinking/sampling 能力字段（serde default）。tokens::rates_for 和 context_window_for 先查 load_models()，查不到再用启发式。刷新内置列表。

### F122 [P2] SseParser::finish 从未被调用，末尾没有空行的事件会丢
类型：bug，工作量：S，文件：crates/rupi-llm/src/sse.rs, crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs

证据：sse.rs:53-58 专门为"部分网关最后一个事件不带空行"写了 finish()，但四个 provider 的流循环结束后都没调用它（lib.rs:620-654、anthropic.rs:708-735、gemini.rs:480-516、vertex.rs:157-191），只有单测（sse.rs:175）和 bench（rupi-benches/benches/sse.rs:17）用到。遇到这种网关，最后的 usage、finish_reason 或最后一段 tool_call 参数会丢失。

建议修法：while 循环结束后执行 `if let Some(ev) = parser.finish() { 处理 ev }`。最好放进下面提议的公共流循环里，一处修好。

### F123 [P2] is_signature_mismatch 过宽：任何含 "thinking" 的 400 都会去掉思考后静默重试
类型：bug，工作量：S，文件：crates/rupi-llm/src/anthropic.rs

证据：anthropic.rs:297-303 的判定是 400 且错误文本含 "signature" 或 "thinking"。新模型拒收 budget_tokens 这类 400（错误文本含 thinking）也会命中，于是 strip_thinking_for_retry（anthropic.rs:307-319）关掉思考后重发。用户设的 `/thinking high` 就这样悄悄失效，只留一条 tracing::warn（anthropic.rs:644,682）。

建议修法：只匹配签名失配的特定措辞（signature 相关、thinking blocks cannot be modified 之类），并给前端发一个可见的提示事件。

### F124 [P2] 思考内容不流式推送，Bedrock 是伪流式且缺缓存和签名重试
类型：incomplete，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/bedrock.rs

证据：StreamEvent 只有 TextDelta 和 Usage（lib.rs:157-166）。Anthropic 的 thinking_delta 只静默累积（anthropic.rs:488-494），主循环在整个响应结束后才 emit_thinking（rupi-agent/src/lib.rs:312-320）。Max 档预算 32768 token 时，TUI 和云端 AG-UI 可能长时间没有任何输出，尽管 agui.rs:265 已有 reasoning 角色。Bedrock 的 complete_streaming 是一次 /invoke 后整体推送（bedrock.rs:149-178；README 已注明），也没有签名失配重试。

建议修法：增加 StreamEvent::ThinkingDelta，经 AgentEvent 透传到 TUI 和 AG-UI。Bedrock 实现 invoke-with-response-stream 的 eventstream 解析，或改用 ConverseStream。

### F125 [P1] 合并 GeminiProvider 与 VertexProvider（几乎整份重复）
类型：simplify，工作量：S，文件：crates/rupi-llm/src/vertex.rs, crates/rupi-llm/src/gemini.rs

证据：VertexProvider::body（vertex.rs:62-90）与 GeminiProvider::body（gemini.rs:52-80）逐行相同。complete 和 complete_streaming（vertex.rs:103-195 对 gemini.rs:428-517）只有三处不同：URL 前缀、鉴权头（Bearer 对 x-goog-api-key）、provider 标签。

建议修法：只保留一个 GeminiProvider { auth: Auth::ApiKey|Bearer, base, label }，VertexProvider::from_options 改为返回按 Vertex 配置的 GeminiProvider。预计减约 150 行，vertex.rs 只剩构造函数和测试。

### F126 [P1] 抽出公共 SSE 循环、usage 推送、错误处理和 Anthropic 请求体
类型：simplify，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/vertex.rs, crates/rupi-llm/src/bedrock.rs

证据："bytes_stream → SseParser → [DONE] → 解析 JSON → 检查 error" 的循环写了 4 份（lib.rs:616-654、anthropic.rs:705-735、gemini.rs:477-516、vertex.rs:154-194）。"任一计数大于 0 就发 Usage" 的代码块写了 4 份（lib.rs:679-691、gemini.rs:500-512、vertex.rs:176-188、bedrock.rs:156-168）。非 2xx 处理各写各的（见错误回包那一条）。Anthropic 的 complete 和 complete_streaming 各写了一遍签名重试（anthropic.rs:640-660 对 675-704）。Bedrock 的 body 重写了一遍 Anthropic 的 body（bedrock.rs:57-88 对 anthropic.rs:73-124），在缓存和 temperature 上已经出现差异。

建议修法：新增 `async fn for_each_sse(resp, f)`（内含 finish）、`fn emit_usage(tx,&Value)`、`fn anthropic_core_body(req, cache: bool)`（Bedrock 复用）、`async fn send_anthropic(&self, req, stream) -> Response`（内含签名重试），错误统一走 ensure_success。预计减 150–200 行，同时修掉上面错误回包和 finish 未调用两条。

### F127 [P2] 删死代码 from_env，provider 推断去重，去掉重复的亲和设置
类型：simplify，工作量：S，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs, crates/rupi-llm/src/route.rs, crates/rupi-cli/src/main.rs, crates/rupi-server/src/run.rs

证据：OpenAiCompatProvider::from_env（lib.rs:559-567）、AnthropicProvider::from_env（anthropic.rs:52-59）、GeminiProvider::from_env（gemini.rs:29-37）全仓没有调用者（grep 只搜到定义），逻辑与 route.rs:124-146,200-209 重复。按模型名前缀推断 provider 的逻辑有三份：route.rs:96-104（私有）、rupi-cli/src/main.rs:737-745、rupi-server/src/run.rs:806-813。OpenRouter 亲和设置了两次：with_kind 里一次（lib.rs:497-499），route.rs:163 又显式 `with_session_affinity(true)`。

建议修法：删掉三个 from_env（约 30 行）；route::infer_provider 改为 pub，CLI 和 server 复用（约 20 行）；去掉 route.rs:163 多余的那次调用。

### F128 [P1] 重试、端到端流式、多工具图片等关键行为缺测试，改环境变量的测试并发有竞态
类型：test-gap，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/route.rs, crates/rupi-llm/src/catalog.rs, crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/gemini.rs

证据：teststub 能返回 429/500（lib.rs:1046-1053），但每次都是同一个状态码，且没有任何测试用到 429/500（grep 确认），post_json_with_retry 的重试和 Retry-After 从未被测过。OpenAI-compat、Anthropic、Gemini 都没有端到端流式测试，只测了累积器；只有 Vertex 用 start_sse（vertex.rs:263-325）。空工具输出、多工具带图、Gemini 同名并行调用、非 JSON 错误体都没有覆盖。provider_for_model_routes_by_prefix（lib.rs:1194-1233）和 provider_from_spec_routes_explicit（route.rs:339-422）并发删除和恢复同一批 key，且只在原值为 Some 时恢复，开发机设了 OPENAI_API_KEY 时会偶发失败。register_extra_provider_merges_models 也在并发修改 RUPI_HOME 和全局登记表（catalog.rs:254-286）。

建议修法：stub 改成按队列返回 (status, body, headers)，断言重试次数和 Retry-After；每个 provider 补 SSE stub 测试，各补一条上述映射的单测；改环境变量的测试用同一把 static Mutex 串行化，或把环境读取改成可注入。

### F129 [P1] 缺 OpenAI Responses API、Anthropic adaptive thinking/effort、tool_choice 等现代接口
类型：missing-feature，工作量：L，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/anthropic.rs

证据：OpenAI 只支持 /chat/completions（lib.rs:508-520），上游 pi-ai 有 openai-responses 实现，只走 Responses 的模型（如 codex 系）和推理摘要都用不了。Anthropic 只支持 budget 式思考（anthropic.rs:105-112），没有 `thinking:{type:"adaptive"}` 和 `output_config.effort`。没有 tool_choice 和 parallel 控制，没有 anthropic-beta 头的配置口，也没有 /v1/models 发现，llama.cpp 和 OpenAI-compat 无法列出远端模型。

建议修法：新增 OpenAiResponses 路由（`/v1/responses` 及其 SSE 事件），作为 CompatKind 的一种或单独的 provider。Anthropic 按模型能力切换 adaptive/effort（与请求体不兼容那一条一起做）。ProviderOptions 增加 extra_headers 和 tool_choice。

### F130 [P2] OpenRouter/Azure/llama.cpp 回复的 Message.provider 一律写成 "openai-compat"
类型：bug，工作量：S，文件：crates/rupi-llm/src/lib.rs

证据：SseAccumulator::finish 和 parse_openai_response 把 provider 硬编码为 "openai-compat"（lib.rs:756、843），而 name() 能区分 openrouter、azure、llamacpp（lib.rs:572-579）。这个字段会写进 JSONL 导出（rupi-memory/src/session_io.rs:222），导出/分享的会话来源因此不准。

建议修法：把 self.name() 传给 finish 和 parse_openai_response。

### F131 [P2] 注释和文档与实现不符
类型：doc-drift，工作量：S，文件：crates/rupi-llm/src/anthropic.rs, crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/overflow.rs, crates/rupi-llm/README.md, docs/UPSTREAM.md

证据：anthropic.rs:724-725 称流错误"让主循环走重试"，实际主循环只做溢出恢复。lib.rs:37-39 称 Some(Off) 会显式关闭思考，实际与 None 相同。overflow.rs:6 称"三家原生 provider 溢出都走显式报错"，但 Bedrock 的错误文本被丢弃。crate README:16 的"目录可被覆盖"只影响列表，不影响窗口和价格。UPSTREAM.md:56 的显式路由列表漏了 llamacpp（README.md:207 里有）。UPSTREAM.md:59 的"成本：内建粗粒度价目"没注明不计缓存价、价目已过时。

建议修法：随对应代码修复一起更新这些注释和文档；在 UPSTREAM 的"已知未做"里补上 Bedrock eventstream、Responses API、Gemini 3 签名等项。

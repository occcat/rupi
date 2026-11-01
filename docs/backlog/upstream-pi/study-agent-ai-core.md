# agent-ai-core

## 上游
从 0.86 到 1.4，agent 包（agent.CHANGELOG.md）改动不多。1.0.0 把 harness（会话存储、durable、compaction、skills）移出，`pi-agent-core` 只保留 `Agent`、agent loop、`streamProxy` 和类型，持久会话交给 `pi-durable`。0.87.0 用 `prepareRequest`（每次 provider 请求前可以替换 context、model、thinking）和 `finishTurn`（工具结果定稿后返回 end 或 continue，error 和 aborted 一律硬退出）取代 `shouldStopAfterTurn`，同时加了 `peekQueuedMessages()`（pkg/agent/src/agent-loop.ts:228-301，agent.ts:339）。agent loop 遇到 `stopReason=length` 的消息时，其中的工具调用全部失败、不执行（agent-loop.ts:273-279, 488）。1.3.0 起，最终参数是畸形 JSON 的调用带 `argumentsError`，报错时附原文（ai.CHANGELOG #10897）。ai 包在 0.86.0 改成 transcript 化的 `TranscriptContext`，系统提示和工具以 system message 出现在会话里，目的是工具变化时不破坏 prompt cache（ai.README.md「System Messages」，1.0.1 inline-tools beta，1.4.0 #10940）。缓存方面，`cacheRetention` 和 `sessionId`（pkg/ai/src/types.ts:247-257）映射为 Anthropic `cache_control`（含 1h ttl，断点打在 system、末工具、末条 user 三处）、OpenAI `prompt_cache_key`（packages/ai/src/api/openai-prompt-cache.ts）以及各网关的会话亲和头。成本方面，`Usage` 带分项 cost、`cacheWrite1h`、`reasoning` 和 `totalFromProvider`（types.ts:459-487）；`calculateCost` 支持价档 tiers，1h 缓存写按 2 倍 input 计价（pkg/ai/src/models.ts:1236-1257）；OpenAI 兼容路径把 input 归一为不含缓存的部分。重试方面，provider 层遵守 `x-should-retry`、408/409/429/5xx 和 `retry-after(-ms)`，等待超过 60s 直接失败交给上层，退避带最多 25% 的 jitter（packages/ai/src/utils/provider-retry.ts:1-68）；agent 层默认重试 3 次、2s 起、60s 封顶，订阅额度和配额耗尽不重试（packages/ai/src/utils/retry.ts，docs/settings.md:128-136）。超时方面，1.2.0 把 timeout 限定为等待响应头（#10609），coding-agent 默认 300s idle 超时（pkg/coding-agent/src/core/http-dispatcher.ts:4）；1.4.0 起任何 HTTP 413 都按上下文溢出处理。虚拟模型（docs/virtual-models.md，coding-agent/src/core/virtual-models.ts）把选择和派发分开：continuation 沿用上一个物理模型以保住缓存和签名，retry 可以换模型，1.3.0 起换模型时丢弃其他模型产生的 thinking。鉴权方面有 `CredentialStore`（modify 串行化刷新，1.0.3 修了轮换后的 refresh token 丢失）和请求级 `env`/`apiKey` 覆盖（ai.README.md「Provider-Scoped Environment Overrides」）。1.4.0 加了 `pi.ai.request` span，以及 duration、first_chunk、tokens、cost 指标，属性低基数、不记录内容（pkg/ai/src/telemetry.ts）。请求卫生类修复包括：错误 tool_result 带图片时改为把图片放在后面（#10794），空白工具输出改用占位符（#8720）。

## rupi 现状
rupi-llm：`LlmProvider` 只有 complete/complete_streaming，外加两个可变 setter：set_session_id 和 set_session_affinity（crates/rupi-llm/src/lib.rs:231-265）。`ChatRequest` 只有 6 个字段：system/messages/tools/max_tokens/temperature/thinking（lib.rs:31-43）。用量只能经 `StreamEvent::Usage` 传出（lib.rs:157-166），`ChatResponse` 没有 usage 字段（lib.rs:147-151），所以非流式 `complete()` 的用量全部丢失。parse_provider_usage 没有统一口径（lib.rs:169-210）：OpenAI 和 Gemini 的 input 含缓存，Anthropic 不含。

重试只在发请求阶段：429/5xx 加 Retry-After，退避 500ms·2^n、封顶 8s，最多 3 次，无 jitter，Retry-After 不封顶（lib.rs:855-935）。shared_http_client 没有任何超时（lib.rs:453-463）。流中出错直接 bail（anthropic.rs:726-731，lib.rs:637-644）。

缓存：Anthropic 只在 system 和末工具打断点（anthropic.rs:76-94），Bedrock 不打（bedrock.rs:57-88），OpenAI 不发 prompt_cache_key（lib.rs:374-398）。会话亲和 id 是构造时生成的随机 uuid（lib.rs:487）。目录 ModelEntry 只有 4 个字段（catalog.rs:28-35），内置 15 条；价目按名字子串硬编码，只算 input/output（rupi-agent/src/tokens.rs:20-90）。

rupi-agent：每个 run 的工具顺序来自 HashMap 迭代（rupi-tools/src/lib.rs:91-94, 119-121；rupi-agent/src/lib.rs:653-656）。max_tokens 恒为 None（lib.rs:707-714），主循环只检查 stop_reason=="error"（lib.rs:784）。溢出时强制压实，最多 2 次（lib.rs:761-780），其余错误直接 return Err。压实摘要用非流式 provider.complete()，用量不计（lib.rs:1276-1289）。有工具级钩子（hooks.rs:37-52）和 steering inbox（lib.rs:283, 822, 1105）。

rupi-server：provider 每个 run 按租户 settings 构建（lib.rs:137-153；run.rs:313, 790-860）。resolve_byok 除 anthropic/gemini 外一律取 openai_api_key/api_key，再回落平台 env（run.rs:832-840）。缺 key 时 provider_or_mock 静默返回 mock（route.rs:290-297）。会话的 model 字段只回显，不参与路由（run.rs:323）。settings 里的 enabledModels 和 base_url 没有任何代码读取（admin.rs:37, 43）。配额只在入院时检查（quota.rs:33-86），token 按 input+output 累加，统计跑在一个满了就丢的事件通道上（run.rs:54-58, 337-361）。persist_tree 不写 usage（db.rs:1089）。

每个 run 还会把 AGENTS.md、commands 和 input.context 作为 System 消息再 push 进会话树并落盘（run.rs:394-401, 883, 956），历史里因此不断堆积重复内容。/metrics 只有 4 个序列（http.rs:76-89），rupi-llm、rupi-agent、rupi-server 没有任何 tracing span。AG-UI 图片 URL 只按前缀放行 https:// 和 http://127.0.0.1，会跟随重定向（agui.rs:390-397）。

## 提案
### 统一用量口径、按目录计价、不丢账的计量链路 [critical/L]
上游：packages/ai/src/types.ts:459-487（Usage.cost 分项、cacheWrite1h、reasoning、totalFromProvider）；pkg/ai/src/models.ts:1236-1257 calculateCost（价档 tiers，1h 缓存写按 2 倍计）；packages/ai/src/api/openai-completions.ts parseChunkUsage（input = prompt − cacheRead − cacheWrite）；ai.CHANGELOG 1.1.0、0.99.0 #10034、0.86.0 #9457、1.4.0 pi.ai.cost；pkg/coding-agent/src/core/usage-totals.ts

差距：① 口径不一：crates/rupi-llm/src/lib.rs:169-210 把 OpenAI 的 prompt_tokens（含 cached）和 Gemini 的 promptTokenCount（含 cachedContent）当作 input，而 Anthropic 的 input_tokens 不含 cache_read，同一个 TokenUsage.input 有两种含义。rupi-core/src/lib.rs:159-164 和 tokens.rs:205-210 的 cache_hit_pct 在 Anthropic 下会超过 100%。② 计价粗：tokens.rs:27-90 按名字子串硬编码价目，tokens.rs:20-23 和 194-196 只算 input/output。③ 云端只记 token、不记金额：run.rs:339-349 把 input+output 累加进 quota_ledger（db.rs:270-276，没有 cost 列），Anthropic 的缓存读写 token 不进配额。④ 会丢账：Usage 和 TextDelta 共用一个容量 256、满了就丢的通道（run.rs:26, 54-58, 359-361），map_task 又在这个通道的消费循环里逐条 await Postgres 写入（run.rs:347）。DB 一慢，通道就满，后续轮次的 Usage 会被丢掉。⑤ 非流式调用不计量（原提案遗漏）：压实摘要走 provider.complete()（rupi-agent/src/lib.rs:1276-1289），ChatResponse 没有 usage 字段（rupi-llm/src/lib.rs:147-151），只有流式路径才发 StreamEvent::Usage。云端压实的输入接近整个上下文窗口，这部分完全不计入配额。⑥ 不落盘：persist_tree 只序列化 node.message（db.rs:1089），SessionTree.usage_by_node（rupi-core/src/lib.rs:187-189）在 load_tree 之后全部丢失（db.rs:1121-1170）。

设计：rupi-core：TokenUsage 增加 reasoning、cache_write_1h、cost_micros 和 cost_source，全部 serde default 以兼容旧 JSONB，并约定 input 不含缓存。rupi-llm：parse_provider_usage 按 provider 归一（OpenAI/Gemini 减去 cached），ChatResponse 增加 usage: Option<TokenUsage>（serde default），流式和非流式路径都填写；压实和复盘调用由 AgentLoop 以 kind=compaction|review 上报。rupi-agent：on_event 是同步回调，所以 Usage 改走一条独立的不丢弃通道（只承载 usage，体量很小，可以用 unbounded，或 tokio::spawn 直接写），不和 TextDelta 共用通道。rupi-server：迁移 v5 新建 usage_events(tenant_id, session_id, run_id, turn, attempt, kind, provider, model, input, output, cache_read, cache_write, reasoning, cost_micros, cost_source, billing byok|platform, price_version, created_at)。主键用 (run_id, turn, attempt, kind)，保证至少一次写入时可以去重，按天分区。表写入不依赖 persist_tree，因为 persist_tree 每次都是全删再全插（db.rs:1074-1078）。quota_ledger 增加 cost_micros。(tenant, day) 是热点行，实时累计放 Redis INCRBY，run 结束时 upsert 一次；权威仍是 usage_events，Redis 丢数据时可以从它重算。tenants 增加 max_cost_micros_per_day。persist_tree 在 blocks 里写入 usage，格式复用 rupi-memory/src/session_io.rs:290 message_blocks_json。

风险：价目表要持续维护和版本化，每条用量记录价目版本。provider 报价和估算不一致时要标明 cost_source。重试和故障转移产生的失败请求是否计费需要明确，部分失败的流至少要记下 message_start 给出的 input。分区表迁移有成本。

### Prompt cache 稳定性：工具顺序确定、历史不重复、历史断点、会话级 cache key [critical/M]
上游：packages/ai/src/api/anthropic-messages.ts:75-97 getCacheControl、1526-1550（末条 user/tool_result 断点）、1655（末工具断点）；packages/ai/src/api/openai-completions.ts:811-816 prompt_cache_key=sessionId；packages/ai/src/api/openai-prompt-cache.ts（截到 64 字符）；pkg/ai/src/types.ts:247-257；ai.CHANGELOG 0.86.0 #9102 #9629、1.0.1 inline-tools、1.4.0 #10940；pkg/coding-agent/src/core/cache-stats.ts:148-168

差距：① 云端每个 run 新建 ToolRegistry（crates/rupi-server/src/tools.rs:13-43，run.rs:310）。注册表是 std HashMap（rupi-tools/src/lib.rs:91-94），每个实例的 RandomState 种子不同，definitions() 按迭代顺序返回（lib.rs:119-121）。所以每个 run 的 tools 数组和 system 里的 <AvailableTools> 列表（rupi-agent/src/lib.rs:98-104, 653-656）顺序都是随机的。Anthropic 的缓存前缀顺序是 tools→system→messages，OpenAI 的自动前缀缓存同样包含 tools，结果是跨 run 的前缀几乎总是 miss。② 历史重复堆积（原提案遗漏）：load_remote_guidance 每个 run 都把 AGENTS.md、CLAUDE.md 和 commands/*.md 作为 Role::System 消息 push 进 tree（run.rs:882-883, 956-959），input.context 也每次 push（run.rs:394-401），finish 时一并落盘（run.rs:724）。同样的内容已经通过 builder.append 放进了 system（run.rs:370-375）。结果是第 N 个 run 的历史里有 N 份 AGENTS.md，按 input 全价计费，而且线性增长，直到压实。③ Anthropic 消息历史上没有断点（anthropic.rs:76-94），多轮工具流中历史每轮都按全价 input 计费；Bedrock 完全不带 cache_control（bedrock.rs:57-88）。④ OpenAI 不发 prompt_cache_key（lib.rs:374-398）。⑤ 会话亲和 id 是构造时生成的随机 uuid（lib.rs:487），云端每个 run 重建 provider（run.rs:313, 850-860），server 从不调用 set_session_id，所以 OpenRouter 亲和只在单个 run 内有效。补充：云端 PostgresMemory 没有实现 prefetch（rupi-server/src/memory.rs:73-106 用的是默认空实现），所以 <Recalled>（rupi-agent/src/lib.rs:699-701）目前不会在云端破坏缓存。

设计：阶段 0（S）：load_remote_guidance 只写入 system（builder.append），不再 push 进 tree；input.context 改为本 run 临时注入、不落盘，或按内容 hash 去重。阶段 1（S）：definitions() 按 name 排序（或改用 BTreeMap）；AgentLoop 合并 tools、mem、skills 之后整体按 name 排序；加单测断言两次构建的序列化 tools 和 system 字节一致。阶段 2（M）：ChatRequest 增加 session_id: Option<String> 和 cache_retention{None,Short,Long}（serde default）。云端填 hash(tenant_id, thread_id)，不暴露原始 id。移除 provider 上的可变 session 状态。Anthropic 和 Bedrock 在最后一条 user 消息（含 tool_result）的末块加 cache_control，加上 system 和末工具共 3 个断点（上限 4 个）。Long 档只对官方端点加 ttl 1h，并提供兼容开关。OpenAI 官方端点发 prompt_cache_key（截到 64 字符）。阶段 3：基于落盘的 usage（见计量提案）导出 cache_miss_tokens 指标（参照 cache-stats.ts，压实后重置），用来发现回归。阶段 4（可选）：discovery 或 MCP 中途新增工具时，用 Anthropic inline-tools 或把新工具挂在消息里，不改动 tools 前缀。

风险：1h TTL 的写入价是 2 倍，要按会话活跃度决定是否启用。部分兼容网关会拒绝未知字段（上游多次为 OpenCode 关掉 prompt_cache_retention），需要 compat 开关。阶段 0 改变了历史形态，旧会话里已经堆积的重复 System 消息需要一次性清理，或在 prompt_history 里去重。frozen memory 变化（memory 工具写入）会让 system 前缀失效，这属于预期行为。

### 错误分类、agent 级重试、流式空闲超时 [critical/M]
上游：packages/ai/src/utils/retry.ts（RETRYABLE / NON_RETRYABLE 模式、retryAssistantCall）；packages/ai/src/utils/provider-retry.ts:1-68（x-should-retry，408/409/429/5xx，retry-after-ms，等待超过 maxRetryDelayMs 默认 60s 即失败，最多 25% jitter）；pkg/ai/src/types.ts:200-217；docs/settings.md:128-136；pkg/coding-agent/src/core/agent-session.ts:3790-3873；pkg/coding-agent/src/core/http-dispatcher.ts:4（300s idle 超时）；ai.CHANGELOG 1.2.0 #10609、0.99.2 #9571、1.4.0 #10920 #11057、1.1.0 #10543

差距：① 只在发请求阶段按状态码重试（crates/rupi-llm/src/lib.rs:907-935），只认 429/5xx（855-857），不认 408/409 和 x-should-retry；退避没有 jitter（860-862）。平台 key 被大量租户共享时，同一秒收到 429 的请求会同步重试。② Retry-After 不封顶：parse_retry_after（865-868）的值直接拿去 sleep（923-927）。上游如果回 3600，run 会睡一小时，期间 heartbeat 持续续约（run.rs:246-267），一直占着租约、并发槽和执行器 handle。③ insufficient_quota 这类额度耗尽的 429 也会重试 3 次。④ 流中错误不重试：Anthropic 的 event:error（如 overloaded）直接 bail（anthropic.rs:726-731），OpenAI 流里的 error 对象也一样（lib.rs:637-644），连接中途断开同样如此（lib.rs:621）。AgentLoop 遇到非溢出错误直接 return Err（rupi-agent/src/lib.rs:761-780），整个 run 失败。⑤ shared_http_client 没有 connect 超时，也没有读超时（lib.rs:453-463）。上游连接卡住时流永远不结束，只有客户端断开或租约丢失才会结束。

设计：rupi-llm 新增 retry.rs，定义 LlmError{kind: Retryable|QuotaExhausted|Overflow|Auth|BadRequest|Aborted, status, retry_after_ms, provider, model, message}。为避免改动所有 provider 的签名，先用 anyhow::Error 承载 LlmError，上层通过 downcast_ref::<LlmError>() 读取；is_overflow_error 也改为优先看 kind。分类规则移植 retry.ts 的两组正则，注意状态码不能匹配 request id 里的数字。provider 层：认 x-should-retry、retry-after-ms、retry-after 和 408/409；等待超过 60s（可配）立即返回 Retryable{retry_after}；退避加 0-25% jitter；provider 层只重试 1-2 次，避免和 agent 层次数相乘。HTTP client：connect_timeout 10s，reqwest 0.12.28 的 read_timeout（块间空闲，默认 120-300s），不设总超时。rupi-agent：只在 LLM 调用阶段重试，此时本轮工具还没执行。已流出的 partial 不写入 session，发 AgentEvent::RetryScheduled{attempt,delay_ms,error_class}，在 select! 里和 CancelFlag 竞速 sleep，然后重发同一个 ChatRequest。重试不占 turn 配额，单独计数，默认 3 次、2s 起、60s 封顶，按套餐可配。QuotaExhausted 和 Auth 直接以 RUN_ERROR code=provider_quota|provider_auth 结束。AG-UI 把重试映射为 CUSTOM 事件，并先关闭已打开的 TEXT_MESSAGE，前端据此清掉部分文本。可选扩展（上游没有）：在 Redis 里按 (provider, credential) 做令牌桶或并发上限，平台 key 被所有租户共享时先在本地排队，而不是打出 429。

风险：重试会让已流出的部分 token 重复计费，失败请求的 usage 也要记账。分类正则误判会白烧重试，比如把配额错误当成瞬时错误。read_timeout 太短会掐断推理模型在首个 chunk 之前的长时间静默；Anthropic 有 ping 事件，OpenAI 兼容网关不一定有，首块之前可以单独设更长的等待。

### 租户凭证：按 provider 严格匹配、云端禁用 mock 兜底、加密存储、请求级 provider 配置 [critical/L]
上游：ai.README.md「Auth / How Auth Resolves」「Credential Store」（read/list/modify 串行化刷新；存储的凭证拥有该 provider；刷新失败不回落 env）、「Provider-Scoped Environment Overrides」；pkg/ai/src/types.ts:164-176；pkg/coding-agent/src/core/runtime-credentials.ts；ai.CHANGELOG 1.0.3、0.99.2（Anthropic workload identity federation）、0.84 #5728（StreamOptions.env）

差距：① key 会发错 provider，包括平台 key：resolve_byok 的兜底分支（crates/rupi-server/src/run.rs:832-840）对 openrouter、azure、bedrock、vertex、llamacpp 和目录里的 extra provider 都取 openai_api_key/api_key，没有就取平台的 RUPI_API_KEY/OPENAI_API_KEY，并作为 opts.api_key 覆盖传入（run.rs:855-859）。provider_from_spec 的 pick_key 会优先使用这个覆盖值（route.rs:80-86），所以即使运维配置了 RUPI_OPENROUTER_KEY 或 AWS_BEARER_TOKEN_BEDROCK，也会被跳过。任何租户只要把 model 设成 openrouter/x 或 bedrock/x（settings 白名单允许，admin.rs:25, 38），平台的 OpenAI key 就会作为 Bearer 发到 openrouter.ai 或 bedrock-runtime.amazonaws.com。这与 docs/LAUNCH.md:98 的承诺不符。② 缺 key 时静默返回 mock：run.rs:859 → route.rs:290-297，未知 provider 也一样。租户拿到 demo mode 的假回答，这次 run 还被计为成功并扣了次数（quota.rs:77）。③ 明文存储：BYOK key 存在 tenants.settings JSONB 里（db.rs:208），只在读取时打码（admin.rs:49-55, 333-335；http.rs:171-186）。Tenant 实现了 Debug 且包含 settings（db.rs:437-438），任何 {tenant:?} 日志都会泄露 key。④ endpoint、region、project 全部读进程 env（route.rs:126-127, 157-159, 176-184；vertex.rs:36-45；bedrock.rs:38-45），无法按租户配置；settings 白名单里的 base_url（admin.rs:43）没有任何代码读取。⑤ 租户没配 key 时回落到平台 env key（run.rs:822-840），计量上不区分 BYOK 和平台付费。⑥ Vertex 用静态 VERTEX_TOKEN（vertex.rs:25-34），大约一小时后过期，不会刷新。

设计：阶段 1（S，先做）：resolve_byok 改为 provider→settings 键的严格映射表（openrouter_api_key、bedrock_api_key 等），不跨 provider 回落，generic api_key 只给 openai。平台 key 只在套餐允许的 provider 上使用，并且只读该 provider 自己的 env 名。云端构造路径不再调用 provider_or_mock：缺凭证时在 preflight 返回 4xx（code=missing_credentials），不扣 run 次数；mock 只在 RUPI_CLOUD_MOCK 或 mock_script 下启用。usage 里记 billing=byok|platform。阶段 2（L）：迁移新建 provider_credentials(id, tenant_id NULL=平台, provider, name, kind api_key|oauth|wif|service_account, ciphertext, wrapped_dek, kek_version, config JSONB{base_url,region,project,location,api_version,deployment_map}, version, status, created_at, rotated_at, last_used_at)。采用信封加密：KEK 来自 KMS 或启动时注入，每行一个 DEK，只在 drive() 内存中解密，绝不写入 Redis 和日志；Tenant 的 Debug 实现要隐去 settings。rupi-llm 新增 ProviderConfig{api_key, base_url, region, project, headers}，提供 provider_from_spec(spec, &ProviderConfig)；云端构造路径不读 env。OAuth 或服务账号刷新用 Redis 短锁加 version 列做 CAS，不要在外部 HTTP 刷新期间持有 PG 的 FOR UPDATE 事务。新 token 先提交再使用。base_url 只允许 https，解析后拒绝私网和元数据地址，并禁止重定向。现有 settings 里的 key 迁移到新表后从 JSONB 中删除。

风险：阶段 1 会让依赖错误回落的现有租户报 missing_credentials，需要先公告并做一次迁移检查。每个 run 都向 KMS 解密有延迟和费用，可以短期缓存 DEK。迁移期间需要双读。密钥轮换需要审计，可以复用 admin_audit。允许租户自带 base_url 会引入 SSRF 风险。

### 模型目录升级为能力表与价目表（Postgres 为权威），并按套餐限制可用模型 [high/M]
上游：pkg/ai/src/types.ts:840-887（input、inputLimits、cost+tiers、contextWindow、maxTokens、thinkingLevelMap、promptCache、compat）；pkg/ai/src/models.ts:1259-1290 clampThinkingLevel；docs/models.md:81-112；ai.CHANGELOG Unreleased（models.dev v2 生成）、1.2.0 personalizeOpenRouterCatalog、0.87.0 #9631

差距：crates/rupi-llm/src/catalog.rs:28-35 的 ModelEntry 只有 id/provider/name/context，内置 models.json 只有 15 条。价目在 crates/rupi-agent/src/tokens.rs:27-90 按名字子串硬编码，云端压实用的上下文窗口也来自这张表。目录由进程全局 Mutex 和 ~/.rupi、RUPI_MODELS 文件组成（catalog.rs:10-13, 195-215），云端 /v1/models 直接返回它（rupi-server/src/http.rs:219-233）。目录里没有 maxTokens、输入模态、可用思考档和缓存 TTL。ThinkingLevel 会把 XHigh/Max 映射成 xhigh 发给任何兼容端点（rupi-llm/src/lib.rs:105-114），图片也会照发给不支持视觉的模型。补充（原提案遗漏）：settings 白名单里有 enabledModels（rupi-server/src/admin.rs:37），但没有任何代码读取。租户可以把 model 设成任意字符串，用平台 key 调用最贵的模型（run.rs:790-847）。

设计：ModelEntry 增加以下字段（均 serde default）：max_tokens；input[text,image]；thinking_map（值为 null 表示不支持该档）；cost{input,output,cache_read,cache_write,tiers[{input_tokens_above,...}]}，单位 $/1M；prompt_cache{short_s,long_s}；image_limits{max_w,max_h,max_bytes}；compat 开关（supports_prompt_cache_key、supports_long_retention、supports_usage_in_stream）。Postgres 新建 model_catalog(provider, id, version, data JSONB, enabled, plans TEXT[])，以内置 JSON 为种子；各副本内存缓存，靠 Redis pub/sub 失效；admin 面可编辑；每条 usage 记录价目 version。resolve_byok 和会话的 model 只允许落在租户套餐的 plans 集合内，否则 preflight 返回 4xx。rupi-llm 新增 clamp_thinking(model, level)；对不支持视觉的模型，图片改发占位文本。

风险：数据维护负担，建议定期从 models.dev 生成候选，人工审核后发布。旧会话可能引用已下线的模型。plans 字段和租户套餐耦合。

### 输出上限按模型设定；length 截断和畸形参数的工具调用不执行 [high/S]
上游：pkg/agent/src/agent-loop.ts:273-279, 488（stopReason=length 时 failToolCallsFromTruncatedMessage）；ai.CHANGELOG 1.3.0 #10897（argumentsError，以原文报错）、1.1.0 #10497（按 3.5 字符/token 估算输入）；pkg/ai/src/types.ts:444-457, 869-870

差距：AgentLoop 永远传 max_tokens: None（crates/rupi-agent/src/lib.rs:707-714），Anthropic、Bedrock、Gemini、Vertex 因此退回 4096 的输出上限（rupi-llm/src/anthropic.rs:84、bedrock.rs:61、gemini.rs:70、vertex.rs:80），写一个几百行的文件就会被截断。主循环只检查 stop_reason=="error"（rupi-agent/src/lib.rs:784）；Anthropic 的 max_tokens、OpenAI 的 length、Gemini 的 MAX_TOKENS（gemini.rs:663-665 原样保留）截断后，其中的工具调用照样进入执行流程。截断的参数 JSON 解析失败后变成 {}（rupi-llm/src/lib.rs:743、anthropic.rs:591、lib.rs:792-797），不会是半截参数。所以实际后果不是写入半截内容，而是模型只收到缺参数一类的错误，看不到原因，于是反复重试。在云端，write、edit、bash 属于 ask_tools（rupi-server/src/run.rs:363-368），参数为 {} 的调用会先触发一次 AG-UI interrupt，让用户审批一个空参数的调用（rupi-agent/src/lib.rs:915-933）。

设计：目录提供 max_tokens 之前，先用按 provider 的保守默认值（例如 Claude 16k-32k，Gemini 按模型），不必等目录提案落地。之后取 max_tokens = min(model.max_tokens, context_window − 估算输入 − 余量)，输入按 3.5 字符/token 估算，套餐可以再设上限。rupi-llm 把 stop_reason 归一为枚举 Stop/Length/ToolUse/Error/Aborted，原始值保留在 raw_stop_reason。ContentBlock::ToolCall 增加 arguments_error: Option<String>（serde default），解析失败时记录错误和原文片段。遇到 Length 且带工具调用时，这些调用不执行、也不走 policy 审批，每个调用合成一条 ToolResult 错误，提示输出达到上限、请分块写入。带 arguments_error 的调用同样不执行，错误里附上原文（截到 2KB）。

风险：提高 max_tokens 会放大单轮的最坏成本，需要和预算闸门配合。Anthropic 的思考预算必须小于 max_tokens，这一约束已有处理（rupi-llm/src/lib.rs:138-144）。

### 运行中的预算与套餐闸门（先用现有 CancelFlag 和宿主侧 thinking 裁剪，轮级钩子作为可选项） [medium/S]
上游：agent.CHANGELOG.md 0.87.0（prepareRequest、finishTurn；返回 end 后在 turn_end 之后立即停止；error/aborted 硬退出）；agent.README.md:124-167；pkg/agent/src/agent-loop.ts:228-248, 254-301；agent.CHANGELOG 0.99.0（thinkingLevel 记录在消息上）

差距：配额只在入院时检查一次（crates/rupi-server/src/quota.rs:33-40）。之后一个 run 最多跑 12 轮（run.rs:363），中途没有任何检查。思考档来自客户端 state（run.rs:700-721 写入 sessions.thinking_level，run.rs:376-382 应用），没有按套餐裁剪；Max 档的 Anthropic 预算是 32768（rupi-llm/src/lib.rs:126-136）。不过原提案说的无法在请求前修改 thinking 或 max_tokens，对云端并不成立：thinking 每个 run 由宿主设置一次（run.rs:376-382），可以直接在那里裁剪；中途停止也可以由宿主在收到 Usage 后调用已有的 CancelFlag.cancel()（rupi-agent/src/lib.rs:676-678, 747, 997），AgentLoop 会在流中或轮次边界退出。真正缺的是：宿主侧没有累计逻辑；Usage 会被丢（见计量提案）；取消后的 run 返回 Ok(Aborted)，被当成 RUN_FINISHED success（run.rs:421-509），没有区分原因。ToolHook（rupi-agent/src/hooks.rs:37-52）只作用于工具。

设计：阶段 1（S，不改 rupi-agent）：run.rs:376-382 按租户套餐把 thinking 裁剪到 max_thinking。在计量提案新增的不丢弃 usage 回调里，累计本 run 成本，并读取 Redis 里的租户当日累计；超过 run 上限或日上限时，记录 stop_cause=budget 并调用 cancel.cancel()。drive() 看到 stop_cause 后发 RUN_ERROR code=quota_exceeded，不发 success；已落盘的工具结果保持一致。Redis 不可用时，退回本 run 的内存累计加一个保守上限。阶段 2（M，可选）：等模型路由或按请求改 max_tokens 时真正需要了，再给 AgentLoop 加 TurnHook{prepare_request(&mut ChatRequest), finish_turn(&TurnSummary) -> Continue|End(StopReason::BudgetExceeded)}，取代取消这种间接手段。

风险：取消是协作式的，正在进行的工具调用会先跑完，可能多花一轮。在工具链中途结束会让任务停在半路，需要给用户明确提示。回调里不能做慢 IO，每轮一次 Redis GET 或 INCRBY 可以接受。

### 逻辑模型路由与多 provider 故障转移（参照 virtual models，改为配置驱动） [high/L]
上游：docs/virtual-models.md（选择与派发分离；reason=user/continuation/retry/direct；按物理模型的窗口判断是否压实）；pkg/coding-agent/src/core/virtual-models.ts:50-118；ai.CHANGELOG 1.3.0 #10995（换模型时丢弃其他模型的 thinking）；ai.README.md「Cross-Provider Handoffs」

差距：provider 只按租户选一次（crates/rupi-server/src/lib.rs:137-153 → run.rs:790-860）。会话级 model（apply_state 写入，run.rs:700-721）只回显在 state snapshot 里（run.rs:323），不参与路由，UI 显示的模型和实际调用的模型可能不一致。没有任何备用 provider，主 provider 出故障时所有请求都失败。assistant Message 没有 model 字段，provider 字段只记 openai-compat 这类协议名（rupi-core/src/lib.rs:59-65；rupi-llm/src/lib.rs:756, 843），无法按物理模型计费，也无法判断 thinking 来自哪个模型。Anthropic 会回放所有带签名的 thinking（anthropic.rs:176-194），换模型后要先收到 400 再剥离重试（anthropic.rs:297-319, 681-701），多花一次请求。压实摘要也用主模型（rupi-agent/src/lib.rs:1276-1289）。

设计：第一步（S）：会话 model 在租户允许的集合内生效（依赖目录提案的 plans），修正显示和实际调用不一致的问题。Postgres 新建 model_routes(id, tenant_id NULL=平台级, name 如 rupi/auto, targets JSONB 有序列表[{provider,model,credential_ref,weight,max_thinking}], direct_target, version)。rupi-llm 新增 ModelRouter 实现 LlmProvider，AgentLoop 不需要感知。ChatRequest 增加 route_reason，由 AgentLoop 设置：首轮 User，工具之后 Continuation，重试 Retry，压实 Direct。规则：Continuation 沿用上一条成功响应的物理目标，保住缓存和签名；Retry 且错误可重试或过载时，切到下一个健康目标；Direct 走 direct_target。目标健康状态用 Redis 熔断键表示，粒度 (provider, model, credential)，短 TTL，各副本共享。Message 增加 model 和 api 字段（serde default），usage 记录物理目标。发送前，丢弃 model 和当前目标不同的 assistant 消息里的 Thinking/RedactedThinking。上下文窗口按本次目标判断，不够就先压实。第一版不做 router state。

风险：切换模型会丢缓存。工具调用 id 格式需要跨 provider 兼容（Anthropic 要求 ^[a-zA-Z0-9_-]+$）。不同模型的行为差异会影响体验。熔断可能来回抖动。计费归属到物理模型之后，需要和套餐价格解耦。

### LLM 请求级 tracing 和 Prometheus 指标，事件带耗时 [high/M]
上游：pkg/ai/src/telemetry.ts（span pi.ai.request；事件 first_chunk / block；指标 request.duration、first_chunk.duration、tokens{kind}、cost；不记录内容；错误截 500 字符；取消保持 ok）；ai.CHANGELOG 1.4.0；agent.CHANGELOG 1.1.0（工具 durationMs）；ai.CHANGELOG 1.1.0（AssistantMessage.durationMs）；pkg/coding-agent/src/core/cache-stats.ts

差距：/metrics 只有 runs_total、rejects_429、executor used/capacity 四个手写序列（crates/rupi-server/src/http.rs:76-89）。rupi-llm、rupi-agent、rupi-server 没有任何 tracing span（grep instrument/info_span/debug_span 结果为 0），只有 rupi-runtime 的 HTTP access 层（rupi-runtime/src/access_log.rs:49-52）。没有按 provider/model 统计的延迟、首 token 时间、错误率、token、成本和重试次数。AgentEvent::ToolEnd 不带耗时（rupi-core/src/lib.rs:474-482），Usage 也不带请求时长。

设计：rupi-agent 在 complete_streaming 外层开 span llm.request{provider, model, route_reason, stop, tokens, cost_micros}，收到首个 delta 时记 first_chunk 事件。不记录提示、输出和参数，错误信息截到 500 字符。rupi-server 用 prometheus 或 metrics crate 导出 rupi_llm_request_duration_seconds{provider,model,stop}、rupi_llm_ttft_seconds、rupi_llm_tokens_total{kind}、rupi_llm_cost_usd_total、rupi_llm_retries_total{class}、rupi_llm_cache_miss_tokens_total、rupi_tool_duration_seconds{tool}。标签里不放 tenant 或 session，避免高基数；租户维度从 Postgres 的 usage_events 汇总。ToolEnd 和 Usage 增加 duration_ms（serde default），随消息一起落盘。可选通过 OTLP 导出。

风险：model 标签的基数要受目录约束，否则租户随意填的 model 字符串会撑爆时间序列。错误文本可能包含用户数据，需要脱敏和截断。不要在每个 delta 上打点。

### 跨副本运行控制：steer / follow-up / abort [medium/M]
上游：agent.README.md「Steering and Follow-up」(390-427)；agent.CHANGELOG 0.87.0 peekQueuedMessages；pkg/agent/src/agent-loop.ts:184-226, 304-317

差距：AgentLoop 已经支持 inbox（crates/rupi-agent/src/lib.rs:231-232, 283, 822, 1105；queue.rs），但 rupi-server 从不调用 with_inbox，运行中再发消息只会得到 409（run.rs:146-161）。取消只能靠 SSE 断开（http.rs:91-107, 639-646），路由表里没有 abort 接口（http.rs:24-41）。用户换个设备就停不了正在跑的 run，网络一抖又会误杀长 run。

设计：新增 POST /v1/sessions/{id}/steer、/follow_up、/abort。消息先写入 Postgres run_inbox(id, tenant_id, session_id, run_id, kind, payload, created_at, consumed_at) 保证持久，再 Redis PUBLISH run:{tenant}:{thread}。持有租约的副本（lease payload 里带 instanceId，quota.rs:20-22）在 drive() 里订阅这个频道，把消息喂给 MessageInbox，把 abort 转成 CancelFlag.cancel()。副本崩溃后，没消费的 inbox 消息在下一个 run 开始时注入。SSE 断开是否取消 run 改为可配置。

风险：至少一次投递可能导致重复注入，需要按 id 去重；跨区域会话需要路由到所属区域；需要理清与 interrupt/resume 流程的交互。

### 图片入口归一化与外置存储 [medium/M]
上游：docs/models.md:81-102（inputLimits.images.resize：2000×2000、4.5MiB、JPEG q80，只编码一次，换模型不重写历史图片）；ai.CHANGELOG 0.87.0 #9631、0.87.1 #9797、1.4.0 #11016（413 按溢出处理）；docs/virtual-models.md（不支持图片的模型收到占位符）

差距：AG-UI 的 data 图片原样接收（crates/rupi-server/src/agui.rs:437-450）。URL 图片只做前缀检查，放行 https:// 和 http://127.0.0.1（agui.rs:390-393）。后者的前缀匹配也会放行 http://127.0.0.1.attacker.example 这样的主机，并允许控制面去请求本机回环上的服务。reqwest 默认跟随重定向，可以被用来访问内网服务（属于盲 SSRF，非 image/* 的响应会被拒绝）。图片不缩放，以 base64 内联写进 messages.blocks（db.rs:1089）和 Redis 会话树缓存（run.rs:725-730），每轮都重放。溢出检测只认没有 body 的 400/413（rupi-llm/src/overflow.rs:35）。

设计：在入口（agui.rs）解码后按目录的 image_limits 缩放并重编码一次（spawn_blocking，限制并发），以 sha256 为 key 写入对象存储（复用 rupi-runtime 的 S3ObjectStore，key 为 {region}/img/{tenant}/{sha}）。ContentBlock::Image 增加可选的 object_ref（serde default），构造请求时按 ref 取字节，副本内用 LRU 缓存。URL 抓取禁止重定向，解析后拒绝私网、回环、链路本地和元数据地址，并限制图片数量。任何 413 都判为溢出，压实时优先丢弃旧图片。

风险：缩放消耗控制面 CPU；每轮从对象存储取图有延迟；历史数据需要迁移。

### 请求卫生与请求身份：几处已知 400，加上终端用户标识 [medium/S]
上游：ai.CHANGELOG 1.3.0 #10794（错误 tool_result 带图片会 400）、1.4.0 #8720（空白工具输出改用占位符）、0.6x（OpenAI 空工具输出发 "(no tool output)"）、1.4.0 client identity；pkg/ai/src/types.ts:264-269 metadata；packages/ai/src/api/anthropic-messages.ts:1306-1311（metadata.user_id）

差距：rupi-llm/src/anthropic.rs:203-224 把图片放进 tool_result.content，is_error=true 时也一样，会触发上游已经修过的 400。空或纯空白的工具输出原样发送（rupi-llm/src/lib.rs:312；anthropic.rs:209-211 会生成 text 为空串的块；rupi-server/src/tools.rs:271-282）。像 mkdir 这种成功但没有输出的 bash 就会产生这种结果，部分 provider 会拒收。请求不带 User-Agent；OpenRouter 的归属头写死了（lib.rs:548-550）；也不带终端用户标识。共用平台 key 时，一个滥用租户可能让 provider 封掉整个 key。

设计：Anthropic 和 Bedrock：is_error 的结果把图片移到 tool_result 之后。空白工具输出统一换成占位文本。ChatRequest 增加 identity{user_agent, attribution, end_user}；云端 end_user 取 HMAC(tenant_id)，映射到 Anthropic 的 metadata.user_id 和 OpenAI 的 user/safety_identifier。User-Agent 设为 rupi/<version>。

风险：兼容网关可能拒绝未知字段，需要 compat 开关；end_user 必须不可逆。

### 可恢复的长请求：provider 侧后台响应句柄（远期） [low/XL]
上游：pkg/ai/src/types.ts:274-283, 326-331, 382, 539-549（DeferredHandle、fetchDeferred/cancelDeferred、SimpleStreamOptions.deferred）；telemetry.ts pi.ai.poll；ai.CHANGELOG 0.84.x #7339

差距：LLM 流绑定在发起请求的那个副本的 reqwest 连接上（crates/rupi-agent/src/lib.rs:717-749）。副本崩溃后流就丢了，租约过期后 run 被回收（quota.rs:122），已经生成的 token 照样付费，结果却拿不到。rupi 也没有 OpenAI Responses API 适配。

设计：先实现 OpenAI Responses 适配（background=true）。DeferredHandle 存进 Postgres，新副本接管租约后继续轮询结果，结果写入会话后接着跑 agent 循环；abort 时尽量取消上游请求。

风险：支持的 provider 很少；轮询有延迟和费用；与流式 UI 的体验冲突；建议等重试和故障转移落地之后再评估。

## 不借鉴
- OAuth 订阅登录（Sign in with ChatGPT，Anthropic/Copilot/Meta/Kimi 订阅，设备码，浏览器回调服务器；ai.CHANGELOG 0.99.0/1.0.0/1.1.0/Unreleased）：消费级订阅不能在 SaaS 里多租户转售或代用，回调服务器也不适合无状态控制面。云端只保留 API key、服务账号和 workload identity federation。
- 空闲 cache warming（coding-agent/src/core/cache-warmer.ts 的 idle 模式，按 15% 续聊概率估算）：会在海量空闲会话上投机发请求，成本和流量不可控。运行中长工具调用期间的预热，等计量和 promptCache 元数据就绪后再评估。
- 由扩展注册、可执行任意 JS 的 virtual model route()（docs/virtual-models.md）：云端不能运行租户代码来做路由，改用 Postgres 里的声明式路由配置。
- 进程级全局注册表（pkg/ai/src/session-resources.ts 的全局 Set；rupi 的 catalog.rs:10-13 extra_lock 和 ~/.rupi/providers.json）：无状态多副本下，全局可变状态会不一致。等真有按会话的传输层时，再把清理挂到 reclaim.rs 的空闲回收和租约释放上。
- 分类器模型（System One/Jev、openai-decisions）和图片生成模型：和编码 agent 的主路径无关，路由前调用分类器还会推迟首 token。
- StreamingJsonParser / ToolCallArgumentsStream（Unreleased #9265）：它解决的是每个 delta 都重新解析一遍的二次方开销。rupi 只追加字符串，结束时解析一次（rupi-llm/src/lib.rs:716-718, 743），没有这个问题。
- streamProxy（浏览器经后端代理，agent.README.md Proxy Usage）：rupi-server 本身就是代理，前端走 AG-UI。
- onProviderStreamEvent 原始事件观察（agent 0.99.0）：会暴露 provider 私有载荷，容易把内容带进日志。调试需求用 debug 级 tracing 就够了。
- 整体改成 transcript system message 模型（ai 0.86.0 的 breaking change）：改动面大，主要收益是中途改提示或工具时不破坏缓存。先借用系统提示保持稳定的原则和 Anthropic 断点做法。
- models.dev 全量目录和按 key 个性化的目录：SaaS 应该维护一份经过审核的小目录，按套餐开放。
- 各 provider 的个别怪癖修复（Kimi/MiMo/Mistral/OpenCode/GPT-6 configuration_update 等）：只在实际接入对应 provider 时按需移植。
- 本机 auth 文件权限（0600）和 coding-agent 的 HTTP 代理/超时 UI 选项：只适用于本机 CLI。
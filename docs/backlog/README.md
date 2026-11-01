# 待办总表

2026-10-31 审查得到的全部发现（F 编号）与对标上游 Pi v1.4.0 的借鉴条目（B 编号）。行号基于 `c293c78`。级别：P0 = high，P1 = medium，P2 = low。不同审查报过的同一问题保留了重复条目，处理时以编号最小的为准。现状与计划见 [`../ROADMAP.md`](../ROADMAP.md)。

- 各 track 的证据与修法：[`tracks/`](tracks/)
- 目标架构：[`upstream-pi/architecture.md`](upstream-pi/architecture.md)
- 借鉴条目详情：[`upstream-pi/ranked.md`](upstream-pi/ranked.md)；领域研究：`upstream-pi/study-*.md`

## 发现（258 条）

| F | 级别 | 类型 | 来源审查 | 标题 | track |
|---:|---|---|---|---|---|
| 0 | P0 | bug | tools-ext-pkg-skills | JSON-RPC 扩展在所有本机前端启动即 panic（current_thread 运行时 + block_in_place） | local-frontends-fixes |
| 1 | P0 | incomplete | tools-ext-pkg-skills | 扩展 UI dialog（select/confirm/input/editor）在 TUI/REPL 无人应答，RPC 流式期间被吞 | local-frontends-fixes |
| 2 | P0 | bug | tools-ext-pkg-skills | 包内扩展脚本不随包安装，manifest 的相对路径按进程 cwd 解析，装好的扩展跑不起来 | tools-ext-fixes |
| 3 | P0 | bug | tools-ext-pkg-skills | truncate_middle 用字节长度判断、按字符数截取，非 ASCII 文本被重复输出 | tools-ext-fixes |
| 4 | P0 | bug | tools-ext-pkg-skills | read 分页在 12KB 处停页后又做中间截断，页内静默丢行；也没有用上移植来的 truncate_head | tools-ext-fixes |
| 5 | P0 | bug | tools-ext-pkg-skills | bash 遇到后台子进程会无限等待管道关闭，超时和取消都失效 | tools-ext-fixes |
| 6 | P1 | bug | tools-ext-pkg-skills | bash 子进程继承宿主 stdin：读 stdin 的命令挂到超时，RPC/TUI 下还会和宿主抢输入 | tools-ext-fixes |
| 7 | P1 | bug | tools-ext-pkg-skills | oneshot 扩展：孙进程不随超时/取消被杀、大参数写 stdin 无视超时、stderr 不截断 | tools-ext-fixes |
| 8 | P1 | bug | tools-ext-pkg-skills | 热重载删除或修改 JSON-RPC 扩展后子进程泄漏（RpcHost 与 incoming 任务形成 Arc 环） | tools-ext-fixes |
| 9 | P1 | incomplete | tools-ext-pkg-skills | `rupi install -l` 装到项目里的扩展运行时不会被加载，`rupi config list` 却显示 on | tools-ext-fixes |
| 10 | P1 | bug | tools-ext-pkg-skills | 跨包同名资源互相覆盖，卸载一个包会误删另一个包的文件 | tools-ext-fixes |
| 11 | P1 | bug | tools-ext-pkg-skills | git 安装钉短 SHA 或无效 SHA 时拉取失败仍报成功，装的是默认分支、锁文件却记成该 SHA | tools-ext-fixes |
| 12 | P1 | bug | tools-ext-pkg-skills | REPL/TUI 每轮 skills.refresh 丢掉 settings 启停过滤和 --skill 单文件 | tools-ext-fixes |
| 13 | P1 | incomplete | tools-ext-pkg-skills | RPC `prompt` 不展开斜杠命令、skill 和扩展命令，`get_commands` 却把它们列出来 | tools-ext-fixes |
| 14 | P1 | incomplete | tools-ext-pkg-skills | 扩展事件订阅：turn_start/session_start 从不投递，session_end 实为每轮 RunEnd | tools-ext-fixes |
| 15 | P1 | bug | tools-ext-pkg-skills | 沙箱 grep/glob 会跟随工作区内指向外部的文件符号链接，与 read 的口径不一致 | tools-ext-fixes |
| 16 | P1 | perf | tools-ext-pkg-skills | ToolRegistry 用 HashMap，工具顺序每个进程随机、热重载后也会变，破坏 prompt cache | tools-ext-fixes |
| 17 | P1 | incomplete | tools-ext-pkg-skills | register* 协议只做了一半：keybinding 只能改 8 个内建动作且只读一次，provider 永久写盘且不能注销，command 不去重 | tools-ext-fixes |
| 18 | P1 | missing-feature | tools-ext-pkg-skills | 扩展不能拦截或改写工具调用，一个 manifest 也只能提供一个工具 | tools-ext-fixes |
| 19 | P1 | bug | tools-ext-pkg-skills | edit 不处理 CRLF/BOM，CRLF 文件用 LF 的 old_string 必然匹配失败 | tools-ext-fixes |
| 20 | P1 | perf | tools-ext-pkg-skills | 云端每次请求都往 /tmp 写新的 skill 目录，从不清理 | tools-ext-fixes |
| 21 | P2 | incomplete | tools-ext-pkg-skills | 扩展热重载在各前端不一致：TUI 只在 /reload 时刷新、RPC 没有重载，运行中启停扩展不生效 | tools-ext-fixes |
| 22 | P2 | bug | tools-ext-pkg-skills | `rupi config list skills` 用 cwd 相对的 skills/builtin，仓库外运行看不到内建 skill | local-frontends-fixes |
| 23 | P2 | bug | tools-ext-pkg-skills | 搜索类工具截断与排序错误：glob 静默截到 200、find 总报“1 more”、grep 行号按字符串排序 | tools-ext-fixes |
| 24 | P2 | bug | tools-ext-pkg-skills | read 遇到非 UTF-8 文本整文件报错；无 path 的沙箱 grep/glob/find/ls 落到进程 cwd 而不是 root | tools-ext-fixes |
| 25 | P2 | bug | tools-ext-pkg-skills | bash 超时时丢弃已经产出的输出（取消路径会保留） | tools-ext-fixes |
| 26 | P2 | missing-feature | tools-ext-pkg-skills | 工具参数与上游 Pi 存在缺口：grep 无 ignoreCase/literal/context、bash 无 timeout 别名/full_output_path、skill 无 disable-model-invocation | tools-ext-fixes |
| 27 | P2 | missing-feature | tools-ext-pkg-skills | 包管理缺 `rupi update`，npm 不支持 dist-tag 也不校验完整性 | tools-ext-fixes |
| 28 | P1 | test-gap | tools-ext-pkg-skills | 扩展协议缺端到端测试：CLI 级 jsonrpc、UI dialog 往返、register* 走线、热重载进程回收 | tools-ext-fixes |
| 29 | P1 | test-gap | tools-ext-pkg-skills | Windows/PowerShell 路径从未编译或运行：CI 无 Windows，单测含未加 cfg 的 unix 代码 | tools-ext-fixes |
| 30 | P2 | test-gap | tools-ext-pkg-skills | pkg 与截断缺少关键用例：同名冲突、git ref 失败、npm 解包、多字节截断、分页完整性 | tools-ext-fixes |
| 31 | P2 | simplify | tools-ext-pkg-skills | 工具注册与搜索实现重复：glob≈find、with_sandboxed_builtins 逐个手写、oneshot execute 两份 | tools-ext-fixes |
| 32 | P2 | simplify | tools-ext-pkg-skills | 死代码与无用依赖：register_all、Utf8Decoder::push、truncate_head、未用参数、未用 crate | tools-ext-fixes |
| 33 | P2 | simplify | tools-ext-pkg-skills | rupi-pkg 重复实现 skill/扩展的名称校验和 frontmatter 解析，与发现侧口径会漂移 | tools-ext-fixes |
| 34 | P2 | doc-drift | tools-ext-pkg-skills | UPSTREAM/README 关于 find/ls、已知未做、事件 tag、扩展截断的描述过时 | tools-ext-fixes |
| 35 | P0 | bug | cli-upstream | JSON-RPC 扩展在任何入口都让 rupi 启动即 panic（current_thread + block_in_place） | local-frontends-fixes |
| 36 | P0 | bug | cli-upstream | RPC 模式会话落盘只在 EOF 时全量写一次：--continue、clone、new_session、被 kill 都会丢回合 | local-frontends-fixes |
| 37 | P0 | bug | cli-upstream | 会话切换后重建 provider 时丢失 provider 前缀和 CLI 选项，OpenRouter/Azure/Bedrock/Vertex/llama.cpp 被静默换成 mock 或打到错误端点 | local-frontends-fixes |
| 38 | P1 | bug | cli-upstream | tracing 日志写到 stdout，污染 `run --json` 和 `--mode rpc` 的 JSONL；--review-apply 也往 stdout 打印 | local-frontends-fixes |
| 39 | P1 | bug | cli-upstream | REPL 的 /import、/export 遇到 I/O 错误时整个 REPL 进程退出 | local-frontends-fixes |
| 40 | P1 | bug | cli-upstream | 缺 key 时静默降级为 mock：-p/run/rpc 退出码 0 并输出假回复，REPL /model 显示“switched” | local-frontends-fixes |
| 41 | P1 | bug | cli-upstream | 每次启动都立刻建会话行，`-c/--continue` 会续到最近一个空会话，而不是最近的真实对话 | local-frontends-fixes |
| 42 | P1 | incomplete | cli-upstream | RPC 缺 7 个上游命令，get_state/get_session_stats 字段不全，auto-retry 开关是空壳 | local-frontends-fixes |
| 43 | P1 | incomplete | cli-upstream | RPC prompt 不展开 prompt 模板、skill 和扩展命令，但 get_commands 把它们列为可调用 | local-frontends-fixes |
| 44 | P1 | incomplete | cli-upstream | --mode rpc 静默忽略 --approve、--subagents、--review*，四个入口的能力不一致 | local-frontends-fixes |
| 45 | P1 | incomplete | cli-upstream | `--mode` 接受任意值，`--mode json` 静默输出纯文本；README 声称已对齐 | local-frontends-fixes |
| 46 | P1 | bug | cli-upstream | `rupi cloud` 在没有 RUPI_CLOUD_KEY 时把本机 LLM key（RUPI_API_KEY）当 Bearer 发给控制面 | local-frontends-fixes |
| 47 | P1 | incomplete | cli-upstream | `rupi cloud prompt` 不流式，有 120s 总超时，不处理 interrupt；遇到一次 Ask 后线程永久 409 | local-frontends-fixes |
| 48 | P1 | incomplete | cli-upstream | REPL 与 TUI 斜杠命令不对齐：REPL 缺 /resume 和 !cmd，自己的提示却让用户用 /resume | local-frontends-fixes |
| 49 | P1 | incomplete | cli-upstream | Pi 的 settings 只读合并后不生效：defaultModel、defaultProvider、defaultThinkingLevel 没有别名 | local-frontends-fixes |
| 50 | P1 | incomplete | cli-upstream | `--approve` 与上游同名但语义相反；上游的 -a/-na、-v、--models、--prompt-template、--theme 等 flag 缺失 | local-frontends-fixes |
| 51 | P1 | missing-feature | cli-upstream | MCP 只能每次传 --mcp-config，没有默认的 mcp.json 发现，也没有 settings 键或 mcp 子命令 | local-frontends-fixes |
| 52 | P1 | incomplete | cli-upstream | 扩展 UI 对话框（select/confirm/input）只有 RPC 端消费，TUI 和 REPL 永远等到 30s 超时后取消 | local-frontends-fixes |
| 53 | P1 | simplify | cli-upstream | 四个入口（run_once/run_chat/run_tui/run_rpc）的装配代码大段复制，约 300 行可以合并 | local-frontends-fixes |
| 54 | P1 | simplify | cli-upstream | /new、/fork、/clone、/import、/export、/name、/session 在 REPL、TUI、AgentSession 各实现一遍 | local-frontends-fixes |
| 55 | P2 | simplify | cli-upstream | 死代码：TUI dispatch_builtin 中被前置拦截的分支、未使用的函数、Resolved.theme | local-frontends-fixes |
| 56 | P2 | bug | cli-upstream | `rupi config list skills` 用相对路径 skills/builtin，换了 cwd 内建 skill 就显示 (none) | local-frontends-fixes |
| 57 | P2 | bug | cli-upstream | --name 在 -c/--resume/--session 时被忽略；RPC 的 --name 只在内存里，不落库 | local-frontends-fixes |
| 58 | P2 | bug | cli-upstream | 全局 flag 必须写在子命令之前；crate README 宣传的 `run -p` 实际会被拒绝 | local-frontends-fixes |
| 59 | P2 | bug | cli-upstream | `--tools` 遇到未知名、`+name` 或 glob 时静默变成空工具表 | local-frontends-fixes |
| 60 | P2 | perf | cli-upstream | TUI 每个事件循环都重新扫描命令目录并读取每个 .md 全文 | local-frontends-fixes |
| 61 | P1 | test-gap | cli-upstream | 二进制级测试没有覆盖 JSON-RPC 扩展、RPC 增量落盘、JSONL 纯净度和 provider 重建 | local-frontends-fixes |
| 62 | P2 | doc-drift | cli-upstream | UPSTREAM.md 的“已知未做”有过期项，RPC/CLI 对照表把部分对齐写成了完全对齐 | local-frontends-fixes |
| 63 | P0 | bug | mcp-memory-config | Streamable HTTP 握手把 notifications/initialized 当成带 id 的请求发出，官方 SDK server 直接拒绝，HTTP 传输无法使用 | mcp-memory-sessions |
| 64 | P0 | bug | mcp-memory-config | --mcp-config 格式过严：url-only server 报缺 command，不写 env 报缺字段，标准 mcpServers 对象格式不认，解析失败还会直接让整个程序退出 | mcp-memory-sessions |
| 65 | P0 | bug | mcp-memory-config | 项目层 .rupi/settings.json 不经信任门：可以自设 defaultProjectTrust=always 给自己授信，也可以通过 extensions 注入扩展工具 | local-frontends-fixes |
| 66 | P0 | bug | mcp-memory-config | /settings 写回时遇到语法不合法的 settings.json 会把整个文件替换为 {key}，用户其余配置静默丢失 | local-frontends-fixes |
| 67 | P0 | bug | mcp-memory-config | tracing 日志写到 stdout：配置或 MCP 的 warning 会污染 --mode rpc 和 run --json 的 JSONL 输出 | local-frontends-fixes |
| 68 | P0 | bug | mcp-memory-config | 任何 jsonrpc 协议扩展都会让 run/chat/tui 启动即 panic（current_thread 运行时里调用 block_in_place） | local-frontends-fixes |
| 69 | P0 | bug | mcp-memory-config | memory 工具的 replace/remove 是全文子串操作：remove 会误伤其他词，replace 缺 \|\|\| 时把整个 MEMORY.md 覆盖成 entry | mcp-memory-sessions |
| 70 | P0 | bug | mcp-memory-config | 导入 Pi JSONL 时丢掉 model_change/thinking_level_change 等条目，父链断开，叶子路径为空：HTML 导出为空，--session 导入的消息数随机 | mcp-memory-sessions |
| 71 | P0 | bug | mcp-memory-config | 导出 JSONL 时，一轮多个工具结果的后续 assistant 挂在第一个 toolResult 下，其余结果落到死分支，Pi 和 rupi 重新导入后都缺 tool_result | mcp-memory-sessions |
| 72 | P0 | bug | mcp-memory-config | sessions.db 不存父子关系，resume 时线性回放：/rewind 后被丢弃的分支重新进入模型上下文 | mcp-memory-sessions |
| 73 | P1 | bug | mcp-memory-config | stdio/WS server 崩溃时在途调用要等满 30s 才报错；所有调用固定 30s 超时；reqwest 的 30s 总超时会切断 GET 常驻流和长 SSE；不会重启 server | mcp-memory-sessions |
| 74 | P1 | bug | mcp-memory-config | MCP 工具名 `{server}_{tool}` 不做规范化，含 `.`、`/`、空格或超过 64 字符时会让整个 LLM 请求被拒 | mcp-memory-sessions |
| 75 | P1 | incomplete | mcp-memory-config | tools/call 只取 text 块：image/audio/resource 内容丢弃，structuredContent 采集后没人用；content 没有 text 时返回空串 | mcp-memory-sessions |
| 76 | P1 | missing-feature | mcp-memory-config | MCP 没有自动发现配置（mcp.json 或 settings 键），没有 /mcp 管理命令；rpc/run 不处理 list_changed；云端完全没有 MCP | local-frontends-fixes |
| 77 | P1 | bug | mcp-memory-config | 记忆密钥扫描用子串匹配，误伤正常文本（disk-usage、task-runner、Slovakia、“private key” 这类说明） | mcp-memory-sessions |
| 78 | P1 | bug | mcp-memory-config | FTS 记忆镜像只增不删：remove 反而把被删文本插入为新记忆，replace 镜像的是 `OLD\|\|\|NEW` 原文，压实丢掉的行永远留在 memory_search 结果里 | mcp-memory-sessions |
| 79 | P1 | bug | mcp-memory-config | trigram FTS 用整串短语查询：少于 3 个字符（如“乌龙”“部署”“Go”）永远查不到，多词查询必须原样相邻才命中；session_search 没有兜底 | mcp-memory-sessions |
| 80 | P1 | bug | mcp-memory-config | 设置 sessionDir 或 --session-dir 后，模型的 session_search 工具仍查 RUPI_HOME/sessions.db，查不到任何会话 | local-frontends-fixes |
| 81 | P1 | incomplete | mcp-memory-config | USER.md 只读不写：引导语告诉模型 USER.md 存用户偏好，但 memory 工具和 CLI 都只能写 MEMORY.md | mcp-memory-sessions |
| 82 | P1 | bug | mcp-memory-config | 本地会话库不存 summary_through：resume 后摘要被当成只覆盖第一条消息，prompt 变成摘要加几乎全部历史 | mcp-memory-sessions |
| 83 | P1 | bug | mcp-memory-config | TUI 的 /resume 和 /sessions 只回放最早 500 行，长会话会丢掉最新的消息；CLI 则静默截在 10000 行 | mcp-memory-sessions |
| 84 | P1 | bug | mcp-memory-config | settings.skills 的 `!name` 禁用只在启动时生效，REPL/TUI 每轮 refresh 后被禁用的 skill 又回来了 | tools-ext-fixes |
| 85 | P1 | bug | mcp-memory-config | settings 是强类型整文件反序列化：任意一个键类型不对就丢弃整个文件；Pi 合法的取值（quietStartup:"header"、对象形式的 packages）会让整层 Pi 设置失效 | local-frontends-fixes |
| 86 | P1 | incomplete | mcp-memory-config | “只读合并 ~/.pi/agent/settings.json”只对同名键有效：Pi 实际的键名 defaultModel、defaultProvider、defaultThinkingLevel 都不读，Pi 的 model/thinking 设置不会生效 | local-frontends-fixes |
| 87 | P1 | incomplete | mcp-memory-config | settings.packages 运行时没有任何下游读取：手写 `"packages":["!npm:foo"]` 不会禁用该包的资源 | local-frontends-fixes |
| 88 | P1 | doc-drift | mcp-memory-config | JSONL 导出号称贴 Pi session-format v3，但有多处硬编码或丢失：toolName 固定为 "tool"、model 为空、api 固定 openai-completions、stopReason 固定 stop、图片丢弃、compaction 挂在侧枝上 | mcp-memory-sessions |
| 89 | P1 | test-gap | mcp-memory-config | MCP 测试只对自写 stub：不校验 notification 不带 id，没有崩溃、超时、配置格式和 HTTP/WS 的 list_changed 用例 | mcp-memory-sessions |
| 90 | P1 | test-gap | mcp-memory-config | 记忆和会话 IO 的关键行为没有测试：replace/remove 语义、镜像一致性、短查询召回、分支持久化、多工具 JSONL 往返、Pi 非 message 条目 | mcp-memory-sessions |
| 91 | P1 | simplify | mcp-memory-config | rupi-mcp 内部大量重复：6 个 spawn 构造函数、3 份分页循环、3 个工具实现各写一遍 execute/execute_with_cancel、两套 RPC call_timeout | mcp-memory-sessions |
| 92 | P2 | bug | mcp-memory-config | mcp-list 参数解析：以 `-` 开头的 server 参数（npx -y、python -I）必须加 `--`；给了 --url 仍要求 COMMAND | local-frontends-fixes |
| 93 | P2 | incomplete | mcp-memory-config | MCP 协议细节缺失：取消不发 notifications/cancelled、protocolVersion 写死且不看协商结果、HTTP 404 时不重新 initialize、roots URI 不做转义 | mcp-memory-sessions |
| 94 | P2 | incomplete | mcp-memory-config | quietStartup、enabledModels、externalEditor 只在 TUI 生效：REPL 横幅不受控，/model 和 RPC get_available_models 不按 enabledModels 过滤 | local-frontends-fixes |
| 95 | P2 | bug | mcp-memory-config | settings.thinking 取值非法时直接终止启动，与“非法配置只 warning”的设计不符；Pi 的 minimal 档也被拒 | local-frontends-fixes |
| 96 | P2 | doc-drift | mcp-memory-config | 记忆上限按字节计算，文档写的是“5000 字符”，中文记忆实际只有约 1/3 容量 | mcp-memory-sessions |
| 97 | P2 | incomplete | mcp-memory-config | MemoryProvider 的 on_session_end 和 shutdown 生命周期钩子没有任何调用方 | local-frontends-fixes |
| 98 | P2 | perf | mcp-memory-config | 会话库没有 messages(session_id) 索引，每次 resume/export 都全表扫描加临时排序；JsonlProvider 每轮同步读两遍整个 turns.jsonl | mcp-memory-sessions |
| 99 | P2 | bug | mcp-memory-config | 会话 id 前缀解析只在最近 200 个会话里找，更早的会话无法用短 id 打开 | mcp-memory-sessions |
| 100 | P2 | simplify | mcp-memory-config | rupi-config 和信任逻辑重复，有死代码：merge 逐字段写 if、两套 on/off 和列表解析、未使用的 is_core_key/settings_specs/TrustPolicy | local-frontends-fixes |
| 101 | P0 | bug | llm | 空工具输出在 Anthropic/Bedrock 侧生成空 text 块，下一次请求 400，会话无法自愈 | llm-wire-compat |
| 102 | P0 | bug | llm | Gemini/Vertex 流式累积器把同一轮里同名的并行工具调用合并成一个，前面的调用丢失 | llm-wire-compat |
| 103 | P0 | bug | llm | Gemini 3 的 thoughtSignature 既不保存也不回放，多步工具调用从第二次请求起 400 | llm-wire-compat |
| 104 | P0 | bug | llm | 请求体与当前一代模型不兼容：总发 temperature、只会 budget_tokens、reasoning_effort 乱发 | llm-wire-compat |
| 105 | P0 | bug | llm | OpenAI-compat 丢弃 reasoning_content/reasoning：DeepSeek V4 思考模式加工具时按文档会 400，TUI 也看不到推理过程 | llm-wire-compat |
| 106 | P0 | bug | llm | 输出上限固定 4096 且无配置项，截断后的工具调用被静默当成 `{}` 参数执行 | llm-wire-compat |
| 107 | P1 | bug | llm | usage 口径各家不一致：Anthropic 的 input 不含缓存，导致成本、命中率、配额失真；Gemini 思考 token 未计入 | llm-wire-compat |
| 108 | P1 | bug | llm | 工具结果图片挂到下一个 tool_result 上（Anthropic/Bedrock） | llm-wire-compat |
| 109 | P1 | bug | llm | 错误回包处理不统一：非 JSON 错误丢状态码，Bedrock 错误文案全丢，溢出无法识别，(no body) 正则永不命中 | llm-wire-compat |
| 110 | P1 | incomplete | llm | 流中错误和过载不重试，注释声称主循环会重试，实际直接 abort | llm-wire-compat |
| 111 | P1 | bug | llm | HTTP 客户端没有任何超时，Retry-After 也不设上限，可能无限挂起 | llm-wire-compat |
| 112 | P1 | bug | llm | 无工具请求也发空 tools 数组：OpenAI 官方 API 拒收，压实摘要和 LLM 复盘静默失效 | llm-wire-compat |
| 113 | P1 | bug | llm | `/thinking off` 与"不干预"发出的请求完全相同，默认开思考的模型关不掉 | llm-wire-compat, local-frontends-fixes |
| 114 | P1 | bug | llm | 扩展登记的 provider 缺自身 key 时会回落到用户的 OpenAI/Anthropic/Gemini 主 key，凭据泄露给扩展指定的 base_url | llm-wire-compat |
| 115 | P1 | bug | llm | RPC 切换、fork、clone 会话时只用 model_id 重建 provider，provider 前缀丢失 | local-frontends-fixes |
| 116 | P1 | incomplete | llm | 云端 BYOK 只认三种 key，其余 provider 拿到 OpenAI key；缺 key 时生产环境静默回复 demo 文本 | local-frontends-fixes |
| 117 | P1 | bug | llm | Gemini 合成的调用 id 每轮都从 call-0 起，跨轮重复，回放的 functionResponse 名字被张冠李戴 | llm-wire-compat |
| 118 | P1 | perf | llm | Anthropic 只在 tools 和 system 上设缓存断点，对话历史每轮按全价重算；Bedrock 完全不缓存 | llm-wire-compat |
| 119 | P1 | incomplete | llm | ChatResponse 仍无 usage 字段（overflow.rs 头注释属实）：后台调用不计量，按 usage 判溢出未实现 | local-frontends-fixes |
| 120 | P1 | incomplete | llm | 模型目录只用于展示：context 覆盖不影响压实，也没有价格和能力字段；内置条目已过时 | llm-wire-compat |
| 121 | P1 | bug | llm | 成本和窗口表（rupi-agent/tokens.rs）过时且不计缓存价 | agent-loop-integrity |
| 122 | P2 | bug | llm | SseParser::finish 从未被调用，末尾没有空行的事件会丢 | llm-wire-compat |
| 123 | P2 | bug | llm | is_signature_mismatch 过宽：任何含 "thinking" 的 400 都会去掉思考后静默重试 | llm-wire-compat |
| 124 | P2 | incomplete | llm | 思考内容不流式推送，Bedrock 是伪流式且缺缓存和签名重试 | llm-wire-compat |
| 125 | P1 | simplify | llm | 合并 GeminiProvider 与 VertexProvider（几乎整份重复） | llm-wire-compat |
| 126 | P1 | simplify | llm | 抽出公共 SSE 循环、usage 推送、错误处理和 Anthropic 请求体 | llm-wire-compat |
| 127 | P2 | simplify | llm | 删死代码 from_env，provider 推断去重，去掉重复的亲和设置 | llm-wire-compat |
| 128 | P1 | test-gap | llm | 重试、端到端流式、多工具图片等关键行为缺测试，改环境变量的测试并发有竞态 | llm-wire-compat |
| 129 | P1 | missing-feature | llm | 缺 OpenAI Responses API、Anthropic adaptive thinking/effort、tool_choice 等现代接口 | llm-wire-compat |
| 130 | P2 | bug | llm | OpenRouter/Azure/llama.cpp 回复的 Message.provider 一律写成 "openai-compat" | llm-wire-compat |
| 131 | P2 | doc-drift | llm | 注释和文档与实现不符 | llm-wire-compat |
| 132 | P0 | bug | e2e-probe | 任何 jsonrpc 协议扩展都会让 chat/run/--mode rpc 启动即 panic | local-frontends-fixes |
| 133 | P0 | bug | e2e-probe | --mode rpc 续接已有会话后，新回合整批落盘失败，内容全丢 | local-frontends-fixes |
| 134 | P0 | bug | e2e-probe | memory replace 不带 `\|\|\|` 时会覆盖整个 MEMORY.md，之后 add 还会拼到同一行 | mcp-memory-sessions |
| 135 | P1 | bug | e2e-probe | tracing 日志写到 stdout 且带 ANSI 颜色，污染 --json 和 --mode rpc 的 JSONL | local-frontends-fixes |
| 136 | P1 | bug | e2e-probe | chat REPL 的信任提问和审批提问读的是同一个管道 stdin，会吃掉脚本行；`echo task \| rupi` 什么都不做 | local-frontends-fixes |
| 137 | P1 | bug | e2e-probe | `config disable prompts <name>` 对包安装的 prompt 不生效，config list 却显示 off | tools-ext-fixes |
| 138 | P1 | doc-drift | e2e-probe | README 里的 `rupi mcp-list --url ...` 跑不通；stdio 参数带 `-` 前缀（如 npx -y）会被 clap 拒绝 | local-frontends-fixes |
| 139 | P1 | incomplete | e2e-probe | REPL 没有 /resume，/sessions 的提示却让用户用 /resume；输入后被当普通消息发给模型 | local-frontends-fixes |
| 140 | P1 | incomplete | e2e-probe | /help 在 REPL 和 TUI 都没实现，会当成 prompt 发给模型 | local-frontends-fixes |
| 141 | P1 | bug | e2e-probe | 没有 API key 时静默走 mock：-p/run --json/rpc 都 exit 0，并报 stop_reason=done | local-frontends-fixes |
| 142 | P1 | bug | e2e-probe | 不支持 socks5/socks5h 代理：常见的 `HTTPS_PROXY=socks5h://...` 环境下所有请求失败 | local-frontends-fixes |
| 143 | P1 | bug | e2e-probe | bash 危险命令名单用子串匹配：`rm -rf /tmp/x` 被误拦，`rm -fr /` 和 `rm -rf  /` 绕过 | cloud-run-safety |
| 144 | P1 | bug | e2e-probe | --continue 和 -r 会接回最近一条 0 消息的空会话；每次失败的 -p 和每次 /new 都留下空会话 | local-frontends-fixes |
| 145 | P1 | bug | e2e-probe | memory remove 是全文子串删除；memory_search 搜的是只增不减的镜像表，已删、已改的事实仍能搜到 | mcp-memory-sessions |
| 146 | P1 | incomplete | e2e-probe | 启发式 review 和 --review-apply 把整句用户输入写进记忆，并生成没有价值的 auto-* skill | agent-loop-integrity |
| 147 | P1 | bug | e2e-probe | RPC get_messages 返回的 id 与 get_tree 的节点 id 不是一套，拿 get_messages 的 id 去 fork 会失败 | local-frontends-fixes |
| 148 | P2 | bug | e2e-probe | session-show 不接受短前缀（--session/--resume/--export 都接受），找不到时仍 exit 0 | local-frontends-fixes |
| 149 | P2 | bug | e2e-probe | bash 工具非零退出的错误文本重复写了 exit：`exit exit status: 3: ` | tools-ext-fixes |
| 150 | P2 | bug | e2e-probe | REPL 的 /model 不校验 provider，切到不存在的模型也提示成功，下一轮才报 403 | local-frontends-fixes |
| 151 | P2 | incomplete | e2e-probe | ext-list 忽略 -e/--extension 和 --no-extensions，与运行时加载的集合不一致 | local-frontends-fixes |
| 152 | P2 | incomplete | e2e-probe | --mcp-config 的 env/args 是必填字段，报错不带文件名；只支持裸数组格式 | mcp-memory-sessions |
| 153 | P2 | incomplete | e2e-probe | login 的提示与 --model 支持的 provider 对不上，未知 provider 也 exit 0 | local-frontends-fixes |
| 154 | P2 | incomplete | e2e-probe | JSONL 导出里 assistant 的 model 恒为空，api 恒为 openai-completions | mcp-memory-sessions |
| 155 | P2 | perf | e2e-probe | LLM HTTP client 没有任何超时，连接或流卡住时会无限等待 | local-frontends-fixes |
| 156 | P2 | bug | e2e-probe | 非交互默认跳过项目信任后，memory scope=project 报「no .git above cwd」，误导用户 | local-frontends-fixes |
| 157 | P2 | incomplete | e2e-probe | --no-tools 下系统提示仍点名 memory/memory_search/session_search/load_skill | local-frontends-fixes |
| 158 | P1 | test-gap | e2e-probe | 本次 e2e 发现的几处崩溃和丢数据都没有测试覆盖 | local-frontends-fixes |
| 159 | P0 | bug | tui | 消息区滚动算法错误：最新的至少 2 行（含边框差与折行）永远被裁掉 | local-frontends-fixes |
| 160 | P0 | bug | tui | tracing 日志默认写到 stdout，TUI 全屏期间的 warn/info 会直接覆盖到画面上 | local-frontends-fixes |
| 161 | P0 | bug | tui | 任何 jsonrpc 扩展都会让启动 panic（current_thread runtime 下调用 block_in_place） | local-frontends-fixes |
| 162 | P0 | bug | tui | 切换会话（/resume、/sessions 回车、/new、/fork、/clone、/import、-r、--resume）后视图不重画，屏上显示的是另一个会话 | local-frontends-fixes |
| 163 | P1 | incomplete | tui | TokenMeter 在 TUI 里从不累加；带 context%/$ 的 status_footer 已成死代码，状态栏缺少 README 承诺的 context% 和 $ | local-frontends-fixes |
| 164 | P1 | bug | tui | 输入框只有一行：换行看不见、长输入被截断、中文光标错位 | local-frontends-fixes |
| 165 | P1 | bug | tui | Shift+Enter 换行在大多数终端不生效（没开 keyboard enhancement），运行中更是无法插入换行 | local-frontends-fixes |
| 166 | P1 | bug | tui | keybindings.json 改绑 send/newline 后 TUI 无法发送；Ctrl/Alt+字母会被当普通字符插入 | local-frontends-fixes |
| 167 | P1 | bug | tui | 运行中排队输入会丢掉大写字母 | local-frontends-fixes |
| 168 | P1 | bug | tui | /tree 与 /sessions 导航器没有视口，条目多时选中行在屏幕外 | local-frontends-fixes |
| 169 | P2 | bug | tui | /tree 打开时未处理的按键会穿透到隐藏的输入框和全局快捷键 | local-frontends-fixes |
| 170 | P1 | bug | tui | 审批弹问与 Ctrl+G 外部编辑器：鼠标捕获未关、回来后不重绘，stdin 还可能被 crossterm 线程抢读 | local-frontends-fixes |
| 171 | P1 | bug | tui | macOS 上 Ctrl+V 是空操作（只尝试 wl-paste/xclip），README 写的“Ctrl+V 贴图”在主力平台不可用 | local-frontends-fixes |
| 172 | P1 | bug | tui | TUI 内重建 provider 绕过 CLI 的 build_provider：--api-key/--provider 失效，openrouter 被错路由，Ctrl+L 静默切到 mock | local-frontends-fixes |
| 173 | P1 | bug | tui | TUI 的 replay_session 只取最早 500 行且不设 tree.id：长会话丢最新上下文，/export、/share 头部 id 错误 | local-frontends-fixes |
| 174 | P1 | bug | tui | 每次发送前 skills.refresh 会整表替换，丢掉 --skill 单文件、settings.skills 的额外路径和 deny 过滤 | tools-ext-fixes |
| 175 | P1 | incomplete | tui | 扩展 UI 对话框（ui/confirm、ui/select、ui/input、ui/editor）在 TUI 里从不展示，扩展只能等 30s 超时拿到 cancelled | local-frontends-fixes |
| 176 | P1 | perf | tui | 空闲循环每个事件（含鼠标移动）都扫命令目录、读文件并完整克隆渲染缓存；有内联图时每帧重传整张图 | local-frontends-fixes |
| 177 | P2 | perf | tui | 流式期间每个 delta 都重新解析整条助手消息（Markdown、高亮、Mermaid），并对整条做哈希 | local-frontends-fixes |
| 178 | P2 | perf | tui | !cmd、/share（gh gist）、剪贴板在 async 主循环里同步阻塞，期间 UI 冻结且无法中止 | local-frontends-fixes |
| 179 | P2 | simplify | tui | 死代码：sessions_block、status_footer，以及 dispatch_builtin 里根本到不了的 /copy /hotkeys /trust /edit /commands 分支 | local-frontends-fixes |
| 180 | P1 | simplify | tui | REPL 与 TUI 的斜杠命令各写一份（会话互操作、/model、/thinking、/plan、/rewind、/goto、/compact、/settings、/reload），约 400 行重复且已分叉 | local-frontends-fixes |
| 181 | P2 | simplify | tui | “采用新会话”逻辑三处拷贝，行为不一致 | local-frontends-fixes |
| 182 | P1 | simplify | tui | app.rs 结构：845 行的 run_loop、三个手动互斥的 overlay Option、两套重复的 Navigator、45 次 11 参数的测试调用 | local-frontends-fixes |
| 183 | P2 | incomplete | tui | REPL 与 TUI 对等性缺口：REPL 的 /sessions 指引 /resume，但 REPL 没有实现 /resume；REPL 也没有 !cmd，TUI 没有每轮扩展热重载 | local-frontends-fixes |
| 184 | P2 | bug | tui | `rupi -r` 每次都先往库里建一条空会话，选择器里取消也会留下来 | local-frontends-fixes |
| 185 | P2 | incomplete | tui | 会话选择器信息不足：不显示时间、cwd、首条消息，也不按当前项目排序 | local-frontends-fixes |
| 186 | P2 | incomplete | tui | 状态栏和模型显示细节：显示 provider 名而不是模型、/model 后不刷新、ModelChange 只有 Ctrl+L 发、提示写死不跟随键位、每次 LLM 调用都往对话里插一行 usage | local-frontends-fixes |
| 187 | P2 | bug | tui | 自定义主题的 id 只哈希部分颜色，改了 tool、system、syntax 等颜色后 /reload 不会重绘已有对话 | local-frontends-fixes |
| 188 | P2 | simplify | tui | 零散未用的 pub API 与字段 | local-frontends-fixes |
| 189 | P1 | missing-feature | tui | 输入编辑能力缺失：没有 ↑↓ 历史、Home/End/Delete、按词移动和删除、/help；Ctrl+C 会直接退出（运行中也是） | local-frontends-fixes |
| 190 | P1 | test-gap | tui | 关键交互路径没有测试，现有的部分测试测的是死代码 | local-frontends-fixes |
| 191 | P2 | doc-drift | tui | README、UPSTREAM 和 crate README 对 TUI 的描述与代码不符 | local-frontends-fixes |
| 192 | P0 | bug | server | drive() 早退不释放 run 租约与并发槽：空 messages 一次请求即可把租户锁 ~3 分钟 | cloud-run-safety |
| 193 | P0 | bug | server | 一条助手消息含多个工具调用且其一需审批：其余调用永不执行、无结果，续跑历史断裂 | cloud-run-safety |
| 194 | P0 | bug | server | 云端 tenant 记忆在其他会话的冻结提示里不可见；冻结缓存按租户键却按会话取数 | cloud-run-safety |
| 195 | P0 | bug | server | macOS execd：bash 超时只杀外层包装 sh，沙箱内作业继续跑并写工作区（abort 前已清空 children） | executor-output-bounds |
| 196 | P1 | bug | server | 会话 model / AG-UI state.model 被接受、回显但不参与选 provider | cloud-run-safety |
| 197 | P1 | bug | server | load_tree 忽略 parent、按 created_at 拍平 current_path：顺序错乱，分支被交错 | cloud-run-safety |
| 198 | P1 | bug | server | AGENTS.md / commands / RunAgentInput.context 每轮作为 System 节点追加进持久会话树（且同时进 system prompt） | cloud-run-safety |
| 199 | P1 | bug | server | `.rupi/commands/*.md` 与隐藏目录下的 SKILL.md 永远发现不到（glob 跳过隐藏目录） | cloud-run-safety |
| 200 | P1 | incomplete | server | 云端 skills 只做了一半：read_resource 不可用，控制面每轮写 temp 目录且不清理，无斜杠命令和自积累 | cloud-run-safety |
| 201 | P1 | bug | server | MESSAGES_SNAPSHOT 中一条 Tool 消息只带第一个 ToolResult，同批其余结果丢失 | cloud-run-safety |
| 202 | P1 | incomplete | server | AG-UI 流与快照 id 不一致；reasoning 在正文之后整段伪分片；工具参数不流式 | cloud-run-safety |
| 203 | P1 | bug | server | SSE 队列满时静默丢任意事件（含 TEXT_MESSAGE_END / RUN_FINISHED） | cloud-run-safety |
| 204 | P1 | bug | server | GET /v1/settings 明文返回 git_token（与管理面掩码列表不一致） | cloud-run-safety |
| 205 | P1 | bug | server | git bootstrap 失败被吞：file:// 或不存在的仓库仍返回 201 ready | cloud-run-safety |
| 206 | P1 | bug | server | 云记忆 replace/remove 语义与本机不一致且易误删：无 id 时覆盖最新一行 | cloud-run-safety |
| 207 | P1 | incomplete | server | settings 白名单里 13 个键被接受却无任何效果；max_turns 写死 12 | cloud-run-safety |
| 208 | P1 | bug | server | 图片 URL 抓取可经重定向 SSRF 到内网 http，且先读完整个 body 才判大小 | cloud-run-safety |
| 209 | P1 | bug | server | 管理 UI：带查询串的 hash 路由全部落到“概览”（会话筛选、租户会话链接、详情链接失效） | cloud-run-safety |
| 210 | P1 | incomplete | server | 管理审计只覆盖部分写操作，UI 也没有审计页 | cloud-run-safety |
| 211 | P1 | perf | server | Redis 会话树缓存命中仍查库（unwrap_or 急求值）；persist_tree 每轮全删全插 | cloud-run-safety |
| 212 | P1 | bug | server | 历史快照对象、workspace_snapshots 与 interrupts 行泄漏；workspace_snapshots 只写不读 | cloud-run-safety |
| 213 | P1 | test-gap | server | 云集成测试缺依赖时以 ok 静默“通过”；Postgres 探测是死代码；macOS CI 假绿 | cloud-run-safety |
| 214 | P1 | test-gap | server | 本机实测：3 个云用例在 macOS 必败或不稳，且有一个在 macOS 上断言恒真 | cloud-run-safety |
| 215 | P1 | missing-feature | server | 云端缺 MCP、AG-UI 前端工具、租户记忆管理接口与显式取消接口 | cloud-run-safety |
| 216 | P1 | test-gap | server | 关键行为没有测试覆盖 | cloud-run-safety |
| 217 | P2 | bug | server | 请求体字段风格不一致导致静默忽略：fork 只认 entry_id，未知 backend 静默回落 | cloud-run-safety |
| 218 | P2 | doc-drift | server | 管理面会话详情给出的 `rupi cloud` 命令无法解析 | cloud-run-safety |
| 219 | P2 | bug | server | 管理 UI 违反自身 CSP（内联 style 被拦）；canWrite 未使用；只能颁发 admin 角色 | cloud-run-safety |
| 220 | P2 | bug | server | run 结果语义粗糙：MaxTurns/Aborted 报 success，editedArgs 不回写历史，resume 时附带的新消息被丢 | cloud-run-safety |
| 221 | P2 | bug | server | 租户 mock provider 缓存永不失效（改剧本或切回真实 provider 都不生效） | cloud-run-safety |
| 222 | P2 | perf | server | 后台与入院路径的全表扫描：每 15s 全租户 reconcile，每次入院全局 reap；会话列表无分页 | cloud-run-safety |
| 223 | P2 | simplify | server | 死代码与重复代码可删约 300+ 行 | cloud-run-safety |
| 224 | P2 | doc-drift | server | 文档与实际行为的小出入：测试“skip”实为 ok，CJK 检索依赖 UTF-8 locale，云端能力边界未列出 | cloud-run-safety |
| 225 | P2 | bug | server | 会话存在性泄露：别租户的会话 id 返回 403、不存在返回 404 | cloud-run-safety |
| 226 | P0 | bug | core-agent | 压实切点会拆散 ToolCall/ToolResult，压实后发出的请求带孤儿 tool 消息，provider 返回 400，会话卡死 | agent-loop-integrity |
| 227 | P0 | bug | core-agent | 第二次及以后的压实从第 0 条开始重发整段前缀，且按全量历史判断触发；超窗后退回只取前 10 条首行的兜底，旧摘要丢失 | agent-loop-integrity |
| 228 | P0 | bug | core-agent | 云端 on_ask Interrupt 遇到同一轮多个工具调用时，只挂起第一个，其余调用（包括已放行的）丢失且没有 ToolResult，resume 后请求 400 | cloud-run-safety |
| 229 | P0 | bug | core-agent | AgentSession::prompt 遇到 provider 错误后 streaming 不复位，RPC 之后的 prompt 全部被拒为 busy，用户节点也不回滚 | local-frontends-fixes |
| 230 | P0 | bug | core-agent | RPC/AgentSession 只在 stdin EOF 时落盘一次：--resume 时因主键冲突整批回滚，new_session/switch 会丢掉前一个会话，崩溃则全部丢失 | local-frontends-fixes |
| 231 | P0 | bug | core-agent | tracing 日志默认写到 stdout，压实和落盘告警会混进 --mode rpc / run --json 的 JSONL 流 | local-frontends-fixes |
| 232 | P0 | bug | core-agent | 主循环从不设置 max_tokens（Anthropic/Gemini 输出被限在 4096），也不处理 max_tokens/length 停止原因，截断的工具调用参数被解析成 {} 照样执行 | agent-loop-integrity |
| 233 | P0 | bug | core-agent | --subagents 子 agent 绕过 --tools/--exclude-tools、hook 和 tool 可见性过滤 | agent-loop-integrity, local-frontends-fixes |
| 234 | P1 | bug | core-agent | hook 已经拒绝的调用仍会走 policy 和审批（Option::or 急切求值）：用户被问一个不会执行的调用，云端还会因此挂起 | cloud-run-safety |
| 235 | P1 | bug | core-agent | 工具过滤只隐藏定义，不拦截执行：--no-tools 和 --exclude-tools memory 下 memory/skill 路由工具照样能执行，提示里还在引导模型写记忆 | cloud-run-safety |
| 236 | P1 | bug | core-agent | /rewind 或 /goto 到摘要覆盖点之前，prompt 仍带着描述已废弃后续的旧摘要；keep_recent_tokens 还被当成消息条数传入 | agent-loop-integrity |
| 237 | P1 | bug | core-agent | resume 后摘要覆盖点被重置为首条消息，压实效果丢失，与文档“resume 时预热窗口”不符 | agent-loop-integrity |
| 238 | P1 | bug | core-agent | AgentSession.set_model 缺 key 时静默换成 Mock 并报成功；switch/fork/clone 用 model_id 重建 provider，丢失 provider 前缀 | local-frontends-fixes |
| 239 | P1 | incomplete | core-agent | 错误路径不发 TurnEnd/RunEnd；AgentEvent::Error 和 StopReason::ProviderError 从未构造；溢出重试会重复发 TurnStart | agent-loop-integrity |
| 240 | P1 | incomplete | core-agent | auto-retry 是空壳：AutoRetryStart/End 从不发射，set_auto_retry/abort_retry 对行为没有影响 | local-frontends-fixes |
| 241 | P1 | incomplete | core-agent | ToolHook（beforeToolCall/afterToolCall）没有任何生产接线入口，扩展的 tool_call 订阅只是通知，不能拦截或改写 | local-frontends-fixes |
| 242 | P1 | incomplete | core-agent | 压实触发与真实用量脱节：没算 system 和工具 schema，TokenMeter 校准系数恒为 1，窗口表与 models.json 是两套数据 | agent-loop-integrity |
| 243 | P1 | bug | core-agent | --mode rpc 丢弃 --approve，也不装配 --subagents 和 review，与 run/chat/TUI 行为不一致 | local-frontends-fixes |
| 244 | P1 | incomplete | core-agent | 子 agent 上下文贫瘠：没有 AGENTS.md 和 --append-system-prompt，没有扩展事件，压实与发现配置用默认值 | agent-loop-integrity |
| 245 | P1 | bug | core-agent | 启发式 reviewer 默认开启且误报严重，只要用了两种工具就产生垃圾 skill 草稿 | agent-loop-integrity |
| 246 | P1 | missing-feature | core-agent | 系统提示没有 cwd、日期、平台和工具使用指引，base 只有一句话 | agent-loop-integrity |
| 247 | P1 | bug | core-agent | 主循环写死 temperature 0.2，OpenAI 推理模型（o3/o4-mini/gpt-5，目录内置）会拒绝这个参数 | agent-loop-integrity |
| 248 | P1 | perf | core-agent | Approver 是同步 trait，在 async 主循环里阻塞读 stdin：审批期间 Ctrl-C/Esc 无效；压实的摘要调用也不响应取消 | local-frontends-fixes |
| 249 | P2 | simplify | core-agent | run_turns 约 490 行，12 处事件广播重复；5 个压实包装和 3 个 10 参数入口可以收敛 | agent-loop-integrity |
| 250 | P2 | simplify | core-agent | CLI 里有 4 份几乎一样的 AgentLoop 装配和 3 份 reviewer 回调；TUI/REPL/run 不走 AgentSession，follow-up 逻辑又实现了一遍 | local-frontends-fixes |
| 251 | P2 | simplify | core-agent | 死 API 和死依赖：run_subagents（只有测试用，且有 bug）、ToolExtension、RulePolicy::plan_mode/readonly/ChainPolicy、重复的 is_mutating、peek_*、SessionNode.summary，rupi-mcp/thiserror 依赖 | agent-loop-integrity |
| 252 | P2 | incomplete | core-agent | 计划模式只拦 write/edit/bash 三个名字，memory 写入、MCP/扩展工具和可选工具照常执行 | cloud-run-safety |
| 253 | P2 | bug | core-agent | 最后一轮工具执行后仍会注入转向消息，但随即以 MaxTurns 退出，这条消息永远得不到回答；review 和记忆 sync 只在 Done 出口运行 | agent-loop-integrity |
| 254 | P2 | missing-feature | core-agent | 思考内容只在整轮结束后一次性发出，工具调用参数不流式，长输出期间 UI 没有进度 | local-frontends-fixes |
| 255 | P2 | perf | core-agent | 每轮同步重读 AGENTS.md 祖先链，并多次全量克隆和估算历史 | agent-loop-integrity |
| 256 | P2 | doc-drift | core-agent | 文档与代码多处不符：子 agent 继承项、run_subagents、hook、usage 校准、resume 预热、core 不依赖 IO、Tool trait 所在 crate | local-frontends-fixes |
| 257 | P1 | test-gap | core-agent | 多轮和异常路径缺测试：压实配对与重复压实、rewind 与摘要、SDK 出错复位、RPC 落盘、云端多调用 interrupt、hook 短路 | local-frontends-fixes |

## 借鉴上游 Pi（42 条）

| B | 价值/工作量 | 阶段 | 标题 | 第一轮 track |
|---:|---|---:|---|---|
| 1 | critical/M | 0 | 云端 transcript 配对止血：guidance 不再写入树、压实切点对齐、多调用 interrupt、发送前修复 | agent-loop-integrity, cloud-run-safety |
| 2 | critical/S | 0 | 凭证严格按 provider 匹配，云端禁用 mock 兜底 | cloud-run-safety |
| 3 | critical/M | 0 | bash 输出设上限，超出部分 spill 到工作区 | executor-output-bounds |
| 4 | critical/S | 0 | 持久化止血：persist_tree 加 run_id 守卫，写入失败不再报 success，连接池设超时 | cloud-run-safety |
| 5 | high/S | 0 | 让取消真正生效：PoolScheduler::abort 和进程组清理 | executor-output-bounds |
| 6 | high/S | 0 | frozen 记忆的缓存键改为按会话区分，修复同租户内跨会话泄露 | cloud-run-safety |
| 7 | high/S | 0 | 图片 URL 抓取的 SSRF 修复（第一步），之后把图片移到外部存储 | cloud-run-safety |
| 8 | critical/M | 0 | 计费漏洞止血：取消和出错也要记账，压实计费，统一 token 口径，运行中检查预算 | agent-loop-integrity, cloud-run-safety |
| 9 | critical/M | 1 | 稳定缓存前缀：工具顺序确定、历史加缓存断点、cache key 和会话亲和 | agent-loop-integrity, llm-wire-compat, tools-ext-fixes |
| 10 | high/S | 1 | LLM HTTP 超时、Retry-After 上限、jitter 和错误分类 | llm-wire-compat |
| 11 | high/S | 1 | 降低 PG 和执行面的固定负载：准入时不再全表 reap，reconcile 分片，执行器 stats 改为后台缓存 | cloud-run-safety, executor-output-bounds |
| 12 | high/S | 1 | 优雅下线（阶段 0）与 panic 隔离 | cloud-run-safety |
| 13 | high/S | 1 | 闲置快照先 claim：引入 snapshotting 中间态 | cloud-run-safety |
| 14 | high/S | 1 | 给持久化格式加版本：遇到不认识的格式时拒绝，不做降级 | cloud-run-safety |
| 15 | high/S | 1 | 处理 length 截断：参数是坏 JSON 的调用不执行，max_tokens 按模型设置 | agent-loop-integrity, cloud-run-safety, llm-wire-compat |
| 16 | critical/M | 1 | 压实改为增量摘要，按投影后的上下文大小触发，并使用结构化模板 | agent-loop-integrity |
| 17 | high/M | 1 | 可观测性基础：request id、收敛对外错误、日志脱敏、低基数指标 | - |
| 18 | medium/S | 1 | 只读工具并行执行 | cloud-run-safety |
| 19 | medium/S | 1 | 工作区资源发现：只扫白名单目录、标记为不可信、每个热工作区只加载一次 | cloud-run-safety |
| 20 | medium/S | 1 | 请求卫生：错误结果中的图片、空工具输出、终端用户标识 | llm-wire-compat |
| 21 | high/M | 1 | 模型目录记录能力，套餐限定可用模型，租户和会话设置真正生效 | cloud-run-safety, llm-wire-compat |
| 22 | critical/L | 2 | 按步骤增量提交 transcript（先提交后展示的 PG 实现） | - |
| 23 | critical/L | 2 | runs 表、epoch fence 租约、恢复用的 reconciler、副本存活表 | - |
| 24 | high/M | 2 | 工具调用先提交 intent，并声明 replay 策略（effect sandwich） | - |
| 25 | high/S | 2 | run 级幂等准入 | - |
| 26 | high/M | 2 | attempt 计数、restart_limit 和 agent 层重试 | - |
| 27 | high/L | 3 | 断线不取消 run、事件流可续传、快照按窗口截取 | - |
| 28 | high/L | 3 | 计费级用量账本、价格目录和租户用量 API | - |
| 29 | high/M | 3 | 租户侧安全审计日志 | - |
| 30 | medium/M | 3 | 跨副本控制：cancel、steer、follow-up 和持久收件箱 | - |
| 31 | medium/S | 3 | 执行器资源计量：工作区占用秒数和 exec 耗时 | - |
| 32 | high/L | 3 | 平滑交接（阶段 1）：在回合边界移交 run，不计为崩溃 | - |
| 33 | medium/M | 3 | 执行面幂等 call_key，连接断开不杀命令 | - |
| 34 | high/L | 4 | 凭证加密存储，并按请求传入 ProviderConfig | - |
| 35 | medium/L | 4 | 分布式追踪（OTel）与 trace carrier 持久化 | - |
| 36 | medium/M | 4 | principal scope 与即时撤销 | - |
| 37 | medium/M | 4 | 按 region 放置 run | - |
| 38 | medium/L | 4 | 逻辑模型路由与跨 provider 故障转移 | - |
| 39 | medium/L | 4 | 多租户远程 MCP：只支持 HTTP，默认 deferred | - |
| 40 | medium/XL | 4 | codemode 路线：嵌套工具管线、结构化输出、在执行面运行 QuickJS/WASM（先做 eval） | - |
| 41 | low/M | 4 | 修复与 Pi JSONL v3 的互操作 | - |
| 42 | low/L | 4 | 统一会话运行时，不再使用进程全局环境变量 | - |

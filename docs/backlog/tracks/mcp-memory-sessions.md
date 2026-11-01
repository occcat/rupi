# Track: mcp-memory-sessions

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/mcp-memory-sessions`，worktree：`.worktree/mcp-memory-sessions`

## 目标
MCP 三种传输能连官方 SDK server、配置宽容；记忆增删改按行精确且镜像一致；本地会话库保存树结构，JSONL 进出口与 Pi 互通。

## 负责文件
- crates/rupi-mcp/**
- crates/rupi-memory/**

不在本 track：CLI/TUI 调用点归 local-frontends-fixes；新增参数时保持旧签名可用或给默认值。

## 特别说明
- 72 给 sqlite messages 加 parent 列与 sessions 的 leaf：做成 user_version 迁移，老库 parent 为 NULL 时按线性回退；更新 tree_from_records 等读取路径。
- 64 load_configs 同时接受数组和 {"mcpServers":{...}}；command/args/env 默认值；单条解析失败只 warn 跳过；整个文件 JSON 语法错误仍返回 Err 并带文件名。
- 63 用官方 Python SDK（如本机有 `pip show mcp` 或可用 uvx）验证 Streamable HTTP 握手；装不了就加 stub 断言 notification 不带 id。
- 80 session_search 查询 sessionDir 的问题如果需要调用点改动，在 rupi-memory 侧提供接口并在报告里写给 local-frontends。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F63 [P0] Streamable HTTP 握手把 notifications/initialized 当成带 id 的请求发出，官方 SDK server 直接拒绝，HTTP 传输无法使用
类型：bug，工作量：S，文件：crates/rupi-mcp/src/lib.rs, crates/rupi-mcp/tests/mcp_bridge.rs

证据：lib.rs:771-801：`notify` 的 Http 分支先 `next_id.fetch_add` 再调 `call_http`，`call_http` 在 lib.rs:498 总是组装 `{"id":id,...}`。handshake 在 lib.rs:403 调 notify。实测：用官方 Python SDK mcp 1.30 的 FastMCP 起 `streamable-http` server，执行 `rupi mcp-list x --url http://127.0.0.1:8765/mcp` 得到 `Error: MCP error for notifications/initialized: {"code":-32602,..."Invalid request parameters"}`。用 curl 重放同样的包，带 id 时返回 200+SSE error，去掉 id 返回 202，之后 tools/list 正常。测试 stub 在 mcp_bridge.rs:349 只按 method 名回 202，不检查 id，所以测试通过。

建议修法：Http 分支的 notify 单独构造不带 id 的 JSON-RPC notification 并 POST，202 或空体视为成功，不再复用 call_http 的 id 路径。stub 改为：notification 带 id 时回 -32600，防止回归。

### F64 [P0] --mcp-config 格式过严：url-only server 报缺 command，不写 env 报缺字段，标准 mcpServers 对象格式不认，解析失败还会直接让整个程序退出
类型：bug，工作量：S，文件：crates/rupi-mcp/src/lib.rs, crates/rupi-cli/src/main.rs, crates/rupi-mcp/README.md

证据：lib.rs:30-40：`McpServerConfig` 只有 `url` 带 `#[serde(default)]`，`command`/`args`/`env` 都是必填。lib.rs:1516-1519：`load_configs` 只接受数组。实测：`[{"name":"h","url":"http://..."}]` 报 `missing field command`；`[{"name":"fake","command":"python3","args":[..]}]` 报 `missing field env`；`{"mcpServers":{...}}` 报 `invalid type: map, expected a sequence`。用户本机 `~/.pi/agent/mcp.json` 就是 mcpServers 格式。main.rs:1833/1948/2216/2729 都是 `load_configs(path)?`，配置文件一个错就让 run/chat/tui/rpc 全部失败，与 README 中“单 server 失败不影响其他”的说法矛盾。main.rs:95 的帮助文本也把 env 写成像是可选的。

建议修法：`command`/`args`/`env` 加 `#[serde(default)]`；`load_configs` 同时接受数组和 `{"mcpServers":{name:{command,args,env,url,type}}}`（name 取 key）；单个条目解析失败只 warning 并跳过；校验 `command` 和 `url` 至少有一个。

### F152 [P2] --mcp-config 的 env/args 是必填字段，报错不带文件名；只支持裸数组格式
类型：incomplete，工作量：S，文件：crates/rupi-mcp/src/lib.rs

证据：McpServerConfig(lib.rs:31-40) 的 args 和 env 都没有 `#[serde(default)]`，load_configs(lib.rs:1516-1519) 直接透传 serde 错误。实测配置 `[{"name":"fake","command":"python3","args":[...]}]` → `Error: missing field \`env\` at line 1 column 89`，没说是哪个文件。生态里常见的 `{"mcpServers":{name:{command,args,env}}}` 格式（Claude Desktop、Cursor 用的）不被接受。

建议修法：args 和 env 加 `#[serde(default)]`。load_configs 的错误用 with_context 带上文件路径。兼容 `{"mcpServers":{...}}` 对象格式。

### F73 [P1] stdio/WS server 崩溃时在途调用要等满 30s 才报错；所有调用固定 30s 超时；reqwest 的 30s 总超时会切断 GET 常驻流和长 SSE；不会重启 server
类型：bug，工作量：M，文件：crates/rupi-mcp/src/jsonrpc.rs, crates/rupi-mcp/src/ws.rs, crates/rupi-mcp/src/lib.rs

证据：jsonrpc.rs:93-101：reader 遇到 EOF 就退出，但不清空 `pending`，挂起的 oneshot 只能等 jsonrpc.rs:168 的 30s 超时。ws.rs:34-54 同样。实测：fake server 在 tools/list 时 `sys.exit(3)`，`rupi mcp-list` 耗时 30.3s 后报 `tools/list timed out after 30s`。call 超时硬编码在 jsonrpc.rs:153、ws.rs:96，执行超过 30s 的 MCP 工具必然失败。lib.rs:248-251 的 `Client::builder().timeout(30s)` 是包括 body 在内的总超时，所以 lib.rs:676-698 的 GET 常驻流每 30s 被断开重连，SSE 回包超过 30s 也会被截断。没有子进程退出检测和重连逻辑。

建议修法：reader 退出时 drain pending，给每个挂起的调用发送 `server exited` 错误，并标记桥失效，后续调用立即失败；配置支持 per-server `timeout`（默认 60s，可设为 0 表示不限）；HTTP 改用 `connect_timeout` 加按请求的 timeout，GET 流不设总超时；McpManager 对失效的 stdio 桥按需重新 spawn。

### F74 [P1] MCP 工具名 `{server}_{tool}` 不做规范化，含 `.`、`/`、空格或超过 64 字符时会让整个 LLM 请求被拒
类型：bug，工作量：S，文件：crates/rupi-mcp/src/lib.rs, crates/rupi-llm/src/lib.rs

证据：lib.rs:1055 和 lib.rs:1395 直接 `format!("{prefix}_{}", tool.name)`。rupi-llm lib.rs:365-366 把 name 原样作为 function.name 发送。在 llm/agent/tools 中 grep 不到任何名字清洗或长度限制。OpenAI 和 Anthropic 都要求名字匹配 `^[a-zA-Z0-9_-]{1,64}$`。只要接入一个工具名带点的 server（这类 server 很常见），每一轮请求都会 400。这条是从代码推出的，没有接真实模型复现。

建议修法：注册时把非法字符替换为 `_`，超长时截断并加短 hash，保留“暴露名 → 原名”映射（McpToolExecutor 里已有 tool_name 字段），冲突时加后缀。

### F75 [P1] tools/call 只取 text 块：image/audio/resource 内容丢弃，structuredContent 采集后没人用；content 没有 text 时返回空串
类型：incomplete，工作量：S，文件：crates/rupi-mcp/src/lib.rs, crates/rupi-tools/src/lib.rs

证据：lib.rs:862-875：只 `filter_map(b.get("text"))`，content 是数组但没有 text 时结果是 `""`，`result.to_string()` 的回退只在 content 不是数组时生效。lib.rs:879 的 `structured` 写入 McpToolResult 后，全仓 grep 不到任何读取。rupi-tools lib.rs:38-43 的 `ToolOutput` 有 `images` 字段，但 McpToolExecutor（lib.rs:1300-1304）只用 text。截图类 MCP 工具返回给模型的是空内容。

建议修法：`McpToolResult` 携带 images（content 里 `type:image` 的 data/mimeType），McpToolExecutor 填入 `ToolOutput.images`；`resource` 块取 text 或 uri；没有 text 但有 structuredContent 时把 structuredContent 序列化为文本。

### F93 [P2] MCP 协议细节缺失：取消不发 notifications/cancelled、protocolVersion 写死且不看协商结果、HTTP 404 时不重新 initialize、roots URI 不做转义
类型：incomplete，工作量：M，文件：crates/rupi-mcp/src/lib.rs

证据：lib.rs:1313-1331：取消时 select 直接返回，不通知 server，pending 条目要等超时才清理。lib.rs:397：`protocolVersion: "2024-11-05"` 固定，initialize 的返回结果被丢弃（lib.rs:394-402），没有 `MCP-Protocol-Version` header。lib.rs:531-532：会话过期返回 404 时直接 bail（实测伪造 session id 得到 404），规范要求客户端重新 initialize。lib.rs:72：`format!("file://{}", dir.display())`，路径含空格或非 ASCII 时不做百分号编码。

建议修法：取消时发送 `notifications/cancelled{requestId}`；保存协商得到的版本，HTTP 带上相应 header；遇到 404 清空 session_id，重新握手后重试一次；roots 用 `url::Url::from_directory_path`。

### F89 [P1] MCP 测试只对自写 stub：不校验 notification 不带 id，没有崩溃、超时、配置格式和 HTTP/WS 的 list_changed 用例
类型：test-gap，工作量：M，文件：crates/rupi-mcp/tests/mcp_bridge.rs, crates/rupi-mcp/tests/fake_mcp_server.py

证据：mcp_bridge.rs:349：stub 对 `notifications/initialized` 只看 method 就回 202，所以 HTTP 握手的 bug 在测试里是绿的。全文没有 server 退出、调用超时、url-only 或缺 env 配置、`mcpServers` 格式、HTTP/WS 上的 tools/list_changed、非 text content 的用例。WS stub（mcp_bridge.rs:523-579）不发送任何 server→client 请求。

建议修法：stub 遇到带 id 的 notification 回 -32600；增加 fake server 中途 exit 时 5s 内报错的断言；为 `load_configs` 增加多种格式的单测；可选地加一个 `#[ignore]` 的官方 SDK 兼容测试（uvx mcp）。

### F91 [P1] rupi-mcp 内部大量重复：6 个 spawn 构造函数、3 份分页循环、3 个工具实现各写一遍 execute/execute_with_cancel、两套 RPC call_timeout
类型：simplify，工作量：M，文件：crates/rupi-mcp/src/lib.rs, crates/rupi-mcp/src/jsonrpc.rs, crates/rupi-mcp/src/ws.rs, crates/rupi-cli/src/main.rs

证据：lib.rs:266-447：spawn/spawn_watched/spawn_ws/spawn_ws_watched/spawn_http/spawn_http_watched，加上 spawn_all_inner 里的 6 路 match（lib.rs:1367-1376）。spawn_inner 与 spawn_ws_inner 的 incoming 路由任务（lib.rs:289-315 与 353-379）逐行重复。list_tools、list_resources、list_prompts 的 cursor 分页（lib.rs:804-842、921-959、984-1018）重复。McpResourceReader、McpPromptGetter、McpToolExecutor 的 execute 与 execute_with_cancel 互为拷贝（lib.rs:1119-1158、1214-1261、1291-1331）。jsonrpc.rs:157-182 与 ws.rs:100-125 的 call_timeout 相同。CLI 中 MCP 启动块复制了 4 次（main.rs:1832、1947、2215、2728）。

建议修法：`McpBridge::connect(cfg, watch: Option<_>)` 按 url scheme 分派；抽一个 `trait RpcChannel { write_json; pending }`，stdio 和 ws 共用 call_timeout 和路由任务；写一个泛型的 `paginate(method, key, parse)`；工具 execute 统一为 `execute_with_cancel(args, &CancelFlag::never())`。预计减少约 250 行。

### F69 [P0] memory 工具的 replace/remove 是全文子串操作：remove 会误伤其他词，replace 缺 ||| 时把整个 MEMORY.md 覆盖成 entry
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:406-416：`replace` 在没有 `|||` 时执行 `content = entry.to_string()`（整文件覆盖）；`remove` 是 `content.replace(entry, "")`，删的是全文所有子串。工具 schema（lib.rs:447-455）没有说明 `OLD ||| NEW` 格式，模型很可能直接传新文本。实测（RUPI_HOME 临时目录）：先 add “uses steam deck for testing” 和 “likes green tea”，执行 `memory-write remove tea` 后文件变成 `uses sm deck for testing` / `likes green `；执行 `memory-write replace "only this line"` 后整个文件只剩这一行。

建议修法：改成按行操作：remove 删除 trim 后与 entry 完全相等的行（或唯一包含 entry 的行，多行命中则报歧义）；replace 改为 schema 显式字段 `old`/`new`，old 必须唯一命中一行，缺 old 直接报错，不再整文件覆盖。

### F134 [P0] memory replace 不带 `|||` 时会覆盖整个 MEMORY.md，之后 add 还会拼到同一行
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:406-412：`"replace" => if let Some((old,new)) = entry.split_once("|||") {...} else { content = entry.to_string(); }`。走 else 分支时整份文件被替换成 entry，而且没有补换行。工具 schema(lib.rs:445-455) 只写了 `op: add/replace/remove, entry`，没告诉模型 `OLD ||| NEW` 这个格式，所以模型按常理调用 replace 就会清空全部记忆。复现（隔离 RUPI_HOME）：先 `memory-write add "fact A: uses pnpm"`、`add "fact B: CI on GitHub"`、`add "fact C: deploy to fly.io"`，再执行 `memory-write replace "fact B: CI on GitLab"` → live state 只剩 `fact B: CI on GitLab`。接着 `memory-write add "fact D"` → 文件内容变成 `fact B: CI on GitLabfact D\n`（od -c 确认中间没有换行）。

建议修法：replace 必须带 OLD 和 NEW，可以拆成两个字段（old_text/new_text），或者强制 `|||` 格式，缺了就报错，不能退化成全量覆盖。按行匹配替换，写盘时保证以换行结尾。tool schema 的 description 写明参数格式。补单测。

### F145 [P1] memory remove 是全文子串删除；memory_search 搜的是只增不减的镜像表，已删、已改的事实仍能搜到
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：remove 分支(lib.rs:414-415) 是 `content = content.replace(entry, "")`。所有 op 都会执行 `self.mirror_memory(mirror_target, entry)`(lib.rs:420)，往 memories 表 INSERT(lib.rs:1430-1435)，remove 也不例外，被删除的文本反而作为一条新记忆插入。memory_search 查的就是这张表(lib.rs:1440-1455)。实测：MEMORY.md 有 `fact B: CI on GitLabfact D` 时执行 `memory-write remove "fact"`，结果变成 ` B: CI on GitLab D`，所有行里的单词 fact 都被删掉。replace 把文件清空后执行 `memory-search pnpm`，仍返回 `[memory] fact A: uses <b>pnpm</b>`。另外 `memory-write add x --scope nowhere` 被静默当成 global 写入，还打印 `updated [nowhere]`(lib.rs:385 `_ =>` 分支)。

建议修法：remove 改成按行精确匹配（trim 后相等）才删整行。镜像改为每次写盘后按文件当前内容重建（先 delete 该 target，再逐行插入），或者 remove/replace 时同步删、改对应镜像行。scope 只接受 global 和 project，其他值报错。

### F70 [P0] 导入 Pi JSONL 时丢掉 model_change/thinking_level_change 等条目，父链断开，叶子路径为空：HTML 导出为空，--session 导入的消息数随机
类型：bug，工作量：M，文件：crates/rupi-memory/src/session_io.rs

证据：session_io.rs:426：未识别的 type 走 `_ => {}`，节点不入表，但其后 message 的 parentId 仍指向它。session_io.rs:432-435：`goto_node(leaf)` 沿父链遇到缺失节点时返回 false（rupi-core lib.rs:345-347），current_path 保持为空。`remap_tree` 在 session_io.rs:726-730 回退为 `out.nodes.keys().next()`，取的是 HashMap 的随机顺序。按上游 session-format.md，model_change 等都是带 id/parentId 的树节点。实测：构造 header→model_change→thinking_level_change→user→assistant 的 JSONL，`rupi --export x.jsonl y.html` 生成的 HTML 没有任何消息；`--session x.jsonl chat` 连跑 6 次，分别得到 1/2/2/2/1/2 条消息。

建议修法：导入时为非 message 条目建立 id 到 parentId 的透传表，把子节点的 parent 重写到最近的消息祖先，并解析 model_change 恢复模型；goto 失败时按 JSONL 顺序选最后一条消息并沿已知父链重建路径，不再依赖 HashMap 顺序。

### F71 [P0] 导出 JSONL 时，一轮多个工具结果的后续 assistant 挂在第一个 toolResult 下，其余结果落到死分支，Pi 和 rupi 重新导入后都缺 tool_result
类型：bug，工作量：S，文件：crates/rupi-memory/src/session_io.rs

证据：session_io.rs:121-155：Tool 消息按每个 ToolResult 拆成多行，第 i>0 行用新 uuid 并串在前一行后面。session_io.rs:59 的下一个节点用 `node.parent`（树节点 id，即第一行 id），只有 parent 为 None 时才用 `prev_jsonl_id`。实测：库中 n2 发起 c1/c2 两个调用，n3 带两个结果。导出后 `n3 parent=n2 R1`、`8010edcf parent=n3 R2`、`n4 parent=n3 done`，从 n4 回溯的路径上没有 R2。rupi 自己 import 时叶子取 n4，同样丢掉 R2，继续对话时 provider 会因缺少 tool_result 报 400。

建议修法：导出时维护“节点 id 到其最后一行 JSONL id”的映射，子节点的 parentId 指向父节点的最后一行；或者把 Pi 的多结果合并成连续条目后，再把下一节点挂到最后一条上。补一个多工具 export→import 往返测试。

### F72 [P0] sessions.db 不存父子关系，resume 时线性回放：/rewind 后被丢弃的分支重新进入模型上下文
类型：bug，工作量：M，文件：crates/rupi-memory/src/lib.rs, crates/rupi-memory/src/session_io.rs, crates/rupi-cli/src/main.rs

证据：lib.rs:1090：messages 表没有 parent 列。main.rs:1546-1559 的 `turn_rows` 只写节点和 blocks。session_io.rs:867-885 的 `tree_from_records` 逐行 `push_with_id`，每行都以上一行为父。实测：chat 中依次输入 “first question”、“abandoned branch msg”、两次 /rewind、“new branch msg”，此时 /tree 显示 abandoned 为旁支（`+`）。`--resume <id>` 后 /tree 变成 first→abandoned→new 一条链，被丢弃的内容回到了上下文里。

建议修法：messages 加 `parent_id TEXT` 列（schema v5 迁移，老库为 NULL 时按线性回退），`SessionMessageRow` 带上 parent；`tree_from_records` 按 parent 建树，并在 sessions 表记录 leaf id，恢复时 goto 到该 leaf。

### F77 [P1] 记忆密钥扫描用子串匹配，误伤正常文本（disk-usage、task-runner、Slovakia、“private key” 这类说明）
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:22-37：`lower.contains("sk-")`、`contains("akia")`、`contains("private key")`，没有词边界，也没有长度或字符集约束。实测 `memory-write add` 以下 4 条全部被拒为 secret：“check disk-usage before running the build”、“the task-runner lives in ops/”、“team is based in Slovakia”、“never commit the private key; use the vault”。review 自动落盘（main.rs:47、2832）也因此静默失败。与此同时，AWS secret、JWT（eyJ…）、`Bearer xxx` 这类真实密钥都没有覆盖。

建议修法：改成带边界的模式：`\bsk-[A-Za-z0-9_-]{20,}`、`\bAKIA[0-9A-Z]{16}\b`（区分大小写）、`gh[pousr]_[A-Za-z0-9]{30,}`、`xox[abps]-[A-Za-z0-9-]{10,}`、`-----BEGIN [A-Z ]*PRIVATE KEY-----`、`eyJ[\w-]+\.[\w-]+\.`；去掉裸的 "private key"。补误伤和漏报两组用例。

### F78 [P1] FTS 记忆镜像只增不删：remove 反而把被删文本插入为新记忆，replace 镜像的是 `OLD|||NEW` 原文，压实丢掉的行永远留在 memory_search 结果里
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:420：`apply_write_scoped` 不论 op 是什么都执行 `mirror_memory(target, entry)`。lib.rs:1430-1436 只有 INSERT，全仓没有 `DELETE FROM memories`（只有 rupi-server 的 PG 实现）。lib.rs:426-438 的 `compact_file` 不同步镜像。实测：remove “tea” 后 `memory-search tea` 仍返回 `[memory] <b>tea</b>`（被删的 entry 本身）和 `likes green <b>tea</b>`（已被删的旧行）。

建议修法：镜像按文件重建：每次写入或压实后，在事务内 `DELETE FROM memories WHERE target=?`，再按当前文件逐行 INSERT（文件不超过 5KB，成本可忽略）。remove 不再镜像 entry。

### F79 [P1] trigram FTS 用整串短语查询：少于 3 个字符（如“乌龙”“部署”“Go”）永远查不到，多词查询必须原样相邻才命中；session_search 没有兜底
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:1073-1075 的 `fts_phrase` 把整个 query 包成一个短语；lib.rs:1093、1109 用 trigram。用同样 tokenizer 的 sqlite3 实测：`"乌龙茶"`→1，`"乌龙"`→0，`"茶"`→0，`"Go"`→0；对 “deploy the staging cluster with helm” 查 `"staging deploy"`→0，`"helm staging"`→0。memory_search 有文件子串兜底（lib.rs:841），也只覆盖原样子串；session_search（lib.rs:867-893）没有任何兜底。中文用户常用的二字词基本召回不到。

建议修法：按空白拆词：每个长度≥3 的词作为独立短语用 AND 连接；有长度<3 的词时退化为 `content LIKE '%w%'` 逐词 AND（有 LIMIT 的扫描可以接受）。补 CJK 二字词和乱序多词测试。

### F81 [P1] USER.md 只读不写：引导语告诉模型 USER.md 存用户偏好，但 memory 工具和 CLI 都只能写 MEMORY.md
类型：incomplete，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-cli/src/main.rs

证据：lib.rs:302-307 只读 USER_FILE。`memory_tool_definition`（lib.rs:446-455）的 scope 只有 global/project。`apply_write_scoped`（lib.rs:374-390）只写 MEMORY.md。CLI 的 `memory-write`（main.rs:276-281、908-916）也一样。引导文本（lib.rs:506）写着 “USER.md keeps user preferences”，但没有写入途径，`user_char_limit` 和 `user_profile_enabled` 只对读路径生效。

建议修法：scope（或 Hermes 风格的 target）增加 `user`，映射到 memories/USER.md，使用 user_char_limit，镜像 target=user；CLI 的 `memory-write --scope user` 同步支持。

### F82 [P1] 本地会话库不存 summary_through：resume 后摘要被当成只覆盖第一条消息，prompt 变成摘要加几乎全部历史
类型：bug，工作量：S，文件：crates/rupi-memory/src/session_io.rs, crates/rupi-memory/src/lib.rs

证据：session_io.rs:878-883：`tree_from_records` 在有 summary 时令 `summary_through = current_path.first()`。sessions 表（lib.rs:1089、1179-1182）没有 summary_through 列（云端 rupi-server db.rs:231 有）。rupi-core 的 `prompt_history` 从 through+1 开始取，所以 resume 后上下文是摘要加第 2 条起的全部历史，压实效果作废：下一轮立即超窗或再次压实（多一次 LLM 调用）。`export_store_jsonl` 也因此把 firstKeptEntryId 写成第二个节点。

建议修法：sessions 表加 `summary_through TEXT`，`persist_turn` 的 summary 参数改为 `(summary, through)`，`tree_from_records` 读回；老库为 NULL 时保持现行回退。

### F83 [P1] TUI 的 /resume 和 /sessions 只回放最早 500 行，长会话会丢掉最新的消息；CLI 则静默截在 10000 行
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-memory/src/lib.rs, crates/rupi-memory/src/session_io.rs

证据：app.rs:327：`store.session_records(id, 500)`。lib.rs:1487-1489：`ORDER BY created_at ASC, rowid ASC LIMIT ?`，即只取最旧的 N 行。session_io.rs:618、636、792 用 10_000。超过 500 行（工具调用多时很快就到）的会话在 TUI 续聊时，会丢掉最近的上下文，并在旧位置上接着写。

建议修法：统一走 `restore_tree`，不设上限（或者用分页游标）；确实要限量时取最新的 N 行（`ORDER BY ... DESC LIMIT N` 再反转），并配合摘要使用。

### F88 [P1] JSONL 导出号称贴 Pi session-format v3，但有多处硬编码或丢失：toolName 固定为 "tool"、model 为空、api 固定 openai-completions、stopReason 固定 stop、图片丢弃、compaction 挂在侧枝上
类型：doc-drift，工作量：M，文件：crates/rupi-memory/src/session_io.rs, docs/UPSTREAM.md

证据：session_io.rs:146：`"toolName": "tool"`。session_io.rs:221-225：`api: "openai-completions"`、`model: ""`、`stopReason: "stop"`（带 toolCall 时也是 stop）。session_io.rs:206-208：assistant 的 Image 和 RedactedThinking 被丢弃；用户图片经 `linear_message_line(full_text)` 变成 `[image png]` 文本。session_io.rs:76-90：compaction 条目的 parentId 是 summary_through，并且追加在文件末尾。上游规范要求 compaction 挂在当前叶子上，kept 段是它的祖先，所以 Pi 打开后这条 compaction 落在死分支上。UPSTREAM.md:35/99 写的是“JSONL 贴 Pi session-format v3”。

建议修法：toolName 从同一路径上对应 ToolCall 的 name 反查；model 和 api 从 Message.provider 或会话模型取；stopReason 在有 ToolCall 时写 `toolUse`；图片导出为 `{type:image,data,mimeType}`；compaction 条目挂在当前叶子上，firstKeptEntryId 指向 through 的下一个节点。

### F154 [P2] JSONL 导出里 assistant 的 model 恒为空，api 恒为 openai-completions
类型：incomplete，工作量：S，文件：crates/rupi-memory/src/session_io.rs

证据：session_io.rs:218-226 写死了 `"api": "openai-completions"` 和 `"model": ""`。实测 `/export` 生成的 rupi-session-75d6a815.jsonl 里 assistant 行是 `"api":"openai-completions","model":"","provider":"openai-compat"`，而且 usage.cost 全为 0。导入 Pi 后看不到用的是哪个模型。

建议修法：节点里记录 model id（TokenUsage 或 blocks JSON 已经有 provider 字段，再加上 model），按 provider 填对应的 api 名。

### F90 [P1] 记忆和会话 IO 的关键行为没有测试：replace/remove 语义、镜像一致性、短查询召回、分支持久化、多工具 JSONL 往返、Pi 非 message 条目
类型：test-gap，工作量：M，文件：crates/rupi-memory/src/lib.rs, crates/rupi-memory/src/session_io.rs

证据：lib.rs 的测试（1538-2397）只覆盖 add、幂等和密钥拒写，没有 replace 和 remove；`secrets_are_refused_not_persisted` 只测阳性，没有误伤用例。session_io.rs 的测试（887-1033）只有 user/assistant 纯文本往返，没有 ToolCall/多 ToolResult、model_change 条目、分支 rewind 后 persist→restore、compaction 往返。

建议修法：为上面已确认的每个 bug 各补一个回归用例（多数是纯函数或临时目录测试，单个用例 S 级工作量）。

### F96 [P2] 记忆上限按字节计算，文档写的是“5000 字符”，中文记忆实际只有约 1/3 容量
类型：doc-drift，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-memory/README.md, docs/UPSTREAM.md

证据：lib.rs:252-279：`fit_to_limit` 用 `content.len()` 和 `line.len()`（字节）判断。rupi-memory README.md:7 和 UPSTREAM.md:71 写的都是“各 5000 字符”。一个汉字 3 字节，中文 MEMORY.md 大约 1666 字时就开始丢最旧的行。

建议修法：统一改用 `chars().count()` 计算（单行截断那段已经按 chars 处理了），或者把文档改成“5000 字节”。

### F98 [P2] 会话库没有 messages(session_id) 索引，每次 resume/export 都全表扫描加临时排序；JsonlProvider 每轮同步读两遍整个 turns.jsonl
类型：perf，工作量：S，文件：crates/rupi-memory/src/lib.rs

证据：lib.rs:1088-1120 只有 PK，没有二级索引。对 `session_records` 的 SQL 执行 `EXPLAIN QUERY PLAN` 得到 `SCAN messages` + `USE TEMP B-TREE FOR ORDER BY`，list_sessions 每次构建 AUTOMATIC INDEX。lib.rs:924-941 的 `append_line` 为了检查最后一个字节 `std::fs::read` 整个文件，lib.rs:943-953 的 `reload_cache` 再整读一次，两者都在 async fn sync_turn（lib.rs:994）里做阻塞 IO；recall（lib.rs:1032）同样同步整读。

建议修法：迁移中加 `CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, created_at)`；JsonlProvider 用 seek 到末尾读 1 字节，缓存改为增量 push，IO 放进 spawn_blocking。

### F99 [P2] 会话 id 前缀解析只在最近 200 个会话里找，更早的会话无法用短 id 打开
类型：bug，工作量：S，文件：crates/rupi-memory/src/session_io.rs

证据：session_io.rs:767：`let rows = store.list_sessions(200)?;` 后按前缀过滤。会话数超过 200 后，`--resume <前缀>` 或 `--session <前缀>` 对较早的会话报 `unknown session`，即使前缀唯一。

建议修法：直接 `SELECT id FROM sessions WHERE id LIKE ?||'%' LIMIT 2`。

# Track: local-frontends-fixes

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/local-frontends-fixes`，worktree：`.worktree/local-frontends-fixes`

## 目标
本机 rupi（run/chat/tui/rpc）不崩、不丢会话、不污染 JSONL，provider 与设置在会话切换中保持正确，TUI 显示正确；把半成品（状态栏 context%/$、REPL 缺的命令）补完。

## 负责文件
- crates/rupi-cli/**
- crates/rupi-tui/**
- crates/rupi-config/**
- crates/rupi-agent/src/session.rs

不在本 track：其他 crate 归各自 track；需要它们配合时在报告里写清楚。

## 特别说明
- 0/35/68/132/161：rupi-cli main 改成 multi_thread 运行时（worker 数 2 即可），并加 smoke 测试用 examples/extensions/echo-rpc.json 跑 run/chat/--mode rpc，确认不再 panic。
- 另一个已确认的半成品：`cargo install --path crates/rupi-cli` 装出来的 rupi 找不到内建 skills（builtin_skills_dir 只从 exe 位置向上找 skills/builtin）。用 include_str! 内嵌 skills/builtin 作为兜底，并修 config_cmd 里 cwd 相对路径的同类问题（22/56/113/116）。
- `rupi login` 列出了不存在的 copilot provider（153）。
- 用户允许“按你的风格精简”：在 P0/P1 修完后，再做 180/54/53/250 的去重（REPL/TUI 斜杠命令、四个入口的装配），行为不变、测试全绿；做不完就在报告里列出。
- rpc 模式 64：--mcp-config 解析失败不再让整个程序退出，改为 warn。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F0 [P0] JSON-RPC 扩展在所有本机前端启动即 panic（current_thread 运行时 + block_in_place）
类型：bug，工作量：M，文件：crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs, crates/rupi-cli/src/main.rs

证据：main.rs:871 `#[tokio::main(flavor = "current_thread")]`；rupi-ext/src/lib.rs:73-75 `block_on_async` 在有运行时时调用 `tokio::task::block_in_place`，它只允许在 multi_thread 运行时使用。RpcHost::start(rpc.rs:23-26)、call_tool(rpc.rs:301-312)、expand_command(rpc.rs:277-299)、ExtensionSet::emit_event(lib.rs:346-350) 都经过这里。load_extensions(main.rs:645-668) 在 run_once/run_chat/run_rpc/run_tui(1956/2225/1841/2737) 中都会调用。复现：把 examples/extensions/echo_rpc.py 配成 jsonrpc manifest 放进 /tmp/rupi-probe/ext，执行 `RUPI_HOME=/tmp/rupi-probe/home rupi --ext-dir /tmp/rupi-probe/ext -p hello`，结果 exit=101，报 `panicked at crates/rupi-ext/src/lib.rs:75:18: can call blocking only when running on the multi-threaded runtime`；`--mode rpc` 和 `chat` 输出相同。单测 lib.rs:703 用 `flavor = "multi_thread"`，掩盖了这个问题。另外 ExternalRpcTool(rpc.rs:591-602) 没有实现 execute_with_cancel，取消时要等满 timeout_secs。

建议修法：短期：把 main 改成 multi_thread 运行时（S）。正式：RpcHost::start/call_tool/expand_command/on_event 全部改成 async；ExtensionSet::register/refresh_extensions 改成 async（调用方本来就在 async 上下文），删掉 block_on_async；ExternalRpcTool 实现 execute_with_cancel（select cancel 后发 `notifications/cancelled`）。补一条 CLI smoke：jsonrpc manifest + `rupi -p`。

### F35 [P0] JSON-RPC 扩展在任何入口都让 rupi 启动即 panic（current_thread + block_in_place）
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs

证据：main.rs:871 `#[tokio::main(flavor = "current_thread")]`；rupi-ext/src/lib.rs:73-75 `block_on_async` 在有 runtime 时走 `tokio::task::block_in_place`；RpcHost::start（rpc.rs:24-25）、expand_command（rpc.rs:284）、call_tool 都经过它，main.rs:645-668 load_extensions 启动即触发。实测：把 examples/extensions/echo-rpc.json 放进 $RUPI_HOME/extensions，执行 `rupi --no-session chat`、`rupi run hi`、`echo '{"type":"get_state"}' | rupi --mode rpc` 都输出 `thread 'main' panicked at crates/rupi-ext/src/lib.rs:75:18: can call blocking only when running on the multi-threaded runtime`，exit=101。run_tui（main.rs:2737）走同一条路径。单测 rupi-ext/src/lib.rs:703 用的是 `#[tokio::test(flavor = "multi_thread")]`，所以 CI 测不出来。docs/UPSTREAM.md 把 "jsonrpc 长连接双向 JSON-RPC" 列为已完成。

建议修法：main 改为 `#[tokio::main(flavor = "multi_thread", worker_threads = 2)]`（current_thread 的 TUI/REPL 本来就不依赖单线程语义）；或者把 RpcHost::start、expand_command、call_tool 改成 async，去掉 block_in_place。同时在 tests/smoke.rs 加一条用 echo-rpc.json 跑 chat/run/rpc 的端到端用例。

### F68 [P0] 任何 jsonrpc 协议扩展都会让 run/chat/tui 启动即 panic（current_thread 运行时里调用 block_in_place）
类型：bug，工作量：S，文件：crates/rupi-ext/src/lib.rs, crates/rupi-cli/src/main.rs

证据：main.rs:871：`#[tokio::main(flavor = "current_thread")]`。rupi-ext lib.rs:72-81：`block_on_async` 在有运行时时调用 `tokio::task::block_in_place`，lib.rs:385 的 `RpcHost::start` 用了它。实测：在 `$RUPI_HOME/extensions/echo.json` 放一个 `protocol:jsonrpc` 的 manifest，`rupi -p hi` 和 `rupi chat` 都报 `panicked at crates/rupi-ext/src/lib.rs:75:18: can call blocking only when running on the multi-threaded runtime`。这不在本子系统内，是验证项目层 extensions 注入时发现的，影响很大。

建议修法：main 改为 multi_thread 运行时；或者把 `ExtensionSet::register/load_all` 改成 async，直接 `.await` `RpcHost::start_async`，去掉 block_in_place。

### F132 [P0] 任何 jsonrpc 协议扩展都会让 chat/run/--mode rpc 启动即 panic
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs, examples/extensions/echo-rpc.json

证据：main.rs:871 是 `#[tokio::main(flavor = "current_thread")]`。rupi-ext/src/lib.rs:73-75 的 block_on_async 在有运行时时调用 `tokio::task::block_in_place`，而这个函数在 current_thread 运行时上会 panic。调用链是 RpcHost::start(rpc.rs:25) ← ExtensionSet::install_one(lib.rs:385) ← register(lib.rs:373) ← load_extensions(main.rs:666)。chat(main.rs:2225)、run(1956)、rpc(1841)、tui(2737) 全部经过这里。复现（cwd 有 echo_rpc.py）：`printf 'hi\n/quit\n' | rupi --model deepseek-v4-flash -e /tmp/rupi-e2e/ext/echo-rpc.json chat` → exit=101，stderr 为 `thread 'main' panicked at crates/rupi-ext/src/lib.rs:75:18: can call blocking only when running on the multi-threaded runtime`。`rupi -e .../echo-rpc.json run "say hi"` 和 `printf '{"type":"get_state"}\n' | rupi -e .../echo-rpc.json --mode rpc` 结果相同，都是 101。只用 oneshot 的 upper.json 时正常。单测 lib.rs:703 用的是 `flavor = "multi_thread"`，所以没测出来。README:239-244 和 crates/rupi-ext/README.md:12 都把 echo-rpc.json 当示例。如果一个包里带 jsonrpc 扩展并被 `rupi install` 装上，之后每次启动都会崩。

建议修法：二选一。(a) main 改成 `#[tokio::main(flavor = "multi_thread")]`。(b) block_on_async 先判断 `Handle::current().runtime_flavor()`：如果是 CurrentThread，就用 `std::thread::scope` 起一个线程，在里面建独立 runtime 去 block_on（rpc 连接要绑在这个专用 runtime 上，或者整体改成 async API）。另外在 rupi-cli/tests/smoke.rs 加一条用真实二进制加载 echo-rpc.json 的冒烟测试。

### F161 [P0] 任何 jsonrpc 扩展都会让启动 panic（current_thread runtime 下调用 block_in_place）
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs

证据：main.rs:871 `#[tokio::main(flavor = "current_thread")]`；rupi-ext lib.rs:73-75 的 block_on_async 调用 `tokio::task::block_in_place`，tokio 规定它在 current_thread 上会 panic。RpcHost::start（rpc.rs:23-26）、expand_command（284）、emit_event（lib.rs:348，TUI 的 Ctrl+L 会调到）都走这条路径。实际运行：`RUPI_HOME=/tmp/rupi-probe-home target/debug/rupi --ext-dir /tmp/rupi-probe/examples/extensions --no-session -p hi` 输出 `panicked at crates/rupi-ext/src/lib.rs:75:18: can call blocking only when running on the multi-threaded runtime`。run_tui 同样先调 load_extensions（main.rs:2737），所以 TUI 一启动就崩。唯一的 jsonrpc 单测（rupi-ext lib.rs:703）用的是 multi_thread flavor，掩盖了这个问题。

建议修法：main 改成 multi_thread（worker_threads=2 即可）；或者把扩展 RPC 改成真正的 async 接口，去掉 block_in_place。另加一个 CLI 冒烟测试，用 examples/extensions 启动 run 和 tui。

### F231 [P0] tracing 日志默认写到 stdout，压实和落盘告警会混进 --mode rpc / run --json 的 JSONL 流
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/lib.rs

证据：main.rs:873-876 用 tracing_subscriber::fmt().with_env_filter("info") 初始化，没有设置 writer，默认就是 stdout。agent 在压实时 tracing::info!("session compressed...")（lib.rs:1319-1323），溢出恢复时 warn（771-773），CLI 落盘失败时 warn（main.rs:1582-1583）。实测：RPC 连续 5 条 30KB prompt 之后，stdout 出现一行带 ANSI 颜色的非 JSON 文本："2026-10-31T17:38:27Z INFO rupi_agent: session compressed: 10 msgs, kept last 6 (20000 tokens budget)"。--resume 的实测中，WARN 行同样出现在 stdout。严格按 JSONL 解析的客户端会在这里出错。

建议修法：把 tracing 初始化改成 .with_writer(std::io::stderr)，在非 TTY 或 RPC/JSON 模式下关掉 ANSI，并允许 RUST_LOG 覆盖。补一个冒烟测试：rpc 模式下 stdout 每一行都必须能解析成 JSON。

### F67 [P0] tracing 日志写到 stdout：配置或 MCP 的 warning 会污染 --mode rpc 和 run --json 的 JSONL 输出
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:873-876：`tracing_subscriber::fmt().with_env_filter("info").try_init()`，没设 `with_writer(std::io::stderr)`，默认写 stdout，还带 ANSI 颜色。rupi-config 的 load_file（lib.rs:622）和 McpManager（rupi-mcp lib.rs:1382）的失败处理都是只 warn。实测：`run --json hi 2>/dev/null` 的 stdout 前两行就是 `WARN rupi_config: settings ... 解析失败`；`--mode rpc --mcp-config dead.json` 的 stdout 第一行是 `WARN rupi_mcp: MCP server 'dead' failed to start`，第二行才是 JSON。rpc 客户端按行解析会出错。

建议修法：`.with_writer(std::io::stderr)`，rpc/json 模式下关掉 ANSI（`.with_ansi(false)`）。

### F160 [P0] tracing 日志默认写到 stdout，TUI 全屏期间的 warn/info 会直接覆盖到画面上
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/lib.rs, crates/rupi-skills/src/lib.rs, crates/rupi-tui/src/app.rs

证据：main.rs:873-876 `tracing_subscriber::fmt().with_env_filter("info").try_init()` 没有设 with_writer；tracing-subscriber 0.3.23 的默认 writer 是 `fn() -> io::Stdout`（fmt/mod.rs:232）。TUI 的 alternate screen 也在 stdout 上。会在 TUI 内触发的日志：每次压实打 info `session compressed`（rupi-agent lib.rs:1319）；每次发送都调 `ctx.skills.refresh`（app.rs:1492），它会对同名 skill 打 warn `skill name collision`（rupi-skills lib.rs:218）；持久化或书签失败也会打 warn（app.rs:1431、tree_nav.rs:90）。同样的问题也会污染 `--mode rpc` 的 stdout JSONL 通道。

建议修法：改成 `.with_writer(std::io::stderr)`，并支持 RUST_LOG。TUI 模式下进一步写到文件（`$RUPI_HOME/logs/rupi.log`），或用 tracing Layer 把 warn 级别转成 view.push_system；env filter 不要硬编码 info。

### F38 [P1] tracing 日志写到 stdout，污染 `run --json` 和 `--mode rpc` 的 JSONL；--review-apply 也往 stdout 打印
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:873-876 `tracing_subscriber::fmt().with_env_filter("info")`，默认 writer 是 stdout，并且带 ANSI 色码。main.rs:611 的注释明确写着“run --json 的 stdout 必须是纯 JSONL”。实测：在 settings.json 里写入非法 JSON 后执行 `rupi run --json hello 2>/dev/null`，用 Python 逐行 json.loads，得到 `non-json stdout lines: 3`（内容是 `\x1b[33m WARN rupi_config: settings ... 解析失败`）；RPC 下的 persist 失败 WARN 也出现在 stdout。MCP server 启动失败（rupi-mcp lib.rs:1381）同样走 tracing::warn。另外 apply_suggestions（main.rs:48-66）用 println；`rupi --review-apply --no-session run --json "请记住我爱喝乌龙茶"` 的 stdout 中有一行非 JSON 的 `[review] memory saved`。

建议修法：使用 `.with_writer(std::io::stderr)`，非 TTY 时关闭 ANSI。apply_suggestions 改为 eprintln，或者接收一个 sink 参数：json 模式下输出为 JSONL 事件，否则写 stderr。

### F135 [P1] tracing 日志写到 stdout 且带 ANSI 颜色，污染 --json 和 --mode rpc 的 JSONL
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:873-876 `tracing_subscriber::fmt().with_env_filter("info").try_init()` 没有 with_writer，默认写 stdout，且开着 ANSI。复现一：`rupi --model deepseek-v4-flash --session 75d6a815-... --compress-threshold 127990 --compress-keep 10 run --json "Reply: J" > json3.out; grep -v '^{' json3.out` → 输出 `\x1b[2m2026-10-31T16:40:55Z\x1b[0m \x1b[32m INFO\x1b[0m rupi_agent: session compressed: 9 msgs, kept last 3`。复现二：rpc 模式下 stdout 混进 `WARN rupi: persist turn failed: ...`（rpc3.out）。chat REPL 里 /compact 时同一条 INFO 也打在 stdout。run.rs 的注释说 --json 时 stdout 只走 JSONL(main.rs:2045)，与实际行为不符。

建议修法：改成 `.with_writer(std::io::stderr)`，非 TTY 时关掉 ANSI（`.with_ansi(std::io::stderr().is_terminal())`），默认级别降到 warn，并接受 `RUST_LOG`。补测试：--json/rpc 的 stdout 每一行都能被 serde_json 解析。

### F229 [P0] AgentSession::prompt 遇到 provider 错误后 streaming 不复位，RPC 之后的 prompt 全部被拒为 busy，用户节点也不回滚
类型：bug，工作量：S，文件：crates/rupi-agent/src/session.rs, crates/rupi-cli/src/rpc.rs

证据：prompt_queued 在 session.rs:326 把 streaming 置为 true，run_with_user(...).await? 在 356 行出错时直接返回，366 行的 store(false) 执行不到。RPC serve 在 rpc.rs:308 要求 !session.is_streaming() 才接受 prompt，否则进入 dispatch，返回 "empty message or missing streamingBehavior while busy"（329-334）。实测：RUPI_BASE_URL=http://127.0.0.1:9/v1 运行 /tmp/rupi-ro-target/debug/rupi --mode rpc，第一条 prompt 返回 connection refused，第二条 prompt 返回 {"success":false,"error":"empty message or missing streamingBehavior while busy"}，get_state 返回 isStreaming:true, messageCount:1。探针 P3：AgentSession 出错后 is_streaming_after_err=true，messages_after_err=1（REPL 和 TUI 都会回滚，SDK 不回滚）。

建议修法：在 prompt_queued 里用 guard 或 match 保证任何出口都把 streaming 置回 false。出错时与 REPL/TUI 一致，回滚 before_len 之后的节点（session.rs 记下 current_path.len()）。补一个 SDK 单测：失败的 provider 跑完后 is_streaming()==false，且历史为空。

### F230 [P0] RPC/AgentSession 只在 stdin EOF 时落盘一次：--resume 时因主键冲突整批回滚，new_session/switch 会丢掉前一个会话，崩溃则全部丢失
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/session.rs

证据：run_rpc 只在 rpc::serve 返回后调用一次 persist_turn_if(rt.persist, &store, &sess.session_id, &sess.session, 0, ...)（main.rs:1896-1905）。before_len=0 意味着整棵树都要重新 INSERT。AgentSession::prompt_queued（session.rs:320-368）从不落盘，sess_db/persist 字段只有 duplicate_session 和 set_session_name 在用。实测：先跑一次 RPC，再用 --resume 跑一次，stdout 出现 "WARN rupi: persist turn failed: UNIQUE constraint failed: messages.id"，sessions.db 里仍只有第一次的 2 条消息，第二次的回合全部丢失。另一次实测：prompt A、new_session、prompt B 之后，sessions 表只有 A 的 id 且 A 没有消息；B 的 2 条消息挂在一个不存在于 sessions 表的 session_id 上（new_session 在 session.rs:374-379 不建行）。

建议修法：把每轮落盘移进 AgentSession：prompt_queued 结束时，如果 persist 为真且有 sess_db，就把 before_len 之后的新节点用 persist_turn 写入（与 CLI 的 turn_rows 共用逻辑）。new_session 时先 create_session_ex 建行。run_rpc 收尾时不再全量重插。补一个 RPC 端到端测试：resume 后回合能落盘；new_session 前后两个会话都能落盘。

### F36 [P0] RPC 模式会话落盘只在 EOF 时全量写一次：--continue、clone、new_session、被 kill 都会丢回合
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/session.rs

证据：main.rs:1896-1905 只在 `rpc::serve` 返回（stdin EOF）后调用 `persist_turn_if(..., before_len=0, ...)`，把整棵树全部 INSERT。实测四种情况：(1) `rupi run first` 之后执行 `echo prompt | rupi --mode rpc --continue`，stdout 出现 `WARN persist turn failed: UNIQUE constraint failed: messages.id`，`rupi sessions` 显示仍是 2 msgs，新回合整体丢失；(2) 先 prompt 再 clone 再 prompt：duplicate_session（session.rs:498-514）已经 persist_tree，EOF 时再次全量插入撞主键，sqlite 查到 `post-clone rows: 0`；(3) new_session（session.rs:374-379）只换内存 id、不建 sessions 行，之后的消息落成孤儿行（session_id=b30b3e1d，sessions 表计数 0），`rupi --resume b30b3e1d run hi` 报 `unknown session`；(4) prompt 完成后 SIGTERM，sqlite 显示 messages=0。TUI（app.rs:1763-1777）和 REPL（main.rs:2688）都是每轮落盘，只有 RPC 不同。

建议修法：在 rpc::serve 的 run_prompt 和 run_bash 结束后按本轮的 before_len 增量落盘（复用 turn_rows/persist_turn_if）。把 persist 逻辑下沉到 AgentSession::prompt_queued，让 new_session 走 create_session_ex、clone/fork 之后重置游标。EOF 时只补未落盘的尾部。

### F133 [P0] --mode rpc 续接已有会话后，新回合整批落盘失败，内容全丢
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-memory/src/lib.rs

证据：run_rpc 只在退出时落一次盘：main.rs:1897-1905 `persist_turn_if(rt.persist, &store, &sess.session_id, &sess.session, 0, ...)`，before_len 传的是 0，等于把整条 current_path 重新 INSERT 一遍。persist_turn(lib.rs:1364-1386) 是单个事务里的普通 INSERT，主键冲突就整体回滚。复现：`(echo '{"id":"p","type":"prompt","message":"Reply with exactly: RESUMED-RPC"}'; sleep 20; echo '{"id":"s","type":"get_state"}') | rupi --model deepseek-v4-flash --session 216fd7e8-... --mode rpc`。get_state 显示 messageCount=5，退出时 stdout 出现 `WARN rupi: persist turn failed: UNIQUE constraint failed: messages.id`。之后 `rupi sessions` 里这条会话仍是 `(3 msgs)`，`rupi session-show 216fd7e8-... | grep -c RESUMED-RPC` 输出 0。rpc 的 fork/clone 也有同样问题：duplicate_session 已经 persist_tree(session.rs:512)，退出时再全量插入一次，同样报 UNIQUE。另外 rpc 只在 stdin EOF 时落盘，进程被 kill 会丢掉整场会话。

建议修法：rpc 模式改成每次 agent 回合结束（agent_end）后按本轮新增节点增量落盘，与 run/chat 的 before_len 语义一致，并记录已落盘的节点集合。或者让 persist_turn 用 `INSERT OR IGNORE` / UPSERT，并对 resume、fork 后的树只写新节点。补一条测试：rpc --session 续接，prompt 一次，退出后检查 msgs 数量。

### F37 [P0] 会话切换后重建 provider 时丢失 provider 前缀和 CLI 选项，OpenRouter/Azure/Bedrock/Vertex/llama.cpp 被静默换成 mock 或打到错误端点
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/session.rs, crates/rupi-tui/src/app.rs, crates/rupi-llm/src/route.rs

证据：REPL `/import`（main.rs:1723-1731）、AgentSession::adopt_session（session.rs:462-467，RPC 的 switch_session/fork/clone 都会走到）、TUI 的 /resume、/new、/import、/fork、/clone 和会话选择器（app.rs:898、1361、1388）都只用 `provider.model_id()` 重建，丢掉了 provider 名。route.rs:96-104 的 infer_provider 对 `meta-llama/llama-3-8b` 推断为 openai。TUI 的 `/model`（app.rs:519）和所有 TUI 重建都调用 `provider_for_model`（rupi-llm lib.rs:436-438，`ProviderOptions::default()`），不带 --api-key/--provider。实测：`OPENROUTER_API_KEY=<redacted> rupi --mode rpc --model openrouter/meta-llama/llama-3-8b`，执行 clone 后再 prompt，回复变成 `demo mode: missing key for meta-llama/llama-3-8b`，stderr 没有任何提示；REPL 下 /import 一个 jsonl 后，stderr 打出 `no RUPI_API_KEY/OPENAI_API_KEY — using mock provider`，随后的对话都是 mock。如果宿主同时设置了 OPENAI_API_KEY，TUI 会把 OpenRouter 的模型 id 发到 api.openai.com。

建议修法：统一一个 `rebuild_provider(current_spec, session_id, &ProviderOptions)`，用 `format!("{}/{}", provider.name(), model_id)`（或者在上下文里保存当前 spec 字符串）作为 spec。给 TuiContext 加 provider_opts 并传入 CLI 的 ProviderOptions。rebuild 失败时保留旧 provider，不要降级成 mock。

### F172 [P1] TUI 内重建 provider 绕过 CLI 的 build_provider：--api-key/--provider 失效，openrouter 被错路由，Ctrl+L 静默切到 mock
类型：bug，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-llm/src/route.rs, crates/rupi-cli/src/main.rs

证据：/model（app.rs:519）、选择器回车（896-901）、Resume（1359-1369）、Adopt（1386-1392）、Ctrl+L/P（2325）都调用 `rupi_llm::provider_for_model`，它用的是 ProviderOptions::default()（rupi-llm lib.rs:436-437），CLI 的 --api-key/--provider 被丢掉；REPL 用的是 `build_provider(arg, Some(&sid), &provider_options(cli))`（main.rs:2417）。会话切换时用 `provider.model_id()` 重建，openrouter 的 model_id 是 "anthropic/claude-sonnet-4.5"（models.json），split_provider_prefix（route.rs:70-78）把 anthropic 当成 provider，结果直连 Anthropic，或者失败后保留带旧亲和 id 的 provider（Adopt 和选择器静默，Resume 只打一行 skipped）。cycle_and_switch_model 的 Err 分支（2336-2342）静默装上 MockProvider，却返回 Ok(spec)；mock 的 model_id 为 None，下次 Ctrl+L 的 position 找不到当前项，总是回到 list[0]，后面的模型永远轮不到。

建议修法：把 ProviderOptions 放进 TuiContext，所有重建统一走一个 `rebuild_provider(spec, sid)`。会话切换保留完整 spec（provider/model[:thinking]），用 set_session_id 更新亲和而不是重建；缺 key 时返回 Err 并提示，不装 mock。Ctrl+L 只轮转有 key 的模型（或 enabledModels）。

### F238 [P1] AgentSession.set_model 缺 key 时静默换成 Mock 并报成功；switch/fork/clone 用 model_id 重建 provider，丢失 provider 前缀
类型：bug，工作量：S，文件：crates/rupi-agent/src/session.rs, crates/rupi-llm/src/route.rs

证据：set_model 调用 provider_or_mock(spec, ...)（session.rs:386），缺 key 时回落到 Mock，返回 "demo mode: missing key"（route.rs:290-297），却照常返回 provider/id，RPC 回 success（rpc.rs apply_set_model）。adopt_session 用 provider_or_mock(self.provider.model_id().unwrap_or("gpt-4o-mini"))（session.rs:462-467）重建 provider：openrouter/anthropic/claude-3.5-sonnet 的 model_id 是 "anthropic/claude-3.5-sonnet"，重新解析后变成 anthropic provider 加 claude-3.5-sonnet（split_provider_prefix 在 route.rs:69-77），azure/bedrock/llamacpp 会被推断成 openai，Mock 会变成 gpt-4o-mini。也就是说，RPC 里一次 fork 或 switch_session 就可能换掉 provider，或者悄悄降级到 demo。

建议修法：AgentSession 保存当前的 model spec 字符串，set_model 成功后更新。adopt_session 不重建 provider，只调用 set_session_id，需要的话由 provider trait 增加 with_session_id。set_model 改用 provider_from_spec，失败时返回 Err，由 RPC 回 success:false。

### F115 [P1] RPC 切换、fork、clone 会话时只用 model_id 重建 provider，provider 前缀丢失
类型：bug，工作量：S，文件：crates/rupi-agent/src/session.rs, crates/rupi-llm/src/route.rs

证据：adopt_session 用 `provider_or_mock(self.provider.model_id().unwrap_or("gpt-4o-mini"), ...)` 重建（session.rs:462-465），switch_session、fork、clone 都会走到这里（session.rs:431,436,448,514,517）。model_id 不含 provider：OpenRouter 的 "anthropic/claude-sonnet-4.5" 经 parse_model_spec 变成原生 Anthropic、模型 "claude-sonnet-4.5"（ID 非法）；Azure 部署名 "gpt-4o" 变成 OpenAI；llamacpp 的 "local" 变成需要 key 的 openai，然后静默回退成 MockProvider。

建议修法：在 AgentSession 里保存原始 spec 字符串（构造时和 set_model 时写入），重建时用它；或者给 trait 加 `fn spec(&self) -> String`。

### F159 [P0] 消息区滚动算法错误：最新的至少 2 行（含边框差与折行）永远被裁掉
类型：bug，工作量：M，文件：crates/rupi-tui/src/app.rs

证据：app.rs:2000-2001 用 `start = total - (chunks[0].height + scroll)`，但正文可用高度是 chunks[0].height-2（Borders::ALL），且 total 是逻辑行数，而 Paragraph 开着 wrap（2037）会把长行折成多行，所以从 start 开始渲染时底部被裁。kitty 叠加（2137-2138）用同一公式，图片行位也会错。scroll 只做 saturating_add（831、1105、1622）没有上限钳制。探针（/tmp 副本加测试，40x12 TestBackend）：推 20 行 System 后 `has L19=false L18=false L17=true`；推一条 300 字符的长行再推 TAIL，结果 `wrap has TAIL=false`。流式输出时最新内容恰好落在屏幕外。

建议修法：按 inner 宽度计算折行后的视觉行数（或改用 ratatui `Paragraph::line_count`、`Paragraph::scroll((offset,0))`），用 inner_h=height-2 锚定底部：offset = wrapped_total - inner_h - scroll；scroll 钳到 [0, wrapped_total-inner_h]。kitty 叠加复用同一个 offset。补一个 TestBackend 回归测试，断言最后一行可见。

### F162 [P0] 切换会话（/resume、/sessions 回车、/new、/fork、/clone、/import、-r、--resume）后视图不重画，屏上显示的是另一个会话
类型：bug，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/slash.rs

证据：run_loop 启动时只推 banner（app.rs:730-732），从不把 ctx.session.history() 回放进 ChatView。会话切换的三处（选择器回车 891-910、Builtin::Resume 1352-1375、Builtin::Adopt 1382-1394）只替换 *ctx.session 并 push 一行 `[resumed ..]`，view.lines 没有清空，也没有重建。/rewind、/goto、/tree 回车（979-981）同样不更新视图。后果：`rupi --resume X` 进来看不到历史；/resume 之后屏上是旧会话的对话，模型看到的却是新会话；/copy 取的是 view 里的旧助手文本（slash.rs:11、app.rs:1180）。

建议修法：新增 `ChatView::replay(&SessionTree)`：清空 lines，按 history() 回放 User、AssistantText、Tool（ToolCall/ToolResult 块）、Thinking 行。在启动、所有 Adopt/Resume、goto、rewind 之后调用（可以统一放进下面的 adopt_session）。/copy 改用 last_assistant_from_session。

### F163 [P1] TokenMeter 在 TUI 里从不累加；带 context%/$ 的 status_footer 已成死代码，状态栏缺少 README 承诺的 context% 和 $
类型：incomplete，工作量：M，文件：crates/rupi-tui/src/app.rs, README.md, docs/UPSTREAM.md

证据：meter 只在 select! 的 `Some(e) = rx.recv()` 分支里更新（app.rs:1709-1726）。但每圈循环开头的 flush_turn_view（1609、1614、1793-1813）会先用 try_recv 把 Usage 抢走，这条路径只写 chrome.footer，不碰 meter。探针连跑 10 轮 drive_turn（provider 发 Usage input=100）：每轮都是 `footer.input=100 meter.input=0`，`meter counted in 0/10 rounds`。所以 /session 附加的用量行（1325-1330）永远是 ↑0 $0。status_footer（1846-1867，用 meter 算 context% 和 $）在 c0a895e（#9）里被 paint_busy 和空闲 draw 调用；52c8736（#11）改成 footer_text（1869-1903，只显示末次请求的 ↑↓RW CH%，不累计，没有 context% 和 $）后它失去调用方，只剩测试 status_footer_shows_token_cost_line（2480）在测死代码。README.md:205 写着“状态栏：↑↓ tokens / context% / $”。

建议修法：把 Usage 处理抽成一个函数，flush_turn_view 和 recv 分支共用（同时更新 meter 和 chrome.footer）。footer_text 改成读 meter：累计值加 `meter.calibrate(session.history_tokens())` 得到 context%，再加 $；回合中途 fut 借着 session，用回合前缓存的 ctx_tokens。删掉 status_footer 和 FooterState 里的重复计数，把测试改成测 footer_text 加 meter。

### F179 [P2] 死代码：sessions_block、status_footer，以及 dispatch_builtin 里根本到不了的 /copy /hotkeys /trust /edit /commands 分支
类型：simplify，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/slash.rs

证据：sessions_block（app.rs:264-289）是 269dba7 的 /sessions 文本列表，47b6dfc（#14）改成 Builtin::SessionNav 后只剩测试 3572 在用；REPL 有一份逐字相同的副本（main.rs:1611-1631）。status_footer 见 meter 那条。run_loop 先走 parse_local_slash（1177-1211，拦下 copy/hotkeys/trust/edit）和 /commands（1212-1228），所以 dispatch_builtin 的 352-364、374-384 永远到不了：/trust 只返回桩 "[trust]"，/hotkeys 用的是 KeyTable::default()，/commands 没有 prompt filter；Builtin::Editor（260、1285-1295）也是死分支。测试 read_only_views_never_fall_through_to_model（3414-3455）断言的正是这些桩，覆盖率是假的。

建议修法：删掉 sessions_block（或者下沉到 rupi-memory 给 REPL 复用）、status_footer（并入 meter 修复）、上述 5 个死分支、Builtin::Editor，以及对应测试，约省 90 行。

### F65 [P0] 项目层 .rupi/settings.json 不经信任门：可以自设 defaultProjectTrust=always 给自己授信，也可以通过 extensions 注入扩展工具
类型：bug，工作量：M，文件：crates/rupi-config/src/lib.rs, crates/rupi-cli/src/main.rs

证据：lib.rs:138-153：`Settings::load` 总是合并 `find_project_settings(cwd)`，与信任结果无关。main.rs:477、1816、1922 用合并后的 `project_trust()` 做信任判定。main.rs:537-543 的 `apply_resource_filters` 无条件把 `settings.extensions` 的路径 `add_extra`。实测 1：在含 .git 和 .rupi/skills 的目录里写 `.rupi/settings.json`=`{"defaultProjectTrust":"always"}`，`rupi -p hi` 输出 `[trust] defaultProjectTrust=always: 本次加载项目资源`；删掉该文件后输出 `非交互默认跳过项目资源`。实测 2：写 `{"extensions":["./ext"]}`，信任被跳过的情况下，`ext-list` 仍列出项目里的 `pwn [oneshot]` 工具，模型调用时就会执行任意命令。上游文档把 defaultProjectTrust 标为 agent-directory only。

建议修法：拆成两步：先只读全局层（rupi + pi），据此决定信任；信任通过后再合并项目层。`defaultProjectTrust` 只从全局层取。项目层的 `extensions`/`skills`/`themes`/`sessionDir` 路径只在 load_project=true 时生效。

### F66 [P0] /settings 写回时遇到语法不合法的 settings.json 会把整个文件替换为 {key}，用户其余配置静默丢失
类型：bug，工作量：S，文件：crates/rupi-config/src/lib.rs

证据：lib.rs:402-420：`persist_patch` 的 `serde_json::from_str(...).unwrap_or_else(|_| json!({}))` 解析失败即视为空对象，然后整文件写回，且不是原子写。实测：`~/.rupi/settings.json` 里有 model/enabledModels/steeringMode 加一个 trailing comma，执行 `/settings theme light` 后文件只剩 `{"theme":"light"}`。`rupi config enable/disable` 走 persist_toggle 后也调同一函数（resources.rs:175），有同样问题。

建议修法：解析失败时拒绝写入并报错（提示行号），或者先备份成 `settings.json.bak` 再写；写入改为临时文件加 rename 的原子写。

### F233 [P0] --subagents 子 agent 绕过 --tools/--exclude-tools、hook 和 tool 可见性过滤
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-agent/src/subagent.rs

证据：run_once、run_chat、run_tui 三处都是先 SubagentTool::new(provider, Arc::new(tools.clone()), ...)，再调用 apply_tool_filter(&mut tools, &rt)（main.rs:2000-2013、2290-2303、2786-2799），所以子注册表是过滤之前的完整副本。子循环 AgentLoop::new(self.max_turns)（subagent.rs:250-258）只继承 policy/approver/plan_mode/thinking（inherit_from 在 171-177），不继承 tool_allow/tool_deny/hooks/on_ask/discovery。效果：rupi --exclude-tools bash --subagents 时父模型看不到 bash，但可以调用 subagent(goal)，子 agent 依然有 bash 可用。

建议修法：在 apply_tool_filter 之后再构造 SubagentTool（先过滤，再克隆，再注册 subagent）。inherit_from 补充复制 tool_allow、tool_deny、hooks、on_ask、tool_execution、discovery 配置。补一个单测：父循环 exclude bash 时，子回执里出现 "bash!" 或 "unknown tool"。

### F39 [P1] REPL 的 /import、/export 遇到 I/O 错误时整个 REPL 进程退出
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：handle_session_slash 中 `tokio::fs::write(&path, body).await?`（main.rs:1701）、`read_to_string(path).await?`（1711）、`import_into_store ... ??`（1719）把错误向上抛，调用处 main.rs:2533-2543 `.await?` 直接让 run_chat 返回 Err。实测：`printf '/import /nonexistent.jsonl\nstill alive?\n' | rupi chat` 输出 `Error: No such file or directory (os error 2)`，exit=1；`/export /nonexistent-dir/x.html` 同样退出；导入空 jsonl 也以 `Error: jsonl contained no messages` 退出。TUI 同名命令（app.rs:590-593、620）返回 `Builtin::Done("[export] failed: ...")`，不会退出。

建议修法：handle_session_slash 内部把错误打印成 `[import] {e:#}` / `[export] failed: {e:#}` 后返回 Ok(true)；或者调用处对 Err 只打印、继续循环。

### F41 [P1] 每次启动都立刻建会话行，`-c/--continue` 会续到最近一个空会话，而不是最近的真实对话
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-memory/src/lib.rs

证据：restore_or_new 在 main.rs:1525-1526 一启动就调用 create_session_ex，进 chat/TUI 后直接 /quit 也会留下一个 0 msgs 的会话。latest_session（rupi-memory lib.rs:1267-1287）按 `COALESCE(updated_at, created_at) DESC` 取第一条，不过滤空会话。实测：`rupi run REAL-CONVERSATION` 后执行 `printf '/quit\n' | rupi chat`，再执行 `rupi -c run follow-up`，输出 `[continue] latest session 62e6761b... restored 0 msgs`。上游 usage.md 写的是“Continue most recent session”。

建议修法：latest_session 加上 `AND EXISTS(SELECT 1 FROM messages WHERE session_id = sessions.id)`；更好的做法是第一次落盘时才建 sessions 行（懒创建），顺便解决 sessions 列表里堆积空会话的问题。

### F144 [P1] --continue 和 -r 会接回最近一条 0 消息的空会话；每次失败的 -p 和每次 /new 都留下空会话
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-cli/src/main.rs

证据：latest_session(lib.rs:1267-1291) 按 `COALESCE(updated_at, created_at)` 取最新一条，不过滤空会话。restore_or_new 在调用 provider 之前就 create_session_ex(main.rs:1525-1526)。实测：REPL 里先 /new（生成 5d6dbff1，0 msgs）再 /quit，然后执行 `rupi -c -p "What exact word did you reply with in your previous answer?"` → stderr `[continue] latest session 5d6dbff1-... restored 0 msgs`，模型回答 `There's no previous answer in this conversation`。`rupi sessions` 共 20 行，其中 4 行是 `(0 msgs)`。

建议修法：latest_session 只选有消息的会话（加 `EXISTS (SELECT 1 FROM messages WHERE session_id=id)`）。或者把建会话推迟到第一次 persist_turn 时（懒创建），失败的运行也就不会留下空行。

### F184 [P2] `rupi -r` 每次都先往库里建一条空会话，选择器里取消也会留下来
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：run_tui 调 `restore_or_new(cli,&db,cli.pick_session)`（main.rs:2751-2754）。defer_picker=true 时只打印 `[pick] opening session selector`（1491-1492）然后继续往下走，最后到 1525 `create_session_ex("default", ...)`。用户在选择器里选别的会话或按 Esc，这条 0 消息的行都会留下，/sessions 里空会话越积越多。

建议修法：推迟创建：-r 时先用临时内存树，用户首次发送或在选择器 Esc 时再 create_session_ex；或者在选中其他会话时删掉这条空行。

### F45 [P1] `--mode` 接受任意值，`--mode json` 静默输出纯文本；README 声称已对齐
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, README.md

证据：main.rs:213-215 的 `mode: Option<String>` 没有 value_parser，只有 main.rs:894 处理 `Some("rpc")`。实测：`rupi --mode json --no-session hi </dev/null` 在 stdout 输出 demo 纯文本，不是 JSONL；`rupi --mode bogus ...` 也正常运行。上游 https://github.com/earendil-works/pi/blob/v0.85.1/packages/coding-agent/docs/usage.md 的 Modes 表列有 `--mode json`。README.md:162 表格写“默认七工具 + TUI / run / --mode json / --mode rpc ✓”，实际只有 `run --json` 能用。

建议修法：`#[arg(long, value_parser = ["text","json","rpc"])]`；`--mode json` 等价于 `run --json`（取 rest 和 stdin 作为 prompt）；`--mode text` 按上游语义处理。

### F46 [P1] `rupi cloud` 在没有 RUPI_CLOUD_KEY 时把本机 LLM key（RUPI_API_KEY）当 Bearer 发给控制面
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:1101-1106：`cli.api_key.or(RUPI_CLOUD_KEY).or(RUPI_API_KEY)`。RUPI_API_KEY 是 OpenAI 兼容 provider 的 key（rupi-llm route.rs:200）。报错文案只提到 RUPI_CLOUD_KEY。实测：只设置 RUPI_API_KEY=<redacted> 时，`rupi cloud --url http://127.0.0.1:9 me` 照常发起请求（报 `cloud GET /v1/me` 连接失败），说明该 key 已被当作 Bearer 使用。用户连接第三方或自建控制面时，模型密钥会泄露给另一个服务。另外，全局的 `--api-key` 同时承担 LLM key 和 cloud key 两种含义。

建议修法：去掉对 RUPI_API_KEY 的回退。cloud 子命令自己定义 `--key`（env = RUPI_CLOUD_KEY），不复用全局的 --api-key。

### F48 [P1] REPL 与 TUI 斜杠命令不对齐：REPL 缺 /resume 和 !cmd，自己的提示却让用户用 /resume
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：REPL 的 `/sessions` 输出 `recent sessions (\`/resume\` 或 \`--session\`)`（main.rs:1617），但 run_chat 和 handle_session_slash 里都没有 /resume 分支。实测 `/resume abc` 被当作普通消息发给模型（返回 demo 回复）。TUI 有 /resume（app.rs:444-456）和 `!cmd`/`!!cmd`（app.rs:1153-1176）；REPL 中 `!echo BANG-OUTPUT` 同样被发给模型。上游 usage.md 中这两项都是交互模式的基础功能。TUI `/model` 无参数时显示 provider.name()（app.rs:517），REPL 显示的是 model（main.rs:2415），两者也不一致。REPL `/fork abc` 带参数时同样被当作消息发给模型。

建议修法：REPL 复用 rupi_tui 的 resolve_session_arg 和 replay_session 实现 /resume；补 `!`/`!!`（复用 run_bang_cmd）。根本解法是把内建斜杠分发抽成一个共享函数（见 simplify 条目）。

### F139 [P1] REPL 没有 /resume，/sessions 的提示却让用户用 /resume；输入后被当普通消息发给模型
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, README.md

证据：handle_session_slash(main.rs:1600-1805) 和 run_chat 主循环(2321-2560) 都没有 /resume 分支，TUI 有(app.rs:444)。REPL 里 /sessions 的标题是 `recent sessions (\`/resume\` 或 \`--session\`):`(main.rs:1617)。README:163 列出 `/sessions`, `/resume <id>`，并说「Most also work in the chat REPL」。实测：在 repl1.in 里输入 `/resume` 后只回显一个空的 `> `，stderr 出现 `[error] openai-compat HTTP 403 Forbidden: This token has no access to model nosuchprovider/xyz`（前一步 /model 切到了假 provider），说明 /resume 被当成用户消息发给了模型。

建议修法：把 TUI 的 resume 解析逻辑（app.rs:291-300 的短前缀解析加 restore_tree）挪进 handle_session_slash 供两边共用，REPL 横幅也加上 /resume。

### F140 [P1] /help 在 REPL 和 TUI 都没实现，会当成 prompt 发给模型
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-tui/src/complete.rs, README.md

证据：README:170 写 `/help` 是 TUI-only 命令。但 TUI 的 dispatch_builtin(app.rs:335-680) 没有 /help 分支，complete.rs:12-15 的 BUILTINS 里也没有 "help"，代码里唯一的 "/help" 出现在测试 app.rs:2618。REPL 同样没有。实测：`printf '/help\n/quit\n' | rupi --model deepseek-v4-flash chat` → 模型调用了 bash/glob/read，自己编了一段帮助文字，消耗 `↑6.4k ↓445` token，review 还打印 `[review] skill draft: auto-help — /help`。

建议修法：REPL 和 TUI 都加 /help，内容从 complete::BUILTINS 加一行说明生成（复用 TUI_HELP 或启动横幅的文字），同时补进补全列表。

### F183 [P2] REPL 与 TUI 对等性缺口：REPL 的 /sessions 指引 /resume，但 REPL 没有实现 /resume；REPL 也没有 !cmd，TUI 没有每轮扩展热重载
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：main.rs:1617 打印 "recent sessions (`/resume` 或 `--session`)"，但 rupi-cli 里搜不到 "/resume" 处理，输入后会当成普通消息发给模型；REPL 也没有 `!`/`!!` 处理（grep 无结果），`!ls` 会发给模型。反过来，REPL 每轮调 refresh_extensions（main.rs:2563），TUI 只在 /reload 时调，而 TuiContext.ext_set 的注释（app.rs:59）却写“与 REPL 同语义”。

建议修法：上面那条共享命令层落地后，/resume 和 !cmd 自然两端都有；TUI 每次发送前调 refresh_extensions（只做一次 mtime 扫描），或者修正注释。

### F22 [P2] `rupi config list skills` 用 cwd 相对的 skills/builtin，仓库外运行看不到内建 skill
类型：bug，工作量：S，文件：crates/rupi-cli/src/config_cmd.rs, crates/rupi-cli/src/main.rs

证据：config_cmd.rs:151-152 调 `skill_search_dirs(PathBuf::from("skills/builtin"), ...)`，而运行时用的是 exe 锚定的 builtin_skills_dir(main.rs:514-531)。实测：cwd=/tmp 时 `rupi skills-list` 能看到 commit-helper，`HOME=/tmp/rupi-probe/fakehome rupi config list skills` 输出 `(none)`。list_skills 还用 HOME 环境变量重新算了一遍 user home，和 main.rs 的 dirs_home 重复。

建议修法：把 skill_dirs/builtin_skills_dir 移到 rupi-skills（或 rupi-config），作为 pub fn，config_cmd 与 main 共用；同时删掉 config_cmd 里的重复实现（约 10 行）。

### F56 [P2] `rupi config list skills` 用相对路径 skills/builtin，换了 cwd 内建 skill 就显示 (none)
类型：bug，工作量：S，文件：crates/rupi-cli/src/config_cmd.rs

证据：config_cmd.rs:151-152 写的是 `skill_search_dirs(PathBuf::from("skills/builtin"), ...)`，而 main.rs:514-531 的 builtin_skills_dir() 专门修过这个问题（注释说明“换个 cwd 跑就静默丢失内建技能”）。实测在 /tmp 下执行：`rupi config list skills` 输出 `skills:\n  (none)`，同一环境下 `rupi skills-list` 能列出 commit-helper。另外 list_prompts 和 list_skills 固定传 load_project=true，不经过信任门。

建议修法：把 builtin_skills_dir 挪到 rupi-skills 或 rupi-config 里共享，config_cmd 和 main 共用一份。

### F113 [P1] `/thinking off` 与"不干预"发出的请求完全相同，默认开思考的模型关不掉
类型：bug，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/gemini.rs

证据：lib.rs:37-39 注释承诺 "Some(Off) = 显式关闭（能关的 provider 省 token）"。实际 openai_effort(Off)、gemini_level(Off)、anthropic_budget(Off) 都返回 None（lib.rs:107,119,128），请求体与 thinking=None 一模一样。按各家文档，DeepSeek V4、Gemini 2.5 Flash/Pro 和 Claude 4.6+ 的部分型号默认开着思考，`--thinking off` 和 `/thinking off` 对它们不起作用。Gemini 只会用 thinkingLevel 表达档位（gemini.rs:72-77），2.5 Flash 关思考需要 `thinkingBudget: 0`；2.5 是否接受 thinkingLevel，官方文档没说清，有第三方报告 400，未能实测。

建议修法：Off 按 provider 和模型映射：DeepSeek 发 `thinking:{type:"disabled"}`，OpenAI gpt-5 发 `reasoning_effort:"minimal"`，Gemini 2.5 发 `thinkingBudget:0`（仅 flash 可关），Gemini 3 发 `thinkingLevel:"minimal"`。依赖上面能力描述那一条的模型能力表。

### F116 [P1] 云端 BYOK 只认三种 key，其余 provider 拿到 OpenAI key；缺 key 时生产环境静默回复 demo 文本
类型：incomplete，工作量：M，文件：crates/rupi-server/src/run.rs, crates/rupi-llm/src/route.rs, crates/rupi-tui/src/app.rs

证据：resolve_byok 的 key 分支只有 anthropic、gemini 和兜底（rupi-server/src/run.rs:814-840）。租户 provider 为 openrouter、azure、bedrock、vertex、llamacpp 时，openai_api_key 或 api_key 被当作 ProviderOptions.api_key 传进去（run.rs:851-858），例如 OpenAI key 被当成 Bedrock Bearer 发出。LAUNCH.md:98 也只写了三种 key，Azure endpoint、Vertex project 等无法按租户配置。provider 推断在这里重抄了一份（run.rs:806-813，对应 route.rs:96-104）。default_provider 走 provider_or_mock（run.rs:859 → route.rs:290-297）：生产环境缺 key 时，助手消息是 "demo mode: missing key for X"，run 本身不报错。TUI 循环切模型出错时也静默换成 mock，界面仍显示 "[model spec]"（rupi-tui/src/app.rs:2336-2342）。

建议修法：按 `{provider}_api_key` 通用查找，租户 settings 支持 azure_endpoint、vertex_project/location。云端仅在 provider==mock 或 RUPI_CLOUD_MOCK=1 时用 mock，其余缺 key 直接让 run 失败并给出原因。route::infer_provider 改为 pub 复用。TUI 切换失败时显示错误。

### F49 [P1] Pi 的 settings 只读合并后不生效：defaultModel、defaultProvider、defaultThinkingLevel 没有别名
类型：incomplete，工作量：S，文件：crates/rupi-config/src/lib.rs, docs/UPSTREAM.md

证据：Settings::load（rupi-config lib.rs:138-153）会合并 ~/.pi/agent/settings.json，但结构体（79-127）只给 `defaultTools` 加了 alias（87 行），`model` 和 `thinking` 没有对应 Pi 键名。实测：在 HOME/.pi/agent/settings.json 写入 `{"defaultProvider":"anthropic","defaultModel":"claude-sonnet-4-5","defaultThinkingLevel":"high"}` 后执行 `rupi -p hi`，走的仍是 openai 路由，回复为“设置 RUPI_API_KEY…”。上游 https://github.com/earendil-works/pi/blob/v0.85.1/packages/coding-agent/docs/settings.md 中的 hideThinkingBlock、retry.*、shellPath、shellCommandPrefix、doubleEscapeAction、treeFilterMode、images.blockImages、enableSkillCommands 也全部缺失，并且 serde 会静默丢弃这些键。docs/UPSTREAM.md 的 CLI/TUI 表写着“只读合并 ~/.pi/agent/settings.json”，没有说明这些键不认。

建议修法：给 model 加 `alias = "defaultModel"`，thinking 加 `alias = "defaultThinkingLevel"`；defaultProvider 与 defaultModel 拼成 `provider/model`。在 /settings 输出里列出被忽略的 Pi 键，并在 UPSTREAM.md 写清支持范围。

### F86 [P1] “只读合并 ~/.pi/agent/settings.json”只对同名键有效：Pi 实际的键名 defaultModel、defaultProvider、defaultThinkingLevel 都不读，Pi 的 model/thinking 设置不会生效
类型：incomplete，工作量：S，文件：crates/rupi-config/src/lib.rs, crates/rupi-config/README.md, docs/UPSTREAM.md

证据：lib.rs:80-84 只认 `model`/`thinking`。上游 settings.md 用的是 `defaultProvider`/`defaultModel`/`defaultThinkingLevel`（取值还包括 `minimal`）。实测：Pi 层写 `{"defaultModel":"anthropic/claude-x","defaultThinkingLevel":"high"}`，`/settings` 显示 `model (unset)` / `thinking (unset)`。README.md:7 和 UPSTREAM 都写着会合并 Pi 设置，没有说明键名不同。hideThinkingBlock、doubleEscapeAction、retry、shellPath、enableSkillCommands 等 Pi 键也全部被忽略，且没有任何提示。

建议修法：Settings 增加 `#[serde(alias)]` 或在合并 Pi 层时映射：defaultModel（与 defaultProvider 拼成 `provider/model`）映射到 model，defaultThinkingLevel 映射到 thinking（minimal 映射到 low）。README 列出已支持和明确不支持的 Pi 键。

### F85 [P1] settings 是强类型整文件反序列化：任意一个键类型不对就丢弃整个文件；Pi 合法的取值（quietStartup:"header"、对象形式的 packages）会让整层 Pi 设置失效
类型：bug，工作量：M，文件：crates/rupi-config/src/lib.rs

证据：lib.rs:615-626：`load_file` 遇到任何 serde 错误都返回 `Settings::default()`。实测：`{"theme":"light","tools":"read,bash"}` 因 tools 不是数组而整文件忽略，theme 也丢了；`{"defaultProjectTrust":"Always"}`（大写）也会丢整个文件。`~/.pi/agent/settings.json` 写 `{"steeringMode":"all","quietStartup":"header"}` 或 `"packages":[{"source":"npm:@x/y","skills":[]}]` 时（两种都是上游文档列出的合法形式）报 `invalid type`，整层 Pi 设置被丢弃，`/settings` 显示 steeringMode 为默认值。同一警告还会打印 3 次（Settings::load 每次启动被调 3 次以上）。

建议修法：先解析成 `serde_json::Value`，再逐键用 `from_value` 容错（坏键单独 warning 后跳过）；quietStartup 接受 bool 或 "header"；packages 接受 string 或 `{source,...}`；ProjectTrust 解析时忽略大小写。进程内缓存 load 的结果。

### F95 [P2] settings.thinking 取值非法时直接终止启动，与“非法配置只 warning”的设计不符；Pi 的 minimal 档也被拒
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-config/README.md

证据：main.rs:1237-1244：settings 里的 thinking 解析失败时返回 `?`。实测 `{"thinking":"bogus"}` 导致 `Error: thinking 解析失败...` 并退出。rupi-config README.md:3 写着“非法文件只 warning，主循环不因配置崩”。上游 defaultThinkingLevel 包含 `minimal`，rupi 只有六档。

建议修法：来自 settings 的取值解析失败时 warning 并回退为 None（CLI flag 解析失败仍报错）；把 `minimal` 映射到 low。

### F136 [P1] chat REPL 的信任提问和审批提问读的是同一个管道 stdin，会吃掉脚本行；`echo task | rupi` 什么都不做
类型：bug，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-core/src/trust.rs, README.md

证据：load_project_resources(main.rs:465-502) 没检查 stdin 是不是 TTY，直接调用 ask_trust_stdin(trust.rs:70-82)。后者注释写着「管道/EOF 直接按跳过处理」，代码实际只处理了 EOF。TerminalApprover(main.rs:1184-1194) 也直接 read_line 读 stdin。复现一（项目里有 .rupi/MEMORY.md）：`printf '/greet who=Alice\n/quit\n' | rupi --model deepseek-v4-flash chat` → 输出 `加载并注入模型上下文？... [trust] 已跳过项目资源`，第一行 /greet 被当成了信任回答，REPL 什么都没执行。复现二：`printf 'Run this bash command exactly: dd if=/dev/zero of=/dev/null count=1\nReply with exactly: SECOND\n/quit\n' | rupi chat` → stderr 出现 `[approve] bash ... [y(es once)/a(ll session)/N]`，第二行被当成审批回答，SECOND 那一轮没跑。复现三：`echo "Reply: BARE" | rupi --model deepseek-v4-flash`（无子命令、非 TTY）走 main.rs:1163-1164 的 run_chat，任务被信任提问吃掉，exit 0、没有任何回复。README:136 写的是 `rupi chat` 「Works over SSH and in pipes」。run 路径(main.rs:1915-1942) 非交互时明确不提问，chat 路径没这样做。

建议修法：stdin 不是 TTY 时，信任门按 run 的语义处理（默认跳过并提示，--trust-project 加载），审批器退化成 AutoApprover(false) 或 --approve。也可以在有 TTY 时改从 /dev/tty 读回答。无子命令、非 TTY、stdin 有内容时，应和 -p 一样把 stdin 当 prompt（append_stdin_if_piped）。

### F150 [P2] REPL 的 /model 不校验 provider，切到不存在的模型也提示成功，下一轮才报 403
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:2412-2436 只要 build_provider 返回 Ok 就打印 switched。未知前缀会退回 openai-compat，模型名原样带上。实测 `/model nosuchprovider/xyz` → `[model switched to nosuchprovider/xyz]`，下一轮出现 `[error] openai-compat HTTP 403 Forbidden: This token has no access to model nosuchprovider/xyz`。

建议修法：前缀不是已知 provider、且模型不在 catalog 也不在 enabledModels 时给出警告或要求确认。也可以切换时发一次轻量探测请求。

### F173 [P1] TUI 的 replay_session 只取最早 500 行且不设 tree.id：长会话丢最新上下文，/export、/share 头部 id 错误
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-memory/src/lib.rs, crates/rupi-memory/src/session_io.rs

证据：replay_session（app.rs:323-332）调用 `session_records(id, 500)`；SQL 是 `ORDER BY created_at ASC ... LIMIT ?`（rupi-memory lib.rs:1488），超过 500 行时保留的是最旧的 500 行，丢掉最近的。CLI 的 restore_tree（session_io.rs:788-797）用 10_000 并设置 `tree.id = id`。tree_from_records 用 SessionTree::new() 生成随机 uuid（session_io.rs:867-868），TUI 版本没有覆盖，而 export_tree_jsonl 用 tree.id 写头部（session_io.rs:48），所以 /resume 之后 /export、/share 的 JSONL 头部 id 和库里的会话 id 对不上。

建议修法：删掉 replay_session，直接用 `rupi_memory::restore_tree`（顺带去重约 10 行）；测试断言 tree.id == id。

### F176 [P1] 空闲循环每个事件（含鼠标移动）都扫命令目录、读文件并完整克隆渲染缓存；有内联图时每帧重传整张图
类型：perf，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-core/src/commands.rs, crates/rupi-tui/src/kitty.rs

证据：run_loop 每圈（773-794）都调 commands::list_filtered：它对每个目录做 read_dir，并且对每个 .md 调 describe_file 读全文（commands.rs:402-409、433-443）；还有 ext list_commands 和 current_dir。鼠标事件（825-844）处理后 continue 到循环顶部重算并重绘；EnableMouseCapture 开的是 ?1003h any-motion（crossterm event.rs:327），移动鼠标每秒触发数十次。draw 里 `visual_lines(..).to_vec()`（1979-1986）克隆整个渲染缓存，`lines.get(start..).to_vec()`（2028）再克隆一次，`chrome.theme.clone()`（1988）每帧克隆主题。paint_visible（kitty.rs:309-330）每帧先 delete 全部放置，再用 a=T 重新传输每张可见图的完整 base64；流式输出时每个 delta 都会触发一次（2116-2146）。

建议修法：custom_names 缓存起来，只在 /reload、/settings 后刷新；只在可见窗口上构建 Paragraph（借用切片，或者 Paragraph::scroll 加上 Text 引用）；Mouse Moved 事件直接忽略，不重绘（或改成只开 ?1000h/?1006h）；kitty 按 image_id 只传一次，之后用 a=p 放置，并且只在滚动或布局变化时重放。

### F60 [P2] TUI 每个事件循环都重新扫描命令目录并读取每个 .md 全文
类型：perf，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-core/src/commands.rs

证据：app.rs:773-785 在 loop 顶部、每次 draw 之前都调用 `commands::list_filtered(&ctx.command_dirs, ...)`。它对每个目录做 read_dir（commands.rs:17-47），并对每个文件调用 describe_file 读取全文（commands.rs:402-409、433-436），而补全只用到名字。每次按键和鼠标事件都会触发这一套同步文件 I/O。

建议修法：缓存命令名列表，只在 /reload、/settings 或目录 mtime 变化时重建；补全只调用 discover_filtered 取名字，不读描述。

### F164 [P1] 输入框只有一行：换行看不见、长输入被截断、中文光标错位
类型：bug，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/view.rs

证据：输入框固定 Constraint::Length(3)（app.rs:1996），渲染方式是 `RLine::from(vec![mark, Span::raw(pre), Span::raw(post)])`（2050-2054），没有 wrap。ratatui 的 Span::styled_graphemes 会过滤掉 "\n"，所以多行输入被拼成一行。光标 x 用 `pre.chars().count()`（2110）计算，不是显示宽度。探针：insert_str("line1\nline2") 后输入行显示 `> line1line2`；输入“你好”后光标在 (5,9)，期望 x=7。超出宽度的文本被截断，光标也会落到框外。

建议修法：输入区高度随内容增长（min(lines,8)+2）。按 `unicode-width` 计算折行和光标位置（ratatui 已依赖 unicode-width），并按光标行做纵向滚动。也可以直接引入 tui-textarea。

### F165 [P1] Shift+Enter 换行在大多数终端不生效（没开 keyboard enhancement），运行中更是无法插入换行
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/keybindings.rs, crates/rupi-tui/README.md

证据：launch（app.rs:201-208）只开了 alt screen、鼠标和 bracketed paste，全仓没有 PushKeyboardEnhancementFlags（grep 无结果）。传统编码下 Shift+Enter 发的是 \r，crossterm 解析成不带修饰键的 KeyCode::Enter（parse.rs:92-94），于是 keys.newline（keybindings.rs:103）永远匹配不上，按下就直接发送。运行中 collect_busy（230-235）同样依赖 SHIFT。README 和 TUI_HELP（684）却把 Shift+Enter 写成主要的换行方式。

建议修法：启动时如果 `crossterm::terminal::supports_keyboard_enhancement()` 为真，就 Push DISAMBIGUATE_ESCAPE_CODES，Guard 里 Pop。增加 Ctrl+J 作为通用换行（raw mode 下 \n 被解析为 Ctrl+J）。文档写明各终端的差异。

### F166 [P1] keybindings.json 改绑 send/newline 后 TUI 无法发送；Ctrl/Alt+字母会被当普通字符插入
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/keybindings.rs

证据：主 match 里 `KeyCode::Char(c) => input.push_char(c)`（app.rs:1123-1126）没有修饰键守卫，并且排在 Enter 分支（1127-1132）前面；Enter 分支又要求 `KeyCode::Enter` 才会检查 keys.send。按 keybindings.rs:212-221 测试用的配置 `{"send":"ctrl+s"}`：Ctrl+S 会插入 's'，Enter 不再匹配 send，落进 `_ => {}`，结果没有任何键能发送。newline 改绑成 ctrl+j 时同样只会插入 'j'。Ctrl+A、Ctrl+W、Ctrl+U、Alt+B 都会插入字母。运行中 collect_busy 把 Enter/Alt+Enter 写死，Ctrl+O 也写死（1685），不读 KeyTable。

建议修法：主 match 先按 KeyTable 判断 send/newline（不限 KeyCode::Enter）；Char 分支加守卫 `if !key.modifiers.intersects(CONTROL|ALT)`；运行中也用 keys.send/newline/fold。补一个 run_loop 层的按键路由测试（把键处理抽成纯函数）。

### F167 [P1] 运行中排队输入会丢掉大写字母
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs

证据：collect_busy（app.rs:224-229）只接受 `KeyCode::Char(c) if key.modifiers.is_empty()`；crossterm 0.28 对大写字母会自动加 SHIFT（parse.rs:128-134 char_code_to_event）。探针：`collect_busy(KeyEvent::new(Char('H'), SHIFT))` 返回 NotMine，buf 为空。运行中输入 "Hello" 转向，实际排队的是 "ello"。

建议修法：守卫改成 `!key.modifiers.intersects(CONTROL | ALT)`，单测补上 SHIFT 大写。

### F168 [P1] /tree 与 /sessions 导航器没有视口，条目多时选中行在屏幕外
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/tree_nav.rs, crates/rupi-tui/src/session_nav.rs

证据：tree_lines（app.rs:2176-2231）和 session_lines（2410-2451）返回全部条目，draw（2023-2026）从第一行开始渲染，没有 offset。TreeNavigator 初始选中最后一个在路径上的节点（tree_nav.rs:69），也就是最底部。探针：30 条消息，40x14 终端：`selected=29 of 30 ; marker visible=false ; msg29 visible=false`。会话选择器最多 50 条（app.rs:753、1299），向下移动超过一屏后同样看不到选中行。

建议修法：给两个 Navigator 加 `offset` 字段，move_by 时按 body 高度保持选中行可见；渲染时只切 `vis[offset..offset+h]`；标题栏已显示 n/m。

### F170 [P1] 审批弹问与 Ctrl+G 外部编辑器：鼠标捕获未关、回来后不重绘，stdin 还可能被 crossterm 线程抢读
类型：bug，工作量：M，文件：crates/rupi-tui/src/app.rs

证据：TuiApprover::approve（app.rs:166-171）只关 raw、bracketed paste 和 alt screen，没有 DisableMouseCapture（launch 开的是 ?1003h any-motion，crossterm event.rs:327）。用户回答时移动鼠标，SGR 报文会混进 read_line，`ApprovalAnswer::parse` 结果变成 Deny。返回时 EnterAlternateScreen（179-183）会清空 alt 屏，但全仓没有调 terminal.clear()（grep 仅测试后端），ratatui 的 diff 以为画面还在，界面基本空白，直到内容变化。open_external_editor（1913-1930）同样没有 clear。另外 crossterm EventStream 的后台线程会阻塞在 poll_internal 上读 tty（stream.rs:44-58；tty_fd 在 isatty 时就是 stdin，file_descriptor.rs:124-126），与 read_line 竞争同一个 fd。

建议修法：审批时同时关闭和恢复 MouseCapture；回到 TUI 后调 `terminal.clear()`（approver 拿不到 terminal，可以设一个 needs_full_redraw 原子标志，由 paint_busy 检查）。更好的做法是把审批做成 TUI 内的模态 overlay（y/a/N 按键），不离开全屏，也不和 EventStream 抢 stdin。

### F171 [P1] macOS 上 Ctrl+V 是空操作（只尝试 wl-paste/xclip），README 写的“Ctrl+V 贴图”在主力平台不可用
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/slash.rs

证据：clipboard_image_bytes（app.rs:2367-2387）和 clipboard_text（2393-2408）只调用 wl-paste、xclip；本机 `which` 显示 wl-paste/xclip 不存在，pbpaste 和 osascript 存在。两个都失败后得到 ClipboardPaste::None，按键无反馈。/copy 的 clipboard_set_text（slash.rs:45-49）却包含 pbcopy，两边不一致。

建议修法：macOS：文本用 pbpaste；图片用 `osascript -e 'the clipboard as «class PNGf»'` 或 `pngpaste`。Windows 用 powershell Get-Clipboard。失败时 push 一行 `[paste] no clipboard tool`。

### F57 [P2] --name 在 -c/--resume/--session 时被忽略；RPC 的 --name 只在内存里，不落库
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：restore_or_new 只在新建会话时把 cli.name 传给 create_session_ex（main.rs:1525-1526），恢复会话的分支（1480-1524）不调用 set_name。实测 `rupi --name renamed-on-continue -c run hi` 之后 `rupi sessions` 不显示新名字。run_rpc 中 `sess.session_name = cli.name.clone()`（main.rs:1888）只影响 get_state，不写 DB。上游 cli.md 中 `-n, --name` 的说明是“Sets the session display name”，不区分新建和恢复。

建议修法：restore_or_new 拿到 id 后，如果 cli.name 有值且需要落盘，就调用 sess_db.set_name(id, name)。

### F58 [P2] 全局 flag 必须写在子命令之前；crate README 宣传的 `run -p` 实际会被拒绝
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-cli/README.md

证据：Cli 的字段都没有 `global = true`（main.rs:80-218）。实测 `rupi run --no-session --model gpt-4o hi` 报 `unexpected argument '--no-session'`；`rupi cloud --url .. --api-key k me` 报 `unexpected argument '--api-key'`；`rupi run -p hi` 报 `unexpected argument '-p'`。crates/rupi-cli/README.md:8 写的是 `rupi run [-p] [--json] …`。

建议修法：给常用 flag（model、provider、api-key、session 系列、tools 系列、thinking、approve 系列）加 `#[arg(global = true)]`，或者在 README 中注明 flag 必须写在子命令前，并删掉 `run [-p]` 的写法。

### F59 [P2] `--tools` 遇到未知名、`+name` 或 glob 时静默变成空工具表
类型：bug，工作量：S，文件：crates/rupi-config/src/lib.rs, crates/rupi-cli/src/main.rs

证据：rupi-config lib.rs:515-533 的 parse_tool_list 和 tool_allowed 只做精确匹配。实测 `rupi --mode rpc --tools '+bash'` 和 `--tools 'ba*'` 发 bash 命令都返回 `bash tool is not registered`，启动时没有任何告警。上游 0.85 只支持纯名单，但最新版 https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md 已支持 `+name/-name` 增量和 `*` 通配，用户照新文档写会得到零工具的 agent，且不会察觉。

建议修法：装配完成后，对 --tools 中没有匹配到任何已注册工具的项发出 stderr 告警；可选支持 `+/-` 增量和 `*` 通配。

### F92 [P2] mcp-list 参数解析：以 `-` 开头的 server 参数（npx -y、python -I）必须加 `--`；给了 --url 仍要求 COMMAND
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：main.rs:312-318：`McpList { command: String, args: Vec<String>, #[arg(long)] url }`，没有 `trailing_var_arg` 或 `allow_hyphen_values`，command 也不是 Option。实测：`rupi mcp-list npx -y @modelcontextprotocol/server-everything` 报 `unexpected argument '-y'`；`rupi mcp-list --url http://...` 报 `required arguments were not provided: <COMMAND>`。README 的 `mcp-list --url` 用法跑不通。

建议修法：`command: Option<String>`，加 `#[arg(trailing_var_arg = true, allow_hyphen_values = true)] args`；command 和 url 二选一用 `required_unless_present`。

### F138 [P1] README 里的 `rupi mcp-list --url ...` 跑不通；stdio 参数带 `-` 前缀（如 npx -y）会被 clap 拒绝
类型：doc-drift，工作量：S，文件：crates/rupi-cli/src/main.rs, README.md

证据：main.rs:312-318 的 McpList 中 `command: String` 是必填位置参数，`args: Vec<String>` 没设 trailing_var_arg 和 allow_hyphen_values。README:199-200 给的用法是 `rupi mcp-list --url http://127.0.0.1:8000/mcp`。实测：`rupi mcp-list --url http://127.0.0.1:9/mcp` → `error: the following required arguments were not provided: <COMMAND>`，exit 2。`rupi mcp-list npx -y @modelcontextprotocol/server-filesystem .` → `error: unexpected argument '-y' found`，exit 2。`rupi mcp-list python3 -I x.py` 同样报错，必须写成 `python3 -- -I x.py` 才行。另外 mcp-list 用硬编码前缀显示工具名 `mcp_echo`(main.rs:1119 `mcp_tool_to_definition("mcp", ...)`)，会话内实际注册的名字是 `fake_echo`（按 server 名加前缀），两边对不上。

建议修法：command 改成 `Option<String>`，与 --url 二选一并加校验。args 加 `#[arg(trailing_var_arg = true, allow_hyphen_values = true)]`。mcp-list 增加 `--name`，默认用 command 的 basename 作前缀，与会话内命名一致。

### F151 [P2] ext-list 忽略 -e/--extension 和 --no-extensions，与运行时加载的集合不一致
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：ExtList(main.rs:990-1010) 只用 ext_dir 和 settings 过滤，没有像 load_extensions(main.rs:645-668) 那样调用 add_extra 和 set_discover。实测 `rupi -e /tmp/rupi-e2e/ext/upper.json ext-list` → `no extensions in .../extensions`，而同样的 -e 用在 chat 里能加载并调用 upper 工具。

建议修法：ext-list 复用 load_extensions 里构建 ExtensionSet 的那段逻辑（抽成 build_ext_set(cli, home, settings)）。

### F153 [P2] login 的提示与 --model 支持的 provider 对不上，未知 provider 也 exit 0
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：print_login_stub(main.rs:697-717) 只列出 anthropic/openai/copilot/vertex/bedrock。实测 `rupi login gemini` → `unknown login provider 'gemini'.`，exit=0。但 `--model` 的帮助里明确支持 gemini/openrouter/azure/llamacpp。copilot 的提示是 `no API key path in this binary`。

建议修法：login 的 provider 列表与 route.rs 的已知 provider 共用一份来源，补上 gemini、openrouter、azure 的环境变量提示，未知 provider 返回非零码。

### F148 [P2] session-show 不接受短前缀（--session/--resume/--export 都接受），找不到时仍 exit 0
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-memory/src/session_io.rs

证据：main.rs:1032-1037 直接用 `store.has_session(&id)` 判断，没有调用现成的 resolve_session_id(session_io.rs:783)。实测：`rupi session-show 75d6a815` → `unknown session: 75d6a815 (see \`sessions\`)`，exit=0。用完整 uuid 可以正常显示。同一前缀 `--session 75d6a815`、`--resume 75d6a815`、`--export 75d6a815` 都能解析。

建议修法：先调用 resolve_session_id，失败时 bail 返回非零码。

### F157 [P2] --no-tools 下系统提示仍点名 memory/memory_search/session_search/load_skill
类型：incomplete，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-memory/src/lib.rs

证据：工具定义确实被过滤掉了(agent lib.rs:653-656 的 retain(tool_visible))，但记忆和 skill 的引导文字仍然注入系统提示。实测 `rupi --no-tools run --json "List the exact names of the tools/functions you can call right now"` → `no tool manifest is present ... The only tool names explicitly referenced in my instructions are: memory, memory_search, session_search, load_skill`。

建议修法：组装系统提示时，按过滤后实际可见的工具集决定是否注入 memory/skill 的引导块和 prompt_snippet。

### F94 [P2] quietStartup、enabledModels、externalEditor 只在 TUI 生效：REPL 横幅不受控，/model 和 RPC get_available_models 不按 enabledModels 过滤
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-agent/src/session.rs

证据：quiet_startup 只在 app.rs:714 被读取，REPL 在 main.rs:2305 无条件打印帮助横幅。enabled_models 只被 TUI 的 Ctrl+P 读取（app.rs:1055-1076），REPL 的 /model（main.rs:2412）不校验，RPC `get_available_models`（rpc.rs:462、session.rs:406-419）返回全目录。external_editor 只用于 TUI 的 Ctrl+G（app.rs:1099）。这些键都在 /settings 里显示可改，但大部分前端不认。

建议修法：REPL 横幅改为 `if !settings.quiet_startup()`；`available_models` 接收 enabledModels 并过滤（为空表示全部）；REPL 增加 `/model`（无参数时列出 enabledModels）或按序循环。

### F42 [P1] RPC 缺 7 个上游命令，get_state/get_session_stats 字段不全，auto-retry 开关是空壳
类型：incomplete，工作量：M，文件：crates/rupi-cli/src/rpc.rs, crates/rupi-agent/src/session.rs

证据：上游 https://github.com/earendil-works/pi/blob/v0.85.1/packages/coding-agent/docs/rpc.md 列出了 cycle_model、cycle_thinking_level、get_available_thinking_levels、export_html、get_fork_messages、get_entries、get_last_assistant_text。实测这 7 个命令都返回 `unknown command: ...`（rpc.rs:518-522）。get_state（session.rs:273-286）缺 model、isCompacting、sessionFile。get_session_stats（rpc.rs:430-442）的 tokens 只是最近一次 Usage（session.rs:327-336 覆盖写），没有 cacheRead/cacheWrite/cost/contextUsage/userMessages/toolCalls。set_auto_retry 只写一个 AtomicBool（session.rs:292-294），全仓只有 state() 读它，重试次数固定为 3（rupi-llm lib.rs:597、614 `post_json_with_retry(..., 3)`）；abort_retry（rpc.rs:451-453）直接回 ok。get_tree 返回 `{tree: 字符串, entries: 扁平数组}`（session.rs:522-542），上游是 `{tree:[{entry,children}], leafId}`。

建议修法：补齐上述 7 个命令，大部分可以复用现有的 TokenMeter、export_tree_html、SessionTree。stats 改为累计 usage 并接入 TokenMeter 的成本和 context%。把 auto_retry 透传给 provider 的重试次数（关闭时为 0）。get_tree 按上游嵌套结构输出。

### F43 [P1] RPC prompt 不展开 prompt 模板、skill 和扩展命令，但 get_commands 把它们列为可调用
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/rpc.rs, crates/rupi-agent/src/session.rs

证据：rpc.rs:308-311 用 queued_from_cmd 原样构造消息，再经 prompt_queued 到 to_user_message（rupi-agent queue.rs:29），中间没有任何 expand。get_commands（session.rs:559-585）返回 prompt/skill/extension 三类命令。上游 rpc.md 写明“Skill commands (/skill:name) and prompt templates (/template) are expanded before sending/queueing”，扩展命令要立即执行。实测：放置 commands/greet.md 后，get_commands 返回了 `greet`，但发送 `/greet world` 后 get_messages 里用户消息仍是原文 `/greet world`，模板没有生效。REPL（main.rs:2584-2596）和 TUI 用的都是 rupi_tui::slash::expand_slash_input。

建议修法：在 serve 的空闲 prompt 分支调用 expand_slash_input(command_dirs, skills, expand=true)，再调用 ext.expand_command。把这个函数下沉到 rupi-core 或 rupi-agent，避免 rpc 依赖 tui crate。steer/follow_up 按上游语义同样展开模板，但不执行扩展命令。

### F44 [P1] --mode rpc 静默忽略 --approve、--subagents、--review*，四个入口的能力不一致
类型：incomplete，工作量：M，文件：crates/rupi-cli/src/main.rs

证据：run_rpc 在 main.rs:1809 写的是 `let _ = approver_for(cli, None)?;`，算出的 approver 被丢弃，agent 构建（1858-1874）里没有 with_approver，所以 `--mode rpc --approve` 下 Ask 裁决仍被拒绝。run_rpc 全函数（1808-1907）没有 subagents 和 reviewer 的装配（grep 计数为 0），而 run_once（2000-2040）、run_chat（2257-2302）、run_tui（2786-2860）都有。MCP 在 RPC 里用的是 spawn_all，不是 watched 版本（1834），收不到 tools/list_changed。

建议修法：把四个入口的装配合并成一个 builder（见 simplify 条目）。RPC 下审批可以接 extension_ui_request/confirm，没有 UI 时按 --approve/--no-approve 处理。

### F243 [P1] --mode rpc 丢弃 --approve，也不装配 --subagents 和 review，与 run/chat/TUI 行为不一致
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：run_rpc 写的是 `let _ = approver_for(cli, None)?;`（main.rs:1809），返回值被丢掉，后面的装配（1857-1874）不调用 with_approver，所以 rupi --mode rpc --approve 下 Ask 一律被拒。RPC 装配里也没有 cli.subagents 和 cli.review_enabled() 两个分支（对比 run_once 的 2000-2040 行），这两个 flag 在 rpc 下静默无效。

建议修法：抽一个共享的 build_agent（见后面的 simplify 条目），四个前端走同一份装配。RPC 至少接上 approver_for 的返回值；交互式审批可以复用 extension_ui_request 的 confirm 往返。

### F1 [P0] 扩展 UI dialog（select/confirm/input/editor）在 TUI/REPL 无人应答，RPC 流式期间被吞
类型：incomplete，工作量：L，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-cli/src/rpc.rs, crates/rupi-tui/src/view.rs, crates/rupi-agent/src/session.rs

证据：RpcHost 收到 ui/select 等请求后会排队，并阻塞等 oneshot，默认 30s（rpc.rs:182-200, 236-271）。消费端只有 AgentSession::poll_extension_ui/complete_extension_ui(session.rs:296-308)，而它们只在 rpc.rs:145/167 被调用。TUI 和 REPL 全仓 grep 不到 poll_ui_requests/complete_ui_request，view.rs:248 的 ExtensionUiRequest 渲染分支没有任何生产者。RPC 模式下 emit_pending_ui 只在空闲 tick 调用(rpc.rs:289-293)，run_prompt 的 select(rpc.rs:618-637) 不轮询 UI；流式期间收到的 `extension_ui_response` 在 handle_during_prompt 里直接回 ok 什么也不做(rpc.rs:850-852)，bash 期间同样如此(rpc.rs:725-727)。扩展在 tools/call 中弹的确认框，按现状必然超时并返回 cancelled。handle_incoming 是单任务串行处理(rpc.rs:45-49)，等待 dialog 的 30s 里该扩展的其他请求和通知全部堵住。

建议修法：RPC：run_prompt/run_bash 的 select 增加 50ms tick 调 emit_pending_ui，handle_during_prompt/handle_during_bash 把 extension_ui_response 转给 session.complete_extension_ui（需要把 extensions 句柄放进 StateSnap）。TUI：drive_turn 期间轮询 ext_set 的 poll_ui_requests，弹 overlay（select/confirm/input），Enter/Esc 时回 complete_ui_request。REPL：在 stdin 上同步提问。handle_incoming 每条请求 spawn 一个任务，避免互相阻塞。

### F52 [P1] 扩展 UI 对话框（select/confirm/input）只有 RPC 端消费，TUI 和 REPL 永远等到 30s 超时后取消
类型：incomplete，工作量：M，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-agent/src/session.rs, crates/rupi-tui/src/app.rs

证据：扩展发出 ui/confirm 等请求时，rupi-ext rpc.rs:182-196 把事件推进 ui_out 并等待 oneshot，默认 30_000ms，超时后回复 `{cancelled:true}`。只有 AgentSession::poll_extension_ui（session.rs:296-302）会排空 ui_out，而它只被 rpc.rs:144-148 调用。TUI 和 REPL 从不调用 poll_ui_requests 或 complete_ui_request（grep 无结果），view.rs:248 的 ExtensionUiRequest 分支永远收不到事件。结果是在 TUI/REPL 中，扩展的确认框每次都卡住工具最多 30s，然后自动取消。这个问题目前被上面的 panic 掩盖。

建议修法：TUI 主循环每帧调用 ext_set 的 poll_ui_requests，弹出 select/confirm/input 覆盖层，用 complete_ui_request 回复；REPL 走 stdin 问答。notify 直接渲染为系统行。

### F175 [P1] 扩展 UI 对话框（ui/confirm、ui/select、ui/input、ui/editor）在 TUI 里从不展示，扩展只能等 30s 超时拿到 cancelled
类型：incomplete，工作量：M，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-agent/src/session.rs, crates/rupi-tui/src/view.rs, crates/rupi-tui/src/app.rs

证据：对话类请求经 push_dialog 进入 RpcHost.ui_out，然后等 oneshot（rpc.rs:181-199、236-271，默认 30_000ms）。只有 AgentSession::poll_extension_ui 和 complete_extension_ui（session.rs:296-308，RPC 模式）会消费它们。TUI 只调 drain_ui_hints（app.rs:1519），从不调 poll_ui_requests/complete_ui_request。view.rs:248-253 的 ExtensionUiRequest 分支对 TUI 是死代码，因为该事件不经 on_event 发出。

建议修法：drive_turn 和空闲循环里轮询 `ExtensionSet` 的 UI 请求（ExtensionSet 增加 poll 和 complete 透传），用 overlay 渲染 confirm、select、input；按键结果再 complete 回去。

### F47 [P1] `rupi cloud prompt` 不流式，有 120s 总超时，不处理 interrupt；遇到一次 Ask 后线程永久 409
类型：incomplete，工作量：M，文件：crates/rupi-cli/src/cloud.rs, crates/rupi-server/src/run.rs

证据：cloud.rs:109-111 的 reqwest 设置了 `timeout(120s)`，是整个请求的总时限；cloud.rs:175 `resp.text().await` 读完整个 SSE 才解析，运行中没有任何输出，超过 120s 的 run 直接失败。parse_sse（cloud.rs:199-234）只认 TEXT_MESSAGE_* 和 RUN_ERROR。服务端对 Ask 用 `with_on_ask(... AskAction::Interrupt)`（run.rs:368），返回 RUN_FINISHED outcome=interrupt（agui.rs:96-102）；此后同一 thread 再发 prompt 会在 run.rs:111-115 返回 `409 pending interrupts require resume`。CLI 没有 resume 或审批参数，用户在 CLI 里无法解锁这个线程，而 interrupt 本身也被静默吞掉（exit 0，无输出）。服务端已有的 delete/fork/clone/export/import/models/settings 路由（http.rs）在 CLI 中也没有对应子命令。

建议修法：改用 bytes_stream 增量解析 SSE，边收边打印；去掉总超时，改为 idle timeout。识别 RUN_FINISHED interrupt：打印待批工具并提示，或者交互式询问 y/N 后带 `resume:[{interruptId,status}]` 重发。补 `cloud delete/fork/export` 等薄封装。

### F180 [P1] REPL 与 TUI 的斜杠命令各写一份（会话互操作、/model、/thinking、/plan、/rewind、/goto、/compact、/settings、/reload），约 400 行重复且已分叉
类型：simplify，工作量：L，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：handle_session_slash（main.rs:1599-1805）对应 dispatch_builtin（app.rs:395-675）：/session、/new、/export、/import、/fork、/clone、/name 逻辑相同，文案已不一致，比如 `[forked x] N nodes` 对比 `[forked x (N nodes)]`，REPL /import 打印完整 id。main.rs:1599 的注释写着“REPL/TUI 共用”，但 TUI 根本没用它。main.rs:2407-2560 对应 app.rs:457-560（plan/model/thinking/rewind/goto/compact）；handle_settings_repl（main.rs:1355-1390）对应 handle_settings_cmd（app.rs:2255-2303）；/reload 重载 settings（main.rs:2351-2365）对应 reload_theme_from_disk（app.rs:2233-2253）；resolve_session_arg（app.rs:293-318）重复了 rupi_memory::resolve_session_ref（session_io.rs:759-781）。

建议修法：在 rupi-tui::slash（rupi-cli 已依赖它）里抽一个纯函数 `run_builtin(&mut CmdCtx, text) -> CmdOutcome{Text, Adopt{id,tree}, Compact(prompt), OpenTree, OpenSessions, Quit, Pass}`；REPL 负责 println 和 async 收尾，TUI 负责 push_system 和 overlay。预计两端合计省 300 到 400 行，同时修掉文案分叉。

### F54 [P1] /new、/fork、/clone、/import、/export、/name、/session 在 REPL、TUI、AgentSession 各实现一遍
类型：simplify，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-agent/src/session.rs

证据：REPL 的 handle_session_slash 在 main.rs:1600-1805（约 205 行）；TUI 的 dispatch_builtin 在 app.rs:388-675（约 290 行）；AgentSession 在 session.rs:374-557（new_session、switch_session、fork_session、clone_session、set_session_name）。fork/clone 的建行 + remap_tree + persist_tree 逻辑在 main.rs:1735-1773、app.rs:623-655、session.rs:498-520 写了三遍。三份实现的行为已经不同：REPL 的 /import 出错会退出，TUI 只提示；provider 重建方式三种写法；RPC 的 new_session 不建库行，REPL 和 TUI 会建。

建议修法：在 rupi-agent（或 rupi-memory）提供 `SessionOps`：返回 `Outcome::{Adopt{id,tree,note}, Message(String), Error(String)}` 的纯函数集合，REPL、TUI、RPC 都只做渲染。AgentSession 的方法直接复用这些函数。预计减少约 200 行，并顺带修掉上述分叉。

### F53 [P1] 四个入口（run_once/run_chat/run_tui/run_rpc）的装配代码大段复制，约 300 行可以合并
类型：simplify，工作量：M，文件：crates/rupi-cli/src/main.rs

证据：重复的块有：信任门 main.rs:1813-1828 与 1919-1943；MCP spawn 在 1832-1840、1947-1955、2215-2223、2728-2736 各一份；memory_store、no_memory、frozen、external 在 1842-1849、1958-1965、2229-2236、2738-2745 各一份；policy、plan、parallel、discovery、thinking 在 1864-1874、1984-1999、2188-2210、2766-2785 各一份；subagents 在 2000-2012、2290-2302、2786-2798 各一份；reviewer 构造在 2018-2040、2257-2289、2800-2860 各一份，其中 TUI 闭包（2829-2857）逐行复制了 apply_suggestions（38-69）；TokenMeter 构造在 2093-2096、2250-2256、2888-2894 各一份；SkillsList/SkillLoad（918-960）手写了 discover_skills（846-861）的逻辑。这些复制已经造成行为分叉，例如 RPC 漏掉了 approver、subagents、review（见上一条）。

建议修法：抽一个 `struct Runtime { rt, tools, mcp, ext_set, mem, frozen, skills, sess_db, session, sid, provider, agent }`，由 `async fn assemble(cli, home, Frontend::{Run,Chat,Tui,Rpc}) -> Runtime` 统一构建；各入口只决定 approver 类型、review sink 和输出方式。reviewer 和 apply 合成一个 `fn review_sink(...)`。预计 main.rs 减少 250-300 行。

### F250 [P2] CLI 里有 4 份几乎一样的 AgentLoop 装配和 3 份 reviewer 回调；TUI/REPL/run 不走 AgentSession，follow-up 逻辑又实现了一遍
类型：simplify，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-agent/src/session.rs

证据：RPC（main.rs:1857-1874）、run（1978-2040）、chat（2187-2302）、TUI（2762-2860）四处重复 configure_agent、context_dirs、policy、plan、approver、parallel、discovery、thinking、subagents、reviewer 的装配。TUI 的 reviewer 闭包（2829-2856）把 apply_suggestions（38-69）的落盘逻辑又抄了一遍。TUI 直接调用 agent.run_with_user（app.rs:1594），自己用 take_follow_up 拼文本（1527-1543），会丢掉图片，与 AgentSession 的 merge_queued 不一致。session.rs:5 却写着“CLI --mode rpc 与 TUI 都走这一层”。

建议修法：抽 fn build_agent(cli, rt, home, load_project, approver_fallback, ctx) -> AgentLoop 和 fn make_reviewer(cli, provider, sink)，预计省约 150 行，同时修掉 rpc 丢 --approve 的问题。让 TUI 和 REPL 也持有 AgentSession，使用 prompt_queued，follow-up、streaming 和错误回滚只保留一份实现。

### F182 [P1] app.rs 结构：845 行的 run_loop、三个手动互斥的 overlay Option、两套重复的 Navigator、45 次 11 参数的测试调用
类型：simplify，工作量：L，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/tree_nav.rs, crates/rupi-tui/src/session_nav.rs

证据：run_loop 在 app.rs:709-1553；drive_turn 16 个参数（1563-1580）；draw 12 个参数，其中三个 overlay Option 互斥全靠调用方手动置 None（1088-1090、1198-1200、1288-1289）。TreeNavigator 和 SessionNavigator 的 query、filtering、clamp_selected、move_by、type_char、backspace 逐字相同（tree_nav.rs:192-226 对比 session_nav.rs:58-92），run_loop 里两段键处理（857-998）也几乎一样。测试里 dispatch_builtin( 共出现 45 次，每次 11 个参数（2940-3790），约 800 行样板。

建议修法：拆成 app/state.rs（App{view,input,chrome,overlay: Overlay::{None,Tree,Sessions,Editor,Approve}}）、input.rs（键路由纯函数，可单测）、commands.rs（见上一条）、render.rs（draw、tree_lines、session_lines、editor_lines、footer）、clipboard.rs。Navigator 抽成泛型 `FilterList<T: Searchable>`，约省 60 行。测试加一个 `Harness::dispatch(text)` 辅助函数，约省 500 行。drive_turn 改为接收 `&TurnDeps` 结构体。

### F181 [P2] “采用新会话”逻辑三处拷贝，行为不一致
类型：simplify，工作量：S，文件：crates/rupi-tui/src/app.rs

证据：选择器回车（app.rs:881-910）、Builtin::Resume（1342-1381）、Builtin::Adopt（1382-1395）都做“替换树、写 sid cell、export_session_id、重建 provider”。差异：Resume 不检查 model 为空，失败会提示；另外两处静默。三处都不更新 chrome.footer.model，也不刷新视图和 meter。

建议修法：抽出 `fn adopt_session(ctx, chrome, view, id, tree, note)`，同时负责视图回放和 footer 更新，约省 50 行。

### F189 [P1] 输入编辑能力缺失：没有 ↑↓ 历史、Home/End/Delete、按词移动和删除、/help；Ctrl+C 会直接退出（运行中也是）
类型：missing-feature，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/complete.rs, crates/rupi-tui/src/view.rs

证据：主 match（app.rs:1008-1132）只处理 Left、Right、Backspace、Char、Enter、Tab、PageUp/PageDown，Up、Down、Home、End、Delete 都落进 `_ => {}`；InputBuffer（view.rs:31-105）没有 delete、home、end、word 操作。complete::BUILTINS（complete.rs:12-16）里没有 help，输入 /help 会发给模型，命令说明只出现在可以被 quietStartup 关掉的启动 banner（app.rs:684、693-695）。空闲时 Ctrl+C 直接 break（999-1001）；运行中 Ctrl+C 返回 End::Quit（1680-1684、1739），会退出整个 TUI，本轮也不落盘。

建议修法：InputBuffer 增加历史环（↑↓，可以从 sessions.db 的 user 行初始化）以及 Home/End/Delete/Ctrl+A/E/W/U 和 Alt+←→；增加 /help（列出 BUILTINS、自定义命令和 KeyTable）；Ctrl+C 改为“有输入则清空，运行中则中止，双击才退出”。

### F190 [P1] 关键交互路径没有测试，现有的部分测试测的是死代码
类型：test-gap，工作量：M，文件：crates/rupi-tui/src/app.rs

证据：没有测试覆盖：视口锚定到底部（上面的探针已证明有 bug）、run_loop 键路由（改绑 send、树导航按键穿透、Ctrl/Alt 字符）、meter 累加（探针 0/10）、会话切换后视图是否回放、导航器视口、输入框多行和 CJK 光标、TuiApprover、扩展 UI 请求。drive_turn 只有一个流式重绘测试（2858）。status_footer_shows_token_cost_line（2480）、sessions_block_lists_and_marks_current（3572），以及 read_only_views 里的 /copy /hotkeys /trust 断言（3414-3455）测的都是 run_loop 永远到不了的代码。

建议修法：把键处理抽成 `fn on_key(&mut App, KeyEvent) -> Effect` 纯函数后补表驱动测试；draw 补 TestBackend 回归测试（底部可见、CJK 光标、多行、导航器选中可见）；drive_turn 补 Usage 到 meter 的测试；删掉测死代码的用例。

### F169 [P2] /tree 打开时未处理的按键会穿透到隐藏的输入框和全局快捷键
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs

证据：会话选择器分支在 match 后有 `continue`（app.rs:928），树导航分支（930-998）在 `_ => {}` 后没有 continue，直接落到 999 之后的主键处理。非过滤状态下按 'x' 会被 push 进 input（1123），Backspace 会删 input，Ctrl+L 会切模型，PageUp 会滚动背景。关闭 /tree 后输入框里会出现残留字符。

建议修法：树分支 match 后加 `continue;`；把 overlay 键处理统一成 `Overlay::handle_key -> Handled|PassThrough`。

### F186 [P2] 状态栏和模型显示细节：显示 provider 名而不是模型、/model 后不刷新、ModelChange 只有 Ctrl+L 发、提示写死不跟随键位、每次 LLM 调用都往对话里插一行 usage
类型：incomplete，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/view.rs

证据：chrome.footer.model = provider.name()（app.rs:746、1043、1071、1498），显示的是 "openai-compat" 而不是模型 id；/model 切换（514-533）之后 footer 要到下次发送才更新（1498）。ModelChange 只有 Ctrl+L 发给扩展（1044-1049），Ctrl+P 和 /model 不发，REPL 的 /model 会发（main.rs:2426）。footer 提示 "Ctrl+O/T 折叠 · Ctrl+E /edit" 写死（1901），不读 KeyTable。meter 用 `msg.contains("model switched")` 字符串匹配来判断（1332）。view.rs:215-224 每次 Usage 都 push 一行 `· usage in=..` 到对话里，工具循环中会刷屏。

建议修法：footer 显示 `provider/model_id[:thinking]`；dispatch 返回结构化的 ModelSwitched，由调用方统一更新 footer、meter 并 emit；提示从 KeyTable 生成；Usage 只更新 footer，不进对话。

### F185 [P2] 会话选择器信息不足：不显示时间、cwd、首条消息，也不按当前项目排序
类型：incomplete，工作量：M，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/session_nav.rs

证据：session_lines（app.rs:2437-2442）只显示 profile、8 位 id、消息数和 name；SessionRow.created（session_nav.rs:9）读出来了但从未展示。from_store 取的是全库最近 50 条（list_sessions 不按 cwd 过滤），而 CLI 的 latest_session 是按 cwd 过滤的。

建议修法：每行显示 updated_at 相对时间、cwd 尾段和首条 user 预览（SessionStore 加一个 list_sessions_ex）；当前 cwd 的会话排在前面，Tab 切换显示全部。

### F178 [P2] !cmd、/share（gh gist）、剪贴板在 async 主循环里同步阻塞，期间 UI 冻结且无法中止
类型：perf，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-tui/src/share.rs

证据：run_bang_cmd 用 `Command::output()`（app.rs:1937-1960），由 1153-1176 同步调用，没有超时，也没有进度显示；`!sleep 30` 会让 TUI 冻结 30 秒。/share 在 dispatch_builtin 里（561-568）调 share::try_gh_gist 的 `Command::new("gh")...output()`（share.rs:75-85），会同步等待网络。运行时是 current_thread（main.rs:871），整个进程都被卡住。

建议修法：改用 tokio::process 加 select!（Esc 取消、默认超时），运行期间显示 `◌ $ cmd …`；/share 放进 spawn_blocking。

### F177 [P2] 流式期间每个 delta 都重新解析整条助手消息（Markdown、高亮、Mermaid），并对整条做哈希
类型：perf，工作量：M，文件：crates/rupi-tui/src/view.rs

证据：visual_lines（view.rs:315-326）每帧对最后一行求 line_fp（396-425，哈希整串；Image 行会哈希整个 base64），指纹变了就 rebuild_from(last)，对整条 AssistantText 重跑 render_markdown_rows。每帧还会在 paint_busy 里 to_vec（见上一条）。长回答的总开销是 O(n²)。

建议修法：给最后一条维护 dirty 标志或长度，代替哈希（push_event 时置脏）；Markdown 按“已闭合段落”增量缓存，只重渲未闭合的尾段。

### F187 [P2] 自定义主题的 id 只哈希部分颜色，改了 tool、system、syntax 等颜色后 /reload 不会重绘已有对话
类型：bug，工作量：S，文件：crates/rupi-tui/src/theme.rs, crates/rupi-tui/src/view.rs

证据：theme_id（theme.rs:447-467）只哈希 name、user、accent、assistant、heading；ChatView 的缓存 key 来自 theme.id（view.rs:133-143）。同名主题只改 tool 或 syntax_* 后 /reload，id 不变，缓存不重建，已渲染的行保持旧颜色。另外 source_of（254-268）有两个相同分支（clippy if_same_then_else）。

建议修法：对 Theme 的全部颜色字段做哈希（或者每次 resolve 递增一个 generation）；合并 source_of 的分支。

### F188 [P2] 零散未用的 pub API 与字段
类型：simplify，工作量：S，文件：crates/rupi-tui/src/theme.rs, crates/rupi-tui/src/markdown.rs, crates/rupi-tui/src/editor.rs, crates/rupi-tui/src/tree_nav.rs, crates/rupi-tui/src/session_nav.rs, crates/rupi-tui/src/app.rs

证据：全仓 grep 的结果：Theme::resolve（theme.rs:108-113）无调用方；ThemeCatalog::resolve（233-237）、markdown::render_markdown/flatten_rows（markdown.rs:15-17、89-102）、TreeNavigator::from_session（tree_nav.rs:59-61）、FileEditor::is_dirty/is_created（editor.rs:124-130）只有测试在用；FileEditor::cursor/line_count（136-142）完全没人用；theme_system_fallback（markdown.rs:104-106）写死 dark 主题，忽略当前主题；SessionRow.created 从不读取。clippy 还报了：TuiApprover 的 Default 可以 derive（152）、Builtin 变体大小差异大（248，Adopt 带整棵 SessionTree）、`.last()` 用在 DoubleEndedIterator 上（1746，应改 next_back）。

建议修法：删掉，或改成 #[cfg(test)]；Adopt 里的 tree 改成 Box<SessionTree>；`.last()` 改为 `.rev().find(..)`。

### F191 [P2] README、UPSTREAM 和 crate README 对 TUI 的描述与代码不符
类型：doc-drift，工作量：S，文件：README.md, docs/UPSTREAM.md, crates/rupi-tui/README.md, crates/rupi-cli/src/main.rs

证据：README.md:205 和 UPSTREAM.md:59 写“状态栏：↑↓ tokens / context% / $”，TUI 的 footer_text（app.rs:1869-1903）既没有 context% 也没有 $。rupi-tui/README.md:5 和 UPSTREAM.md:102 写“Shift+Enter 换行”“Ctrl+V 贴图”“增量渲染”：Shift+Enter 在传统终端不生效，Ctrl+V 在 macOS 不可用，每帧还会完整克隆渲染缓存。main.rs:1599 注释写“REPL/TUI 共用的会话互操作斜杠”，但 TUI 有自己的一份。app.rs:59 写“/reload 与 REPL 同语义”，但 REPL 是每轮自动重载。

建议修法：随上面各条修复一起更新文档；修复之前先把这些描述改成实际行为（Alt+Enter/Ctrl+J 换行、各平台剪贴板依赖、footer 字段）。

### F61 [P1] 二进制级测试没有覆盖 JSON-RPC 扩展、RPC 增量落盘、JSONL 纯净度和 provider 重建
类型：test-gap，工作量：M，文件：crates/rupi-cli/tests/smoke.rs, crates/rupi-cli/tests/cloud_thin.rs, crates/rupi-ext/src/lib.rs

证据：smoke.rs 里 grep `jsonrpc|echo-rpc` 无结果，ext_list 用例（984 行）只覆盖 oneshot，所以 current_thread 下的 panic 没被发现（rupi-ext 单测用的是 multi_thread，lib.rs:703）。RPC 用例（56-276）都不带 --continue，也不在 clone、new_session 后检查 `rupi sessions`；有的断言只用 `out.contains(...)` 判断，stdout 混入 WARN 也能通过，没有逐行 JSON 解析。没有测试检查 provider 重建后的路由。cloud_thin.rs:50-57 在连不上 Postgres 时直接 return，本机无数据库时显示通过但什么都没测，CI 有 services，不受影响。

建议修法：增加以下用例：(1) 加载 examples/extensions/echo-rpc.json 跑 chat、run、rpc 并断言 exit 0；(2) RPC 依次执行 --continue + prompt、clone + prompt、new_session + prompt，每步之后 `rupi sessions` 的消息数都应递增；(3) `run --json` 和 rpc 的 stdout 每一行都能被 serde_json 解析（同时放一个非法 settings.json 触发 warn）；(4) cloud_thin 在跳过时打印 `SKIP` 到 stderr，或标记为 #[ignore]。

### F158 [P1] 本次 e2e 发现的几处崩溃和丢数据都没有测试覆盖
类型：test-gap，工作量：M，文件：crates/rupi-cli/tests/smoke.rs, crates/rupi-ext/src/lib.rs, crates/rupi-memory/src/lib.rs

证据：crates/rupi-cli/tests 下只有 smoke.rs 和 cloud_thin.rs。没有测试用真实二进制（current_thread 运行时）加载 jsonrpc 扩展，rupi-ext 的单测只用 multi_thread(lib.rs:703)。没有测试覆盖 `--mode rpc --session <已有会话>` 的回合落盘，smoke.rs:1339 只测了 run 加 --session jsonl。没有测试检查 --json 和 rpc 的 stdout 每一行都是 JSON。memory 的 replace（不带 `|||`）和 remove 的语义没有单测，grep `"replace"` 只命中实现代码。也没有测试覆盖 chat 在管道 stdin 下的信任和审批提问。

建议修法：在 smoke.rs 补以下用例（都可以用 mock provider，不需要真实 key）：(1) `-e examples/extensions/echo-rpc.json run hi` 退出码为 0；(2) rpc 先建会话，再 --session 续接并 prompt，检查 sessions 里的 msgs 数增加；(3) 触发压实的 --json，每一行都能 serde_json 解析；(4) 带 .rupi 项目时用管道驱动 chat，第一行能执行。rupi-memory 补 replace/remove 的单测。

## 补充分配的问题

### F40 [P1] 缺 key 时静默降级为 mock：-p/run/rpc 退出码 0 并输出假回复，REPL /model 显示“switched”
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs

证据：build_provider（main.rs:729-781）在 provider_from_spec 失败时一律返回 `Box::new(MockProvider...)`，从不返回 Err，所以 REPL `/model` 的 `Err(e) => eprintln!("[model] switch failed...staying on {model}")`（main.rs:2432）是死分支。实测：`rupi -p --no-session hi`（无 key）stdout 输出“demo mode：设置 RUPI_API_KEY 后可接真实模型…”，exit=0。REPL 中在 OPENAI_API_KEY=<redacted> 的情况下执行 `/model anthropic/claude-sonnet-4-5`（无 Anthropic key），提示 `[model switched to anthropic/claude-sonnet-4-5]`，原本可用的 OpenAI 会话被换成了 mock。上游 Pi 在缺凭证时直接报错退出。

建议修法：build_provider 返回 Result。只有交互入口首次启动、且没有任何 key 时才进 demo mode，并给出醒目提示。run/-p/--json/rpc 缺 key 时以非零退出。/model 切换失败时保留原 provider。

### F50 [P1] `--approve` 与上游同名但语义相反；上游的 -a/-na、-v、--models、--prompt-template、--theme 等 flag 缺失
类型：incomplete，工作量：M，文件：crates/rupi-cli/src/main.rs

证据：上游 https://github.com/earendil-works/pi/blob/v0.85.1/packages/coding-agent/docs/usage.md:248-249 中 `-a/--approve` 的含义是“Trust project-local files for this run”；rupi 的 `--approve`（main.rs:200-202、1421-1424）却是“Ask 裁决自动放行”，即自动批准危险 bash。照搬 Pi 脚本的用户会意外打开危险工具的自动放行。rupi 用 `--trust-project` 表示上游 -a 的语义。实测 `-a` 和 `-v` 都报 `unexpected argument`。usage.md 中的 --models、--prompt-template、--no-prompt-templates、--theme、--no-themes、--use-theme、--tui-mode、--verbose、`--list-models [search]` 都没有实现：`--list-models sonnet` 与不带参数的输出相同（都是 16 行）。ThinkingLevel 把 minimal 映射为 Low（rupi-llm lib.rs:64）。上游的 `pi remove`、`pi list`、`pi update` 别名也没有（rupi 只有 uninstall、packages）。

建议修法：把 rupi 的自动放行改名为 `--yolo` 或 `--auto-approve`（保留旧名并给出弃用告警），`-a/--approve`、`-na/--no-approve` 对齐上游的项目信任语义。补 `-v`、`--list-models <search>` 过滤、`--models`（覆盖 enabledModels）、`--prompt-template`、`--theme`、`--use-theme`。增加 remove、list 子命令别名。

### F51 [P1] MCP 只能每次传 --mcp-config，没有默认的 mcp.json 发现，也没有 settings 键或 mcp 子命令
类型：missing-feature，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-config/src/lib.rs

证据：全仓 grep `mcp.json|mcpServers` 无结果。MCP 只通过 main.rs:97 的 `--mcp-config <PATH>` 进入四个入口（1832、1947、2215、2728）。MCP 是 rupi 相对 Pi 0.85 的卖点之一（README.md:163），但用户每次启动都要手动带这个参数，配置也不能放进项目。上游最新版 https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/cli.md 已有 `~/.pi/agent/mcp.json`、`.pi/mcp.json`（需项目信任）和 `pi mcp add/remove/list`。rupi 只有 `mcp-list <command>` 用于单个 server 探活。server 启动失败时只打一条 tracing::warn（rupi-mcp lib.rs:1381），而这条日志目前写到 stdout。

建议修法：默认读取 `$RUPI_HOME/mcp.json` 和受信任项目下的 `.rupi/mcp.json`（同时兼容 Pi 路径），--mcp-config 作为覆盖。增加 `rupi mcp add/remove/list` 来读写这个文件。

### F55 [P2] 死代码：TUI dispatch_builtin 中被前置拦截的分支、未使用的函数、Resolved.theme
类型：simplify，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-cli/src/main.rs

证据：app.rs:352-360 的 `/copy`、`/hotkeys`、`/trust` 返回 `"[copy] no assistant text"` 和 `"[trust]"` 占位文本，但运行时已经在 app.rs:1177 的 parse_local_slash 处被拦截；app.rs:374-384 的 `/commands`（不带 filter）也被 1212-1228 先拦截。这些分支只有单测能走到，单测测的是占位行为而不是真实行为。rustc 报告 sessions_block（app.rs:266-289）和 status_footer（1846-1867）未使用，只在测试中被调用。main.rs:1221-1222 的 `#[allow(dead_code)] theme` 字段在 Resolved::load 里计算后从未被读取。config_cmd.rs:138 和 141 对同一个 settings.extensions 两次构造 ResourceFilter。

建议修法：删除 dispatch_builtin 里被拦截的 4 个分支及对应的单测断言，改为测试 parse_local_slash 的路径；删除 sessions_block、status_footer 和它们的测试，或者把 /sessions 的非 TTY 回退接到 sessions_block 上；删除 Resolved.theme；config_cmd 复用同一个 filter。约减少 80 行。

### F62 [P2] UPSTREAM.md 的“已知未做”有过期项，RPC/CLI 对照表把部分对齐写成了完全对齐
类型：doc-drift，工作量：S，文件：docs/UPSTREAM.md, README.md

证据：docs/UPSTREAM.md:116 写“已知未做：子 agent 继承 policy/approver”，但 SubagentTool::inherit_from（rupi-agent subagent.rs:171-177）已经实现，main.rs:2009、2299、2795 也在调用。UPSTREAM.md 的“RPC 扩面”一行看起来像已完全对齐，没有列出上述 7 个缺失命令、prompt 不展开模板和 EOF 才落盘的问题。README.md:162 声称 `--mode json` ✓。UPSTREAM 写“只读合并 Pi settings”，没有说明 defaultModel 等键不生效。

建议修法：删掉过期的 subagent 条目；在 RPC 行和 settings 行写明当前缺口；README 表格中把 `--mode json` 改为 `run --json`，或者先实现 --mode json。

### F76 [P1] MCP 没有自动发现配置（mcp.json 或 settings 键），没有 /mcp 管理命令；rpc/run 不处理 list_changed；云端完全没有 MCP
类型：missing-feature，工作量：M，文件：crates/rupi-cli/src/main.rs, crates/rupi-mcp/src/lib.rs, crates/rupi-server/src

证据：只有 `--mcp-config` flag（main.rs:95-97），全仓 grep 不到 `mcp.json` 或 `mcpServers`，每次启动都得传 flag；用户已有的 `~/.pi/agent/mcp.json` 不会被读取。main.rs:1834 和 1949（rpc/run）用 `spawn_all` 而不是 `spawn_all_watched`，长期运行的 rpc 会话收不到工具变更。TUI/REPL 都没有 `/mcp` 斜杠命令可以查看状态或重连（grep 不到）。在 crates/rupi-server、rupi-runtime 中 grep `mcp` 无结果，云端 run 无法挂 MCP。MCP 的 bootstrap 代码块在 main.rs 中复制了 4 次。

建议修法：抽出 `load_mcp(cli, home, settings, load_project)`：依次读 `--mcp-config`、`~/.rupi/mcp.json`、受信任时的项目 `.rupi/mcp.json`，以及只读的 `~/.pi/agent/mcp.json`。rpc 也用 watched，并在每次 prompt 前排空队列。加 `/mcp` 命令（列出 server/工具/状态、reconnect）。云端可以后续单独立项。

### F80 [P1] 设置 sessionDir 或 --session-dir 后，模型的 session_search 工具仍查 RUPI_HOME/sessions.db，查不到任何会话
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-cli/src/main.rs

证据：main.rs:411-421 的 `memory_store` 用 `MemoryStore::new(home)`（RUPI_HOME）。lib.rs:195-204 的 `with_sessions` 打开 `SessionStore::open(&self.home)`，session_search 工具（lib.rs:880-881）就查这个库。会话实际写入 `session_store_home`（main.rs:553-557，会解析 sessionDir）。实测：设 `sessionDir` 后跑一轮，消息只出现在 `sd/sessions/sessions.db`，`RUPI_HOME/sessions.db` 根本不存在。CLI 的 `session-search` 子命令用的是正确路径，能查到；模型工具查的库里没有消息。

建议修法：MemoryStore 增加 `sessions_home` 字段（`with_sessions_home(resolve_session_dir(..))`），session_search 和记忆镜像都用会话库目录；或者让记忆镜像单独用 `memories.db`。

### F87 [P1] settings.packages 运行时没有任何下游读取：手写 `"packages":["!npm:foo"]` 不会禁用该包的资源
类型：incomplete，工作量：M，文件：crates/rupi-config/src/lib.rs, crates/rupi-cli/src/config_cmd.rs, crates/rupi-cli/src/main.rs

证据：全仓 grep `settings.packages` 只有 3 处：config_cmd.rs:119（`rupi config list` 显示 on/off）、main.rs:2354 和 app.rs:2246（/reload 时复制字段）。skill、扩展、prompt、主题的发现都不检查 packages。禁用包之所以有效，只是因为 `toggle_package`（config_cmd.rs:46-69）顺带往 skills/prompts/extensions/themes 写了 `!name`。语义也与上游不同：上游的 packages 是要安装的包源列表，不是开关列表。

建议修法：二选一：（a）运行时根据 packages.json 记录把被 deny 的包物化出的资源名并入各类 filter；（b）明确 packages 只是 `rupi config` 的 UI 状态，并在文档里说明。推荐（a），同时兼容上游的对象形式。

### F141 [P1] 没有 API key 时静默走 mock：-p/run --json/rpc 都 exit 0，并报 stop_reason=done
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, README.md

证据：main.rs:746-776 构造 provider 失败时一律退回 MockProvider 加一段固定回复。实测（unset 所有 key）：`rupi -p hello` → stderr `[rupi] no RUPI_API_KEY/OPENAI_API_KEY — using mock provider (demo mode)`，stdout `demo mode：设置 RUPI_API_KEY 后可接真实模型。...`，exit=0。`rupi run --json hello` 末行 `{"session_id":...,"stop_reason":"done","text":"demo mode：...","type":"run_result"}`，exit=0。显式写 `--model openai/gpt-4o` 或 `anthropic/claude-sonnet-4-5` 也一样走 mock。README:59 有说明，但脚本和 CI 无法区分成功和没配 key。

建议修法：非交互的 run、-p、--json、rpc 缺 key 时直接报错并以非零码退出，mock 只在显式 `--provider mock` 或 `RUPI_DEMO=1` 时启用。如果保留 mock，run_result 和 rpc get_state 里加上 `"mock": true`。

### F142 [P1] 不支持 socks5/socks5h 代理：常见的 `HTTPS_PROXY=socks5h://...` 环境下所有请求失败
类型：bug，工作量：S，文件：Cargo.toml

证据：Cargo.toml:39 的 reqwest features 是 json/stream/http2/charset/rustls-tls-webpki-roots，没有 `socks`。在本机默认环境（HTTPS_PROXY 和 ALL_PROXY 都是 socks5h://127.0.0.1:7890）下执行 `rupi --model deepseek-v4-flash -p "Reply with exactly the word PONG"` → `Error: error sending request for url (https://本地 .env 配置的网关/v1/chat/completions) ... 2: unsupported scheme socks5h`，exit 1。同一环境下 curl 2 秒内返回 200。这次失败还在库里留下一条 0 msgs 的空会话（c3dbe980）。unset 代理变量后一切正常。同一个 reqwest 配置也被 rupi-mcp、rupi-pkg 和 cloud 子命令共用。

建议修法：workspace 的 reqwest 加上 `socks` feature。或者在错误链里识别 `unsupported scheme` 并提示用户检查哪个代理变量。

### F147 [P1] RPC get_messages 返回的 id 与 get_tree 的节点 id 不是一套，拿 get_messages 的 id 去 fork 会失败
类型：bug，工作量：S，文件：crates/rupi-agent/src/session.rs, crates/rupi-cli/src/rpc.rs

证据：get_messages(rpc.rs:373-379 → session.rs:370-372) 返回的是 `Message` 自带的 id，get_tree(session.rs:522-541) 返回的是树节点 id，fork_session(session.rs:470-475) 只认节点 id。同一会话实测：get_messages 的 ids 是 `['541f276e','a945ade2']`，get_tree 的 ids 是 `['ffaa47a1','fc05aeb7','b2c3a487']`。`{"type":"fork","entryId":"541f276e-cf2d-..."}` → `{"error":"unknown entryId: 541f276e-...","success":false}`，用 get_tree 的 ffaa47a1 则成功。另外 get_messages 漏掉了 bash 命令注入的那条节点（2 条对 3 条）。

建议修法：get_messages 输出 `{entryId: 节点 id, message}`，按 current_path 遍历节点而不是 history() 里的 Message。或者在 fork 时把 message id 映射回节点 id。

### F155 [P2] LLM HTTP client 没有任何超时，连接或流卡住时会无限等待
类型：perf，工作量：S，文件：crates/rupi-llm/src/lib.rs

证据：shared_http_client(lib.rs:452-461) 是 `reqwest::Client::builder().build()`，没有 connect_timeout，也没有 read_timeout。实测同一条 `rupi -p "Reply: T1"` 连跑三次，耗时 186s、2s、2s。更早一次 70s，而 curl 只要 2s。慢的原因可能在网关那边，没有确认，但客户端没有任何超时，也就没有恢复手段。run 模式下 Ctrl-C 会直接杀进程(main.rs:2116)。

建议修法：设置 connect_timeout（约 15s）。流式响应按 chunk 设空闲超时（例如 60s 没收到新数据就报错，进入现有的重试逻辑），并允许 settings 或环境变量覆盖。

### F156 [P2] 非交互默认跳过项目信任后，memory scope=project 报「no .git above cwd」，误导用户
类型：bug，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-cli/src/main.rs

证据：memory_store(main.rs:412-422) 在 load_project=false 时不设置 project。apply_write_scoped(lib.rs:376-379) 统一报 `no project detected (no .git above cwd)`。实测：在有 .git 的临时项目里执行 `rupi --review-apply run "Remember: this project always uses tabs..."` → `[memory error]`，模型转述 `the project-scoped write failed (no .git above the cwd)`，实际原因是 stderr 里的 `[trust] 非交互默认跳过项目资源`。

建议修法：MemoryStore 区分「没找到项目」和「项目不受信任」两种情况，后者提示使用 --trust-project 或 /trust。

### F240 [P1] auto-retry 是空壳：AutoRetryStart/End 从不发射，set_auto_retry/abort_retry 对行为没有影响
类型：incomplete，工作量：M，文件：crates/rupi-agent/src/session.rs, crates/rupi-cli/src/rpc.rs, crates/rupi-llm/src/lib.rs, crates/rupi-core/src/lib.rs

证据：auto_retry_enabled 只在 state() 和 set_auto_retry 里读写（session.rs:192-194, 284, 288-294）。RPC set_auto_retry 只改这个标志（rpc.rs:447-450, 840-846），abort_retry 只回 ok（451-453, 847-849）。真正的重试在 post_json_with_retry 里，固定 3 次，不发事件，也不读这个开关（rupi-llm lib.rs:907-935）。grep 显示 AutoRetryStart/AutoRetryEnd 只在 core 定义、TUI 渲染和 ext 标签里出现，没有任何构造点。

建议修法：把重试从 HTTP 层上提到 AgentLoop（或通过 provider 回调）：可重试的错误触发 AutoRetryStart/End 事件，并读取 AgentSession 的开关；abort_retry 取消当前退避等待（与 CancelFlag 联动）。如果不做，就删掉开关和事件，RPC 回 unsupported。

### F241 [P1] ToolHook（beforeToolCall/afterToolCall）没有任何生产接线入口，扩展的 tool_call 订阅只是通知，不能拦截或改写
类型：incomplete，工作量：M，文件：crates/rupi-agent/src/hooks.rs, crates/rupi-agent/src/lib.rs, crates/rupi-ext/src/rpc.rs, docs/UPSTREAM.md

证据：grep 全仓：with_hook、DenyToolsHook、RedirectCommandHook、RecordingHook 只在 rupi-agent 的单测里使用，CLI、settings、扩展、server 都不装 hook。RpcHost::on_event 用 rpc.notify 发通知，结果直接丢弃（rupi-ext rpc.rs:549-563），扩展无法 block 或修改参数。UPSTREAM.md:15 却把 beforeToolCall/afterToolCall 列为已对齐。

建议修法：做一个 ExtHook：对订阅了 tool_call 的 jsonrpc 扩展，用 call_timeout("hooks/before_tool_call") 拿到 {block?, args?}，after 同理，在 CLI 装配时对每个 rpc host 调 with_hook。settings 增加 hooks 配置（deny 列表和 redirect 规则）。如果暂时不做，UPSTREAM 改成“仅 SDK API”。

### F248 [P1] Approver 是同步 trait，在 async 主循环里阻塞读 stdin：审批期间 Ctrl-C/Esc 无效；压实的摘要调用也不响应取消
类型：perf，工作量：M，文件：crates/rupi-agent/src/policy.rs, crates/rupi-agent/src/lib.rs, crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：Approver::approve 是同步函数（policy.rs:22-24），在 run_turns 的 future 里直接调用（lib.rs:949）。TerminalApprover 和 TuiApprover 在里面 std::io::stdin().read_line（main.rs:1186-1195；app.rs:160-190），这会阻塞 tokio worker。REPL 的 select! 里 ctrl_c 分支和 run future 在同一个任务上，审批阻塞期间轮询不到，CancelFlag 的文档也承认“审批问询不受影响”（core lib.rs:666-668）。压实的 provider.complete(req).await（lib.rs:1289）不和 cancel 竞速，在 run 开头（649）和轮中（1102）执行时，长摘要期间 Esc 没有反应。

建议修法：把 Approver 改成 async（async_trait），或者在 spawn_blocking 里调用并与 cancel.cancelled() 一起 select；取消时视为拒绝并走中止收尾。compress_inner 的摘要调用同样和 cancel 竞速，取消时跳过压实。

### F254 [P2] 思考内容只在整轮结束后一次性发出，工具调用参数不流式，长输出期间 UI 没有进度
类型：missing-feature，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-llm/src/lib.rs

证据：StreamEvent 只有 TextDelta 和 Usage 两种（rupi-llm lib.rs:158-166）。Thinking 事件在拿到完整响应之后才由 emit_thinking 发出（lib.rs:818, 312-320）。工具调用参数（例如 write 的大段内容）在生成过程中完全没有事件，TUI 和 AG-UI 只能看到转圈。

建议修法：StreamEvent 增加 ThinkingDelta 和 ToolCallDelta{id,name,args_delta}，三家 provider 的 SSE 累积器透出这两类事件，AgentEvent 对应增加 ThinkingDelta 和 ToolCallStreaming，TUI 渲染“正在写 path…（N 字节）”。

### F256 [P2] 文档与代码多处不符：子 agent 继承项、run_subagents、hook、usage 校准、resume 预热、core 不依赖 IO、Tool trait 所在 crate
类型：doc-drift，工作量：S，文件：docs/UPSTREAM.md, crates/rupi-agent/README.md, crates/rupi-core/README.md, crates/rupi-agent/src/session.rs, crates/rupi-agent/src/tokens.rs

证据：UPSTREAM.md:116 把“子 agent 继承 policy/approver”列为已知未做，但实际已经由 SubagentTool::inherit_from 实现（subagent.rs:171-177），而真正缺的工具过滤和 hook 继承没有写。agent README 说“`subagent`：`run_subagents` 并发扇出……`--subagents` 开启”，但 --subagents 装的是 SubagentTool，run_subagents 只有测试在用。UPSTREAM.md:15 把 hook 写成已对齐，生产里没有接线。UPSTREAM.md:24 和 tokens.rs:2 写的“provider usage 校准比例”，实际 ratio 恒为 1。UPSTREAM.md:27 写的“resume 时预热窗口”，实际 summary_through 被重置。session.rs:5 写的“CLI --mode rpc 与 TUI 都走这一层”，TUI 并不走。core README 写“不依赖 LLM 或 IO”和“ToolDefinition + Tool trait”，但 trust.rs/commands.rs 读写 stdin 和文件，Tool trait 在 rupi-tools/src/lib.rs:75。

建议修法：随上面几项修复同步修改文档；在这些功能修好之前，先把 UPSTREAM 的“已知未做”列表更新为本报告里实测确认的缺口。

### F257 [P1] 多轮和异常路径缺测试：压实配对与重复压实、rewind 与摘要、SDK 出错复位、RPC 落盘、云端多调用 interrupt、hook 短路
类型：test-gap，工作量：M，文件：crates/rupi-agent/src/lib.rs, crates/rupi-agent/src/session.rs, crates/rupi-cli/src/rpc.rs, crates/rupi-server/src/run.rs

证据：现有测试几乎都是单轮或单步的。没有测试覆盖：压实后保留窗口不以 Tool 开头（P10）；长会话中摘要输入有上界（P12/P13）；rewind 到覆盖点之前后摘要被清掉（P14）；AgentSession 出错后 is_streaming 为 false（P3）；RPC 在 --resume 或 new_session 后能落盘（实测丢数据）；on_ask Interrupt 同轮两个调用（P1，现有 on_ask_interrupt_stops_before_tool_execution 只测单个调用）；hook 拒绝后不问审批（P16）；provider 出错时事件序列有 RunEnd（P4）；rpc stdout 全是 JSON（实测有 INFO 行）。session.rs 只有 2 个测试，都走成功路径。

建议修法：把本报告的 P1、P3、P4、P10、P12、P14、P16 探针直接改写成 rupi-agent 单测（都只需要 MockProvider 或几行替身）。在 rupi-cli/tests/smoke.rs 里加 rpc 冒烟测试，覆盖 resume、new_session 和 stdout 纯 JSON。

### F97 [P2] MemoryProvider 的 on_session_end 和 shutdown 生命周期钩子没有任何调用方
类型：incomplete，工作量：S，文件：crates/rupi-memory/src/lib.rs, crates/rupi-cli/src/main.rs

证据：lib.rs:631、643 定义了 `shutdown` 和 `on_session_end`，MemoryManager 没有对应的 `*_all` 包装，全仓 grep 不到调用。prefetch、sync、pre_compress 都已接入（rupi-agent lib.rs:658、837、1267）。外部 provider（Honcho/Mem0 类）收不到会话结束信号，无法 flush。

建议修法：MemoryManager 增加 `session_end_all`、`shutdown_all`，在 REPL/TUI 的 /quit、/new 和 run 结束时调用，失败只 warning。

### F100 [P2] rupi-config 和信任逻辑重复，有死代码：merge 逐字段写 if、两套 on/off 和列表解析、未使用的 is_core_key/settings_specs/TrustPolicy
类型：simplify，工作量：M，文件：crates/rupi-config/src/lib.rs, crates/rupi-config/src/resources.rs, crates/rupi-core/src/trust.rs, crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs

证据：lib.rs:628-682 的 merge 是 17 个 `if over.x.is_some()`。lib.rs:446-452 的 parse_model_list 与 lib.rs:515-521 的 parse_tool_list 完全相同；lib.rs:454-466 的 eq_on/eq_off 与 resources.rs:196-208 的 token_on/token_off 相同；resources.rs:42-44 的 `as_str` 只是返回 key()。`is_core_key`（lib.rs:275）和 `settings_specs`（resources.rs:137）全仓无调用方。rupi-core trust.rs 的 `TrustPolicy`/`decide_load_project` 没有被使用，CLI 却在 main.rs:465-502、1812-1828、1918-1944 手写了 3 份信任判定。TUI 的 handle_settings_cmd（app.rs:2278-2291）复制了 apply_queue_settings。Resolved.theme（main.rs:1220-1221）带 `#[allow(dead_code)]`。Settings::load 每次启动被调 3 次以上（每次 session_store_home 调用都会重新 load）。

建议修法：merge 改用宏或 Value 深合并；删除重复的解析和开关函数；信任判定统一走 `decide_load_project`（interactive 参数决定是否询问）；Settings 加载一次后传递。预计减少约 120 行。

### F119 [P1] ChatResponse 仍无 usage 字段（overflow.rs 头注释属实）：后台调用不计量，按 usage 判溢出未实现
类型：incomplete，工作量：M，文件：crates/rupi-llm/src/lib.rs, crates/rupi-llm/src/overflow.rs, crates/rupi-agent/src/lib.rs

证据：ChatResponse 只有 message 和 stop_reason 两个字段（lib.rs:147-151），overflow.rs:5-6 所说"当前 ChatResponse 无 usage 字段"依旧成立。usage 只能经由 complete_streaming 的 StreamEvent 传出。压实摘要（rupi-agent/src/lib.rs:1289）和 LLM 复盘（review.rs:323）走的是 complete()，消耗的 token 进不了 TokenMeter、usage_by_node，也进不了云配额（rupi-server/src/run.rs:339-348）。trait 的默认 complete_streaming（lib.rs:250-264）从不推送 Usage。overflow.rs:5 提到的 z.ai/MiMo 静默溢出检测没有实现。

建议修法：ChatResponse 增加 `#[serde(default)] usage: Option<TokenUsage>`，在三家的解析器里填好；默认 complete_streaming 补发 Usage；后台调用计入会话和配额。之后再按 input 是否超过窗口实现静默溢出检测。

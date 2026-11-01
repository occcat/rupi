# Track: tools-ext-fixes

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/tools-ext-fixes`，worktree：`.worktree/tools-ext-fixes`

## 目标
内置工具输出不丢不重、bash 不被后台进程挂住、扩展与包装好就能跑、工具顺序确定。

## 负责文件
- crates/rupi-tools/**
- crates/rupi-ext/**
- crates/rupi-pkg/**
- crates/rupi-skills/**
- examples/**
- skills/**

不在本 track：rupi-cli/rupi-tui 的调用点归 local-frontends-fixes：需要调用点配合时，尽量让新行为在 crate 内部默认生效（例如 SkillRegistry::refresh 记住构造时的额外路径与过滤）。

## 特别说明
- 0/35 JSON-RPC 扩展 panic 的根因是 rupi-cli main 用 current_thread 运行时，local-frontends-fixes 会把它改成 multi_thread；你这边负责 rpc.rs 里 ExternalRpcTool 的取消（execute_with_cancel + notifications/cancelled）与 Arc 环泄漏，并保证在 multi_thread 运行时下端到端可用（crate 级测试）。
- 12/84/174：让 SkillRegistry 在 refresh 时保留构造时的额外 skill 路径、单文件 skill 与启停过滤，调用点不用改就能修好。

## 借鉴上游 Pi 的条目（来自对标 Pi v1.4.0 的分析，按 SaaS 价值排序）

### B9. 稳定缓存前缀：工具顺序确定、历史加缓存断点、cache key 和会话亲和 [critical/M]
本 track 范围：只做 ToolRegistry 定义输出按 name 稳定排序（新增工具不改变已有顺序前缀）

上游参考：packages/ai/src/api/anthropic-messages.ts:1526-1550（在最后一条 user 消息上打断点）；packages/ai/src/api/openai-prompt-cache.ts；pkg/durable/src/tool-search/tool-search.ts:14-15；ai.CHANGELOG.md 0.86.0 #9102 #9629、1.4.0 #10940；pkg/coding-agent/src/core/cache-stats.ts

差距：已核实。ToolRegistry 用 HashMap 存储，definitions() 按哈希顺序输出（crates/rupi-tools/src/lib.rs:91-94、117-121），而云端每个 run 都新建注册表（run.rs:310）。结果是不同 run 之间 tools 和 AvailableTools 的顺序随机，OpenAI 兼容网关（包括 deepseek）的前缀缓存基本命中不了。Anthropic 只在 system 和最后一个工具上打了断点（crates/rupi-llm/src/anthropic.rs:72-94），历史消息上没有；Bedrock 一个断点都没有。云端也从不设置 session_id 和 prompt_cache_key。

设计：阶段 1（S）：definitions() 按 name 排序，AgentLoop 合并工具后再整体排序一次；加单测，断言两次构建的序列化结果逐字节相同；之后新发现的工具一律追加到末尾。阶段 2（M）：ChatRequest 增加 session_id 和 cache_retention，云端填 hash(tenant_id, thread_id)。Anthropic 和 Bedrock 在最后一条消息上打断点，并保留上一轮末尾的断点，总数不超过 4 个。OpenAI 官方端点发 prompt_cache_key，兼容网关加 compat 开关。按 provider 导出缓存命中率指标，标签里不放 tenant。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F2 [P0] 包内扩展脚本不随包安装，manifest 的相对路径按进程 cwd 解析，装好的扩展跑不起来
类型：bug，工作量：M，文件：crates/rupi-pkg/src/install.rs, crates/rupi-pkg/src/layout.rs, crates/rupi-ext/src/lib.rs, crates/rupi-mcp/src/jsonrpc.rs, examples/extensions/echo-rpc.json

证据：materialize(install.rs:325-331) 只把 `*.json` 复制成 `~/.rupi/extensions/<name>.json`，同目录的 .py/.sh 不复制；push_ext_file(layout.rs:277-285) 只收 json。实测：pkgS/extensions/{echo.json,echo_rpc.py} 执行 `rupi install ./pkgS` 后，home9/extensions 下只有 echo.json。spawn_with_input(lib.rs:197-204) 和 StdioRpc::spawn(jsonrpc.rs:79-86) 都没设置 current_dir，也没有按 manifest 目录展开路径。实测：relext/rel.json 的 `args:["./script.sh"]` 在 cwd=/tmp 时返回 `exit status: 127: sh: ./script.sh: No such file or directory`。官方示例 examples/extensions/echo-rpc.json:12 的 `examples/extensions/echo_rpc.py` 只有 cwd 在仓库根时才能工作。

建议修法：ExtensionManifest 增加运行时字段 `base_dir`（load_one 时填 manifest 所在目录）；spawn 时设置 `current_dir(base_dir)`，以 `./` 开头的 command/args 按 base_dir 解析，并支持 `${EXT_DIR}` 占位。rupi-pkg 物化扩展时改为复制整个扩展目录（或 checkout 引用）：`extensions/<name>/` 下放 manifest 和脚本，ExtensionSet::scan_dir 支持一层子目录的 `*.json`。示例 manifest 改成相对 manifest 的路径。

### F3 [P0] truncate_middle 用字节长度判断、按字符数截取，非 ASCII 文本被重复输出
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-ext/src/lib.rs

证据：lib.rs:1478-1496：`if s.len() <= limit` 比较的是字节数，`keep = limit/2` 后用 `chars().take(keep)` 截头尾各 6000 个字符。中文每字 3 字节，19–36KB 的中文文本头尾两段会重叠，内容重复且变长。实测：300 行中文 19200B 的文件，read 输出 24603B，`第000行` 出现 2 次；仓库自带 docs/UPSTREAM.md（14579B）read 输出 16473B，比原文件还大，并带 `…[truncated N chars]…` 标记（N 实际是字节差）。调用点：read(lib.rs:574)、edit diff(lib.rs:765)、bash 取消时的部分输出(lib.rs:925)、扩展 stdout(rupi-ext/src/lib.rs:232)。

建议修法：按字节截取并对齐到 char boundary（`s.floor_char_boundary` 或手写 is_char_boundary 回退），头尾各 `limit/2` 字节，计数统一用字节并写明“bytes”。加多字节单测（中文、emoji）。

### F4 [P0] read 分页在 12KB 处停页后又做中间截断，页内静默丢行；也没有用上移植来的 truncate_head
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-tools/src/truncate.rs, docs/UPSTREAM.md

证据：read_path_paged 在 `page_bytes >= MAX_TOOL_OUTPUT(12000)` 时停止收行(lib.rs:522-526)，追加 `…[truncated: lines 1-120 of 3000 — pass offset=120]` 标记，然后对“页内容+标记”整体做 truncate_middle(lib.rs:574)，因为总长超过 12000，页中间被剪掉。实测：3000 行×100B 的文件，第一页报告 lines 1-120，但 L0060/L0061 缺失，换成 `…[truncated 184 chars]…`；按提示 offset=120 翻页，永远看不到这两行。只要源码文件超过 12KB、不带 limit 去读，就会触发。truncate.rs:2 写着“read 保头”，但 truncate_head 全仓只在 lib.rs:18 被 re-export，没有调用方。上游 read 是 2000 行/50KB 头截（UPSTREAM.md:41 左栏），rupi 实际是 12KB，比上游小 4 倍，文档没有说明。

建议修法：读页循环改成按 DEFAULT_MAX_BYTES(50KB)/limit 停在整行边界，删掉末尾的 truncate_middle（单行超长时按 Pi 的做法提示用 bash sed 读取）；offset 提示用实际交付的最后一行。UPSTREAM.md 写明真实上限。加“分页拼接等于原文件”的单测。

### F5 [P0] bash 遇到后台子进程会无限等待管道关闭，超时和取消都失效
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs

证据：End::Done 分支(lib.rs:902-903) 在 `child.wait()` 返回后调用 `join_pipe_readers(out_task, err_task, None)`，没有时限，要等到所有持有 stdout/stderr 写端的孙进程退出。这时 select 已经结束，timeout 和 cancel 都不再生效。实测：`{"command":"sleep 6 & echo started","timeout_secs":2}` 6.04s 后才返回 "started"；换成 `npm run dev &`、`python -m http.server &` 会一直挂到进程自然结束，Ctrl-C 也取消不了。上游 bash.js:101-103 明确处理了“detached descendants 继承 stdio 不应挂住”。

建议修法：End::Done 后用剩余超时（或固定 200–500ms 宽限）给 join_pipe_readers 设上限，到点后 kill_pid_tree 整组并舍弃残余；也可以把 Done 后的 join 放进同一个 select 继续监听 cancel/timeout。加单测：`sleep 5 & echo hi` 须在 1s 内返回 hi。

### F6 [P1] bash 子进程继承宿主 stdin：读 stdin 的命令挂到超时，RPC/TUI 下还会和宿主抢输入
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-cli/src/rpc.rs

证据：BashTool::execute_with_cancel(lib.rs:865-881) 只把 stdout/stderr 设成 piped，没有 `.stdin(Stdio::null())`。实测：ToolRegistry 执行 `read x; echo got:$x`（timeout 3s）时，父进程 stdin 是一个长时间不关闭的管道，3.04s 后返回 timed out；`rupi --mode rpc` 发送 `{"type":"bash","command":"cat >/dev/null; echo AFTER"}` 要等 8s，直到 stdin EOF 才返回，期间子进程和 rpc.rs 的 BufReader 竞争同一个 JSONL 通道。TUI 下子进程继承 raw-mode TTY。上游 bash.js:67 用 `stdio: ["ignore", "pipe", "pipe"]`。

建议修法：cmd.stdin(std::process::Stdio::null())；rupi-cli/src/rpc.rs 的 RPC bash 走同一工具，自动受益。加单测：`cat` 立即返回空串。

### F7 [P1] oneshot 扩展：孙进程不随超时/取消被杀、大参数写 stdin 无视超时、stderr 不截断
类型：bug，工作量：S，文件：crates/rupi-ext/src/lib.rs, crates/rupi-ext/README.md

证据：spawn_with_input(lib.rs:192-222) 没有 process_group，只靠 kill_on_drop 杀直接子进程。实测：`sh -c "cat >/dev/null; sleep 41 & wait"`、timeout 1s，返回后 `pgrep -f "sleep 41"` 仍有残留进程 85832。stdin 的 `write_all` 在超时计时开始之前执行(lib.rs:214-219)：子进程不读 stdin、参数超过管道缓冲时会一直阻塞。实测：`sleep 30` 加 300KB 参数、timeout_secs=2，8s 后仍未返回。失败分支把 stderr 原样拼进结果(lib.rs:241-242)，实测 `seq 1 200000 >&2; exit 1` 回包 1288915 字节。README:10 写的“stdout 与内置 bash 同口径 12k 折叠、超时 kill_on_drop”与实现不符：bash 是 50KB 尾截，stderr 完全不截。

建议修法：复用 rupi_tools::shell::configure_isolation 与 kill_tree（改成 pub(crate) → pub）；把写 stdin 和 wait 一起放进 timeout/select；stdout/stderr 统一走 bound_bash_output。execute 改成调用 execute_with_cancel(&CancelFlag::new())，删掉重复实现（约 15 行）。README 按实际口径改写。

### F8 [P1] 热重载删除或修改 JSON-RPC 扩展后子进程泄漏（RpcHost 与 incoming 任务形成 Arc 环）
类型：bug，工作量：S，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-ext/src/lib.rs

证据：start_async 中 `let peer = host.clone(); tokio::spawn(async move { while let Some(msg) = incoming.recv().await { peer.handle_incoming(msg).await } })`(rpc.rs:44-49)。incoming 的发送端由 host.rpc 的 route_task 持有，任务又持有 host，形成环，StdioRpc::_child 的 kill_on_drop 永远不会触发。drop_rpc(lib.rs:362-364) 只从 map 里删除。实测：加载 jsonrpc 扩展后删除 manifest，refresh_extensions 返回 `[ext] removed leaky`，再 drop ExtensionSet 和 ToolRegistry，`pgrep -f LEAKPROBE_77` 仍是 1。manifest 修改走 install_one→drop_rpc→start(lib.rs:382-388)，每改一次泄漏一个旧进程。

建议修法：任务里持有 Weak<RpcHost>，每条消息 upgrade 一次，失败则退出；或者给 RpcHost 加显式 shutdown（发 `shutdown` 通知 + kill 子进程 + abort 任务），drop_rpc 时调用。加单测断言进程退出。

### F10 [P1] 跨包同名资源互相覆盖，卸载一个包会误删另一个包的文件
类型：bug，工作量：M，文件：crates/rupi-pkg/src/install.rs

证据：materialize(install.rs:302-349) 直接覆盖 `skills/<name>`、`commands/<name>.md`、`extensions/<name>.json`、`themes/<name>.json`，不检查是否已被其他包记录占用；remove_materialized(install.rs:351-368) 按名字删除，不看归属。实测：pkgA、pkgB 都含 name=shared 的扩展，先后安装后 shared.json 内容是 B；`rupi uninstall pkgB` 后 extensions 目录变空，`rupi packages` 仍显示 pkgA 有 `ext=[shared]`，锁文件与磁盘不一致。

建议修法：安装前扫描锁文件中其他记录：遇到冲突默认报错，`--force` 时覆盖并把旧包记录里的这一项移除；卸载时只删除没有被其他记录引用的条目。补冲突单测。

### F11 [P1] git 安装钉短 SHA 或无效 SHA 时拉取失败仍报成功，装的是默认分支、锁文件却记成该 SHA
类型：bug，工作量：S，文件：crates/rupi-pkg/src/install.rs

证据：fetch_git(install.rs:211-224) 中 `git fetch --depth 1 origin <sha>` 的 status 只在 success 时才去 checkout，失败直接忽略，最后 `Ok(())`；checkout 的 status 也被丢弃。is_sha(install.rs:228-230) 接受 7 位以上的十六进制，但 git 不支持按缩写 SHA fetch。实测：`rupi install git:file:///tmp/rupi-probe/repo1@4890318`（v1 的短 SHA）输出 `fatal: couldn't find remote ref 4890318`，随后 `installed ... version 4890318`，实际 SKILL.md 内容是 v2；`@deadbeef` 也照样报 installed。git 的 detached HEAD 提示直接打到用户终端（`.status()` 继承 stdio）。

建议修法：fetch/checkout 失败直接 bail；缩写 SHA 改为先完整 clone（不带 --depth）再 `git checkout <sha>`，或者要求 40 位 SHA；子命令加 `-c advice.detachedHead=false` 并捕获输出。锁文件记录 `git rev-parse HEAD` 得到的真实 commit。

### F15 [P1] 沙箱 grep/glob 会跟随工作区内指向外部的文件符号链接，与 read 的口径不一致
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-tools/README.md

证据：ignore_walker 的 `follow_links(false)`(lib.rs:1176-1180) 只控制是否进入目录链接，文件链接仍会被产出；`path.is_file()` 和 `std::fs::read_to_string` 都会跟随链接(lib.rs:1220, 1352)。SandboxedTool 只校验 `path` 参数(lib.rs:317-337)。实测：root/link.txt → ../other/out.txt，沙箱 grep `{"pattern":"needle","path":"."}` 返回 `root/link.txt:1:needle_out`，glob 也列出 link.txt，而 read 同一路径会被拒绝。README:14 写的是 read/write/edit/glob/grep 都拒绝符号链接逃逸。

建议修法：walker 回调中对 `ent.path_is_symlink()` 的条目做 canonicalize，结果必须 starts_with(root)，否则跳过；SandboxedTool 把 root 通过参数或包装器传进 grep/glob/find。加单测。

### F16 [P1] ToolRegistry 用 HashMap，工具顺序每个进程随机、热重载后也会变，破坏 prompt cache
类型：perf，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-agent/src/lib.rs, crates/rupi-llm/src/anthropic.rs

证据：`tools: HashMap<String, Arc<dyn Tool>>`(lib.rs:92-94)，definitions() 按 HashMap 迭代顺序输出(lib.rs:119-121)，AgentLoop 直接用它构造请求的 tools 和系统提示的 AvailableTools(agent lib.rs:653-698)。实测：同一进程连建 4 个 with_builtins，得到 4 种顺序（例如 glob,bash,edit,... / read,edit,think,...）；同一 registry 注册再注销 30 个扩展后顺序变了。Anthropic 在“最后一个工具”上打 cache_control(anthropic.rs:76-79)，按前缀缓存，跨进程 --resume、/reload、MCP list_changed 之后都会缓存失效，请求也无法复现。

建议修法：改用 BTreeMap（按名字稳定排序）或 IndexMap/Vec 保持插入顺序；definitions() 输出稳定序。0 行增量，补一个顺序稳定性单测。

### F17 [P1] register* 协议只做了一半：keybinding 只能改 8 个内建动作且只读一次，provider 永久写盘且不能注销，command 不去重
类型：incomplete，工作量：M，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-tui/src/keybindings.rs, crates/rupi-tui/src/app.rs, crates/rupi-llm/src/catalog.rs

证据：registerKeybinding 写入进程级 static(rpc.rs:504-523)；KeyTable::load 只把 send/newline/paste/model/thinking/fold/enabledModels/editor 这 8 个动作映射过去，其他忽略(keybindings.rs:144-158)，没法把快捷键绑到扩展命令（Pi 的 registerShortcut 会绑到 handler）。TUI 启动时只 load 一次(app.rs:749)，运行期注册不生效，扩展卸载后也不清理。registerProvider 每次扩展启动都调用 register_extra_provider，写 `~/.rupi/providers.json` 和 `models.json`(catalog.rs:99-129)，扩展删除后仍留在磁盘，没有 unregisterProvider（上游 types.d.ts:1373 有）。registerCommand 直接 push，重复注册会出现重复条目(rpc.rs:154-166)。dispatch_builtin 的 `/hotkeys` 用的是 KeyTable::default()(app.rs:355-357)，主循环先被 parse_local_slash(app.rs:1177-1182) 截走，这条分支只有测试会走到。

建议修法：keybinding 增加 `command` 目标（触发扩展命令 expand），TUI 每次 drain 时合并，按扩展名归属并在 drop_rpc 时清理；provider 只登记在内存（或带来源标记，卸载时删除），增加 unregisterProvider；registerCommand 按 name upsert；删除 dispatch_builtin 里死掉的 /hotkeys 分支。

### F19 [P1] edit 不处理 CRLF/BOM，CRLF 文件用 LF 的 old_string 必然匹配失败
类型：bug，工作量：M，文件：crates/rupi-tools/src/lib.rs

证据：apply_edits 对原文做精确 `match_indices`(lib.rs:686-755)，没有行尾归一化。实测：文件内容 "a\r\nb\r\nc\r\n"，old_string "a\nb" 返回 `edit[0]: old_string not found in file`。模型生成的 old_string 都是 LF，Windows 仓库或 CRLF 文件基本无法 edit。上游 edit-diff.js:9-22 有 detectLineEnding/normalizeToLF/restoreLineEndings，141-170 还有 fuzzy 匹配（尾空白、Unicode 引号）。

建议修法：读入后剥掉 BOM，记录行尾风格并统一归一成 LF，old/new 同样归一；替换后按原行尾风格还原再写回。可选：精确匹配失败时做尾空白归一化重试。

### F23 [P2] 搜索类工具截断与排序错误：glob 静默截到 200、find 总报“1 more”、grep 行号按字符串排序
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-tools/src/optional.rs

证据：glob_blocking 达到 GLOB_MAX_HITS 后直接 Quit，没有截断标记(lib.rs:1227-1238)，实测 250 个文件输出 200 行、没有 truncated 字样。find_blocking 第一次超限就 fetch_add(1) 并 Quit(optional.rs:113-116)，实测 limit=10、共 50 个文件时输出 `...[truncated 1 more]`。grep 对 `path:line:text` 整串排序(lib.rs:1276-1278)，实测同一文件行序为 hit10,hit11,hit12,hit1,hit2,...。

建议修法：glob/find 截断时输出“至少 N+ more”或继续计数（只计数不收集）；grep 收集 (path, line_no, text) 元组，按 (path, line_no) 排序后再格式化。

### F24 [P2] read 遇到非 UTF-8 文本整文件报错；无 path 的沙箱 grep/glob/find/ls 落到进程 cwd 而不是 root
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-tools/src/optional.rs

证据：read_path_paged 用 `read_line` 读入 String(lib.rs:493-530)，遇到非法字节直接 Err。实测 latin1 文件返回 `read ... failed: invalid utf-8 sequence`，上游 read.js:112 用 `toString("utf-8")` 有损解码。SandboxedTool 只在有 `path` 参数时改写(lib.rs:319-336)，GlobTool/GrepTool/FindTool/LsTool 默认 "."(lib.rs:1096-1100, 1140-1144; optional.rs:71-75, 153-157)。实测：进程 cwd≠root 时沙箱 grep 不带 path 返回 `./out.txt:1:needle_out`（root 外内容）。目前 CLI 中 root==启动 cwd，问题被掩盖。

建议修法：read 按字节读行后用 decode_utf8 有损解码；SandboxedTool 在 `path` 缺失、且工具 schema 含 path 时注入 root。

### F25 [P2] bash 超时时丢弃已经产出的输出（取消路径会保留）
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs

证据：End::TimedOut 分支 `let _ = Self::join_pipe_readers(...)` 丢弃尾部 ring(lib.rs:932-944)，只返回 `command timed out after Ns`；End::Cancelled 分支会附上 `[partial output]`(lib.rs:915-931)。实测：`echo partial; sleep 5`、timeout 1s 返回 "command timed out after 1s"，没有 partial。长构建超时后，模型看不到停在了哪里。

建议修法：TimedOut 与 Cancelled 共用同一段收尾，回包附上 bound_bash_output 处理后的部分输出。

### F149 [P2] bash 工具非零退出的错误文本重复写了 exit：`exit exit status: 3: `
类型：bug，工作量：S，文件：crates/rupi-tools/src/lib.rs

证据：lib.rs:912 `ToolOutput::err(format!("exit {status}: {s}"))`，而 ExitStatus 的 Display 本身输出 `exit status: 3`。实测 `rupi run --json "Use bash to run: exit 3"` 的 tool_execution_end 里是 `"content":"exit exit status: 3: "`。

建议修法：改成 `format!("exit code {}: {s}", status.code().map_or("signal".into(), |c| c.to_string()))`。

### F12 [P1] REPL/TUI 每轮 skills.refresh 丢掉 settings 启停过滤和 --skill 单文件
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-skills/src/lib.rs

证据：启动时 discover_skills(main.rs:846-861) 做 ingest_paths(extra)+retain_allowed(filter)；每轮 REPL 只调 `skills.refresh(&skill_dirs_runtime(..))`，再 ingest `cli.skill`(main.rs:2578-2582)，不再应用 `settings.skills` 的 `!name` 和 `+path`。TUI 每轮只调 `ctx.skills.refresh(&ctx.skill_dirs)`(app.rs:1492)，不 ingest 任何东西，而 skill_dirs_runtime 会排除单文件(main.rs:836-842)。实测：settings.json 写 `{"skills":["!secret-skill"]}` 后，chat 第一次 `/skills` 看不到 secret-skill，发一条消息后第二次 `/skills` 出现 `- secret-skill: should be disabled`。`rupi config disable skills x`（PR #40）因此只在第一轮有效。

建议修法：SkillRegistry 保存 discover 配置（dirs、extra_paths、allow 闭包或 ResourceFilter），提供 `refresh_configured()`；REPL/TUI/RPC 统一调用它。或者把 discover_skills 抽成可复用的 reload 函数，每轮调用。

### F84 [P1] settings.skills 的 `!name` 禁用只在启动时生效，REPL/TUI 每轮 refresh 后被禁用的 skill 又回来了
类型：bug，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-tui/src/app.rs, crates/rupi-skills/src/lib.rs

证据：main.rs:846-861 的 `discover_skills` 只在启动时执行 `retain_allowed`。REPL 每轮执行 main.rs:2578 的 `skills.refresh(...)`，TUI 在 app.rs:1492 执行 `ctx.skills.refresh(&ctx.skill_dirs)`。rupi-skills lib.rs:172-222 的 refresh 整表替换，不带过滤，也不再 ingest settings 里的 extra_paths。实测：`settings.json`=`{"skills":["!foo"]}`，chat 里第一次 /skills 没有 foo，问一句后再 /skills 出现了 `- foo: foo skill`。

建议修法：SkillRegistry 保存一个 allow 过滤器和 extra_paths（`set_filter`），refresh 结束时自动应用并重新 ingest；CLI/TUI 不再各自手工 retain。

### F174 [P1] 每次发送前 skills.refresh 会整表替换，丢掉 --skill 单文件、settings.skills 的额外路径和 deny 过滤
类型：bug，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-skills/src/lib.rs, crates/rupi-cli/src/main.rs

证据：app.rs:1492 `ctx.skills.refresh(&ctx.skill_dirs)`；refresh 最后执行 `*self.skills.write().unwrap() = skills`（rupi-skills lib.rs:221），整表替换。启动时 discover_skills（main.rs:846-861）额外做了 ingest_paths(cli.skill 和 filter.extra_paths) 和 `retain_allowed(filter.allows)`，这些只在启动时生效。所以首轮发送后，`--skill path/SKILL.md` 加载的 skill 消失，settings 里 `"skills":["!foo"]` 禁用的 foo 又出现。REPL（main.rs:2578-2582）会补 ingest cli.skill，但同样丢掉 extra_paths 和 deny 过滤。

建议修法：把“发现、ingest、过滤”封装成 `SkillLoader::reload(&registry)`，启动和每轮都调它；TuiContext 持有 loader，不再只持有 dirs。

### F30 [P2] pkg 与截断缺少关键用例：同名冲突、git ref 失败、npm 解包、多字节截断、分页完整性
类型：test-gap，工作量：M，文件：crates/rupi-pkg/src/install.rs, crates/rupi-tools/src/lib.rs, crates/rupi-tools/src/truncate.rs

证据：rupi-pkg 一共 8 个测试，只覆盖本地往返(install.rs:445-497)、本地 git clone 不带 ref(500-542)和 spec 解析；冲突、ref 钉选失败、npm（无 mock registry）都没有测试。truncate_middle 没有任何单测；read 分页测试(lib.rs:1663-1701) 只用 50 行小文件，碰不到 12KB 截断。

建议修法：增加：两包同名资源安装+卸载、`@<短sha>`/`@deadbeef` 期望报错、本地 tgz 模拟 npm（抽出 fetch_npm 的 HTTP 层）、中文 30KB 的 truncate_middle、对大文件分页逐页拼接并与原文逐字节比较。

### F28 [P1] 扩展协议缺端到端测试：CLI 级 jsonrpc、UI dialog 往返、register* 走线、热重载进程回收
类型：test-gap，工作量：M，文件：crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs, crates/rupi-cli/tests/smoke.rs

证据：唯一的 jsonrpc 测试 lib.rs:703 使用 multi_thread 运行时并直接调 RpcHost，没覆盖 CLI 的 current_thread 运行时（所以 panic 没被发现）。smoke.rs 中扩展相关测试只有 ext-list(983-1003) 和 extension_ui_response 的解析(106-124)。rpc.rs 测试只测 register_provider_params 的校验、push_keybinding、event_tags、dialog_event 的形状(652-700)，没有 registerCommand/subscribe/ui/select 通过管道往返的测试，也没有删除扩展后进程退出的断言。

建议修法：smoke 增加：jsonrpc manifest + `rupi -p`、`--mode rpc` 下 get_commands 含扩展命令、extension_ui_request→extension_ui_response 往返；rupi-ext 增加 current_thread 运行时用例和热重载后 pgrep 断言。

### F29 [P1] Windows/PowerShell 路径从未编译或运行：CI 无 Windows，单测含未加 cfg 的 unix 代码
类型：test-gap，工作量：M，文件：.github/workflows/ci.yml, crates/rupi-tools/src/shell.rs, crates/rupi-tools/src/lib.rs, crates/rupi-skills/src/lib.rs

证据：ci.yml 的矩阵只有 `os: [ubuntu-latest, macos-latest]`；shell.rs 中 `#[cfg(windows)]` 的 spawn_taskkill/pwsh_on_path/creation_flags(shell.rs:152-156, 180-224) 在 CI 中从不编译。测试 `use std::os::unix::fs::symlink`(tools lib.rs:2011, skills lib.rs:854) 没加 `#[cfg(unix)]`，Windows 上 `cargo test` 编译不过。bash 相关单测依赖 seq/sleep/printf/pgrep。HostShell::current 每次 bash 调用都会 spawn 一次 `where.exe pwsh.exe`(shell.rs:65-69, 213-224)，没有缓存。

建议修法：CI 增加 windows-latest 的 build + `cargo test -p rupi-tools -p rupi-skills`；unix 专用测试加 `#[cfg(unix)]`，Windows 版用 PowerShell 命令（Start-Sleep、Write-Output）；pwsh 探测结果放进 OnceLock 缓存。

### F31 [P2] 工具注册与搜索实现重复：glob≈find、with_sandboxed_builtins 逐个手写、oneshot execute 两份
类型：simplify，工作量：S，文件：crates/rupi-tools/src/lib.rs, crates/rupi-tools/src/optional.rs, crates/rupi-ext/src/lib.rs

证据：glob_blocking(lib.rs:1208-1239) 与 find_blocking(optional.rs:86-130) 是同一个 walker 加 glob_rel_matches，差别只在绝对/相对路径和上限；GlobTool 与 FindTool 的 pattern 校验逐字重复(lib.rs:1083-1095 vs optional.rs:55-70)。with_sandboxed_builtins(lib.rs:197-223) 7 段几乎一样的 register，register_optional 与 register_sandboxed_optional(lib.rs:162-193) 又各写一遍 match。ExternalTool::execute 与 execute_with_cancel 重复 spawn/timeout(rupi-ext lib.rs:143-186)。BashTool::kill_tree(lib.rs:973-977) 只是转发一次。

建议修法：抽出 `walk_glob(base, pattern, limit, PathStyle)`，供 glob/find 共用，校验函数也共用（约省 45 行）；用 `fn builtin(name) -> Option<Arc<dyn Tool>>` 加循环包装 sandbox（约省 30 行）；ExternalTool::execute 改为调用 execute_with_cancel（约省 15 行）；删除 BashTool::kill_tree 包装。

### F32 [P2] 死代码与无用依赖：register_all、Utf8Decoder::push、truncate_head、未用参数、未用 crate
类型：simplify，工作量：S，文件：crates/rupi-ext/src/lib.rs, crates/rupi-ext/src/rpc.rs, crates/rupi-tools/src/utf8.rs, crates/rupi-tools/src/truncate.rs, crates/rupi-tools/Cargo.toml, crates/rupi-skills/Cargo.toml, crates/rupi-pkg/Cargo.toml

证据：rupi_ext::register_all(lib.rs:512-525) 只在 tests/agent_ext.rs:41 被调用，前端都用 ExtensionSet::register。Utf8Decoder 的增量 push/finish(utf8.rs:17-54) 只服务于 decode_all，全仓没有增量使用者。truncate_head 没有调用方。dialog_event 与 parse_tool_result 的 `source` 参数都是 `let _ = source;`(rpc.rs:403, 642)。依赖引用计数为 0：rupi-skills 的 tokio/chrono、rupi-tools 的 tracing/serde、rupi-pkg 的 rupi-core。

建议修法：删除 register_all（测试改用 ExtensionSet::register），删除未用参数和依赖；Utf8Decoder 简化为 decode_utf8（约 30 行）；truncate_head 接到 read 上（见 read 分页那条）或者删除。

### F33 [P2] rupi-pkg 重复实现 skill/扩展的名称校验和 frontmatter 解析，与发现侧口径会漂移
类型：simplify，工作量：S，文件：crates/rupi-pkg/src/layout.rs, crates/rupi-skills/src/lib.rs, crates/rupi-ext/src/lib.rs

证据：layout.rs 自写了 frontmatter_field(392-407)、is_skill_name(409-418)、is_ext_name(428-433)，与 rupi-skills 的 split_frontmatter/validate_name(lib.rs:115-148)、ExtensionManifest::validate(rupi-ext lib.rs:94-110) 各维护一份。skill_install_name 在 frontmatter name 不合法时退回成清洗后的目录名(layout.rs:337-346)，但发现侧 Skill::load 会按 frontmatter 拒绝这个 skill，结果是安装成功却永远发现不到，也没有任何提示。

建议修法：rupi-pkg 依赖 rupi-skills/rupi-ext（或把校验函数下沉到 rupi-core），安装前用 Skill::load / load_one 做一次校验，失败就报错（约省 40 行）。

### F34 [P2] UPSTREAM/README 关于 find/ls、已知未做、事件 tag、扩展截断的描述过时
类型：doc-drift，工作量：S，文件：docs/UPSTREAM.md, README.md, crates/rupi-ext/README.md, crates/rupi-tools/README.md

证据：UPSTREAM.md:44 写“`grep` / `find` / `ls` → `grep` / `glob`；`ls` 用 bash”，但 PR #29(d963c5a) 已经加了可选的 find/ls(optional.rs)。README.md:135/162/188 和 rupi-tools/README.md 只写七工具，没提 `--tools find,ls`。UPSTREAM.md:116 的“已知未做：子 agent 继承 policy/approver；悬空符号链接写入逃逸沙箱”两项都已实现(subagent.rs:171-177 inherit_from；lib.rs:281-309 resolve_sandbox_path + 测试 lib.rs:2046-2065)。UPSTREAM.md:105 的 turn_start 事件实际不投递。rupi-ext/README.md:10 的“stdout 与内置 bash 同口径 12k 折叠”与实现不符（bash 是 50KB 尾截，扩展 stderr 不截）。rupi-tools/README.md:14 说 glob/grep 拒绝符号链接逃逸，实际会跟随文件链接。

建议修法：按代码更新这几处：工具表补上可选的 find/ls 及开启方式；删掉已完成的“已知未做”；事件 tag 与扩展截断口径在修复后同步；沙箱说明写明 bash 不受限、grep 跟随链接的现状（或修复后删除）。

## 补充分配的问题

### F9 [P1] `rupi install -l` 装到项目里的扩展运行时不会被加载，`rupi config list` 却显示 on
类型：incomplete，工作量：S，文件：crates/rupi-cli/src/main.rs, crates/rupi-cli/src/config_cmd.rs, crates/rupi-pkg/src/install.rs

证据：InstallOpts::root 在 local 时是 `cwd/.rupi`(install.rs:52-58)，扩展写到 `.rupi/extensions`。运行时 ext_dir 只取 `--ext-dir` 或 `home/extensions`(main.rs:638-642)，load_extensions 不扫项目目录。config_cmd.rs:139-140 却同时列出 `cwd/.rupi/extensions`。实测：在 git 项目里 `rupi install -l examples/packages/demo` 后，`.rupi/extensions/demo-echo.json` 已存在，`rupi config list` 显示 `extensions: on demo-echo`，`rupi ext-list` 输出 `no extensions in .../home2/extensions`，`rupi --trust-project -p hi` 没有 `[ext] 1 extensions`。project_resources 的信任清单(main.rs:426-459) 也不含 `.rupi/extensions`。

建议修法：load_extensions 在 load_project 为真时 add_extra(cwd/.rupi/extensions)（经过信任门）；project_resources 把 `.rupi/extensions`、`.rupi/themes` 纳入探测和清单；ext-list 同样合并。

### F13 [P1] RPC `prompt` 不展开斜杠命令、skill 和扩展命令，`get_commands` 却把它们列出来
类型：incomplete，工作量：M，文件：crates/rupi-agent/src/session.rs, crates/rupi-cli/src/rpc.rs

证据：AgentSession::prompt_queued(session.rs:320-367) 直接把 `q.to_user_message()` 交给 run_with_user，没有调 expand_slash_input、expand_command、expand_as_command；get_commands(session.rs:559-585) 却列出 prompt、`skill:<name>`、extension 三类命令。实测：在 RUPI_HOME/commands/ship.md 写 `Ship it: $ARGUMENTS`，RPC 发送 `{"type":"prompt","message":"/ship now"}`，get_messages 得到 user `['/ship now']`，原文被原样发给模型。上游 agent-session.js:1640-1661 在 prompt 中会执行扩展命令并展开 skill 和模板。REPL(main.rs:2584-2596) 与 TUI 会展开，三个前端行为不一致。

建议修法：把 REPL 中 expand_slash_input + ext expand_command 的逻辑下沉到 AgentSession（需要持有 command_dirs、skills、ext 命令执行器），prompt_queued 在空闲首条消息上展开；steer/follow_up 照 Pi 规则不展开。

### F14 [P1] 扩展事件订阅：turn_start/session_start 从不投递，session_end 实为每轮 RunEnd
类型：incomplete，工作量：S，文件：crates/rupi-agent/src/lib.rs, crates/rupi-ext/src/rpc.rs, docs/UPSTREAM.md

证据：AgentLoop 只通过 on_event 回调发出 TurnStart(lib.rs:680)，扩展循环 `for e in extensions { e.on_event(..) }` 只出现在 TurnEnd/RunEnd/ToolStart/ToolEnd/UiHint/UiPromptStart/End(lib.rs:828-1145)，没有 TurnStart。event_tags 把 TurnStart{turn:1} 映射成 session_start(rpc.rs:320-326)，这条路径永远走不到；session_start 只有 RpcHost 启动时那次合成通知(rpc.rs:68-81)。RunEnd 被标成 `session_end`(rpc.rs:327)，每次 prompt 结束都会触发。UPSTREAM.md:105 把 `turn_start` 列为核心事件 tag。

建议修法：在 lib.rs:680 后对 extensions 调用 on_event(&TurnStart)；session_end 改为在会话真正结束（进程退出、/new、switch_session）时由前端发出；RunEnd 只保留 `run_end` 标签。

### F18 [P1] 扩展不能拦截或改写工具调用，一个 manifest 也只能提供一个工具
类型：missing-feature，工作量：L，文件：crates/rupi-ext/src/rpc.rs, crates/rupi-agent/src/lib.rs

证据：AgentLoop 已有 ToolHook::before，可以改参数或拒绝(lib.rs:884-898, with_hook lib.rs:421)，但全仓除测试外没有调用 with_hook 的地方，扩展只能收到 tool_call 通知(rpc.rs:549-564)，不能 block。上游 ToolCallEventResult.block/reason（types.d.ts:1089-1095）是权限类扩展的基础。apply_initialize(rpc.rs:85-119) 只解析 commands/events，ExternalRpcTool 的定义固定等于 manifest 本身(rpc.rs:591-594)，一个长连接扩展无法注册多个工具（Pi 的 registerTool 可以多次调用）。

建议修法：新增 `ExtToolHook`：对订阅了 `tool_call` 且声明 `capabilities.hooks` 的扩展发 `hooks/before_tool` 请求（带超时），回 `{block, reason, args}`；前端装配时 `agent.with_hook`。initialize 支持 `capabilities.tools[]`，每项注册一个 ExternalRpcTool（tools/call 带 name 路由）。

### F20 [P1] 云端每次请求都往 /tmp 写新的 skill 目录，从不清理
类型：perf，工作量：S，文件：crates/rupi-skills/src/lib.rs, crates/rupi-server/src/run.rs

证据：ingest_markdown(lib.rs:226-266) 每次都在 `temp_dir()/rupi-remote-skills/<pid>-<nanos>/<name>/SKILL.md` 新建目录，没有删除逻辑，全仓 grep `rupi-remote-skills` 只有这一处。rupi-server 每个 /v1/agent 请求都会调 load_remote_guidance(run.rs:312)，对 glob 到的最多 32 个 SKILL.md 逐个 ingest(run.rs:902-919)。长期运行的控制面 /tmp 会无限增长（每请求 × 每 skill 一个目录）。

建议修法：给 SkillRegistry 增加内存 skill（`Skill::from_markdown(hint, body)`，dir 指向一个虚拟或远端路径，read_resource 走 Executor），不再落盘；如果必须落盘，用请求级 TempDir 并在结束时删除。

### F21 [P2] 扩展热重载在各前端不一致：TUI 只在 /reload 时刷新、RPC 没有重载，运行中启停扩展不生效
类型：incomplete，工作量：S，文件：crates/rupi-tui/src/app.rs, crates/rupi-cli/src/main.rs, crates/rupi-cli/src/rpc.rs, crates/rupi-ext/README.md

证据：REPL 每轮调用 refresh_extensions(main.rs:2563)；TUI 只在 `/reload` 时调用(app.rs:534-548)，发送前不刷新；--mode rpc 没有任何 refresh 或 reload 命令。README（rupi-ext/README.md:10）写的是“/reload 或每轮自动检查”。REPL 的 /reload 会重读 settings.extensions(main.rs:2355)，但不会重新 set_name_filter，`rupi config disable extensions x` 要重启才生效。

建议修法：TUI 在 app.rs:1492 处与 skills.refresh 一起调 refresh_extensions；RPC 增加 `reload` 命令，并在每次空闲 prompt 前刷新；/reload 时重新执行 apply_resource_filters，再对 snapshot 全量重算。

### F26 [P2] 工具参数与上游 Pi 存在缺口：grep 无 ignoreCase/literal/context、bash 无 timeout 别名/full_output_path、skill 无 disable-model-invocation
类型：missing-feature，工作量：M，文件：crates/rupi-tools/src/lib.rs, crates/rupi-skills/src/lib.rs

证据：上游 grep schema 有 ignoreCase/literal/context/limit(grep.js:11-18)，rupi grep 只有 pattern/path/include/max_results(lib.rs:1118-1127)，include 对应的是上游的 glob。上游 bash 参数名是 `timeout`，截断时给 full_output_path(bash.js:28-45)；rupi 只认 `timeout_secs`(lib.rs:965-969)，按 Pi 习惯发送 `timeout` 会被静默忽略，截断后的全文无处可查。上游 skills.js:262-279 支持 `disable-model-invocation`，不进系统提示、只能斜杠调用；rupi 的 Front(lib.rs:49-55) 不解析这个字段。

建议修法：grep 增加 ignore_case（`(?i)`）、literal（regex::escape）、context（前后 N 行）；bash 接受 `timeout` 别名，截断时把全量输出写到 temp 文件并回传路径；Skill 增加 disable_model_invocation 字段，index_block 和 tool 曝光时过滤。

### F27 [P2] 包管理缺 `rupi update`，npm 不支持 dist-tag 也不校验完整性
类型：missing-feature，工作量：M，文件：crates/rupi-pkg/src/install.rs, crates/rupi-cli/src/main.rs

证据：CLI 只有 Install/Uninstall/Packages(main.rs:320-340)，上游有 update(package-manager-cli.js:233)。fetch_npm 把版本当精确版本号查 `/versions/{ver}/dist/tarball`(install.rs:246-258)，`npm:foo@latest` 或 `@next` 会报 no tarball URL；下载后不校验 `dist.integrity`/`shasum` 就直接 tar 解包(install.rs:259-291)。fetch_git/fetch_npm 中的 git/tar 是阻塞的 std::process::Command，在 async fn 里调用(install.rs:183-226, 281-284)。

建议修法：增加 `rupi update [spec]`：按锁文件里的 spec 重新安装，git 拉最新 ref，npm 查 dist-tags.latest；版本先查 dist-tags 再查 versions；校验 sha512 integrity；git/tar 改用 tokio::process。

### F137 [P1] `config disable prompts <name>` 对包安装的 prompt 不生效，config list 却显示 off
类型：bug，工作量：S，文件：crates/rupi-core/src/commands.rs, crates/rupi-pkg/src/install.rs, crates/rupi-cli/src/config_cmd.rs

证据：rupi-pkg 把包里的 prompts 装进 `<home>/commands/`(install.rs:320)。过滤只对父目录名是 `prompts` 的文件生效(commands.rs:36-42 `if is_prompt_file(&p)`，is_prompt_file 定义在 commands.rs:220-225)。config_cmd.rs:177 的 list_prompts 直接用 allows_prompt 判定开关。复现：`rupi install /tmp/rupi-e2e/pkgsrc/demo` → `rupi config disable prompts greet`（settings 写入 `"prompts":["!greet"]`）→ `rupi config list` 显示 `off  greet`，但 `rupi commands` 仍列出 `/greet  Greet someone`，在 REPL 里输入 `/greet who=Alice` 仍然打印 `[command /greet]` 并发给模型。

建议修法：PromptFilter 的 deny/allow 对 commands/ 和 prompts/ 两个目录都生效（去掉 is_prompt_file 的限制，或者让安装器把 prompts 装进 `prompts/`）。config list 与运行时用同一个判定函数。

# Track: executor-output-bounds

进度见 [`docs/ROADMAP.md`](../../ROADMAP.md)。F 编号与 [`../README.md`](../README.md) 总表一致，B 编号见 [`../upstream-pi/ranked.md`](../upstream-pi/ranked.md)。行号基于 `c293c78`。本文原是写给实现者的任务说明。

分支：`thoxvi/executor-output-bounds`，worktree：`.worktree/executor-output-bounds`

## 目标
执行面对单个命令的输出、时长和取消都有硬约束，不会因一个租户拖垮节点；取消真正杀掉整组进程。

## 负责文件
- crates/rupi-runtime/**
- crates/rupi-server/src/tools.rs
- crates/rupi-server/src/bin/**
- crates/rupi-server/Cargo.toml 中的 [[bin]] 段

不在本 track：rupi-server 其余文件归 cloud-run-safety。

## 特别说明
- macOS 上 rupi-runtime 的测试会打印 `tar: Option --transform=s|^|../| is not supported`（tar.rs:poison_tarball 依赖 GNU tar）。让这个测试辅助函数在 bsdtar 上也能产出带 ../ 成员的归档（例如用 Rust 直接写 ustar 头 + flate2，看 Cargo.lock 里已有哪些 crate），从而让“拒绝越界解包”的防护测试在 macOS 上真正执行。
- rupi-execd / rupi-sandboxd 在 rupi-runtime 和 rupi-server 各有一份 [[bin]]，构建时出现 `output filename collision` 警告。先 diff 两份源码，确认 CI、docs、tests 的引用后，只保留一份并消除冲突（rupi-server 集成测试依赖 CARGO_BIN_EXE_rupi-execd，注意保持可用）。
- ExecResult 新增字段必须 #[serde(default)]，新旧控制面与执行节点可以混跑。
- 这是加固我们自己的执行节点隔离与资源限制的防御性工作。

## 借鉴上游 Pi 的条目（来自对标 Pi v1.4.0 的分析，按 SaaS 价值排序）

### B3. bash 输出设上限，超出部分 spill 到工作区 [critical/M]
本 track 范围：全部

上游参考：pkg/env/daemon/src/exec.rs:129-190（Spill）；pkg/env/docs/protocol.md「Scheduling」（未发送输出超过 4 MiB 就施加背压）；coding-agent.CHANGELOG.md:8、27（#11360 maxBuffer 和 truncated）；pkg/durable/src/tools/bash.ts:20-26

差距：已核实。Engine::exec_for 用 wait_with_output 把 stdout 和 stderr 全部读进内存（crates/rupi-runtime/src/engine.rs:320-323），超时上限 300 秒，execd 和 sandboxd 都走这条路径。HttpExecutor 用 resp.text() 读完整响应体（crates/rupi-runtime/src/http.rs:36-38）。ExecBashTool 把输出原样拼接（crates/rupi-server/src/tools.rs:255-284）。一条 yes 命令就能让共享的 execd OOM，同节点的其他租户一起受影响；这些输出还会进入模型上下文、SSE、Redis，以及 PG 的两个 GIN 索引。

设计：Engine 改为并发、有界地读取管道（复用 rupi-tools 的 PipeTail），只保留尾部 50KB 或 2000 行。超过阈值后，把完整输出追加写到工作区 .rupi/out/<call>.log，权限 0600，每个会话最多 8 个文件、共 32 MiB，快照打 tar 时排除这个目录。超过硬上限（例如 64 MiB）时，先对进程组发 SIGTERM，再发 SIGKILL。ExecResult 增加 truncated、total_bytes、full_output_path、duration_ms、timed_out 字段，全部 serde default，新旧版本可以混跑。控制面的 ExecBashTool 和 resume 路径再套一层 bound_bash_output。exec 和 fs 端点各自读取有上限的响应体，不要在 snapshot 共用的 post() 上加全局上限。磁盘写满的问题另做卷配额。

### B5. 让取消真正生效：PoolScheduler::abort 和进程组清理 [high/S]
本 track 范围：全部

上游参考：pkg/env/docs/protocol.md「Cancellation」「Liveness」（cancel 杀进程组；连续 30 秒收不到字节就杀掉所有进程组并退出）

差距：已核实。云端的 app.executor 是 PoolScheduler，它的 impl Executor（crates/rupi-runtime/src/pool.rs:156 起）没有实现 abort，用的是 trait 的默认空实现（crates/rupi-runtime/src/lib.rs:256-258）。所以 run.rs:230-237 里的 abort 在云端什么也不做：SSE 断开、心跳丢失或 cancel 之后，bash 会一直跑到 300 秒超时，还可能在下一个 run 里继续写同一个工作区。kill_on_drop 只杀直接子进程 sh（crates/rupi-runtime/src/jail.rs:183），命令运行在独立的进程组里（jail.rs:195），孙进程会变成孤儿进程继续运行。

设计：PoolScheduler 实现 abort，按 pick(h) 转发；加集成测试，断言取消后 execd 的进程组已被杀掉。Engine 在 exec 超时或被 drop 时对进程组调用 killpg，不再只依赖 kill_on_drop。把执行和 HTTP 连接解绑的工作放在第 33 项。

### B11. 降低 PG 和执行面的固定负载：准入时不再全表 reap，reconcile 分片，执行器 stats 改为后台缓存 [high/S]
本 track 范围：只做执行面：PoolScheduler 后台缓存 stats、HttpExecutor 设连接/总超时与有界读取

上游参考：pkg/durable/docs/group-commit.md §1、§4-5（合并低价值的小写入）；telemetry.README.md:209

差距：已核实。每次 acquire_lease 都会跑一次全局的 reap_stale_runs（quota.rs:122，db.rs:936-947）。每个副本每 15 秒遍历一遍所有租户做 reconcile，每个租户一次 count(*)（crates/rupi-server/src/reclaim.rs:15-20）；10 万租户、50 个副本时约 33 万 QPS，而且和业务流量无关。/metrics、/ready 和每次 alloc 都要串行请求所有执行节点的 stats，客户端没有设超时（http.rs:76-89，pool.rs:134-145，crates/rupi-runtime/src/http.rs:22）。/metrics 也不鉴权。

设计：acquire 只检查自己会话的 run_id 是否过期；全局 reap 只在 reclaim 循环里做，加 LIMIT，并用 pg_try_advisory_lock 保证同一时间只有一个副本在跑。reconcile 改为每个 tick 执行一条 GROUP BY tenant_id，只修正计数不为零的租户。PoolScheduler 起一个后台 poller，每 5 秒并发拉一次 stats 并缓存，ranked、/ready、/metrics 都读缓存。reqwest 设连接超时和总超时。/metrics 移到 RUPI_METRICS_BIND 指定的内网端口。

## 代码审查发现的问题（逐条已在代码或实跑中核实；P0=high，P1=medium，P2=low）

### F195 [P0] macOS execd：bash 超时只杀外层包装 sh，沙箱内作业继续跑并写工作区（abort 前已清空 children）
类型：bug，工作量：S，文件：crates/rupi-runtime/src/engine.rs, crates/rupi-runtime/src/jail.rs

证据：engine.rs:319-327 先等 `tokio::time::timeout(child.wait_with_output())`，无论结果如何都先 `s.children.clear()`，之后超时分支才调 `self.abort(handle)`（engine.rs:341）。此时 children 已空，killpg 不会执行。超时导致 child 被 drop，kill_on_drop（jail.rs:183）只 SIGKILL 直接子进程，也就是 macOS 的外层 `/bin/sh -c 'sandbox-exec … .rupi-job.sh > file'`（jail.rs:455-470），内层 `/bin/sh .rupi-job.sh` 被 init 收养后继续运行。实测：bash `sleep 12; echo late > late.txt`、timeout_secs=2，2.0s 返回 'timed out'，但 ps 里仍有 ppid=1 的 `/bin/sh …/.rupi-job.sh`，12s 后 late.txt 被写出。Linux 上 bwrap 有 --die-with-parent（jail.rs:253），能兜住；无 bwrap 的 plain_jailed_sh 路径同样不会杀孙进程。

建议修法：超时分支在 clear 之前先 take 出 pid 再 killpg（或把 clear 移到 match 之后，只在正常结束时清空）。drop 路径也按进程组 kill，而不只依赖 kill_on_drop。补一个超时后作业确实停止的测试，macOS 和 Linux 都要覆盖。

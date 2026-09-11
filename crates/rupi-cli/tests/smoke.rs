//! CLI 端到端冒烟：key-less 子命令全链路（对标 README 快速开始）。
//!
//! 每个用例独占 `RUPI_HOME` + cwd（temp 隔离目录，无 .git、无项目资源，
//! 信任门不弹），并剥掉宿主真实 key 环境变量，强制走 Mock/本地路径，
//! 并行跑也不串台。TUI 需真终端，不在此覆盖。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 隔离家目录：每次调用唯一，调用方负责 cwd 也指过去（避开项目信任门）。
fn fresh_home() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("rupi-smoke-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rupi(home: &Path, args: &[&str]) -> Command {
    rupi_at(home, home, home, args)
}

fn rupi_at(rupi_home: &Path, user_home: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rupi"));
    cmd.env("RUPI_HOME", rupi_home)
        .env("HOME", user_home)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .env_remove("RUPI_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("RUPI_ANTHROPIC_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("RUPI_GEMINI_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY");
    cmd
}

fn out_text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn help_lists_key_subcommands() {
    let home = fresh_home();
    let o = rupi(&home, &["--help"]).output().unwrap();
    assert!(o.status.success());
    let (out, _) = out_text(&o);
    for sub in [
        "memory-show",
        "memory-write",
        "skills-list",
        "skill-distill",
        "sessions",
        "mcp-list",
        "ext-list",
        "commands",
        "login",
        "models",
    ] {
        assert!(out.contains(sub), "help 缺子命令 {sub}:\n{out}");
    }
}

#[test]
fn login_is_stubbed_and_models_catalog_prints() {
    let home = fresh_home();
    let o = rupi(&home, &["login", "anthropic"]).output().unwrap();
    assert!(o.status.success(), "login 非零: {o:?}");
    let (out, _) = out_text(&o);
    assert!(out.contains("stubbed"), "login 应声明 OAuth 未落地:\n{out}");
    assert!(out.contains("ANTHROPIC_API_KEY"), "{out}");

    let o = rupi(&home, &["--list-models"]).output().unwrap();
    assert!(o.status.success(), "--list-models 非零: {o:?}");
    let (out, _) = out_text(&o);
    assert!(out.contains("openai/gpt-4o-mini"), "--list-models:\n{out}");
    assert!(out.contains("anthropic/"), "{out}");
    assert!(out.contains("bedrock/"), "{out}");

    let o = rupi(&home, &["models"]).output().unwrap();
    assert!(o.status.success(), "models 非零: {o:?}");
    let (out, _) = out_text(&o);
    assert!(out.contains("vertex/"), "models:\n{out}");
}

#[test]
fn memory_write_show_search_roundtrip() {
    let home = fresh_home();
    let o = rupi(&home, &["memory-write", "add", "likes oolong tea"])
        .output()
        .unwrap();
    assert!(o.status.success(), "memory-write 失败: {o:?}");
    let o = rupi(&home, &["memory-show"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(out.contains("oolong"), "memory-show 无写入条目:\n{out}");
    let o = rupi(&home, &["memory-search", "oolong"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(out.contains("oolong"), "memory-search 查不到:\n{out}");
}

#[test]
fn builtin_skills_visible_regardless_of_cwd() {
    // 内建技能走 exe 锚定发现：cwd 是隔离 temp 目录（无 skills/builtin）也必须可见，
    // 否则换个目录跑就静默丢失 commit-helper 这类内建技能。
    let home = fresh_home();
    let o = rupi(&home, &["skills-list"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "skills-list 非零退出: {o:?}");
    assert!(out.contains("commit-helper"), "cwd 外内建技能丢失:\n{out}");
    let o = rupi(&home, &["skill-load", "commit-helper"])
        .output()
        .unwrap();
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "skill-load 非零退出: {o:?}");
    assert!(!out.trim().is_empty(), "skill-load 空输出:\n{o:?}");
}

#[test]
fn skill_distill_then_load() {
    let home = fresh_home();
    let o = rupi(
        &home,
        &[
            "skill-distill",
            "tea-guide",
            "brew oolong",
            "warm pot",
            "steep 30s",
        ],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "skill-distill 失败: {o:?}");
    let draft = home.join("skills").join("tea-guide").join("SKILL.md");
    assert!(draft.exists(), "草稿未落盘: {}", draft.display());
    let o = rupi(&home, &["skill-load", "tea-guide"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(out.contains("steep 30s"), "skill-load 无全文:\n{out}");
    let o = rupi(&home, &["skills-list"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(out.contains("tea-guide"), "skills-list 无新 skill:\n{out}");
}

fn write_skill(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# {name}\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn skills_list_scans_pi_and_agents_standard_dirs() {
    // 对齐 Pi：全局 ~/.pi/agent/skills、~/.agents/skills；项目 .pi/skills 与
    // 祖先 .agents/skills（停在 git 根）。HOME 与 RUPI_HOME 隔离，避免吃到宿主技能。
    let rupi_home = fresh_home();
    let user_home = fresh_home();
    let root = fresh_home();
    let repo = root.join("repo");
    let nested = repo.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: fake\n").unwrap();
    write_skill(
        &user_home
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("pi-global"),
        "pi-global",
        "from pi home",
    );
    write_skill(
        &user_home
            .join(".agents")
            .join("skills")
            .join("agents-global"),
        "agents-global",
        "from agents home",
    );
    write_skill(
        &nested.join(".pi").join("skills").join("cwd-pi"),
        "cwd-pi",
        "from cwd pi",
    );
    write_skill(
        &repo.join(".agents").join("skills").join("repo-agents"),
        "repo-agents",
        "from repo agents",
    );
    write_skill(
        &nested.join(".rupi").join("skills").join("cwd-rupi"),
        "cwd-rupi",
        "from rupi project",
    );
    write_skill(
        &root
            .join("outside")
            .join(".agents")
            .join("skills")
            .join("leak"),
        "leak-skill",
        "must stay outside git root",
    );

    let o = rupi_at(&rupi_home, &user_home, &nested, &["skills-list"])
        .output()
        .unwrap();
    let (out, err) = out_text(&o);
    assert!(
        o.status.success(),
        "skills-list 非零退出:\nstdout={out}\nstderr={err}"
    );
    for name in [
        "commit-helper",
        "pi-global",
        "agents-global",
        "cwd-pi",
        "repo-agents",
        "cwd-rupi",
    ] {
        assert!(out.contains(name), "skills-list 缺 {name}:\n{out}");
    }
    assert!(
        !out.contains("leak-skill"),
        "仓外祖先 skill 不应被扫到:\n{out}"
    );
}

#[test]
fn sessions_empty_then_run_persists() {
    let home = fresh_home();
    let o = rupi(&home, &["sessions"]).output().unwrap();
    assert!(o.status.success());
    // 非交互 run（Mock 演示）：无 key 也能跑完并落盘（全局 flag 在子命令前）
    let o = rupi(&home, &["--no-approve", "run", "say hi"])
        .output()
        .unwrap();
    let (out, err) = out_text(&o);
    assert!(o.status.success(), "run 失败:\nstdout={out}\nstderr={err}");
    assert!(out.contains("demo mode"), "run 未走 Mock 演示:\n{out}");
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(!out.trim().is_empty(), "run 后 sessions 为空");
}

#[test]
fn no_memory_run_still_works() {
    // --no-memory：回合内撤下内建记忆工具与指导块，run 照常走通
    let home = fresh_home();
    let o = rupi(&home, &["--no-memory", "run", "say hi"])
        .output()
        .unwrap();
    let (out, err) = out_text(&o);
    assert!(
        o.status.success(),
        "--no-memory run 失败:\nstdout={out}\nstderr={err}"
    );
    assert!(out.contains("demo mode"));
}

/// chat 子进程：全局 flag 在子命令前，stdin 喂整段输入后 EOF。
fn chat_with(home: &Path, global: &[&str], input: &[u8]) -> Output {
    let mut args: Vec<&str> = global.to_vec();
    args.push("chat");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rupi"));
    child
        .env("RUPI_HOME", home)
        .current_dir(home)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("RUPI_API_KEY")
        .env_remove("OPENAI_API_KEY");
    let mut child = child.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn settings_and_system_md_apply() {
    let home = fresh_home();
    std::fs::write(
        home.join("settings.json"),
        r#"{"model":"gpt-4o-mini","theme":"light","tools":["think"],"compaction":{"reserveTokens":99,"keepRecentTokens":50}}"#,
    )
    .unwrap();
    std::fs::write(home.join("SYSTEM.md"), "YOU ARE CUSTOM SYS").unwrap();
    let o = rupi(&home, &["--help"]).output().unwrap();
    assert!(o.status.success());
    let (out, _) = out_text(&o);
    assert!(out.contains("continue"), "help 缺 --continue:\n{out}");
    assert!(out.contains("no-session"), "help 缺 --no-session:\n{out}");
    assert!(
        out.contains("system-prompt"),
        "help 缺 --system-prompt:\n{out}"
    );
}

#[test]
fn continue_and_no_session_and_export_import() {
    let home = fresh_home();
    let o = rupi(
        &home,
        &["--no-approve", "--name", "alpha", "run", "first hello"],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "named run 失败: {o:?}");
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(out.contains("alpha"), "sessions 未显示 --name:\n{out}");
    let o = rupi(
        &home,
        &["--no-approve", "--continue", "run", "second hello"],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "--continue run 失败: {o:?}");
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out2, _) = out_text(&o);
    assert_eq!(out2.lines().count(), 1, "--continue 不应建新会话:\n{out2}");

    let home2 = fresh_home();
    let o = rupi(&home2, &["--no-approve", "--no-session", "run", "ghost"])
        .output()
        .unwrap();
    assert!(o.status.success(), "--no-session run 失败: {o:?}");
    let o = rupi(&home2, &["sessions"]).output().unwrap();
    let (out3, _) = out_text(&o);
    assert!(
        out3.contains("no sessions") || out3.lines().count() == 0,
        "--no-session 仍落盘:\n{out3}"
    );

    let o = chat_with(&home, &["--no-review"], "/export\n/quit\n".as_bytes());
    assert!(o.status.success(), "/export 失败: {o:?}");
    let (out, err) = out_text(&o);
    assert!(
        out.contains("[export]") || err.contains("[export]"),
        "缺 export 确认:\n{out}\n{err}"
    );
}

#[test]
fn exclude_tools_help_and_chat_name() {
    let home = fresh_home();
    let o = chat_with(
        &home,
        &["--no-review", "--tools", "think", "--exclude-tools", "bash"],
        "/name demo-session\n/quit\n".as_bytes(),
    );
    assert!(o.status.success(), "/name 失败: {o:?}");
    let (out, _) = out_text(&o);
    assert!(out.contains("[name] demo-session"), "缺 /name 回显:\n{out}");
}

#[test]
fn chat_quit_exits_zero() {
    let home = fresh_home();
    let o = chat_with(&home, &[], b"/quit\n");
    assert!(o.status.success(), "chat /quit 非零退出: {o:?}");
}

#[test]
fn chat_compact_reports_compaction_operation() {
    // /compact 走 force_compress_with_event：真压实发射 CompactionStart/End，
    // stderr 可见 [compacting]/[compacted] 进度行（对标上游 compaction operation）。
    let home = fresh_home();
    let o = chat_with(
        &home,
        &["--no-review", "--compress-keep", "2"],
        "hi one\nhello two\n/compact\n/quit\n".as_bytes(),
    );
    let (_, err) = out_text(&o);
    assert!(o.status.success(), "chat /compact 非零退出: {o:?}");
    assert!(err.contains("[compacting]"), "缺压实开始行:\n{err}");
    assert!(
        err.contains("[compacted: summarized"),
        "缺压实结束行:\n{err}"
    );
}

#[test]
fn chat_bare_goto_shows_usage_without_model_call() {
    // 裸 `/goto` 必须本地拦截给用法：stdout 有 usage，且 stdout 无 demo mode
    //（无模型回包=没漏进模型白烧一轮；stderr 的 mock 横幅恒含 demo mode，只能断言 stdout）。
    let home = fresh_home();
    let o = chat_with(&home, &[], b"/goto\n/quit\n");
    assert!(o.status.success(), "chat 裸 /goto 非零退出: {o:?}");
    let (out, _) = out_text(&o);
    assert!(out.contains("usage:"), "裸 /goto 未给用法:\n{out}");
    assert!(
        !out.contains("demo mode"),
        "裸 /goto 漏进模型白烧一轮:\n{out}"
    );
}

#[test]
fn heuristic_review_on_by_default_no_review_opts_out() {
    // 默认启发式复盘：“请记住”触发 memory 建议并打印；--no-review 关闭后无声。
    let home = fresh_home();
    let o = chat_with(&home, &[], "请记住我爱喝乌龙茶\n/quit\n".as_bytes());
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "chat 非零退出: {o:?}");
    assert!(
        out.contains("[review] memory add"),
        "默认复盘未建议:\n{out}"
    );
    let home2 = fresh_home();
    let o = chat_with(
        &home2,
        &["--no-review"],
        "请记住我爱喝乌龙茶\n/quit\n".as_bytes(),
    );
    let (out2, _) = out_text(&o);
    assert!(o.status.success(), "--no-review chat 非零退出: {o:?}");
    assert!(!out2.contains("[review]"), "--no-review 仍有复盘:\n{out2}");
}

#[test]
fn chat_review_apply_persists_memory_and_failure() {
    // --review-apply：启发式复盘的记忆/失败建议真正落盘（自积累端到端证据）。
    // 落盘位置：<RUPI_HOME>/memories/MEMORY.md 与 failures.md。
    let home = fresh_home();
    let o = chat_with(
        &home,
        &["--review-apply"],
        "请记住我爱喝乌龙茶\n/quit\n".as_bytes(),
    );
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "--review-apply chat 非零退出: {o:?}");
    assert!(
        out.contains("[review] memory saved"),
        "记忆建议未落盘:\n{out}"
    );
    let mem = std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap_or_default();
    assert!(mem.contains("乌龙茶"), "MEMORY.md 无复盘条目:\n{mem}");

    let home2 = fresh_home();
    let o = chat_with(
        &home2,
        &["--review-apply"],
        "不对，你搞错了目录\n/quit\n".as_bytes(),
    );
    let (out2, _) = out_text(&o);
    assert!(o.status.success(), "--review-apply chat 非零退出: {o:?}");
    assert!(
        out2.contains("[review] failure saved"),
        "失败建议未落盘:\n{out2}"
    );
    let fails =
        std::fs::read_to_string(home2.join("memories").join("failures.md")).unwrap_or_default();
    assert!(fails.contains("目录"), "failures.md 无纠正条目:\n{fails}");
}

#[test]
fn run_review_apply_persists_memory() {
    // 非交互 run 同样走复盘落盘（与 chat 不同的接线点 run_once，值得单独覆盖）。
    let home = fresh_home();
    let o = rupi(
        &home,
        &[
            "--review-apply",
            "--no-approve",
            "run",
            "请记住我爱喝乌龙茶",
        ],
    )
    .output()
    .unwrap();
    let (out, err) = out_text(&o);
    assert!(
        o.status.success(),
        "run 非零退出:\nstdout={out}\nstderr={err}"
    );
    assert!(
        out.contains("[review] memory saved"),
        "run 记忆建议未落盘:\n{out}"
    );
    let mem = std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap_or_default();
    assert!(mem.contains("乌龙茶"), "MEMORY.md 无复盘条目:\n{mem}");
    // 同一轮的会话落盘也按 trigram 可查（中文会话召回不断）
    let o = rupi(&home, &["session-search", "乌龙茶"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "session-search 非零退出: {o:?}");
    assert!(out.contains("乌龙茶"), "会话中文未召回:\n{out}");
}

#[test]
fn run_resume_continues_same_session() {
    // --resume 断点续聊：第二次 run 不建新会话，同会话消息数增长
    let home = fresh_home();
    let o = rupi(&home, &["--no-approve", "run", "first hello"])
        .output()
        .unwrap();
    assert!(o.status.success(), "首次 run 失败: {o:?}");
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out, _) = out_text(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "首次 run 后应恰 1 会话:\n{out}");
    let sid = lines[0]
        .split_whitespace()
        .nth(1)
        .expect("解析会话 id")
        .to_string();

    let o = rupi(
        &home,
        &["--no-approve", "--resume", &sid, "run", "second hello"],
    )
    .output()
    .unwrap();
    let (out2, err2) = out_text(&o);
    assert!(
        o.status.success(),
        "resume run 失败:\nstdout={out2}\nstderr={err2}"
    );
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out3, _) = out_text(&o);
    let lines3: Vec<&str> = out3.lines().collect();
    assert_eq!(lines3.len(), 1, "resume 后不应建新会话:\n{out3}");
    assert!(
        lines3[0].contains(&sid[..8.min(sid.len())]) || lines3[0].contains(&sid),
        "会话 id 不一致:\n{out3}"
    );
    // 两轮共 4 条消息（2 问 2 答），数只增不重置
    let count: i64 = lines3[0]
        .split('(')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(count >= 4, "续聊后消息数未增长:\n{out3}");
}

#[test]
fn run_resume_empty_session_starts_fresh() {
    // 进 chat 即退：留下零消息会话行；resume 它应续进同 id 而非 bail。
    // 完全未知的 id 仍 bail（与空会话区分）。
    let home = fresh_home();
    let o = chat_with(&home, &[], b"/quit\n");
    assert!(o.status.success(), "空 chat 非零退出: {o:?}");
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out, _) = out_text(&o);
    let sid = out
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .expect("解析空会话 id")
        .to_string();

    let o = rupi(&home, &["--no-approve", "--resume", &sid, "run", "hello"])
        .output()
        .unwrap();
    let (out2, err2) = out_text(&o);
    assert!(
        o.status.success(),
        "空会话 resume 失败:\nstdout={out2}\nstderr={err2}"
    );
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out3, _) = out_text(&o);
    assert_eq!(out3.lines().count(), 1, "空会话续聊建了新会话:\n{out3}");

    let o = rupi(
        &home,
        &["--no-approve", "--resume", "deadbeef", "run", "hi"],
    )
    .output()
    .unwrap();
    assert!(!o.status.success(), "未知会话 resume 应失败: {o:?}");
    let (_, err) = out_text(&o);
    assert!(err.contains("unknown session"), "未知会话提示不对:\n{err}");
}

#[test]
fn memory_write_refuses_secrets() {
    // 密钥落盘即拒绝（Hermes secret scanning）：非零退出 + 文件不被污染
    let home = fresh_home();
    let o = rupi(&home, &["memory-write", "add", "api key sk-abc123"])
        .output()
        .unwrap();
    assert!(!o.status.success(), "密钥写入应被拒绝: {o:?}");
    let (_, err) = out_text(&o);
    assert!(err.contains("secret"), "拒绝提示不对:\n{err}");
    let mem = std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap_or_default();
    assert!(!mem.contains("sk-abc123"), "密钥泄漏进记忆文件:\n{mem}");
}

#[test]
fn ext_list_discovers_valid_and_skips_invalid() {
    // 扩展发现端到端：有效 manifest 列出，非法（坏名/坏 schema）静默跳过不炸整单
    let home = fresh_home();
    let dir = home.join("extensions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("shout.json"),
        r#"{"name":"shout","description":"shout text","input_schema":{"type":"object"},"command":"true"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("bad.json"),
        r#"{"name":"Bad Name!","description":"bad","input_schema":[],"command":""}"#,
    )
    .unwrap();
    let o = rupi(&home, &["ext-list"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "ext-list 非零退出: {o:?}");
    assert!(out.contains("shout"), "有效扩展未列出:\n{out}");
    assert!(!out.contains("Bad Name"), "非法扩展应跳过:\n{out}");
}

#[test]
fn repl_toggles_all_respond() {
    // REPL 开关全路径：每个内建切换都给反馈且不炸（拼写/崩溃回归网）
    let home = fresh_home();
    let o = chat_with(
        &home,
        &[],
        "/plan\n/thinking\n/model\n/skills\n/commands\n/reload\n/tree\n/rewind\n/goto zzz\n/quit\n"
            .as_bytes(),
    );
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "开关遍历非零退出: {o:?}");
    for want in [
        "[plan mode on]",
        "[thinking",
        "[model ",
        "[ext] no changes",
        "[rewind] nothing to undo",
        "(empty session",
        "unknown or ambiguous",
    ] {
        assert!(out.contains(want), "缺 `{want}`:\n{out}");
    }
}

#[test]
fn trust_gate_skip_remember_and_silence() {
    // 项目信任门三态：n 跳过进聊天、y 记住、下次同目录免扰（cwd 即 home，项目资源自带）。
    let home = fresh_home();
    std::fs::create_dir_all(home.join(".rupi")).unwrap();
    std::fs::write(
        home.join(".rupi").join("MEMORY.md"),
        "project convention: tabs",
    )
    .unwrap();

    let o = chat_with(&home, &[], "n\n/quit\n".as_bytes());
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "信任门跳过非零退出: {o:?}");
    assert!(out.contains("[trust]"), "未弹信任门:\n{out}");
    assert!(out.contains("已跳过"), "跳过无反馈:\n{out}");

    let o = chat_with(&home, &[], "y\n/quit\n".as_bytes());
    assert!(o.status.success(), "信任记住非零退出: {o:?}");

    let o = chat_with(&home, &[], b"/quit\n");
    let (out3, _) = out_text(&o);
    assert!(o.status.success(), "记住后进聊天非零退出: {o:?}");
    assert!(!out3.contains("[trust]"), "记住后仍打扰:\n{out3}");
}

#[test]
fn empty_states_exit_zero() {
    let home = fresh_home();
    for sub in ["ext-list", "commands"] {
        let o = rupi(&home, &[sub]).output().unwrap();
        assert!(o.status.success(), "{sub} 非零退出: {o:?}");
    }
    for sub in ["session-search", "memory-search"] {
        let o = rupi(&home, &[sub, "nothing-here"]).output().unwrap();
        assert!(o.status.success(), "{sub} 非零退出: {o:?}");
        // 空结果给提示而非零输出（与 ext-list/commands 同先例）
        let (out, _) = out_text(&o);
        assert!(out.contains("no matching"), "{sub} 空结果零输出:\n{out}");
    }
    // 空会话库给提示而非零输出
    let o = rupi(&home, &["sessions"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(
        out.contains("no sessions yet"),
        "sessions 空库零输出:\n{out}"
    );
    // 未知会话 id 给提示（与“存在但空”区分）
    let o = rupi(&home, &["session-show", "deadbeef"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(o.status.success(), "session-show 非零退出: {o:?}");
    assert!(
        out.contains("unknown session"),
        "session-show 未知 id 零输出:\n{out}"
    );
    // 空扩展目录给提示而非零输出（审计项回归）
    let o = rupi(&home, &["ext-list"]).output().unwrap();
    let (out, _) = out_text(&o);
    assert!(!out.trim().is_empty(), "ext-list 空目录零输出");
}

#[test]
fn mcp_list_probes_fake_server() {
    let home = fresh_home();
    // fake server 路径相对 workspace 拼（CARGO_MANIFEST_DIR 即 rupi-cli 包目录）。
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("rupi-mcp")
        .join("tests")
        .join("fake_mcp_server.py");
    assert!(server.exists(), "fake server 缺失: {}", server.display());
    let o = rupi(&home, &["mcp-list", "python3", server.to_str().unwrap()])
        .output()
        .unwrap();
    let (out, err) = out_text(&o);
    assert!(
        o.status.success(),
        "mcp-list 失败:\nstdout={out}\nstderr={err}"
    );
    assert!(out.contains("mcp_echo"), "未探到 fake 工具:\n{out}");
    assert!(out.contains("== resources =="), "缺资源区段:\n{out}");
    assert!(
        out.contains("test://notes/hello"),
        "未探到 fake 资源:\n{out}"
    );
    assert!(out.contains("== prompts =="), "缺模板区段:\n{out}");
    assert!(out.contains("greet"), "未探到 fake 模板:\n{out}");
}

#[test]
fn run_sends_session_id_affinity_header() {
    // 本地 OpenAI 桩：记下每请求 x-session-id，回固定文本（无 key 也测真 HTTP 路径）。
    use std::io::{BufRead, Read, Write};
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_clone = seen.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().take(8) {
            let Ok(mut sock) = sock else { break };
            let seen = seen_clone.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
                let mut content_len = 0usize;
                let mut sid = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let t = line.trim();
                    if t.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = t.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("x-session-id") {
                            sid = v.trim().to_string();
                        }
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                seen.lock().unwrap().push(sid);
                // run 走流式：stream:true 回 SSE（delta 增量 + [DONE]），否则回单 JSON
                let streaming = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
                    .unwrap_or(false);
                let (ctype, payload) = if streaming {
                    (
                        "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"stub-hi\"}}]}\n\ndata: [DONE]\n\n"
                            .to_string(),
                    )
                } else {
                    (
                        "application/json",
                        r#"{"choices":[{"message":{"content":"stub-hi"},"finish_reason":"stop"}]}"#
                            .to_string(),
                    )
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(payload.as_bytes());
            });
        }
    });

    let stub_env = |cmd: &mut Command| {
        cmd.env("RUPI_API_KEY", "fake-test-key")
            .env("RUPI_BASE_URL", format!("http://{addr}/v1"))
            .env("RUPI_SESSION_AFFINITY", "1");
    };
    let home = fresh_home();
    let mut cmd = rupi(&home, &["--no-approve", "run", "hello affinity"]);
    stub_env(&mut cmd);
    let o = cmd.output().unwrap();
    let (out, err) = out_text(&o);
    assert!(o.status.success(), "run 失败:\nstdout={out}\nstderr={err}");
    assert!(out.contains("stub-hi"), "桩回包未进正文:\n{out}");
    let sid = err
        .lines()
        .find_map(|l| l.strip_prefix("[session "))
        .and_then(|s| s.split(']').next())
        .map(str::to_string)
        .expect("stderr 应有 [session id]");
    let got = seen.lock().unwrap();
    assert!(!got.is_empty(), "桩没收到任何请求");
    assert!(
        got.iter().all(|h| h == &sid),
        "亲和头应全等于会话 id {sid}：{got:?}"
    );
    drop(got);

    // --resume 同会话：亲和头保持同 id（对标上游 sessionId 跨续聊粘滞，
    // 实例级随机 id 只保同进程，续聊即换域）
    let mut cmd2 = rupi(&home, &["--no-approve", "--resume", &sid, "run", "again"]);
    stub_env(&mut cmd2);
    let o2 = cmd2.output().unwrap();
    let (out2, err2) = out_text(&o2);
    assert!(
        o2.status.success(),
        "resume run 失败:\nstdout={out2}\nstderr={err2}"
    );
    assert!(out2.contains("stub-hi"), "续聊桩回包未进正文:\n{out2}");
    let got2 = seen.lock().unwrap();
    assert!(
        got2.iter().all(|h| h == &sid),
        "续聊后亲和头仍应等于同会话 id {sid}：{got2:?}"
    );
}

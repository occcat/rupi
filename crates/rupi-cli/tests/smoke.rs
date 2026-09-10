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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rupi"));
    cmd.env("RUPI_HOME", home)
        .current_dir(home)
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
    ] {
        assert!(out.contains(sub), "help 缺子命令 {sub}:\n{out}");
    }
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
fn chat_quit_exits_zero() {
    let home = fresh_home();
    let o = chat_with(&home, &[], b"/quit\n");
    assert!(o.status.success(), "chat /quit 非零退出: {o:?}");
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

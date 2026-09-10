use std::sync::{Arc, Mutex, OnceLock};

use rupi_ai::{FauxProvider, FauxScript};
use rupi_coding_agent::config::ConfigPaths;
use rupi_coding_agent::harness::{resolve_model, Harness, HarnessOptions};
use rupi_coding_agent::tools::create_coding_tools;
use rupi_coding_agent::AgentSettings;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[tokio::test]
async fn print_mode_with_faux() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("hello.txt"), "hi").unwrap();

    let mut options = HarnessOptions {
        paths: ConfigPaths::resolve(&cwd),
        settings: AgentSettings {
            memory_enabled: true,
            skill_accumulation: true,
            ..AgentSettings::default()
        },
        model: resolve_model("faux/faux"),
        extra_tools: vec![],
        exclude_tools: vec![],
        faux: Some(Arc::new(FauxProvider::new([FauxScript::text(
            "I see hello.txt",
        )]))),
        live: None,
        compact: true,
        accumulate: true,
    };
    options.paths.cwd = cwd;

    let mut harness = Harness::bootstrap(options).await.unwrap();
    let out = harness.print("what files?").await.unwrap();
    assert!(out.text.contains("hello.txt"));
    assert!(out.events.iter().any(|e| e.kind() == "agent_end"));
}

#[tokio::test]
async fn tools_read_write_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = create_coding_tools(
        tmp.path().to_path_buf(),
        &["read".into(), "write".into(), "edit".into()],
        true,
    );
    let write = tools.get("write").unwrap();
    write
        .execute(
            "1",
            serde_json::json!({"path": "a.txt", "content": "alpha"}),
        )
        .await;
    let edit = tools.get("edit").unwrap();
    let r = edit
        .execute(
            "2",
            serde_json::json!({"path": "a.txt", "old_text": "alpha", "new_text": "beta"}),
        )
        .await;
    assert!(!r.is_error);
    let read = tools.get("read").unwrap();
    let r = read
        .execute("3", serde_json::json!({"path": "a.txt"}))
        .await;
    assert!(r.text.contains("beta"));
}

#[tokio::test]
async fn sandbox_blocks_outside_path() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = create_coding_tools(tmp.path().to_path_buf(), &["read".into()], true);
    let read = tools.get("read").unwrap();
    let r = read
        .execute("1", serde_json::json!({"path": "/etc/passwd"}))
        .await;
    assert!(r.is_error);
}

#[tokio::test]
async fn skill_self_accumulate_from_remember() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut options = HarnessOptions {
        paths: ConfigPaths::resolve(&cwd),
        settings: AgentSettings::default(),
        model: resolve_model("faux/faux"),
        extra_tools: vec![],
        exclude_tools: vec![],
        faux: Some(Arc::new(FauxProvider::new([FauxScript::text("ok")]))),
        live: None,
        compact: false,
        accumulate: true,
    };
    options.paths.cwd = cwd;
    let mut harness = Harness::bootstrap(options).await.unwrap();
    let out = harness
        .print("Remember that the staging SSH port is 2222")
        .await
        .unwrap();
    let acc = out.accumulation.expect("accumulation");
    assert!(!acc.memories.is_empty());
}

#[test]
fn upstream_constant() {
    assert!(rupi_coding_agent::UPSTREAM.contains("0.85.1"));
}

fn which_python() -> Option<String> {
    for cand in ["python3", "python"] {
        if std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys; sys.exit(0)")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(cand.to_string());
        }
    }
    None
}

fn mock_mcp_script() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../rupi-mcp/tests/fixtures/mock_mcp.py")
}

async fn boot_faux(cwd: &std::path::Path, scripts: Vec<FauxScript>) -> Harness {
    let mut options = HarnessOptions {
        paths: ConfigPaths::resolve(cwd),
        settings: AgentSettings::default(),
        model: resolve_model("faux/faux"),
        extra_tools: vec![],
        exclude_tools: vec![],
        faux: Some(Arc::new(FauxProvider::new(scripts))),
        live: None,
        compact: false,
        accumulate: true,
    };
    options.paths.cwd = cwd.to_path_buf();
    Harness::bootstrap(options).await.unwrap()
}

#[tokio::test]
async fn project_memory_in_frozen_prompt_and_tool() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    let mem = cwd.join(".rupi").join("memories");
    std::fs::create_dir_all(&mem).unwrap();
    std::fs::write(mem.join("PROJECT.md"), "repo uses nextest").unwrap();

    let harness = boot_faux(&cwd, vec![FauxScript::text("ack")]).await;
    let names = harness.agent.tools.names();
    assert!(names.contains(&"memory".to_string()));
    assert!(names.contains(&"session_search".to_string()));
    assert!(names.contains(&"subagent".to_string()));

    let mem_tool = harness.agent.tools.get("memory").unwrap();
    let block = harness
        .memory
        .as_ref()
        .unwrap()
        .frozen_prompt_block();
    assert!(block.contains("repo uses nextest"));
    assert!(block.contains("PROJECT"));

    let r = mem_tool
        .execute(
            "1",
            serde_json::json!({
                "action": "add",
                "target": "project",
                "content": "CI image is rust:1.88"
            }),
        )
        .await;
    assert!(!r.is_error);
    assert!(cwd.join(".rupi/memories/PROJECT.md").exists());
    let on_disk = std::fs::read_to_string(cwd.join(".rupi/memories/PROJECT.md")).unwrap();
    assert!(on_disk.contains("CI image is rust:1.88"));
    // frozen snapshot stays put
    assert!(!harness
        .memory
        .as_ref()
        .unwrap()
        .frozen_prompt_block()
        .contains("CI image"));
}

#[tokio::test]
async fn session_search_indexes_turns_and_repl_search() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut harness = boot_faux(
        &cwd,
        vec![
            FauxScript::text("noted the zebra"),
            FauxScript::tool(
                "session_search",
                serde_json::json!({"query": "unique-zebra-token"}),
            ),
            FauxScript::text("search done"),
        ],
    )
    .await;
    harness
        .print("please index unique-zebra-token for later")
        .await
        .unwrap();

    match harness.handle_repl_line("/search unique-zebra-token").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Printed(text) => {
            assert!(text.contains("unique-zebra-token"), "{text}");
        }
        other => panic!("{other:?}"),
    }

    let out = harness.print("search now").await.unwrap();
    let joined = out
        .messages
        .iter()
        .map(|m| match m {
            rupi_ai::Message::ToolResult { content, .. } => rupi_ai::content_text(content),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("unique-zebra-token"), "{joined}");
}

#[tokio::test]
async fn subagent_tool_returns_isolated_summary() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("readme.md"), "hello subagent").unwrap();

    let mut harness = boot_faux(
        &cwd,
        vec![
            FauxScript::tool("subagent", serde_json::json!({"task": "summarize readme"})),
            FauxScript::text("child saw readme"),
            FauxScript::text("parent wrapped"),
        ],
    )
    .await;
    let out = harness.print("delegate").await.unwrap();
    let tool_text = out
        .messages
        .iter()
        .find_map(|m| match m {
            rupi_ai::Message::ToolResult { content, is_error, .. } if !*is_error => {
                Some(rupi_ai::content_text(content))
            }
            _ => None,
        })
        .unwrap_or_default();
    assert!(tool_text.contains("child saw readme"), "{tool_text}");
    assert!(out.text.contains("parent wrapped"));
}

#[tokio::test]
async fn permission_gate_blocks_destructive_bash() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    std::env::remove_var("RUPI_ALLOW_DESTRUCTIVE");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut harness = boot_faux(
        &cwd,
        vec![
            FauxScript::tool(
                "bash",
                serde_json::json!({"command": "rm -rf /tmp/rupi-should-not"}),
            ),
            FauxScript::text("after block"),
        ],
    )
    .await;
    let out = harness.print("wipe").await.unwrap();
    let err = out.messages.iter().any(|m| match m {
        rupi_ai::Message::ToolResult { is_error, content, .. } => {
            *is_error && rupi_ai::content_text(content).contains("destructive")
        }
        _ => false,
    });
    assert!(err, "expected destructive bash to be blocked");
}

#[tokio::test]
async fn repl_commands_help_memory_mcp_quit() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut harness = boot_faux(&cwd, vec![FauxScript::text("hi")]).await;

    match harness.handle_repl_line("/help").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Printed(t) => {
            assert!(t.contains("/mcp"));
            assert!(t.contains("/skill"));
            assert!(t.contains("/search"));
        }
        other => panic!("{other:?}"),
    }
    match harness.handle_repl_line("/memory").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Printed(t) => {
            assert!(t.contains("PROJECT"));
            assert!(t.contains("USER PROFILE"));
        }
        other => panic!("{other:?}"),
    }
    match harness.handle_repl_line("/mcp").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Printed(t) => {
            assert!(t.contains("no MCP tools") || t.contains("mcp_"));
        }
        other => panic!("{other:?}"),
    }
    match harness.handle_repl_line("/session").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Printed(t) => assert!(t.contains("session")),
        other => panic!("{other:?}"),
    }
    match harness.handle_repl_line("/quit").await.unwrap() {
        rupi_coding_agent::ReplOutcome::Quit => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn harness_loads_stdio_mcp_from_mcp_json() {
    let _g = env_lock();
    let python = match which_python() {
        Some(p) => p,
        None => return,
    };
    let script = mock_mcp_script();
    assert!(script.exists(), "missing {script:?}");

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("RUPI_HOME", tmp.path());
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        tmp.path().join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "mock": {
                    "command": python,
                    "args": [script.display().to_string()]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let mut harness = boot_faux(
        &cwd,
        vec![
            FauxScript::tool(
                "mcp_mock_echo",
                serde_json::json!({"text": "wired-through-harness"}),
            ),
            FauxScript::text("mcp done"),
        ],
    )
    .await;
    assert!(
        harness
            .mcp_tool_names
            .iter()
            .any(|n| n == "mcp_mock_echo"),
        "mcp tools: {:?}",
        harness.mcp_tool_names
    );
    let out = harness.print("call echo").await.unwrap();
    let tool_text = out
        .messages
        .iter()
        .find_map(|m| match m {
            rupi_ai::Message::ToolResult { content, .. } => Some(rupi_ai::content_text(content)),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        tool_text.contains("wired-through-harness"),
        "tool result: {tool_text}"
    );
}

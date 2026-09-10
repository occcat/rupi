use std::sync::Arc;

use rupi_ai::{FauxProvider, FauxScript};
use rupi_coding_agent::config::ConfigPaths;
use rupi_coding_agent::harness::{resolve_model, Harness, HarnessOptions};
use rupi_coding_agent::tools::create_coding_tools;
use rupi_coding_agent::AgentSettings;

#[tokio::test]
async fn print_mode_with_faux() {
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

    let mut harness = Harness::bootstrap(options).unwrap();
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
    let mut harness = Harness::bootstrap(options).unwrap();
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

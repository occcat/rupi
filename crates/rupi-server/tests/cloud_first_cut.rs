//! 云第一刀验收：双租户隔离、AG-UI 流式 + interrupt、write/bash 只经 Executor。
//! 无 DATABASE_URL 时跳过（macOS CI）；Ubuntu cloud job 会带 Postgres/Redis。

use rupi_core::ContentBlock;
use rupi_llm::{ChatResponse, MockProvider};
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::{spawn, App, Cache};
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// 共享本机/CI 的 Postgres，验收用例互斥。
static HARNESS_LOCK: Mutex<()> = Mutex::const_new(());

fn env_urls() -> Option<(String, String)> {
    let db = std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| Some("postgresql://rupi:rupi@127.0.0.1:5432/rupi".into()));
    let redis = std::env::var("REDIS_URL")
        .ok()
        .or_else(|| Some("redis://127.0.0.1:6379".into()));
    match (db, redis) {
        (Some(d), Some(r)) => Some((d, r)),
        _ => None,
    }
}

async fn ping_deps(db_url: &str, redis_url: &str) -> bool {
    if db::connect(db_url).await.is_err() {
        eprintln!("skip: no postgres at {db_url}");
        return false;
    }
    match redis::Client::open(redis_url) {
        Ok(c) => c.get_connection().is_ok(),
        Err(_) => false,
    }
}

struct Harness {
    base: String,
    execd_root: std::path::PathBuf,
    exec_url: String,
    redis_url: String,
    key_a: String,
    key_b: String,
    tenant_a: String,
    #[allow(dead_code)]
    tenant_b: String,
    pool: db::PgPool,
    _server: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let guard = HARNESS_LOCK.lock().await;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect(&db_url).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key_a = auth::generate_key();
        let key_b = auth::generate_key();
        let ta = db::create_tenant(&pool, "alpha", &key_a).await.ok()?;
        let tb = db::create_tenant(&pool, "beta", &key_b).await.ok()?;
        db::update_settings(
            &pool,
            &ta.id,
            &json!({
                "provider": "mock",
                "mock_script": mock_script()
            }),
        )
        .await
        .ok()?;
        db::update_settings(
            &pool,
            &tb.id,
            &json!({"provider": "mock"}),
        )
        .await
        .ok()?;

        let root = std::env::temp_dir().join(format!("rupi-execd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "exec-secret".into(),
        })
        .await
        .ok()?;
        let exec_url = format!("http://{exec_addr}");
        let cache = Cache::connect(&redis_url).await;
        let app = App {
            pool: pool.clone(),
            cache,
            executor: Arc::new(rupi_runtime::http::HttpExecutor::new(
                exec_url.clone(),
                "exec-secret",
            )),
            provider_factory: Arc::new(|t| rupi_server::run::default_provider(t)),
        };
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        Some(Self {
            base: format!("http://{addr}"),
            execd_root: root,
            exec_url,
            redis_url,
            key_a,
            key_b,
            tenant_a: ta.id,
            tenant_b: tb.id,
            pool,
            _server: srv,
            _execd: exec_h,
            _guard: guard,
        })
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }
}

fn mock_script() -> Value {
    let write = ChatResponse {
        message: rupi_core::Message {
            id: "a".into(),
            role: rupi_core::Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "tc-w".into(),
                name: "write".into(),
                arguments: json!({"path": "hello.txt", "content": "cloud-ok"}),
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        },
        stop_reason: "tool_calls".into(),
    };
    let text1 = MockProvider::text_response("stream-hello-from-cloud");
    let text2 = MockProvider::text_response("wrote-via-executor");
    let memory = ChatResponse {
        message: rupi_core::Message {
            id: "m".into(),
            role: rupi_core::Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "tc-mem".into(),
                name: "memory".into(),
                arguments: json!({"op":"add","entry":"[core] likes cloud-tea","scope":"tenant"}),
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        },
        stop_reason: "tool_calls".into(),
    };
    let text3 = MockProvider::text_response("remembered-in-postgres");
    let text4 = MockProvider::text_response("still-here-after-cache-flush");
    json!([text1, write, text2, memory, text3, text4])
}

fn parse_sse(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                out.push(v);
            }
        }
    }
    out
}

async fn post_agent(h: &Harness, key: &str, body: Value) -> (u16, String) {
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(key)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, text)
}

#[tokio::test]
async fn two_tenants_isolated_and_agui_interrupt() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let c = h.client();

    // /me
    let me_a = c
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&h.key_a)
        .send()
        .await
        .unwrap();
    assert_eq!(me_a.status(), 200);
    let me_a: Value = me_a.json().await.unwrap();
    assert_eq!(me_a["name"], "alpha");

    let me_bad = c
        .get(format!("{}/v1/me", h.base))
        .bearer_auth("rupi_not_a_real_key")
        .send()
        .await
        .unwrap();
    assert_eq!(me_bad.status(), 401);

    // 建会话（向 Executor 申请卷）
    let created = c
        .post(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.key_a)
        .json(&json!({"name": "thread-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{}", created.text().await.unwrap());
    let sess: Value = created.json().await.unwrap();
    let sid = sess["id"].as_str().unwrap().to_string();
    assert_eq!(sess["runtime"]["backend"], "remote-http");
    assert!(sess["runtime"]["handle"].as_str().unwrap().len() > 4);

    // B 看不见 A 的会话
    let sneak = c
        .get(format!("{}/v1/sessions/{sid}", h.base))
        .bearer_auth(&h.key_b)
        .send()
        .await
        .unwrap();
    assert!(
        sneak.status() == 403 || sneak.status() == 404,
        "{}",
        sneak.status()
    );

    let list_b = c
        .get(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.key_b)
        .send()
        .await
        .unwrap();
    let list_b: Value = list_b.json().await.unwrap();
    assert_eq!(list_b["sessions"].as_array().unwrap().len(), 0);

    // 第一轮：流式文本（mock 第一条）
    let (st, body) = post_agent(
        &h,
        &h.key_a,
        json!({
            "threadId": sid,
            "runId": "run-1",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let evs = parse_sse(&body);
    let types: Vec<&str> = evs
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect();
    assert!(types.contains(&"RUN_STARTED"), "{types:?}");
    assert!(
        types.contains(&"TEXT_MESSAGE_CONTENT") || types.contains(&"TEXT_MESSAGE_START"),
        "{types:?}\n{body}"
    );
    assert!(types.contains(&"RUN_FINISHED"), "{types:?}");
    assert!(!types.contains(&"CUSTOM") && !types.contains(&"RAW"));
    let finished = evs.iter().find(|e| e["type"] == "RUN_FINISHED").unwrap();
    assert_eq!(finished["outcome"]["type"], "success");

    // 第二轮：write → interrupt（尚未改卷）
    let (st, body) = post_agent(
        &h,
        &h.key_a,
        json!({
            "threadId": sid,
            "runId": "run-2",
            "messages": [{"id":"u2","role":"user","content":"please write a file"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let evs = parse_sse(&body);
    let types: Vec<&str> = evs
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect();
    assert!(types.contains(&"TOOL_CALL_START"), "{types:?}\n{body}");
    assert!(types.contains(&"TOOL_CALL_END"), "{types:?}");
    assert!(!types.contains(&"TOOL_CALL_RESULT"), "{types:?}");
    let fin = evs.iter().find(|e| e["type"] == "RUN_FINISHED").unwrap();
    assert_eq!(fin["outcome"]["type"], "interrupt");
    let iid = fin["outcome"]["interrupts"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 卷上还没有文件
    let hits = walk_named(&h.execd_root, "hello.txt");
    assert!(hits.is_empty(), "must not write before approve: {hits:?}");

    // 批准后才经 Executor 写
    let (st, body) = post_agent(
        &h,
        &h.key_a,
        json!({
            "threadId": sid,
            "runId": "run-3",
            "messages": [],
            "resume": [{
                "interruptId": iid,
                "status": "resolved",
                "payload": {"approved": true}
            }]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let evs = parse_sse(&body);
    let types: Vec<&str> = evs
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect();
    assert!(types.contains(&"TOOL_CALL_RESULT"), "{types:?}\n{body}");
    assert!(!types.contains(&"TOOL_CALL_START"), "resume must not re-emit start");
    let fin = evs.iter().find(|e| e["type"] == "RUN_FINISHED").unwrap();
    assert_eq!(fin["outcome"]["type"], "success");

    let hits = walk_named(&h.execd_root, "hello.txt");
    assert_eq!(hits.len(), 1, "{hits:?}");
    let content = std::fs::read_to_string(&hits[0]).unwrap();
    assert!(content.contains("cloud-ok"), "{content}");

    // 记忆工具写进 Postgres（不是 sandbox MEMORY.md）
    let (st, body) = post_agent(
        &h,
        &h.key_a,
        json!({
            "threadId": sid,
            "runId": "run-4",
            "messages": [{"id":"u4","role":"user","content":"remember my tea"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let hits_a = db::search_memories(&h.pool, &h.tenant_a, "cloud-tea", 10)
        .await
        .unwrap();
    assert!(!hits_a.is_empty(), "{hits_a:?}");
    let md_hits = walk_named(&h.execd_root, "MEMORY.md");
    assert!(
        md_hits.is_empty(),
        "sandbox MEMORY.md must not be the authority: {md_hits:?}"
    );

    // B 搜不到 A 的记忆
    let created_b = c
        .post(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.key_b)
        .json(&json!({"name": "thread-b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created_b.status(), 201);
    let sid_b = created_b.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let hits_b = db::search_memories(&h.pool, &{
        let t = db::tenant_by_key_hash(&h.pool, &auth::hash_key(&h.key_b))
            .await
            .unwrap()
            .unwrap();
        t.id
    }, "cloud-tea", 10)
    .await
    .unwrap();
    assert!(hits_b.is_empty(), "{hits_b:?}");

    // fork / export
    let forked = c
        .post(format!("{}/v1/sessions/{sid}/fork", h.base))
        .bearer_auth(&h.key_a)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(forked.status(), 200, "{}", forked.text().await.unwrap());
    let exp = c
        .get(format!("{}/v1/sessions/{sid}/export", h.base))
        .bearer_auth(&h.key_a)
        .send()
        .await
        .unwrap();
    assert_eq!(exp.status(), 200);
    let jsonl = exp.text().await.unwrap();
    assert!(jsonl.contains("\"type\":\"session\"") || jsonl.contains("message"), "{jsonl}");

    // 句柄配额
    {
        let conn = h.pool.get().await.unwrap();
        conn.execute(
            "UPDATE tenants SET max_handles = 2 WHERE id = $1",
            &[&h.tenant_a],
        )
        .await
        .unwrap();
    }
    let overflow = c
        .post(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.key_a)
        .json(&json!({"name": "too-many"}))
        .send()
        .await
        .unwrap();
    assert_eq!(overflow.status(), 429, "{}", overflow.text().await.unwrap());

    // 丢 Redis 缓存后仍从 Postgres 续聊（FLUSHDB，不杀共享进程）
    let _ = Command::new("redis-cli")
        .args(["-u", &h.redis_url, "FLUSHDB"])
        .status();
    let disabled = App {
        pool: h.pool.clone(),
        cache: Cache::disabled(),
        executor: Arc::new(rupi_runtime::http::HttpExecutor::new(
            h.exec_url.clone(),
            "exec-secret",
        )),
        provider_factory: Arc::new(|t| rupi_server::run::default_provider(t)),
    };
    let (addr2, _srv2) = spawn(disabled, "127.0.0.1:0").await.unwrap();
    let (st, body) = {
        let resp = reqwest::Client::new()
            .post(format!("http://{addr2}/v1/agent"))
            .bearer_auth(&h.key_a)
            .header("Accept", "text/event-stream")
            .json(&json!({
                "threadId": sid,
                "runId": "run-5",
                "messages": [{"id":"u5","role":"user","content":"still there?"}]
            }))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap_or_default())
    };
    assert_eq!(st, 200, "{body}");
    let evs = parse_sse(&body);
    let types: Vec<&str> = evs
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect();
    assert!(types.contains(&"RUN_FINISHED"), "{types:?}\n{body}");
    assert!(body.contains("still-here-after-cache-flush") || types.contains(&"TEXT_MESSAGE_CONTENT"), "{body}");
    let _ = sid_b;
}

fn walk_named(root: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root);
    for e in walker.into_iter().flatten() {
        if e.file_name() == name {
            out.push(e.path().to_path_buf());
        }
    }
    out
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// 独立进程：API 进程树看不到用户 bash 命令。
#[tokio::test]
async fn api_process_table_does_not_see_user_bash() {
    let _guard = HARNESS_LOCK.lock().await;
    let Some((db_url, redis_url)) = env_urls() else {
        return;
    };
    if !ping_deps(&db_url, &redis_url).await {
        return;
    }
    let pool = db::connect(&db_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let _ = db::reset_all(&pool).await;
    let key = auth::generate_key();
    let t = db::create_tenant(&pool, "proc", &key).await.unwrap();
    let marker = format!("rupi-exec-marker-{}", uuid::Uuid::new_v4());
    let script = json!([
        {
            "type": "tool",
            "id": "c-bash",
            "name": "bash",
            "arguments": { "command": format!("sleep 2; echo {marker}"), "timeout_secs": 10 }
        },
        { "text": "done-bash" }
    ]);
    db::update_settings(
        &pool,
        &t.id,
        &json!({"provider":"mock","mock_script": [
            ChatResponse {
                message: rupi_core::Message {
                    id: "a".into(),
                    role: rupi_core::Role::Assistant,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "c-bash".into(),
                        name: "bash".into(),
                        arguments: json!({"command": format!("sleep 2; echo {marker}"), "timeout_secs": 10}),
                    }],
                    provider: None,
                    created_at: chrono::Utc::now(),
                },
                stop_reason: "tool_calls".into(),
            },
            MockProvider::text_response("done-bash")
        ]}),
    )
    .await
    .unwrap();
    let _ = script;

    let execd_bin = env!("CARGO_BIN_EXE_rupi-execd");
    let server_bin = env!("CARGO_BIN_EXE_rupi-server");
    let root = std::env::temp_dir().join(format!("rupi-execd-proc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let exec_port = free_port();
    let api_port = free_port();

    let mut execd = Command::new(execd_bin)
        .args([
            "--listen",
            &format!("127.0.0.1:{exec_port}"),
            "--root",
            root.to_str().unwrap(),
            "--token",
            "tok",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Command::new(server_bin)
        .env("DATABASE_URL", &db_url)
        .env("REDIS_URL", &redis_url)
        .env("RUPI_EXECUTOR_URL", format!("http://127.0.0.1:{exec_port}"))
        .env("RUPI_EXEC_TOKEN", "tok")
        .env("RUPI_LISTEN", format!("127.0.0.1:{api_port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let api = format!("http://127.0.0.1:{api_port}");
    let mut up = false;
    for _ in 0..40 {
        if reqwest::Client::new()
            .get(format!("{api}/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !up {
        let _ = execd.kill();
        let _ = server.kill();
        panic!("rupi-server did not become healthy on {api}");
    }

    let client = reqwest::Client::new();
    let created = client
        .post(format!("{api}/v1/sessions"))
        .bearer_auth(&key)
        .json(&json!({}))
        .send()
        .await;
    let Ok(created) = created else {
        let _ = execd.kill();
        let _ = server.kill();
        return;
    };
    if created.status() != 201 {
        let _ = execd.kill();
        let _ = server.kill();
        panic!("create session {}", created.status());
    }
    let sid = created.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 先打断再批准，让 bash 跑起来
    let run1 = client
        .post(format!("{api}/v1/agent"))
        .bearer_auth(&key)
        .json(&json!({
            "threadId": sid,
            "runId": "p1",
            "messages": [{"role":"user","content":"run bash"}]
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let evs = parse_sse(&run1);
    let iid = evs
        .iter()
        .find(|e| e["type"] == "RUN_FINISHED")
        .and_then(|e| e["outcome"]["interrupts"][0]["id"].as_str())
        .unwrap_or("")
        .to_string();
    assert!(!iid.is_empty(), "{run1}");

    let server_pid = server.id();
    let join = tokio::spawn({
        let key = key.clone();
        let sid = sid.clone();
        let iid = iid.clone();
        let api = api.clone();
        async move {
            let _ = reqwest::Client::new()
                .post(format!("{api}/v1/agent"))
                .bearer_auth(key)
                .json(&json!({
                    "threadId": sid,
                    "runId": "p2",
                    "messages": [],
                    "resume": [{"interruptId": iid, "status":"resolved", "payload":{"approved": true}}]
                }))
                .send()
                .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let tree = process_cmdline_tree(server_pid);
    let leak = tree.iter().any(|c| c.contains(&marker));
    let _ = join.await;
    let _ = execd.kill();
    let _ = server.kill();
    assert!(
        !leak,
        "API process tree must not contain user command {marker}: {tree:?}"
    );
}

fn process_cmdline_tree(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![pid];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if let Ok(raw) = std::fs::read(format!("/proc/{p}/cmdline")) {
            out.push(String::from_utf8_lossy(&raw).replace('\0', " "));
        }
        if let Ok(children) = std::fs::read_to_string(format!("/proc/{p}/task/{p}/children")) {
            for c in children.split_whitespace() {
                if let Ok(n) = c.parse::<u32>() {
                    stack.push(n);
                }
            }
        }
    }
    out
}

#[test]
fn local_rpc_binary_still_works() {
    let rupi = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/rupi");
    if !rupi.exists() {
        eprintln!("skip rpc check: rupi binary not built yet");
        return;
    }
    let home = std::env::temp_dir().join(format!("rupi-rpc-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(&rupi)
        .args(["--mode", "rpc", "--no-approve"])
        .env("RUPI_HOME", &home)
        .env("HOME", &home)
        .current_dir(&home)
        .env_remove("RUPI_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut sin = child.stdin.take().unwrap();
        writeln!(sin, r#"{{"id":"1","type":"get_state"}}"#).unwrap();
        writeln!(sin, r#"{{"id":"2","type":"prompt","message":"hello"}}"#).unwrap();
    }
    let o = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("get_state") || out.contains("success"), "{out}");
}

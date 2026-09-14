//! P0 / 云 P1 上线回归。无 Postgres / Redis 时跳过（与其他 cloud_* 相同）。

mod common;

use async_trait::async_trait;
use futures::StreamExt;
use rupi_core::{ContentBlock, Message, Role};
use rupi_llm::{ChatRequest, ChatResponse, LlmProvider, MockProvider, StreamEvent};
use rupi_memory::MemoryProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_runtime::MemoryObjectStore;
use rupi_runtime::ObjectStore;
use rupi_server::auth;
use rupi_server::db;
use rupi_server::memory::PostgresMemory;
use rupi_server::{spawn, App, Cache};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::lock_harness;

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

struct SlowCountProvider {
    starts: Arc<AtomicU32>,
}

#[async_trait]
impl LlmProvider for SlowCountProvider {
    fn name(&self) -> &str {
        "slow-count"
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let n = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if n == 1 {
            return Ok(ChatResponse {
                message: Message {
                    id: "a".into(),
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "tc-think".into(),
                        name: "think".into(),
                        arguments: json!({"thought": "hold"}),
                    }],
                    provider: None,
                    created_at: chrono::Utc::now(),
                },
                stop_reason: "tool_calls".into(),
            });
        }
        Ok(MockProvider::text_response("should-not-reach"))
    }
}

struct Harness {
    base: String,
    admin: String,
    key: String,
    tenant: String,
    pool: db::PgPool,
    cache: Cache,
    exec_url: String,
    exec_token: String,
    exec_root: PathBuf,
    _server: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Harness {
    async fn start() -> Option<Self> {
        Self::start_with_factory(Arc::new(|t| rupi_server::run::default_provider(t))).await
    }

    async fn start_with_factory(factory: rupi_server::ProviderFactory) -> Option<Self> {
        let guard = lock_harness().await?;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect(&db_url).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key = auth::generate_key();
        let t = db::create_tenant(&pool, "harden-a", &key).await.ok()?;
        let root = std::env::temp_dir().join(format!("rupi-harden-execd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "exec-secret".into(),
            ..Default::default()
        })
        .await
        .ok()?;
        let exec_url = format!("http://{exec_addr}");
        let cache = Cache::connect(&redis_url).await;
        let admin = "harden-admin".to_string();
        let app = App::new(
            pool.clone(),
            cache.clone(),
            Arc::new(rupi_runtime::http::HttpExecutor::new(
                exec_url.clone(),
                "exec-secret",
            )),
            factory,
        )
        .with_admin_token(admin.clone());
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            base: format!("http://{addr}"),
            admin,
            key,
            tenant: t.id,
            pool,
            cache,
            exec_url,
            exec_token: "exec-secret".into(),
            exec_root: root,
            _server: srv,
            _execd: exec_h,
            _guard: guard,
        })
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap()
    }

    async fn create_session(&self) -> String {
        self.create_session_ready().await.0
    }

    async fn create_session_ready(&self) -> (String, String) {
        let resp = self
            .client()
            .post(format!("{}/v1/sessions", self.base))
            .bearer_auth(&self.key)
            .json(&json!({"name": "harden"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
        let v: Value = resp.json().await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let handle = v["runtime"]["handle"].as_str().unwrap_or("").to_string();
        assert!(!handle.is_empty(), "{v}");
        (id, handle)
    }

    fn workspace_path(&self, session: &str, handle: &str, rel: &str) -> PathBuf {
        self.exec_root
            .join(&self.tenant)
            .join(session)
            .join(handle)
            .join(rel)
    }

    async fn exec_write(&self, handle: &str, path: &str, content: &str) {
        let resp = self
            .client()
            .post(format!("{}/v1/fs/write", self.exec_url))
            .bearer_auth(&self.exec_token)
            .json(&json!({
                "handle": handle,
                "tenant_id": self.tenant,
                "path": path,
                "content": content
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_drop_cancels_before_second_model_call() {
    let starts = Arc::new(AtomicU32::new(0));
    let starts_c = starts.clone();
    let Some(h) = Harness::start_with_factory(Arc::new(move |_t| {
        Arc::new(SlowCountProvider {
            starts: starts_c.clone(),
        })
    }))
    .await
    else {
        return;
    };
    let sid = h.create_session().await;
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "threadId": sid,
            "runId": "run-cancel",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.status());
    let mut stream = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
    assert!(first.is_ok(), "expected first SSE chunk");
    drop(stream);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let n = starts.load(Ordering::SeqCst);
    assert!(
        n <= 1,
        "SSE drop must cancel AgentLoop before the second model call; starts={n}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_settings_whitelist_and_mock_script_forbidden() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let unknown = h
        .client()
        .patch(format!("{}/v1/settings", h.base))
        .bearer_auth(&h.key)
        .json(&json!({"oauthClientId": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400, "{}", unknown.text().await.unwrap());

    let mock = h
        .client()
        .patch(format!("{}/v1/settings", h.base))
        .bearer_auth(&h.key)
        .json(&json!({"mock_script": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(mock.status(), 403, "{}", mock.text().await.unwrap());

    let ok = h
        .client()
        .patch(format!("{}/v1/settings", h.base))
        .bearer_auth(&h.key)
        .json(&json!({"model": "gpt-4o-mini"}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "{}", ok.text().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daily_token_quota_returns_429() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let sid = h.create_session().await;
    db::patch_tenant_quota(&h.pool, &h.tenant, None, None, None, Some(1), None)
        .await
        .unwrap();
    db::bump_quota_tokens(&h.pool, &h.tenant, 1).await.unwrap();
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .json(&json!({
            "threadId": sid,
            "runId": "run-quota",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "{}", resp.text().await.unwrap());
    let metrics = h
        .client()
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("rupi_rejects_429_total"), "{metrics}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_replace_and_remove_mutate_rows() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let sid = h.create_session().await;
    let mem = PostgresMemory::new(
        h.pool.clone(),
        h.tenant.clone(),
        sid.clone(),
        h.cache.clone(),
    );
    mem.handle_tool_call(
        "memory",
        json!({"op":"add","entry":"alpha-fact","scope":"tenant"}),
    )
    .await
    .unwrap();
    let rows = db::list_memories(&h.pool, &h.tenant, Some(&sid))
        .await
        .unwrap();
    assert!(
        rows.iter().any(|(_, _, c)| c.contains("alpha-fact")),
        "{rows:?}"
    );
    mem.handle_tool_call("memory", json!({"op":"replace","entry":"beta-fact"}))
        .await
        .unwrap();
    let rows = db::list_memories(&h.pool, &h.tenant, Some(&sid))
        .await
        .unwrap();
    assert!(
        rows.iter().any(|(_, _, c)| c.contains("beta-fact")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|(_, _, c)| c.contains("alpha-fact")),
        "replace must update the row, not append: {rows:?}"
    );
    mem.handle_tool_call("memory", json!({"op":"remove","entry":"beta-fact"}))
        .await
        .unwrap();
    let rows = db::list_memories(&h.pool, &h.tenant, Some(&sid))
        .await
        .unwrap();
    assert!(
        !rows.iter().any(|(_, _, c)| c.contains("beta-fact")),
        "remove must delete the row: {rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_delete_tenant_and_ready_metrics() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let ready = h
        .client()
        .get(format!("{}/ready", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    let ready: Value = ready.json().await.unwrap();
    assert!(ready.get("nodes").is_some(), "{ready}");
    assert_eq!(ready["ok"], true);

    let metrics = h
        .client()
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("rupi_runs_total"), "{metrics}");

    let del = h
        .client()
        .delete(format!("{}/admin/api/tenants/{}", h.base, h.tenant))
        .bearer_auth(&h.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204, "{}", del.text().await.unwrap());
    let gone = db::load_tenant(&h.pool, &h.tenant).await.unwrap();
    assert!(gone.is_none());
    let me = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 401);
}

#[tokio::test]
async fn memory_object_store_shared_across_clones() {
    let a = MemoryObjectStore::new();
    let b = a.clone();
    a.put("ws/t/s/h.tgz", b"blob").await.unwrap();
    assert_eq!(b.get("ws/t/s/h.tgz").await.unwrap(), b"blob");
}

struct BashSleepProvider {
    command: String,
}

#[async_trait]
impl LlmProvider for BashSleepProvider {
    fn name(&self) -> &str {
        "bash-sleep"
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            message: rupi_core::Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "tc-bash".into(),
                    name: "bash".into(),
                    arguments: json!({"command": self.command, "timeout_secs": 120}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        })
    }
}

struct UsageProvider {
    input: u64,
    output: u64,
}

#[async_trait]
impl LlmProvider for UsageProvider {
    fn name(&self) -> &str {
        "usage"
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatResponse> {
        Ok(MockProvider::text_response("usage-ok"))
    }

    async fn complete_streaming(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<ChatResponse> {
        let resp = self.complete(req).await?;
        let _ = tx
            .send(StreamEvent::Usage {
                input: self.input,
                output: self.output,
            })
            .await;
        Ok(resp)
    }
}

fn parse_sse_events(raw: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for chunk in raw.split("\n\n") {
        for line in chunk.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                out.push(v);
            }
        }
    }
    out
}

fn process_cmdlines_contain(needle: &str) -> bool {
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str() else {
                continue;
            };
            if !pid.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else {
                continue;
            };
            if String::from_utf8_lossy(&cmd).contains(needle) {
                return true;
            }
        }
    }
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-ax", "-o", "command="])
        .output()
    {
        return String::from_utf8_lossy(&out.stdout).contains(needle);
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_drop_aborts_inflight_bash() {
    let token = format!("rupi-stop-{}", uuid::Uuid::new_v4().simple());
    let command = format!(
        "echo started > started.txt; while [ ! -f {token} ]; do sleep 1; done; echo survived > survived.txt"
    );
    let Some(h) = Harness::start_with_factory({
        let command = command.clone();
        Arc::new(move |_t| {
            Arc::new(BashSleepProvider {
                command: command.clone(),
            })
        })
    })
    .await
    else {
        return;
    };
    let (sid, handle) = h.create_session_ready().await;
    let first = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .json(&json!({
            "threadId": sid,
            "runId": "run-bash-1",
            "messages": [{"id":"u1","role":"user","content":"run"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200, "{}", first.status());
    let body = first.text().await.unwrap();
    let evs = parse_sse_events(&body);
    let iid = evs
        .iter()
        .find(|e| e["type"] == "RUN_FINISHED")
        .and_then(|e| e["outcome"]["interrupts"][0]["id"].as_str())
        .unwrap_or("")
        .to_string();
    assert!(!iid.is_empty(), "{body}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let resp = client
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "threadId": sid,
            "runId": "run-bash-2",
            "messages": [],
            "resume": [{
                "interruptId": iid,
                "status": "resolved",
                "payload": {"approved": true}
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.status());
    let mut stream = resp.bytes_stream();
    let started = h.workspace_path(&sid, &handle, "started.txt");
    let mut saw = false;
    for _ in 0..80 {
        let _ = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if started.exists() {
            saw = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saw, "bash must start before cancel; missing {started:?}");
    assert!(
        process_cmdlines_contain(&token),
        "expected in-flight bash with {token}"
    );
    drop(stream);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !process_cmdlines_contain(&token),
        "SSE drop must abort/kill the in-flight bash ({token})"
    );
    assert!(
        !h.workspace_path(&sid, &handle, "survived.txt").exists(),
        "killed bash must not write survived.txt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_loop_reads_agents_and_skills_via_executor() {
    let mock = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "saw-workspace",
    )]));
    let mock_c = mock.clone();
    let Some(h) = Harness::start_with_factory(Arc::new(move |_t| mock_c.clone())).await else {
        return;
    };
    let (sid, handle) = h.create_session_ready().await;
    h.exec_write(
        &handle,
        "AGENTS.md",
        "workspace-agents-marker-9f3c: always mention tea",
    )
    .await;
    h.exec_write(
        &handle,
        "skills/launch-guide/SKILL.md",
        "---\nname: launch-guide\ndescription: launch checklist helper for workspace tests\n---\nFollow the launch checklist.\n",
    )
    .await;
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .json(&json!({
            "threadId": sid,
            "runId": "run-agents",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let systems = mock.seen_systems.lock().unwrap().clone();
    let joined = systems.join("\n");
    assert!(
        joined.contains("workspace-agents-marker-9f3c"),
        "system must include Executor AGENTS.md: {joined}"
    );
    assert!(
        joined.contains("launch-guide"),
        "system must include ingested SKILL.md: {joined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_event_increments_tokens_today() {
    let Some(h) = Harness::start_with_factory(Arc::new(|_t| {
        Arc::new(UsageProvider {
            input: 80,
            output: 20,
        })
    }))
    .await
    else {
        return;
    };
    let before = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    assert_eq!(before.status(), 200);
    let before: Value = before.json().await.unwrap();
    let start = before["quota"]["tokensToday"].as_i64().unwrap_or(0);
    let sid = h.create_session().await;
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .json(&json!({
            "threadId": sid,
            "runId": "run-usage",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let after = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    let after: Value = after.json().await.unwrap();
    let end = after["quota"]["tokensToday"].as_i64().unwrap_or(0);
    assert_eq!(end, start + 100, "before={before} after={after}");
}

#[tokio::test]
async fn ready_is_503_when_executor_unreachable() {
    let Some(_guard) = lock_harness().await else {
        return;
    };
    let Some((db_url, redis_url)) = env_urls() else {
        return;
    };
    if !ping_deps(&db_url, &redis_url).await {
        return;
    }
    let pool = db::connect(&db_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let cache = Cache::connect(&redis_url).await;
    let app = App::new(
        pool,
        cache,
        Arc::new(rupi_runtime::http::HttpExecutor::new(
            "http://127.0.0.1:9",
            "tok",
        )),
        Arc::new(|t| rupi_server::run::default_provider(t)),
    );
    let (addr, _srv) = spawn(app, "127.0.0.1:0").await.unwrap();
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "{}", resp.status());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["postgres"], true, "{body}");
}

#[tokio::test]
async fn ready_is_503_when_executor_pool_empty() {
    let Some(_guard) = lock_harness().await else {
        return;
    };
    let Some((db_url, redis_url)) = env_urls() else {
        return;
    };
    if !ping_deps(&db_url, &redis_url).await {
        return;
    }
    let pool = db::connect(&db_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let cache = Cache::connect(&redis_url).await;
    let app = App::new(
        pool,
        cache,
        Arc::new(rupi_runtime::PoolScheduler::new(Vec::new())),
        Arc::new(|t| rupi_server::run::default_provider(t)),
    );
    let (addr, _srv) = spawn(app, "127.0.0.1:0").await.unwrap();
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false, "{body}");
}

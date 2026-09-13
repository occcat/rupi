//! P0 / 云 P1 上线回归。无 Postgres / Redis 时跳过（与其他 cloud_* 相同）。

mod common;

use async_trait::async_trait;
use futures::StreamExt;
use rupi_core::{ContentBlock, Message, Role};
use rupi_llm::{ChatRequest, ChatResponse, LlmProvider, MockProvider};
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_runtime::MemoryObjectStore;
use rupi_runtime::ObjectStore;
use rupi_memory::MemoryProvider;
use rupi_server::auth;
use rupi_server::db;
use rupi_server::memory::PostgresMemory;
use rupi_server::{spawn, App, Cache};
use serde_json::{json, Value};
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
            root,
            token: "exec-secret".into(),
            ..Default::default()
        })
        .await
        .ok()?;
        let cache = Cache::connect(&redis_url).await;
        let admin = "harden-admin".to_string();
        let app = App::new(
            pool.clone(),
            cache.clone(),
            Arc::new(rupi_runtime::http::HttpExecutor::new(
                format!("http://{exec_addr}"),
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
        v["id"].as_str().unwrap().to_string()
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
    db::bump_quota_tokens(&h.pool, &h.tenant, 1)
        .await
        .unwrap();
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
    assert!(
        metrics.contains("rupi_rejects_429_total"),
        "{metrics}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_replace_and_remove_mutate_rows() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let sid = h.create_session().await;
    let mem = PostgresMemory::new(h.pool.clone(), h.tenant.clone(), sid.clone(), h.cache.clone());
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

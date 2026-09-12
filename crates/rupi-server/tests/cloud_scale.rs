//! 扩容刀：多副本租约、池耗尽、闲置快照、配额对账、Redis 降级。
//! 无 DATABASE_URL / 依赖不可达时跳过（与第一刀相同）。

mod common;

use common::lock_harness;
use rupi_llm::MockProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_runtime::{LocalObjectStore, ObjectStore};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::quota;
use rupi_server::{reclaim, spawn, App, Cache};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

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

struct Dual {
    a: String,
    b: String,
    key: String,
    tenant: String,
    pool: db::PgPool,
    cache: Cache,
    app_a: App,
    app_b: App,
    execd_root: std::path::PathBuf,
    _srv_a: tokio::task::JoinHandle<anyhow::Result<()>>,
    _srv_b: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Dual {
    async fn start(max_workspaces: u32) -> Option<Self> {
        let guard = lock_harness().await?;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect(&db_url).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key = auth::generate_key();
        let t = db::create_tenant(&pool, "scale", &key).await.ok()?;
        db::update_settings(
            &pool,
            &t.id,
            &json!({
                "provider": "mock",
                "mock_script": [
                    MockProvider::text_response("replica-hello"),
                    MockProvider::text_response("after-cache-kill")
                ]
            }),
        )
        .await
        .ok()?;

        let root = std::env::temp_dir().join(format!("rupi-execd-scale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "scale-tok".into(),
            max_workspaces,
            warm_pool: 0,
        })
        .await
        .ok()?;
        let exec = Arc::new(rupi_runtime::http::HttpExecutor::new(
            format!("http://{exec_addr}"),
            "scale-tok",
        ));
        let cache = Cache::connect(&redis_url).await;
        let store: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(
            std::env::temp_dir().join(format!("rupi-obj-scale-{}", std::process::id())),
        ));
        let factory: rupi_server::ProviderFactory =
            Arc::new(|ten| rupi_server::run::default_provider(ten));
        let app_a = App::new(
            pool.clone(),
            cache.clone(),
            exec.clone(),
            factory.clone(),
        )
        .with_instance_id("replica-a")
        .with_object_store(store.clone());
        let app_b = App::new(pool.clone(), cache.clone(), exec, factory)
            .with_instance_id("replica-b")
            .with_object_store(store);
        let (addr_a, srv_a) = spawn(app_a.clone(), "127.0.0.1:0").await.ok()?;
        let (addr_b, srv_b) = spawn(app_b.clone(), "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            a: format!("http://{addr_a}"),
            b: format!("http://{addr_b}"),
            key,
            tenant: t.id,
            pool,
            cache,
            app_a,
            app_b,
            execd_root: root,
            _srv_a: srv_a,
            _srv_b: srv_b,
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
}

async fn create_session(h: &Dual, base: &str) -> String {
    let resp = h
        .client()
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&h.key)
        .json(&json!({"name": "s"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn post_agent(h: &Dual, base: &str, body: Value) -> (u16, String) {
    let resp = h
        .client()
        .post(format!("{base}/v1/agent"))
        .bearer_auth(&h.key)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_replicas_lease_hydrate_and_ready() {
    let Some(h) = Dual::start(8).await else {
        return;
    };
    let ready_a: Value = h
        .client()
        .get(format!("{}/ready", h.a))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ready_b: Value = h
        .client()
        .get(format!("{}/ready", h.b))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready_a["instanceId"], "replica-a");
    assert_eq!(ready_b["instanceId"], "replica-b");
    assert_eq!(ready_a["postgres"], true);
    assert_eq!(ready_a["redis"], true);

    let sid = create_session(&h, &h.a).await;

    assert!(
        quota::acquire_lease(
            &h.cache,
            &h.pool,
            &h.tenant,
            &sid,
            "held",
            "replica-a",
        )
        .await
    );
    let (st, body) = post_agent(
        &h,
        &h.b,
        json!({
            "threadId": sid,
            "runId": "should-409",
            "messages": [{"id":"u","role":"user","content":"nope"}]
        }),
    )
    .await;
    assert_eq!(st, 409, "{body}");
    quota::release_lease(
        &h.cache,
        &h.pool,
        &h.tenant,
        &sid,
        "held",
        "replica-a",
    )
    .await;

    let (st, body) = post_agent(
        &h,
        &h.a,
        json!({
            "threadId": sid,
            "runId": "r-a",
            "messages": [{"id":"u1","role":"user","content":"hello"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("RUN_FINISHED") || body.contains("replica-hello"), "{body}");

    let got = h
        .client()
        .get(format!("{}/v1/sessions/{sid}", h.b))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let sess: Value = got.json().await.unwrap();
    assert!(sess["tree"]["nodes"].as_u64().unwrap_or(0) >= 1, "{sess}");

    let (st, body) = post_agent(
        &h,
        &h.b,
        json!({
            "threadId": sid,
            "runId": "r-b",
            "messages": [{"id":"u2","role":"user","content":"again"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_exhaust_active_run_is_429() {
    let Some(h) = Dual::start(1).await else {
        return;
    };
    let s1 = create_session(&h, &h.a).await;
    assert!(
        quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &s1, "busy", "replica-a").await
    );
    let resp = h
        .client()
        .post(format!("{}/v1/sessions", h.a))
        .bearer_auth(&h.key)
        .json(&json!({"name": "no-room"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "{}", resp.text().await.unwrap());
    quota::release_lease(&h.cache, &h.pool, &h.tenant, &s1, "busy", "replica-a").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_snapshot_releases_and_restores() {
    let Some(h) = Dual::start(2).await else {
        return;
    };
    let sid = create_session(&h, &h.a).await;
    let row = db::get_session(&h.pool, &h.tenant, &sid).await.unwrap().unwrap();
    let handle = rupi_runtime::WorkspaceHandle {
        id: row.runtime_handle.clone().unwrap(),
        backend: row.runtime_backend.clone().unwrap(),
        region: row.region.clone(),
        kind: row.runtime_kind.clone(),
    };
    h.app_a
        .executor
        .fs_write(
            &handle,
            rupi_runtime::FsWriteRequest {
                path: "keep.txt".into(),
                content: "from-volume".into(),
            },
        )
        .await
        .unwrap();

    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE sessions SET last_used_at = now() - interval '2 hours' WHERE id = $1",
            &[&sid],
        )
        .await
        .unwrap();
    }

    let n = reclaim::reclaim_once(&h.app_a).await;
    assert!(n >= 1, "expected snapshot, got {n}");
    let after = db::get_session(&h.pool, &h.tenant, &sid).await.unwrap().unwrap();
    assert_eq!(after.workspace_state.as_deref(), Some("snapshotted"));
    assert!(after.runtime_handle.is_none());
    assert!(walk_named(&h.execd_root, "keep.txt").is_empty());

    let hot = reclaim::ensure_hot(&h.app_b, &h.tenant, &after).await.unwrap();
    let got = h
        .app_b
        .executor
        .fs_read(
            &hot,
            rupi_runtime::FsReadRequest {
                path: "keep.txt".into(),
                offset: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(got.content.contains("from-volume"), "{}", got.content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshotted_does_not_count_as_hot_handle() {
    let Some(h) = Dual::start(2).await else {
        return;
    };
    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE tenants SET max_handles = 1 WHERE id = $1",
            &[&h.tenant],
        )
        .await
        .unwrap();
    }
    let sid = create_session(&h, &h.a).await;
    let blocked = h
        .client()
        .post(format!("{}/v1/sessions", h.a))
        .bearer_auth(&h.key)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 429);

    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE sessions SET last_used_at = now() - interval '2 hours' WHERE id = $1",
            &[&sid],
        )
        .await
        .unwrap();
    }
    assert!(reclaim::reclaim_once(&h.app_a).await >= 1);
    let second = h
        .client()
        .post(format!("{}/v1/sessions", h.b))
        .bearer_auth(&h.key)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 201, "{}", second.text().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redis_kill_degrades_and_tightens() {
    let Some(h) = Dual::start(4).await else {
        return;
    };
    let sid = create_session(&h, &h.a).await;
    let (st, body) = post_agent(
        &h,
        &h.a,
        json!({
            "threadId": sid,
            "runId": "before-kill",
            "messages": [{"id":"u","role":"user","content":"hi"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");

    h.cache.kill().await;
    assert!(!h.cache.available().await);

    let sid2 = create_session(&h, &h.b).await;
    let (st, body) = post_agent(
        &h,
        &h.b,
        json!({
            "threadId": sid2,
            "runId": "after-kill",
            "messages": [{"id":"u2","role":"user","content":"still?"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");

    assert!(
        quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &sid, "held", "x").await
    );
    let tenant = db::load_tenant(&h.pool, &h.tenant).await.unwrap().unwrap();
    match quota::admit_run(&h.cache, &h.pool, &tenant).await {
        quota::Admit::TooMany => {}
        quota::Admit::Ok => panic!("redis down must tighten, not admit a second run"),
    }
    quota::release_lease(&h.cache, &h.pool, &h.tenant, &sid, "held", "x").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_reconcile_and_stale_run_reap() {
    let Some(h) = Dual::start(4).await else {
        return;
    };
    let sid = create_session(&h, &h.a).await;
    let _ = h
        .cache
        .set_ex(&Cache::rl_conc_key(&h.tenant), "99", 600)
        .await;
    let n = quota::reconcile(&h.cache, &h.pool, &h.tenant).await;
    assert_eq!(n, 0);
    let raw = h.cache.get(&Cache::rl_conc_key(&h.tenant)).await;
    assert_eq!(raw.as_deref(), Some("0"));

    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE sessions SET run_id = 'stale', instance_id = 'dead', updated_at = now() - interval '10 minutes'
             WHERE id = $1",
            &[&sid],
        )
        .await
        .unwrap();
    }
    assert!(db::count_active_runs(&h.pool, &h.tenant).await.unwrap() >= 1);
    let reaped = db::reap_stale_runs(&h.pool, 60).await.unwrap();
    assert!(reaped >= 1);
    assert_eq!(db::count_active_runs(&h.pool, &h.tenant).await.unwrap(), 0);
    assert!(
        quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &sid, "fresh", "replica-b").await
    );
    quota::release_lease(&h.cache, &h.pool, &h.tenant, &sid, "fresh", "replica-b").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daily_quota_and_fts_and_negative_cache() {
    let Some(h) = Dual::start(4).await else {
        return;
    };
    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE tenants SET max_runs_per_day = 1 WHERE id = $1",
            &[&h.tenant],
        )
        .await
        .unwrap();
    }
    let sid = create_session(&h, &h.a).await;
    let (st, body) = post_agent(
        &h,
        &h.a,
        json!({
            "threadId": sid,
            "runId": "day-1",
            "messages": [{"id":"u","role":"user","content":"one"}]
        }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = post_agent(
        &h,
        &h.b,
        json!({
            "threadId": sid,
            "runId": "day-2",
            "messages": [{"id":"u2","role":"user","content":"two"}]
        }),
    )
    .await;
    assert_eq!(st, 429, "{body}");

    db::insert_memory(&h.pool, &h.tenant, Some(&sid), "tenant", "scale-fts-needle")
        .await
        .unwrap();
    let hits = db::search_memories(&h.pool, &h.tenant, "needle", 5)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "{hits:?}");

    let missing = uuid::Uuid::new_v4().to_string();
    let (st, _) = post_agent(
        &h,
        &h.a,
        json!({
            "threadId": missing,
            "runId": "nope",
            "messages": [{"id":"u","role":"user","content":"x"}]
        }),
    )
    .await;
    assert_eq!(st, 404);
    assert!(h.cache.is_missing_session(&h.tenant, &missing).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_instance_conc_cap_is_shared() {
    let Some(h) = Dual::start(8).await else {
        return;
    };
    {
        let c = h.pool.get().await.unwrap();
        c.execute(
            "UPDATE tenants SET max_concurrent_runs = 2 WHERE id = $1",
            &[&h.tenant],
        )
        .await
        .unwrap();
    }
    let s1 = create_session(&h, &h.a).await;
    let s2 = create_session(&h, &h.a).await;
    let s3 = create_session(&h, &h.b).await;
    let s4 = create_session(&h, &h.b).await;
    assert!(
        quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &s1, "c1", "replica-a").await
    );
    assert!(
        quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &s2, "c2", "replica-b").await
    );
    let a = post_agent(
        &h,
        &h.a,
        json!({
            "threadId": s3,
            "runId": "over-a",
            "messages": [{"id":"u","role":"user","content":"no"}]
        }),
    );
    let b = post_agent(
        &h,
        &h.b,
        json!({
            "threadId": s4,
            "runId": "over-b",
            "messages": [{"id":"u2","role":"user","content":"no"}]
        }),
    );
    let (ra, rb) = tokio::join!(a, b);
    assert_eq!(ra.0, 429, "{}", ra.1);
    assert_eq!(rb.0, 429, "{}", rb.1);
    quota::release_lease(&h.cache, &h.pool, &h.tenant, &s1, "c1", "replica-a").await;
    quota::release_lease(&h.cache, &h.pool, &h.tenant, &s2, "c2", "replica-b").await;
}

fn walk_named(root: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(root).into_iter().flatten() {
        if e.file_name() == name {
            out.push(e.path().to_path_buf());
        }
    }
    out
}

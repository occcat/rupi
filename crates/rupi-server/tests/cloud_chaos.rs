//! 云打磨：缩小规模的池耗尽 / 杀副本回归，加上本机万级短流入口。
//!
//! CI `cloud` job 只跑未 ignore 的用例（个位数会话、短 mock 流）。
//! 默认 **不** 打一万长连接。万级入口见 `ten_thousand_streams`（`#[ignore]`）
//! 和 `docs/LAUNCH.md`「容量演练」。

mod common;

use common::lock_harness;
use rupi_llm::MockProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_runtime::{MemoryObjectStore, ObjectStore};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::quota;
use rupi_server::{reclaim, spawn, App, Cache};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        return false;
    }
    match redis::Client::open(redis_url) {
        Ok(c) => c.get_connection().is_ok(),
        Err(_) => false,
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(32)
        .build()
        .unwrap()
}

struct Pair {
    a: String,
    b: String,
    key: String,
    tenant: String,
    pool: db::PgPool,
    cache: Cache,
    app_b: App,
    store: Arc<dyn ObjectStore>,
    srv_a: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    _srv_b: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Pair {
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
        let t = db::create_tenant(&pool, "chaos", &key).await.ok()?;
        db::update_settings(
            &pool,
            &t.id,
            &json!({
                "provider": "mock",
                "mock_script": [
                    MockProvider::text_response("chaos-hello"),
                    MockProvider::text_response("after-kill")
                ]
            }),
        )
        .await
        .ok()?;
        db::set_tenant_caps_ex(&pool, &t.id, 32, 64, 100000, 2000)
            .await
            .ok()?;

        let root = std::env::temp_dir().join(format!("rupi-chaos-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root,
            token: "chaos".into(),
            max_workspaces,
            warm_pool: 0,
            insecure: false,
        })
        .await
        .ok()?;
        let exec = Arc::new(rupi_runtime::http::HttpExecutor::new(
            format!("http://{exec_addr}"),
            "chaos",
        ));
        let cache = Cache::connect(&redis_url).await;
        let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
        let factory: rupi_server::ProviderFactory =
            Arc::new(|ten| rupi_server::run::default_provider(ten));
        let app_a = App::new(pool.clone(), cache.clone(), exec.clone(), factory.clone())
            .with_instance_id("chaos-a")
            .with_object_store(store.clone());
        let app_b = App::new(pool.clone(), cache.clone(), exec, factory)
            .with_instance_id("chaos-b")
            .with_object_store(store.clone());
        let (addr_a, srv_a) = spawn(app_a, "127.0.0.1:0").await.ok()?;
        let (addr_b, srv_b) = spawn(app_b.clone(), "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            a: format!("http://{addr_a}"),
            b: format!("http://{addr_b}"),
            key,
            tenant: t.id,
            pool,
            cache,
            app_b,
            store,
            srv_a: Some(srv_a),
            _srv_b: srv_b,
            _execd: exec_h,
            _guard: guard,
        })
    }

    fn kill_a(&mut self) {
        if let Some(h) = self.srv_a.take() {
            h.abort();
        }
    }
}

async fn create_session(base: &str, key: &str, name: &str) -> (u16, Option<String>, String) {
    let resp = client()
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(key)
        .json(&json!({"name": name}))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let st = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            let id = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["id"].as_str().map(|s| s.to_string()));
            (st, id, body)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

async fn post_agent(
    base: &str,
    key: &str,
    thread: &str,
    run_id: &str,
    text: &str,
) -> (u16, String) {
    let resp = client()
        .post(format!("{base}/v1/agent"))
        .bearer_auth(key)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "threadId": thread,
            "runId": run_id,
            "messages": [{"role":"user","content": text}]
        }))
        .send()
        .await;
    match resp {
        Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
        Err(e) => (0, e.to_string()),
    }
}

/// 池满且会话占着 run：并发建仓全是 429。放开租约后下一次建仓走抢占（排队）成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_exhaust_429_then_preempt_queue() {
    let Some(h) = Pair::start(1).await else {
        return;
    };
    let (st, sid, body) = create_session(&h.a, &h.key, "held").await;
    assert_eq!(st, 201, "{body}");
    let sid = sid.expect("session id");
    assert!(quota::acquire_lease(&h.cache, &h.pool, &h.tenant, &sid, "busy", "chaos-a").await);

    let mut futs = Vec::new();
    for i in 0..6 {
        let base = h.a.clone();
        let key = h.key.clone();
        futs.push(async move { create_session(&base, &key, &format!("overflow-{i}")).await });
    }
    let results = futures::future::join_all(futs).await;
    let r429 = results
        .iter()
        .filter(|(st, _, body)| *st == 429 && body.contains("pool exhausted"))
        .count();
    eprintln!("CHAOS pool_busy overflow 429={r429}/6");
    assert_eq!(r429, 6, "{results:?}");

    quota::release_lease(&h.cache, &h.pool, &h.tenant, &sid, "busy", "chaos-a").await;

    let (st, queued, body) = create_session(&h.a, &h.key, "queued").await;
    assert_eq!(st, 201, "idle slot should preempt/queue, {body}");
    let queued = queued.expect("queued id");

    let held = db::get_session(&h.pool, &h.tenant, &sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.workspace_state.as_deref(), Some("snapshotted"));
    let fresh = db::get_session(&h.pool, &h.tenant, &queued)
        .await
        .unwrap()
        .unwrap();
    assert!(fresh.runtime_handle.is_some(), "{fresh:?}");
}

/// 杀掉控制面副本 A 后，B 仍能读 Postgres 会话树和共享对象存储里的快照。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_replica_session_and_snapshot_readable() {
    let Some(mut h) = Pair::start(4).await else {
        return;
    };
    let (st, sid, body) = create_session(&h.a, &h.key, "keep").await;
    assert_eq!(st, 201, "{body}");
    let sid = sid.expect("session id");

    let (st, body) = post_agent(&h.a, &h.key, &sid, "before-kill", "hello").await;
    assert_eq!(st, 200, "{body}");
    assert!(
        body.contains("RUN_FINISHED") || body.contains("chaos-hello"),
        "{body}"
    );

    let row = db::get_session(&h.pool, &h.tenant, &sid)
        .await
        .unwrap()
        .unwrap();
    let handle = rupi_runtime::WorkspaceHandle {
        id: row.runtime_handle.clone().unwrap(),
        backend: row.runtime_backend.clone().unwrap(),
        tenant_id: Some(row.tenant_id.clone()),
        region: row.region.clone(),
        kind: row.runtime_kind.clone(),
    };
    h.app_b
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
    let n = reclaim::reclaim_once(&h.app_b).await;
    assert!(n >= 1, "expected snapshot before kill, got {n}");
    let snapped = db::get_session(&h.pool, &h.tenant, &sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapped.workspace_state.as_deref(), Some("snapshotted"));
    let snap_key = snapped.snapshot_key.clone().expect("snapshot_key");
    assert!(
        rupi_runtime::snapshot_key_matches_region(&snap_key, snapped.region.as_deref()),
        "snapshot key must be region-scoped: {snap_key}"
    );
    assert!(!h.store.get(&snap_key).await.unwrap().is_empty());

    h.kill_a();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let dead = client().get(format!("{}/ready", h.a)).send().await;
    assert!(
        dead.as_ref().is_err() || dead.as_ref().ok().is_some_and(|r| !r.status().is_success()),
        "replica A should be gone"
    );

    let got = client()
        .get(format!("{}/v1/sessions/{sid}", h.b))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        got.status(),
        200,
        "{}",
        got.text().await.unwrap_or_default()
    );
    let sess: Value = got.json().await.unwrap();
    assert!(sess["tree"]["nodes"].as_u64().unwrap_or(0) >= 1, "{sess}");
    assert_eq!(sess["runtime"]["status"], "snapshotted");

    let hot = reclaim::ensure_hot(&h.app_b, &h.tenant, &snapped)
        .await
        .unwrap();
    let file = h
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
    assert!(file.content.contains("from-volume"), "{}", file.content);

    let (st, body) = post_agent(&h.b, &h.key, &sid, "after-kill", "still?").await;
    assert_eq!(st, 200, "{body}");
}

struct LoadN {
    base: String,
    key: String,
    _srv: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl LoadN {
    async fn start(sessions: usize, conc: usize) -> Option<Self> {
        let guard = lock_harness().await?;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect_with_size(&db_url, 48).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key = auth::generate_key();
        let t = db::create_tenant(&pool, "chaos-load", &key).await.ok()?;
        db::update_settings(
            &pool,
            &t.id,
            &json!({
                "provider": "mock",
                "mock_script": [MockProvider::text_response("load10k-ok")]
            }),
        )
        .await
        .ok()?;
        let handles = sessions.max(conc).max(8) as i32;
        db::set_tenant_caps_ex(&pool, &t.id, handles, handles, 1_000_000, 4_000)
            .await
            .ok()?;

        let root = std::env::temp_dir().join(format!("rupi-chaos-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root,
            token: "chaos-load".into(),
            max_workspaces: handles as u32,
            warm_pool: 0,
            insecure: false,
        })
        .await
        .ok()?;
        let cache = Cache::connect(&redis_url).await;
        let app = App::new(
            pool,
            cache,
            Arc::new(rupi_runtime::http::HttpExecutor::new(
                format!("http://{exec_addr}"),
                "chaos-load",
            )),
            Arc::new(|ten| rupi_server::run::default_provider(ten)),
        );
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            base: format!("http://{addr}"),
            key,
            _srv: srv,
            _execd: exec_h,
            _guard: guard,
        })
    }
}

async fn run_short_streams(h: &LoadN, streams: usize, conc: usize) -> (usize, usize) {
    let c = client();
    let sessions = 2usize.max(conc.min(8));
    let mut ids = Vec::new();
    for i in 0..sessions {
        let resp = c
            .post(format!("{}/v1/sessions", h.base))
            .bearer_auth(&h.key)
            .json(&json!({"name": format!("reg-{i}")}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            201,
            "{}",
            resp.text().await.unwrap()
        );
        let v: Value = resp.json().await.unwrap();
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    let mut run_ok = 0usize;
    let mut run_err = 0usize;
    let workers = conc.min(ids.len()).max(1);
    for wave in (0..streams).step_by(workers) {
        let n = workers.min(streams - wave);
        let mut futs = Vec::new();
        for (i, sid) in ids.iter().take(n).cloned().enumerate() {
            let c = c.clone();
            let base = h.base.clone();
            let key = h.key.clone();
            let run_id = format!("short-{wave}-{i}");
            futs.push(async move {
                let resp = c
                    .post(format!("{base}/v1/agent"))
                    .bearer_auth(&key)
                    .header("Accept", "text/event-stream")
                    .json(&json!({
                        "threadId": sid,
                        "runId": run_id,
                        "messages": [{"role":"user","content":"ping"}]
                    }))
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        let st = r.status().as_u16();
                        let body = r.text().await.unwrap_or_default();
                        st == 200 && (body.contains("RUN_FINISHED") || body.contains("load10k-ok"))
                    }
                    Err(_) => false,
                }
            });
        }
        for ok in futures::future::join_all(futs).await {
            if ok {
                run_ok += 1;
            } else {
                run_err += 1;
            }
        }
    }
    (run_ok, run_err)
}

/// CI 小回归：同一条短流路径，2 会话 / 4 次，不跑一万。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_stream_load_regression() {
    let Some(h) = LoadN::start(4, 2).await else {
        return;
    };
    let (ok, err) = run_short_streams(&h, 4, 2).await;
    assert_eq!(ok, 4, "short-stream regression failed err={err}");
}

/// 本机 / 文档触发的万级短流。默认 CI 不跑（`#[ignore]`）。
///
/// ```text
/// cargo test -p rupi-server --test cloud_chaos ten_thousand_streams -- --ignored --nocapture
/// ```
///
/// 环境变量：`RUPI_LOAD_SESSIONS`（默认 256）、`RUPI_LOAD_STREAMS`（默认 10000）、
/// `RUPI_LOAD_CONC`（默认 32）。不要在 GitHub Actions 里去掉 ignore 或把 CONC 拉到一万。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "local/docs 万级短流；默认 CI 不跑"]
async fn ten_thousand_streams() {
    let sessions = env_usize("RUPI_LOAD_SESSIONS", 256);
    let streams = env_usize("RUPI_LOAD_STREAMS", 10_000);
    let conc = env_usize("RUPI_LOAD_CONC", 32).min(sessions);
    let Some(h) = LoadN::start(sessions, conc).await else {
        eprintln!("skip ten_thousand_streams: no postgres/redis");
        return;
    };
    let c = client();
    let t0 = Instant::now();
    let mut ids = Vec::with_capacity(sessions);
    for chunk in (0..sessions).step_by(16) {
        let end = (chunk + 16).min(sessions);
        let mut futs = Vec::new();
        for i in chunk..end {
            let c = c.clone();
            let base = h.base.clone();
            let key = h.key.clone();
            futs.push(async move {
                let resp = c
                    .post(format!("{base}/v1/sessions"))
                    .bearer_auth(&key)
                    .json(&json!({"name": format!("n-{i}")}))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().as_u16() == 201 => r
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|v| v["id"].as_str().map(|s| s.to_string())),
                    _ => None,
                }
            });
        }
        ids.extend(futures::future::join_all(futs).await.into_iter().flatten());
    }
    assert_eq!(ids.len(), sessions, "session create short of {sessions}");

    let run_n = streams;
    let mut run_ok = 0usize;
    let mut run_err = 0usize;
    let workers = conc.min(ids.len()).max(1);
    for wave in (0..run_n).step_by(workers) {
        let n = workers.min(run_n - wave);
        let mut futs = Vec::new();
        for (i, sid) in ids.iter().take(n).cloned().enumerate() {
            let c = c.clone();
            let base = h.base.clone();
            let key = h.key.clone();
            let run_id = format!("n10k-{wave}-{i}");
            futs.push(async move {
                let resp = c
                    .post(format!("{base}/v1/agent"))
                    .bearer_auth(&key)
                    .header("Accept", "text/event-stream")
                    .json(&json!({
                        "threadId": sid,
                        "runId": run_id,
                        "messages": [{"role":"user","content":"ping"}]
                    }))
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        let st = r.status().as_u16();
                        let body = r.text().await.unwrap_or_default();
                        st == 200 && (body.contains("RUN_FINISHED") || body.contains("load10k-ok"))
                    }
                    Err(_) => false,
                }
            });
        }
        for ok in futures::future::join_all(futs).await {
            if ok {
                run_ok += 1;
            } else {
                run_err += 1;
            }
        }
    }
    eprintln!(
        "LOAD10k sessions={sessions} streams={run_n} conc={workers} ok={run_ok} err={run_err} elapsed_ms={}",
        t0.elapsed().as_millis()
    );
    assert_eq!(run_ok, run_n, "10k-entry streams failed err={run_err}");
}

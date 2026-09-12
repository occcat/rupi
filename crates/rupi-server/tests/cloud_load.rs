//! 千级压测：并发建会话 + AG-UI 流 + 句柄配额。可复现，不是口头估算。

mod common;

use common::lock_harness;
use rupi_llm::MockProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::{spawn, App, Cache};
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

struct Load {
    base: String,
    key_a: String,
    key_b: String,
    _srv: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Load {
    async fn start() -> Option<Self> {
        let guard = lock_harness().await?;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect_with_size(&db_url, 48).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key_a = auth::generate_key();
        let key_b = auth::generate_key();
        let ta = db::create_tenant(&pool, "load-a", &key_a).await.ok()?;
        let tb = db::create_tenant(&pool, "load-b", &key_b).await.ok()?;
        db::update_settings(
            &pool,
            &ta.id,
            &json!({
                "provider": "mock",
                "mock_script": [MockProvider::text_response("load-ok")]
            }),
        )
        .await
        .ok()?;
        db::update_settings(
            &pool,
            &tb.id,
            &json!({
                "provider": "mock",
                "mock_script": [MockProvider::text_response("quota-ok")]
            }),
        )
        .await
        .ok()?;
        db::set_tenant_caps_ex(&pool, &ta.id, 512, 2000, 100000, 2000).await.ok()?;
        db::set_tenant_caps_ex(&pool, &tb.id, 4, 16, 100000, 2000).await.ok()?;

        let root = std::env::temp_dir().join(format!("rupi-load-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root,
            token: "load".into(),
            max_workspaces: 2000,
            warm_pool: 0,
        })
        .await
        .ok()?;
        let cache = Cache::connect(&redis_url).await;
        let app = App::new(
            pool,
            cache,
            Arc::new(rupi_runtime::http::HttpExecutor::new(
                format!("http://{exec_addr}"),
                "load",
            )),
            Arc::new(|t| rupi_server::run::default_provider(t)),
        );
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            base: format!("http://{addr}"),
            key_a,
            key_b,
            _srv: srv,
            _execd: exec_h,
            _guard: guard,
        })
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(32)
            .build()
            .unwrap()
    }
}

const SESSION_N: usize = 1000;
const RUN_N: usize = 256;
const CREATE_CONC: usize = 16;
const RUN_CONC: usize = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn thousand_sessions_and_quota_hold() {
    let Some(h) = Load::start().await else {
        return;
    };
    let c = h.client();

    let t0 = Instant::now();
    let mut ids = Vec::with_capacity(SESSION_N);
    let mut create_ok = 0usize;
    let mut create_err = 0usize;
    let mut create_ms = Vec::new();
    for chunk_start in (0..SESSION_N).step_by(CREATE_CONC) {
        let end = (chunk_start + CREATE_CONC).min(SESSION_N);
        let mut futs = Vec::new();
        for i in chunk_start..end {
            let c = c.clone();
            let base = h.base.clone();
            let key = h.key_a.clone();
            futs.push(async move {
                let t = Instant::now();
                let mut last = (0u16, None, 0u64);
                for attempt in 0..4 {
                    let resp = c
                        .post(format!("{base}/v1/sessions"))
                        .bearer_auth(&key)
                        .json(&json!({"name": format!("s-{i}")}))
                        .send()
                        .await;
                    last = match resp {
                        Ok(r) => {
                            let st = r.status().as_u16();
                            let v = r.json::<Value>().await.ok();
                            (st, v, t.elapsed().as_millis() as u64)
                        }
                        Err(e) => {
                            if attempt == 3 {
                                eprintln!("LOAD create {i} err: {e}");
                            }
                            (0, None, t.elapsed().as_millis() as u64)
                        }
                    };
                    if last.0 == 201 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20 * (attempt + 1) as u64)).await;
                }
                last
            });
        }
        for (st, v, ms) in futures::future::join_all(futs).await {
            create_ms.push(ms);
            if st == 201 {
                create_ok += 1;
                if let Some(id) = v.and_then(|x| x["id"].as_str().map(|s| s.to_string())) {
                    ids.push(id);
                }
            } else {
                create_err += 1;
                if create_err <= 8 {
                    eprintln!("LOAD create fail status={st} body={v:?}");
                }
            }
        }
    }
    let create_elapsed = t0.elapsed();
    create_ms.sort_unstable();
    let p50 = create_ms
        .get(create_ms.len() / 2)
        .copied()
        .unwrap_or(0);
    eprintln!(
        "LOAD sessions n={SESSION_N} ok={create_ok} err={create_err} p50_ms={p50} elapsed_ms={}",
        create_elapsed.as_millis()
    );
    assert_eq!(create_ok, SESSION_N, "expected {SESSION_N} sessions, err={create_err}");
    assert_eq!(ids.len(), SESSION_N);

    let run_n = RUN_N.min(ids.len());
    let t1 = Instant::now();
    let mut run_ok = 0usize;
    let mut run_err = 0usize;
    let mut run_ms = Vec::new();
    for chunk_start in (0..run_n).step_by(RUN_CONC) {
        let end = (chunk_start + RUN_CONC).min(run_n);
        let mut futs = Vec::new();
        for (i, sid) in ids[chunk_start..end].iter().cloned().enumerate() {
            let c = c.clone();
            let base = h.base.clone();
            let key = h.key_a.clone();
            let run_id = format!("load-run-{chunk_start}-{i}");
            futs.push(async move {
                let t = Instant::now();
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
                        (st, body, t.elapsed().as_millis() as u64)
                    }
                    Err(e) => (0, e.to_string(), t.elapsed().as_millis() as u64),
                }
            });
        }
        for (st, body, ms) in futures::future::join_all(futs).await {
            run_ms.push(ms);
            if st == 200 && (body.contains("load-ok") || body.contains("RUN_FINISHED")) {
                run_ok += 1;
            } else {
                run_err += 1;
                if run_err <= 3 {
                    eprintln!("LOAD run fail status={st} body={}", &body[..body.len().min(200)]);
                }
            }
        }
    }
    run_ms.sort_unstable();
    let rp50 = run_ms.get(run_ms.len() / 2).copied().unwrap_or(0);
    eprintln!(
        "LOAD runs n={run_n} ok={run_ok} err={run_err} p50_ms={rp50} elapsed_ms={}",
        t1.elapsed().as_millis()
    );
    assert_eq!(run_ok, run_n, "concurrent AG-UI runs failed err={run_err}");

    // 句柄配额在并发下站得住：16 上限，80 并发创建。
    let t2 = Instant::now();
    let mut futs = Vec::new();
    for i in 0..80 {
        let c = c.clone();
        let base = h.base.clone();
        let key = h.key_b.clone();
        futs.push(async move {
            let resp = c
                .post(format!("{base}/v1/sessions"))
                .bearer_auth(&key)
                .json(&json!({"name": format!("qb-{i}")}))
                .send()
                .await;
            resp.map(|r| r.status().as_u16()).unwrap_or(0)
        });
    }
    let statuses = futures::future::join_all(futs).await;
    let ok201 = statuses.iter().filter(|s| **s == 201).count();
    let r429 = statuses.iter().filter(|s| **s == 429).count();
    eprintln!(
        "LOAD handle_quota n=80 ok201={ok201} r429={r429} other={} elapsed_ms={}",
        80 - ok201 - r429,
        t2.elapsed().as_millis()
    );
    assert_eq!(ok201, 16, "handle quota must admit exactly max_handles");
    assert!(r429 >= 50, "handle quota must 429 the rest, got {r429}");
}

//! 多区域：控制面无状态跨区；会话权威在 Postgres；执行按 region 调度。

mod common;

use common::lock_harness;
use rupi_llm::MockProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_runtime::sandbox::{self, SandboxdConfig};
use rupi_runtime::{BackendKind, PoolNode, PoolScheduler};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::{spawn, App, Cache};
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
        return false;
    }
    match redis::Client::open(redis_url) {
        Ok(c) => c.get_connection().is_ok(),
        Err(_) => false,
    }
}

struct Regions {
    us: String,
    eu: String,
    key: String,
    us_root: std::path::PathBuf,
    eu_root: std::path::PathBuf,
    pool: db::PgPool,
    _srv_us: tokio::task::JoinHandle<anyhow::Result<()>>,
    _srv_eu: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _sandbox: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Regions {
    async fn start() -> Option<Self> {
        let guard = lock_harness().await?;
        let (db_url, redis_url) = env_urls()?;
        if !ping_deps(&db_url, &redis_url).await {
            return None;
        }
        let pool = db::connect(&db_url).await.ok()?;
        db::migrate(&pool).await.ok()?;
        let _ = db::reset_all(&pool).await;
        let key = auth::generate_key();
        let t = db::create_tenant(&pool, "multi-region", &key).await.ok()?;
        db::update_settings(
            &pool,
            &t.id,
            &json!({
                "provider": "mock",
                "mock_script": [
                    MockProvider::text_response("from-us"),
                    MockProvider::text_response("from-eu-replica")
                ]
            }),
        )
        .await
        .ok()?;
        db::set_tenant_caps(&pool, &t.id, 8, 32, 10000).await.ok()?;

        let us_root = std::env::temp_dir().join(format!("rupi-reg-us-{}", std::process::id()));
        let eu_root = std::env::temp_dir().join(format!("rupi-reg-eu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&us_root);
        let _ = std::fs::remove_dir_all(&eu_root);
        std::fs::create_dir_all(&us_root).ok()?;
        std::fs::create_dir_all(&eu_root).ok()?;

        let (us_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: us_root.clone(),
            token: "tok".into(),
            max_workspaces: 16,
            warm_pool: 0,
        })
        .await
        .ok()?;
        let (eu_addr, sb_h) = sandbox::spawn(SandboxdConfig {
            bind: "127.0.0.1:0".into(),
            root: eu_root.clone(),
            token: "tok".into(),
            max_sandboxes: 16,
            warm_pool: 0,
            region: "eu-west".into(),
        })
        .await
        .ok()?;

        let exec: Arc<dyn rupi_runtime::Executor> = Arc::new(PoolScheduler::new(vec![
            PoolNode::new(
                "remote-http:us-east",
                Arc::new(rupi_runtime::http::HttpExecutor::new(
                    format!("http://{us_addr}"),
                    "tok",
                )),
            )
            .with_kind(BackendKind::RemoteHttp)
            .with_region("us-east"),
            PoolNode::new(
                "sandbox:eu-west",
                Arc::new(rupi_runtime::SandboxExecutor::new(
                    format!("http://{eu_addr}"),
                    "tok",
                )),
            )
            .with_kind(BackendKind::Sandbox)
            .with_region("eu-west"),
        ]));
        let cache = Cache::connect(&redis_url).await;
        let factory: rupi_server::ProviderFactory =
            Arc::new(|ten| rupi_server::run::default_provider(ten));
        let app_us = App::new(pool.clone(), cache.clone(), exec.clone(), factory.clone())
            .with_instance_id("cp-us")
            .with_region("us-east");
        let app_eu = App::new(pool.clone(), cache, exec, factory)
            .with_instance_id("cp-eu")
            .with_region("eu-west");
        let (addr_us, srv_us) = spawn(app_us, "127.0.0.1:0").await.ok()?;
        let (addr_eu, srv_eu) = spawn(app_eu, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            us: format!("http://{addr_us}"),
            eu: format!("http://{addr_eu}"),
            key,
            us_root,
            eu_root,
            pool,
            _srv_us: srv_us,
            _srv_eu: srv_eu,
            _execd: exec_h,
            _sandbox: sb_h,
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

fn tree_has(root: &std::path::Path, needle: &str) -> bool {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains(needle))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_plane_stateless_exec_pinned_by_region() {
    let Some(h) = Regions::start().await else {
        return;
    };
    let ready_us: Value = h
        .client()
        .get(format!("{}/ready", h.us))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready_us["region"], "us-east");
    assert_eq!(ready_us["instanceId"], "cp-us");

    let created = h
        .client()
        .post(format!("{}/v1/sessions", h.us))
        .bearer_auth(&h.key)
        .json(&json!({"name": "us-thread", "region": "us-east", "backend": "remote-http"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{}", created.text().await.unwrap());
    let us_sess: Value = created.json().await.unwrap();
    assert_eq!(us_sess["region"], "us-east");
    let us_id = us_sess["id"].as_str().unwrap().to_string();
    let us_handle = us_sess["runtime"]["handle"].as_str().unwrap().to_string();
    assert!(tree_has(&h.us_root, &us_handle));
    assert!(!tree_has(&h.eu_root, &us_handle));

    let created_eu = h
        .client()
        .post(format!("{}/v1/sessions", h.eu))
        .bearer_auth(&h.key)
        .json(&json!({"name": "eu-thread", "region": "eu-west", "backend": "sandbox"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created_eu.status(), 201, "{}", created_eu.text().await.unwrap());
    let eu_sess: Value = created_eu.json().await.unwrap();
    assert_eq!(eu_sess["region"], "eu-west");
    assert_eq!(eu_sess["runtime"]["kind"], "sandbox");
    let eu_handle = eu_sess["runtime"]["handle"].as_str().unwrap();
    assert!(tree_has(&h.eu_root, eu_handle));
    assert!(!tree_has(&h.us_root, eu_handle));

    // 权威在 Postgres：eu 控制面能读 us 会话。
    let got = h
        .client()
        .get(format!("{}/v1/sessions/{us_id}", h.eu))
        .bearer_auth(&h.key)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let got: Value = got.json().await.unwrap();
    assert_eq!(got["region"], "us-east");
    assert_eq!(got["id"], us_id);

    // 跨区编排：eu 副本跑 us 会话，执行仍落 us-east 节点。
    let (st, body) = {
        let resp = h
            .client()
            .post(format!("{}/v1/agent", h.eu))
            .bearer_auth(&h.key)
            .header("Accept", "text/event-stream")
            .json(&json!({
                "threadId": us_id,
                "runId": "cross-1",
                "messages": [{"role":"user","content":"hello from eu replica"}]
            }))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap_or_default())
    };
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("from-us") || body.contains("RUN_FINISHED"), "{body}");

    let tenant_id = db::session_owner(&h.pool, &us_id).await.unwrap().unwrap();
    let row = db::get_session(&h.pool, &tenant_id, &us_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.region.as_deref(), Some("us-east"));
    assert!(tree_has(&h.us_root, &us_handle));

    // 没有该区域的执行面 → 拒绝，不偷偷落到别的区。
    let miss = h
        .client()
        .post(format!("{}/v1/sessions", h.us))
        .bearer_auth(&h.key)
        .json(&json!({"region": "ap-south", "backend": "sandbox"}))
        .send()
        .await
        .unwrap();
    assert!(
        miss.status().as_u16() == 503 || miss.status().as_u16() == 429,
        "{}",
        miss.status()
    );
}

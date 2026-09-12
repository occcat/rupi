//! 第二种 Executor：sandbox 集群与现有 execd 并列、可插拔。

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

struct Harness {
    base: String,
    key: String,
    execd_root: std::path::PathBuf,
    sandbox_root: std::path::PathBuf,
    _srv: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
    _sandbox: tokio::task::JoinHandle<anyhow::Result<()>>,
    _guard: common::HarnessGuard,
}

impl Harness {
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
        let t = db::create_tenant(&pool, "dual-backend", &key).await.ok()?;
        db::update_settings(
            &pool,
            &t.id,
            &json!({
                "provider": "mock",
                "mock_script": [MockProvider::text_response("via-pluggable-executor")]
            }),
        )
        .await
        .ok()?;
        db::set_tenant_caps(&pool, &t.id, 8, 32, 10000).await.ok()?;

        let exec_root = std::env::temp_dir().join(format!("rupi-be-exec-{}", std::process::id()));
        let sb_root = std::env::temp_dir().join(format!("rupi-be-sb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&exec_root);
        let _ = std::fs::remove_dir_all(&sb_root);
        std::fs::create_dir_all(&exec_root).ok()?;
        std::fs::create_dir_all(&sb_root).ok()?;
        let (exec_addr, exec_h) = execd::spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: exec_root.clone(),
            token: "tok".into(),
            max_workspaces: 16,
            warm_pool: 0,
        })
        .await
        .ok()?;
        let (sb_addr, sb_h) = sandbox::spawn(SandboxdConfig {
            bind: "127.0.0.1:0".into(),
            root: sb_root.clone(),
            token: "tok".into(),
            max_sandboxes: 16,
            warm_pool: 0,
            region: "local".into(),
        })
        .await
        .ok()?;
        let nodes = vec![
            PoolNode::new(
                "remote-http",
                Arc::new(rupi_runtime::http::HttpExecutor::new(
                    format!("http://{exec_addr}"),
                    "tok",
                )),
            )
            .with_kind(BackendKind::RemoteHttp)
            .with_region("local"),
            PoolNode::new(
                "sandbox",
                Arc::new(rupi_runtime::SandboxExecutor::new(
                    format!("http://{sb_addr}"),
                    "tok",
                )),
            )
            .with_kind(BackendKind::Sandbox)
            .with_region("local"),
        ];
        let cache = Cache::connect(&redis_url).await;
        let app = App::new(
            pool,
            cache,
            Arc::new(PoolScheduler::new(nodes)),
            Arc::new(|ten| rupi_server::run::default_provider(ten)),
        )
        .with_region("local");
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            base: format!("http://{addr}"),
            key,
            execd_root: exec_root,
            sandbox_root: sb_root,
            _srv: srv,
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

async fn create(h: &Harness, backend: &str) -> Value {
    let resp = h
        .client()
        .post(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.key)
        .json(&json!({"name": backend, "backend": backend}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    resp.json().await.unwrap()
}

fn tree_has(root: &std::path::Path, needle: &str) -> bool {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains(needle))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandbox_and_execd_are_pluggable() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let ready: Value = h
        .client()
        .get(format!("{}/ready", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<String> = ready["executors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"remote-http".into()), "{ready}");
    assert!(kinds.contains(&"sandbox".into()), "{ready}");

    let sb = create(&h, "sandbox").await;
    assert_eq!(sb["runtime"]["kind"], "sandbox");
    assert_eq!(sb["runtime"]["backend"], "sandbox");
    let hid = sb["runtime"]["handle"].as_str().unwrap();
    assert!(
        tree_has(&h.sandbox_root, hid),
        "sandbox handle must live under sandboxd root"
    );
    assert!(
        !tree_has(&h.execd_root, hid),
        "sandbox handle must not appear on execd"
    );

    let rh = create(&h, "remote-http").await;
    assert_eq!(rh["runtime"]["kind"], "remote-http");
    let hid2 = rh["runtime"]["handle"].as_str().unwrap();
    assert!(tree_has(&h.execd_root, hid2));
    assert!(!tree_has(&h.sandbox_root, hid2));

    let sid = sb["id"].as_str().unwrap();
    let resp = h
        .client()
        .post(format!("{}/v1/agent", h.base))
        .bearer_auth(&h.key)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "threadId": sid,
            "runId": "r1",
            "messages": [{"role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
}

//! 本机 `rupi cloud` 连控制面：AG-UI/HTTP，不在本机执行 bash。

use rupi_llm::MockProvider;
use rupi_runtime::execd::{self, ExecdConfig};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::{spawn, App, Cache};
use serde_json::json;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;

struct HarnessGuard {
    _client: tokio_postgres::Client,
    _driver: tokio::task::JoinHandle<()>,
}

async fn lock_harness() -> Option<HarnessGuard> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| Some("postgresql://rupi:rupi@127.0.0.1:5432/rupi".into()))?;
    let (client, conn) = tokio_postgres::connect(&url, NoTls).await.ok()?;
    let driver = tokio::spawn(async move {
        let _ = conn.await;
    });
    if client
        .batch_execute("SELECT pg_advisory_lock(852674)")
        .await
        .is_err()
    {
        return None;
    }
    Some(HarnessGuard {
        _client: client,
        _driver: driver,
    })
}

fn rupi() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rupi"));
    cmd.stdin(Stdio::null())
        .env_remove("RUPI_API_KEY")
        .env_remove("OPENAI_API_KEY");
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rupi_cloud_talks_agui_http() {
    let Some(_guard) = lock_harness().await else {
        return;
    };
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://rupi:rupi@127.0.0.1:5432/rupi".into());
    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    if db::connect(&db_url).await.is_err() {
        return;
    }
    let pool = db::connect(&db_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let _ = db::reset_all(&pool).await;
    let key = auth::generate_key();
    let t = db::create_tenant(&pool, "thin", &key).await.unwrap();
    db::update_settings(
        &pool,
        &t.id,
        &json!({
            "provider": "mock",
            "mock_script": [MockProvider::text_response("thin-client-hello")]
        }),
    )
    .await
    .unwrap();
    db::set_tenant_caps(&pool, &t.id, 4, 8, 1000).await.unwrap();

    let root = std::env::temp_dir().join(format!("rupi-thin-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let (exec_addr, _eh) = execd::spawn(ExecdConfig {
        bind: "127.0.0.1:0".into(),
        root,
        token: "thin".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    let cache = Cache::connect(&redis_url).await;
    let app = App::new(
        pool,
        cache,
        Arc::new(rupi_runtime::http::HttpExecutor::new(
            format!("http://{exec_addr}"),
            "thin",
        )),
        Arc::new(|ten| rupi_server::run::default_provider(ten)),
    );
    let (addr, _srv) = spawn(app, "127.0.0.1:0").await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let url = format!("http://{addr}");

    let me = rupi()
        .args(["--api-key", &key, "cloud", "--url", &url, "me"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&me.stdout);
    let err = String::from_utf8_lossy(&me.stderr);
    assert!(me.status.success(), "rupi cloud me:\n{out}\n{err}");
    assert!(out.contains("thin") || out.contains("tenantId"), "{out}");

    let prompt = rupi()
        .args([
            "--api-key",
            &key,
            "cloud",
            "--url",
            &url,
            "prompt",
            "hello from thin client",
        ])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&prompt.stdout);
    let err = String::from_utf8_lossy(&prompt.stderr);
    assert!(prompt.status.success(), "rupi cloud prompt:\n{out}\n{err}");
    assert!(
        out.contains("thin-client-hello"),
        "expected AG-UI text, got:\n{out}\n{err}"
    );
    assert!(
        !out.contains("\"type\":\"prompt\""),
        "must not emit local RPC JSON: {out}"
    );
}

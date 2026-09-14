//! 按 `docs/LAUNCH.md` 用真实二进制：bootstrap → execd → rupi-server → 管理面种 Key → 会话 → AG-UI mock。
//! 无 Postgres / Redis 时跳过（与其他 cloud_* 相同）。

mod common;

use rupi_server::db;
use serde_json::Value;
use std::process::{Child, Command, Stdio};
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

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct Kids(Vec<Child>);

impl Drop for Kids {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn collect_text_deltas(body: &str) -> String {
    let mut text = String::new();
    for line in body.lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("TEXT_MESSAGE_CONTENT") | Some("TEXT_MESSAGE_DELTA") => {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    text.push_str(d);
                }
            }
            _ => {}
        }
    }
    text
}

async fn wait_http(url: &str, want: u16) -> bool {
    let c = reqwest::Client::new();
    for _ in 0..50 {
        if let Ok(r) = c.get(url).send().await {
            if r.status().as_u16() == want {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_md_binaries_admin_key_session_mock() {
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
    let _ = db::reset_all(&pool).await;

    let server_bin = env!("CARGO_BIN_EXE_rupi-server");
    let execd_bin = env!("CARGO_BIN_EXE_rupi-execd");

    let admin_out = Command::new(server_bin)
        .args(["--database-url", &db_url, "--bootstrap-admin"])
        .output()
        .unwrap();
    assert!(
        admin_out.status.success(),
        "bootstrap-admin: {}",
        String::from_utf8_lossy(&admin_out.stderr)
    );
    let admin_stdout = String::from_utf8_lossy(&admin_out.stdout);
    assert!(
        admin_stdout.contains("key=rupi_admin_"),
        "{admin_stdout} stderr={}",
        String::from_utf8_lossy(&admin_out.stderr)
    );

    let tenant_out = Command::new(server_bin)
        .args([
            "--database-url",
            &db_url,
            "--bootstrap-tenant",
            "launch-cli",
        ])
        .output()
        .unwrap();
    assert!(
        tenant_out.status.success(),
        "bootstrap-tenant: {}",
        String::from_utf8_lossy(&tenant_out.stderr)
    );
    let tenant_stdout = String::from_utf8_lossy(&tenant_out.stdout);
    assert!(
        tenant_stdout.contains("key=rupi_") && tenant_stdout.contains("name=launch-cli"),
        "{tenant_stdout}"
    );

    let root = std::env::temp_dir().join(format!("rupi-launch-execd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let exec_port = free_port();
    let api_port = free_port();
    let execd = Command::new(execd_bin)
        .args([
            "--listen",
            &format!("127.0.0.1:{exec_port}"),
            "--root",
            root.to_str().unwrap(),
            "--token",
            "launch-cli-exec",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Command::new(server_bin)
        .env("DATABASE_URL", &db_url)
        .env("REDIS_URL", &redis_url)
        .env("RUPI_LISTEN", format!("127.0.0.1:{api_port}"))
        .env(
            "RUPI_EXECUTOR_URLS",
            format!("http://127.0.0.1:{exec_port}"),
        )
        .env("RUPI_EXEC_TOKEN", "launch-cli-exec")
        .env("RUPI_ADMIN_TOKEN", "launch-cli-admin")
        .env("RUPI_CLOUD_MOCK", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _kids = Kids(vec![execd, server]);
    let api = format!("http://127.0.0.1:{api_port}");
    assert!(
        wait_http(&format!("{api}/health"), 200).await,
        "rupi-server did not become healthy on {api}"
    );
    assert!(
        wait_http(&format!("{api}/ready"), 200).await,
        "/ready did not become 200 on {api}"
    );

    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let me = c
        .get(format!("{api}/admin/api/me"))
        .bearer_auth("launch-cli-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 200, "{}", me.text().await.unwrap());

    let created = c
        .post(format!("{api}/admin/api/tenants"))
        .bearer_auth("launch-cli-admin")
        .json(&serde_json::json!({"name": "from-admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{}", created.text().await.unwrap());
    let created: Value = created.json().await.unwrap();
    let key = created["key"].as_str().expect("admin key").to_string();
    assert!(key.starts_with("rupi_"), "{key}");

    let sess = c
        .post(format!("{api}/v1/sessions"))
        .bearer_auth(&key)
        .json(&serde_json::json!({"name": "launch-thread"}))
        .send()
        .await
        .unwrap();
    assert_eq!(sess.status(), 201, "{}", sess.text().await.unwrap());
    let sess: Value = sess.json().await.unwrap();
    let sid = sess["id"].as_str().expect("session id").to_string();

    let agent = c
        .post(format!("{api}/v1/agent"))
        .bearer_auth(&key)
        .header("Accept", "text/event-stream")
        .json(&serde_json::json!({
            "threadId": sid,
            "runId": "launch-1",
            "messages": [{"id": "m1", "role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(agent.status(), 200, "{}", agent.status());
    let body = agent.text().await.unwrap();
    let text = collect_text_deltas(&body);
    assert_eq!(text, "hello from rupi-server (mock)", "{body}");
    assert!(body.contains("RUN_FINISHED"), "{body}");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_503_when_server_points_at_dead_execd() {
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

    let server_bin = env!("CARGO_BIN_EXE_rupi-server");
    let api_port = free_port();
    let dead = free_port();
    let server = Command::new(server_bin)
        .env("DATABASE_URL", &db_url)
        .env("REDIS_URL", &redis_url)
        .env("RUPI_LISTEN", format!("127.0.0.1:{api_port}"))
        .env("RUPI_EXECUTOR_URLS", format!("http://127.0.0.1:{dead}"))
        .env("RUPI_EXEC_TOKEN", "x")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _kids = Kids(vec![server]);
    let api = format!("http://127.0.0.1:{api_port}");
    assert!(
        wait_http(&format!("{api}/health"), 200).await,
        "server did not listen"
    );
    let resp = reqwest::Client::new()
        .get(format!("{api}/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "{}", resp.text().await.unwrap());
}

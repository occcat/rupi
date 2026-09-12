//! 管理面：独立 admin 凭证、租户/Key、会话目录、配额、settings 白名单、静态 UI。
//! 租户 Key 不能进 /admin；吊销后不能打 /v1。本机 RPC 不在此测。

mod common;

use rupi_runtime::execd::{self, ExecdConfig};
use rupi_server::auth;
use rupi_server::db;
use rupi_server::{spawn, App, Cache};
use serde_json::{json, Value};
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

struct Harness {
    base: String,
    admin: String,
    tenant_key: String,
    tenant_id: String,
    pool: db::PgPool,
    _server: tokio::task::JoinHandle<anyhow::Result<()>>,
    _execd: tokio::task::JoinHandle<anyhow::Result<()>>,
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
        let tenant_key = auth::generate_key();
        let t = db::create_tenant(&pool, "ops-alpha", &tenant_key)
            .await
            .ok()?;
        db::update_settings(&pool, &t.id, &json!({"provider": "mock", "model": "gpt-4o-mini"}))
            .await
            .ok()?;

        let root = std::env::temp_dir().join(format!("rupi-admin-execd-{}", std::process::id()));
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
        let cache = Cache::connect(&redis_url).await;
        let admin = "admin-test-token".to_string();
        let app = App::new(
            pool.clone(),
            cache,
            Arc::new(rupi_runtime::http::HttpExecutor::new(
                format!("http://{exec_addr}"),
                "exec-secret",
            )),
            Arc::new(|ten| rupi_server::run::default_provider(ten)),
        )
        .with_admin_token(admin.clone());
        let (addr, srv) = spawn(app, "127.0.0.1:0").await.ok()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some(Self {
            base: format!("http://{addr}"),
            admin,
            tenant_key,
            tenant_id: t.id,
            pool,
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

    async fn admin_get(&self, path: &str) -> reqwest::Response {
        self.client()
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.admin)
            .send()
            .await
            .unwrap()
    }

    async fn admin_json(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self
            .client()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.admin);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.unwrap();
        let code = res.status().as_u16();
        let v = if code == 204 {
            json!(null)
        } else {
            res.json().await.unwrap_or(json!(null))
        };
        (code, v)
    }
}

#[tokio::test]
async fn admin_auth_and_static_ui() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let html = h.client().get(format!("{}/admin", h.base)).send().await.unwrap();
    assert_eq!(html.status().as_u16(), 200);
    let body = html.text().await.unwrap();
    assert!(body.contains("Rupi 管理面"), "{body}");
    assert!(body.contains("/admin/assets/admin.js"));

    let css = h
        .client()
        .get(format!("{}/admin/assets/admin.css", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(css.status().as_u16(), 200);

    let no = h
        .client()
        .get(format!("{}/admin/api/me", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(no.status().as_u16(), 401);

    let tenant = h
        .client()
        .get(format!("{}/admin/api/me", h.base))
        .bearer_auth(&h.tenant_key)
        .send()
        .await
        .unwrap();
    assert_eq!(tenant.status().as_u16(), 401);

    let me = h.admin_get("/admin/api/me").await;
    assert_eq!(me.status().as_u16(), 200);
    let v: Value = me.json().await.unwrap();
    assert_eq!(v["role"], "admin");
}

#[tokio::test]
async fn admin_tenants_keys_quota_settings() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (code, created) = h
        .admin_json(
            reqwest::Method::POST,
            "/admin/api/tenants",
            Some(json!({
                "name": "beta",
                "defaultModel": "gpt-4o-mini",
                "defaultRegion": "local",
                "quota": { "maxConcurrentRuns": 3, "maxHandles": 4, "maxQps": 2 }
            })),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let new_id = created["tenant"]["id"].as_str().unwrap().to_string();
    let once = created["key"].as_str().unwrap().to_string();
    assert!(once.starts_with("rupi_"), "{once}");
    assert_eq!(created["tenant"]["quota"]["maxHandles"], 4);

    let (c2, listed) = h.admin_json(reqwest::Method::GET, "/admin/api/tenants", None).await;
    assert_eq!(c2, 200);
    assert!(listed["total"].as_i64().unwrap() >= 2);

    let (c3, key_created) = h
        .admin_json(
            reqwest::Method::POST,
            &format!("/admin/api/tenants/{new_id}/keys"),
            None,
        )
        .await;
    assert_eq!(c3, 201, "{key_created}");
    let extra = key_created["key"].as_str().unwrap().to_string();
    let kid = key_created["record"]["id"].as_str().unwrap().to_string();

    let me = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&extra)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 200);

    let (c4, _) = h
        .admin_json(reqwest::Method::POST, &format!("/admin/api/keys/{kid}/revoke"), None)
        .await;
    assert_eq!(c4, 200);
    let me2 = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&extra)
        .send()
        .await
        .unwrap();
    assert_eq!(me2.status().as_u16(), 401);
    let still = h
        .client()
        .get(format!("{}/v1/me", h.base))
        .bearer_auth(&once)
        .send()
        .await
        .unwrap();
    assert_eq!(still.status().as_u16(), 200);

    let (c5, q) = h
        .admin_json(
            reqwest::Method::PATCH,
            &format!("/admin/api/tenants/{new_id}/quota"),
            Some(json!({"maxConcurrentRuns": 1, "maxQps": 1})),
        )
        .await;
    assert_eq!(c5, 200, "{q}");
    assert_eq!(q["maxConcurrentRuns"], 1);
    assert_eq!(q["maxHandles"], 4);

    let (c6, bad) = h
        .admin_json(
            reqwest::Method::PATCH,
            &format!("/admin/api/tenants/{new_id}/settings"),
            Some(json!({"oauthClientId": "nope"})),
        )
        .await;
    assert_eq!(c6, 400, "{bad}");
    assert!(bad["error"].as_str().unwrap_or("").contains("unknown"));

    let (c7, ok) = h
        .admin_json(
            reqwest::Method::PATCH,
            &format!("/admin/api/tenants/{new_id}/settings"),
            Some(json!({"model": "gpt-4o-mini", "thinking": "low", "api_key": "sk-test"})),
        )
        .await;
    assert_eq!(c7, 200, "{ok}");
    assert_eq!(ok["settings"]["api_key"], "***");
    assert_eq!(ok["settings"]["thinking"], "low");

    let (c8, got) = h
        .admin_json(
            reqwest::Method::GET,
            &format!("/admin/api/tenants/{new_id}/settings"),
            None,
        )
        .await;
    assert_eq!(c8, 200);
    assert_eq!(got["settings"]["api_key"], "***");

    let (c9, mask_keep) = h
        .admin_json(
            reqwest::Method::PATCH,
            &format!("/admin/api/tenants/{new_id}/settings"),
            Some(json!({"api_key": "***", "provider": "mock"})),
        )
        .await;
    assert_eq!(c9, 200, "{mask_keep}");
    let stored = db::load_tenant(&h.pool, &new_id).await.unwrap().unwrap();
    assert_eq!(stored.settings["api_key"], "sk-test");
    assert_eq!(stored.settings["provider"], "mock");
}

#[tokio::test]
async fn admin_sessions_executors_and_admin_keys() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let created = h
        .client()
        .post(format!("{}/v1/sessions", h.base))
        .bearer_auth(&h.tenant_key)
        .json(&json!({"name": "ops-sess", "region": "local"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status().as_u16(), 201, "{}", created.text().await.unwrap());
    let sess: Value = created.json().await.unwrap();
    let sid = sess["id"].as_str().unwrap().to_string();

    let (c1, catalog) = h
        .admin_json(
            reqwest::Method::GET,
            &format!("/admin/api/sessions?tenantId={}&status=ready", h.tenant_id),
            None,
        )
        .await;
    assert_eq!(c1, 200, "{catalog}");
    assert!(catalog["total"].as_i64().unwrap() >= 1);
    let found = catalog["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == sid);
    assert!(found, "{catalog}");

    let (c2, detail) = h
        .admin_json(reqwest::Method::GET, &format!("/admin/api/sessions/{sid}"), None)
        .await;
    assert_eq!(c2, 200, "{detail}");
    assert_eq!(detail["tenantId"], h.tenant_id);
    assert!(detail["openHint"]["path"].as_str() == Some("/v1/agent"));

    let (c3, exec) = h.admin_json(reqwest::Method::GET, "/admin/api/executors", None).await;
    assert_eq!(c3, 200, "{exec}");
    assert!(exec["nodes"].as_array().is_some());
    assert!(exec["handles"].as_array().is_some());

    let (c4, regions) = h.admin_json(reqwest::Method::GET, "/admin/api/regions", None).await;
    assert_eq!(c4, 200, "{regions}");
    assert_eq!(regions["controlPlane"], "local");

    let (c5, ak) = h.admin_json(reqwest::Method::POST, "/admin/api/admin-keys", None).await;
    assert_eq!(c5, 201, "{ak}");
    let admin_key = ak["key"].as_str().unwrap().to_string();
    assert!(admin_key.starts_with("rupi_admin_"));
    let aid = ak["record"]["id"].as_str().unwrap().to_string();

    let me = h
        .client()
        .get(format!("{}/admin/api/me", h.base))
        .bearer_auth(&admin_key)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 200);

    let (c6, _) = h
        .admin_json(
            reqwest::Method::POST,
            &format!("/admin/api/admin-keys/{aid}/revoke"),
            None,
        )
        .await;
    assert_eq!(c6, 200);
    let me2 = h
        .client()
        .get(format!("{}/admin/api/me", h.base))
        .bearer_auth(&admin_key)
        .send()
        .await
        .unwrap();
    assert_eq!(me2.status().as_u16(), 401);

    let del = h
        .client()
        .delete(format!("{}/admin/api/sessions/{sid}", h.base))
        .bearer_auth(&h.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);
    let gone = h
        .client()
        .get(format!("{}/v1/sessions/{sid}", h.base))
        .bearer_auth(&h.tenant_key)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status().as_u16(), 404);
}

#[test]
fn settings_unit() {
    use rupi_server::admin::merge_allowed_settings;
    assert!(merge_allowed_settings(json!({}), &json!({"notAKey": 1})).is_err());
    let m = merge_allowed_settings(json!({"api_key": "keep"}), &json!({"api_key": "***"})).unwrap();
    assert_eq!(m["api_key"], "keep");
}

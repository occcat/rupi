//! PostgreSQL 权威存储。所有查询带 `tenant_id`。

use chrono::{DateTime, Utc};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use rupi_core::{Message, Role, SessionNode, SessionTree};
use serde_json::Value;
use tokio_postgres::NoTls;
use uuid::Uuid;

pub type PgPool = Pool;

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let cfg: tokio_postgres::Config = database_url.parse()?;
    let mgr = Manager::from_config(
        cfg,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Ok(Pool::builder(mgr).max_size(32).build()?)
}

pub async fn connect_with_size(database_url: &str, max_size: usize) -> anyhow::Result<PgPool> {
    let cfg: tokio_postgres::Config = database_url.parse()?;
    let mgr = Manager::from_config(
        cfg,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Ok(Pool::builder(mgr).max_size(max_size.max(1)).build()?)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.batch_execute(
        r#"
        CREATE TABLE IF NOT EXISTS tenants (
          id TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
          settings JSONB NOT NULL DEFAULT '{}',
          default_model TEXT,
          max_concurrent_runs INT NOT NULL DEFAULT 2,
          max_handles INT NOT NULL DEFAULT 8
        );
        CREATE TABLE IF NOT EXISTS api_keys (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
          key_hash TEXT NOT NULL UNIQUE,
          key_prefix TEXT NOT NULL,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS sessions (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
          name TEXT,
          model TEXT,
          thinking_level TEXT,
          auto_compaction BOOLEAN NOT NULL DEFAULT true,
          runtime_backend TEXT,
          runtime_handle TEXT,
          parent_session TEXT,
          summary TEXT,
          summary_through TEXT,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
          updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
          run_id TEXT
        );
        CREATE INDEX IF NOT EXISTS sessions_tenant ON sessions(tenant_id, updated_at DESC);
        CREATE TABLE IF NOT EXISTS messages (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL,
          session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
          parent TEXT,
          role TEXT NOT NULL,
          content TEXT NOT NULL,
          blocks JSONB,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS messages_session ON messages(tenant_id, session_id);
        CREATE TABLE IF NOT EXISTS memories (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
          session_id TEXT,
          scope TEXT NOT NULL,
          layer TEXT NOT NULL DEFAULT 'extended',
          content TEXT NOT NULL,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS memories_tenant ON memories(tenant_id);
        CREATE TABLE IF NOT EXISTS interrupts (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL,
          session_id TEXT NOT NULL,
          run_id TEXT NOT NULL,
          tool_call_id TEXT NOT NULL,
          tool TEXT NOT NULL,
          args JSONB NOT NULL,
          reason TEXT NOT NULL,
          status TEXT NOT NULL DEFAULT 'pending',
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS quota_ledger (
          tenant_id TEXT NOT NULL,
          day DATE NOT NULL,
          tokens BIGINT NOT NULL DEFAULT 0,
          runs INT NOT NULL DEFAULT 0,
          PRIMARY KEY (tenant_id, day)
        );
        CREATE TABLE IF NOT EXISTS workspace_snapshots (
          id TEXT PRIMARY KEY,
          tenant_id TEXT NOT NULL,
          session_id TEXT NOT NULL,
          object_key TEXT NOT NULL,
          bytes BIGINT NOT NULL DEFAULT 0,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS snapshots_session ON workspace_snapshots(tenant_id, session_id);
        "#,
    )
    .await?;
    let _ = c
        .batch_execute(
            "ALTER TABLE tenants ADD COLUMN IF NOT EXISTS max_runs_per_day INT NOT NULL DEFAULT 10000;
             ALTER TABLE tenants ADD COLUMN IF NOT EXISTS max_tokens_per_day BIGINT NOT NULL DEFAULT 100000000;
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS instance_id TEXT;
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS workspace_state TEXT NOT NULL DEFAULT 'hot';
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS snapshot_key TEXT;
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ;
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS region TEXT;
             ALTER TABLE sessions ADD COLUMN IF NOT EXISTS runtime_kind TEXT;
             ALTER TABLE tenants ADD COLUMN IF NOT EXISTS default_region TEXT;
             ALTER TABLE tenants ADD COLUMN IF NOT EXISTS max_qps INT NOT NULL DEFAULT 8;",
        )
        .await;
    let _ = c.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm;").await;
    let _ = c
        .batch_execute(
            "CREATE INDEX IF NOT EXISTS memories_trgm ON memories USING gin (content gin_trgm_ops);
             CREATE INDEX IF NOT EXISTS messages_trgm ON messages USING gin (content gin_trgm_ops);
             CREATE INDEX IF NOT EXISTS messages_tenant_created ON messages(tenant_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS memories_tenant_created ON memories(tenant_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS sessions_tenant_run ON sessions(tenant_id) WHERE run_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS sessions_idle ON sessions(tenant_id, last_used_at)
               WHERE workspace_state = 'hot';",
        )
        .await;
    // FTS：simple 配置对中英都可用；查询仍带 tenant_id，按 tenant 分区时可剪枝。
    let _ = c
        .batch_execute(
            "ALTER TABLE messages ADD COLUMN IF NOT EXISTS content_tsv tsvector
               GENERATED ALWAYS AS (to_tsvector('simple', coalesce(content, ''))) STORED;
             ALTER TABLE memories ADD COLUMN IF NOT EXISTS content_tsv tsvector
               GENERATED ALWAYS AS (to_tsvector('simple', coalesce(content, ''))) STORED;
             CREATE INDEX IF NOT EXISTS messages_tsv ON messages USING gin (content_tsv);
             CREATE INDEX IF NOT EXISTS memories_tsv ON memories USING gin (content_tsv);",
        )
        .await;
    let _ = c
        .batch_execute(
            "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS revoked_at TIMESTAMPTZ;
             CREATE TABLE IF NOT EXISTS admin_keys (
               id TEXT PRIMARY KEY,
               key_hash TEXT NOT NULL UNIQUE,
               key_prefix TEXT NOT NULL,
               created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
               revoked_at TIMESTAMPTZ
             );",
        )
        .await;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub settings: Value,
    pub default_model: Option<String>,
    pub max_concurrent_runs: i32,
    pub max_handles: i32,
    pub max_runs_per_day: i32,
    pub max_tokens_per_day: i64,
    pub default_region: Option<String>,
    pub max_qps: i32,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub tenant_id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub auto_compaction: bool,
    pub runtime_backend: Option<String>,
    pub runtime_handle: Option<String>,
    pub parent_session: Option<String>,
    pub summary: Option<String>,
    pub summary_through: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub run_id: Option<String>,
    pub instance_id: Option<String>,
    pub workspace_state: Option<String>,
    pub snapshot_key: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub region: Option<String>,
    pub runtime_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InterruptRow {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub tool_call_id: String,
    pub tool: String,
    pub args: Value,
    pub reason: String,
    pub status: String,
}

pub async fn create_tenant(pool: &PgPool, name: &str, raw_key: &str) -> anyhow::Result<Tenant> {
    let id = Uuid::new_v4().to_string();
    let hash = crate::auth::hash_key(raw_key);
    let prefix: String = raw_key.chars().take(12).collect();
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO tenants(id, name) VALUES ($1, $2)",
        &[&id, &name],
    )
    .await?;
    c.execute(
        "INSERT INTO api_keys(id, tenant_id, key_hash, key_prefix) VALUES ($1, $2, $3, $4)",
        &[&Uuid::new_v4().to_string(), &id, &hash, &prefix],
    )
    .await?;
    load_tenant(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant insert vanished"))
}

pub async fn load_tenant(pool: &PgPool, id: &str) -> anyhow::Result<Option<Tenant>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT id, name, settings, default_model, max_concurrent_runs, max_handles,
                    COALESCE(max_runs_per_day, 10000), COALESCE(max_tokens_per_day, 100000000),
                    default_region, COALESCE(max_qps, 8)
             FROM tenants WHERE id = $1",
            &[&id],
        )
        .await?;
    Ok(row.as_ref().map(map_tenant))
}

pub async fn tenant_by_key_hash(pool: &PgPool, hash: &str) -> anyhow::Result<Option<Tenant>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT t.id, t.name, t.settings, t.default_model, t.max_concurrent_runs, t.max_handles,
                    COALESCE(t.max_runs_per_day, 10000), COALESCE(t.max_tokens_per_day, 100000000),
                    t.default_region, COALESCE(t.max_qps, 8)
             FROM api_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE k.key_hash = $1 AND k.revoked_at IS NULL",
            &[&hash],
        )
        .await?;
    Ok(row.as_ref().map(map_tenant))
}

fn map_tenant(r: &tokio_postgres::Row) -> Tenant {
    Tenant {
        id: r.get(0),
        name: r.get(1),
        settings: r.get(2),
        default_model: r.get(3),
        max_concurrent_runs: r.get(4),
        max_handles: r.get(5),
        max_runs_per_day: r.get(6),
        max_tokens_per_day: r.get(7),
        default_region: r.get(8),
        max_qps: r.get(9),
    }
}

pub async fn set_tenant_region(pool: &PgPool, tenant_id: &str, region: &str) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE tenants SET default_region = $2 WHERE id = $1",
        &[&tenant_id, &region],
    )
    .await?;
    Ok(())
}

pub async fn set_tenant_caps(
    pool: &PgPool,
    tenant_id: &str,
    max_concurrent_runs: i32,
    max_handles: i32,
    max_runs_per_day: i32,
) -> anyhow::Result<()> {
    set_tenant_caps_ex(
        pool,
        tenant_id,
        max_concurrent_runs,
        max_handles,
        max_runs_per_day,
        8,
    )
    .await
}

pub async fn set_tenant_caps_ex(
    pool: &PgPool,
    tenant_id: &str,
    max_concurrent_runs: i32,
    max_handles: i32,
    max_runs_per_day: i32,
    max_qps: i32,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE tenants SET max_concurrent_runs = $2, max_handles = $3, max_runs_per_day = $4,
                max_qps = $5
         WHERE id = $1",
        &[
            &tenant_id,
            &max_concurrent_runs,
            &max_handles,
            &max_runs_per_day,
            &max_qps,
        ],
    )
    .await?;
    Ok(())
}

pub async fn list_tenant_ids(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    let c = pool.get().await?;
    let rows = c.query("SELECT id FROM tenants", &[]).await?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

pub async fn update_settings(pool: &PgPool, tenant_id: &str, settings: &Value) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE tenants SET settings = $2 WHERE id = $1",
        &[&tenant_id, settings],
    )
    .await?;
    Ok(())
}

fn map_session(r: &tokio_postgres::Row) -> SessionRow {
    SessionRow {
        id: r.get(0),
        tenant_id: r.get(1),
        name: r.get(2),
        model: r.get(3),
        thinking_level: r.get(4),
        auto_compaction: r.get(5),
        runtime_backend: r.get(6),
        runtime_handle: r.get(7),
        parent_session: r.get(8),
        summary: r.get(9),
        summary_through: r.get(10),
        created_at: r.get(11),
        updated_at: r.get(12),
        run_id: r.get(13),
        instance_id: r.get(14),
        workspace_state: r.get(15),
        snapshot_key: r.get(16),
        last_used_at: r.get(17),
        region: r.get(18),
        runtime_kind: r.get(19),
    }
}

const SESSION_COLS: &str = "id, tenant_id, name, model, thinking_level, auto_compaction,
    runtime_backend, runtime_handle, parent_session, summary, summary_through,
    created_at, updated_at, run_id, instance_id, workspace_state, snapshot_key, last_used_at,
    region, runtime_kind";

pub async fn insert_session(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    name: Option<&str>,
    model: Option<&str>,
    backend: Option<&str>,
    handle: Option<&str>,
    parent: Option<&str>,
    region: Option<&str>,
    runtime_kind: Option<&str>,
) -> anyhow::Result<SessionRow> {
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO sessions(id, tenant_id, name, model, runtime_backend, runtime_handle, parent_session, workspace_state, last_used_at, region, runtime_kind)
         VALUES ($1,$2,$3,$4,$5,$6,$7, 'hot', now(), $8, $9)",
        &[
            &id,
            &tenant_id,
            &name,
            &model,
            &backend,
            &handle,
            &parent,
            &region,
            &runtime_kind,
        ],
    )
    .await?;
    get_session(pool, tenant_id, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session insert vanished"))
}

pub async fn get_session(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
) -> anyhow::Result<Option<SessionRow>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = $1 AND tenant_id = $2"),
            &[&id, &tenant_id],
        )
        .await?;
    Ok(row.as_ref().map(map_session))
}

pub async fn session_owner(pool: &PgPool, id: &str) -> anyhow::Result<Option<String>> {
    let c = pool.get().await?;
    let row = c
        .query_opt("SELECT tenant_id FROM sessions WHERE id = $1", &[&id])
        .await?;
    Ok(row.map(|r| r.get(0)))
}

pub async fn list_sessions(pool: &PgPool, tenant_id: &str) -> anyhow::Result<Vec<SessionRow>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            &format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE tenant_id = $1 ORDER BY updated_at DESC"
            ),
            &[&tenant_id],
        )
        .await?;
    Ok(rows.iter().map(map_session).collect())
}

pub async fn count_sessions(pool: &PgPool, tenant_id: &str) -> anyhow::Result<i64> {
    let c = pool.get().await?;
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1",
            &[&tenant_id],
        )
        .await?
        .get(0);
    Ok(n)
}

/// 并发句柄：未 snapshot 的会话都占配额（含刚入院、尚未 alloc 完的）。
pub async fn count_hot_handles(pool: &PgPool, tenant_id: &str) -> anyhow::Result<i64> {
    let c = pool.get().await?;
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM sessions
             WHERE tenant_id = $1
               AND COALESCE(workspace_state, 'hot') <> 'snapshotted'",
            &[&tenant_id],
        )
        .await?
        .get(0);
    Ok(n)
}

/// 租户行锁下建会话。超额返回 `Ok(None)`，由调用方 `429`。
pub async fn insert_session_if_under_cap(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    name: Option<&str>,
    model: Option<&str>,
    backend: Option<&str>,
    handle: Option<&str>,
    parent: Option<&str>,
    region: Option<&str>,
    runtime_kind: Option<&str>,
) -> anyhow::Result<Option<SessionRow>> {
    let mut c = pool.get().await?;
    let tx = c.transaction().await?;
    let locked = tx
        .query_opt(
            "SELECT max_handles FROM tenants WHERE id = $1 FOR UPDATE",
            &[&tenant_id],
        )
        .await?;
    let Some(locked) = locked else {
        tx.rollback().await?;
        anyhow::bail!("tenant vanished");
    };
    let max: i32 = locked.get(0);
    let n: i64 = tx
        .query_one(
            "SELECT count(*) FROM sessions
             WHERE tenant_id = $1
               AND COALESCE(workspace_state, 'hot') <> 'snapshotted'",
            &[&tenant_id],
        )
        .await?
        .get(0);
    if n >= max as i64 {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.execute(
        "INSERT INTO sessions(id, tenant_id, name, model, runtime_backend, runtime_handle, parent_session, workspace_state, last_used_at, region, runtime_kind)
         VALUES ($1,$2,$3,$4,$5,$6,$7, 'hot', now(), $8, $9)",
        &[
            &id,
            &tenant_id,
            &name,
            &model,
            &backend,
            &handle,
            &parent,
            &region,
            &runtime_kind,
        ],
    )
    .await?;
    tx.commit().await?;
    get_session(pool, tenant_id, id).await
}

pub async fn count_active_runs(pool: &PgPool, tenant_id: &str) -> anyhow::Result<i64> {
    let c = pool.get().await?;
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM sessions
             WHERE tenant_id = $1 AND run_id IS NOT NULL AND run_id <> ''",
            &[&tenant_id],
        )
        .await?
        .get(0);
    Ok(n)
}

pub async fn update_session_meta(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    name: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
    auto_compaction: Option<bool>,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE sessions SET
            name = COALESCE($3, name),
            model = COALESCE($4, model),
            thinking_level = COALESCE($5, thinking_level),
            auto_compaction = COALESCE($6, auto_compaction),
            updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
        &[&id, &tenant_id, &name, &model, &thinking, &auto_compaction],
    )
    .await?;
    Ok(())
}

pub async fn set_runtime(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    backend: &str,
    handle: &str,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE sessions SET runtime_backend = $3, runtime_handle = $4, updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
        &[&id, &tenant_id, &backend, &handle],
    )
    .await?;
    Ok(())
}

pub async fn set_run_id(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    run_id: Option<&str>,
    instance_id: Option<&str>,
) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = if let Some(rid) = run_id {
        c.execute(
            "UPDATE sessions SET run_id = $3, instance_id = $4, last_used_at = now(), updated_at = now()
             WHERE id = $1 AND tenant_id = $2 AND (run_id IS NULL OR run_id = '')",
            &[&id, &tenant_id, &rid, &instance_id],
        )
        .await?
    } else {
        c.execute(
            "UPDATE sessions SET run_id = NULL, instance_id = NULL, updated_at = now()
             WHERE id = $1 AND tenant_id = $2",
            &[&id, &tenant_id],
        )
        .await?
    };
    Ok(n > 0)
}

pub async fn clear_run_id(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    run_id: &str,
) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE sessions SET run_id = NULL, instance_id = NULL, updated_at = now()
             WHERE id = $1 AND tenant_id = $2 AND run_id = $3",
            &[&id, &tenant_id, &run_id],
        )
        .await?;
    Ok(n > 0)
}

pub async fn touch_run(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    run_id: &str,
) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE sessions SET last_used_at = now(), updated_at = now()
             WHERE id = $1 AND tenant_id = $2 AND run_id = $3",
            &[&id, &tenant_id, &run_id],
        )
        .await?;
    Ok(n > 0)
}

pub async fn touch_session(pool: &PgPool, tenant_id: &str, id: &str) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE sessions SET last_used_at = now(), updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
        &[&id, &tenant_id],
    )
    .await?;
    Ok(())
}

/// 副本被杀后 Redis 租约过期，但 PG `run_id` 可能残留。扫掉超过 `older_secs` 没心跳的。
pub async fn reap_stale_runs(pool: &PgPool, older_secs: i64) -> anyhow::Result<u64> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE sessions SET run_id = NULL, instance_id = NULL, updated_at = now()
             WHERE run_id IS NOT NULL
               AND updated_at < now() - ($1::double precision * interval '1 second')",
            &[&(older_secs as f64)],
        )
        .await?;
    Ok(n)
}

pub async fn mark_snapshotted(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    snapshot_key: &str,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE sessions SET workspace_state = 'snapshotted', runtime_handle = NULL,
                snapshot_key = $3, updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
        &[&id, &tenant_id, &snapshot_key],
    )
    .await?;
    Ok(())
}

pub async fn mark_hot(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    backend: &str,
    handle: &str,
    snapshot_key: Option<&str>,
    kind: Option<&str>,
    region: Option<&str>,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE sessions SET workspace_state = 'hot', runtime_backend = $3, runtime_handle = $4,
                snapshot_key = COALESCE($5, snapshot_key), last_used_at = now(), updated_at = now(),
                runtime_kind = COALESCE($6, runtime_kind), region = COALESCE($7, region)
         WHERE id = $1 AND tenant_id = $2",
        &[
            &id,
            &tenant_id,
            &backend,
            &handle,
            &snapshot_key,
            &kind,
            &region,
        ],
    )
    .await?;
    Ok(())
}

pub async fn insert_snapshot(
    pool: &PgPool,
    tenant_id: &str,
    session_id: &str,
    object_key: &str,
    bytes: i64,
) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO workspace_snapshots(id, tenant_id, session_id, object_key, bytes)
         VALUES ($1,$2,$3,$4,$5)",
        &[&id, &tenant_id, &session_id, &object_key, &bytes],
    )
    .await?;
    Ok(id)
}

pub async fn list_reclaim_candidates(
    pool: &PgPool,
    tenant_id: Option<&str>,
    min_idle: std::time::Duration,
    limit: i64,
) -> anyhow::Result<Vec<SessionRow>> {
    let c = pool.get().await?;
    let secs = min_idle.as_secs_f64();
    let rows = if let Some(tid) = tenant_id {
        c.query(
            &format!(
                "SELECT {SESSION_COLS} FROM sessions
                 WHERE tenant_id = $1
                   AND COALESCE(workspace_state, 'hot') = 'hot'
                   AND runtime_handle IS NOT NULL
                   AND (run_id IS NULL OR run_id = '')
                   AND COALESCE(last_used_at, created_at) <= now() - ($2::double precision * interval '1 second')
                   AND NOT EXISTS (
                     SELECT 1 FROM interrupts i
                     WHERE i.session_id = sessions.id AND i.tenant_id = sessions.tenant_id
                       AND i.status = 'pending'
                   )
                 ORDER BY COALESCE(last_used_at, created_at) ASC
                 LIMIT $3"
            ),
            &[&tid, &secs, &limit],
        )
        .await?
    } else {
        c.query(
            &format!(
                "SELECT {SESSION_COLS} FROM sessions
                 WHERE COALESCE(workspace_state, 'hot') = 'hot'
                   AND runtime_handle IS NOT NULL
                   AND (run_id IS NULL OR run_id = '')
                   AND COALESCE(last_used_at, created_at) <= now() - ($1::double precision * interval '1 second')
                   AND NOT EXISTS (
                     SELECT 1 FROM interrupts i
                     WHERE i.session_id = sessions.id AND i.tenant_id = sessions.tenant_id
                       AND i.status = 'pending'
                   )
                 ORDER BY COALESCE(last_used_at, created_at) ASC
                 LIMIT $2"
            ),
            &[&secs, &limit],
        )
        .await?
    };
    Ok(rows.iter().map(map_session).collect())
}

pub async fn persist_tree(
    pool: &PgPool,
    tenant_id: &str,
    session_id: &str,
    tree: &SessionTree,
) -> anyhow::Result<()> {
    let mut c = pool.get().await?;
    let tx = c.transaction().await?;
    tx.execute(
        "DELETE FROM messages WHERE tenant_id = $1 AND session_id = $2",
        &[&tenant_id, &session_id],
    )
    .await?;
    let mut order: Vec<&String> = tree.nodes.keys().collect();
    order.sort_by_key(|id| tree.nodes[*id].created_at);
    for id in order {
        let node = &tree.nodes[id];
        let role = match node.message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let content = node.message.full_text();
        let blocks = serde_json::to_value(&node.message).unwrap_or(Value::Null);
        tx.execute(
            "INSERT INTO messages(id, tenant_id, session_id, parent, role, content, blocks, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            &[
                id,
                &tenant_id,
                &session_id,
                &node.parent,
                &role,
                &content,
                &blocks,
                &node.message.created_at,
            ],
        )
        .await?;
    }
    tx.execute(
        "UPDATE sessions SET summary = $3, summary_through = $4, updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
        &[
            &session_id,
            &tenant_id,
            &tree.summary,
            &tree.summary_through,
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn load_tree(
    pool: &PgPool,
    tenant_id: &str,
    session_id: &str,
) -> anyhow::Result<SessionTree> {
    let sess = get_session(pool, tenant_id, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT id, parent, role, content, blocks, created_at
             FROM messages WHERE tenant_id = $1 AND session_id = $2
             ORDER BY created_at ASC",
            &[&tenant_id, &session_id],
        )
        .await?;
    let mut tree = SessionTree::new();
    tree.id = session_id.to_string();
    tree.nodes.clear();
    tree.current_path.clear();
    tree.summary = sess.summary;
    tree.summary_through = sess.summary_through;
    for r in rows {
        let id: String = r.get(0);
        let parent: Option<String> = r.get(1);
        let role_s: String = r.get(2);
        let content: String = r.get(3);
        let blocks: Option<Value> = r.get(4);
        let created_at: DateTime<Utc> = r.get(5);
        let message = if let Some(Value::Object(_)) = &blocks {
            serde_json::from_value::<Message>(blocks.unwrap()).unwrap_or_else(|_| {
                Message::text(parse_role(&role_s), content)
            })
        } else {
            Message::text(parse_role(&role_s), content)
        };
        tree.nodes.insert(
            id.clone(),
            SessionNode {
                id: id.clone(),
                parent,
                message,
                summary: None,
                created_at,
            },
        );
        tree.current_path.push(id);
    }
    Ok(tree)
}

fn parse_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

pub async fn delete_session(pool: &PgPool, tenant_id: &str, id: &str) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "DELETE FROM sessions WHERE id = $1 AND tenant_id = $2",
            &[&id, &tenant_id],
        )
        .await?;
    Ok(n > 0)
}

pub async fn insert_memory(
    pool: &PgPool,
    tenant_id: &str,
    session_id: Option<&str>,
    scope: &str,
    content: &str,
) -> anyhow::Result<String> {
    if rupi_memory::contains_secret(content) {
        anyhow::bail!("refused: entry looks like a secret; store a reference instead");
    }
    let layer = if content.contains("[core]") {
        "core"
    } else if content.contains("[user]") {
        "user"
    } else if content.contains("[failure]") {
        "failure"
    } else {
        "extended"
    };
    let id = Uuid::new_v4().to_string();
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO memories(id, tenant_id, session_id, scope, layer, content)
         VALUES ($1,$2,$3,$4,$5,$6)",
        &[&id, &tenant_id, &session_id, &scope, &layer, &content],
    )
    .await?;
    Ok(id)
}

pub async fn list_memories(
    pool: &PgPool,
    tenant_id: &str,
    session_id: Option<&str>,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let c = pool.get().await?;
    let rows = if let Some(sid) = session_id {
        c.query(
            "SELECT layer, scope, content FROM memories
             WHERE tenant_id = $1 AND (session_id IS NULL OR session_id = $2)
             ORDER BY created_at ASC",
            &[&tenant_id, &sid],
        )
        .await?
    } else {
        c.query(
            "SELECT layer, scope, content FROM memories
             WHERE tenant_id = $1 ORDER BY created_at ASC",
            &[&tenant_id],
        )
        .await?
    };
    Ok(rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect())
}

pub async fn search_memories(
    pool: &PgPool,
    tenant_id: &str,
    query: &str,
    limit: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let c = pool.get().await?;
    let like = format!("%{query}%");
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let rows = match c
        .query(
            "SELECT scope, content FROM memories
             WHERE tenant_id = $1 AND (
               content ILIKE $2
               OR content_tsv @@ plainto_tsquery('simple', $3)
             )
             ORDER BY created_at DESC LIMIT $4",
            &[&tenant_id, &like, &q, &limit],
        )
        .await
    {
        Ok(r) => r,
        Err(_) => {
            c.query(
                "SELECT scope, content FROM memories
                 WHERE tenant_id = $1 AND content ILIKE $2
                 ORDER BY created_at DESC LIMIT $3",
                &[&tenant_id, &like, &limit],
            )
            .await?
        }
    };
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

pub async fn search_sessions(
    pool: &PgPool,
    tenant_id: &str,
    query: &str,
    limit: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let c = pool.get().await?;
    let like = format!("%{query}%");
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let rows = match c
        .query(
            "SELECT session_id, content FROM messages
             WHERE tenant_id = $1 AND (
               content ILIKE $2
               OR content_tsv @@ plainto_tsquery('simple', $3)
             )
             ORDER BY created_at DESC LIMIT $4",
            &[&tenant_id, &like, &q, &limit],
        )
        .await
    {
        Ok(r) => r,
        Err(_) => {
            c.query(
                "SELECT session_id, content FROM messages
                 WHERE tenant_id = $1 AND content ILIKE $2
                 ORDER BY created_at DESC LIMIT $3",
                &[&tenant_id, &like, &limit],
            )
            .await?
        }
    };
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

pub async fn insert_interrupt(
    pool: &PgPool,
    tenant_id: &str,
    session_id: &str,
    run_id: &str,
    tool_call_id: &str,
    tool: &str,
    args: &Value,
    reason: &str,
) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO interrupts(id, tenant_id, session_id, run_id, tool_call_id, tool, args, reason)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        &[
            &id,
            &tenant_id,
            &session_id,
            &run_id,
            &tool_call_id,
            &tool,
            args,
            &reason,
        ],
    )
    .await?;
    Ok(id)
}

pub async fn pending_interrupts(
    pool: &PgPool,
    tenant_id: &str,
    session_id: &str,
) -> anyhow::Result<Vec<InterruptRow>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT id, session_id, run_id, tool_call_id, tool, args, reason, status
             FROM interrupts WHERE tenant_id = $1 AND session_id = $2 AND status = 'pending'
             ORDER BY created_at ASC",
            &[&tenant_id, &session_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| InterruptRow {
            id: r.get(0),
            session_id: r.get(1),
            run_id: r.get(2),
            tool_call_id: r.get(3),
            tool: r.get(4),
            args: r.get(5),
            reason: r.get(6),
            status: r.get(7),
        })
        .collect())
}

pub async fn set_interrupt_status(
    pool: &PgPool,
    tenant_id: &str,
    id: &str,
    status: &str,
) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "UPDATE interrupts SET status = $3 WHERE id = $1 AND tenant_id = $2",
        &[&id, &tenant_id, &status],
    )
    .await?;
    Ok(())
}

pub async fn bump_quota_run(pool: &PgPool, tenant_id: &str) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO quota_ledger(tenant_id, day, runs) VALUES ($1, CURRENT_DATE, 1)
         ON CONFLICT (tenant_id, day) DO UPDATE SET runs = quota_ledger.runs + 1",
        &[&tenant_id],
    )
    .await?;
    Ok(())
}

pub async fn bump_quota_tokens(pool: &PgPool, tenant_id: &str, tokens: i64) -> anyhow::Result<()> {
    if tokens <= 0 {
        return Ok(());
    }
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO quota_ledger(tenant_id, day, tokens) VALUES ($1, CURRENT_DATE, $2)
         ON CONFLICT (tenant_id, day) DO UPDATE SET tokens = quota_ledger.tokens + $2",
        &[&tenant_id, &tokens],
    )
    .await?;
    Ok(())
}

pub async fn today_quota(pool: &PgPool, tenant_id: &str) -> anyhow::Result<(i64, i64)> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT COALESCE(tokens, 0)::bigint, COALESCE(runs, 0)::bigint
             FROM quota_ledger WHERE tenant_id = $1 AND day = CURRENT_DATE",
            &[&tenant_id],
        )
        .await?;
    Ok(row
        .map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1)))
        .unwrap_or((0, 0)))
}

/// 测试用：清掉本库云表（不碰别的库）。
pub async fn reset_all(pool: &PgPool) -> anyhow::Result<()> {
    let c = pool.get().await?;
    c.batch_execute(
        "TRUNCATE interrupts, memories, messages, sessions, api_keys, admin_keys, quota_ledger, workspace_snapshots, tenants CASCADE",
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TenantListItem {
    pub tenant: Tenant,
    pub created_at: DateTime<Utc>,
    pub session_count: i64,
    pub key_count: i64,
}

#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    pub id: String,
    pub tenant_id: String,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct AdminKeyRow {
    pub id: String,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct OverviewCounts {
    pub tenants: i64,
    pub sessions: i64,
    pub running: i64,
    pub snapshotted: i64,
    pub allocated: i64,
    pub keys_active: i64,
}

#[derive(Debug, Clone)]
pub struct HandleGroup {
    pub backend: String,
    pub region: String,
    pub kind: String,
    pub allocated: i64,
    pub hot: i64,
    pub snapshotted: i64,
}

fn map_api_key(r: &tokio_postgres::Row) -> ApiKeyRow {
    ApiKeyRow {
        id: r.get(0),
        tenant_id: r.get(1),
        key_prefix: r.get(2),
        created_at: r.get(3),
        revoked_at: r.get(4),
    }
}

fn map_admin_key(r: &tokio_postgres::Row) -> AdminKeyRow {
    AdminKeyRow {
        id: r.get(0),
        key_prefix: r.get(1),
        created_at: r.get(2),
        revoked_at: r.get(3),
    }
}

pub async fn count_tenants(pool: &PgPool, query: &str) -> anyhow::Result<i64> {
    let c = pool.get().await?;
    let like = format!("%{query}%");
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM tenants
             WHERE $1 = '' OR name ILIKE $2 OR id ILIKE $2",
            &[&query, &like],
        )
        .await?
        .get(0);
    Ok(n)
}

pub async fn list_tenants(
    pool: &PgPool,
    query: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<TenantListItem>> {
    let c = pool.get().await?;
    let like = format!("%{query}%");
    let rows = c
        .query(
            "SELECT t.id, t.name, t.settings, t.default_model, t.max_concurrent_runs, t.max_handles,
                    COALESCE(t.max_runs_per_day, 10000), COALESCE(t.max_tokens_per_day, 100000000),
                    t.default_region, COALESCE(t.max_qps, 8), t.created_at,
                    (SELECT count(*) FROM sessions s WHERE s.tenant_id = t.id),
                    (SELECT count(*) FROM api_keys k WHERE k.tenant_id = t.id AND k.revoked_at IS NULL)
             FROM tenants t
             WHERE $1 = '' OR t.name ILIKE $2 OR t.id ILIKE $2
             ORDER BY t.created_at DESC
             LIMIT $3 OFFSET $4",
            &[&query, &like, &limit, &offset],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| TenantListItem {
            tenant: map_tenant(r),
            created_at: r.get(10),
            session_count: r.get(11),
            key_count: r.get(12),
        })
        .collect())
}

pub async fn patch_tenant_meta(
    pool: &PgPool,
    tenant_id: &str,
    name: Option<&str>,
    default_model: Option<&str>,
    default_region: Option<&str>,
) -> anyhow::Result<u64> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE tenants SET
                name = COALESCE($2, name),
                default_model = COALESCE($3, default_model),
                default_region = COALESCE($4, default_region)
             WHERE id = $1",
            &[&tenant_id, &name, &default_model, &default_region],
        )
        .await?;
    Ok(n)
}

pub async fn patch_tenant_quota(
    pool: &PgPool,
    tenant_id: &str,
    max_concurrent_runs: Option<i32>,
    max_handles: Option<i32>,
    max_runs_per_day: Option<i32>,
    max_tokens_per_day: Option<i64>,
    max_qps: Option<i32>,
) -> anyhow::Result<u64> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE tenants SET
                max_concurrent_runs = COALESCE($2, max_concurrent_runs),
                max_handles = COALESCE($3, max_handles),
                max_runs_per_day = COALESCE($4, max_runs_per_day),
                max_tokens_per_day = COALESCE($5, max_tokens_per_day),
                max_qps = COALESCE($6, max_qps)
             WHERE id = $1",
            &[
                &tenant_id,
                &max_concurrent_runs,
                &max_handles,
                &max_runs_per_day,
                &max_tokens_per_day,
                &max_qps,
            ],
        )
        .await?;
    Ok(n)
}

pub async fn quota_history(
    pool: &PgPool,
    tenant_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT day::text, COALESCE(tokens, 0)::bigint, COALESCE(runs, 0)::bigint
             FROM quota_ledger WHERE tenant_id = $1
             ORDER BY day DESC LIMIT $2",
            &[&tenant_id, &limit],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect())
}

pub async fn list_api_keys(pool: &PgPool, tenant_id: &str) -> anyhow::Result<Vec<ApiKeyRow>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT id, tenant_id, key_prefix, created_at, revoked_at
             FROM api_keys WHERE tenant_id = $1
             ORDER BY created_at DESC",
            &[&tenant_id],
        )
        .await?;
    Ok(rows.iter().map(map_api_key).collect())
}

pub async fn get_api_key(pool: &PgPool, id: &str) -> anyhow::Result<Option<ApiKeyRow>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT id, tenant_id, key_prefix, created_at, revoked_at
             FROM api_keys WHERE id = $1",
            &[&id],
        )
        .await?;
    Ok(row.as_ref().map(map_api_key))
}

pub async fn create_api_key(
    pool: &PgPool,
    tenant_id: &str,
    raw_key: &str,
) -> anyhow::Result<ApiKeyRow> {
    if load_tenant(pool, tenant_id).await?.is_none() {
        anyhow::bail!("tenant vanished");
    }
    let id = Uuid::new_v4().to_string();
    let hash = crate::auth::hash_key(raw_key);
    let prefix = crate::auth::key_prefix(raw_key, 12);
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO api_keys(id, tenant_id, key_hash, key_prefix) VALUES ($1, $2, $3, $4)",
        &[&id, &tenant_id, &hash, &prefix],
    )
    .await?;
    get_api_key(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("api key vanished"))
}

pub async fn revoke_api_key(pool: &PgPool, id: &str) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
            &[&id],
        )
        .await?;
    Ok(n > 0)
}

pub async fn list_admin_keys(pool: &PgPool) -> anyhow::Result<Vec<AdminKeyRow>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT id, key_prefix, created_at, revoked_at FROM admin_keys
             ORDER BY created_at DESC",
            &[],
        )
        .await?;
    Ok(rows.iter().map(map_admin_key).collect())
}

pub async fn get_admin_key(pool: &PgPool, id: &str) -> anyhow::Result<Option<AdminKeyRow>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT id, key_prefix, created_at, revoked_at FROM admin_keys WHERE id = $1",
            &[&id],
        )
        .await?;
    Ok(row.as_ref().map(map_admin_key))
}

pub async fn create_admin_key(pool: &PgPool, raw_key: &str) -> anyhow::Result<AdminKeyRow> {
    let id = Uuid::new_v4().to_string();
    let hash = crate::auth::hash_key(raw_key);
    let prefix = crate::auth::key_prefix(raw_key, 16);
    let c = pool.get().await?;
    c.execute(
        "INSERT INTO admin_keys(id, key_hash, key_prefix) VALUES ($1, $2, $3)",
        &[&id, &hash, &prefix],
    )
    .await?;
    get_admin_key(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("admin key vanished"))
}

pub async fn revoke_admin_key(pool: &PgPool, id: &str) -> anyhow::Result<bool> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "UPDATE admin_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
            &[&id],
        )
        .await?;
    Ok(n > 0)
}

pub async fn admin_by_key_hash(pool: &PgPool, hash: &str) -> anyhow::Result<Option<AdminKeyRow>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "SELECT id, key_prefix, created_at, revoked_at FROM admin_keys
             WHERE key_hash = $1 AND revoked_at IS NULL",
            &[&hash],
        )
        .await?;
    Ok(row.as_ref().map(map_admin_key))
}

pub async fn get_session_any(pool: &PgPool, id: &str) -> anyhow::Result<Option<SessionRow>> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = $1"),
            &[&id],
        )
        .await?;
    Ok(row.as_ref().map(map_session))
}

fn session_status_sql() -> &'static str {
    r#"(
        $3 = ''
        OR ($3 = 'running' AND run_id IS NOT NULL AND run_id <> '')
        OR ($3 = 'snapshotted' AND COALESCE(workspace_state, 'hot') = 'snapshotted')
        OR ($3 = 'ready' AND runtime_handle IS NOT NULL
            AND COALESCE(workspace_state, 'hot') <> 'snapshotted')
        OR ($3 = 'hot' AND COALESCE(workspace_state, 'hot') = 'hot')
        OR ($3 = 'none' AND runtime_handle IS NULL
            AND COALESCE(workspace_state, 'hot') <> 'snapshotted'
            AND (run_id IS NULL OR run_id = ''))
    )"#
}

pub async fn list_sessions_admin(
    pool: &PgPool,
    tenant_id: &str,
    region: &str,
    status: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<SessionRow>> {
    let c = pool.get().await?;
    let sql = format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE ($1 = '' OR tenant_id = $1)
           AND ($2 = '' OR region = $2)
           AND {}
         ORDER BY updated_at DESC
         LIMIT $4 OFFSET $5",
        session_status_sql()
    );
    let rows = c
        .query(&sql, &[&tenant_id, &region, &status, &limit, &offset])
        .await?;
    Ok(rows.iter().map(map_session).collect())
}

pub async fn count_sessions_admin(
    pool: &PgPool,
    tenant_id: &str,
    region: &str,
    status: &str,
) -> anyhow::Result<i64> {
    let c = pool.get().await?;
    let sql = format!(
        "SELECT count(*) FROM sessions
         WHERE ($1 = '' OR tenant_id = $1)
           AND ($2 = '' OR region = $2)
           AND {}",
        session_status_sql()
    );
    let n: i64 = c
        .query_one(&sql, &[&tenant_id, &region, &status])
        .await?
        .get(0);
    Ok(n)
}

pub async fn overview_counts(pool: &PgPool) -> anyhow::Result<OverviewCounts> {
    let c = pool.get().await?;
    let row = c
        .query_one(
            "SELECT
                (SELECT count(*) FROM tenants),
                (SELECT count(*) FROM sessions),
                (SELECT count(*) FROM sessions WHERE run_id IS NOT NULL AND run_id <> ''),
                (SELECT count(*) FROM sessions WHERE COALESCE(workspace_state, 'hot') = 'snapshotted'),
                (SELECT count(*) FROM sessions WHERE runtime_handle IS NOT NULL),
                (SELECT count(*) FROM api_keys WHERE revoked_at IS NULL)",
            &[],
        )
        .await?;
    Ok(OverviewCounts {
        tenants: row.get(0),
        sessions: row.get(1),
        running: row.get(2),
        snapshotted: row.get(3),
        allocated: row.get(4),
        keys_active: row.get(5),
    })
}

pub async fn handle_groups(pool: &PgPool) -> anyhow::Result<Vec<HandleGroup>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT COALESCE(runtime_backend, '(none)'),
                    COALESCE(region, '(none)'),
                    COALESCE(runtime_kind, '(none)'),
                    count(*) FILTER (WHERE runtime_handle IS NOT NULL),
                    count(*) FILTER (WHERE COALESCE(workspace_state, 'hot') = 'hot'),
                    count(*) FILTER (WHERE COALESCE(workspace_state, 'hot') = 'snapshotted')
             FROM sessions
             GROUP BY 1, 2, 3
             ORDER BY 1, 2, 3",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| HandleGroup {
            backend: r.get(0),
            region: r.get(1),
            kind: r.get(2),
            allocated: r.get(3),
            hot: r.get(4),
            snapshotted: r.get(5),
        })
        .collect())
}

pub async fn list_session_regions(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT DISTINCT region FROM sessions
             WHERE region IS NOT NULL AND region <> ''
             ORDER BY 1",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

pub async fn list_tenant_regions(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "SELECT DISTINCT default_region FROM tenants
             WHERE default_region IS NOT NULL AND default_region <> ''
             ORDER BY 1",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

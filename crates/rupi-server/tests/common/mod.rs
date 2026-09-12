//! 跨测试二进制互斥：`cloud_first_cut` 与 `cloud_scale` 会并行打同一套 Postgres。

use tokio_postgres::NoTls;

/// 持有 `pg_advisory_lock` 的连接。drop 即释放。
pub struct HarnessGuard {
    _client: tokio_postgres::Client,
    _driver: tokio::task::JoinHandle<()>,
}

pub async fn lock_harness() -> Option<HarnessGuard> {
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

//! 云记忆/会话检索：中文词必须能命中（FTS `simple` 不够，靠 pg_trgm + CJK n-gram）。
//! 无 Postgres 时跳过，与其他 cloud_* 测试相同。

mod common;

use rupi_core::{Message, Role, SessionTree};
use rupi_server::auth;
use rupi_server::db;

use common::lock_harness;

fn db_url() -> Option<String> {
    std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| Some("postgresql://rupi:rupi@127.0.0.1:5432/rupi".into()))
}

async fn connect_or_skip() -> Option<(common::HarnessGuard, db::PgPool)> {
    let guard = lock_harness().await?;
    let url = db_url()?;
    let pool = match db::connect(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip: no postgres at {url}: {e:#}");
            return None;
        }
    };
    if let Err(e) = db::migrate(&pool).await {
        eprintln!("skip: migrate failed: {e:#}");
        return None;
    }
    Some((guard, pool))
}

#[tokio::test]
async fn chinese_query_hits_memory_and_session() {
    let Some((_guard, pool)) = connect_or_skip().await else {
        return;
    };
    let _ = db::reset_all(&pool).await;
    let key_a = auth::generate_key();
    let key_b = auth::generate_key();
    let ta = db::create_tenant(&pool, "fts-zh-a", &key_a)
        .await
        .expect("tenant a");
    let tb = db::create_tenant(&pool, "fts-zh-b", &key_b)
        .await
        .expect("tenant b");

    db::insert_memory(
        &pool,
        &ta.id,
        None,
        "tenant",
        "请记住我爱喝乌龙茶，不要加糖",
    )
    .await
    .expect("insert zh memory");
    db::insert_memory(&pool, &ta.id, None, "tenant", "scale-fts-needle")
        .await
        .expect("insert en memory");

    let tea = db::search_memories(&pool, &ta.id, "乌龙茶", 5)
        .await
        .expect("search 乌龙茶");
    assert!(
        tea.iter().any(|(_, c)| c.contains("乌龙茶")),
        "中文词「乌龙茶」未召回: {tea:?}"
    );

    let oolong = db::search_memories(&pool, &ta.id, "乌龙", 5)
        .await
        .expect("search 乌龙");
    assert!(
        oolong.iter().any(|(_, c)| c.contains("乌龙茶")),
        "中文子串「乌龙」未召回: {oolong:?}"
    );

    let sugar = db::search_memories(&pool, &ta.id, "加糖", 5)
        .await
        .expect("search 加糖");
    assert!(
        sugar.iter().any(|(_, c)| c.contains("加糖")),
        "中文词「加糖」未召回: {sugar:?}"
    );

    let en = db::search_memories(&pool, &ta.id, "needle", 5)
        .await
        .expect("search needle");
    assert!(
        en.iter().any(|(_, c)| c.contains("scale-fts-needle")),
        "英文 needle 回归失败: {en:?}"
    );

    let isolated = db::search_memories(&pool, &tb.id, "乌龙茶", 5)
        .await
        .expect("tenant b search");
    assert!(isolated.is_empty(), "跨租户漏出: {isolated:?}");

    // 对准 n-gram：不靠 ILIKE，content_tsv 单独用中文 bigram 也能命中。
    {
        let c = pool.get().await.expect("conn");
        let ngrams: String = c
            .query_one("SELECT rupi_cjk_ngrams($1)", &[&"请记住我爱喝乌龙茶"])
            .await
            .expect("rupi_cjk_ngrams")
            .get(0);
        assert!(
            ngrams.contains("乌龙") && ngrams.contains("龙茶"),
            "CJK n-gram 未覆盖词面: {ngrams}"
        );
        let n: i64 = c
            .query_one(
                "SELECT count(*) FROM memories
                 WHERE tenant_id = $1
                   AND content_tsv @@ to_tsquery('simple', '乌龙 & 龙茶')",
                &[&ta.id],
            )
            .await
            .expect("tsv bigram")
            .get(0);
        assert!(n >= 1, "content_tsv 缺少 CJK bigram，中文 FTS 仍只靠 ILIKE");
        let sim: f32 = c
            .query_one(
                "SELECT word_similarity('乌龙茶', content) FROM memories
                 WHERE tenant_id = $1 AND content LIKE '%乌龙茶%' LIMIT 1",
                &[&ta.id],
            )
            .await
            .expect("word_similarity")
            .get(0);
        assert!(
            sim > 0.0,
            "pg_trgm word_similarity 对中文词为 0（扩展或策略未生效）: {sim}"
        );
    }

    let sid = "fts-zh-session";
    db::insert_session(
        &pool,
        &ta.id,
        sid,
        Some("cjk"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("session");
    let mut tree = SessionTree::new();
    tree.id = sid.to_string();
    tree.push(Message::text(Role::User, "昨天聊过乌龙茶的冲泡温度"));
    db::persist_tree(&pool, &ta.id, sid, &tree)
        .await
        .expect("persist tree");

    let sessions = db::search_sessions(&pool, &ta.id, "冲泡", 5)
        .await
        .expect("session search");
    assert!(
        sessions
            .iter()
            .any(|(id, c)| id == sid && c.contains("冲泡")),
        "会话中文词「冲泡」未召回: {sessions:?}"
    );
}

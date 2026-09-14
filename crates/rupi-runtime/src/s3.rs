//! S3 兼容对象存储。多副本共享同一桶，闲置快照才能跨控制面读到。

use crate::ObjectStore;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub prefix: String,
}

impl S3Config {
    /// `s3://bucket/prefix` + 环境变量；或 `http(s)://host/bucket`。
    pub fn from_uri(uri: &str) -> anyhow::Result<Self> {
        let uri = uri.trim();
        let (bucket, prefix) = if let Some(rest) = uri.strip_prefix("s3://") {
            let (b, p) = rest.split_once('/').unwrap_or((rest, ""));
            (b.to_string(), p.trim_matches('/').to_string())
        } else if let Some((_, rest)) = uri.split_once("://") {
            let mut parts = rest.splitn(2, '/');
            let _host = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");
            let (b, p) = path.split_once('/').unwrap_or((path, ""));
            (b.to_string(), p.trim_matches('/').to_string())
        } else {
            anyhow::bail!("snapshot uri must be s3://bucket/prefix or https://host/bucket");
        };
        if bucket.is_empty() {
            anyhow::bail!("s3 bucket required");
        }
        let endpoint = std::env::var("RUPI_S3_ENDPOINT")
            .or_else(|_| std::env::var("AWS_ENDPOINT_URL"))
            .unwrap_or_else(|_| "https://s3.amazonaws.com".into());
        let region = std::env::var("RUPI_S3_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .unwrap_or_else(|_| "us-east-1".into());
        let access_key = std::env::var("RUPI_S3_ACCESS_KEY")
            .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
            .map_err(|_| anyhow::anyhow!("RUPI_S3_ACCESS_KEY or AWS_ACCESS_KEY_ID required"))?;
        let secret_key = std::env::var("RUPI_S3_SECRET_KEY")
            .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
            .map_err(|_| anyhow::anyhow!("RUPI_S3_SECRET_KEY or AWS_SECRET_ACCESS_KEY required"))?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket,
            region,
            access_key,
            secret_key,
            prefix,
        })
    }

    fn object_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), key)
        }
    }

    fn object_path(&self, key: &str) -> String {
        uri_encode_path(&format!("{}/{}", self.bucket, self.object_key(key)))
    }
}

#[derive(Clone)]
pub struct S3ObjectStore {
    cfg: S3Config,
    client: reqwest::Client,
}

impl S3ObjectStore {
    pub fn new(cfg: S3Config) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.cfg.endpoint,
            self.cfg.bucket,
            self.cfg.object_key(key)
        )
    }

    fn bucket_url(&self) -> String {
        format!("{}/{}", self.cfg.endpoint, self.cfg.bucket)
    }

    fn host_of(url: &str) -> String {
        url.split_once("://")
            .and_then(|(_, r)| r.split('/').next())
            .unwrap_or("")
            .to_string()
    }

    fn authorization(
        &self,
        method: &str,
        canon_uri: &str,
        body: &[u8],
        host: &str,
    ) -> (String, String, String) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ts = chrono_like(now);
        let date = &ts[..8];
        let payload = hex::encode(Sha256::digest(body));
        let canon = format!(
            "{method}\n{canon_uri}\n\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{ts}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload}"
        );
        let canon_hash = hex::encode(Sha256::digest(canon.as_bytes()));
        let scope = format!("{date}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{ts}\n{scope}\n{canon_hash}");
        let signing = signing_key(&self.cfg.secret_key, date, &self.cfg.region);
        let sig = hex::encode(hmac_sha256(&signing, string_to_sign.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig}",
            self.cfg.access_key
        );
        (auth, payload, ts)
    }

    async fn signed(
        &self,
        method: &str,
        key: &str,
        body: &[u8],
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let url = self.url(key);
        let host = Self::host_of(&url);
        let canon_uri = format!("/{}", self.cfg.object_path(key));
        let (auth, payload, ts) = self.authorization(method, &canon_uri, body, &host);
        let mut req = match method {
            "PUT" => self.client.put(&url),
            "GET" => self.client.get(&url),
            "DELETE" => self.client.delete(&url),
            _ => anyhow::bail!("s3 method"),
        };
        req = req
            .header("host", host)
            .header("x-amz-content-sha256", payload)
            .header("x-amz-date", ts)
            .header("authorization", auth);
        if method == "PUT" {
            req = req.body(body.to_vec());
        }
        Ok(req)
    }

    /// 建桶（MinIO / S3 兼容）。已存在则忽略。
    pub async fn ensure_bucket(&self) -> anyhow::Result<()> {
        let url = self.bucket_url();
        let host = Self::host_of(&url);
        let canon_uri = format!("/{}", uri_encode_path(&self.cfg.bucket));
        let (auth, payload, ts) = self.authorization("PUT", &canon_uri, b"", &host);
        let resp = self
            .client
            .put(&url)
            .header("host", host)
            .header("x-amz-content-sha256", payload)
            .header("x-amz-date", ts)
            .header("authorization", auth)
            .body(Vec::<u8>::new())
            .send()
            .await?;
        let st = resp.status().as_u16();
        if st == 200 || st == 409 || st == 204 {
            return Ok(());
        }
        anyhow::bail!("s3 create bucket {}: {}", self.cfg.bucket, resp.status())
    }
}

fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac_sha256(&k, region.as_bytes());
    let k = hmac_sha256(&k, b"s3");
    hmac_sha256(&k, b"aws4_request")
}

fn chrono_like(unix: u64) -> String {
    // YYYYMMDDTHHMMSSZ — 避免再拉 chrono 进 runtime。
    let days = unix / 86400;
    let secs = unix % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        crate::store::LocalObjectStore::new("/tmp").path_for_check(key)?;
        let resp = self.signed("PUT", key, bytes).await?.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("s3 put {}: {}", key, resp.status());
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        crate::store::LocalObjectStore::new("/tmp").path_for_check(key)?;
        let resp = self.signed("GET", key, b"").await?.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("s3 get {}: {}", key, resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        crate::store::LocalObjectStore::new("/tmp").path_for_check(key)?;
        let resp = self.signed("DELETE", key, b"").await?.send().await?;
        if resp.status().as_u16() == 404 || resp.status().is_success() {
            return Ok(());
        }
        anyhow::bail!("s3 delete {}: {}", key, resp.status())
    }
}

/// 进程内共享桶：测跨「副本」读，不必起 MinIO。
#[derive(Clone, Default)]
pub struct MemoryObjectStore {
    inner: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ObjectStore for MemoryObjectStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        crate::store::LocalObjectStore::new("/tmp").path_for_check(key)?;
        self.inner
            .lock()
            .await
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        crate::store::LocalObjectStore::new("/tmp").path_for_check(key)?;
        self.inner
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("object not found"))
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.lock().await.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regional_object_key;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::put;
    use axum::{body::Bytes, Router};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn memory_store_is_shared() {
        let a = MemoryObjectStore::new();
        let b = a.clone();
        a.put("ws/t/s/h.tgz", b"blob").await.unwrap();
        assert_eq!(b.get("ws/t/s/h.tgz").await.unwrap(), b"blob");
        assert!(S3Config::from_uri("not-a-uri").is_err());
    }

    type Bucket = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    async fn spawn_s3_compat() -> (String, tokio::task::JoinHandle<()>) {
        let store: Bucket = Arc::new(Mutex::new(HashMap::new()));
        let app = Router::new()
            .route("/{bucket}", put(create_bucket))
            .route(
                "/{bucket}/{*key}",
                put(put_obj).get(get_obj).delete(del_obj),
            )
            .with_state(store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), h)
    }

    async fn create_bucket(
        State(store): State<Bucket>,
        Path(bucket): Path<String>,
        headers: HeaderMap,
    ) -> StatusCode {
        if headers.get("authorization").is_none() {
            return StatusCode::FORBIDDEN;
        }
        store.lock().await.insert(format!("{bucket}/"), Vec::new());
        StatusCode::OK
    }

    async fn put_obj(
        State(store): State<Bucket>,
        Path((bucket, key)): Path<(String, String)>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        if headers.get("authorization").is_none() {
            return StatusCode::FORBIDDEN;
        }
        store
            .lock()
            .await
            .insert(format!("{bucket}/{key}"), body.to_vec());
        StatusCode::OK
    }

    async fn get_obj(
        State(store): State<Bucket>,
        Path((bucket, key)): Path<(String, String)>,
        headers: HeaderMap,
    ) -> Result<Vec<u8>, StatusCode> {
        if headers.get("authorization").is_none() {
            return Err(StatusCode::FORBIDDEN);
        }
        store
            .lock()
            .await
            .get(&format!("{bucket}/{key}"))
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)
    }

    async fn del_obj(
        State(store): State<Bucket>,
        Path((bucket, key)): Path<(String, String)>,
    ) -> StatusCode {
        store.lock().await.remove(&format!("{bucket}/{key}"));
        StatusCode::NO_CONTENT
    }

    fn store_for(endpoint: &str, bucket: &str) -> S3ObjectStore {
        S3ObjectStore::new(S3Config {
            endpoint: endpoint.to_string(),
            bucket: bucket.into(),
            region: "us-east-1".into(),
            access_key: "rupi".into(),
            secret_key: "rupisecret1".into(),
            prefix: String::new(),
        })
    }

    /// 真 HTTP：两个 S3ObjectStore 客户端走同一 S3 兼容服务，多副本 put/get。
    #[tokio::test]
    async fn s3_compat_two_replicas_put_get() {
        let (endpoint, _h) = spawn_s3_compat().await;
        let a = store_for(&endpoint, "rupi-snap");
        let b = store_for(&endpoint, "rupi-snap");
        a.ensure_bucket().await.unwrap();
        let us = regional_object_key("us-east", "ws/t/s/h.tgz");
        let eu = regional_object_key("eu-west", "ws/t/s/h.tgz");
        a.put(&us, b"from-a").await.unwrap();
        assert_eq!(b.get(&us).await.unwrap(), b"from-a");
        b.put(&eu, b"from-b-eu").await.unwrap();
        assert_eq!(a.get(&eu).await.unwrap(), b"from-b-eu");
        assert_ne!(a.get(&us).await.unwrap(), a.get(&eu).await.unwrap());
        a.delete(&us).await.unwrap();
        assert!(b.get(&us).await.is_err());
    }

    /// MinIO（或 `RUPI_S3_ENDPOINT` 指向的兼容服务）。未配置则跳过。
    #[tokio::test]
    async fn s3_minio_two_replicas_if_configured() {
        let Ok(endpoint) = std::env::var("RUPI_S3_ENDPOINT") else {
            return;
        };
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return;
        }
        let access = std::env::var("RUPI_S3_ACCESS_KEY")
            .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
            .unwrap_or_else(|_| "rupi".into());
        let secret = std::env::var("RUPI_S3_SECRET_KEY")
            .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
            .unwrap_or_else(|_| "rupisecret1".into());
        let region = std::env::var("RUPI_S3_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .unwrap_or_else(|_| "us-east-1".into());
        let bucket = format!("rupi-it-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let cfg = S3Config {
            endpoint,
            bucket,
            region,
            access_key: access,
            secret_key: secret,
            prefix: "it".into(),
        };
        let a = S3ObjectStore::new(cfg.clone());
        let b = S3ObjectStore::new(cfg);
        a.ensure_bucket().await.expect("MinIO/S3 create bucket");
        let us = regional_object_key("us-east", "ws/t/s/h.tgz");
        let eu = regional_object_key("eu-west", "ws/t/s/h.tgz");
        a.put(&us, b"minio-a").await.expect("s3 put");
        assert_eq!(b.get(&us).await.expect("s3 get replica"), b"minio-a");
        b.put(&eu, b"minio-eu").await.unwrap();
        assert_eq!(a.get(&eu).await.unwrap(), b"minio-eu");
        assert_ne!(a.get(&us).await.unwrap(), a.get(&eu).await.unwrap());
        a.delete(&us).await.unwrap();
        a.delete(&eu).await.unwrap();
    }
}

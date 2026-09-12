//! 工作区闲置快照的对象存储。控制面只持 key；卷本体在 Executor。
//! 本机目录实现给测试与单机部署；生产可换成 S3 兼容实现，不改调用方。

use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()>;
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
}

/// 目录当对象桶。key 只允许 `[A-Za-z0-9/._-]`，禁止 `..`。
#[derive(Clone, Debug)]
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> anyhow::Result<PathBuf> {
        if key.is_empty()
            || key.starts_with('/')
            || key.contains("..")
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        {
            anyhow::bail!("invalid object key");
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let p = self.path_for(key)?;
        if let Some(dir) = p.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(p, bytes).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let p = self.path_for(key)?;
        Ok(tokio::fs::read(p).await?)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let p = self.path_for(key)?;
        match tokio::fs::remove_file(p).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 最小 base64（RFC 4648）。避免为快照再拉依赖。
pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = data.get(i + 1).copied();
        let b2 = data.get(i + 2).copied();
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                out.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
                out.push(T[(b2 & 0x3f) as usize] as char);
            }
            (Some(b1), None) => {
                out.push(T[((b1 & 0x0f) << 2) as usize] as char);
                out.push('=');
            }
            (None, _) => {
                out.push('=');
                out.push('=');
            }
        }
        i += 3;
    }
    out
}

pub fn b64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    fn val(c: u8) -> anyhow::Result<u8> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => anyhow::bail!("invalid base64"),
        }
    }
    let raw: Vec<u8> = s.bytes().filter(|b| *b != b'\n' && *b != b'\r').collect();
    if !raw.len().is_multiple_of(4) {
        anyhow::bail!("invalid base64 length");
    }
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    for chunk in raw.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let v0 = val(chunk[0])?;
        let v1 = val(chunk[1])?;
        out.push((v0 << 2) | (v1 >> 4));
        if pad < 2 {
            let v2 = val(chunk[2])?;
            out.push((v1 << 4) | (v2 >> 2));
            if pad < 1 {
                let v3 = val(chunk[3])?;
                out.push((v2 << 6) | v3);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for s in [b"" as &[u8], b"a", b"ab", b"abc", b"hello workspace"] {
            assert_eq!(b64_decode(&b64_encode(s)).unwrap(), s);
        }
    }

    #[tokio::test]
    async fn local_store_put_get_del() {
        let root = std::env::temp_dir().join(format!("rupi-obj-{}", uuid::Uuid::new_v4()));
        let store = LocalObjectStore::new(&root);
        store.put("ws/t/s/h.tgz", b"tar").await.unwrap();
        assert_eq!(store.get("ws/t/s/h.tgz").await.unwrap(), b"tar");
        store.delete("ws/t/s/h.tgz").await.unwrap();
        assert!(store.get("ws/t/s/h.tgz").await.is_err());
        assert!(store.put("../x", b"no").await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}

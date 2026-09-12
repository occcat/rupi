use sha2::{Digest, Sha256};

pub fn hash_key(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    hex::encode(h.finalize())
}

pub fn generate_key() -> String {
    format!(
        "rupi_{}",
        hex::encode(uuid::Uuid::new_v4().as_bytes()) + &hex::encode(uuid::Uuid::new_v4().as_bytes())
    )
}

pub fn generate_admin_key() -> String {
    format!(
        "rupi_admin_{}",
        hex::encode(uuid::Uuid::new_v4().as_bytes()) + &hex::encode(uuid::Uuid::new_v4().as_bytes())
    )
}

/// 比较两段口令：先哈希再恒定时间比，避免短口令提前返回。
pub fn tokens_eq(a: &str, b: &str) -> bool {
    let ha = hash_key(a);
    let hb = hash_key(b);
    let (aa, bb) = (ha.as_bytes(), hb.as_bytes());
    if aa.len() != bb.len() {
        return false;
    }
    aa.iter().zip(bb).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn key_prefix(raw: &str, n: usize) -> String {
    raw.chars().take(n).collect()
}

pub fn extract_bearer(header: Option<&str>) -> Option<&str> {
    header.and_then(|h| {
        let h = h.trim();
        h.strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
            .map(str::trim)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_key("abc"), hash_key("abc"));
        assert_ne!(hash_key("abc"), hash_key("abd"));
        assert_eq!(hash_key("abc").len(), 64);
    }

    #[test]
    fn bearer_parse() {
        assert_eq!(extract_bearer(Some("Bearer k1")), Some("k1"));
        assert_eq!(extract_bearer(Some("k1")), None);
    }

    #[test]
    fn tokens_eq_is_stable() {
        assert!(tokens_eq("secret", "secret"));
        assert!(!tokens_eq("secret", "Secret"));
        assert!(!tokens_eq("", "x"));
    }
}

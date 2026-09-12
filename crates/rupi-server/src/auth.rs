use sha2::{Digest, Sha256};

pub fn hash_key(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    hex::encode(h.finalize())
}

pub fn generate_key() -> String {
    format!("rupi_{}", hex::encode(uuid::Uuid::new_v4().as_bytes()) + &hex::encode(uuid::Uuid::new_v4().as_bytes()))
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
}

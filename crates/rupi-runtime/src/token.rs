//! 执行节点口令：恒定时间比较；空 token 只允许回环 + 显式 insecure。

/// SHA-256 后再按字节异或，避免短口令提前返回。
pub fn tokens_eq(a: &str, b: &str) -> bool {
    let ha = sha256_hex(a);
    let hb = sha256_hex(b);
    let (aa, bb) = (ha.as_bytes(), hb.as_bytes());
    if aa.len() != bb.len() {
        return false;
    }
    aa.iter().zip(bb).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// 监听地址是否只绑本机。
pub fn is_loopback_bind(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "0:0:0:0:0:0:0:1"
    )
}

/// HTTP(S) URL 的 host 是否本机。
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = if host.starts_with('[') {
        host.split(']').next().unwrap_or(host).trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "0:0:0:0:0:0:0:1"
    )
}

/// 非回环监听必须有 token；空 token 仅 `127.0.0.1`/`::1` 且 `--insecure`。
pub fn validate_listen_token(bind: &str, token: &str, insecure: bool) -> Result<(), String> {
    if !token.is_empty() {
        return Ok(());
    }
    if is_loopback_bind(bind) && insecure {
        return Ok(());
    }
    if is_loopback_bind(bind) {
        return Err(
            "empty token on loopback requires --insecure (or RUPI_EXEC_INSECURE=1)".into(),
        );
    }
    Err("non-loopback listen requires --token / RUPI_EXEC_TOKEN".into())
}

/// 控制面打执行面：明文 HTTP 仅本机。
pub fn validate_executor_url(url: &str, insecure: bool) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    if let Some((scheme, _)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("https") {
            return Ok(());
        }
        if scheme.eq_ignore_ascii_case("http") {
            if is_loopback_url(url) || insecure {
                return Ok(());
            }
            return Err(format!(
                "plaintext executor URL only allowed for loopback (got {url}); use https or --insecure"
            ));
        }
    }
    Err(format!("unsupported executor URL: {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_eq_stable() {
        assert!(tokens_eq("secret", "secret"));
        assert!(!tokens_eq("secret", "Secret"));
        assert!(!tokens_eq("", "x"));
    }

    #[test]
    fn loopback_and_listen_policy() {
        assert!(is_loopback_bind("127.0.0.1:8090"));
        assert!(is_loopback_bind("[::1]:8190"));
        assert!(!is_loopback_bind("0.0.0.0:8090"));
        assert!(!is_loopback_bind("10.0.0.2:8090"));
        assert!(validate_listen_token("127.0.0.1:0", "", true).is_ok());
        assert!(validate_listen_token("127.0.0.1:0", "", false).is_err());
        assert!(validate_listen_token("0.0.0.0:8090", "", true).is_err());
        assert!(validate_listen_token("0.0.0.0:8090", "tok", false).is_ok());
        assert!(validate_executor_url("http://127.0.0.1:8090", false).is_ok());
        assert!(validate_executor_url("http://10.1.2.3:8090", false).is_err());
        assert!(validate_executor_url("https://10.1.2.3:8090", false).is_ok());
    }
}

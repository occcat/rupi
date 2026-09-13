//! Git bootstrap：协议/host 白名单；租户 token 只进这一次 clone，不进常驻环境。

use crate::ToolText;
use std::path::Path;
use std::process::Stdio;

const DEFAULT_HOSTS: &[&str] = &[
    "github.com",
    "gitlab.com",
    "bitbucket.org",
    "git.sr.ht",
    "codeberg.org",
];

#[derive(Debug, Clone)]
pub struct GitSpec {
    pub url: String,
    pub host: String,
    pub https: bool,
}

pub fn parse_git_url(url: &str) -> Result<GitSpec, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("git url required".into());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("").split('@').next_back().unwrap_or("");
        let host = host.split(':').next().unwrap_or(host);
        if host.is_empty() {
            return Err("https git url missing host".into());
        }
        return Ok(GitSpec {
            url: format!("https://{rest}"),
            host: host.to_ascii_lowercase(),
            https: true,
        });
    }
    if let Some(rest) = url.strip_prefix("git@") {
        let host = rest.split(':').next().unwrap_or("").to_ascii_lowercase();
        if host.is_empty() || !rest.contains(':') {
            return Err("git@ url must be git@host:path".into());
        }
        return Ok(GitSpec {
            url: url.to_string(),
            host,
            https: false,
        });
    }
    Err("git url must be https:// or git@host:path".into())
}

pub fn host_allowed(host: &str, extra: &[String]) -> bool {
    let h = host.to_ascii_lowercase();
    DEFAULT_HOSTS.iter().any(|d| *d == h) || extra.iter().any(|e| e.eq_ignore_ascii_case(&h))
}

/// 只把 token 注入这一次 `git` 进程。https 走 `x-access-token`；ssh 走 `GIT_ASKPASS` 不落地。
pub async fn clone_into(
    dir: &Path,
    spec: &GitSpec,
    token: Option<&str>,
    extra_hosts: &[String],
) -> Result<ToolText, String> {
    if !host_allowed(&spec.host, extra_hosts) {
        return Err(format!("git host not allowed: {}", spec.host));
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    cmd.env_remove("GIT_ASKPASS");
    cmd.env_remove("GIT_CONFIG_COUNT");
    cmd.env_remove("GIT_CONFIG_KEY_0");
    cmd.env_remove("GIT_CONFIG_VALUE_0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        if spec.https {
            // 一次性 header，不写 ~/.git-credentials。
            cmd.env("GIT_CONFIG_COUNT", "1");
            cmd.env("GIT_CONFIG_KEY_0", "http.extraHeader");
            cmd.env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Bearer {tok}"),
            );
            cmd.arg(&spec.url);
        } else {
            return Err("git@ clone with token is not supported; use https:// + tenant token".into());
        }
    } else {
        cmd.arg(&spec.url);
    }
    cmd.arg(".").current_dir(dir).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(ToolText::ok(format!("cloned {}", spec.url)))
    } else {
        Ok(ToolText::err(String::from_utf8_lossy(&out.stderr)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_whitelist() {
        let s = parse_git_url("https://github.com/occcat/rupi.git").unwrap();
        assert_eq!(s.host, "github.com");
        assert!(host_allowed(&s.host, &[]));
        assert!(parse_git_url("http://github.com/x").is_err());
        assert!(parse_git_url("file:///tmp/repo").is_err());
        let ssh = parse_git_url("git@gitlab.com:group/repo.git").unwrap();
        assert_eq!(ssh.host, "gitlab.com");
        assert!(!host_allowed("evil.example", &[]));
        assert!(host_allowed("git.example.com", &["git.example.com".into()]));
    }
}

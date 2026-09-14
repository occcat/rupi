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

impl GitSpec {
    /// 测试/本机绝对路径仓库。`parse_git_url` 不接受 `file://` 或裸路径。
    pub fn local_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let p = path.as_ref();
        if !p.is_absolute() {
            return Err("local git path must be absolute".into());
        }
        Ok(Self {
            url: p.display().to_string(),
            host: "local".into(),
            https: false,
        })
    }
}

pub fn parse_git_url(url: &str) -> Result<GitSpec, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("git url required".into());
    }
    if url.starts_with("file:") {
        return Err("file:// git urls are not allowed".into());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest
            .split('/')
            .next()
            .unwrap_or("")
            .split('@')
            .next_back()
            .unwrap_or("");
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

pub fn allow_anonymous_git() -> bool {
    std::env::var("RUPI_GIT_ALLOW_ANON").ok().as_deref() == Some("1")
}

/// 只把 token 注入这一次 `git` 进程。https 走 Bearer header；ssh / 匿名默认拒。
pub async fn clone_into(
    dir: &Path,
    spec: &GitSpec,
    token: Option<&str>,
    extra_hosts: &[String],
) -> Result<ToolText, String> {
    clone_into_with(dir, spec, token, extra_hosts, allow_anonymous_git()).await
}

pub async fn clone_into_with(
    dir: &Path,
    spec: &GitSpec,
    token: Option<&str>,
    extra_hosts: &[String],
    allow_anon: bool,
) -> Result<ToolText, String> {
    if !host_allowed(&spec.host, extra_hosts) {
        return Err(format!("git host not allowed: {}", spec.host));
    }
    if spec.host == "local" {
        return run_clone(dir, &spec.url, None).await;
    }
    if !spec.https {
        return Err("anonymous git@ clone is not allowed; use https:// + tenant token".into());
    }
    let tok = token.map(str::trim).filter(|s| !s.is_empty());
    if tok.is_none() && !allow_anon {
        return Err(
            "https git clone requires a tenant token (set RUPI_GIT_ALLOW_ANON=1 for public read-only)"
                .into(),
        );
    }
    run_clone(dir, &spec.url, tok).await
}

async fn run_clone(dir: &Path, url: &str, token: Option<&str>) -> Result<ToolText, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    cmd.env_remove("GIT_ASKPASS");
    cmd.env_remove("GIT_CONFIG_COUNT");
    cmd.env_remove("GIT_CONFIG_KEY_0");
    cmd.env_remove("GIT_CONFIG_VALUE_0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(tok) = token {
        cmd.env("GIT_CONFIG_COUNT", "1");
        cmd.env("GIT_CONFIG_KEY_0", "http.extraHeader");
        cmd.env("GIT_CONFIG_VALUE_0", format!("Authorization: Bearer {tok}"));
    }
    cmd.arg(url);
    cmd.arg(".")
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(ToolText::ok(format!("cloned {url}")))
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
        let file_err = parse_git_url("file:///tmp/repo").unwrap_err();
        assert!(file_err.contains("file://"), "{file_err}");
        let ssh = parse_git_url("git@gitlab.com:group/repo.git").unwrap();
        assert_eq!(ssh.host, "gitlab.com");
        assert!(!host_allowed("evil.example", &[]));
        assert!(host_allowed("git.example.com", &["git.example.com".into()]));
    }

    #[tokio::test]
    async fn clone_rejects_anonymous_and_file() {
        let dir = std::env::temp_dir().join(format!("rupi-git-deny-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(parse_git_url("file:///tmp/repo").is_err());
        let ssh = parse_git_url("git@github.com:occcat/rupi.git").unwrap();
        let err = clone_into_with(&dir, &ssh, None, &[], false)
            .await
            .unwrap_err();
        assert!(err.contains("git@") || err.contains("not allowed"), "{err}");
        let https = parse_git_url("https://github.com/occcat/rupi.git").unwrap();
        let err = clone_into_with(&dir, &https, None, &[], false)
            .await
            .unwrap_err();
        assert!(err.contains("token"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn clone_local_repo() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }
        let root = std::env::temp_dir().join(format!("rupi-git-local-{}", uuid::Uuid::new_v4()));
        let src = root.join("src");
        let dest = root.join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("hello.txt"), "from-local-repo").unwrap();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&src)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["add", "hello.txt"]);
        run(&["commit", "-m", "init"]);
        let spec = GitSpec::local_path(&src).unwrap();
        let denied = clone_into_with(&dest, &spec, None, &[], false)
            .await
            .unwrap_err();
        assert!(denied.contains("not allowed"), "{denied}");
        let out = clone_into_with(&dest, &spec, None, &["local".into()], false)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let copied = std::fs::read_to_string(dest.join("hello.txt")).unwrap();
        assert_eq!(copied, "from-local-repo");
        let _ = std::fs::remove_dir_all(root);
    }
}

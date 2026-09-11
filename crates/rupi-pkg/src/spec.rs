//! `git:` / `npm:` / 本地路径规格（对标 Pi `pi install`）。

use std::path::{Path, PathBuf};

/// 安装源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Npm {
        name: String,
        version: Option<String>,
    },
    Git {
        url: String,
        git_ref: Option<String>,
    },
    Local {
        path: PathBuf,
    },
}

impl PackageSource {
    /// 锁文件 / 卸载用的稳定 id（不含版本与 ref）。
    pub fn id(&self) -> String {
        match self {
            Self::Npm { name, .. } => format!("npm:{name}"),
            Self::Git { url, .. } => format!("git:{url}"),
            Self::Local { path } => format!("local:{}", path.display()),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Npm { .. } => "npm",
            Self::Git { .. } => "git",
            Self::Local { .. } => "local",
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Npm { name, .. } => name.clone(),
            Self::Git { url, .. } => git_slug(url),
            Self::Local { path } => path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("package")
                .to_string(),
        }
    }
}

/// 解析 `npm:@scope/pkg@1.2.3`、`git:github.com/user/repo@v1`、https/ssh URL、本地路径。
pub fn parse_spec(spec: &str) -> anyhow::Result<PackageSource> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("empty package spec");
    }
    if let Some(rest) = spec.strip_prefix("npm:") {
        let (name, version) = split_npm(rest)?;
        return Ok(PackageSource::Npm { name, version });
    }
    if let Some(rest) = spec.strip_prefix("git:") {
        let (url, git_ref) = split_git_ref(rest);
        return Ok(PackageSource::Git {
            url: normalize_git_url(&url),
            git_ref,
        });
    }
    if let Some(rest) = spec.strip_prefix("file:") {
        return Ok(PackageSource::Local {
            path: PathBuf::from(rest),
        });
    }
    if has_url_scheme(spec) {
        let (url, git_ref) = split_git_ref(spec);
        return Ok(PackageSource::Git {
            url: normalize_git_url(&url),
            git_ref,
        });
    }
    let p = Path::new(spec);
    if spec.starts_with('.') || spec.starts_with('/') || p.exists() {
        return Ok(PackageSource::Local {
            path: PathBuf::from(spec),
        });
    }
    anyhow::bail!(
        "unrecognized spec `{spec}` (want npm:<pkg>, git:<url>, https://…, or a local path)"
    )
}

fn has_url_scheme(s: &str) -> bool {
    s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("ssh://")
        || s.starts_with("git://")
}

fn split_npm(rest: &str) -> anyhow::Result<(String, Option<String>)> {
    let rest = rest.trim();
    if rest.is_empty() {
        anyhow::bail!("npm: spec missing package name");
    }
    if rest.starts_with('@') {
        let Some(slash) = rest.find('/') else {
            anyhow::bail!("scoped npm package must be @scope/name");
        };
        let after = &rest[slash + 1..];
        if let Some(at) = after.rfind('@') {
            let name = format!("{}{}", &rest[..=slash], &after[..at]);
            let ver = &after[at + 1..];
            if ver.is_empty() {
                anyhow::bail!("empty npm version");
            }
            return Ok((name, Some(ver.to_string())));
        }
        return Ok((rest.to_string(), None));
    }
    if let Some(at) = rest.rfind('@') {
        let name = &rest[..at];
        let ver = &rest[at + 1..];
        if name.is_empty() || ver.is_empty() {
            anyhow::bail!("invalid npm spec `{rest}`");
        }
        return Ok((name.to_string(), Some(ver.to_string())));
    }
    Ok((rest.to_string(), None))
}

/// 拆 git URL 与可选 `@ref`。`git@host:path` 的首个 `@` 不是 ref。
fn split_git_ref(rest: &str) -> (String, Option<String>) {
    if rest.starts_with("git@") {
        if let Some(i) = rest[1..].rfind('@') {
            let i = i + 1;
            let candidate = &rest[i + 1..];
            if looks_like_ref(candidate) {
                return (rest[..i].to_string(), Some(candidate.to_string()));
            }
        }
        return (rest.to_string(), None);
    }
    if let Some(scheme) = rest.find("://") {
        let after = &rest[scheme + 3..];
        // https://user:pass@host/path@ref — ref 是最后一段且不含 /
        if let Some(i) = after.rfind('@') {
            let abs = scheme + 3 + i;
            let candidate = &rest[abs + 1..];
            if looks_like_ref(candidate) {
                return (rest[..abs].to_string(), Some(candidate.to_string()));
            }
        }
        return (rest.to_string(), None);
    }
    // github.com/user/repo@v1
    if let Some(i) = rest.rfind('@') {
        let candidate = &rest[i + 1..];
        if looks_like_ref(candidate) {
            return (rest[..i].to_string(), Some(candidate.to_string()));
        }
    }
    (rest.to_string(), None)
}

fn looks_like_ref(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains(':') && !s.contains('@')
}

fn normalize_git_url(s: &str) -> String {
    if has_url_scheme(s) || s.starts_with("git@") || s.starts_with("file:") {
        return s.to_string();
    }
    format!("https://{s}")
}

/// `https://github.com/user/repo.git` → `github.com/user/repo`
pub fn git_slug(url: &str) -> String {
    let s = url.trim_end_matches('/').trim_end_matches(".git");
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s
        .strip_prefix("ssh://git@")
        .or_else(|| s.strip_prefix("ssh://"))
        .unwrap_or(s);
    let s = s.strip_prefix("git://").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("git@") {
        return sanitize_slug(&rest.replace(':', "/"));
    }
    if let Some(rest) = s.strip_prefix("file://") {
        return sanitize_slug(rest.trim_start_matches('/'));
    }
    sanitize_slug(s)
}

pub fn npm_slug(name: &str) -> String {
    sanitize_slug(&name.replace('/', "__"))
}

pub fn sanitize_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' || c == '/' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('.').trim_matches('/').to_string();
    if out.is_empty() || out.contains("..") {
        "package".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_npm_specs() {
        assert_eq!(
            parse_spec("npm:left-pad").unwrap(),
            PackageSource::Npm {
                name: "left-pad".into(),
                version: None
            }
        );
        assert_eq!(
            parse_spec("npm:@foo/bar@1.2.3").unwrap(),
            PackageSource::Npm {
                name: "@foo/bar".into(),
                version: Some("1.2.3".into())
            }
        );
        assert_eq!(
            parse_spec("npm:@foo/bar").unwrap(),
            PackageSource::Npm {
                name: "@foo/bar".into(),
                version: None
            }
        );
    }

    #[test]
    fn parse_git_specs() {
        match parse_spec("git:github.com/user/repo@v1").unwrap() {
            PackageSource::Git { url, git_ref } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("v1"));
            }
            other => panic!("{other:?}"),
        }
        match parse_spec("git:git@github.com:user/repo@v1").unwrap() {
            PackageSource::Git { url, git_ref } => {
                assert_eq!(url, "git@github.com:user/repo");
                assert_eq!(git_ref.as_deref(), Some("v1"));
            }
            other => panic!("{other:?}"),
        }
        match parse_spec("https://github.com/user/repo@abc").unwrap() {
            PackageSource::Git { url, git_ref } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("abc"));
            }
            other => panic!("{other:?}"),
        }
        match parse_spec("git:git@github.com:user/repo").unwrap() {
            PackageSource::Git { url, git_ref } => {
                assert_eq!(url, "git@github.com:user/repo");
                assert!(git_ref.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_local_path() {
        match parse_spec("./rel/pkg").unwrap() {
            PackageSource::Local { path } => assert_eq!(path, PathBuf::from("./rel/pkg")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn git_slug_strips_scheme() {
        assert_eq!(
            git_slug("https://github.com/user/repo.git"),
            "github.com/user/repo"
        );
        assert_eq!(git_slug("git@github.com:user/repo"), "github.com/user/repo");
    }
}

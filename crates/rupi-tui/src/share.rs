//! `/share`：秘密 gist + 查看器 URL；无凭证则只写本地文件。
//! 对标 Pi `session-share`（Radius/OAuth 不移植）。

use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_VIEWER: &str = "https://pi.dev/session/";

/// `RUPI_SHARE_VIEWER_URL` > `PI_SHARE_VIEWER_URL` > `https://pi.dev/session/#<id>`。
pub fn viewer_url(gist_id: &str) -> String {
    let base = std::env::var("RUPI_SHARE_VIEWER_URL")
        .or_else(|_| std::env::var("PI_SHARE_VIEWER_URL"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_VIEWER.to_string());
    if base.contains('#') {
        format!("{base}{gist_id}")
    } else if base.ends_with('/') {
        format!("{base}#{gist_id}")
    } else {
        format!("{base}/#{gist_id}")
    }
}

pub fn default_share_path(sid: &str, ext: &str) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(format!("rupi-session-{}.{}", &sid[..8.min(sid.len())], ext))
}

/// 测试或离线：`allow_upload=false` 只写本地，不调 `gh`。
pub fn share_jsonl(jsonl: &str, sid: &str, allow_upload: bool) -> String {
    if allow_upload && !share_offline() {
        if let Some(msg) = try_gh_gist(jsonl, sid) {
            return msg;
        }
    }
    write_local(jsonl, sid)
}

fn share_offline() -> bool {
    matches!(
        std::env::var("RUPI_SHARE_OFFLINE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn write_local(jsonl: &str, sid: &str) -> String {
    let path = default_share_path(sid, "jsonl");
    match write_file(&path, jsonl) {
        Ok(()) => format!(
            "[share] no gh/GITHUB_TOKEN; wrote {}\n  set GH_TOKEN or install gh to upload a secret gist",
            path.display()
        ),
        Err(e) => format!("[share] write failed: {e}"),
    }
}

fn write_file(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, body).map_err(|e| e.to_string())
}

fn try_gh_gist(jsonl: &str, sid: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!("rupi-share-{}", &sid[..8.min(sid.len())]));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("rupi-session-{}.jsonl", &sid[..8.min(sid.len())]));
    write_file(&file, jsonl).ok()?;
    let out = Command::new("gh")
        .args([
            "gist",
            "create",
            "--desc",
            &format!("rupi session {sid}"),
            file.to_str()?,
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains("gist.github.com"))?;
    let id = url.rsplit('/').next().filter(|s| !s.is_empty())?;
    Some(format!("[share] {}\n  gist {url}", viewer_url(id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_url_uses_env_then_default() {
        let saved_r = std::env::var("RUPI_SHARE_VIEWER_URL").ok();
        let saved_p = std::env::var("PI_SHARE_VIEWER_URL").ok();
        unsafe {
            std::env::remove_var("RUPI_SHARE_VIEWER_URL");
            std::env::remove_var("PI_SHARE_VIEWER_URL");
        }
        assert_eq!(viewer_url("abc123"), "https://pi.dev/session/#abc123");
        unsafe { std::env::set_var("PI_SHARE_VIEWER_URL", "https://example.com/s") };
        assert_eq!(viewer_url("abc123"), "https://example.com/s/#abc123");
        unsafe { std::env::set_var("RUPI_SHARE_VIEWER_URL", "https://rupi.example/view/") };
        assert_eq!(viewer_url("abc123"), "https://rupi.example/view/#abc123");
        match saved_r {
            Some(v) => unsafe { std::env::set_var("RUPI_SHARE_VIEWER_URL", v) },
            None => unsafe { std::env::remove_var("RUPI_SHARE_VIEWER_URL") },
        }
        match saved_p {
            Some(v) => unsafe { std::env::set_var("PI_SHARE_VIEWER_URL", v) },
            None => unsafe { std::env::remove_var("PI_SHARE_VIEWER_URL") },
        }
    }

    #[test]
    fn share_without_upload_writes_local_jsonl() {
        let cwd =
            std::env::temp_dir().join(format!("rupi-share-cwd-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let prev = std::env::current_dir().ok();
        std::env::set_current_dir(&cwd).unwrap();
        let sid = "abcdef12-0000-0000-0000-000000000001";
        let msg = share_jsonl("{\"type\":\"session\"}\n", sid, false);
        assert!(msg.contains("[share]"), "{msg}");
        assert!(msg.contains("wrote"), "{msg}");
        let path = cwd.join("rupi-session-abcdef12.jsonl");
        assert!(path.is_file(), "{path:?} missing; {msg}");
        if let Some(p) = prev {
            let _ = std::env::set_current_dir(p);
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

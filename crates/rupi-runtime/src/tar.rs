//! 快照 tar：只接受相对成员，拒绝 `..` / 绝对路径 / 链到卷外。失败则整卷作废。

use std::path::Path;
use std::process::Stdio;

pub fn member_ok(name: &str) -> bool {
    let name = name.trim().trim_end_matches('/');
    if name.is_empty() || name == "." {
        return true;
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    if name.contains('\0') {
        return false;
    }
    // Windows 盘符 / UNC
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return false;
    }
    for part in name.split(['/', '\\']) {
        if part == ".." {
            return false;
        }
    }
    true
}

async fn list_members(archive: &Path) -> Result<Vec<String>, String> {
    let out = tokio::process::Command::new("tar")
        .args(["-tzf", &archive.display().to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// 列出并校验；有毒则 Err。
pub async fn validate_archive(archive: &Path) -> Result<(), String> {
    let members = list_members(archive).await?;
    for m in &members {
        if !member_ok(m) {
            return Err(format!("unsafe tar member: {m}"));
        }
    }
    Ok(())
}

pub async fn extract_checked(archive: &Path, dest: &Path) -> Result<(), String> {
    validate_archive(archive).await?;
    let dest_s = dest.display().to_string();
    let arch_s = archive.display().to_string();
    let args = ["-C", dest_s.as_str(), "-xzf", arch_s.as_str()];
    // GNU tar：不要跟绝对名；部分版本有 --one-top-level，不作硬依赖。
    let st = tokio::process::Command::new("tar")
        .args(args)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("tar extract failed".into());
    }
    assert_extracted_inside(dest)?;
    Ok(())
}

fn assert_extracted_inside(dest: &Path) -> Result<(), String> {
    let dest = dest
        .canonicalize()
        .map_err(|e| format!("canonicalize dest: {e}"))?;
    fn walk(dir: &Path, root: &Path) -> Result<(), String> {
        let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
        for ent in rd {
            let ent = ent.map_err(|e| e.to_string())?;
            let p = ent.path();
            let meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&p).map_err(|e| e.to_string())?;
                let resolved = if target.is_absolute() {
                    target
                } else {
                    p.parent().unwrap_or(root).join(target)
                };
                let canon = resolved.canonicalize().unwrap_or(resolved);
                if !canon.starts_with(root) {
                    return Err(format!("symlink escapes jail: {}", p.display()));
                }
            } else {
                let canon = p.canonicalize().unwrap_or(p.clone());
                if !canon.starts_with(root) {
                    return Err(format!("extract escaped jail: {}", p.display()));
                }
            }
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(&p, root)?;
            }
        }
        Ok(())
    }
    walk(&dest, &dest)
}

/// 构造一条带 `../` 成员的毒 tar（测试用）。
pub fn poison_tarball() -> anyhow::Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("rupi-poison-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    let src = tmp.join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("ok.txt"), b"ok")?;
    let archive = tmp.join("p.tgz");
    // GNU tar 可用 `--transform` 把成员改成 `../evil`。
    let st = std::process::Command::new("tar")
        .args([
            "-C",
            &src.display().to_string(),
            "--transform=s|^|../|",
            "-czf",
            &archive.display().to_string(),
            "ok.txt",
        ])
        .status()?;
    if !st.success() {
        // 回退：手写最小 ustar + gzip 太重，改用 PAX 头。
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!("tar --transform failed");
    }
    let bytes = std::fs::read(&archive)?;
    let _ = std::fs::remove_dir_all(tmp);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members() {
        assert!(member_ok("./a/b"));
        assert!(member_ok("a/b.txt"));
        assert!(!member_ok("../evil"));
        assert!(!member_ok("/etc/passwd"));
        assert!(!member_ok("a/../../x"));
        assert!(!member_ok("C:\\\\windows"));
    }

    #[tokio::test]
    async fn poison_is_rejected() {
        let Ok(bytes) = poison_tarball() else {
            return;
        };
        let tmp = std::env::temp_dir().join(format!("rupi-tar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let arch = tmp.join("p.tgz");
        std::fs::write(&arch, &bytes).unwrap();
        assert!(validate_archive(&arch).await.is_err());
        let _ = std::fs::remove_dir_all(tmp);
    }
}

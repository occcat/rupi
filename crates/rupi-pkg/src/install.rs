//! 拉取 git/npm/本地包并物化到 skills / commands / extensions。

use crate::layout::{
    command_install_name, copy_tree, discover, extension_install_name, skill_install_name,
    PackageResources, ResourceKind,
};
use crate::spec::{git_slug, npm_slug, parse_spec, PackageSource};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 安装结果。
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub record: PackageRecord,
    pub skipped_ts_extensions: usize,
}

/// `packages.json` 一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRecord {
    pub id: String,
    pub spec: String,
    pub source: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub checkout: String,
    pub skills: Vec<String>,
    pub commands: Vec<String>,
    pub extensions: Vec<String>,
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PackageLock {
    #[serde(default)]
    packages: Vec<PackageRecord>,
}

/// 安装选项。
pub struct InstallOpts {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub local: bool,
    pub kind: ResourceKind,
}

impl InstallOpts {
    fn root(&self) -> PathBuf {
        if self.local {
            self.cwd.join(".rupi")
        } else {
            self.home.clone()
        }
    }
}

/// 从规格安装。不跑 `npm install` / 包脚本。
pub async fn install(spec: &str, opts: &InstallOpts) -> anyhow::Result<InstallReport> {
    let source = parse_spec(spec)?;
    let source = resolve_local(source, &opts.cwd)?;
    let root = opts.root();
    std::fs::create_dir_all(&root)?;

    let (checkout, version) = fetch_into(&source, &root).await?;
    let resources = opts.kind.filter(discover(&checkout)?);
    if resources.is_empty() {
        if resources.skipped_ts_extensions > 0 {
            anyhow::bail!(
                "{} has only TypeScript/JS extensions; rupi skips TS — use a JSON-RPC manifest (*.json)",
                checkout.display()
            );
        }
        anyhow::bail!(
            "no skills/, prompts|commands/*.md, or extensions/*.json in {}",
            checkout.display()
        );
    }

    let mut lock = load_lock(&root);
    if let Some(prev) = lock.packages.iter().find(|p| p.id == source.id()).cloned() {
        remove_materialized(&root, &prev);
    }

    let materialized = materialize(&root, &resources)?;
    let record = PackageRecord {
        id: source.id(),
        spec: spec.to_string(),
        source: source.kind_name().to_string(),
        name: source.display_name(),
        version,
        checkout: rel_to(&root, &checkout),
        skills: materialized.skills,
        commands: materialized.commands,
        extensions: materialized.extensions,
        local: opts.local,
    };
    lock.packages.retain(|p| p.id != record.id);
    lock.packages.push(record.clone());
    save_lock(&root, &lock)?;
    Ok(InstallReport {
        skipped_ts_extensions: resources.skipped_ts_extensions,
        record,
    })
}

/// 按 id / name / 原 spec 卸载。
pub fn uninstall(spec: &str, opts: &InstallOpts) -> anyhow::Result<PackageRecord> {
    let root = opts.root();
    let mut lock = load_lock(&root);
    let idx = lock
        .packages
        .iter()
        .position(|p| match_record(p, spec))
        .ok_or_else(|| anyhow::anyhow!("package not found: {spec}"))?;
    let rec = lock.packages.remove(idx);
    remove_materialized(&root, &rec);
    if rec.source != "local" {
        let ck = root.join(&rec.checkout);
        if ck.exists() {
            let _ = std::fs::remove_dir_all(&ck);
        }
    }
    save_lock(&root, &lock)?;
    Ok(rec)
}

/// 已安装列表。
pub fn list_installed(opts: &InstallOpts) -> Vec<PackageRecord> {
    load_lock(&opts.root()).packages
}

fn match_record(p: &PackageRecord, spec: &str) -> bool {
    let s = spec.trim();
    p.id == s
        || p.spec == s
        || p.name == s
        || parse_spec(s)
            .ok()
            .is_some_and(|src| p.id == src.id() || p.name == src.display_name())
}

fn resolve_local(source: PackageSource, cwd: &Path) -> anyhow::Result<PackageSource> {
    match source {
        PackageSource::Local { path } => {
            let abs = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            let abs = abs
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("local package not found: {}: {e}", abs.display()))?;
            Ok(PackageSource::Local { path: abs })
        }
        other => Ok(other),
    }
}

async fn fetch_into(
    source: &PackageSource,
    root: &Path,
) -> anyhow::Result<(PathBuf, Option<String>)> {
    match source {
        PackageSource::Local { path } => Ok((path.clone(), None)),
        PackageSource::Git { url, git_ref } => {
            let dest = root.join("packages/git").join(git_slug(url));
            fetch_git(url, git_ref.as_deref(), &dest)?;
            Ok((dest, git_ref.clone()))
        }
        PackageSource::Npm { name, version } => {
            let dest = root.join("packages/npm").join(npm_slug(name));
            let ver = fetch_npm(name, version.as_deref(), &dest).await?;
            Ok((dest, Some(ver)))
        }
    }
}

fn fetch_git(url: &str, git_ref: Option<&str>, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_s = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 checkout path"))?;
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .args(["clone", "--depth", "1"]);
    if let Some(r) = git_ref {
        if !is_sha(r) {
            cmd.args(["--branch", r]);
        }
    }
    cmd.args([url, dest_s]);
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("git clone: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if let Some(r) = git_ref {
        if is_sha(r) {
            let st = Command::new("git")
                .current_dir(dest)
                .args(["fetch", "--depth", "1", "origin", r])
                .status()?;
            if st.success() {
                let _ = Command::new("git")
                    .current_dir(dest)
                    .args(["checkout", r])
                    .status()?;
            }
        }
    }
    Ok(())
}

fn is_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

async fn fetch_npm(name: &str, version: Option<&str>, dest: &Path) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("rupi-pkg")
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let encoded = name.replace('/', "%2f");
    let url = format!("https://registry.npmjs.org/{encoded}");
    let meta: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ver = match version {
        Some(v) => v.to_string(),
        None => meta
            .pointer("/dist-tags/latest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("npm {name}: no dist-tags.latest"))?
            .to_string(),
    };
    let tarball = meta
        .pointer(&format!("/versions/{ver}/dist/tarball"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("npm {name}@{ver}: no tarball URL"))?
        .to_string();
    let bytes = client
        .get(&tarball)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;
    let tmp = dest
        .parent()
        .unwrap_or(dest)
        .join(format!(".{}.tgz", npm_slug(name)));
    std::fs::write(&tmp, &bytes)?;
    let tmp_s = tmp
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 tarball path"))?;
    let dest_s = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 dest path"))?;
    let out = Command::new("tar")
        .args(["-xzf", tmp_s, "-C", dest_s, "--strip-components=1"])
        .output()
        .map_err(|e| anyhow::anyhow!("tar: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() {
        anyhow::bail!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(ver)
}

struct Materialized {
    skills: Vec<String>,
    commands: Vec<String>,
    extensions: Vec<String>,
}

fn materialize(root: &Path, res: &PackageResources) -> anyhow::Result<Materialized> {
    let mut skills = Vec::new();
    let mut commands = Vec::new();
    let mut extensions = Vec::new();
    for dir in &res.skills {
        let name = skill_install_name(dir);
        let dest = root.join("skills").join(&name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        copy_tree(dir, &dest)?;
        skills.push(name);
    }
    for path in &res.commands {
        let Some(name) = command_install_name(path) else {
            continue;
        };
        let dest_dir = root.join("commands");
        std::fs::create_dir_all(&dest_dir)?;
        std::fs::copy(path, dest_dir.join(format!("{name}.md")))?;
        commands.push(name);
    }
    for path in &res.extensions {
        let name = extension_install_name(path);
        let dest_dir = root.join("extensions");
        std::fs::create_dir_all(&dest_dir)?;
        std::fs::copy(path, dest_dir.join(format!("{name}.json")))?;
        extensions.push(name);
    }
    skills.sort();
    commands.sort();
    extensions.sort();
    Ok(Materialized {
        skills,
        commands,
        extensions,
    })
}

fn remove_materialized(root: &Path, rec: &PackageRecord) {
    for s in &rec.skills {
        let p = root.join("skills").join(s);
        let _ = std::fs::remove_dir_all(p);
    }
    for c in &rec.commands {
        let p = root.join("commands").join(format!("{c}.md"));
        let _ = std::fs::remove_file(p);
    }
    for e in &rec.extensions {
        let p = root.join("extensions").join(format!("{e}.json"));
        let _ = std::fs::remove_file(p);
    }
}

fn lock_path(root: &Path) -> PathBuf {
    root.join("packages.json")
}

fn load_lock(root: &Path) -> PackageLock {
    let p = lock_path(root);
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_lock(root: &Path, lock: &PackageLock) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    let json = serde_json::to_string_pretty(lock)?;
    std::fs::write(lock_path(root), json)?;
    Ok(())
}

fn rel_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// 把安装报告打成人类可读的几行。
pub fn format_report(r: &InstallReport) -> String {
    let rec = &r.record;
    let mut lines = vec![format!("installed {} ({})", rec.name, rec.id)];
    if let Some(v) = &rec.version {
        lines.push(format!("  version {v}"));
    }
    if !rec.skills.is_empty() {
        lines.push(format!("  skills: {}", rec.skills.join(", ")));
    }
    if !rec.commands.is_empty() {
        lines.push(format!("  commands: {}", rec.commands.join(", ")));
    }
    if !rec.extensions.is_empty() {
        lines.push(format!("  extensions: {}", rec.extensions.join(", ")));
    }
    if r.skipped_ts_extensions > 0 {
        lines.push(format!(
            "  skipped {} TypeScript/JS extensions (use a JSON-RPC *.json manifest)",
            r.skipped_ts_extensions
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn opts(home: &Path) -> InstallOpts {
        InstallOpts {
            home: home.to_path_buf(),
            cwd: home.to_path_buf(),
            local: false,
            kind: ResourceKind::Auto,
        }
    }

    #[tokio::test]
    async fn install_local_package_roundtrip() {
        let pid = std::process::id();
        let home = std::env::temp_dir().join(format!("rupi-pkg-home-{pid}"));
        let pkg = std::env::temp_dir().join(format!("rupi-pkg-src-{pid}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&pkg);
        write(
            &pkg,
            "skills/demo-skill/SKILL.md",
            "---\nname: demo-skill\ndescription: demo skill for install tests\n---\nDo the thing.\n",
        );
        write(
            &pkg,
            "prompts/greet.md",
            "---\ndescription: greet\n---\nHello {{who:-world}}\n",
        );
        write(
            &pkg,
            "extensions/demo-echo.json",
            r#"{"name":"demo-echo","description":"echo","input_schema":{"type":"object"},"command":"true"}"#,
        );
        write(
            &pkg,
            "package.json",
            r#"{"name":"demo","keywords":["pi-package"]}"#,
        );

        let o = opts(&home);
        let report = install(pkg.to_str().unwrap(), &o).await.unwrap();
        assert_eq!(report.record.skills, vec!["demo-skill".to_string()]);
        assert_eq!(report.record.commands, vec!["greet".to_string()]);
        assert_eq!(report.record.extensions, vec!["demo-echo".to_string()]);
        assert!(home.join("skills/demo-skill/SKILL.md").is_file());
        assert!(home.join("commands/greet.md").is_file());
        assert!(home.join("extensions/demo-echo.json").is_file());

        let listed = list_installed(&o);
        assert_eq!(listed.len(), 1);
        let rec = uninstall("demo-skill", &o)
            .unwrap_or_else(|_| uninstall(pkg.to_str().unwrap(), &o).unwrap());
        assert_eq!(rec.skills, vec!["demo-skill".to_string()]);
        assert!(!home.join("skills/demo-skill/SKILL.md").exists());
        assert!(list_installed(&o).is_empty());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[tokio::test]
    async fn install_from_local_git_clone() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let pid = std::process::id();
        let home = std::env::temp_dir().join(format!("rupi-pkg-githome-{pid}"));
        let repo = std::env::temp_dir().join(format!("rupi-pkg-gitrepo-{pid}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&repo);
        write(
            &repo,
            "skills/git-skill/SKILL.md",
            "---\nname: git-skill\ndescription: from a local git repo\n---\n",
        );
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "."])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(&repo)
            .status();
        let url = format!("file://{}", repo.display());
        let spec = format!("git:{url}");
        let o = opts(&home);
        let report = install(&spec, &o).await.unwrap();
        assert_eq!(report.record.skills, vec!["git-skill".to_string()]);
        assert!(home.join("skills/git-skill/SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&repo);
    }
}

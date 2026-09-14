//! `rupi config`：列出并启停 packages / extensions / skills / prompts / themes。
//! 对标 `pi config` 的 `!`/`+` 模式，写入 settings.json；不做 OAuth。

use rupi_config::{config_write_target, persist_toggle, ConfigKind, ResourceFilter, Settings};
use rupi_pkg::{list_installed, InstallOpts, PackageRecord};
use rupi_skills::SkillRegistry;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Action {
    List { kind: Option<ConfigKind> },
    Enable { kind: ConfigKind, name: String },
    Disable { kind: ConfigKind, name: String },
}

pub fn run(home: &Path, cwd: &Path, local: bool, action: Action) -> anyhow::Result<()> {
    match action {
        Action::List { kind } => {
            print!("{}", format_list(home, cwd, kind.as_ref())?);
            Ok(())
        }
        Action::Enable { kind, name } => toggle(home, cwd, local, kind, &name, true),
        Action::Disable { kind, name } => toggle(home, cwd, local, kind, &name, false),
    }
}

fn toggle(
    home: &Path,
    cwd: &Path,
    local: bool,
    kind: ConfigKind,
    name: &str,
    enable: bool,
) -> anyhow::Result<()> {
    let path = config_write_target(home, cwd, local);
    if kind == ConfigKind::Packages {
        toggle_package(home, cwd, &path, name, enable)?;
    } else {
        persist_toggle(&path, kind, name, enable)?;
    }
    let verb = if enable { "enabled" } else { "disabled" };
    println!("{verb} {} `{name}` → {}", kind.as_str(), path.display());
    Ok(())
}

fn toggle_package(
    home: &Path,
    cwd: &Path,
    path: &Path,
    name: &str,
    enable: bool,
) -> anyhow::Result<()> {
    persist_toggle(path, ConfigKind::Packages, name, enable)?;
    if let Some(rec) = find_package(home, cwd, name) {
        for s in &rec.skills {
            persist_toggle(path, ConfigKind::Skills, s, enable)?;
        }
        for c in &rec.commands {
            persist_toggle(path, ConfigKind::Prompts, c, enable)?;
        }
        for e in &rec.extensions {
            persist_toggle(path, ConfigKind::Extensions, e, enable)?;
        }
        for t in &rec.themes {
            persist_toggle(path, ConfigKind::Themes, t, enable)?;
        }
    }
    Ok(())
}

fn find_package(home: &Path, cwd: &Path, name: &str) -> Option<PackageRecord> {
    for local in [false, true] {
        let opts = InstallOpts {
            home: home.to_path_buf(),
            cwd: cwd.to_path_buf(),
            local,
            kind: rupi_pkg::ResourceKind::Auto,
        };
        if let Some(rec) = list_installed(&opts).into_iter().find(|p| {
            rupi_config::names_match(&p.id, name)
                || rupi_config::names_match(&p.name, name)
                || rupi_config::names_match(&p.spec, name)
        }) {
            return Some(rec);
        }
    }
    None
}

fn format_list(home: &Path, cwd: &Path, only: Option<&ConfigKind>) -> anyhow::Result<String> {
    let settings = Settings::load(home, cwd);
    let mut out = String::new();
    let kinds: Vec<ConfigKind> = match only {
        Some(k) => vec![*k],
        None => ConfigKind::all().to_vec(),
    };
    for kind in kinds {
        out.push_str(&format!("{}:\n", kind.as_str()));
        let rows = match kind {
            ConfigKind::Packages => list_packages(home, cwd, &settings),
            ConfigKind::Extensions => list_extensions(home, cwd, &settings),
            ConfigKind::Skills => list_skills(home, cwd, &settings),
            ConfigKind::Prompts => list_prompts(home, cwd, &settings),
            ConfigKind::Themes => list_themes(home, cwd, &settings),
        };
        if rows.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for (name, on, src) in rows {
                let flag = if on { "on " } else { "off" };
                out.push_str(&format!("  {flag}  {name}  {src}\n"));
            }
        }
    }
    Ok(out)
}

fn list_packages(home: &Path, cwd: &Path, settings: &Settings) -> Vec<(String, bool, String)> {
    let filter = ResourceFilter::from_specs(settings.packages.as_deref());
    let mut rows = Vec::new();
    for local in [false, true] {
        let opts = InstallOpts {
            home: home.to_path_buf(),
            cwd: cwd.to_path_buf(),
            local,
            kind: rupi_pkg::ResourceKind::Auto,
        };
        let src = if local { "project" } else { "user" };
        for rec in list_installed(&opts) {
            let on = filter.allows(&rec.id);
            rows.push((rec.id, on, src.to_string()));
        }
    }
    rows
}

fn list_extensions(home: &Path, cwd: &Path, settings: &Settings) -> Vec<(String, bool, String)> {
    let filter = ResourceFilter::from_specs(settings.extensions.as_deref());
    let mut dirs = vec![home.join("extensions")];
    dirs.push(cwd.join(".rupi/extensions"));
    let extra = ResourceFilter::from_specs(settings.extensions.as_deref());
    dirs.extend(extra.extra_paths);
    scan_named_json(&dirs, &filter, |p| rupi_pkg::extension_install_name(p))
}

fn list_skills(home: &Path, cwd: &Path, settings: &Settings) -> Vec<(String, bool, String)> {
    let filter = ResourceFilter::from_specs(settings.skills.as_deref());
    let user = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let dirs =
        rupi_skills::skill_search_dirs(PathBuf::from("skills/builtin"), home, &user, cwd, true);
    let extra: Vec<PathBuf> = filter.extra_paths.clone();
    let reg = SkillRegistry::discover(&dirs);
    if !extra.is_empty() {
        reg.ingest_paths(&extra);
    }
    let mut names = reg.names();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let on = filter.allows(&n);
            (n, on, "discovered".to_string())
        })
        .collect()
}

fn list_prompts(home: &Path, _cwd: &Path, settings: &Settings) -> Vec<(String, bool, String)> {
    let filter = rupi_core::commands::PromptFilter::from_specs(settings.prompts.as_deref());
    let dirs = rupi_core::commands::command_dirs_for(home, true, &filter);
    let table = rupi_core::commands::discover_filtered(&dirs, Some(&filter));
    let mut names: Vec<String> = table.keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let on = filter.allows_prompt(&n);
            (n, on, "discovered".to_string())
        })
        .collect()
}

fn list_themes(home: &Path, cwd: &Path, settings: &Settings) -> Vec<(String, bool, String)> {
    let filter = ResourceFilter::from_specs(settings.themes.as_deref());
    let cat = rupi_tui::theme::ThemeCatalog::discover(home, cwd, true, settings.themes.as_deref());
    cat.entries
        .into_iter()
        .map(|e| {
            let on = e.source == "builtin" || filter.allows(&e.name);
            (e.name, on, e.source.to_string())
        })
        .collect()
}

fn scan_named_json(
    dirs: &[PathBuf],
    filter: &ResourceFilter,
    name_of: impl Fn(&Path) -> String,
) -> Vec<(String, bool, String)> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for p in files {
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = name_of(&p);
            if !seen.insert(name.clone()) {
                continue;
            }
            rows.push((name.clone(), filter.allows(&name), "discovered".into()));
        }
    }
    rows
}

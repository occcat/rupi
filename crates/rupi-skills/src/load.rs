use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::Deserialize;

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub file_path: PathBuf,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: Option<bool>,
}

pub fn parse_frontmatter(content: &str) -> Option<(serde_yaml::Value, String)> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Some((serde_yaml::Value::Mapping(Default::default()), normalized));
    }
    let end = normalized.find("\n---")?;
    let yaml = &normalized[3..end];
    let body = normalized[end + 4..].trim().to_string();
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap_or(serde_yaml::Value::Null);
    Some((value, body))
}

pub fn format_skill_invocation(skill: &Skill, extra: Option<&str>) -> String {
    let dir = skill
        .file_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {dir}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.content
    );
    match extra {
        Some(e) if !e.is_empty() => format!("{block}\n\n{e}"),
        _ => block,
    }
}

/// Pi discovery order plus Hermes/rupi homes:
/// `~/.rupi/skills`, `~/.rupi/agent/skills`, `~/.pi/agent/skills`,
/// `.rupi/skills`, `.pi/skills`, `.agents/skills` walking ancestors.
pub fn discover_skill_dirs(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join("skills"),
        home.join("agent").join("skills"),
        dirs_home().join(".pi").join("agent").join("skills"),
        dirs_home().join(".hermes").join("skills"),
    ];
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        dirs.push(dir.join(".rupi").join("skills"));
        dirs.push(dir.join(".pi").join("skills"));
        dirs.push(dir.join(".agents").join("skills"));
        dirs.push(dir.join("skills"));
        cur = dir.parent().map(|p| p.to_path_buf());
        if dir.parent().is_none() {
            break;
        }
    }
    dirs.into_iter().filter(|p| p.is_dir()).collect()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn load_skills(dirs: &[PathBuf]) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        load_dir(dir, dir, true, &mut skills, &mut diagnostics, &mut seen);
    }
    (skills, diagnostics)
}

fn load_dir(
    dir: &Path,
    root: &Path,
    include_root_md: bool,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
    seen: &mut std::collections::HashSet<String>,
) {
    let walker = WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(6))
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let is_skill_md = name == "SKILL.md";
        let is_root_md = include_root_md
            && path.parent() == Some(dir)
            && name.ends_with(".md")
            && name != "SKILL.md";
        if !is_skill_md && !is_root_md {
            continue;
        }
        match load_skill_file(path) {
            Ok(Some(skill)) => {
                if seen.insert(skill.name.clone()) {
                    skills.push(skill);
                }
            }
            Ok(None) => {
                if is_skill_md {
                    diagnostics.push(SkillDiagnostic {
                        code: "invalid_metadata",
                        message: "SKILL.md missing description".into(),
                        path: path.to_path_buf(),
                    });
                }
            }
            Err(e) => diagnostics.push(SkillDiagnostic {
                code: "read_failed",
                message: e,
                path: path.to_path_buf(),
            }),
        }
        let _ = root;
    }
}

fn load_skill_file(path: &Path) -> Result<Option<Skill>, String> {
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (fm_value, body) = parse_frontmatter(&raw).ok_or_else(|| "parse_failed".to_string())?;
    let fm: SkillFrontmatter = serde_yaml::from_value(fm_value).unwrap_or_default();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("skill");
    let name = fm.name.unwrap_or_else(|| parent.to_string());
    let description = fm.description.unwrap_or_default();
    let is_declared = path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md");
    if description.trim().is_empty() && !is_declared {
        return Ok(None);
    }
    if description.trim().is_empty() {
        return Ok(None);
    }
    if name.len() > MAX_NAME_LENGTH || !valid_name(&name) {
        // still load, matching Pi's warning-not-reject diagnostics
    }
    Ok(Some(Skill {
        name,
        description,
        content: body,
        file_path: path.to_path_buf(),
        disable_model_invocation: fm.disable_model_invocation.unwrap_or(false),
    }))
}

pub fn valid_name(name: &str) -> bool {
    let re_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    re_ok
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name.len() <= MAX_NAME_LENGTH
}

pub fn skill_prompt_entries(skills: &[Skill]) -> Vec<rupi_agent_core::SkillPromptEntry> {
    skills
        .iter()
        .map(|s| rupi_agent_core::SkillPromptEntry {
            name: s.name.clone(),
            description: s.description.clone(),
            location: s.file_path.display().to_string(),
            disable_model_invocation: s.disable_model_invocation,
        })
        .collect()
}

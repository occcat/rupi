use ignore::WalkBuilder;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_NAME_LENGTH: usize = 64;
    #[allow(dead_code)]
    pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub content: String,
    pub disable_model_invocation: bool,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub message: String,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation", default)]
    disable_model_invocation: bool,
}

pub fn load_skills(dirs: &[PathBuf]) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let mut walker = WalkBuilder::new(dir);
        walker.hidden(false).git_ignore(true);
        for entry in walker.build() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let is_skill_md = name == "SKILL.md";
            let is_root_md = path.parent() == Some(dir.as_path()) && name.ends_with(".md");
            if !is_skill_md && !is_root_md {
                continue;
            }
            match load_skill_from_file(path) {
                Ok(Some(skill)) => {
                    if !seen.insert(skill.name.clone()) {
                        diagnostics.push(SkillDiagnostic {
                            message: format!("duplicate skill name `{}`, keeping first", skill.name),
                            path: path.to_path_buf(),
                        });
                        continue;
                    }
                    skills.push(skill);
                }
                Ok(None) => {}
                Err(d) => diagnostics.push(d),
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    (skills, diagnostics)
}

fn load_skill_from_file(path: &Path) -> Result<Option<Skill>, SkillDiagnostic> {
    let raw = fs::read_to_string(path).map_err(|e| SkillDiagnostic {
        message: format!("read failed: {e}"),
        path: path.to_path_buf(),
    })?;
    let (fm, body) = parse_frontmatter(&raw).ok_or_else(|| SkillDiagnostic {
        message: "missing YAML frontmatter".into(),
        path: path.to_path_buf(),
    })?;
    let parsed: Frontmatter = serde_yaml::from_str(&fm).map_err(|e| SkillDiagnostic {
        message: format!("parse failed: {e}"),
        path: path.to_path_buf(),
    })?;
    let description = match parsed.description.filter(|d| !d.trim().is_empty()) {
        Some(d) => d,
        None => {
            return Err(SkillDiagnostic {
                message: "missing description; skill not loaded".into(),
                path: path.to_path_buf(),
            });
        }
    };
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    let name = parsed
        .name
        .filter(|n| !n.is_empty())
        .unwrap_or(parent_name);
    let file_path = path.to_path_buf();
    let base_dir = path.parent().unwrap_or(path).to_path_buf();
    Ok(Some(Skill {
        name,
        description,
        file_path,
        base_dir,
        content: body.trim().to_string(),
        disable_model_invocation: parsed.disable_model_invocation,
        source: "disk".into(),
    }))
}

fn parse_frontmatter(raw: &str) -> Option<(String, String)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let rest = raw.strip_prefix("---\n").or_else(|| raw.strip_prefix("---\r\n"))?;
    let (end, sep_len) = if let Some(i) = rest.find("\n---\n") {
        (i, 5)
    } else if let Some(i) = rest.find("\n---\r\n") {
        (i, 6)
    } else if let Some(i) = rest.find("\r\n---\r\n") {
        (i, 7)
    } else {
        return None;
    };
    Some((rest[..end].to_string(), rest[end + sep_len..].to_string()))
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|s| !s.disable_model_invocation).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".into(),
        "Use skill_view to load the full SKILL.md when the task matches.".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    for s in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", xml(&s.name)));
        lines.push(format!("    <description>{}</description>", xml(&s.description)));
        lines.push(format!(
            "    <location>{}</location>",
            xml(&s.file_path.display().to_string())
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn valid_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LENGTH {
        return Err("skill name must be 1-64 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("skill name must be lowercase a-z, 0-9, hyphens".into());
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err("skill name must not start/end with hyphen or contain consecutive hyphens".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_skill_md() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("pdf-tools");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf-tools\ndescription: Extract text from PDFs.\n---\n\n# PDF\nUse pdftotext.\n",
        )
        .unwrap();
        let (skills, diags) = load_skills(&[dir.path().to_path_buf()]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "pdf-tools");
        assert!(skills[0].content.contains("pdftotext"));
    }
}

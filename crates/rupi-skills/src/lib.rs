//! rupi-skills: Agent Skills 开放标准 + Skill 自积累。
//! 标准：目录 + `SKILL.md`（YAML frontmatter 必须含 name/description，body 为指令），
//! 渐进披露三阶段：metadata(~100 tokens) → 全文(<5000 tokens) → resources/scripts 按需加载。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub dir: PathBuf,
    pub meta: SkillMetadata,
    /// SKILL.md 全文（body 部分，不含 frontmatter）
    pub instructions: String,
    pub has_scripts: bool,
    pub has_references: bool,
}

impl Skill {
    /// 解析单个 skill 目录。校验 name/description 约束（小写-数字-连字符，≤64/≤1024）。
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let md_path = dir.join("SKILL.md");
        let raw = std::fs::read_to_string(&md_path)?;
        let (front, body) = split_frontmatter(&raw)?;
        let meta: SkillMetadata = serde_yaml::from_str(&front)?;
        validate_name(&meta.name)?;
        if meta.description.is_empty() || meta.description.len() > 1024 {
            anyhow::bail!("skill description must be 1..=1024 chars");
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
            instructions: body,
            has_scripts: dir.join("scripts").exists(),
            has_references: dir.join("references").exists(),
        })
    }

    /// 第一阶段：仅 metadata（约 100 tokens），全部 skill 常驻系统提示。
    pub fn advertise(&self) -> String {
        format!("- {}: {}", self.meta.name, self.meta.description)
    }
}

fn split_frontmatter(raw: &str) -> anyhow::Result<(String, String)> {
    let raw = raw.trim_start();
    if !raw.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter");
    }
    let mut parts = raw.splitn(3, "---");
    parts.next();
    let front = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing frontmatter"))?
        .to_string();
    let body = parts.next().unwrap_or("").trim().to_string();
    Ok((front, body))
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("skill name must be 1..=64 chars");
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("skill name must not start/end with hyphen");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!("skill name must be lowercase alnum + hyphen");
    }
    Ok(())
}

/// Skill 注册表：多目录发现（内建 / 用户 / 项目级），渐进披露加载。
/// 内部可变：`refresh` 原地热更新，会话内新蒸馏的 skill 下一轮即对模型可见。
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: std::sync::RwLock<Vec<Skill>>,
}

impl SkillRegistry {
    pub fn discover(dirs: &[PathBuf]) -> Self {
        let mut this = Self::default();
        this.refresh(dirs);
        this
    }

    /// 重扫目录并原地替换，返回 skill 数量。失败的目录只 warning（与初次发现同语义）。
    pub fn refresh(&self, dirs: &[PathBuf]) -> usize {
        let mut skills = vec![];
        for base in dirs {
            if !base.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(base)
                .max_depth(2)
                .into_iter()
                .flatten()
            {
                let p = entry.path();
                if p.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
                    if let Some(dir) = p.parent() {
                        match Skill::load(dir) {
                            Ok(s) => skills.push(s),
                            Err(e) => tracing::warn!("skip skill {}: {e:#}", dir.display()),
                        }
                    }
                }
            }
        }
        // 同名去重：先发现者胜
        let mut seen = std::collections::HashSet::new();
        skills.retain(|s| seen.insert(s.meta.name.clone()));
        let n = skills.len();
        *self.skills.write().unwrap() = skills;
        n
    }

    /// 系统提示索引块（阶段 1）。
    pub fn index_block(&self) -> String {
        let skills = self.skills.read().unwrap();
        if skills.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n<AvailableSkills>\nLoad full instructions with load_skill(name) when a task matches.\n");
        for sk in skills.iter() {
            s.push_str(&sk.advertise());
            s.push('\n');
        }
        s.push_str("</AvailableSkills>\n");
        s
    }

    /// 阶段 2：激活 skill，返回全文指令。
    pub fn load_skill(&self, name: &str) -> Option<String> {
        self.skills
            .read()
            .unwrap()
            .iter()
            .find(|s| s.meta.name == name)
            .map(|s| s.instructions.clone())
    }

    /// 阶段 3：按需读资源文件（references/、assets/、templates/）。
    pub fn read_resource(&self, name: &str, rel: &str) -> anyhow::Result<String> {
        let skills = self.skills.read().unwrap();
        let sk = skills
            .iter()
            .find(|s| s.meta.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown skill {name}"))?;
        let p = sk.dir.join(rel);
        if !p.starts_with(&sk.dir) {
            anyhow::bail!("path escapes skill dir");
        }
        Ok(std::fs::read_to_string(&p)?)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.skills.read().unwrap().len()
    }
}

/// Skill 自积累：从一轮成功 transcript 提炼可复用流程，生成新 SKILL.md 草稿。
/// 由后台 review（对标 Hermes background_review）调用，落盘前需人工/规则校验。
pub struct SkillAccumulator {
    pub dest_dir: PathBuf,
}

impl SkillAccumulator {
    pub fn new(dest_dir: PathBuf) -> Self {
        Self { dest_dir }
    }

    /// 启发式提炼：调用者传入“任务描述 + 关键步骤”，生成符合规范的 SKILL.md。
    /// 落盘前校验：description 压成单行且 1..=1024 字符（否则写出的 frontmatter 下次发现即跳过），
    /// steps 非空（空流程没有复用价值），超 50 步截断。
    pub fn propose(
        &self,
        name: &str,
        description: &str,
        steps: &[String],
    ) -> anyhow::Result<PathBuf> {
        validate_name(name)?;
        let description: String = description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if description.is_empty() || description.len() > 1024 {
            anyhow::bail!("skill description must be 1..=1024 chars");
        }
        if steps.is_empty() {
            anyhow::bail!("skill steps must be non-empty");
        }
        let dir = self.dest_dir.join(name);
        if dir.exists() {
            anyhow::bail!("skill {name} already exists");
        }
        std::fs::create_dir_all(dir.join("references"))?;
        let mut body =
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n");
        body.push_str(" distilled from a successful session. Follow these steps:\n\n");
        for (i, st) in steps.iter().take(50).enumerate() {
            body.push_str(&format!("{}. {}\n", i + 1, st));
        }
        body.push_str("\nMove details to `references/` if this file grows past 500 lines.\n");
        std::fs::write(dir.join("SKILL.md"), &body)?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_load_and_progressive_disclosure() {
        let base = std::env::temp_dir().join(format!("rupi-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: do demo things\n---\n\n# Demo\nDo X.\n",
        )
        .unwrap();
        let reg = SkillRegistry::discover(&[base.clone()]);
        assert_eq!(reg.len(), 1);
        assert!(reg.index_block().contains("demo-skill"));
        assert!(reg.load_skill("demo-skill").unwrap().contains("Do X"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn accumulator_rejects_bad_names() {
        let acc = SkillAccumulator::new(std::env::temp_dir());
        assert!(acc.propose("Bad_Name", "d", &[]).is_err());
    }

    #[test]
    fn accumulator_validates_description_and_steps() {
        let base = std::env::temp_dir().join(format!("rupi-skill-acc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let acc = SkillAccumulator::new(base.clone());
        // 空步骤 / 空描述 / 超长描述拒绝
        assert!(acc.propose("s1", "ok", &[]).is_err());
        assert!(acc.propose("s2", "  \n ", &["step".into()]).is_err());
        assert!(acc.propose("s3", &"x".repeat(2000), &["step".into()]).is_err());
        // 换行描述压单行，落盘后可被重新发现
        let dir = acc
            .propose("multiline-desc", "line one\nline two", &["do it".into()])
            .unwrap();
        let raw = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(raw.contains("line one line two"));
        let reg = SkillRegistry::discover(&[base.clone()]);
        assert!(reg.load_skill("multiline-desc").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_refresh_picks_up_new_skills() {
        let base = std::env::temp_dir().join(format!("rupi-skill-ref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let reg = SkillRegistry::discover(&[base.clone()]);
        assert_eq!(reg.len(), 0);
        assert!(reg.index_block().is_empty());
        // 会话中途新增 skill：refresh 后立即可见（自积累闭环）
        let dir = base.join("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: fresh-skill\ndescription: just arrived\n---\n\n# Fresh\nGo.\n",
        )
        .unwrap();
        assert_eq!(reg.refresh(&[base.clone()]), 1);
        assert!(reg.index_block().contains("fresh-skill"));
        assert!(reg.load_skill("fresh-skill").unwrap().contains("Go."));
        let _ = std::fs::remove_dir_all(&base);
    }
}

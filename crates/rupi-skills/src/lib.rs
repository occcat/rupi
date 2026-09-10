//! rupi-skills: Agent Skills 开放标准 + Skill 自积累。
//! 标准：目录 + `SKILL.md`（YAML frontmatter 必须含 name/description，body 为指令），
//! 渐进披露三阶段：metadata(~100 tokens) → 全文(<5000 tokens) → resources/scripts 按需加载。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

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
    /// 实际入口文件：常规为 `SKILL.md`，根 `.md` 回退时为对应的 `.md` 文件。
    /// `load_skill` 的 location 必须指真实文件，否则模型按址取不到内容。
    pub entry: PathBuf,
    pub meta: SkillMetadata,
    /// SKILL.md 全文（body 部分，不含 frontmatter）
    pub instructions: String,
    pub has_scripts: bool,
    pub has_references: bool,
}

impl Skill {
    /// 解析单个 skill 目录。校验 name/description 约束（小写-数字-连字符，≤64/≤1024 字符）。
    /// description 按字符计数（中文场景按字节算会把上限压到 1/3）。
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Self::load_file(&dir.join("SKILL.md"))
    }

    /// 从任意 `.md` 文件解析 skill（对标上游根 `.md` 回退）：frontmatter 缺 name 时
    /// 回退父目录名；缺 description（或空）则拒绝——无 description 无法 advertise。
    pub fn load_file(md_path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(md_path)?;
        let (front, body) = split_frontmatter(&raw)?;
        #[derive(serde::Deserialize)]
        struct Front {
            name: Option<String>,
            description: Option<String>,
            license: Option<String>,
            compatibility: Option<String>,
        }
        let front: Front = serde_yaml::from_str(&front)?;
        let dir = md_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let name = match front.name.filter(|n| !n.is_empty()) {
            Some(n) => n,
            None => dir
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("skill name missing and parent dir has no name"))?
                .to_string(),
        };
        let description = match front.description.filter(|d| !d.trim().is_empty()) {
            Some(d) => d,
            None => anyhow::bail!("skill description must be 1..=1024 chars"),
        };
        Self::assemble(
            dir,
            md_path.to_path_buf(),
            name,
            description,
            body,
            front.license,
            front.compatibility,
        )
    }

    fn assemble(
        dir: PathBuf,
        entry: PathBuf,
        name: String,
        description: String,
        body: String,
        license: Option<String>,
        compatibility: Option<String>,
    ) -> anyhow::Result<Self> {
        validate_name(&name)?;
        if description.is_empty() || description.chars().count() > 1024 {
            anyhow::bail!("skill description must be 1..=1024 chars");
        }
        Ok(Self {
            dir: dir.clone(),
            entry,
            meta: SkillMetadata {
                name,
                description,
                license,
                compatibility,
            },
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
    // 上游 validateName 同款：连续连字符拒绝（`auto--x` 这类 review 拼装名上游不认）
    if name.contains("--") {
        anyhow::bail!("skill name must not contain consecutive hyphens");
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
        let this = Self::default();
        this.refresh(dirs);
        this
    }

    /// 重扫目录并原地替换，返回 skill 数量。失败的目录只 warning（与初次发现同语义）。
    ///
    /// 发现规则对标上游 `loadSkillsFromDirInternal`：含 `SKILL.md` 的目录即视为
    /// skill 根，不再下钻（skill 内部 `references/*.md` 不会被误收）；否则递归子目录
    /// （不限深度，支持 `base/group/<skill>/SKILL.md` 分组嵌套）；仅顶层直接 `.md`
    /// 子文件可回退为 skill（有 frontmatter description 才收，无则静默跳过）。
    /// `.gitignore/.ignore/.fdignore` 逐目录 honor（与上游同名文件同语义，点文件与
    /// `node_modules` 照例先跳过）。条目按文件名排序，保证同名去重（先发现者胜）跨次稳定。
    pub fn refresh(&self, dirs: &[PathBuf]) -> usize {
        let mut skills: Vec<Skill> = vec![];
        let mut seen_files = std::collections::HashSet::new();
        for base in dirs {
            if !base.exists() {
                continue;
            }
            // 预扫 ignore 规则一次建成 matcher（与 visit 同步遍历，规则集等价于懒收集）。
            let mut builder = GitignoreBuilder::new(base);
            Self::collect_ignore_lines(
                base,
                base,
                &mut builder,
                &mut std::collections::HashSet::new(),
                0,
            );
            let ig = match builder.build() {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("skill ignore rules invalid under {}: {e:#}", base.display());
                    Gitignore::empty()
                }
            };
            Self::visit(
                base,
                base,
                true,
                &ig,
                &mut skills,
                &mut seen_files,
                &mut std::collections::HashSet::new(),
                0,
            );
        }
        // 同名去重：先发现者胜；后来者记 warning（对标上游 collision diagnostic）。
        let mut seen = std::collections::HashSet::new();
        let mut collisions = vec![];
        skills.retain(|s| {
            if seen.insert(s.meta.name.clone()) {
                true
            } else {
                collisions.push(format!("{} at {}", s.meta.name, s.entry.display()));
                false
            }
        });
        for c in collisions {
            tracing::warn!("skill name collision, keeping first: {c}");
        }
        let n = skills.len();
        *self.skills.write().unwrap() = skills;
        n
    }

    /// 递归扫描单个目录。`root` 为本次 base（算相对路径用），`include_root_files`
    /// 仅顶层为真。目录 symlink 跟随进入，但 canonical 目录去重（防环＋同一 skill
    /// 经 symlink 多路径到达只收一次）；断链 symlink 的 `is_file/is_dir` 为假，天然跳过。
    /// 被忽略的 `SKILL.md` 不具终端语义（对标上游 `ignores → continue`，外层继续下钻）。
    #[allow(clippy::too_many_arguments)]
    fn visit(
        dir: &Path,
        root: &Path,
        include_root_files: bool,
        ig: &Gitignore,
        out: &mut Vec<Skill>,
        seen_files: &mut std::collections::HashSet<PathBuf>,
        seen_dirs: &mut std::collections::HashSet<PathBuf>,
        depth: usize,
    ) {
        if depth > 32 {
            tracing::warn!("skill scan too deep, stop at {}", dir.display());
            return;
        }
        let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !seen_dirs.insert(canon_dir) {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => {
                let mut v: Vec<_> = rd.flatten().collect();
                v.sort_by_key(|a| a.file_name());
                v
            }
            Err(e) => {
                tracing::warn!("skip skill dir {}: {e:#}", dir.display());
                return;
            }
        };
        // skill 根优先：未被忽略的 SKILL.md 收后即返，不再下钻。
        // 注：matched 必须传完整路径（ignore 内部按 base 做 strip_prefix，
        // 传相对路径恒为 NoMatch）。
        if let Some(md) = entries
            .iter()
            .find(|e| e.file_name().to_str() == Some("SKILL.md"))
            .map(|e| e.path())
            .filter(|p| p.is_file() && !ig.matched(p, false).is_ignore())
        {
            Self::collect(out, seen_files, root, &md, true);
            return;
        }
        for e in &entries {
            let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let p = e.path();
            let is_dir = p.is_dir();
            // 目录按完整路径测（ignore 内部自行相对 base 换算，目录加不加 `/` 皆可，
            // 传完整路径即可；断链 symlink 双假，天然跳过）。
            if ig.matched(&p, is_dir).is_ignore() {
                continue;
            }
            if is_dir {
                Self::visit(&p, root, false, ig, out, seen_files, seen_dirs, depth + 1);
            } else if include_root_files && p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("md") {
                Self::collect(out, seen_files, root, &p, false);
            }
        }
    }

    /// base 下相对路径（POSIX 分隔，供 ignore 规则拼前缀）。root 自身返回全文，
    /// 调用方以 `dir == root` 判空前缀；symlink 指外同样回退全文（规则按字面拼接，
    /// 与上游 `relativeEnvPath` 回退一致：裸 pattern 照 basename 生效，锚定 pattern 不命中）。
    fn rel_posix(root: &Path, p: &Path) -> String {
        match p.strip_prefix(root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
            _ => p.to_string_lossy().replace('\\', "/"),
        }
    }

    /// 预扫 ignore 规则（对标上游 `addIgnoreRules` 逐目录收集）：`.gitignore/.ignore/.fdignore`
    /// 按“相对 root 前缀”改写后一次建成 matcher。遍历与 visit 同步（含 SKILL.md 终端语义），
    /// 规则集等价于边走边收。缺文件静默，读失败记 warn（与上游 file_info/read_failed 同约）。
    fn collect_ignore_lines(
        dir: &Path,
        root: &Path,
        builder: &mut GitignoreBuilder,
        seen_dirs: &mut std::collections::HashSet<PathBuf>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !seen_dirs.insert(canon_dir) {
            return;
        }
        let rel = Self::rel_posix(root, dir);
        // root 自身 rel 为全文（rel_posix 回退），以 dir==root 判空前缀。
        let prefix = if dir == root {
            String::new()
        } else {
            format!("{rel}/")
        };
        for name in [".gitignore", ".ignore", ".fdignore"] {
            let file = dir.join(name);
            let content = match std::fs::read_to_string(&file) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!("skip unreadable {}: {e:#}", file.display());
                    continue;
                }
            };
            for line in content.lines() {
                if let Some(pat) = Self::prefix_ignore_pattern(line, &prefix) {
                    if let Err(err) = builder.add_line(None, &pat) {
                        tracing::warn!("bad ignore pattern {}: {err}", file.display());
                    }
                }
            }
        }
        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => return,
        };
        entries.sort_by_key(|a| a.file_name());
        if entries
            .iter()
            .any(|e| e.file_name().to_str() == Some("SKILL.md") && e.path().is_file())
        {
            return;
        }
        for e in &entries {
            let skip = e
                .file_name()
                .to_str()
                .map(|n| n.starts_with('.') || n == "node_modules")
                .unwrap_or(true);
            if skip {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                Self::collect_ignore_lines(&p, root, builder, seen_dirs, depth + 1);
            }
        }
    }

    /// 上游 `prefixIgnorePattern` 同款移植：空行/注释丢弃，`!` 取反，`/` 去锚，
    /// 再拼相对 root 前缀。唯二适配：无 slash 裸 pattern 在子目录前缀下强制锚定
    /// （`group/*.log` 只拦 group 下；gitignore 裸 pattern 本会匹配任意层级），
    /// `\#`/`\!` 转义保留反斜杠透传（预先剥掉会让 `#` 被当注释丢掉）。
    fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
            return None;
        }
        let mut pattern = line.to_string();
        let mut negated = false;
        if pattern.starts_with('!') {
            negated = true;
            pattern.remove(0);
        }
        if pattern.starts_with('/') {
            pattern.remove(0);
        }
        let anchored = pattern.contains('/') || pattern.ends_with('/') || prefix.is_empty();
        let mut out = if anchored {
            format!("{prefix}{pattern}")
        } else {
            format!("/{prefix}{pattern}")
        };
        if negated {
            out.insert(0, '!');
        }
        Some(out)
    }

    /// 收单个 `.md` 为 skill：canonical 文件去重；`declared`（SKILL.md）失败记 warn，
    /// 根 `.md` 回退失败只记 debug（普通 md 本来就不是 skill，不应噪音）。
    fn collect(
        out: &mut Vec<Skill>,
        seen_files: &mut std::collections::HashSet<PathBuf>,
        _root: &Path,
        md: &Path,
        declared: bool,
    ) {
        match Skill::load_file(md) {
            Ok(s) => {
                let canon = md.canonicalize().unwrap_or_else(|_| md.to_path_buf());
                if seen_files.insert(canon) {
                    out.push(s);
                }
            }
            Err(e) if declared => tracing::warn!("skip skill {}: {e:#}", md.display()),
            Err(e) => tracing::debug!("skip non-skill md {}: {e:#}", md.display()),
        }
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

    /// 阶段 2：激活 skill，返回调用块（对标上游 `formatSkillInvocation`）：
    /// location 让模型知道 skill 落在哪，`References are relative to …` 让阶段 3 的
    /// `read_resource(name, path)` 相对路径有锚可依，不再盲猜。
    pub fn load_skill(&self, name: &str) -> Option<String> {
        self.skills.read().unwrap().iter().find(|s| s.meta.name == name).map(|s| {
            format!(
                "<skill name=\"{name}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                s.entry.display(),
                s.dir.display(),
                s.instructions,
            )
        })
    }

    /// 阶段 3：按需读资源文件（references/、assets/、templates/）。
    /// 符号链接消解后仍须在 skill 目录内（与工作区沙箱同口径）：skill 可由复盘自动
    /// 蒸馏，词法 `starts_with` 挡不住目录内指外的软链。
    pub fn read_resource(&self, name: &str, rel: &str) -> anyhow::Result<String> {
        let skills = self.skills.read().unwrap();
        let sk = skills
            .iter()
            .find(|s| s.meta.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown skill {name}"))?;
        let canonical_dir = sk.dir.canonicalize().unwrap_or_else(|_| sk.dir.clone());
        // 只读存在文件：消解失败与读失败同为 Err（原 read_to_string 语义不变）
        let canonical = sk.dir.join(rel).canonicalize()?;
        if !canonical.starts_with(&canonical_dir) {
            anyhow::bail!("path escapes skill dir");
        }
        Ok(std::fs::read_to_string(&canonical)?)
    }

    fn len(&self) -> usize {
        self.skills.read().unwrap().len()
    }

    /// 模型可见的工具 schema（阶段 2 + 3）：没有它们，执行分支再完备模型也调不到。
    /// 只在注册表非空时由调用方挂载，避免空 skill 环境污染工具表。
    pub fn tool_definitions(&self) -> Vec<rupi_core::ToolDefinition> {
        if self.len() == 0 {
            return vec![];
        }
        vec![
            rupi_core::ToolDefinition {
                name: "load_skill".into(),
                description: "Load a skill's full instructions by name (progressive disclosure stage 2)".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
                prompt_snippet: Some(
                    "load_skill(name): read full skill instructions when a task matches".into(),
                ),
            },
            rupi_core::ToolDefinition {
                name: "read_resource".into(),
                description: "Read a file inside a skill dir (references/, scripts/, assets/); path must stay inside the skill".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "path": {"type": "string"}
                    },
                    "required": ["name", "path"]
                }),
                prompt_snippet: Some(
                    "read_resource(name, path): fetch skill resources on demand (stage 3)".into(),
                ),
            },
        ]
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

    /// 同名 skill 是否已存在。调用方（review 落盘）在 propose 前预检：已存在即静默
    /// 跳过，避免每轮重复打印 `skill draft skipped` 噪音；propose 内仍保留拒绝
    /// 检查（防 TOCTOU，显式 `skill-distill` 命令走 Err 语义不变）。
    pub fn exists(&self, name: &str) -> bool {
        self.dest_dir.join(name).exists()
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
        let description: String = description.split_whitespace().collect::<Vec<_>>().join(" ");
        if description.is_empty() || description.chars().count() > 1024 {
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
        // description 进 YAML frontmatter：转义反斜杠/引号并消除 `---`，
        // 否则模型输出的特殊字符会让下次 refresh 解析失败、skill 静默丢失。
        let safe_description = description
            .replace("---", "—")
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let mut body =
            format!("---\nname: {name}\ndescription: \"{safe_description}\"\n---\n\n# {name}\n\n");
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
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert_eq!(reg.len(), 1);
        assert!(reg.index_block().contains("demo-skill"));
        assert!(reg.load_skill("demo-skill").unwrap().contains("Do X"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_skill_returns_invocation_block_with_location() {
        // 对标上游 formatSkillInvocation：location + 相对引用锚点随指令一起给模型，
        // 阶段 3 的 read_resource 相对路径有锚可依
        let base = std::env::temp_dir().join(format!("rupi-skill-invoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: do demo things\n---\n\n# Demo\nDo X.\n",
        )
        .unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        let body = reg.load_skill("demo-skill").expect("skill");
        assert!(body.contains("<skill name=\"demo-skill\""), "{body}");
        assert!(body.contains("SKILL.md"), "{body}");
        assert!(
            body.contains(&format!("References are relative to {}.", dir.display())),
            "{body}"
        );
        assert!(body.contains("Do X."), "{body}");
        assert!(body.trim_end().ends_with("</skill>"), "{body}");
        assert!(reg.load_skill("missing").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tool_definitions_visible_only_when_skills_exist() {
        let base = std::env::temp_dir().join(format!("rupi-skill-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let empty = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(empty.tool_definitions().is_empty());
        let dir = base.join("res");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: res-skill\ndescription: has refs\n---\n\n# Res\nSee references.\n",
        )
        .unwrap();
        std::fs::write(dir.join("references").join("deep.md"), "DEEP-KNOWLEDGE").unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        let defs = reg.tool_definitions();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().all(|d| d.prompt_snippet.is_some()));
        assert!(defs.iter().any(|d| d.name == "load_skill"));
        assert!(defs.iter().any(|d| d.name == "read_resource"));
        // 越界读拒绝
        assert!(reg.read_resource("res-skill", "../SKILL.md").is_err());
        assert!(reg
            .read_resource("res-skill", "references/deep.md")
            .unwrap()
            .contains("DEEP-KNOWLEDGE"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn accumulator_rejects_bad_names() {
        let acc = SkillAccumulator::new(std::env::temp_dir());
        assert!(acc.propose("Bad_Name", "d", &[]).is_err());
        // 上游同款：连续连字符拒绝（与 Skill::load 共用 validate_name）
        assert!(acc.propose("auto--x", "d", &["s".into()]).is_err());
    }

    #[test]
    fn accumulator_validates_description_and_steps() {
        let base = std::env::temp_dir().join(format!("rupi-skill-acc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let acc = SkillAccumulator::new(base.clone());
        // 空步骤 / 空描述 / 超长描述拒绝
        assert!(acc.propose("s1", "ok", &[]).is_err());
        assert!(acc.propose("s2", "  \n ", &["step".into()]).is_err());
        assert!(acc
            .propose("s3", &"x".repeat(2000), &["step".into()])
            .is_err());
        // 换行描述压单行，落盘后可被重新发现
        let dir = acc
            .propose("multiline-desc", "line one\nline two", &["do it".into()])
            .unwrap();
        let raw = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(raw.contains("line one line two"));
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(reg.load_skill("multiline-desc").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_refresh_picks_up_new_skills() {
        let base = std::env::temp_dir().join(format!("rupi-skill-ref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
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
        assert_eq!(reg.refresh(std::slice::from_ref(&base)), 1);
        assert!(reg.index_block().contains("fresh-skill"));
        assert!(reg.load_skill("fresh-skill").unwrap().contains("Go."));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_resource_rejects_symlink_escapes() {
        // 目录内软链指外：词法检查放行，canonicalize 口径必须拦下；
        // 指内的软链照常用（与工作区沙箱同语义）
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("rupi-skill-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("linked");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: link-skill\ndescription: has links\n---\n\n# Linked\nSee refs.\n",
        )
        .unwrap();
        std::fs::write(dir.join("references").join("deep.md"), "DEEP-KNOWLEDGE").unwrap();
        std::fs::write(base.join("outside.txt"), "OUTSIDE-SECRET").unwrap();
        symlink(base.join("outside.txt"), dir.join("references").join("evil")).unwrap();
        symlink(
            dir.join("references").join("deep.md"),
            dir.join("references").join("ok"),
        )
        .unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        let err = reg
            .read_resource("link-skill", "references/evil")
            .unwrap_err();
        assert!(!err.to_string().contains("OUTSIDE-SECRET"), "错误信息不得回显目标内容");
        assert_eq!(
            reg.read_resource("link-skill", "references/ok").unwrap(),
            "DEEP-KNOWLEDGE"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn discovery_supports_nested_groups_and_root_md_fallback() {
        // 对标上游：分组嵌套 base/group/<skill>/SKILL.md 可发现；顶层根 .md
        // 有 description 回退为 skill，无 description 静默跳过；skill 目录为终端，
        // 其内部 references/*.md 与嵌套 SKILL.md 不得被误收。
        let base = std::env::temp_dir().join(format!("rupi-skill-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let grouped = base.join("group").join("deep-skill");
        std::fs::create_dir_all(grouped.join("references")).unwrap();
        std::fs::write(
            grouped.join("SKILL.md"),
            "---\nname: deep-skill\ndescription: nested skill\n---\n\n# Deep\nGo deep.\n",
        )
        .unwrap();
        std::fs::write(grouped.join("references").join("notes.md"), "just notes").unwrap();
        std::fs::write(
            grouped.join("references").join("SKILL.md"),
            "---\nname: fake-inner\ndescription: must not load\n---\n\n# Fake\n",
        )
        .unwrap();
        std::fs::write(
            base.join("standalone.md"),
            "---\nname: root-note\ndescription: top-level md fallback\n---\n\n# Root\nHi.\n",
        )
        .unwrap();
        std::fs::write(base.join("plain.md"), "# no frontmatter, not a skill\n").unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert_eq!(reg.len(), 2);
        assert!(reg.load_skill("deep-skill").unwrap().contains("Go deep."));
        let root = reg.load_skill("root-note").expect("root md fallback");
        assert!(root.contains("Hi."));
        assert!(root.contains("standalone.md"), "{root}");
        assert!(reg.load_skill("fake-inner").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn skill_name_falls_back_to_parent_dir() {
        // frontmatter 缺 name：回退父目录名（对标上游 parentDirName）；缺 description 仍拒绝。
        let base = std::env::temp_dir().join(format!("rupi-skill-noname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("fallback-name");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\ndescription: nameless but describable\n---\n\n# Body\nContent.\n",
        )
        .unwrap();
        let sk = Skill::load(&dir).expect("name fallback");
        assert_eq!(sk.meta.name, "fallback-name");
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(reg.load_skill("fallback-name").is_some());
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: fallback-name\n---\n\n# Body\nNo description.\n",
        )
        .unwrap();
        assert!(Skill::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn description_limit_counts_chars_not_bytes() {
        // 400 中文字符 ≈1200 字节：按字节算会被误杀，按字符算应通过。
        let base = std::env::temp_dir().join(format!("rupi-skill-cjk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let acc = SkillAccumulator::new(base.clone());
        let desc = "中".repeat(400);
        let dir = acc.propose("cjk-skill", &desc, &["do it".into()]).expect("CJK 400 字应通过");
        assert!(dir.join("SKILL.md").exists());
        assert!(acc.propose("cjk-too-long", &"中".repeat(2000), &["do it".into()]).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn accumulator_exists_precheck_avoids_noisy_repropose() {
        // review 落盘预检：已存在即跳过，不再走到 propose 的 Err 分支打印噪音。
        let base = std::env::temp_dir().join(format!("rupi-skill-exists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let acc = SkillAccumulator::new(base.clone());
        assert!(!acc.exists("dup-skill"));
        acc.propose("dup-skill", "first write", &["step".into()]).unwrap();
        assert!(acc.exists("dup-skill"));
        assert!(!acc.exists("other-skill"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gitignore_excluded_skill_dir_is_skipped() {
        // .gitignore 排除 skill 目录：被排除者消失，兄弟 skill 存活。
        let base = std::env::temp_dir().join(format!("rupi-skill-ign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for name in ["hidden", "visible"] {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# Body\nContent.\n"),
            )
            .unwrap();
        }
        std::fs::write(base.join(".gitignore"), "hidden/\n").unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(reg.load_skill("hidden").is_none());
        assert!(reg.load_skill("visible").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gitignore_negation_reinstates_skill() {
        // `!` 取反：`group/*` 排除内容但保留目录本身可再取回（gitignore 标准语义：
        // 父目录本身被排除后子项不可取回，故此处用 `group/*` 而非 `group/`）。
        let base = std::env::temp_dir().join(format!("rupi-skill-neg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for name in ["keep", "drop"] {
            let dir = base.join("group").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# Body\nContent.\n"),
            )
            .unwrap();
        }
        std::fs::write(base.join(".gitignore"), "group/*\n!group/keep/\n").unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(reg.load_skill("keep").is_some());
        assert!(reg.load_skill("drop").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dotignore_file_also_filters_skills() {
        // `.ignore` 文件同样生效（上游三件套之一），无命中 pattern 不影响发现。
        let base = std::env::temp_dir().join(format!("rupi-skill-dotign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for name in ["gone", "stays"] {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# Body\nContent.\n"),
            )
            .unwrap();
        }
        std::fs::write(base.join(".ignore"), "gone/\nno-such-skill-anywhere/\n").unwrap();
        let reg = SkillRegistry::discover(std::slice::from_ref(&base));
        assert!(reg.load_skill("gone").is_none());
        assert!(reg.load_skill("stays").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}

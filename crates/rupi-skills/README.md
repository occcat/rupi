# rupi-skills

Agent Skills：目录 + `SKILL.md`（frontmatter 必含 `name` / `description`）。

渐进披露：metadata → 全文 → `read_resource` 读 `references/` / `assets/`。

发现顺序（先发现者胜）：

1. exe 锚定 `skills/builtin`
2. `~/.rupi/skills`
3. 项目 `.rupi/skills`
4. `~/.pi/agent/skills`、`~/.agents/skills`
5. 从 cwd 上溯的 `.pi/skills` / `.agents/skills`（有 `.git` 停在仓库根）

`SkillAccumulator::propose` 落盘前校验 name / description / steps。`rupi skill-distill` 手工蒸馏。REPL/TUI 每轮 `refresh`。

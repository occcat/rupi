//! 权限门：对标 Pi 的 permission gates 扩展与 plan mode。
//!
//! 执行链：`RulePolicy` 先判（允许/拒绝/转人工）→ `Ask` 有 `Approver` 则问，无则拒绝。
//! 拒绝永远转成 tool error 回给模型，主循环不中断；plan mode = 只读规则 + 提示词声明。

use serde::{Deserialize, Serialize};

/// 工具调用裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    Ask(String),
}

pub trait Policy: Send + Sync {
    fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision;
}

/// 审批器：交互式 human-in-the-loop（REPL 用 stdin 实现，TUI/非交互可另接）。
/// 同步即可：审批是低频人工动作，调用方在拿到裁决后再继续。
pub trait Approver: Send + Sync {
    fn approve(&self, tool: &str, args: &serde_json::Value, reason: &str) -> bool;
}

/// Ask 的三种宿主动作。`Interrupt` 供云控制面把提案落盘并以 AG-UI 收尾；
/// 本机 TUI/CLI 继续走 [`Approver`]（Allow/Deny），不使用此枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskAction {
    Allow,
    Deny,
    /// 暂停本轮：不执行该工具、不发 `ToolEnd`。宿主读 [`PendingInterrupt`]。
    Interrupt,
}

/// 云路径挂起的工具提案（不进 [`crate::AgentEvent`] serde）。
#[derive(Debug, Clone)]
pub struct PendingInterrupt {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub reason: String,
}

/// 默认：全放行（与此前行为一致）。
pub struct AllowAll;
impl Policy for AllowAll {
    fn decide(&self, _tool: &str, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}

/// 计划模式白名单：只放行只读工具（对标上游 plan mode 的 read-only 工具集）。
/// memory 写入、MCP/扩展工具和可选工具都可能有副作用，一律不在其中。
/// `subagent` 放行：子 agent 继承父的计划模式，同样只能只读。
const PLAN_MODE_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "find",
    "ls",
    "think",
    "load_skill",
    "read_resource",
    "memory_search",
    "session_search",
    "recall",
    "search_tools",
    "subagent",
];

pub fn plan_mode_allows(tool: &str) -> bool {
    PLAN_MODE_TOOLS.contains(&tool)
}

/// 基于规则的策略（可组合叠加）：
/// - `readonly`: 只放行 [`plan_mode_allows`] 里的只读工具（plan mode 内核）
/// - `deny_tools`: 按名拒绝
/// - `bash_block`: bash 命令命中任一规则即转人工（无 Approver 则拒绝），按 argv 判，见 [`bash_rule_matches`]
/// - `ask_tools`: 按名转人工（如 memory 写、外部扩展调用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RulePolicy {
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub deny_tools: Vec<String>,
    #[serde(default)]
    pub ask_tools: Vec<String>,
    #[serde(default)]
    pub bash_block: Vec<String>,
}

impl RulePolicy {
    pub fn plan_mode() -> Self {
        Self {
            readonly: true,
            ..Default::default()
        }
    }
}

impl Policy for RulePolicy {
    fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision {
        if self.deny_tools.iter().any(|t| t == tool) {
            return Decision::Deny(format!("tool {tool} is denied by policy"));
        }
        if self.readonly && !plan_mode_allows(tool) {
            return Decision::Deny(format!(
                "plan mode: {tool} is disabled; describe the plan instead"
            ));
        }
        if self.ask_tools.iter().any(|t| t == tool) {
            return Decision::Ask(format!("tool {tool} requires approval"));
        }
        if tool == "bash" {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(hit) = self.bash_block.iter().find(|b| bash_rule_matches(b, cmd)) {
                return Decision::Ask(format!("bash command matched blocklist: {hit}"));
            }
        }
        Decision::Allow
    }
}

/// 一条 bash 规则（如 `rm -rf /`、`mkfs`、`dd if=`）是否命中命令。
///
/// 不做子串匹配（`rm -rf /` 会误拦 `rm -rf /tmp/x`，又放过 `rm -fr /`）：先按 shell
/// 规则分词，按 `;` `&&` `|` 换行和括号切成简单命令，剥掉 `sudo`/`env`/赋值前缀和路径，
/// 再逐条比 argv：
/// - 命令名相同（`mkfs` 也匹配 `mkfs.ext4`）；
/// - 规则里的短选项字母都出现在命令里（顺序、合并与否不限；rm 的 `--recursive`/`--force` 等价 `-r`/`-f`）；
/// - 规则里的位置参数都出现在命令里；目标 `/` 同时覆盖 `/*`、`~`、`$HOME`。
/// - `dd` 规则只在 `of=` 指向块设备时命中（`of=/dev/null` 不拦）。
///
/// `sh -c '…'`、`bash -c`、`eval` 会展开一层再判。这是防手滑的安全网，不是沙箱。
pub fn bash_rule_matches(rule: &str, command: &str) -> bool {
    let rule_words = shell_words(rule).into_iter().flatten().collect::<Vec<_>>();
    let Some(rule_cmd) = simple_command(&rule_words) else {
        return false;
    };
    command_matches(&rule_cmd, command, 0)
}

fn command_matches(rule: &SimpleCommand, command: &str, depth: usize) -> bool {
    for words in shell_words(command) {
        let Some(cmd) = simple_command(&words) else {
            continue;
        };
        if depth < 2 {
            if let Some(inner) = cmd.nested_script() {
                if command_matches(rule, &inner, depth + 1) {
                    return true;
                }
            }
        }
        if cmd.matches(rule) {
            return true;
        }
    }
    false
}

/// 剥掉包装后的简单命令：`name` 为去路径的命令名，`flags` 为短选项字母，`args` 为其余参数。
struct SimpleCommand {
    name: String,
    flags: std::collections::BTreeSet<char>,
    args: Vec<String>,
}

impl SimpleCommand {
    fn matches(&self, rule: &SimpleCommand) -> bool {
        let same_name = self.name == rule.name
            || self
                .name
                .strip_prefix(rule.name.as_str())
                .is_some_and(|rest| rest.starts_with('.'));
        if !same_name || !rule.flags.is_subset(&self.flags) {
            return false;
        }
        if rule.name == "dd" {
            return self
                .args
                .iter()
                .filter_map(|a| a.strip_prefix("of="))
                .any(is_block_device);
        }
        rule.args.iter().all(|want| {
            self.args.iter().any(|got| {
                got == want || (want == "/" && is_root_like(got)) || (want == "~" && is_home(got))
            })
        })
    }

    /// `sh -c 'script'` / `eval …` 里的脚本，展开一层再判。
    fn nested_script(&self) -> Option<String> {
        if self.name == "eval" {
            return Some(self.args.join(" "));
        }
        if matches!(self.name.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh")
            && self.flags.contains(&'c')
        {
            return self.args.first().cloned();
        }
        None
    }
}

fn is_home(p: &str) -> bool {
    let p = p.trim_end_matches("/*").trim_end_matches('/');
    matches!(p, "~" | "$HOME" | "${HOME}")
}

fn is_root_like(p: &str) -> bool {
    let trimmed = p.trim_end_matches("/*");
    trimmed.chars().all(|c| c == '/') || is_home(p)
}

/// 整盘设备：`/dev/sda`、`/dev/nvme0n1`、`/dev/disk2`、`/dev/mmcblk0` 等（含分区）。
fn is_block_device(p: &str) -> bool {
    let Some(dev) = p.strip_prefix("/dev/") else {
        return false;
    };
    [
        "sd", "hd", "vd", "xvd", "nvme", "disk", "rdisk", "mmcblk", "md", "dm-", "loop",
    ]
    .iter()
    .any(|pre| dev.starts_with(pre))
}

/// 从一组词里取简单命令：跳过 `VAR=x` 赋值和 `sudo`/`env`/`command`/`exec`/`nice`/`time`/
/// `nohup` 包装（及其选项），命令名去掉路径。
fn simple_command(words: &[String]) -> Option<SimpleCommand> {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let is_assign = w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        let base = w.rsplit('/').next().unwrap_or(w);
        if is_assign
            || matches!(
                base,
                "sudo" | "env" | "command" | "exec" | "nice" | "time" | "nohup"
            )
        {
            i += 1;
            // 包装命令自己的选项（`sudo -u root`、`nice -n 5`）不当成目标命令
            while i < words.len() && words[i].starts_with('-') {
                i += 1;
            }
            continue;
        }
        break;
    }
    let name = words.get(i)?;
    let name = name.rsplit('/').next().unwrap_or(name).to_string();
    let mut flags = std::collections::BTreeSet::new();
    let mut args = Vec::new();
    let mut end_of_opts = false;
    for w in &words[i + 1..] {
        if end_of_opts || w == "-" || !w.starts_with('-') {
            args.push(w.clone());
        } else if w == "--" {
            end_of_opts = true;
        } else if let Some(long) = w.strip_prefix("--") {
            match (name.as_str(), long) {
                ("rm", "recursive") => {
                    flags.insert('r');
                }
                ("rm", "force") => {
                    flags.insert('f');
                }
                _ => args.push(w.clone()),
            }
        } else {
            for c in w[1..].chars() {
                // rm 的 -R 与 -r 同义
                flags.insert(if name == "rm" && c == 'R' { 'r' } else { c });
            }
        }
    }
    Some(SimpleCommand { name, flags, args })
}

/// 极简 shell 分词：处理单/双引号与反斜杠转义，按未引用的 `;` `&` `|` 换行、括号、
/// 反引号切成多条简单命令。未闭合的引号吞到行尾。
fn shell_words(s: &str) -> Vec<Vec<String>> {
    let mut cmds: Vec<Vec<String>> = vec![];
    let mut words: Vec<String> = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    let flush_word = |cur: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(cur));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                if !matches!(n, '"' | '\\' | '$' | '`') {
                                    cur.push('\\');
                                }
                                cur.push(n);
                            }
                        }
                        _ => cur.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        cur.push(n);
                    }
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' => {
                flush_word(&mut cur, &mut in_word, &mut words);
                if !words.is_empty() {
                    cmds.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => flush_word(&mut cur, &mut in_word, &mut words),
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    flush_word(&mut cur, &mut in_word, &mut words);
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds
}

/// 三态审批答案（对标 pi-mcp-adapter 的 Allow once / Allow for session / Deny）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    Once,
    Session,
    Deny,
}

impl ApprovalAnswer {
    /// y/yes/once → 本次放行；a/all/session/always → 本会话记住；其余（含空行）→ 拒绝。
    pub fn parse(line: &str) -> Self {
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" | "once" => ApprovalAnswer::Once,
            "a" | "all" | "session" | "always" => ApprovalAnswer::Session,
            _ => ApprovalAnswer::Deny,
        }
    }
}

/// 会话级审批记忆：用户选过 "for session" 的（工具 + 规则原因），本会话内不再打扰。
/// key 带 reason：如 bash_block 的不同命中各自记忆，避免一次放行污染整类工具。
#[derive(Debug, Default)]
pub struct SessionApprovalCache {
    approved: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl SessionApprovalCache {
    fn key(tool: &str, reason: &str) -> String {
        format!("{tool}\0{reason}")
    }

    pub fn is_approved(&self, tool: &str, reason: &str) -> bool {
        self.approved
            .lock()
            .unwrap()
            .contains(&Self::key(tool, reason))
    }

    pub fn approve_session(&self, tool: &str, reason: &str) {
        self.approved
            .lock()
            .unwrap()
            .insert(Self::key(tool, reason));
    }

    /// 交互式审批器共用入口：先查记忆，未命中则调 `ask` 问用户并按三态处理。
    pub fn decide_with(
        &self,
        tool: &str,
        reason: &str,
        ask: impl FnOnce() -> ApprovalAnswer,
    ) -> bool {
        if self.is_approved(tool, reason) {
            return true;
        }
        match ask() {
            ApprovalAnswer::Deny => false,
            ApprovalAnswer::Once => true,
            ApprovalAnswer::Session => {
                self.approve_session(tool, reason);
                true
            }
        }
    }
}

/// 组合策略：依次裁决，首个非 Allow 生效。
pub struct ChainPolicy(pub Vec<Box<dyn Policy>>);
impl Policy for ChainPolicy {
    fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision {
        for p in &self.0 {
            match p.decide(tool, args) {
                Decision::Allow => continue,
                other => return other,
            }
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_allows_read_denies_mutations() {
        let p = RulePolicy::plan_mode();
        assert_eq!(p.decide("read", &serde_json::json!({})), Decision::Allow);
        assert!(matches!(
            p.decide("write", &serde_json::json!({})),
            Decision::Deny(_)
        ));
        assert!(matches!(
            p.decide("bash", &serde_json::json!({})),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn bash_blocklist_asks() {
        let p = RulePolicy {
            bash_block: vec!["rm -rf".into(), "mkfs".into()],
            ..Default::default()
        };
        assert_eq!(
            p.decide("bash", &serde_json::json!({"command": "ls"})),
            Decision::Allow
        );
        assert!(matches!(
            p.decide("bash", &serde_json::json!({"command": "rm -rf /tmp/x"})),
            Decision::Ask(_)
        ));
    }

    /// 与 CLI 默认名单一致：`rm -rf /`、`mkfs`、`dd if=`。
    fn default_rules() -> RulePolicy {
        RulePolicy {
            bash_block: vec!["rm -rf /".into(), "mkfs".into(), "dd if=".into()],
            ..Default::default()
        }
    }

    #[test]
    fn bash_blocklist_does_not_flag_safe_commands() {
        let p = default_rules();
        for cmd in [
            "rm -rf /tmp/rupi-e2e/does-not-exist-dir && echo removed",
            "rm -rf ./build",
            "rm -f /tmp/a.txt",
            "dd if=/dev/zero of=/dev/null count=1",
            "dd if=/dev/zero of=./disk.img bs=1M count=4",
            "echo 'rm -rf /' > notes.txt",
            "grep -r mkfs docs/",
            "ls /",
        ] {
            assert_eq!(
                p.decide("bash", &serde_json::json!({ "command": cmd })),
                Decision::Allow,
                "{cmd}"
            );
        }
    }

    #[test]
    fn bash_blocklist_catches_reordered_and_wrapped_forms() {
        let p = default_rules();
        for cmd in [
            "rm -rf /",
            "rm -fr /",
            "rm -r -f /",
            "rm -rf  /",
            "rm --recursive --force /",
            "rm -Rf /*",
            "rm -rf ~",
            "rm -rf $HOME",
            "sudo rm -rf /",
            "/bin/rm -rf /",
            "cd /tmp; rm -rf /",
            "true && rm -fr / || echo x",
            "sh -c 'rm -rf /'",
            "bash -lc \"rm -fr /*\"",
            "mkfs.ext4 /dev/sdb1",
            "dd if=/dev/zero of=/dev/sda bs=1M",
            "dd of=/dev/nvme0n1 if=image.raw",
        ] {
            assert!(
                matches!(
                    p.decide("bash", &serde_json::json!({ "command": cmd })),
                    Decision::Ask(_)
                ),
                "{cmd}"
            );
        }
    }

    #[test]
    fn readonly_policy_uses_plan_mode_allowlist() {
        let p = RulePolicy::plan_mode();
        for tool in ["read", "grep", "memory_search", "load_skill"] {
            assert_eq!(
                p.decide(tool, &serde_json::json!({})),
                Decision::Allow,
                "{tool}"
            );
        }
        for tool in [
            "memory",
            "mcp_fs__write_file",
            "ext_upper",
            "find_and_replace",
        ] {
            assert!(
                matches!(p.decide(tool, &serde_json::json!({})), Decision::Deny(_)),
                "{tool}"
            );
        }
    }

    #[test]
    fn approval_answer_parse() {
        assert_eq!(ApprovalAnswer::parse("y"), ApprovalAnswer::Once);
        assert_eq!(ApprovalAnswer::parse("YES"), ApprovalAnswer::Once);
        assert_eq!(ApprovalAnswer::parse("a"), ApprovalAnswer::Session);
        assert_eq!(ApprovalAnswer::parse("all"), ApprovalAnswer::Session);
        assert_eq!(ApprovalAnswer::parse(""), ApprovalAnswer::Deny);
        assert_eq!(ApprovalAnswer::parse("n"), ApprovalAnswer::Deny);
    }

    #[test]
    fn session_cache_remembers_per_tool_and_reason() {
        let c = SessionApprovalCache::default();
        assert!(c.decide_with("bash", "r1", || ApprovalAnswer::Session));
        // 同工具同原因不再问
        assert!(c.decide_with("bash", "r1", || panic!("should not ask again")));
        // 同工具不同原因仍要问
        assert!(!c.decide_with("bash", "r2", || ApprovalAnswer::Deny));
        // Once 不记忆
        assert!(c.decide_with("write", "r", || ApprovalAnswer::Once));
        assert!(!c.is_approved("write", "r"));
    }

    #[test]
    fn chain_first_non_allow_wins() {
        let c = ChainPolicy(vec![
            Box::new(AllowAll),
            Box::new(RulePolicy {
                deny_tools: vec!["bash".into()],
                ..Default::default()
            }),
        ]);
        assert!(matches!(
            c.decide("bash", &serde_json::json!({})),
            Decision::Deny(_)
        ));
        assert_eq!(c.decide("read", &serde_json::json!({})), Decision::Allow);
    }
}

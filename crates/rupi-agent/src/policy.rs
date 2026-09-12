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

/// 基于规则的策略（可组合叠加）：
/// - `readonly`: 拒绝 write/edit/bash（plan mode 内核）
/// - `deny_tools`: 按名拒绝
/// - `bash_block`: bash 命令含任一子串即转人工（无 Approver 则拒绝）
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

    fn is_mutating(tool: &str) -> bool {
        matches!(tool, "write" | "edit" | "bash")
    }
}

impl Policy for RulePolicy {
    fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision {
        if self.deny_tools.iter().any(|t| t == tool) {
            return Decision::Deny(format!("tool {tool} is denied by policy"));
        }
        if self.readonly && Self::is_mutating(tool) {
            return Decision::Deny(format!(
                "plan mode: {tool} is disabled; describe the plan instead"
            ));
        }
        if self.ask_tools.iter().any(|t| t == tool) {
            return Decision::Ask(format!("tool {tool} requires approval"));
        }
        if tool == "bash" {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(hit) = self.bash_block.iter().find(|b| cmd.contains(b.as_str())) {
                return Decision::Ask(format!("bash command matched blocklist: {hit}"));
            }
        }
        Decision::Allow
    }
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

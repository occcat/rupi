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

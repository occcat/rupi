//! 后台 review：对标 Hermes `background_review`。
//! 主循环结束后 fork 一次安静复盘：从本轮 transcript 提炼可持久化的记忆与 Skill 草稿。
//! 失败隔离：review 超时/失败只记 debug，绝不影响主流程；是否落盘由调用方（CLI `--review-apply`）决定。

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 一轮对话的复盘输入：用户原文 + 助手终答 + 本轮实际执行的工具序列。
#[derive(Debug, Clone, Default)]
pub struct TurnTranscript {
    pub user: String,
    pub assistant: String,
    pub tool_names: Vec<String>,
}

/// 建议写入长期记忆的一条操作（当前只提炼 `add`，replace/remove 留给 agent 显式调用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryOpSuggestion {
    pub entry: String,
}

/// 建议沉淀为新 Skill 的草稿（调用方用 `SkillAccumulator::propose` 落盘）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDraftSuggestion {
    pub name: String,
    pub description: String,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewSuggestion {
    pub memory_ops: Vec<MemoryOpSuggestion>,
    /// 本轮学到的失败教训（用户纠正 / 助手自认失败），调用方写 failures.md。
    #[serde(default)]
    pub failures: Vec<String>,
    pub skill_draft: Option<SkillDraftSuggestion>,
}

impl ReviewSuggestion {
    pub fn is_empty(&self) -> bool {
        self.memory_ops.is_empty() && self.failures.is_empty() && self.skill_draft.is_none()
    }
}

#[async_trait::async_trait]
pub trait Reviewer: Send + Sync {
    async fn review_turn(&self, t: &TurnTranscript) -> anyhow::Result<ReviewSuggestion>;
}

/// 启发式 reviewer（无 LLM 依赖，离线可用）：
///
/// - 记忆：用户说“记住/remember”，或自述偏好（“我是/我是…/我喜欢/my … is”）→ `add`。
/// - Skill：本轮动用 ≥2 个不同工具 → 把工具序列提炼为 skill 草稿。
pub struct HeuristicReviewer {
    pub max_entry_chars: usize,
}

impl Default for HeuristicReviewer {
    fn default() -> Self {
        Self {
            max_entry_chars: 300,
        }
    }
}

impl HeuristicReviewer {
    fn memory_entry(&self, user: &str) -> Option<String> {
        let t = user.trim();
        if t.is_empty() {
            return None;
        }
        let lower = t.to_lowercase();
        let wants_remember =
            t.contains("记住") || lower.contains("remember") || t.contains("请记住");
        // 自述型表达：中式“我是/我喜欢/我的”与英式“my … is / i like / i am”
        let self_statement = t.contains("我是")
            || t.contains("我喜欢")
            || t.contains("我的")
            || lower.contains("my ")
            || lower.contains("i like")
            || lower.contains("i am")
            || lower.contains("i prefer");
        if wants_remember || self_statement {
            let entry: String = t.chars().take(self.max_entry_chars).collect();
            Some(entry)
        } else {
            None
        }
    }

    /// 纠正检测（Hermes correction detection 对齐）：用户纠正立刻记一条失败，
    /// 助手自认失败也记一条。不确定时宁可不记，避免污染 failures.md。
    fn failure_entry(&self, t: &TurnTranscript) -> Option<String> {
        let user = t.user.trim();
        if user.is_empty() {
            return None;
        }
        let lower = user.to_lowercase();
        let correction = user.contains("不对")
            || user.contains("不是这样")
            || user.contains("你错了")
            || user.contains("你搞错了")
            || user.contains("错了")
            || user.contains("重来")
            || lower.contains("that's wrong")
            || lower.contains("you're wrong")
            || lower.contains("you misunderstood")
            || lower.contains("not what i asked")
            || lower.contains("incorrect");
        if correction {
            let entry: String = user.chars().take(self.max_entry_chars).collect();
            return Some(format!("user correction: {entry}"));
        }
        let a = t.assistant.to_lowercase();
        let admitted = t.assistant.contains("失败了")
            || t.assistant.contains("出错了")
            || t.assistant.contains("没能")
            || a.contains("failed to")
            || a.contains("couldn't complete")
            || a.contains("error occurred");
        if admitted {
            let entry: String = t.assistant.chars().take(self.max_entry_chars).collect();
            return Some(format!("assistant reported failure: {entry}"));
        }
        None
    }

    fn skill_draft(&self, t: &TurnTranscript) -> Option<SkillDraftSuggestion> {
        let mut distinct = vec![];
        for n in &t.tool_names {
            if !distinct.contains(n) {
                distinct.push(n.clone());
            }
        }
        if distinct.len() < 2 {
            return None;
        }
        let slug: String = t
            .user
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() > 2)
            .take(4)
            .collect::<Vec<_>>()
            .join("-");
        if slug.is_empty() {
            return None;
        }
        let name = format!("auto-{}", &slug[..slug.len().min(50)]);
        Some(SkillDraftSuggestion {
            description: format!(
                "distilled from: {}",
                t.user.chars().take(100).collect::<String>()
            ),
            steps: distinct
                .iter()
                .map(|d| format!("use tool `{d}` as done in the session"))
                .collect(),
            name,
        })
    }
}

#[async_trait::async_trait]
impl Reviewer for HeuristicReviewer {
    async fn review_turn(&self, t: &TurnTranscript) -> anyhow::Result<ReviewSuggestion> {
        Ok(ReviewSuggestion {
            memory_ops: self
                .memory_entry(&t.user)
                .map(|entry| MemoryOpSuggestion { entry })
                .into_iter()
                .collect(),
            failures: self.failure_entry(t).into_iter().collect(),
            skill_draft: self.skill_draft(t),
        })
    }
}

/// 带超时执行 review，超时/失败返回空建议（调用方无需处理 error）。
pub async fn review_with_timeout(
    reviewer: &Arc<dyn Reviewer>,
    t: &TurnTranscript,
    timeout: std::time::Duration,
) -> ReviewSuggestion {
    match tokio::time::timeout(timeout, reviewer.review_turn(t)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("background review failed: {e:#}");
            ReviewSuggestion::default()
        }
        Err(_) => {
            tracing::debug!("background review timed out");
            ReviewSuggestion::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_request_yields_memory_op() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "请记住我的编辑器是 vim".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(s.memory_ops.len(), 1);
        assert!(s.memory_ops[0].entry.contains("vim"));
        assert!(s.skill_draft.is_none());
    }

    #[tokio::test]
    async fn english_self_statement_yields_memory_op() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "remember: my shell is fish".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(s.memory_ops.len(), 1);
    }

    #[tokio::test]
    async fn correction_yields_failure_entry() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "不对，你搞错了目录".into(),
                assistant: "ok".into(),
                tool_names: vec![],
            })
            .await
            .unwrap();
        assert_eq!(s.failures.len(), 1);
        assert!(s.failures[0].contains("correction"));
    }

    #[tokio::test]
    async fn admitted_failure_yields_failure_entry() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "deploy it".into(),
                assistant: "I failed to connect to the host".into(),
                tool_names: vec![],
            })
            .await
            .unwrap();
        assert_eq!(s.failures.len(), 1);
        assert!(s.failures[0].contains("failure"));
    }

    #[tokio::test]
    async fn plain_question_yields_nothing() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "what time is it?".into(),
                assistant: "no idea".into(),
                tool_names: vec![],
            })
            .await
            .unwrap();
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn multi_tool_turn_yields_skill_draft_with_valid_name() {
        let r = HeuristicReviewer::default();
        let s = r
            .review_turn(&TurnTranscript {
                user: "read config and search logs".into(),
                assistant: "done".into(),
                tool_names: vec!["read".into(), "bash".into(), "read".into()],
            })
            .await
            .unwrap();
        let d = s.skill_draft.expect("draft");
        assert!(d.name.starts_with("auto-"));
        assert!(d.name.len() <= 64);
        assert!(d.steps.len() >= 2);
    }
}

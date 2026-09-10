//! Tool-call hooks：对标上游 Pi `beforeToolCall` / `afterToolCall`。
//!
//! 位置：before 在权限门之前（可改写参数，改写后仍过策略门；可提前拒绝），
//! after 在执行之后（含拒绝路径，可观察/改写结果）。拒绝一律转 tool error
//! 回模型，主循环不中断，与 policy / plan-mode 门同语义。

use rupi_tools::ToolOutput;
use std::collections::HashSet;
use std::sync::Mutex;

/// before 钩子裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// 放行；`Some` = 改写后的全量参数（后续钩子与策略门看到新值）。
    Proceed { args: Option<serde_json::Value> },
    /// 拒绝执行，转 tool error 回模型。
    Deny { reason: String },
}

impl HookDecision {
    pub fn allow() -> Self {
        Self::Proceed { args: None }
    }

    pub fn rewrite(args: serde_json::Value) -> Self {
        Self::Proceed { args: Some(args) }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }
}

/// 工具调用钩子：before 可拦截/改写，after 可观察/改写结果。
#[async_trait::async_trait]
pub trait ToolHook: Send + Sync {
    async fn before(&self, _name: &str, _args: &serde_json::Value) -> HookDecision {
        HookDecision::allow()
    }

    async fn after(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        output: ToolOutput,
    ) -> ToolOutput {
        output
    }
}

/// 按名拒绝工具（before）。例：演示环境禁 bash，禁子代理递归。
pub struct DenyToolsHook {
    denied: HashSet<String>,
    reason: Option<String>,
}

impl DenyToolsHook {
    pub fn new(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            denied: tools.into_iter().map(|t| t.into()).collect(),
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[async_trait::async_trait]
impl ToolHook for DenyToolsHook {
    async fn before(&self, name: &str, _args: &serde_json::Value) -> HookDecision {
        if self.denied.contains(name) {
            HookDecision::deny(
                self.reason
                    .clone()
                    .unwrap_or_else(|| format!("tool {name} is disabled by hook")),
            )
        } else {
            HookDecision::allow()
        }
    }
}

/// bash 命令重定向（before）：`command` 含 `from` 子串即替换为 `to`。
/// 对标把 `pip`/`python` 调用重定向到 `uv` 的用法；只改参数文本，不碰执行语义。
pub struct RedirectCommandHook {
    rules: Vec<(String, String)>,
}

impl RedirectCommandHook {
    pub fn new(rules: Vec<(impl Into<String>, impl Into<String>)>) -> Self {
        Self {
            rules: rules
                .into_iter()
                .map(|(f, t)| (f.into(), t.into()))
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl ToolHook for RedirectCommandHook {
    async fn before(&self, name: &str, args: &serde_json::Value) -> HookDecision {
        if name != "bash" {
            return HookDecision::allow();
        }
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let mut rewritten = cmd.to_owned();
        for (from, to) in &self.rules {
            if rewritten.contains(from) {
                rewritten = rewritten.replacen(from, to, 1);
            }
        }
        if rewritten == cmd {
            HookDecision::allow()
        } else {
            let mut args = args.clone();
            args["command"] = serde_json::Value::String(rewritten);
            HookDecision::rewrite(args)
        }
    }
}

/// 记录型钩子：记下每次调用的 (tool, args, 输出)，供复盘/测试断言。
#[derive(Default)]
pub struct RecordingHook {
    pub calls: Mutex<Vec<RecordedCall>>,
}

#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub name: String,
    pub args: serde_json::Value,
    pub content: String,
    pub is_error: bool,
}

#[async_trait::async_trait]
impl ToolHook for RecordingHook {
    async fn after(&self, name: &str, args: &serde_json::Value, output: ToolOutput) -> ToolOutput {
        self.calls.lock().unwrap().push(RecordedCall {
            name: name.to_owned(),
            args: args.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
        });
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_hook_blocks_listed_tool() {
        let h = DenyToolsHook::new(["bash"]);
        assert!(matches!(
            h.before("bash", &serde_json::json!({})).await,
            HookDecision::Deny { .. }
        ));
        assert!(matches!(
            h.before("read", &serde_json::json!({})).await,
            HookDecision::Proceed { args: None }
        ));
    }

    #[tokio::test]
    async fn redirect_hook_rewrites_bash_command() {
        let h = RedirectCommandHook::new(vec![("pip install", "uv pip install")]);
        // 非 bash 不动
        assert!(matches!(
            h.before("read", &serde_json::json!({"path": "pip install"}))
                .await,
            HookDecision::Proceed { args: None }
        ));
        // 未命中规则不动
        assert!(matches!(
            h.before("bash", &serde_json::json!({"command": "echo hi"}))
                .await,
            HookDecision::Proceed { args: None }
        ));
        // 命中改写
        match h
            .before(
                "bash",
                &serde_json::json!({"command": "pip install requests"}),
            )
            .await
        {
            HookDecision::Proceed { args: Some(a) } => {
                assert_eq!(a["command"], "uv pip install requests");
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recording_hook_captures_outcome() {
        let h = RecordingHook::default();
        let out = h
            .after("bash", &serde_json::json!({}), ToolOutput::ok("hi"))
            .await;
        assert_eq!(out.content, "hi");
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert!(!calls[0].is_error);
    }
}

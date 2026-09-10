//! 渐进式工具发现：工具 schema 按需可见，省上下文（MCP/扩展工具多时最有用）。
//!
//! 常驻工具每轮全量注入；可发现工具只在 `search_tools` 关键词命中后，
//! 才在后续轮次注入 schema。执行永远按名路由（注册表），发现只管
//! “模型看不看得到”，不管“能不能执行”——误伤时模型多搜一次即可恢复。

use rupi_core::ToolDefinition;
use std::collections::HashSet;

/// 内建七件套 + Skill 加载器：默认常驻（加载器是发现的入口，不能被发现门卡住）。
pub fn default_always() -> HashSet<String> {
    [
        "read",
        "write",
        "edit",
        "bash",
        "glob",
        "grep",
        "think",
        "load_skill",
        "read_resource",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// 常驻工具名（默认见 [`default_always`]）。
    pub always: HashSet<String>,
    /// search 单次返回上限。
    pub limit: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            always: default_always(),
            limit: 10,
        }
    }
}

/// `search_tools` 元工具定义（发现开启时才注入）。
pub fn search_tools_definition() -> ToolDefinition {
    ToolDefinition {
        name: "search_tools".into(),
        description:
            "Search tools not yet visible by keyword; matched tools become usable in later turns"
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "keywords, e.g. memory skill mcp"},
                "limit": {"type": "integer", "description": "max hits (default 10)"}
            },
            "required": ["query"]
        }),
        prompt_snippet: Some("search_tools(query): discover hidden tools by keyword".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredHit {
    pub name: String,
    pub description: String,
    pub score: usize,
}

fn terms(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(str::to_owned)
        .collect()
}

/// 关键词检索：query terms 在（name + description）token 中的命中数排序；
/// 常驻与 `search_tools` 自身不参选；0 命中返回空（调用方转“无匹配”提示）。
pub fn search_definitions(
    all: &[ToolDefinition],
    always: &HashSet<String>,
    query: &str,
    limit: usize,
) -> Vec<ScoredHit> {
    let qt = terms(query);
    if qt.is_empty() {
        return vec![];
    }
    let mut hits: Vec<ScoredHit> = vec![];
    for t in all {
        if t.name == "search_tools" || always.contains(&t.name) {
            continue;
        }
        let hay = terms(&format!("{} {}", t.name, t.description));
        let score = qt
            .iter()
            .filter(|q| hay.iter().any(|h| h.contains(*q)))
            .count();
        if score > 0 {
            hits.push(ScoredHit {
                name: t.name.clone(),
                description: t.description.clone(),
                score,
            });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.name.cmp(&b.name)));
    hits.truncate(limit.max(1));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: desc.into(),
            input_schema: serde_json::json!({}),
            prompt_snippet: None,
        }
    }

    fn sample() -> Vec<ToolDefinition> {
        vec![
            def("read", "read a file"),
            def("memory_search", "search learned long-term memories"),
            def("memory", "persist durable facts across sessions"),
            def("mcp__fetch", "fetch a url via mcp"),
        ]
    }

    #[test]
    fn ranks_relevant_first_and_skips_always_on() {
        let always = default_always();
        let hits = search_definitions(&sample(), &always, "memory learned", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "memory_search");
        assert!(hits.iter().all(|h| h.name != "read"));
    }

    #[test]
    fn empty_query_and_no_match_yield_empty() {
        let always = default_always();
        assert!(search_definitions(&sample(), &always, "", 10).is_empty());
        assert!(search_definitions(&sample(), &always, "zzz-nope", 10).is_empty());
    }

    #[test]
    fn limit_honored_and_search_tools_never_candidate() {
        let always = HashSet::new();
        let mut all = sample();
        all.push(search_tools_definition());
        let hits = search_definitions(&all, &always, "read memory fetch", 2);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.name != "search_tools"));
    }
}

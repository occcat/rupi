//! 模型目录（对标 pi-ai `models.json`）：内置一份 + `~/.rupi/models.json` / `RUPI_MODELS` 覆盖合并。

use serde::{Deserialize, Serialize};

const BUILTIN: &str = include_str!("../models.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub name: String,
    #[serde(default)]
    pub context: u64,
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

/// 内置目录（编译进二进制）。
pub fn builtin_models() -> Vec<ModelEntry> {
    serde_json::from_str::<CatalogFile>(BUILTIN)
        .map(|c| c.models)
        .unwrap_or_default()
}

/// 合并后的目录：内置在前，用户文件按 `(provider, id)` 覆盖或追加。
pub fn load_models() -> Vec<ModelEntry> {
    let mut models = builtin_models();
    for extra in user_catalogs() {
        for e in extra {
            if let Some(slot) = models
                .iter_mut()
                .find(|m| m.provider == e.provider && m.id == e.id)
            {
                *slot = e;
            } else {
                models.push(e);
            }
        }
    }
    models
}

fn user_catalogs() -> Vec<Vec<ModelEntry>> {
    let mut paths = Vec::new();
    if let Ok(p) = std::env::var("RUPI_MODELS") {
        paths.push(std::path::PathBuf::from(p));
    }
    if let Ok(home) = std::env::var("RUPI_HOME") {
        paths.push(std::path::PathBuf::from(home).join("models.json"));
    } else if let Ok(home) = std::env::var("HOME") {
        paths.push(std::path::PathBuf::from(home).join(".rupi/models.json"));
    }
    let mut out = Vec::new();
    for p in paths {
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(c) = serde_json::from_str::<CatalogFile>(&raw) {
            out.push(c.models);
        }
    }
    out
}

/// 人类可读列表（`--list-models`）。
pub fn format_catalog(models: &[ModelEntry]) -> String {
    let mut s = String::from("provider/model  (use --model provider/model[:thinking])\n");
    for m in models {
        let ctx = if m.context > 0 {
            format!("  ctx={}", m.context)
        } else {
            String::new()
        };
        s.push_str(&format!(
            "  {}/{}  — {}{ctx}\n",
            m.provider, m.id, m.name
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_core_providers() {
        let m = builtin_models();
        assert!(m.iter().any(|e| e.provider == "openai" && e.id == "gpt-4o-mini"));
        assert!(m.iter().any(|e| e.provider == "anthropic"));
        assert!(m.iter().any(|e| e.provider == "gemini"));
        assert!(m.iter().any(|e| e.provider == "openrouter"));
        assert!(m.iter().any(|e| e.provider == "azure"));
        assert!(m.iter().any(|e| e.provider == "bedrock"));
        assert!(m.iter().any(|e| e.provider == "vertex"));
        let text = format_catalog(&m);
        assert!(text.contains("openai/gpt-4o-mini"));
    }
}

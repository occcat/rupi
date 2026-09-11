//! 模型目录（对标 pi-ai `models.json`）：内置一份 + `~/.rupi/models.json` / `RUPI_MODELS` 覆盖合并。
//! 扩展可通过 [`register_extra_provider`] 按 OpenAI/Anthropic/Gemini 协议登记 `base_url` + 模型表。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const BUILTIN: &str = include_str!("../models.json");

fn extra_lock() -> &'static Mutex<Vec<ExtraProvider>> {
    static EXTRA: OnceLock<Mutex<Vec<ExtraProvider>>> = OnceLock::new();
    EXTRA.get_or_init(|| Mutex::new(Vec::new()))
}

/// 扩展登记的兼容协议 provider（并入 `models.json` 合并）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraProvider {
    pub name: String,
    /// `openai` | `anthropic` | `gemini`
    pub protocol: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

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

/// 合并后的目录：内置在前，用户文件 + 扩展登记按 `(provider, id)` 覆盖或追加。
pub fn load_models() -> Vec<ModelEntry> {
    let mut models = builtin_models();
    for extra in user_catalogs() {
        merge_models(&mut models, extra);
    }
    for p in extra_providers() {
        let tagged: Vec<ModelEntry> = p
            .models
            .into_iter()
            .map(|mut m| {
                if m.provider.is_empty() {
                    m.provider = p.name.clone();
                }
                m
            })
            .collect();
        merge_models(&mut models, tagged);
    }
    models
}

fn merge_models(models: &mut Vec<ModelEntry>, extra: Vec<ModelEntry>) {
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

pub fn extra_providers() -> Vec<ExtraProvider> {
    extra_lock().lock().unwrap().clone()
}

pub fn find_extra_provider(name: &str) -> Option<ExtraProvider> {
    extra_lock()
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .cloned()
}

/// 登记扩展 provider：校验协议，写入内存，并把模型并入用户 `models.json`。
pub fn register_extra_provider(p: ExtraProvider) -> anyhow::Result<ExtraProvider> {
    let proto = p.protocol.trim().to_ascii_lowercase();
    if !matches!(proto.as_str(), "openai" | "anthropic" | "gemini") {
        anyhow::bail!("registerProvider protocol must be openai|anthropic|gemini");
    }
    if p.name.trim().is_empty() || p.base_url.trim().is_empty() {
        anyhow::bail!("registerProvider requires name and base_url");
    }
    let mut p = p;
    p.protocol = proto;
    p.name = p.name.trim().to_ascii_lowercase();
    for m in &mut p.models {
        if m.provider.is_empty() {
            m.provider = p.name.clone();
        }
        if m.name.is_empty() {
            m.name = m.id.clone();
        }
    }
    {
        let mut g = extra_lock().lock().unwrap();
        if let Some(slot) = g.iter_mut().find(|x| x.name == p.name) {
            *slot = p.clone();
        } else {
            g.push(p.clone());
        }
    }
    merge_user_models(&p.models);
    persist_extra_providers(&providers_path());
    Ok(p)
}

pub fn load_extra_providers(path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(list) = serde_json::from_str::<Vec<ExtraProvider>>(&raw) else {
        return;
    };
    let mut g = extra_lock().lock().unwrap();
    for p in list {
        if let Some(slot) = g.iter_mut().find(|x| x.name == p.name) {
            *slot = p;
        } else {
            g.push(p);
        }
    }
}

pub fn persist_extra_providers(path: &Path) {
    let list = extra_providers();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, format!("{body}\n"));
    }
}

fn providers_path() -> PathBuf {
    if let Ok(home) = std::env::var("RUPI_HOME") {
        return PathBuf::from(home).join("providers.json");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".rupi/providers.json");
    }
    PathBuf::from("providers.json")
}

fn merge_user_models(entries: &[ModelEntry]) {
    if entries.is_empty() {
        return;
    }
    let path = if let Ok(home) = std::env::var("RUPI_HOME") {
        PathBuf::from(home).join("models.json")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".rupi/models.json")
    } else {
        return;
    };
    let mut models = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<CatalogFile>(&raw)
            .map(|c| c.models)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    merge_models(&mut models, entries.to_vec());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({"models": models});
    if let Ok(s) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(path, format!("{s}\n"));
    }
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
        s.push_str(&format!("  {}/{}  — {}{ctx}\n", m.provider, m.id, m.name));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_core_providers() {
        let m = builtin_models();
        assert!(m
            .iter()
            .any(|e| e.provider == "openai" && e.id == "gpt-4o-mini"));
        assert!(m.iter().any(|e| e.provider == "anthropic"));
        assert!(m.iter().any(|e| e.provider == "gemini"));
        assert!(m.iter().any(|e| e.provider == "openrouter"));
        assert!(m.iter().any(|e| e.provider == "azure"));
        assert!(m.iter().any(|e| e.provider == "bedrock"));
        assert!(m.iter().any(|e| e.provider == "vertex"));
        let text = format_catalog(&m);
        assert!(text.contains("openai/gpt-4o-mini"));
    }

    #[test]
    fn register_extra_provider_merges_models() {
        let home = std::env::temp_dir().join(format!("rupi-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let saved = std::env::var("RUPI_HOME").ok();
        unsafe { std::env::set_var("RUPI_HOME", home.as_os_str()) };
        extra_lock().lock().unwrap().clear();
        let p = register_extra_provider(ExtraProvider {
            name: "acme".into(),
            protocol: "openai".into(),
            base_url: "https://acme.example/v1".into(),
            api_key_env: Some("ACME_KEY".into()),
            models: vec![ModelEntry {
                id: "acme-1".into(),
                provider: String::new(),
                name: "Acme One".into(),
                context: 32_000,
            }],
        })
        .unwrap();
        assert_eq!(p.name, "acme");
        assert!(load_models()
            .iter()
            .any(|m| m.provider == "acme" && m.id == "acme-1"));
        extra_lock().lock().unwrap().clear();
        if let Some(v) = saved {
            unsafe { std::env::set_var("RUPI_HOME", v) };
        } else {
            unsafe { std::env::remove_var("RUPI_HOME") };
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}

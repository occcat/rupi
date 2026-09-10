use serde::{Deserialize, Serialize};

/// Approximate token count used by Pi compaction (`chars / 4`).
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil() as u32
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_read: u32,
    #[serde(default)]
    pub cache_write: u32,
    #[serde(default)]
    pub total_cost: u32,
}

impl Usage {
    pub fn total_tokens(self) -> u32 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

pub fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        total_cost: a.total_cost.saturating_add(b.total_cost),
    }
}

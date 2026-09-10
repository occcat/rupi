use regex::Regex;
use once_cell::sync::Lazy;

static INJECTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(ignore (all )?previous instructions|you are now|system prompt|exfiltrat|curl\s+[^ ]+\s*\|\s*(ba)?sh|id_rsa|BEGIN OPENSSH PRIVATE KEY)").unwrap()
});

static INVISIBLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\u200B-\u200F\u202A-\u202E\u2060-\u206F\uFEFF]").unwrap()
});

pub fn scan_memory_entry(content: &str) -> Result<(), String> {
    if INVISIBLE.is_match(content) {
        return Err("memory entry contains invisible Unicode and was blocked".into());
    }
    if INJECTION.is_match(content) {
        return Err("memory entry matched an injection/exfiltration pattern and was blocked".into());
    }
    Ok(())
}

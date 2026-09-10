use regex::Regex;
use std::sync::OnceLock;

/// Block prompt-injection / exfil patterns before they land in the system prompt.
pub fn scan_memory_entry(content: &str) -> Result<(), String> {
    if content.chars().any(|c| {
        let u = c as u32;
        (0x200B..=0x200F).contains(&u) || (0x202A..=0x202E).contains(&u) || u == 0xFEFF
    }) {
        return Err("memory entry contains invisible Unicode and was blocked".into());
    }
    let lower = content.to_ascii_lowercase();
    const BANNED: &[&str] = &[
        "ignore previous instructions",
        "ignore all previous",
        "you are now",
        "system prompt",
        "exfiltrat",
        "curl ",
        "wget ",
        "/etc/passwd",
        "id_rsa",
        "begin private key",
        "<script",
    ];
    for pat in BANNED {
        if lower.contains(pat) {
            return Err(format!(
                "memory entry blocked by security scan (matched `{pat}`)"
            ));
        }
    }
    static CREDS: OnceLock<Regex> = OnceLock::new();
    let creds = CREDS.get_or_init(|| {
        Regex::new(r"(?i)(api[_-]?key|secret|password|token)\s*[:=]\s*\S+").unwrap()
    });
    if creds.is_match(content) {
        return Err("memory entry looks like a credential dump and was blocked".into());
    }
    Ok(())
}

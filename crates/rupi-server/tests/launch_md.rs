//! `docs/LAUNCH.md` 必须跟真实 clap / 回环可写路径对齐。不需要 Postgres。

use std::path::PathBuf;

fn launch_md() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/LAUNCH.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn section<'a>(src: &'a str, heading: &str, next: &str) -> &'a str {
    let rest = src
        .split(heading)
        .nth(1)
        .unwrap_or_else(|| panic!("missing {heading}"));
    rest.split(next).next().unwrap_or(rest)
}

#[test]
fn launch_md_lists_real_flags_and_loopback_paths() {
    let src = launch_md();
    assert!(src.contains("--redis-cluster"), "missing --redis-cluster");
    assert!(
        src.contains("RUPI_SANDBOX_WARM"),
        "sandboxd --warm-pool env is RUPI_SANDBOX_WARM, not RUPI_EXEC_WARM"
    );
    assert!(
        src.contains("RUPI_CLOUD_MOCK"),
        "local AG-UI smoke needs RUPI_CLOUD_MOCK"
    );
    assert!(src.contains("./rupi-data/execd"), "{src}");
    assert!(src.contains("./rupi-data/sandboxd"), "{src}");

    let local = section(&src, "## 本机先跑通", "## 对外听");
    let recipe_cmds: String = local
        .split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, block)| block)
        .collect();
    assert!(
        !recipe_cmds.contains("/var/lib/rupi"),
        "loopback commands must not use /var/lib/rupi (not writable): {recipe_cmds}"
    );
    assert!(
        local.contains("cargo build --release"),
        "loopback recipe should build release bins"
    );
    assert!(
        local.contains("/admin/api/tenants"),
        "loopback recipe should seed a key via /admin"
    );
    assert!(local.contains("/v1/sessions"), "{local}");
    assert!(local.contains("/v1/agent"), "{local}");
    assert!(local.contains("RUPI_CLOUD_MOCK=1"), "{local}");
    assert!(
        local.contains("TEXT_MESSAGE"),
        "mock smoke must say to join SSE deltas, not grep the raw body"
    );
}

#[test]
fn launch_md_documents_ready_503() {
    let src = launch_md();
    assert!(
        src.contains("503"),
        "/ready must document HTTP 503 when postgres or executor is down"
    );
}

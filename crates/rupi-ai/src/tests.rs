use crate::{estimate_tokens, FauxProvider, FauxScript, Message, ModelCatalog};

#[test]
fn catalog_has_faux_and_claude() {
    let cat = ModelCatalog::builtin();
    assert!(cat.get("faux", "faux").is_some());
    assert!(cat.get("anthropic", "claude-sonnet-4-5").is_some());
    assert_eq!(cat.resolve("openai/gpt-4o").unwrap().provider, "openai");
}

#[test]
fn estimate_tokens_chars_div_4() {
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcdefgh"), 2);
}

#[test]
fn faux_scripts_produce_tool_calls() {
    let model = ModelCatalog::builtin().resolve("faux/faux").unwrap();
    let p = FauxProvider::new([FauxScript::tool(
        "read",
        serde_json::json!({"path": "README.md"}),
    )]);
    let msg = p.next_message(&model);
    assert_eq!(msg.tool_calls().len(), 1);
}

#[test]
fn message_json_roundtrip() {
    let m = Message::user("hi");
    let s = serde_json::to_string(&m).unwrap();
    let back: Message = serde_json::from_str(&s).unwrap();
    assert_eq!(back.role_name(), "user");
}

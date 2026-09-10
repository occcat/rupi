use std::fs;

use rupi_skills::{
    accumulate_from_transcript, format_skill_invocation, load_skills, parse_frontmatter, SkillManageTool,
};
use rupi_ai::Message;

#[test]
fn load_skill_md_and_root_md() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("python-debug");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: python-debug\ndescription: Debug python segfaults\n---\n\nUse faulthandler.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("quick.md"),
        "---\ndescription: A root skill file\n---\n\nHello.\n",
    )
    .unwrap();
    let (skills, diag) = load_skills(&[dir.path().to_path_buf()]);
    assert!(diag.is_empty(), "{diag:?}");
    assert!(skills.iter().any(|s| s.name == "python-debug"));
    let py = skills.iter().find(|s| s.name == "python-debug").unwrap();
    let inv = format_skill_invocation(py, Some("fix this"));
    assert!(inv.contains("<skill name=\"python-debug\""));
    assert!(inv.contains("fix this"));
}

#[test]
fn skill_manage_create_view_patch_delete() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillManageTool::new(dir.path().to_path_buf());
    tool.apply(&serde_json::json!({
        "action": "create",
        "name": "deploy-staging",
        "description": "Deploy the staging server",
        "content": "1. ssh\n2. systemctl restart"
    }))
    .unwrap();
    let viewed = tool
        .apply(&serde_json::json!({"action":"view","name":"deploy-staging"}))
        .unwrap();
    assert!(viewed.contains("systemctl"));
    tool.apply(&serde_json::json!({
        "action": "patch",
        "name": "deploy-staging",
        "old_text": "systemctl restart",
        "new_text": "systemctl reload"
    }))
    .unwrap();
    tool.apply(&serde_json::json!({"action":"delete","name":"deploy-staging"}))
        .unwrap();
}

#[test]
fn write_approval_stages() {
    let dir = tempfile::tempdir().unwrap();
    let mut tool = SkillManageTool::new(dir.path().to_path_buf());
    tool.approval.required = true;
    let r = tool
        .apply(&serde_json::json!({
            "action": "create",
            "name": "x",
            "description": "d",
            "content": "body"
        }))
        .unwrap();
    assert_eq!(r, "staged for approval");
    assert!(tool.pending.lock().unwrap().len() == 1);
}

#[test]
fn accumulate_remember_that() {
    let msgs = vec![
        Message::user("Remember that staging SSH uses port 2222"),
        Message::assistant_text("Noted."),
    ];
    let acc = accumulate_from_transcript(&msgs, None, None);
    assert!(!acc.memories.is_empty());
}

#[test]
fn frontmatter_without_closing_is_none() {
    let parsed = parse_frontmatter("---\nname: x\n");
    assert!(parsed.is_none());
}

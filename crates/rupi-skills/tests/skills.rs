use rupi_ai::{FauxProvider, Message, Model, ProviderKind, ScriptedTurn};
use rupi_memory::MemoryStore;
use rupi_skills::{
    load_skills, run_self_improvement_review, ReviewSettings, SkillLibrary, SkillManageTool,
    SkillOrigin, SkillViewTool,
};
use rupi_agent::{Tool, ToolContext};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn skill_view_then_manage_create() {
    let dir = tempdir().unwrap();
    let skills_dir = dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    let library = SkillLibrary::new(vec![]);
    let manage = SkillManageTool::new(library.clone(), skills_dir.clone());
    let ctx = ToolContext::new(dir.path());
    let created = manage
        .execute(
            json!({
                "action": "create",
                "name": "rust-testing",
                "description": "How to run Rust tests in this repo.",
                "content": "---\nname: rust-testing\ndescription: How to run Rust tests in this repo.\n---\n\n# Rust testing\n\nRun cargo test.\n"
            }),
            &ctx,
        )
        .await;
    assert!(!created.is_error, "{}", created.content);
    let (skills, _) = load_skills(&[skills_dir.clone()]);
    assert_eq!(skills[0].name, "rust-testing");
}

#[tokio::test]
async fn background_review_requires_skill_view_before_patch() {
    let dir = tempdir().unwrap();
    let skills_dir = dir.path().join("skills");
    let skill = skills_dir.join("rust-testing");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: rust-testing\ndescription: Run the Rust test suite.\n---\n\nRun cargo test.\n",
    )
    .unwrap();
    let (skills, _) = load_skills(&[skills_dir.clone()]);
    let library = SkillLibrary::new(skills);
    let manage = SkillManageTool::new(library.clone(), skills_dir.clone())
        .with_origin(SkillOrigin::BackgroundReview);
    let ctx = ToolContext::new(dir.path());
    let blocked = manage
        .execute(
            json!({
                "action": "patch",
                "name": "rust-testing",
                "old_string": "Run cargo test.",
                "new_string": "Run cargo test --workspace."
            }),
            &ctx,
        )
        .await;
    assert!(blocked.is_error, "expected read-before-write refusal");
    assert!(blocked.content.contains("skill_view"));

    let view = SkillViewTool::new(library);
    let loaded = view
        .execute(json!({"name": "rust-testing"}), &ctx)
        .await;
    assert!(!loaded.is_error, "{}", loaded.content);
    let patched = manage
        .execute(
            json!({
                "action": "patch",
                "name": "rust-testing",
                "old_string": "Run cargo test.",
                "new_string": "Run cargo test --workspace."
            }),
            &ctx,
        )
        .await;
    assert!(!patched.is_error, "{}", patched.content);
    let body = std::fs::read_to_string(skill.join("SKILL.md")).unwrap();
    assert!(body.contains("cargo test --workspace"));
}

#[tokio::test]
async fn self_improvement_review_creates_skill_with_faux() {
    let dir = tempdir().unwrap();
    let skills_dir = dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    let memory = Arc::new(MemoryStore::open(dir.path().join("memories")).unwrap());
    let provider = Arc::new(FauxProvider::new(vec![
        ScriptedTurn::tool(
            "skill_manage",
            json!({
                "action": "create",
                "name": "tab-indent",
                "description": "This project uses tabs for indentation.",
                "content": "---\nname: tab-indent\ndescription: This project uses tabs for indentation.\n---\n\nAlways indent with tabs.\n"
            }),
        ),
        ScriptedTurn::text("Saved skill tab-indent."),
    ]));
    let transcript = vec![
        Message::user_text("Please always indent with tabs in this repo."),
        Message::Assistant(rupi_ai::AssistantMessage::text_only("Understood.")),
    ];
    let outcome = run_self_improvement_review(
        &transcript,
        provider,
        Model::new("faux", ProviderKind::Faux),
        memory,
        vec![],
        skills_dir.clone(),
        dir.path().to_path_buf(),
        &ReviewSettings::default(),
    )
    .await;
    assert!(outcome.ran);
    assert!(outcome.tool_names.iter().any(|n| n == "skill_manage"));
    assert!(skills_dir.join("tab-indent/SKILL.md").exists());
}

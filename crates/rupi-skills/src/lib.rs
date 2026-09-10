//! Agent Skills with progressive disclosure and Hermes-style self-accumulation.

mod discover;
mod manage;
mod review;
mod view;

pub use discover::{format_skills_for_prompt, load_skills, Skill, SkillDiagnostic};
pub use manage::{SkillManageTool, SkillOrigin};
pub use review::{
    run_self_improvement_review, ReviewOutcome, ReviewSettings, SKILL_REVIEW_PROMPT, MEMORY_REVIEW_PROMPT,
};
pub use view::{SkillLibrary, SkillViewTool, SkillsListTool};

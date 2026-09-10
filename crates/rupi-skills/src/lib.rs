//! AgentSkills loader, skill_manage, and Hermes-style self-accumulation.

mod accumulate;
mod load;
mod manage;

pub use accumulate::{accumulate_from_transcript, Accumulation, Reviewer};
pub use load::{
    discover_skill_dirs, format_skill_invocation, load_skills, parse_frontmatter, skill_prompt_entries,
    Skill, SkillDiagnostic, MAX_DESCRIPTION_LENGTH, MAX_NAME_LENGTH,
};
pub use manage::{SkillManageTool, SkillWriteApproval};

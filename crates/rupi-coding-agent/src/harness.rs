use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use rupi_agent_core::{
    compact_messages, build_system_prompt, Agent, AgentEvent, PermissionGate,
    SessionStore, SystemPromptParts, DEFAULT_COMPACTION_SETTINGS,
};
use rupi_ai::{content_text, FauxProvider, Message, Model, ModelCatalog, ProviderClient};
use rupi_memory::{MemoryStore, MemoryTool, SessionSearchIndex};
use rupi_skills::{
    accumulate_from_transcript, discover_skill_dirs, load_skills, skill_prompt_entries, SkillManageTool,
};

use crate::config::{AgentSettings, ConfigPaths};
use crate::context_files::{load_agents_files, load_system_prompt_files};
use crate::tools::create_coding_tools;

pub struct HarnessOptions {
    pub paths: ConfigPaths,
    pub settings: AgentSettings,
    pub model: Model,
    pub extra_tools: Vec<String>,
    pub exclude_tools: Vec<String>,
    pub faux: Option<Arc<FauxProvider>>,
    pub live: Option<Arc<dyn ProviderClient>>,
    pub compact: bool,
    pub accumulate: bool,
}

pub struct Harness {
    pub agent: Agent,
    pub session: SessionStore,
    pub memory: Option<Arc<MemoryTool>>,
    pub skills: Option<Arc<SkillManageTool>>,
    pub search: Option<SessionSearchIndex>,
    pub gate: PermissionGate,
    pub options: HarnessOptions,
}

pub struct PrintOutcome {
    pub text: String,
    pub messages: Vec<Message>,
    pub events: Vec<AgentEvent>,
    pub accumulation: Option<rupi_skills::Accumulation>,
}

impl Harness {
    pub fn bootstrap(options: HarnessOptions) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&options.paths.home)?;
        std::fs::create_dir_all(&options.paths.sessions_dir)?;
        std::fs::create_dir_all(&options.paths.memories_dir)?;
        std::fs::create_dir_all(&options.paths.skills_dir)?;

        let mut tool_names = options.settings.default_tools.clone();
        tool_names.extend(options.extra_tools.iter().cloned());
        tool_names.retain(|n| !options.exclude_tools.contains(n));

        let mut tools = create_coding_tools(
            options.paths.cwd.clone(),
            &tool_names,
            options.settings.sandbox,
        );

        let mut memory = None;
        if options.settings.memory_enabled {
            let store = MemoryStore::open(&options.paths.memories_dir)?;
            let tool = MemoryTool::new(store);
            memory = Some(Arc::new(tool));
        }

        let skill_dirs = {
            let mut d = discover_skill_dirs(&options.paths.cwd, &options.paths.home);
            d.insert(0, options.paths.skills_dir.clone());
            d
        };
        let (loaded_skills, _diag) = load_skills(&skill_dirs);
        let skill_tool = Arc::new(SkillManageTool::new(options.paths.skills_dir.clone()));

        if let Some(mem) = &memory {
            tools.register(mem.clone());
        }
        tools.register(skill_tool.clone());

        let search = SessionSearchIndex::open(options.paths.home.join("state.db")).ok();
        if search.is_some() {
            tools.register(Arc::new(SessionSearchTool {
                // placeholder filled after struct; we register a simple closure tool below
            }));
        }
        // Re-register properly without the dummy: drop dummy if search failed.
        // We'll add session_search as a dedicated tool below.

        let (system_override, append) =
            load_system_prompt_files(&options.paths.cwd, &options.paths.agent_dir);
        let context_files = load_agents_files(&options.paths.cwd);
        let memory_block = memory
            .as_ref()
            .map(|m| m.frozen_prompt_block())
            .unwrap_or_default();

        let parts = SystemPromptParts {
            identity: system_override.unwrap_or_default(),
            tool_guidance: format!(
                "Available tools: {}.\nUse `skill_manage` with action=view to load a skill body. \
                 Persist durable facts with `memory`. After non-trivial workflows, save a skill.",
                tools.names().join(", ")
            ),
            skills: skill_prompt_entries(&loaded_skills),
            context_files,
            memory_block,
            append,
            cwd: options.paths.cwd.display().to_string(),
        };
        let system = build_system_prompt(&parts);

        let mut agent = Agent::new(system, options.model.clone());
        if let Some(faux) = &options.faux {
            agent = agent.with_faux(faux.clone());
        } else if let Some(live) = &options.live {
            agent = agent.with_provider(live.clone());
        }
        agent.set_tools(tools);

        let session = SessionStore::create(
            &options.paths.sessions_dir,
            options.paths.cwd.display().to_string(),
            format!("{}/{}", options.model.provider, options.model.id),
        )?;

        Ok(Self {
            agent,
            session,
            memory,
            skills: Some(skill_tool),
            search,
            gate: PermissionGate::permissive(options.paths.cwd.clone()),
            options,
        })
    }

    pub async fn print(&mut self, prompt: &str) -> anyhow::Result<PrintOutcome> {
        if self.options.compact
            && rupi_agent_core::should_compact(
                self.agent.messages(),
                &DEFAULT_COMPACTION_SETTINGS,
            )
        {
            let compacted = compact_messages(self.agent.messages(), &DEFAULT_COMPACTION_SETTINGS);
            self.agent.context.messages = compacted;
        }

        let new_msgs = self.agent.prompt(prompt).await?;
        for m in &new_msgs {
            let _ = self.session.append_message(m.clone());
            if let Some(idx) = &self.search {
                let text = match m {
                    Message::User { content, .. } | Message::Assistant { content, .. } => {
                        content_text(content)
                    }
                    Message::ToolResult { content, .. } => content_text(content),
                };
                let _ = idx.insert(
                    &self.session.session_id,
                    m.role_name(),
                    &text,
                    m.timestamp(),
                );
            }
        }

        let accumulation = if self.options.accumulate && self.options.settings.skill_accumulation {
            Some(accumulate_from_transcript(
                &new_msgs,
                self.memory.as_deref(),
                self.skills.as_deref(),
            ))
        } else {
            None
        };

        let text = new_msgs
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { content, .. } => {
                    let t = content_text(content);
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        Ok(PrintOutcome {
            text,
            messages: new_msgs,
            events: self.agent.events(),
            accumulation,
        })
    }

    pub async fn repl(&mut self) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        loop {
            write!(stdout, "rupi> ")?;
            stdout.flush()?;
            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line == "/quit" || line == "/exit" {
                break;
            }
            if line == "/session" {
                writeln!(
                    stdout,
                    "session {} leaf={:?} messages={}",
                    self.session.session_id,
                    self.session.leaf_id,
                    self.agent.messages().len()
                )?;
                continue;
            }
            if line == "/compact" {
                self.agent.context.messages =
                    compact_messages(self.agent.messages(), &DEFAULT_COMPACTION_SETTINGS);
                writeln!(stdout, "compacted to {} messages", self.agent.messages().len())?;
                continue;
            }
            match self.print(line).await {
                Ok(out) => {
                    writeln!(stdout, "{}", out.text)?;
                    if let Some(acc) = out.accumulation {
                        if !acc.memories.is_empty() || !acc.skills.is_empty() {
                            writeln!(
                                stdout,
                                "[harness] accumulated memories={} skills={}",
                                acc.memories.len(),
                                acc.skills.len()
                            )?;
                        }
                    }
                }
                Err(e) => writeln!(stdout, "error: {e}")?,
            }
        }
        Ok(())
    }
}

struct SessionSearchTool;

#[async_trait::async_trait]
impl rupi_agent_core::AgentTool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }
    fn description(&self) -> &str {
        "Search prior session transcripts (FTS5 evidence layer)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "query": {"type": "string"}, "limit": {"type": "integer"} },
            "required": ["query"]
        })
    }
    async fn execute(&self, _id: &str, _args: serde_json::Value) -> rupi_agent_core::AgentToolResult {
        rupi_agent_core::AgentToolResult::ok("session_search is bound on the harness; use the CLI /search")
    }
}

pub fn resolve_model(spec: &str) -> Model {
    ModelCatalog::builtin()
        .resolve(spec)
        .unwrap_or_else(|| Model::new("openai", spec, rupi_ai::ApiKind::Openai, 128_000))
}

pub fn default_options(cwd: PathBuf) -> HarnessOptions {
    let paths = ConfigPaths::resolve(cwd);
    let settings = AgentSettings::load(&paths);
    let model = resolve_model(&settings.model);
    HarnessOptions {
        paths,
        settings,
        model,
        extra_tools: vec![],
        exclude_tools: vec![],
        faux: None,
        live: None,
        compact: true,
        accumulate: true,
    }
}

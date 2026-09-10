use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rupi_agent_core::{
    build_system_prompt, compact_messages, Agent, AgentEvent, PermissionGate, SessionStore,
    SystemPromptParts, DEFAULT_COMPACTION_SETTINGS,
};
use rupi_ai::{content_text, FauxProvider, Message, Model, ModelCatalog, ProviderClient};
use rupi_memory::{MemoryStore, MemoryTool, SessionSearchIndex};
use rupi_skills::{
    accumulate_from_transcript, discover_skill_dirs, format_skill_invocation, load_skills,
    skill_prompt_entries, Skill, SkillManageTool,
};

use crate::config::{AgentSettings, ConfigPaths};
use crate::context_files::{load_agents_files, load_system_prompt_files};
use crate::mcp::connect_configured_mcp;
use crate::tools::{
    create_coding_tools, create_read_only_tools, SessionSearchTool, SubagentTool,
};

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
    pub loaded_skills: Vec<Skill>,
    pub search: Option<Arc<Mutex<SessionSearchIndex>>>,
    pub mcp_tool_names: Vec<String>,
    pub gate: PermissionGate,
    pub options: HarnessOptions,
}

pub struct PrintOutcome {
    pub text: String,
    pub messages: Vec<Message>,
    pub events: Vec<AgentEvent>,
    pub accumulation: Option<rupi_skills::Accumulation>,
}

#[derive(Debug)]
pub enum ReplOutcome {
    Quit,
    Printed(String),
}

impl Harness {
    pub async fn bootstrap(options: HarnessOptions) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&options.paths.home)?;
        std::fs::create_dir_all(&options.paths.sessions_dir)?;
        std::fs::create_dir_all(&options.paths.memories_dir)?;
        std::fs::create_dir_all(&options.paths.project_memories_dir)?;
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
            let store = MemoryStore::open_layered(
                &options.paths.memories_dir,
                &options.paths.project_memories_dir,
            )?;
            let mut tool = MemoryTool::new(store);
            tool.write_approval = options.settings.write_approval;
            memory = Some(Arc::new(tool));
        }

        let skill_dirs = {
            let mut d = discover_skill_dirs(&options.paths.cwd, &options.paths.home);
            d.insert(0, options.paths.skills_dir.clone());
            d
        };
        let (loaded_skills, _diag) = load_skills(&skill_dirs);
        let mut skill_tool = SkillManageTool::new(options.paths.skills_dir.clone());
        skill_tool.approval.required = options.settings.write_approval;
        let skill_tool = Arc::new(skill_tool);

        if let Some(mem) = &memory {
            tools.register(mem.clone());
        }
        tools.register(skill_tool.clone());

        let search = SessionSearchIndex::open(options.paths.home.join("state.db"))
            .ok()
            .map(|idx| Arc::new(Mutex::new(idx)));
        if let Some(idx) = &search {
            tools.register(Arc::new(SessionSearchTool {
                index: idx.clone(),
            }));
        }

        let mcp = connect_configured_mcp(&options.paths).await;
        for t in mcp.tools {
            tools.register(t);
        }

        let provider: Arc<dyn ProviderClient> = if let Some(f) = &options.faux {
            f.clone() as Arc<dyn ProviderClient>
        } else if let Some(l) = &options.live {
            l.clone()
        } else {
            Arc::new(FauxProvider::new([])) as Arc<dyn ProviderClient>
        };
        tools.register(Arc::new(SubagentTool {
            model: options.model.clone(),
            provider,
            tools: create_read_only_tools(options.paths.cwd.clone(), options.settings.sandbox),
        }));

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
                "Available tools: {}.\n\
                 Use `skill_manage` action=view (or /skill name) to load a skill body. \
                 Persist durable facts with `memory` (target=memory|user|project). \
                 Search past transcripts with `session_search`. \
                 Delegate read-only research to `subagent`. \
                 After non-trivial workflows, save a skill.",
                tools.names().join(", ")
            ),
            skills: skill_prompt_entries(&loaded_skills),
            context_files,
            memory_block,
            append,
            cwd: options.paths.cwd.display().to_string(),
        };
        let system = build_system_prompt(&parts);

        let gate = PermissionGate::sandboxed(options.paths.cwd.clone());
        let mut agent = Agent::new(system, options.model.clone()).with_gate(gate.clone());
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
            loaded_skills,
            search,
            mcp_tool_names: mcp.names,
            gate,
            options,
        })
    }

    pub async fn print(&mut self, prompt: &str) -> anyhow::Result<PrintOutcome> {
        if self.options.compact
            && rupi_agent_core::should_compact(self.agent.messages(), &DEFAULT_COMPACTION_SETTINGS)
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
                if let Ok(g) = idx.lock() {
                    let _ = g.insert(
                        &self.session.session_id,
                        m.role_name(),
                        &text,
                        m.timestamp(),
                    );
                }
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

    pub async fn handle_repl_line(&mut self, line: &str) -> anyhow::Result<ReplOutcome> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(ReplOutcome::Printed(String::new()));
        }
        if line == "/quit" || line == "/exit" {
            return Ok(ReplOutcome::Quit);
        }
        if line == "/help" {
            return Ok(ReplOutcome::Printed(
                "/session  session id + leaf + message count\n\
                 /tree     persisted JSONL entries\n\
                 /compact  extractive context compaction\n\
                 /memory   frozen USER/MEMORY/PROJECT snapshot\n\
                 /mcp      discovered MCP tools\n\
                 /search q FTS5 past transcripts\n\
                 /skill    list skills; /skill name [args] loads one (Pi /skill:name)\n\
                 /quit"
                    .into(),
            ));
        }
        if line == "/session" {
            return Ok(ReplOutcome::Printed(format!(
                "session {} leaf={:?} messages={}",
                self.session.session_id,
                self.session.leaf_id,
                self.agent.messages().len()
            )));
        }
        if line == "/tree" {
            let body = self
                .session
                .entries
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ReplOutcome::Printed(body));
        }
        if line == "/compact" {
            self.agent.context.messages =
                compact_messages(self.agent.messages(), &DEFAULT_COMPACTION_SETTINGS);
            return Ok(ReplOutcome::Printed(format!(
                "compacted to {} messages",
                self.agent.messages().len()
            )));
        }
        if line == "/memory" {
            let block = self
                .memory
                .as_ref()
                .map(|m| m.frozen_prompt_block())
                .unwrap_or_else(|| "(memory disabled)".into());
            return Ok(ReplOutcome::Printed(block));
        }
        if line == "/mcp" {
            if self.mcp_tool_names.is_empty() {
                return Ok(ReplOutcome::Printed(
                    "(no MCP tools; add mcp.json under ~/.rupi or .rupi/)".into(),
                ));
            }
            return Ok(ReplOutcome::Printed(self.mcp_tool_names.join("\n")));
        }
        if let Some(q) = line.strip_prefix("/search") {
            let q = q.trim();
            let out = match &self.search {
                Some(idx) if !q.is_empty() => match idx.lock() {
                    Ok(g) => match g.search(q, 8) {
                        Ok(hits) if hits.is_empty() => "no matches".into(),
                        Ok(hits) => hits
                            .iter()
                            .map(|h| format!("[{} {}] {}", h.session_id, h.role, h.text))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        Err(e) => format!("search error: {e}"),
                    },
                    Err(e) => e.to_string(),
                },
                _ => "usage: /search <query>".into(),
            };
            return Ok(ReplOutcome::Printed(out));
        }
        if line == "/skill" || line.starts_with("/skill ") || line.starts_with("/skill:") {
            let rest = line
                .trim_start_matches("/skill:")
                .trim_start_matches("/skill")
                .trim();
            if rest.is_empty() {
                if self.loaded_skills.is_empty() {
                    return Ok(ReplOutcome::Printed("(no skills discovered)".into()));
                }
                let body = self
                    .loaded_skills
                    .iter()
                    .map(|s| format!("{} — {}", s.name, s.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(ReplOutcome::Printed(body));
            }
            let (name, extra) = rest
                .split_once(' ')
                .map(|(n, e)| (n, Some(e)))
                .unwrap_or((rest, None));
            if let Some(skill) = self.loaded_skills.iter().find(|s| s.name == name) {
                let prompt = format_skill_invocation(skill, extra);
                match self.print(&prompt).await {
                    Ok(out) => return Ok(ReplOutcome::Printed(out.text)),
                    Err(e) => return Ok(ReplOutcome::Printed(format!("error: {e}"))),
                }
            }
            return Ok(ReplOutcome::Printed(format!("unknown skill `{name}`")));
        }
        match self.print(line).await {
            Ok(out) => {
                let mut body = out.text;
                if let Some(acc) = out.accumulation {
                    if !acc.memories.is_empty() || !acc.skills.is_empty() {
                        body.push_str(&format!(
                            "\n[harness] accumulated memories={} skills={}",
                            acc.memories.len(),
                            acc.skills.len()
                        ));
                    }
                }
                Ok(ReplOutcome::Printed(body))
            }
            Err(e) => Ok(ReplOutcome::Printed(format!("error: {e}"))),
        }
    }

    pub async fn repl(&mut self) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        writeln!(
            stdout,
            "commands: /help /session /compact /tree /memory /mcp /search <q> /skill [name] /quit"
        )?;
        loop {
            write!(stdout, "rupi> ")?;
            stdout.flush()?;
            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 {
                break;
            }
            match self.handle_repl_line(&line).await? {
                ReplOutcome::Quit => break,
                ReplOutcome::Printed(text) if text.is_empty() => {}
                ReplOutcome::Printed(text) => writeln!(stdout, "{text}")?,
            }
        }
        Ok(())
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

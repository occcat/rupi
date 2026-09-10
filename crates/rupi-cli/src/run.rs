use crate::args::{Args, Mode};
use crate::config::{
    load_settings, project_rupi_dir, seed_bundled_skills, AppPaths, Settings,
};
use anyhow::{bail, Context, Result};
use rupi_agent::{
    builtin_tools, build_system_prompt, load_context_files, run_agent_loop, AgentEvent,
    AgentLoopConfig, BuildSystemPromptOptions, CompactionSettings, SessionManager, SkillPromptEntry,
    ToolName, ToolRegistry, ToolSnippet,
};
use rupi_ai::{
    provider_for, content_text, Message, Model, Provider, ProviderKind, StreamOptions, ThinkingLevel,
};
use rupi_mcp::{load_mcp_config, mcp_tools_from_manager, McpManager};
use rupi_memory::{MemoryStore, MemoryTool, SessionIndex, SessionSearchTool};
use rupi_skills::{
    load_skills, run_self_improvement_review, ReviewSettings, SkillLibrary, SkillManageTool,
    SkillViewTool, SkillsListTool,
};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub async fn run(args: Args, paths: AppPaths) -> Result<()> {
    for d in &args.diagnostics {
        eprintln!("warning: {d}");
    }
    let cwd = std::env::current_dir()?;
    let settings = load_settings(&paths.settings);
    seed_bundled_skills(&paths.skills).ok();

    let kind = resolve_provider(&args, &settings)?;
    if args.list_models {
        print_models(kind);
        return Ok(());
    }
    let model_id = resolve_model(&args, &settings, kind);
    let api_key = args
        .api_key
        .clone()
        .or_else(|| std::env::var(kind.env_key()).ok())
        .or_else(|| std::env::var("RUPI_API_KEY").ok());
    let base_url = args
        .base_url
        .clone()
        .or_else(|| settings.base_url.clone())
        .or_else(|| std::env::var("RUPI_BASE_URL").ok());
    let provider = provider_for(kind, api_key, base_url);
    let mut model = Model::new(model_id, kind);
    model.supports_thinking = !matches!(
        args.thinking.unwrap_or(ThinkingLevel::Off),
        ThinkingLevel::Off
    );

    let mut tools = if args.no_tools {
        ToolRegistry::empty()
    } else if let Some(allow) = &args.tools {
        let mut reg = ToolRegistry::empty();
        for name in allow {
            if let Some(tn) = ToolName::parse(name) {
                reg.push(rupi_agent::create_tool(tn));
            } else {
                eprintln!("warning: unknown built-in tool `{name}`");
            }
        }
        reg
    } else {
        ToolRegistry::new(builtin_tools(&args.exclude_tools))
    };

    let memory_enabled = settings.memory.enabled;
    let memory = Arc::new(MemoryStore::open(&paths.memories)?);
    let index = Arc::new(SessionIndex::open(&paths.state_db)?);
    if memory_enabled {
        tools.push(Arc::new(MemoryTool::new(memory.clone())));
        tools.push(Arc::new(SessionSearchTool::new(index.clone())));
    }

    let mut skill_dirs = vec![paths.skills.clone(), PathBuf::from("~/.agents/skills")];
    let project_skills = cwd.join(".rupi/skills");
    let agents_skills = cwd.join(".agents/skills");
    skill_dirs.push(project_skills);
    skill_dirs.push(agents_skills);
    for extra in &args.skills {
        skill_dirs.push(PathBuf::from(extra));
    }
    let skill_dirs: Vec<PathBuf> = skill_dirs
        .into_iter()
        .map(|p| expand_home(p))
        .filter(|p| p.exists() || args.skills.iter().any(|s| expand_home(PathBuf::from(s)) == *p))
        .collect();
    let (skills, skill_diags) = if args.no_skills {
        (Vec::new(), Vec::new())
    } else {
        load_skills(&skill_dirs)
    };
    for d in &skill_diags {
        eprintln!("skill warning: {} ({})", d.message, d.path.display());
    }
    let library = SkillLibrary::new(skills.clone());
    if !args.no_skills {
        tools.push(Arc::new(SkillsListTool::new(library.clone())));
        tools.push(Arc::new(SkillViewTool::new(library.clone())));
        tools.push(Arc::new(
            SkillManageTool::new(library.clone(), paths.skills.clone())
                .with_write_approval(settings.skills.write_approval),
        ));
    }

    // MCP
    let mut mcp_file = load_mcp_config(&paths.mcp).unwrap_or_default();
    let project_mcp = project_rupi_dir(&cwd).join("mcp.json");
    if let Ok(proj) = load_mcp_config(&project_mcp) {
        for (k, v) in proj.mcp_servers {
            mcp_file.mcp_servers.insert(k, v);
        }
    }
    let mcp_manager = Arc::new(McpManager::connect_all(&mcp_file).await);
    if !mcp_file.mcp_servers.is_empty() {
        let mcp_tools = mcp_tools_from_manager(mcp_manager.clone(), settings.mcp.direct_tools).await;
        tools.extend(mcp_tools);
    }

    let context_files = if args.no_context_files {
        Vec::new()
    } else {
        load_context_files(&cwd, Some(&paths.home))
    };

    let selected: Vec<String> = tools.names();
    let snippets: Vec<ToolSnippet> = tools
        .snippets()
        .into_iter()
        .map(|(name, snippet)| ToolSnippet { name, snippet })
        .collect();
    let skill_entries: Vec<SkillPromptEntry> = skills
        .iter()
        .map(|s| SkillPromptEntry {
            name: s.name.clone(),
            description: s.description.clone(),
            file_path: s.file_path.display().to_string(),
            disable_model_invocation: s.disable_model_invocation,
        })
        .collect();

    let system = build_system_prompt(BuildSystemPromptOptions {
        custom_prompt: args.system_prompt.clone(),
        selected_tools: selected.clone(),
        tool_snippets: snippets,
        prompt_guidelines: tools.guidelines(),
        append_system_prompt: args.append_system_prompt.clone(),
        cwd: cwd.clone(),
        context_files: context_files
            .iter()
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect(),
        skills: skill_entries,
        memory_block: if memory_enabled {
            let block = memory.prompt_block();
            if block.is_empty() {
                None
            } else {
                Some(block)
            }
        } else {
            None
        },
        docs_hint: Some(
            "rupi is a Rust clone of Pi (earendil-works/pi 0.85.x) with Hermes-style memory and skill self-accumulation."
                .into(),
        ),
    });

    let mut loop_config = AgentLoopConfig::new(
        model.clone(),
        provider.clone(),
        tools.clone(),
        system.clone(),
        cwd.clone(),
    );
    loop_config.compaction = CompactionSettings {
        enabled: settings.compaction.enabled,
        reserve_tokens: settings.compaction.reserve_tokens,
        retain_tail: 6,
    };
    loop_config.stream_options = StreamOptions {
        thinking: args.thinking.unwrap_or(ThinkingLevel::Off),
        ..Default::default()
    };

    let sessions = SessionManager::new(&paths.sessions);
    let mut session = if args.no_session {
        None
    } else if let Some(p) = &args.session {
        Some(sessions.load(Path::new(p)).await?)
    } else if args.continue_session || args.resume {
        sessions.latest().await?
    } else {
        Some(sessions.create(&cwd, args.name.clone()).await?)
    };

    if let Some(s) = &session {
        index
            .upsert_session(
                &s.header.id,
                s.header.name.as_deref().unwrap_or("untitled"),
                &s.header.cwd,
                &s.header.timestamp,
            )
            .ok();
    }

    let history = session.as_ref().map(|s| s.messages()).unwrap_or_default();

    let prompt = args.messages.join(" ");
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let interactive = stdin_tty && stdout_tty && !args.print && args.mode != Some(Mode::Json);

    let review_settings = ReviewSettings {
        enabled: settings.skills.self_accumulate,
        min_tool_calls: 1,
        write_approval: settings.skills.write_approval,
    };

    if args.mode == Some(Mode::Rpc) {
        return run_rpc(loop_config, history).await;
    }

    if interactive {
        run_repl(
            &args,
            loop_config,
            sessions,
            session,
            history,
            mcp_manager,
            memory,
            provider,
            model,
            skills,
            paths.skills.clone(),
            cwd,
            review_settings,
            index,
        )
        .await
    } else {
        let json = args.mode == Some(Mode::Json);
        let user = if prompt.is_empty() {
            read_stdin_to_string()?
        } else {
            prompt
        };
        if user.trim().is_empty() {
            bail!("no prompt provided; pass one after -p or via stdin");
        }
        run_once(
            vec![Message::user_text(user)],
            history,
            &loop_config,
            session.as_mut(),
            &sessions,
            json,
            &review_settings,
            provider,
            model,
            memory,
            skills,
            paths.skills.clone(),
            cwd,
            index,
        )
        .await
    }
}

fn resolve_provider(args: &Args, settings: &Settings) -> Result<ProviderKind> {
    let name = args
        .provider
        .clone()
        .or_else(|| std::env::var("RUPI_PROVIDER").ok())
        .or_else(|| settings.provider.clone())
        .unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                "anthropic".into()
            } else if std::env::var("OPENAI_API_KEY").is_ok() {
                "openai".into()
            } else if std::env::var("GEMINI_API_KEY").is_ok() || std::env::var("GOOGLE_API_KEY").is_ok()
            {
                "google".into()
            } else if std::env::var("OPENROUTER_API_KEY").is_ok() {
                "openrouter".into()
            } else {
                "faux".into()
            }
        });
    ProviderKind::parse(&name).with_context(|| format!("unknown provider `{name}`"))
}

fn resolve_model(args: &Args, settings: &Settings, kind: ProviderKind) -> String {
    args.model
        .clone()
        .or_else(|| std::env::var("RUPI_MODEL").ok())
        .or_else(|| settings.model.clone())
        .unwrap_or_else(|| match kind {
            ProviderKind::Anthropic => "claude-sonnet-4-5".into(),
            ProviderKind::OpenAi => "gpt-4.1".into(),
            ProviderKind::Google => "gemini-2.5-pro".into(),
            ProviderKind::OpenRouter => "anthropic/claude-sonnet-4.5".into(),
            ProviderKind::OpenAiCompat => "llama3.1".into(),
            ProviderKind::Faux => "faux".into(),
        })
}

fn print_models(kind: ProviderKind) {
    let list: &[&str] = match kind {
        ProviderKind::Anthropic => &["claude-sonnet-4-5", "claude-opus-4-6", "claude-haiku-4-5"],
        ProviderKind::OpenAi => &["gpt-4.1", "gpt-4.1-mini", "o4-mini"],
        ProviderKind::Google => &["gemini-2.5-pro", "gemini-2.5-flash"],
        ProviderKind::OpenRouter => &["anthropic/claude-sonnet-4.5", "openai/gpt-4.1"],
        ProviderKind::OpenAiCompat => &["llama3.1", "qwen2.5-coder"],
        ProviderKind::Faux => &["faux"],
    };
    for m in list {
        println!("{kind}/{m}");
    }
}

async fn run_once(
    prompts: Vec<Message>,
    history: Vec<Message>,
    config: &AgentLoopConfig,
    session: Option<&mut rupi_agent::Session>,
    sessions: &SessionManager,
    json: bool,
    review: &ReviewSettings,
    provider: Arc<dyn Provider>,
    model: Model,
    memory: Arc<MemoryStore>,
    skills: Vec<rupi_skills::Skill>,
    skills_dir: PathBuf,
    cwd: PathBuf,
    index: Arc<SessionIndex>,
) -> Result<()> {
    let mut tool_count = 0usize;
    let session_id = session.as_ref().map(|s| s.header.id.clone());
    let new_msgs = run_agent_loop(prompts, history.clone(), config, |ev| {
        emit_event(&ev, json, &mut tool_count);
    })
    .await;

    if let Some(session) = session {
        for msg in &new_msgs {
            sessions.append(session, msg.clone()).await.ok();
            if let Some(sid) = &session_id {
                let (role, text) = match msg {
                    Message::User { content, .. } => ("user", content_text(content)),
                    Message::Assistant(a) => ("assistant", a.combined_text()),
                    Message::Tool {
                        tool_name, content, ..
                    } => (tool_name.as_str(), content_text(content)),
                    Message::System { content, .. } => ("system", content.clone()),
                };
                index
                    .index_message(sid, role, &text, &chrono::Utc::now().to_rfc3339())
                    .ok();
            }
        }
    }

    if review.enabled {
        let mut full = history;
        full.extend(new_msgs.clone());
        let outcome = run_self_improvement_review(
            &full,
            provider,
            model,
            memory,
            skills,
            skills_dir,
            cwd,
            review,
        )
        .await;
        if outcome.ran && !outcome.tool_names.is_empty() && !json {
            eprintln!("memory/skill review: {}", outcome.tool_names.join(", "));
        }
    }
    Ok(())
}

fn emit_event(ev: &AgentEvent, json: bool, tool_count: &mut usize) {
    if json {
        let v = match ev {
            AgentEvent::AgentStart => serde_json::json!({"type":"agent_start"}),
            AgentEvent::TurnStart => serde_json::json!({"type":"turn_start"}),
            AgentEvent::TextDelta { text } => {
                serde_json::json!({"type":"text_delta","text": text})
            }
            AgentEvent::ToolExecutionStart { name, args, .. } => {
                *tool_count += 1;
                serde_json::json!({"type":"tool_start","name": name, "args": args})
            }
            AgentEvent::ToolExecutionEnd {
                name,
                is_error,
                preview,
                ..
            } => serde_json::json!({"type":"tool_end","name": name, "is_error": is_error, "preview": preview}),
            AgentEvent::AgentEnd { .. } => serde_json::json!({"type":"agent_end"}),
            AgentEvent::Error { message } => serde_json::json!({"type":"error","message": message}),
            AgentEvent::Compaction { summary, .. } => {
                serde_json::json!({"type":"compaction","summary": summary})
            }
            _ => return,
        };
        println!("{}", v);
        return;
    }
    match ev {
        AgentEvent::TextDelta { text } => {
            print!("{text}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        AgentEvent::ToolExecutionStart { name, args, .. } => {
            *tool_count += 1;
            eprintln!("\n→ {name} {}", compact_json(args));
        }
        AgentEvent::ToolExecutionEnd {
            name,
            is_error,
            preview,
            ..
        } => {
            if *is_error {
                eprintln!("✗ {name}: {preview}");
            } else {
                eprintln!("✓ {name}");
            }
        }
        AgentEvent::AgentEnd { .. } => println!(),
        AgentEvent::Error { message } => eprintln!("error: {message}"),
        AgentEvent::Compaction { .. } => eprintln!("(context compacted)"),
        _ => {}
    }
}

fn compact_json(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() > 120 {
        format!("{}…", &s[..117])
    } else {
        s
    }
}

async fn run_repl(
    args: &Args,
    mut config: AgentLoopConfig,
    sessions: SessionManager,
    mut session: Option<rupi_agent::Session>,
    mut history: Vec<Message>,
    mcp: Arc<McpManager>,
    memory: Arc<MemoryStore>,
    provider: Arc<dyn Provider>,
    model: Model,
    skills: Vec<rupi_skills::Skill>,
    skills_dir: PathBuf,
    cwd: PathBuf,
    review: ReviewSettings,
    index: Arc<SessionIndex>,
) -> Result<()> {
    println!("rupi {} · {}/{}", env!("CARGO_PKG_VERSION"), model.provider, model.id);
    println!("type /help for commands, /quit to exit");
    if let Some(s) = &session {
        println!("session {}", s.header.id);
    }
    let mut rl = DefaultEditor::new()?;
    let mut first = args.messages.join(" ");
    loop {
        let line = if !first.is_empty() {
            let t = std::mem::take(&mut first);
            t
        } else {
            match rl.readline("› ") {
                Ok(l) => {
                    let _ = rl.add_history_entry(l.as_str());
                    l
                }
                Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            }
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            match handle_slash(
                cmd,
                &mut config,
                &mcp,
                &memory,
                &skills,
                session.as_ref(),
            )
            .await?
            {
                SlashResult::Continue => continue,
                SlashResult::Quit => break,
            }
        }
        let prompt = vec![Message::user_text(line)];
        run_once(
            prompt,
            history.clone(),
            &config,
            session.as_mut(),
            &sessions,
            false,
            &review,
            provider.clone(),
            model.clone(),
            memory.clone(),
            skills.clone(),
            skills_dir.clone(),
            cwd.clone(),
            index.clone(),
        )
        .await?;
        if let Some(s) = &session {
            history = s.messages();
        }
    }
    Ok(())
}

enum SlashResult {
    Continue,
    Quit,
}

async fn handle_slash(
    cmd: &str,
    config: &mut AgentLoopConfig,
    mcp: &Arc<McpManager>,
    memory: &Arc<MemoryStore>,
    skills: &[rupi_skills::Skill],
    session: Option<&rupi_agent::Session>,
) -> Result<SlashResult> {
    let mut parts = cmd.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match name {
        "help" => {
            println!(
                "/help            this text\n\
                 /model           show current model\n\
                 /mcp             MCP server status\n\
                 /skills          list skills\n\
                 /memory          show MEMORY.md / USER.md snapshot\n\
                 /session         session info\n\
                 /compact on|off  toggle compaction\n\
                 /quit            exit"
            );
        }
        "model" => println!("{}/{}", config.model.provider, config.model.id),
        "mcp" => println!("{}", mcp.status_text().await),
        "skills" => {
            if skills.is_empty() {
                println!("(no skills)");
            } else {
                for s in skills {
                    println!("- {} — {}", s.name, s.description);
                }
            }
        }
        "memory" => {
            let block = memory.prompt_block();
            if block.is_empty() {
                println!("(empty memory)");
            } else {
                println!("{block}");
            }
        }
        "session" => {
            if let Some(s) = session {
                println!("{}  {} entries", s.header.id, s.entries.len());
            } else {
                println!("(no session)");
            }
        }
        "compact" => {
            match rest {
                "off" => config.compaction.enabled = false,
                "on" => config.compaction.enabled = true,
                _ => {}
            }
            println!(
                "compaction {}",
                if config.compaction.enabled { "on" } else { "off" }
            );
        }
        "quit" | "exit" | "q" => return Ok(SlashResult::Quit),
        _ => println!("unknown command /{name} — try /help"),
    }
    Ok(SlashResult::Continue)
}

async fn run_rpc(config: AgentLoopConfig, mut history: Vec<Message>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line)?;
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            "prompt" => {
                let text = v
                    .pointer("/params/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let new_msgs = run_agent_loop(
                    vec![Message::user_text(text)],
                    history.clone(),
                    &config,
                    |_| {},
                )
                .await;
                history.extend(new_msgs.iter().cloned());
                let reply = new_msgs
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::Assistant(a) => Some(a.combined_text()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let resp = serde_json::json!({"jsonrpc":"2.0","id": id, "result": {"text": reply}});
                stdout
                    .write_all(format!("{resp}\n").as_bytes())
                    .await?;
                stdout.flush().await?;
            }
            "shutdown" => break,
            _ => {
                let resp = serde_json::json!({
                    "jsonrpc":"2.0","id": id,
                    "error": {"code": -32601, "message": format!("unknown method {method}")}
                });
                stdout
                    .write_all(format!("{resp}\n").as_bytes())
                    .await?;
            }
        }
    }
    Ok(())
}

fn expand_home(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p
}

fn read_stdin_to_string() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

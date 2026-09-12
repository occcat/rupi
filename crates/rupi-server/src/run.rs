//! 一轮 AG-UI：hydrate Postgres → AgentLoop → 落盘 → 官方 SSE。

use crate::agui::{
    self, cloud_state, last_user_from_input, tree_to_agui_messages, AguiEvent, EventMapper,
    RunAgentInput,
};
use crate::cache::Cache;
use crate::db::{self, PgPool, Tenant};
use crate::memory::PostgresMemory;
use crate::quota;
use crate::reclaim;
use crate::tools::cloud_tools;
use crate::App;
use rupi_agent::{
    AgentLoop, AskAction, RulePolicy,
};
use rupi_core::{CancelFlag, ContentBlock, Message, Role, SessionTree};
use rupi_llm::{LlmProvider, MockProvider, ThinkingLevel};
use rupi_memory::{FrozenMemory, MemoryManager, MemoryStore};
use rupi_runtime::WorkspaceHandle;
use rupi_skills::SkillRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum Preflight {
    Stream(mpsc::UnboundedReceiver<AguiEvent>, rupi_core::CancelFlag),
    Status { code: u16, body: serde_json::Value },
}

fn emit(tx: &mpsc::UnboundedSender<AguiEvent>, ev: AguiEvent) {
    let _ = tx.send(ev);
}

pub async fn start_run(app: App, tenant: Tenant, input: RunAgentInput) -> Preflight {
    if input.tools.iter().any(|_| true) {
        // 客户端 tools 忽略，不当事故。
    }
    if input
        .forwarded_props
        .as_ref()
        .is_some_and(|v| !v.is_null() && v != &serde_json::json!({}))
    {
        tracing::debug!("forwardedProps ignored (not an RPC tunnel)");
    }

    if app
        .cache
        .is_missing_session(&tenant.id, &input.thread_id)
        .await
    {
        return Preflight::Status {
            code: 404,
            body: serde_json::json!({"error": "session not found"}),
        };
    }
    let Some(sess) = (match db::get_session(&app.pool, &tenant.id, &input.thread_id).await {
        Ok(s) => s,
        Err(e) => {
            return Preflight::Status {
                code: 500,
                body: serde_json::json!({"error": e.to_string()}),
            }
        }
    }) else {
        return match db::session_owner(&app.pool, &input.thread_id).await {
            Ok(Some(other)) if other != tenant.id => Preflight::Status {
                code: 403,
                body: serde_json::json!({"error": "forbidden"}),
            },
            _ => {
                app.cache
                    .remember_missing_session(&tenant.id, &input.thread_id)
                    .await;
                Preflight::Status {
                    code: 404,
                    body: serde_json::json!({"error": "session not found"}),
                }
            }
        };
    };

    let pending = db::pending_interrupts(&app.pool, &tenant.id, &input.thread_id)
        .await
        .unwrap_or_default();
    if !pending.is_empty() && input.resume.is_empty() {
        return Preflight::Status {
            code: 409,
            body: serde_json::json!({"error": "pending interrupts require resume"}),
        };
    }
    if !pending.is_empty() {
        let ids: Vec<&str> = pending.iter().map(|p| p.id.as_str()).collect();
        for p in &pending {
            if !input.resume.iter().any(|r| r.interrupt_id == p.id) {
                return Preflight::Status {
                    code: 400,
                    body: serde_json::json!({"error": "resume must cover all interrupts", "missing": p.id}),
                };
            }
        }
        let _ = ids;
    }

    match quota::admit_run(&app.cache, &app.pool, &tenant).await {
        crate::quota::Admit::TooMany => {
            return Preflight::Status {
                code: 429,
                body: serde_json::json!({"error": "quota"}),
            }
        }
        crate::quota::Admit::Ok => {}
    }

    if !quota::acquire_lease(
        &app.cache,
        &app.pool,
        &tenant.id,
        &input.thread_id,
        &input.run_id,
        &app.instance_id,
    )
    .await
    {
        quota::release_run(&app.cache, &tenant.id).await;
        return Preflight::Status {
            code: 409,
            body: serde_json::json!({"error": "run already in progress"}),
        };
    }

    let (tx, rx) = mpsc::unbounded_channel::<AguiEvent>();
    let cancel = CancelFlag::new();
    let cancel_drive = cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = drive(
            app.clone(),
            tenant,
            sess,
            input,
            tx.clone(),
            cancel_drive,
        )
        .await
        {
            emit(&tx, agui::run_error(&e.to_string(), None));
        }
    });
    Preflight::Stream(rx, cancel)
}

async fn drive(
    app: App,
    tenant: Tenant,
    sess: db::SessionRow,
    input: RunAgentInput,
    tx: mpsc::UnboundedSender<AguiEvent>,
    cancel: CancelFlag,
) -> anyhow::Result<()> {
    emit(
        &tx,
        agui::run_started(
            &input.thread_id,
            &input.run_id,
            input.parent_run_id.as_deref(),
        ),
    );
    apply_state(&app.pool, &tenant.id, &input).await?;

    let mut tree = {
        let key = Cache::sess_tree_key(&tenant.id, &input.thread_id);
        let fill = Cache::fill_key("tree", &tenant.id, &input.thread_id);
        let local = app.cache.singleflight(&key).await;
        let _g = local.lock().await;
        if let Some(raw) = app.cache.get(&key).await {
            serde_json::from_str::<SessionTree>(&raw)
                .ok()
                .unwrap_or(db::load_tree(&app.pool, &tenant.id, &input.thread_id).await?)
        } else {
            let locked = app.cache.fill_lock(&fill, 5).await;
            if !locked {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                if let Some(raw) = app.cache.get(&key).await {
                    if let Ok(t) = serde_json::from_str::<SessionTree>(&raw) {
                        t
                    } else {
                        db::load_tree(&app.pool, &tenant.id, &input.thread_id).await?
                    }
                } else {
                    db::load_tree(&app.pool, &tenant.id, &input.thread_id).await?
                }
            } else {
                let t = db::load_tree(&app.pool, &tenant.id, &input.thread_id).await?;
                let _ = app
                    .cache
                    .set_ex(&key, &serde_json::to_string(&t).unwrap_or_default(), 300)
                    .await;
                app.cache.fill_unlock(&fill).await;
                t
            }
        }
    };
    let handle = reclaim::ensure_hot(&app, &tenant.id, &sess).await?;
    let _ = db::touch_session(&app.pool, &tenant.id, &input.thread_id).await;

    let hb_app = app.clone();
    let hb_tenant = tenant.id.clone();
    let hb_thread = input.thread_id.clone();
    let hb_run = input.run_id.clone();
    let hb_inst = app.instance_id.clone();
    let hb_cancel = cancel.clone();
    let hb = tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(quota::HEARTBEAT_SECS));
        loop {
            iv.tick().await;
            if hb_cancel.is_cancelled() {
                break;
            }
            if !quota::heartbeat(
                &hb_app.cache,
                &hb_app.pool,
                &hb_tenant,
                &hb_thread,
                &hb_run,
                &hb_inst,
            )
            .await
            {
                hb_cancel.cancel();
                break;
            }
        }
    });

    let pgmem = PostgresMemory::new(
        app.pool.clone(),
        tenant.id.clone(),
        input.thread_id.clone(),
        app.cache.clone(),
    );
    let frozen_key = Cache::mem_frozen_key(&tenant.id);
    let frozen = if let Some(raw) = app.cache.get(&frozen_key).await {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            FrozenMemory {
                memory: v.get("memory").and_then(|x| x.as_str()).unwrap_or("").into(),
                user: v.get("user").and_then(|x| x.as_str()).unwrap_or("").into(),
                failures: v.get("failures").and_then(|x| x.as_str()).unwrap_or("").into(),
            }
        } else {
            pgmem.frozen().await
        }
    } else {
        let f = pgmem.frozen().await;
        let blob = serde_json::json!({
            "memory": f.memory, "user": f.user, "failures": f.failures
        });
        let _ = app
            .cache
            .set_ex(&frozen_key, &blob.to_string(), 300)
            .await;
        f
    };

    let mut store = MemoryStore::new(std::env::temp_dir().join(format!("rupi-cloud-{}", tenant.id)));
    store.memory_enabled = false;
    store.user_profile_enabled = false;
    let mut mem = MemoryManager::new(store);
    let _ = mem.register_external("postgres".into(), Box::new(pgmem.clone()));

    let tools = cloud_tools(app.executor.clone(), handle.clone());
    let skills = SkillRegistry::default();
    let provider = app.provider_for(&tenant);
    let cancel = CancelFlag::new();

    let pending = db::pending_interrupts(&app.pool, &tenant.id, &input.thread_id).await?;
    let pending_ids: Vec<String> = pending.iter().map(|p| p.id.clone()).collect();
    emit(
        &tx,
        agui::messages_snapshot(tree_to_agui_messages(&tree)),
    );
    emit(
        &tx,
        agui::state_snapshot(cloud_state(
            &input.thread_id,
            sess.name.as_deref(),
            sess.model.as_deref(),
            sess.thinking_level.as_deref(),
            sess.auto_compaction,
            &pending_ids,
            sess.runtime_backend.as_deref(),
        )),
    );

    let mut mapper = EventMapper::new(input.thread_id.clone(), input.run_id.clone());
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<rupi_core::AgentEvent>();
    let map_tx = tx.clone();
    let map_task = tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            for a in mapper.map(&ev) {
                emit(&map_tx, a);
            }
        }
        for a in mapper.finish_open() {
            emit(&map_tx, a);
        }
    });

    let on_event = move |e: rupi_core::AgentEvent| {
        let _ = ev_tx.send(e);
    };

    let mut agent = AgentLoop::new(12)
        .with_policy(Arc::new(RulePolicy {
            ask_tools: vec!["write".into(), "edit".into(), "bash".into()],
            ..Default::default()
        }))
        .with_on_ask(|_, _, _| AskAction::Interrupt)
        .with_compaction_enabled(sess.auto_compaction);
    if let Some(t) = sess
        .thinking_level
        .as_deref()
        .and_then(|s| s.parse::<ThinkingLevel>().ok())
    {
        agent = agent.with_thinking(t);
    }

    let result = if !input.resume.is_empty() {
        resume_and_continue(
            &app,
            &tenant,
            &input,
            &mut tree,
            &handle,
            &tools,
            &mem,
            &frozen,
            &skills,
            &*provider,
            &agent,
            &on_event,
            &cancel,
        )
        .await
    } else {
        let user = last_user_from_input(&input)
            .ok_or_else(|| anyhow::anyhow!("RunAgentInput.messages must end with a user turn"))?;
        for ctx in &input.context {
            if !ctx.value.is_empty() {
                tree.push(Message::text(
                    Role::System,
                    format!("[context: {}]\n{}", ctx.description, ctx.value),
                ));
            }
        }
        agent
            .run_with_user(
                &*provider,
                &mut tree,
                user,
                &tools,
                &mem,
                &frozen,
                &skills,
                &[],
                &on_event,
                &cancel,
            )
            .await
    };

    drop(on_event);
    let _ = map_task.await;

    match result {
        Ok(_) => {}
        Err(e) => {
            emit(&tx, agui::run_error(&e.to_string(), None));
            finish(&app, &tenant, &input.thread_id, &input.run_id, &tree).await;
            hb.abort();
            return Ok(());
        }
    }

    if let Some(pend) = agent.take_pending_interrupt() {
        let iid = db::insert_interrupt(
            &app.pool,
            &tenant.id,
            &input.thread_id,
            &input.run_id,
            &pend.tool_call_id,
            &pend.name,
            &pend.arguments,
            &pend.reason,
        )
        .await?;
        db::persist_tree(&app.pool, &tenant.id, &input.thread_id, &tree).await?;
        app.cache
            .invalidate_session(&tenant.id, &input.thread_id)
            .await;
        let snap = db::load_tree(&app.pool, &tenant.id, &input.thread_id).await?;
        emit(
            &tx,
            agui::messages_snapshot(tree_to_agui_messages(&snap)),
        );
        emit(
            &tx,
            agui::state_snapshot(cloud_state(
                &input.thread_id,
                sess.name.as_deref(),
                sess.model.as_deref(),
                sess.thinking_level.as_deref(),
                sess.auto_compaction,
                std::slice::from_ref(&iid),
                sess.runtime_backend.as_deref(),
            )),
        );
        quota::release_lease(
            &app.cache,
            &app.pool,
            &tenant.id,
            &input.thread_id,
            &input.run_id,
            &app.instance_id,
        )
        .await;
        quota::release_run(&app.cache, &tenant.id).await;
        hb.abort();
        emit(
            &tx,
            agui::run_finished_interrupt(
                &input.thread_id,
                &input.run_id,
                vec![serde_json::json!({
                    "id": iid,
                    "reason": "tool_call",
                    "message": pend.reason,
                    "toolCallId": pend.tool_call_id,
                    "responseSchema": {
                        "type": "object",
                        "properties": {
                            "approved": {"type": "boolean"},
                            "editedArgs": {"type": "object"}
                        },
                        "required": ["approved"]
                    }
                })],
            ),
        );
    } else {
        finish(&app, &tenant, &input.thread_id, &input.run_id, &tree).await;
        hb.abort();
        emit(
            &tx,
            agui::run_finished_success(&input.thread_id, &input.run_id),
        );
    }
    Ok(())
}

async fn resume_and_continue(
    app: &App,
    tenant: &Tenant,
    input: &RunAgentInput,
    tree: &mut SessionTree,
    handle: &WorkspaceHandle,
    tools: &rupi_tools::ToolRegistry,
    mem: &MemoryManager,
    frozen: &FrozenMemory,
    skills: &SkillRegistry,
    provider: &dyn LlmProvider,
    agent: &AgentLoop,
    on_event: &(dyn Fn(rupi_core::AgentEvent) + Sync),
    cancel: &CancelFlag,
) -> anyhow::Result<rupi_core::StopReason> {
    let pending = db::pending_interrupts(&app.pool, &tenant.id, &input.thread_id).await?;
    let exec = app.executor.clone();
    for p in &pending {
        let entry = input
            .resume
            .iter()
            .find(|r| r.interrupt_id == p.id)
            .ok_or_else(|| anyhow::anyhow!("missing resume {}", p.id))?;
        if entry.status == "cancelled" {
            db::set_interrupt_status(&app.pool, &tenant.id, &p.id, "cancelled").await?;
            tree.push(Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_call_id: p.tool_call_id.clone(),
                    content: "cancelled by user".into(),
                    is_error: true,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            });
            on_event(rupi_core::AgentEvent::ToolEnd {
                tool_call_id: p.tool_call_id.clone(),
                name: p.tool.clone(),
                content: "cancelled by user".into(),
                is_error: true,
            });
            continue;
        }
        let approved = entry
            .payload
            .as_ref()
            .and_then(|v| v.get("approved"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        db::set_interrupt_status(&app.pool, &tenant.id, &p.id, "resolved").await?;
        if !approved {
            tree.push(Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_call_id: p.tool_call_id.clone(),
                    content: "denied by user".into(),
                    is_error: true,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            });
            on_event(rupi_core::AgentEvent::ToolEnd {
                tool_call_id: p.tool_call_id.clone(),
                name: p.tool.clone(),
                content: "denied by user".into(),
                is_error: true,
            });
            continue;
        }
        let mut args = p.args.clone();
        if let Some(edited) = entry
            .payload
            .as_ref()
            .and_then(|v| v.get("editedArgs"))
            .cloned()
        {
            if edited.is_object() {
                args = edited;
            }
        }
        let out = match p.tool.as_str() {
            "write" => {
                let t = exec
                    .fs_write(
                        handle,
                        rupi_runtime::FsWriteRequest {
                            path: args
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .into(),
                            content: args
                                .get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .into(),
                        },
                    )
                    .await?;
                if t.is_error {
                    rupi_tools::ToolOutput::err(t.content)
                } else {
                    rupi_tools::ToolOutput::ok(t.content)
                }
            }
            "edit" => {
                let t = exec
                    .fs_edit(
                        handle,
                        rupi_runtime::FsEditRequest {
                            path: args
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .into(),
                            arguments: args.clone(),
                        },
                    )
                    .await?;
                if t.is_error {
                    rupi_tools::ToolOutput::err(t.content)
                } else {
                    rupi_tools::ToolOutput::ok(t.content)
                }
            }
            "bash" => {
                let r = exec
                    .exec(
                        handle,
                        rupi_runtime::ExecRequest {
                            command: args
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .into(),
                            timeout_secs: args.get("timeout_secs").and_then(|v| v.as_u64()),
                        },
                    )
                    .await?;
                let mut s = r.stdout;
                if !r.stderr.is_empty() {
                    s.push('\n');
                    s.push_str(&r.stderr);
                }
                if r.exit_code != 0 {
                    rupi_tools::ToolOutput::err(s)
                } else {
                    rupi_tools::ToolOutput::ok(s)
                }
            }
            _ => rupi_tools::ToolOutput::err(format!("cannot resume tool {}", p.tool)),
        };
        tree.push(Message {
            id: uuid::Uuid::new_v4().to_string(),
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_call_id: p.tool_call_id.clone(),
                content: out.content.clone(),
                is_error: out.is_error,
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        });
        on_event(rupi_core::AgentEvent::ToolEnd {
            tool_call_id: p.tool_call_id.clone(),
            name: p.tool.clone(),
            content: out.content,
            is_error: out.is_error,
        });
    }
    agent
        .continue_session(
            provider,
            tree,
            tools,
            mem,
            frozen,
            skills,
            &[],
            on_event,
            cancel,
        )
        .await
}

async fn apply_state(pool: &PgPool, tenant_id: &str, input: &RunAgentInput) -> anyhow::Result<()> {
    let Some(state) = &input.state else {
        return Ok(());
    };
    if state.is_null() {
        return Ok(());
    }
    let name = state.get("sessionName").and_then(|v| v.as_str());
    let model = state.get("model").and_then(|v| v.as_str());
    let thinking = state.get("thinkingLevel").and_then(|v| v.as_str());
    let auto = state.get("autoCompaction").and_then(|v| v.as_bool());
    db::update_session_meta(
        pool,
        tenant_id,
        &input.thread_id,
        name,
        model,
        thinking,
        auto,
    )
    .await
}

async fn finish(app: &App, tenant: &Tenant, thread: &str, run_id: &str, tree: &SessionTree) {
    let _ = db::persist_tree(&app.pool, &tenant.id, thread, tree).await;
    app.cache.invalidate_session(&tenant.id, thread).await;
    app.cache.invalidate_memory(&tenant.id).await;
    quota::release_lease(
        &app.cache,
        &app.pool,
        &tenant.id,
        thread,
        run_id,
        &app.instance_id,
    )
    .await;
    quota::release_run(&app.cache, &tenant.id).await;
}

/// 有 mock 剧本时返回可跨 run 复用的实例；由 [`crate::App`] 按租户缓存。
pub fn cached_mock(tenant: &Tenant) -> Option<Arc<MockProvider>> {
    if let Some(script) = tenant.settings.get("mock_script") {
        if let Ok(resps) = serde_json::from_value::<Vec<rupi_llm::ChatResponse>>(script.clone()) {
            return Some(Arc::new(MockProvider::new(resps)));
        }
    }
    if tenant
        .settings
        .get("provider")
        .and_then(|v| v.as_str())
        == Some("mock")
        || std::env::var("RUPI_CLOUD_MOCK").ok().as_deref() == Some("1")
    {
        if let Ok(path) = std::env::var("RUPI_MOCK_SCRIPT") {
            if let Ok(raw) = std::fs::read_to_string(path) {
                if let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
                    let script = items
                        .into_iter()
                        .map(script_item_to_response)
                        .collect();
                    return Some(Arc::new(MockProvider::new(script)));
                }
            }
        }
        return Some(Arc::new(MockProvider::new(vec![MockProvider::text_response(
            "hello from rupi-server (mock)",
        )])));
    }
    None
}

/// 测试可注入剧本；默认 Mock（无 BYOK）或租户 settings 里的模型。
pub fn default_provider(tenant: &Tenant) -> Arc<dyn LlmProvider> {
    if let Some(p) = cached_mock(tenant) {
        return p;
    }
    let model = tenant
        .settings
        .get("model")
        .and_then(|v| v.as_str())
        .or(tenant.default_model.as_deref())
        .unwrap_or("gpt-4o-mini");
    let key = tenant
        .settings
        .get("openai_api_key")
        .or_else(|| tenant.settings.get("api_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("RUPI_API_KEY").ok());
    let opts = rupi_llm::ProviderOptions {
        api_key: key,
        ..Default::default()
    };
    rupi_llm::provider_or_mock(model, &opts).into()
}

fn script_item_to_response(v: serde_json::Value) -> rupi_llm::ChatResponse {
    if v.get("type").and_then(|t| t.as_str()) == Some("tool") {
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("c1")
            .to_string();
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("write")
            .to_string();
        let arguments = v.get("arguments").cloned().unwrap_or(serde_json::json!({}));
        return rupi_llm::ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
    }
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("ok");
    MockProvider::text_response(text)
}

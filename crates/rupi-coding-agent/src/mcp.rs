use std::sync::Arc;

use rupi_agent_core::AgentTool;
use rupi_mcp::{
    load_mcp_configs, HttpTransport, McpClient, McpToolBridge, PrefixedTool, ServerConfig,
    StdioTransport, TransportKind,
};
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::ConfigPaths;

pub struct ConnectedMcp {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub names: Vec<String>,
}

pub async fn connect_configured_mcp(paths: &ConfigPaths) -> ConnectedMcp {
    let global = paths
        .mcp_config_paths()
        .iter()
        .find(|p| p.starts_with(&paths.home))
        .and_then(|p| std::fs::read_to_string(p).ok());
    // Layer all files: last project file wins via load_mcp_configs(global, project)
    let mut project = None;
    for p in [
        paths.cwd.join(".pi").join("mcp.json"),
        paths.cwd.join(".rupi").join("mcp.json"),
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            project = Some(s);
        }
    }
    // Also merge agent-dir as global-ish if home mcp.json missing
    let global = global.or_else(|| std::fs::read_to_string(paths.agent_dir.join("mcp.json")).ok());
    let configs = load_mcp_configs(global.as_deref(), project.as_deref());
    connect_servers(&configs, &paths.cwd.display().to_string()).await
}

pub async fn connect_servers(configs: &[ServerConfig], cwd: &str) -> ConnectedMcp {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut names = Vec::new();
    for cfg in configs {
        if cfg.lazy {
            continue;
        }
        match connect_one(cfg, cwd).await {
            Ok((prefixed, mut ts)) => {
                names.extend(prefixed);
                tools.append(&mut ts);
            }
            Err(e) => warn!("mcp server `{}` failed: {e}", cfg.name),
        }
    }
    ConnectedMcp { tools, names }
}

async fn connect_one(
    cfg: &ServerConfig,
    cwd: &str,
) -> Result<(Vec<String>, Vec<Arc<dyn AgentTool>>), String> {
    let env: Vec<(String, String)> = cfg.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let transport: Box<dyn rupi_mcp::Transport> = match cfg.transport {
        TransportKind::Stdio => {
            let cmd = cfg
                .command
                .as_deref()
                .ok_or_else(|| "stdio server missing command".to_string())?;
            Box::new(
                StdioTransport::spawn(cmd, &cfg.args, &env)
                    .await
                    .map_err(|e| e.to_string())?,
            )
        }
        TransportKind::Http | TransportKind::Sse => {
            let url = cfg
                .url
                .as_deref()
                .ok_or_else(|| "http server missing url".to_string())?;
            let headers = cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Box::new(HttpTransport::new(url, headers))
        }
        TransportKind::Memory => return Err("memory transport is test-only".into()),
    };
    let mut client = McpClient::new(&cfg.name, transport);
    client
        .initialize(vec![cwd.to_string()])
        .await
        .map_err(|e| e.to_string())?;
    let specs = client.list_tools().await.map_err(|e| e.to_string())?;
    let client = Arc::new(Mutex::new(client));
    let mut out = Vec::new();
    let mut names = Vec::new();
    for spec in specs {
        let bridge = McpToolBridge::new(
            &cfg.name,
            &spec.name,
            spec.annotated_description(),
            spec.input_schema,
            client.clone(),
        );
        let prefixed = PrefixedTool::new(bridge);
        names.push(prefixed.prefix_name.clone());
        out.push(Arc::new(prefixed) as Arc<dyn AgentTool>);
    }
    Ok((names, out))
}

//! 本机 `rupi` 瘦客户端：只消费云控制面 AG-UI/HTTP。
//! 不在本机执行 bash，不拉起 TUI。

use anyhow::Context;
use clap::Subcommand;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// GET /v1/me
    Me,
    /// GET /v1/sessions
    Sessions,
    /// GET /v1/sessions/{id}
    Session { id: String },
    /// 建会话（可省略）并 POST /v1/agent，打印助手文本
    Prompt {
        /// 用户话
        text: Vec<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        region: Option<String>,
        /// `remote-http` 或 `sandbox`
        #[arg(long)]
        backend: Option<String>,
        /// 把 SSE 事件打成 JSONL
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn run(url: &str, api_key: &str, action: Action) -> anyhow::Result<()> {
    let client = Client::new(url, api_key)?;
    match action {
        Action::Me => {
            println!("{}", serde_json::to_string_pretty(&client.me().await?)?);
        }
        Action::Sessions => {
            println!(
                "{}",
                serde_json::to_string_pretty(&client.sessions().await?)?
            );
        }
        Action::Session { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&client.session(&id).await?)?
            );
        }
        Action::Prompt {
            text,
            session,
            name,
            region,
            backend,
            json,
        } => {
            let msg = text.join(" ");
            anyhow::ensure!(!msg.is_empty(), "prompt 需要文本");
            let sid = match session {
                Some(id) => id,
                None => {
                    let created = client.create_session(name.as_deref(), region.as_deref(), backend.as_deref()).await?;
                    created
                        .get("id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("create session: no id"))?
                        .to_string()
                }
            };
            let out = client.prompt(&sid, &msg).await?;
            if json {
                for ev in &out.events {
                    println!("{}", serde_json::to_string(ev)?);
                }
            } else if !out.text.is_empty() {
                println!("{}", out.text);
            }
            if out.error.is_some() {
                anyhow::bail!("{}", out.error.unwrap());
            }
        }
    }
    Ok(())
}

struct Client {
    base: String,
    key: String,
    http: reqwest::Client,
}

struct PromptOut {
    text: String,
    events: Vec<Value>,
    error: Option<String>,
}

impl Client {
    fn new(base: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
        })
    }

    async fn me(&self) -> anyhow::Result<Value> {
        self.get("/v1/me").await
    }

    async fn sessions(&self) -> anyhow::Result<Value> {
        self.get("/v1/sessions").await
    }

    async fn session(&self, id: &str) -> anyhow::Result<Value> {
        self.get(&format!("/v1/sessions/{id}")).await
    }

    async fn create_session(
        &self,
        name: Option<&str>,
        region: Option<&str>,
        backend: Option<&str>,
    ) -> anyhow::Result<Value> {
        let mut body = json!({});
        if let Some(n) = name {
            body["name"] = json!(n);
        }
        if let Some(r) = region {
            body["region"] = json!(r);
        }
        if let Some(b) = backend {
            body["backend"] = json!(b);
        }
        let resp = self
            .http
            .post(format!("{}/v1/sessions", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .context("cloud POST /v1/sessions")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("cloud create session {status}: {text}");
        }
        serde_json::from_str(&text).context(text)
    }

    async fn prompt(&self, thread_id: &str, message: &str) -> anyhow::Result<PromptOut> {
        let run_id = Uuid::new_v4().to_string();
        let resp = self
            .http
            .post(format!("{}/v1/agent", self.base))
            .bearer_auth(&self.key)
            .header("Accept", "text/event-stream")
            .json(&json!({
                "threadId": thread_id,
                "runId": run_id,
                "messages": [{"id": Uuid::new_v4().to_string(), "role": "user", "content": message}]
            }))
            .send()
            .await
            .context("cloud POST /v1/agent")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("cloud agent {status}: {body}");
        }
        Ok(parse_sse(&body))
    }

    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .with_context(|| format!("cloud GET {path}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("cloud {path} {status}: {text}");
        }
        serde_json::from_str(&text).context(text)
    }
}

fn parse_sse(body: &str) -> PromptOut {
    let mut events = Vec::new();
    let mut text = String::new();
    let mut error = None;
    for line in body.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("TEXT_MESSAGE_CONTENT") | Some("TEXT_MESSAGE_DELTA") => {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    text.push_str(d);
                }
            }
            Some("RUN_ERROR") => {
                error = Some(
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("RUN_ERROR")
                        .to_string(),
                );
            }
            _ => {}
        }
        events.push(v);
    }
    PromptOut {
        text,
        events,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_collects_text_not_rpc() {
        let body = "data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t\",\"runId\":\"r\"}\n\ndata: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"delta\":\"hi-cloud\"}\n\ndata: {\"type\":\"RUN_FINISHED\"}\n";
        let out = parse_sse(body);
        assert_eq!(out.text, "hi-cloud");
        assert!(out.events.iter().all(|e| {
            e.get("type").and_then(|t| t.as_str()) != Some("CUSTOM")
                && e.get("type").and_then(|t| t.as_str()) != Some("RAW")
        }));
    }
}

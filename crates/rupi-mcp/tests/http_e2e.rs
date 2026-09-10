//! Streamable HTTP + SSE fallback against a local MCP endpoint.
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use rupi_mcp::{HttpTransport, McpClient, PROTOCOL_VERSION};

#[tokio::test]
async fn http_json_list_and_call() {
    let url = spawn_mcp_http(false).await;
    let mut client = McpClient::new("mock-http", Box::new(HttpTransport::new(&url, vec![])));
    let init = client.initialize(vec!["/tmp".into()]).await.unwrap();
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool("echo", json!({"text": "via-http"}))
        .await
        .unwrap();
    assert_eq!(result.text(), "via-http");
}

#[tokio::test]
async fn http_sse_fallback_list_and_call() {
    let url = spawn_mcp_http(true).await;
    let mut client = McpClient::new("mock-sse", Box::new(HttpTransport::new(&url, vec![])));
    client.initialize(vec!["/tmp".into()]).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    let result = client
        .call_tool("echo", json!({"text": "via-sse"}))
        .await
        .unwrap();
    assert_eq!(result.text(), "via-sse");
}

async fn spawn_mcp_http(sse: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = handle_conn(stream, sse).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

async fn handle_conn(mut stream: TcpStream, sse: bool) -> std::io::Result<()> {
    loop {
        let Some(req) = read_json_rpc(&mut stream).await? else {
            return Ok(());
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-http", "version": "1"}
            }),
            "notifications/initialized" => Value::Null,
            "tools/list" => json!({
                "tools": [{
                    "name": "echo",
                    "description": "echo",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"]
                    }
                }]
            }),
            "tools/call" => {
                let text = req["params"]["arguments"]["text"].as_str().unwrap_or("");
                json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                })
            }
            _ => json!({"error": "unknown"}),
        };
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        });
        write_http_response(&mut stream, &payload, sse).await?;
    }
}

async fn read_json_rpc(stream: &mut TcpStream) -> std::io::Result<Option<Value>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(header_end) = find_header_end(&buf) {
            let header = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let len = header.lines().find_map(|l| {
                l.strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
            }).unwrap_or(0);
            let body_start = header_end + 4;
            while buf.len() < body_start + len {
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            if len == 0 {
                return Ok(None);
            }
            let body = &buf[body_start..body_start + len];
            return Ok(serde_json::from_slice(body).ok());
        }
        if buf.len() > 1_000_000 {
            return Ok(None);
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_http_response(stream: &mut TcpStream, payload: &Value, sse: bool) -> std::io::Result<()> {
    let body = if sse {
        format!("data: {payload}\n\n")
    } else {
        payload.to_string()
    };
    let ctype = if sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nMcp-Session-Id: t1\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

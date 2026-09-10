//! 真实子进程联调：spawn fake MCP server → tools/list → register → tools/call。

use rupi_mcp::{McpManager, McpServerConfig};
use rupi_tools::{Tool, ToolRegistry};
use std::sync::Arc;

fn fake_config() -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp_server.py");
    McpServerConfig::new("fake", "python3", vec![script.to_string()])
}

#[tokio::test]
async fn bridge_lists_and_calls_tools() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    let tools = bridge.list_tools().await.expect("list");
    assert_eq!(tools.len(), 3);
    let echo = tools.iter().find(|t| t.name == "echo").unwrap();

    // string "true" 应被 sanitize 为 boolean true → shout 生效
    let r = bridge
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi", "shout": "true"}),
            &echo.input_schema,
        )
        .await
        .expect("call");
    assert!(!r.is_error);
    assert_eq!(r.text, "HI");

    // tool 执行失败走 isError 结果，而非协议 error
    let fail = tools.iter().find(|t| t.name == "fail").unwrap();
    let r = bridge
        .call_tool("fail", serde_json::json!({}), &fail.input_schema)
        .await
        .expect("call");
    assert!(r.is_error);
    assert_eq!(r.text, "boom");
}

#[tokio::test]
async fn bridge_lists_and_reads_resources() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    let resources = bridge.list_resources().await.expect("list");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "test://notes/hello");
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));

    let text = bridge
        .read_resource("test://notes/hello")
        .await
        .expect("read");
    assert_eq!(text, "HELLO-RESOURCE-CONTENT");

    // 未知 uri 走协议 error（Err），不伪装成空文本
    assert!(bridge.read_resource("test://notes/missing").await.is_err());
}

#[tokio::test]
async fn manager_registers_resource_reader_as_native() {
    let configs = vec![fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.contains(&"fake_read_resource".to_string()));

    // description 自带可用 URI（模型不用猜），原生执行真读远端
    let defs = registry.definitions();
    let reader_def = defs
        .iter()
        .find(|d| d.name == "fake_read_resource")
        .unwrap();
    assert!(reader_def.prompt_snippet.is_some());
    assert!(
        reader_def.description.contains("test://notes/hello"),
        "description 未内嵌可用 URI: {}",
        reader_def.description
    );

    let tool: Arc<dyn Tool> = Arc::new(rupi_mcp::McpResourceReader::new(
        "fake",
        manager.entries[0].bridge.clone(),
        &[],
    ));
    let out = tool
        .execute(serde_json::json!({"uri": "test://notes/hello"}))
        .await
        .expect("exec");
    assert!(!out.is_error);
    assert_eq!(out.content, "HELLO-RESOURCE-CONTENT");

    // 缺 uri 参数与未知资源都转 tool error，不抛
    let out = tool.execute(serde_json::json!({})).await.expect("exec");
    assert!(out.is_error);
}

#[tokio::test]
async fn bridge_lists_and_renders_prompts() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    let prompts = bridge.list_prompts().await.expect("list");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "greet");
    assert!(prompts[0].arguments.is_array());

    let text = bridge
        .get_prompt("greet", serde_json::json!({"name": "Ada"}))
        .await
        .expect("get");
    assert_eq!(text, "Hello, Ada!");

    // 未知模板走协议 error（Err），不伪装成空文本
    assert!(bridge
        .get_prompt("nope", serde_json::json!({}))
        .await
        .is_err());
}

#[tokio::test]
async fn manager_registers_prompt_getter_as_native() {
    let configs = vec![fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.contains(&"fake_get_prompt".to_string()));

    // description 自带可用模板名，原生执行真渲染远端
    let defs = registry.definitions();
    let getter_def = defs.iter().find(|d| d.name == "fake_get_prompt").unwrap();
    assert!(getter_def.prompt_snippet.is_some());
    assert!(
        getter_def.description.contains("greet"),
        "description 未内嵌可用模板: {}",
        getter_def.description
    );

    let tool: Arc<dyn Tool> = Arc::new(rupi_mcp::McpPromptGetter::new(
        "fake",
        manager.entries[0].bridge.clone(),
        &[],
    ));
    let out = tool
        .execute(serde_json::json!({"name": "greet", "arguments": {"name": "Ada"}}))
        .await
        .expect("exec");
    assert!(!out.is_error);
    assert_eq!(out.content, "Hello, Ada!");

    // 缺 name 参数转 tool error，不抛
    let out = tool.execute(serde_json::json!({})).await.expect("exec");
    assert!(out.is_error);
}

#[tokio::test]
async fn manager_registers_remote_tools_as_native() {
    let configs = vec![fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.contains(&"fake_echo".to_string()));
    assert!(names.contains(&"fake_fail".to_string()));

    // 注册后的原生工具可直接执行，且 prompt_snippet 必填
    let defs = registry.definitions();
    let echo_def = defs.iter().find(|d| d.name == "fake_echo").unwrap();
    assert!(echo_def.prompt_snippet.is_some());

    let tool: Arc<dyn Tool> = Arc::new(rupi_mcp::McpToolExecutor::new(
        "fake",
        manager.entries[0].bridge.clone(),
        &rupi_mcp::McpTool {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}}),
        },
    ));
    let out = tool
        .execute(serde_json::json!({"text": "yo"}))
        .await
        .expect("exec");
    assert!(!out.is_error);
    assert_eq!(out.content, "yo");
}

#[tokio::test]
async fn bridge_answers_roots_list_requests() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    assert!(!bridge.roots.is_empty());
    assert!(bridge.roots[0].uri.starts_with("file://"));
    // fake server 在 initialized 后反向请求 roots/list，轮询 probe 直到桥应答到达
    let probe = bridge
        .list_tools()
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.name == "roots_probe")
        .expect("roots_probe tool");
    let mut seen = String::new();
    for _ in 0..50 {
        let r = bridge
            .call_tool("roots_probe", serde_json::json!({}), &probe.input_schema)
            .await
            .expect("call");
        if r.text != "null" {
            seen = r.text;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen.contains("file://"), "server saw roots: {seen}");
}

#[tokio::test]
async fn manager_skips_dead_servers_without_misalignment() {
    let bad = McpServerConfig::new("dead", "definitely-not-a-real-binary-xyz", vec![]);
    let configs = vec![bad, fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    // 死 server 只跳过自己：存活项仍与自己的 config 配对（zip 错位回归）
    assert_eq!(manager.entries.len(), 1);
    assert_eq!(manager.entries[0].config.name, "fake");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.iter().all(|n| n.starts_with("fake_")));
    assert!(names.contains(&"fake_echo".to_string()));
}

#[test]
fn server_request_response_covers_roots_ping_unknown() {
    let roots = vec![rupi_mcp::McpRoot {
        uri: "file:///proj".into(),
        name: "proj".into(),
    }];
    let r = rupi_mcp::server_request_response("roots/list", Some(7), &roots).unwrap();
    assert_eq!(r["id"], serde_json::json!(7));
    assert_eq!(
        r["result"]["roots"][0]["uri"],
        serde_json::json!("file:///proj")
    );
    let p = rupi_mcp::server_request_response("ping", Some(8), &roots).unwrap();
    assert_eq!(p["result"], serde_json::json!({}));
    let e = rupi_mcp::server_request_response("sampling/createMessage", Some(9), &roots).unwrap();
    assert_eq!(e["error"]["code"], serde_json::json!(-32601));
    assert!(rupi_mcp::server_request_response("ping", None, &roots).is_none());
}

/// 最小 StreamableHTTP stub：手写 HTTP/1.1 成帧（每连接一请求，`Connection: close`）。
/// initialize→JSON+session 头，initialized→202，tools/list→JSON，tools/call→SSE 流（含 ping 注释、
/// 一条 ping 反向请求、一条 roots/list 反向请求在结果之前，验证桥增量应答）。
/// 无 method 但带 id+result/error 的 POST 即桥对反向请求的应答，记入 `answers` 后回 202。
/// `seen.session_header` 记录非握手请求是否回传 session。
struct HttpStubSeen {
    session_header: std::sync::Mutex<bool>,
    answers: std::sync::Mutex<Vec<serde_json::Value>>,
}

async fn start_http_stub() -> (String, Arc<HttpStubSeen>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let seen = Arc::new(HttpStubSeen {
        session_header: std::sync::Mutex::new(false),
        answers: std::sync::Mutex::new(vec![]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let seen_clone = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let seen = seen_clone.clone();
            tokio::spawn(async move {
                let (rh, mut wh) = sock.into_split();
                let mut reader = tokio::io::BufReader::new(rh);
                let mut content_len = 0usize;
                let mut has_session = false;
                let mut request_line = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if request_line.is_empty() {
                        request_line = line.trim().to_string();
                    }
                    let t = line.trim();
                    if t.is_empty() {
                        break;
                    }
                    let lower = t.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("mcp-session-id:") {
                        has_session = true;
                    }
                }
                // 独立 GET 常驻流：推一条 roots/list 反向请求（id 55）后关流；
                // 桥应以后台任务应答（记入 answers），断线重连会再推一次，同 id 去重断言即可。
                if request_line.starts_with("GET") {
                    let payload =
                        "data: {\"jsonrpc\":\"2.0\",\"id\":55,\"method\":\"roots/list\",\"params\":{}}\n\n"
                            .as_bytes();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    wh.write_all(head.as_bytes()).await.unwrap();
                    wh.write_all(payload).await.unwrap();
                    return;
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    reader.read_exact(&mut body).await.unwrap();
                }
                let req: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = req.get("id").cloned().unwrap_or(serde_json::json!(0));
                if method != "initialize" && has_session {
                    *seen.session_header.lock().unwrap() = true;
                }
                // 桥对流内反向请求的应答：无 method + 带 id + result/error，记下后回 202
                if method.is_empty()
                    && req.get("id").is_some()
                    && (req.get("result").is_some() || req.get("error").is_some())
                {
                    seen.answers.lock().unwrap().push(req);
                    let head = "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\n\
                        Content-Length: 0\r\nConnection: close\r\n\r\n";
                    wh.write_all(head.as_bytes()).await.unwrap();
                    return;
                }
                let (status, ctype, extra, payload): (_, _, _, Vec<u8>) = match method {
                    "initialize" => (
                        "200 OK",
                        "application/json",
                        "mcp-session-id: stub-1\r\n",
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2024-11-05"}})
                            .to_string()
                            .into_bytes(),
                    ),
                    "notifications/initialized" => ("202 Accepted", "application/json", "", vec![]),
                    "tools/list" => (
                        "200 OK",
                        "application/json",
                        "",
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                            "name":"hecho","description":"http echo",
                            "inputSchema":{"type":"object"}}]}})
                        .to_string()
                        .into_bytes(),
                    ),
                    "tools/call" => (
                        "200 OK",
                        "text/event-stream",
                        "",
                        format!(
                            ": ping\n\ndata: {{\"jsonrpc\":\"2.0\",\"id\":999,\"method\":\"ping\"}}\n\n\
                             data: {{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"roots/list\",\"params\":{{}}}}\n\n\
                             data: {{\"jsonrpc\":\"2.0\",\"id\":{id},\
                             \"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"HTTP-HI\"}}]}}}}\n\n"
                        )
                        .into_bytes(),
                    ),
                    _ => (
                        "200 OK",
                        "application/json",
                        "",
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"not found"}})
                            .to_string()
                            .into_bytes(),
                    ),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n{extra}\r\n",
                    payload.len()
                );
                wh.write_all(head.as_bytes()).await.unwrap();
                wh.write_all(&payload).await.unwrap();
            });
        }
    });
    (url, seen)
}

#[tokio::test]
async fn http_transport_lists_calls_tools_and_keeps_session() {
    let (url, seen) = start_http_stub().await;
    let mut cfg = McpServerConfig::new("h", "", vec![]);
    cfg.url = Some(url);
    let bridge = rupi_mcp::McpBridge::spawn_http(cfg)
        .await
        .expect("spawn http");
    let tools = bridge.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "hecho");

    // tools/call 经 SSE 流回包：桥正确挑出本轮 id（跳过 ping 注释），
    // 流内 ping 与 roots/list 反向请求当场 POST 应答（此前已知局限：server 侧超时）
    let r = bridge
        .call_tool("hecho", serde_json::json!({}), &tools[0].input_schema)
        .await
        .expect("call");
    assert!(!r.is_error);
    assert_eq!(r.text, "HTTP-HI");

    // initialize 后下发的 session id，后续请求经 header 回传
    assert!(*seen.session_header.lock().unwrap(), "session id 未回传");
    // 反向 roots/list（id 77）已应答且带本机 file:// 根；ping（id 999）回空结果
    let answers = seen.answers.lock().unwrap();
    let roots_answer = answers.iter().find(|a| a["id"] == 77).expect("roots 应答");
    assert!(
        roots_answer["result"]["roots"][0]["uri"]
            .as_str()
            .unwrap_or("")
            .starts_with("file://"),
        "roots 应答带本机根：{roots_answer}"
    );
    let ping_answer = answers.iter().find(|a| a["id"] == 999).expect("ping 应答");
    assert_eq!(ping_answer["result"], serde_json::json!({}));
}

#[tokio::test]
async fn http_server_stream_answers_pushed_requests() {
    let (url, seen) = start_http_stub().await;
    let mut cfg = McpServerConfig::new("h", "", vec![]);
    cfg.url = Some(url);
    let bridge = rupi_mcp::McpBridge::spawn_http(cfg)
        .await
        .expect("spawn http");
    // GET 常驻流是后台任务：轮询等 stub 收到对推送 roots/list（id 55）的应答
    let mut found = None;
    for _ in 0..60 {
        {
            let answers = seen.answers.lock().unwrap();
            if let Some(a) = answers.iter().find(|a| a["id"] == 55) {
                found = Some(a.clone());
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let roots_answer = found.expect("GET 流推送的 roots/list（id 55）未被应答");
    assert!(
        roots_answer["result"]["roots"][0]["uri"]
            .as_str()
            .unwrap_or("")
            .starts_with("file://"),
        "推送应答带本机根：{roots_answer}"
    );
    // bridge drop 即 abort 流任务：此处显式 drop，泄漏/死锁会直接挂测试
    drop(bridge);
}

#[tokio::test]
async fn tools_list_changed_refreshes_registry() {
    // 动态假 server（FAKE_MCP_DYNAMIC=1）：第 1 次 list 回基础 3 件套并附通知，
    // 第 2 次多出 late，第 3 次起 late 消失。不断线热刷新两轮。
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp_server.py");
    let mut cfg = McpServerConfig::new("fake", "python3", vec![script.to_string()]);
    cfg.env.insert("FAKE_MCP_DYNAMIC".into(), "1".into());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let manager = McpManager::spawn_all_watched(&[cfg], tx)
        .await
        .expect("spawn all");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.contains(&"fake_echo".to_string()));
    assert!(
        !names.iter().any(|n| n == "fake_late"),
        "首轮 list 不应有 late"
    );

    // 第 1 个通知到达 → 刷新 → late 出现且端到端可调
    let srv = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("通知超时")
        .expect("通道关闭");
    assert_eq!(srv, "fake");
    let added = manager
        .refresh_server(&mut registry, "fake")
        .await
        .expect("refresh");
    assert_eq!(added, vec!["fake_late".to_string()]);
    let out = registry
        .execute("fake_late", serde_json::json!({}))
        .await
        .expect("exec late");
    assert!(!out.is_error);
    assert_eq!(out.content, "LATE-OK");

    // 第 2 个通知到达 → 再刷新 → late 消失，echo 常驻不受影响
    let srv = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("通知超时")
        .expect("通道关闭");
    assert_eq!(srv, "fake");
    let added = manager
        .refresh_server(&mut registry, "fake")
        .await
        .expect("refresh");
    assert!(added.is_empty());
    assert!(
        registry.definitions().iter().all(|d| d.name != "fake_late"),
        "消失的工具应注销"
    );
    assert!(
        registry.definitions().iter().any(|d| d.name == "fake_echo"),
        "常驻工具不应被误伤"
    );
    // 未知 server 刷新直接报错，不动注册表
    assert!(manager.refresh_server(&mut registry, "nope").await.is_err());
}

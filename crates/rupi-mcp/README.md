# rupi-mcp

MCP 桥。Pi 的 core 不内置 MCP；这里按扩展装。

```
spawn → initialize → tools/list → registerTool 为 {server}_{name}
```

- stdio：换行 JSON-RPC 2.0，30s 超时
- Streamable HTTP：`url` 配置，POST 单 JSON 或 SSE，`mcp-session-id` 保持；独立 GET 常驻流
- `sanitize_params`：按 schema 把 string 纠回 boolean/number/integer
- `prompt_snippet` 必填，否则 agent 看不见工具
- `resources/list→read` → `{server}_read_resource`
- `prompts/list→get` → `{server}_get_prompt`
- server→client：`roots/list`、`ping`
- `McpManager` 配对注册，单 server 失败不影响其他

探活：`rupi mcp-list <command>` 或 `--url`。

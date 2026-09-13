# rupi-agent

`AgentLoop`：流式助手消息 → 工具 → 再循环，直到无工具调用。TUI、`chat`、`run`、`--mode rpc`、云 `POST /v1/agent` 都走这里。

- `PromptBuilder`：base + 记忆冻结块 + skill 索引 + MCP `promptSnippet` + 扩展片段
- `policy`：`AllowAll` / `RulePolicy` / `ChainPolicy` + `Approver`；拒绝变 tool error。`--plan` 禁 write/edit/bash。危险 bash 子串转人工审批
- `subagent`：`run_subagents` 并发扇出，深度 guard 断递归；`--subagents` 开启
- 压实：`maybe_compress` 按 token（默认 reserve 16384 / keep 20000）；溢出最多恢复 2 次
- `MessageInbox`：运行中转向 / 跟进
- `create_agent_session`：给 `--mode rpc` 用的宿主句柄

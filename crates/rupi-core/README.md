# rupi-core

类型与 trait，不依赖 LLM 或 IO。对标 `pi-agent-core`。

- `Message` / `ContentBlock` / `Role`
- `SessionTree`：`branch_from` / `rewind_to` / `goto_node` / `prompt_history`
- `AgentEvent`：文本增量、工具起止、压实、usage、UI 问询
- `ToolDefinition` + `Tool` trait
- `Extension` 点；斜杠命令与 `@file` 展开在 `commands` / `template`

本机与 `rupi-server` 共用这一层。

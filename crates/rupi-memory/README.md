# rupi-memory

本机 Hermes 风格记忆 + SQLite 会话库。

- 文件：`MEMORY.md` / `USER.md` / `failures.md`，各 5000 字符，超限按行丢最旧
- 启动冻结快照注入系统提示；会话内写盘即时，下个 session 才改快照
- `[core]` 行固定注入；有 `[core]` 时其余走 `memory_search`
- `SessionStore`：`sessions.db`（WAL + FTS5 trigram）。每节点 `content` 纯文本 + `blocks` 完整消息 JSON
- `MemoryProvider` 七方法；`MemoryManager` 只允许一个外部 provider。`--memory-provider jsonl` 走 `JsonlProvider`
- 密钥子串拒写

云控制面的会话/记忆权威是 PostgreSQL，不走这个 SQLite。sandbox 卷上的 `MEMORY.md` 也不是云主存。

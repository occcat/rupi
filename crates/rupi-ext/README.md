# rupi-ext

外部进程扩展：一个 `*.json` manifest + 一条可执行命令。

| `protocol` | 行为 |
|---|---|
| `oneshot`（默认） | stdin 写 `arguments` JSON，stdout 即工具结果 |
| `jsonrpc` | 长连接双向 JSON-RPC（复用 `rupi-mcp` 换行帧）。`initialize` 可注册斜杠命令、订阅 `tool_call` / `turn_end` / `session_*` |

非零退出码 → tool error（`stderr` 并入），主循环不崩。`ExtensionSet::refresh` 按 mtime 增量重载；`/reload` 或每轮自动检查。stdout 与内置 bash 同口径 12k 折叠，超时 `kill_on_drop`。

`--ext-dir` / `ext-list`。WASM 不在本 crate。示例：`examples/extensions/echo-rpc.json`。

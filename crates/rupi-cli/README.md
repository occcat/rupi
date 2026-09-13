# rupi-cli

`rupi` 二进制：把各 crate 装成本机入口，并提供连云的瘦客户端。

```text
rupi                  # 默认 TUI
rupi chat             # REPL
rupi run [-p] [--json] …
rupi --mode rpc       # 本机 JSONL；云对话不用这条
rupi cloud --url …    # AG-UI/HTTP；需要 --api-key 或 RUPI_CLOUD_KEY
```

子命令还包括记忆、skill、会话检索、MCP 探活、`install` / `login` / `models`。`login` 只打印 API key 环境变量。

本机 `bash` 是 cwd 上的 `sh -c`。`rupi cloud` 不在本机执行 bash，也不进 TUI。

# rupi-benches

仓内 Criterion，不发布。

```bash
cargo bench -p rupi-benches
```

| bench | 测什么 |
|---|---|
| `sse` | SSE 解析 |
| `edit` | 多处 edit + diff |
| `compaction` | 会话压实 |
| `session_store` | SQLite `SessionStore` |

启动耗时：仓库根 `scripts/bench-startup.sh`（hyperfine + release 二进制）。

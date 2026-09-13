# rupi-runtime

云执行面。控制面只认 `Executor`；工作区与用户命令在外部进程落地。

| 后端 | 二进制 | 用途 |
|---|---|---|
| `remote-http` | `rupi-execd` | 登记的远程机器池 |
| `sandbox` | `rupi-sandboxd` | sandbox 集群节点 |

```bash
export RUPI_EXEC_TOKEN=...
rupi-execd --listen 127.0.0.1:8090 --root /tmp/rupi-execd --token "$RUPI_EXEC_TOKEN"
rupi-sandboxd --listen 127.0.0.1:8190 --root /tmp/rupi-sandboxd --token "$RUPI_EXEC_TOKEN"
```

非回环禁止空 token。回环空 token 必须 `--insecure`（`RUPI_EXEC_INSECURE=1`）。`sandboxd` 是句柄根 jail，不是微 VM。控制面用 `--executor-urls` / `--sandbox-urls`（可写 `region=url`）登记这些节点。本 crate 不在 API 机起 Docker 或 `sh -c`。上线见 [`docs/LAUNCH.md`](../../docs/LAUNCH.md)。

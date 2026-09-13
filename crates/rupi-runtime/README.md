# rupi-runtime

云执行面。控制面只认 `Executor`；工作区与用户命令在外部进程落地。

| 后端 | 二进制 | 用途 |
|---|---|---|
| `remote-http` | `rupi-execd` | 登记的远程机器池 |
| `sandbox` | `rupi-sandboxd` | sandbox 集群节点 |

```bash
rupi-execd --listen 127.0.0.1:8090 --root /tmp/rupi-execd
rupi-sandboxd --listen 127.0.0.1:8190 --root /tmp/rupi-sandboxd
```

`RUPI_EXEC_TOKEN` 可选。控制面用 `--executor-urls` / `--sandbox-urls`（可写 `region=url`）登记这些节点。本 crate 不在 API 机起 Docker 或 `sh -c`。

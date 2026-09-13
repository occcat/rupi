# rupi-server

无状态云控制面。不链接 `rupi-tui`。

- 会话与记忆：PostgreSQL（行级 `tenant_id`）
- 缓存：Redis。挂了降级读库，不当主存
- 对话：`POST /v1/agent`（AG-UI `RunAgentInput` → SSE `BaseEvent`）
- 薄 REST：`/v1/me`、`/v1/sessions`、fork/clone、进出口、settings、models
- 管理面：`/admin`（静态页）+ `/admin/api/*`。凭证是 `RUPI_ADMIN_TOKEN` 或 `rupi_admin_*` Key
- 执行：只打外部 `Executor`（见 `rupi-runtime`）

```bash
rupi-server \
  --listen 127.0.0.1:8080 \
  --database-url postgres://rupi:rupi@127.0.0.1:5432/rupi \
  --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 \
  --admin-token "$RUPI_ADMIN_TOKEN"
```

`--bootstrap-tenant NAME` / `--bootstrap-admin` 建一把明文 Key 后退出，给本地和 CI。没有订阅登录。

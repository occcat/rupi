# 上线说明

对着真实 CLI flag 写。没有的开关这里也不写。本机 CLI/TUI 打磨（find/ls、theme、`/copy` 等）不挡上线。

对照：[`UPSTREAM.md`](UPSTREAM.md)。

## 组件

1. PostgreSQL：会话、记忆、配额、审计权威
2. Redis：缓存。挂了必须能读库
3. `rupi-execd` 和/或 `rupi-sandboxd`：用户 bash 只在这里
4. `rupi-server`：无状态控制面，可多副本
5. `/admin`：`RUPI_ADMIN_TOKEN` 或库内 `rupi_admin_*` Key

控制面进程里不跑用户 `sh -c`。`sandboxd` 是句柄根 jail（bwrap / landlock / chroot 尽力），**不是**微 VM。

## 真实开关

### `rupi-server`

| flag | 环境变量 | 默认 | 说明 |
|---|---|---|---|
| `--listen` | `RUPI_LISTEN` | `127.0.0.1:8080` | 控制面监听 |
| `--database-url` | `DATABASE_URL` | （必填） | Postgres |
| `--database-read-url` | `DATABASE_READ_URL` | 无 | 只读副本；hydrate / 写仍走主库 |
| `--redis-url` | `REDIS_URL` | `redis://127.0.0.1:6379` | 缓存 |
| `--redis-cluster` | — | false | Cluster 客户端。也可用 `REDIS_CLUSTER=1`，或 URL `redis-cluster://` / 逗号多 seed |
| `--executor-url` | `RUPI_EXECUTOR_URL` | 空 | 单个 execd |
| `--executor-urls` | `RUPI_EXECUTOR_URLS` | 空 | 逗号分隔，可写 `region=url`；优先于单个 URL |
| `--sandbox-urls` | `RUPI_SANDBOX_URLS` | 空 | sandboxd 节点，格式同上 |
| `--executor-token` | `RUPI_EXEC_TOKEN` | 空 | 打执行节点的口令 |
| `--region` | `RUPI_REGION` | `local` | 本副本区域标签 |
| `--instance-id` | `RUPI_INSTANCE_ID` | 随机 UUID | 副本身份 |
| `--snapshot-dir` | `RUPI_SNAPSHOT_DIR` | `./rupi-data/snapshots` | 本机对象盘（相对 cwd；多副本不够） |
| `--snapshot-uri` | `RUPI_SNAPSHOT_URI` | 无 | `s3://bucket/prefix` 或 `memory:`（测试）；优先于 snapshot-dir |
| `--insecure-exec` | `RUPI_EXEC_INSECURE` | false | 允许控制面用明文 HTTP 打**非回环**执行节点 |
| `--tls-cert` | `RUPI_TLS_CERT` | 无 | PEM 证书。与 `--tls-key` 一起在进程内 rustls 终止 TLS |
| `--tls-key` | `RUPI_TLS_KEY` | 无 | PEM 私钥 |
| `--insecure` | `RUPI_LISTEN_INSECURE` | false | 允许非回环**明文**监听。生产应 rustls 或前面反代 |
| `--idle-secs` | `RUPI_IDLE_SECS` | 1800 | 闲置回收 |
| `--admin-token` | `RUPI_ADMIN_TOKEN` | 空 | 管理面共享口令（等同 `admin` 角色） |
| `--bootstrap-tenant NAME` | — | 无 | 建租户、打印明文 Key 后**退出** |
| `--bootstrap-admin` | — | false | 建管理 Key（`admin` 角色）、打印后**退出** |

没有 `--mode` / `--bind`。对外 443 仍可把 TLS 放在前面；也可以 `--tls-cert`/`--tls-key` 让控制面自己终止。明文只给回环，或显式 `--insecure`。

### `rupi-execd`

| flag | 环境变量 | 默认 |
|---|---|---|
| `--listen` | — | `127.0.0.1:8090` |
| `--root` | — | `/tmp/rupi-execd` |
| `--token` | `RUPI_EXEC_TOKEN` | 空 |
| `--max-workspaces` | `RUPI_EXEC_MAX` | 64 |
| `--warm-pool` | `RUPI_EXEC_WARM` | 2 |
| `--insecure` | `RUPI_EXEC_INSECURE` | false |

`--root` 必须可写。本机回环用 `./rupi-data/execd`。`/var/lib/...` 只有目录已存在且进程用户可写时才用，否则启动直接 `Permission denied`。

### `rupi-sandboxd`

与 execd 相同的 `--listen` / `--root` / `--token` / `--insecure`。`--warm-pool` **不是** `RUPI_EXEC_WARM`：

| flag | 环境变量 | 默认 |
|---|---|---|
| `--warm-pool` | `RUPI_SANDBOX_WARM` | 2 |
| `--max-sandboxes` | `RUPI_SANDBOX_MAX` | 64 |
| `--region` | `RUPI_REGION` | `local` |

### 仅环境变量（无独立 clap）

| 变量 | 作用 |
|---|---|
| `REDIS_CLUSTER=1` | 与 `--redis-cluster` 相同（clap 本身不读这个 env） |
| `RUPI_DB_INSECURE=1` | 允许非回环明文 `DATABASE_URL`（跳过 rustls） |
| `RUPI_GIT_ALLOW_ANON=1` | 允许无 token 的公开 https git clone（默认关） |
| `RUPI_CLOUD_ALLOW_MOCK=1` | 租户 `PATCH /v1/settings` 可以写 `mock_script` |
| `RUPI_CLOUD_MOCK` / `RUPI_MOCK_SCRIPT` | 控制面走 mock 剧本（本地/CI）。`=1` 时无剧本回 `hello from rupi-server (mock)` |
| `RUPI_S3_ENDPOINT` 或 `AWS_ENDPOINT_URL` | S3 兼容 endpoint（默认 `https://s3.amazonaws.com`） |
| `RUPI_S3_REGION` 或 `AWS_REGION` | 默认 `us-east-1` |
| `RUPI_S3_ACCESS_KEY` 或 `AWS_ACCESS_KEY_ID` | 必填（用 `s3://` 时） |
| `RUPI_S3_SECRET_KEY` 或 `AWS_SECRET_ACCESS_KEY` | 必填（用 `s3://` 时） |
| `RUPI_EXEC_ALLOC_QUEUE_MS` | 池满时 execd `alloc` 最多排队多少毫秒，超时仍 `429`。默认 `0`（立刻拒绝） |
| `RUPI_LOAD_SESSIONS` | 万级短流入口的会话数，默认 `256` |
| `RUPI_LOAD_STREAMS` | 万级短流入口完成的 mock AG-UI 次数，默认 `10000` |
| `RUPI_LOAD_CONC` | 万级短流同时在飞的流数，默认 `32`。不要在 GitHub Actions 里拉到一万 |

## 必须守住的默认值

- **空 token**：非回环监听直接拒启动。回环空 token 还必须带 `--insecure`（或 `RUPI_EXEC_INSECURE=1`）。`0.0.0.0` 即使 `--insecure` 也不能空 token。比较走恒定时间 `tokens_eq`。
- **Postgres**：回环可用明文。非回环默认走进程内 **rustls**（强制 `sslmode=require`）。`sslmode=` 字面量不能当 TLS。非回环明文必须 `RUPI_DB_INSECURE=1`。
- **控制面监听**：回环可用明文。非回环默认必须 `--tls-cert` + `--tls-key`（进程内 rustls）。非回环明文必须 `--insecure` / `RUPI_LISTEN_INSECURE=1`。
- **执行面 URL**：控制面打 `http://` 非回环节点必须 `--insecure-exec`。回环 `http://127.0.0.1` 可以。生产用 HTTPS 或不要把 execd 暴露到公网。
- **管理面 RBAC**：库内 `rupi_admin_*` Key 有 `admin` / `operator` / `viewer`。`RUPI_ADMIN_TOKEN` 等同 `admin`。`viewer` 只读；`operator` 可改租户/配额/会话，不能颁发或吊销管理 Key。
- **快照按区域分**：对象 key 是 `{region}/ws/{tenant}/{session}/{handle}.tgz`。跨区域不会默默读到另一区的副本。
- **schema**：`migrate` 写 `schema_migrations`，当前版本 4。重复启动只补未登记的步骤。
- `--bootstrap-tenant` / `--bootstrap-admin` 只用来种第一把 Key。正式进程不要带。
- 租户 BYOK 按 provider 读 `anthropic_api_key` / `gemini_api_key` / `openai_api_key`（或 `api_key`）。不会把 OpenAI key 塞给 Anthropic。
- `git` bootstrap 只接受 `https://` 或 `git@host:path`（`file://` 一律拒）。host 默认白名单（github.com / gitlab.com / bitbucket.org / git.sr.ht / codeberg.org），可用 settings / 请求体 `git_hosts` 加。无 token 的 `git@` / https 默认拒；公开只读 https 需 `RUPI_GIT_ALLOW_ANON=1`。租户 token 只进这一次 clone。
- 租户 `PATCH /v1/settings` 与管理面同一白名单；`***` 不覆盖密钥；`mock_script` 仅 `RUPI_CLOUD_ALLOW_MOCK=1` 或管理面。
- **`/ready`**：Postgres 不通，或没有任何执行节点 `stats` 成功（`capacity > 0`），HTTP **503**，JSON `ok: false`。Redis 挂了仍 200（缓存可降级）。不配 `--executor-urls` / `--sandbox-urls` 时进程能起来，但 `/ready` 是 503。

## 本机先跑通（回环）

回环也要带执行口令。空 token 起 execd 必须再加 `--insecure`，生产不要这么干。

`--root` 写相对 cwd 的 `./rupi-data/...`（与默认 `--snapshot-dir` 同一棵树）。不要用 `/var/lib/rupi/...`，非 root 会在建目录时直接退出。

```bash
# 本页后面用 target/release
cargo build --release -p rupi-server

# Postgres / Redis 自备。与 CI 同款镜像可以是：
#   docker run -d --name rupi-pg -e POSTGRES_USER=rupi -e POSTGRES_PASSWORD=rupi \
#     -e POSTGRES_DB=rupi -p 127.0.0.1:5432:5432 postgres:16
#   docker run -d --name rupi-redis -p 127.0.0.1:6379:6379 redis:7
# migrate 会 `CREATE EXTENSION IF NOT EXISTS pg_trgm`（需要 contrib）。
# 扩展失败只让中文检索降级，不挡启动。

export RUPI_ADMIN_TOKEN=...
export RUPI_EXEC_TOKEN=...
export DATABASE_URL=postgres://rupi:rupi@127.0.0.1:5432/rupi
# 无 BYOK 时打 AG-UI 用 mock（写在正式 rupi-server 进程环境里）
export RUPI_CLOUD_MOCK=1

# 种 Key（打印后退出）
./target/release/rupi-server \
  --database-url "$DATABASE_URL" \
  --bootstrap-admin
./target/release/rupi-server \
  --database-url "$DATABASE_URL" \
  --bootstrap-tenant demo
# 上面打印的租户 Key 当作 RUPI_CLOUD_KEY

./target/release/rupi-execd \
  --listen 127.0.0.1:8090 \
  --root ./rupi-data/execd \
  --token "$RUPI_EXEC_TOKEN"

# 可选第二种后端
./target/release/rupi-sandboxd \
  --listen 127.0.0.1:8190 \
  --root ./rupi-data/sandboxd \
  --token "$RUPI_EXEC_TOKEN"

./target/release/rupi-server \
  --listen 127.0.0.1:8080 \
  --database-url "$DATABASE_URL" \
  --redis-url redis://127.0.0.1:6379 \
  --executor-urls http://127.0.0.1:8090 \
  --sandbox-urls http://127.0.0.1:8190 \
  --executor-token "$RUPI_EXEC_TOKEN" \
  --admin-token "$RUPI_ADMIN_TOKEN"
```

验收：

```bash
curl -sS -o /tmp/ready.json -w "%{http_code}\n" http://127.0.0.1:8080/ready
# 健康必须是 200；JSON ok=true。Postgres / 执行面挂了是 503。
curl -sS http://127.0.0.1:8080/health
curl -sS http://127.0.0.1:8080/metrics
curl -sS -H "Authorization: Bearer $RUPI_ADMIN_TOKEN" http://127.0.0.1:8080/admin/api/me

# 管理面种 Key（也可用上面 bootstrap 打出来的 RUPI_CLOUD_KEY）
CREATE=$(curl -sS -H "Authorization: Bearer $RUPI_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"demo-live"}' \
  http://127.0.0.1:8080/admin/api/tenants)
export RUPI_CLOUD_KEY=$(printf '%s' "$CREATE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')

curl -sS -H "Authorization: Bearer $RUPI_CLOUD_KEY" http://127.0.0.1:8080/v1/me

SESS=$(curl -sS -H "Authorization: Bearer $RUPI_CLOUD_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"thread-1"}' \
  http://127.0.0.1:8080/v1/sessions)
SID=$(printf '%s' "$SESS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

# POST /v1/agent 用租户 Key。RUPI_CLOUD_MOCK=1 时 TEXT_MESSAGE_* delta 拼起来是
# hello from rupi-server (mock)（原始 SSE 不会出现这一整句）。
curl -sS -H "Authorization: Bearer $RUPI_CLOUD_KEY" \
  -H "Accept: text/event-stream" \
  -H "Content-Type: application/json" \
  -d "{\"threadId\":\"$SID\",\"runId\":\"run-1\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}]}" \
  http://127.0.0.1:8080/v1/agent

# 掐 SSE 后 AgentLoop 停、exec 停
# 租户 A 的 bash 读不到租户 B 的卷
```

管理面：`http://127.0.0.1:8080/admin`。

## 对外听

`--listen 0.0.0.0:8080`（或前面反代的 443）。执行面不要对公网裸 HTTP。多副本共用同一 Postgres / Redis，以及共享对象存储：

```bash
export RUPI_SNAPSHOT_URI=s3://rupi-snapshots/prod
export RUPI_S3_ENDPOINT=https://s3.example.com
export RUPI_S3_REGION=us-east-1
export RUPI_S3_ACCESS_KEY=...
export RUPI_S3_SECRET_KEY=...
```

工作区在 Executor 上，不在 API 盘。`--snapshot-dir` 只够单机；未指定时写到 `./rupi-data/snapshots`。多副本用 `--snapshot-uri` / `RUPI_SNAPSHOT_URI`（S3 兼容，MinIO 或任何 SigV4 路径样式服务）。对象按会话 `region` 分前缀。CI 里 `S3ObjectStore` 走进程内 S3 兼容 HTTP；本机若起了 MinIO，设 `RUPI_S3_ENDPOINT` 会再打一枪真服务。

控制面自己终止 TLS：

```bash
rupi-server --listen 0.0.0.0:8443 \
  --tls-cert /etc/rupi/tls.crt --tls-key /etc/rupi/tls.key \
  --database-url "postgres://rupi:rupi@db.internal:5432/rupi" \
  ...
```

非回环明文（只在显式 insecure 时）：

```bash
rupi-server --listen 0.0.0.0:8080 --insecure ...
```

非回环 Postgres 走 rustls：

```bash
--database-url "postgres://rupi:rupi@db.internal:5432/rupi"
```

非回环执行面明文必须同时：

```bash
rupi-server --insecure-exec --executor-urls http://10.0.0.8:8090 ...
# 且 execd 必须有非空 --token
```

## 容量演练

CI `cloud` job 跑缩小规模的池耗尽 / 杀副本（`crates/rupi-server/tests/cloud_chaos.rs`）：池满且有 run → `429`；放开后抢占闲置卷再入院；杀掉一个控制面副本后另一副本仍能读会话树和共享快照。另有一条非 ignore 的短流回归（2 会话 / 4 次）。**默认 CI / 每张 PR 不跑万级，也不会打满一万条长 SSE。**

本机万级入口是短 mock 流（做完就结束），不是一万条常驻连接。GitHub Actions `workflow_dispatch` 勾 `ten_thousand_streams` 也会跑同一条 `#[ignore]` 用例：

```bash
# 需要本机 Postgres / Redis（与 cloud job 相同的 DATABASE_URL / REDIS_URL）
cargo test -p rupi-server --test cloud_chaos ten_thousand_streams -- --ignored --nocapture
```

默认 256 会话、1 万次短流、并发 32。真要逼近一万并发，把 `RUPI_LOAD_SESSIONS` 和 `RUPI_LOAD_CONC` 都拉高（会打满本机 fd / 工作区，GitHub Actions 不要这么干）：

```bash
RUPI_LOAD_SESSIONS=1024 RUPI_LOAD_STREAMS=10000 RUPI_LOAD_CONC=128 \
  cargo test -p rupi-server --test cloud_chaos ten_thousand_streams -- --ignored --nocapture
```

池满排队（可选）：`RUPI_EXEC_ALLOC_QUEUE_MS=200` 让 execd 在 `release` 到来前等一会儿，超时仍 `429`。

## 未在本机验证

Windows `powershell` 本轮未做。CI 是 ubuntu / macos。

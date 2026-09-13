# rupi-tools

Pi 默认七件套 + 注册表。

| 工具 | 行为 |
|---|---|
| `read` | `offset`/`limit`；大文件截断标注；图片先 `metadata` |
| `write` | 写文件 |
| `edit` | 唯一匹配；`replace_all`；`edits[]`（兼容 `oldText`/`newText`）；回包 diff |
| `bash` | `timeout_secs`；输出保尾 2000 行 / 50KB；进程组 SIGKILL |
| `glob` / `grep` | 文件发现与搜索 |
| `think` | 思考块 |

`ToolRegistry::with_builtins` 无路径箍。`with_sandboxed_builtins` 把 read/write/edit/glob/grep 约束在启动 cwd（canonicalize；`..` / 绝对路径 / 符号链接逃逸拒绝）。本机 `bash` 仍是 `sh -c`。云路径不在本 crate 起 bash，见 `rupi-runtime`。

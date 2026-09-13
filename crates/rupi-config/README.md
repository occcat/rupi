# rupi-config

配置合并，非法文件只 warning，主循环不因配置崩。

1. `~/.rupi/settings.json`
2. 从 cwd 上溯的 `.rupi/settings.json`（后覆盖前）
3. 只读合并 `~/.pi/agent/settings.json` 与 `.pi/settings.json`（同键 rupi 优先）

核心键：`model` / `thinking` / `compaction` / `tools` / `theme` / `steeringMode` / `followUpMode` / `defaultProjectTrust` / `externalEditor` / `enabledModels`。`/settings` 热改并写回（有项目文件写项目，否则写家目录）。

也读 `SYSTEM.md` / `APPEND_SYSTEM.md`。

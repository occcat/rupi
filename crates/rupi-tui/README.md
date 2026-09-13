# rupi-tui

ratatui 聊天界面。`rupi` 无子命令时默认进这里。视图在 `view`（可单测），主循环在 `app`。

键位写在启动系统行：Enter 发送，Shift+Enter 换行，运行中 Enter 转向 / Alt+Enter 跟进，Esc 中止，Ctrl+L/P 模型，Shift+Tab 思考档，Ctrl+O/T 折叠，Ctrl+V 贴图，Ctrl+G 外编，`!cmd` / `!!cmd`，`/tree` 导航，`/sessions` 选择器。`~/.rupi/keybindings.json` 可改。

云控制面不链接本 crate。

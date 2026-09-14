# rupi-tui

ratatui 聊天界面。`rupi` 无子命令时默认进这里。视图在 `view`（可单测），主循环在 `app`。助手 Markdown 围栏有语法高亮；` ```mermaid ` 画成 Unicode；Kitty/Ghostty/WezTerm 下用图形协议内联图（`RUPI_KITTY=0` 关）。

键位写在启动系统行：Enter 发送，Shift+Enter 换行，运行中 Enter 转向 / Alt+Enter 跟进，Esc 中止，Ctrl+L/P 模型，Shift+Tab 思考档，Ctrl+O/T 折叠，Ctrl+V 贴图，Ctrl+E / `/edit` 进程内改工作区文件，Ctrl+G 外编输入框，`!cmd` / `!!cmd`，`/tree` 导航，`/sessions` 选择器。`~/.rupi/keybindings.json` 可改（`editor` 默认 Ctrl+E）。

云控制面不链接本 crate。

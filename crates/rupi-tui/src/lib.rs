//! rupi-tui: ratatui 聊天终端（对标 Pi 的 terminal UI）。
//! 纯视图逻辑在 `view`（可单测），终端主循环在 `app`。

pub mod app;
pub mod view;

pub use app::{launch, TuiContext, TurnRecord};

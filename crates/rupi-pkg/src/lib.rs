//! `rupi install git:/npm:`：拉取包并把 skills / commands / extensions 物化到 `~/.rupi`。
//!
//! 对标 Pi packages（`pi install npm:@foo/bar` / `git:host/user/repo`），但不执行
//! `npm install` 或包内脚本；TypeScript 扩展只计数跳过，rupi 只收 `*.json` manifest。

mod install;
mod layout;
mod spec;

pub use install::{
    format_report, install, list_installed, uninstall, InstallOpts, InstallReport, PackageRecord,
};
pub use layout::{discover, ResourceKind};
pub use spec::{parse_spec, PackageSource};

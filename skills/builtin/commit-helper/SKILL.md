---
name: commit-helper
description: git commit flow with checks and a typed message
---

# commit-helper

1. Run `git status --short` to see pending changes.
2. Run relevant checks (`cargo test` / `cargo clippy`) before committing.
3. Write the message as `<type>: <subject>` with a body listing files changed.

Move details to `references/` if this file grows past 500 lines.

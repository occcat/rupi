---
name: commit-helper
description: standarized git commit flow with checks and message format
---

# commit-helper

 distilled from a successful session. Follow these steps:

1. Run `git status --short` to see pending changes.
2. Run relevant checks (`cargo test` / `cargo clippy`) before committing.
3. Write message as `<type>: <subject>` with body listing files changed.

Move details to `references/` if this file grows past 500 lines.

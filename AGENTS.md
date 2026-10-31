- rupi is a Rust port of `@earendil-works/pi-coding-agent` 0.85.x. When behavior overlaps with Pi, match Pi's semantics and name the upstream counterpart. Record every deliberate difference, and every upstream feature that is not ported, in `docs/UPSTREAM.md`.
- Keep the crate graph directed and do not add cycles. `rupi-core` holds types and traits only, with no LLM calls and no I/O, and depends on no other rupi crate. Feature crates (`rupi-llm`, `rupi-tools`, `rupi-mcp`, `rupi-memory`, `rupi-skills`, `rupi-ext`, `rupi-config`, `rupi-pkg`, `rupi-runtime`) sit above core. `rupi-agent` composes them. `rupi-tui`, `rupi-cli`, and `rupi-server` are the front ends, and nothing depends on a front end except `rupi-cli`. Add a new edge between feature crates only when there is no smaller option.
- Every front end (TUI, `chat` REPL, `run`, `--mode rpc`, cloud `POST /v1/agent`) runs the same `AgentLoop`. Put new agent behavior in `rupi-agent`, not in a single front end, and give slash commands the same meaning in the TUI and the REPL.
- The cloud control plane (`rupi-server`) never runs user shell commands or local Docker, and never links `rupi-tui`. User commands go through the `Executor` trait to `rupi-execd` or `rupi-sandboxd`. PostgreSQL is the source of truth and Redis is only a cache.
- Keep security defaults strict: file tools stay inside the sandbox root, memory writes that look like secrets are refused, non-loopback listeners need TLS or an explicit `--insecure`, and executors refuse empty tokens off loopback. Never log `Authorization` headers or API keys.
- A provider error must not leave a broken turn in the session. Roll the turn back and surface the response body to the user.
- Keep `docs/LAUNCH.md` in sync with the real `rupi-server`, `rupi-execd`, and `rupi-sandboxd` flags; `crates/rupi-server/tests/launch_md.rs` enforces part of this. When a user-facing flag, command, or setting changes, update `README.md`, `README.zh-CN.md`, and the affected crate README in the same change.
- The default branch is `main`. Base branches and worktrees on the latest `origin/main` and target PRs at `main`.

## Branches and Worktrees

- Name work branches `thoxvi/<short-capability-summary>` in English kebab-case, for example `thoxvi/session-export-html`. Check that the name is not already used locally or on the remote before creating it.
- Create each work branch in its own worktree under `.worktree/` (git-ignored) so several branches can be worked on in parallel, and keep the main checkout on an up-to-date `main`.
- After a PR is merged, confirm the commits are on `main`, then delete the merged local and remote branches and their worktrees, and fast-forward the local `main`.

## Commits and PRs

- Commit at every small milestone on the work branch.
- Commit messages follow Conventional Commits, `type(scope): summary`, and the message text is written in Chinese. Valid types are `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`, `ci`, and `chore`. Use the crate name without the `rupi-` prefix as the scope when it helps, for example `fix(agent): ...` or `feat(tui): ...`.
- Every commit message is multi-line: a summary line, a blank line, then a body. Make the body as detailed as the change needs. It describes only what changed and why, not how it was tested or what comes next.
- PR titles use the same `type(scope): summary` format. The PR body is written in Chinese and covers the implementation plan, the test plan, the test process, the test results, and the conclusion.

## Style Guide

### General Principles

- Follow the existing structure of the crate you are changing. Do not add a new crate, module, or abstraction for a single caller.
- Do not extract single-use helpers. Inline the logic unless the helper is reused, hides a genuinely complex boundary, or has a name that makes the caller clearer.
- Validate untrusted input once at the boundary that owns it (CLI args, settings files, HTTP bodies, tool arguments, JSON-RPC messages) and pass typed values inward.
- Before adding complexity for an unlikely race or security edge case, explain the concrete failure mode, its likelihood, and the cost, and get agreement first.
- Delete dead code instead of keeping it behind `#[allow(dead_code)]`. Do not leave `todo!()` or `unimplemented!()` in non-test code.
- Prefer early returns, `let ... else`, and `?` over nested `match` or `if` chains.

### Errors

- Return `anyhow::Result` from application code and add context with `.context(...)` at I/O and process boundaries. Use typed errors only where a caller matches on the variant.
- Do not `unwrap()` or `expect()` in non-test code unless the invariant is local and obvious; leave a short comment when it is not.
- Tool failures become tool errors that the model sees. They must not abort the agent loop.

### Async and I/O

- Do not block the Tokio runtime. Wrap `std::fs` and `rusqlite` calls in `tokio::task::spawn_blocking`, or use `tokio::fs`, when they run inside async code on a hot path.
- Reuse the shared `reqwest::Client` from `rupi-llm` instead of building a new one per request.
- Child processes must be killable: put them in their own process group and honor the cancellation flag.

### Comments and Text

- Write code comments and doc comments in Chinese, matching the rest of the codebase. Keep identifiers, log messages meant for machines, and protocol field names in English.
- Comment non-obvious constraints, upstream references, and surprising behavior. Do not comment obvious assignments or control flow.
- Keep CLI help text, README sections, and docs short and specific. Name the real flag, path, or number instead of describing it.

## Testing

- Test the real implementation. Use `MockProvider`, the local TCP and HTTP stubs, and `crates/rupi-mcp/tests/fake_mcp_server.py` instead of hand-written mocks of internal types.
- Every bug fix gets a regression test that fails before the fix.
- Tests must not depend on the developer's home directory or real API keys. Set `RUPI_HOME` to a fresh temporary directory, and guard any test that changes process environment variables with a lock.
- `rupi-server` integration tests need PostgreSQL and Redis through `DATABASE_URL` and `REDIS_URL` and skip when they are absent. Run them before changing `rupi-server`, `rupi-runtime`, or the shared agent loop.
- MCP and extension tests need `python3` on `PATH`.

## Checks

- Before pushing, run the same checks as CI from the repository root:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked --exclude rupi-benches`
  - `cargo test --workspace --locked --exclude rupi-benches`
- Do not add new clippy warnings in code you touch.
- When dependencies change, commit the updated `Cargo.lock`, and run `cargo deny check licenses bans sources` and `cargo audit` if they are installed.
- The MSRV is Rust 1.88 (`rust-version` in the root `Cargo.toml`). Do not use language or library features newer than that.

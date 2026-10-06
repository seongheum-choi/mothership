# Working on mothership

mothership is a long-running service that other people's work depends on, so changes favour clarity and small surface area over cleverness.

## Standards

- **Style baseline:** the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for naming, conversions, documentation and type design, applied as far as they fit a binary crate.
- **Lints:** `cargo clippy --all-targets` with `clippy::pedantic` (configured in `Cargo.toml`) must stay at zero warnings. `scripts/check.sh` treats warnings as errors. Silence a lint only on the item that needs it, with `#[expect(clippy::..., reason = "...")]`.
- **Formatting:** `cargo fmt` with default settings.
- **Errors:** `anyhow::Result` with `.context(...)` at I/O and API boundaries. Log with `{e:#}` so the cause chain shows. Panics only for broken invariants, with an `expect` message that names the invariant.
- **Logging:** `tracing` macros, never `println!` or `eprintln!`. Prefix session-scoped lines with `[{key}]`.
- **Unsafe:** forbidden (`unsafe_code = "forbid"`).
- **Dependencies:** add one only when the standard library or an existing dependency can't do the job in a few lines.
- **Tests:** every branch-heavy pure function (parsers, mappings, path logic) has a unit test next to it. No mocking frameworks. Network and process boundaries are checked by running the binary.
- **Docs:** each module starts with a `//!` line saying what it is for. Comments explain why, not what.

## Layout

| Module | Responsibility |
| --- | --- |
| `main.rs` | Startup |
| `app.rs` | Shared state, HTTP router, agent environment |
| `config.rs` | Settings |
| `store.rs` | `state.json` and atomic private writes |
| `session.rs` | `Surface` trait, per-conversation workers, turn loop |
| `agent.rs` | Claude Code process and its stream-json protocol |
| `linear/`, `zulip/` | Surfaces: webhooks, API clients, how updates are shown |
| `worktree.rs` | Git worktrees per issue |
| `sandbox.rs` | Home directory read restrictions |
| `tunnel.rs`, `review.rs` | Extension points for ingress and review systems |

## Git

- Pull requests merge by rebase only; merge commits and squash merges are turned off, and a ruleset keeps `main` history linear.
- Bring a branch up to date with `git rebase origin/main`, never by merging `main` into it, then push with `git push --force-with-lease`.
- Keep each commit buildable and self-explanatory, since rebase merging keeps them all on `main`.

## Before committing

Run `scripts/check.sh`. The `.githooks/pre-commit` hook runs it when `core.hooksPath` points at `.githooks`.

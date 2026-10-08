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
| `config/` | Settings: `Config`, its loading and derived paths (`mod.rs`), the environment over `<home>/.env` and list formats (`vars.rs`), per-surface settings (`surfaces.rs`) |
| `repos/` | `repos.json`, and which repository a Linear issue belongs to |
| `modes.rs` | `<home>/modes/*.md`, and which mode a Linear issue's labels pick |
| `store.rs` | `state.json` and atomic private writes |
| `signature.rs` | Webhook signatures: HMAC-SHA256 hex, constant-time compare, strict hex decode |
| `session/` | `Surface` trait, per-conversation workers, turn loop |
| `agent.rs` | Claude Code process and its stream-json protocol |
| `linear/` | Linear surface: `Surface` impl and routes (`mod.rs`), webhook intake (`webhook.rs`), workspace pinning (`pin.rs`), repository and mode choice (`routing.rs`), closed-issue cleanup (`cleanup.rs`), activity rendering (`activity.rs`), OAuth install (`oauth.rs`), GraphQL and token refresh (`api.rs`), pull request links on the session (`pr.rs`), Linear MCP config (`mcp.rs`) |
| `zulip/` | Zulip surface: webhook intake and the `Ticket` (`mod.rs`), conversation keys, topic context and mention stripping (`conversation.rs`), agent launch and posting the answer (`surface.rs`), REST client (`api.rs`) |
| `github/` | GitHub surface: `GitHub` state and webhook intake (`mod.rs`), what an event says and its prompt (`feedback.rs`), which session it continues (`route.rs`), delivery dedup and hourly prompt cap (`guard.rs`), `gh-reply` and origin resolution (`reply.rs`) |
| `sessions.rs` | Session lookups shared by surfaces: a pre-routing session's repository, the session that owns a branch |
| `worktree.rs` | Git worktrees per issue |
| `sandbox.rs` | Home directory read restrictions |
| `tunnel.rs`, `review.rs` | Extension points for ingress and review systems |

## Git

- Pull requests merge by rebase only; merge commits and squash merges are turned off, and a ruleset keeps `main` history linear.
- Bring a branch up to date with `git rebase origin/main`, never by merging `main` into it, then push with `git push --force-with-lease`.
- Keep each commit buildable and self-explanatory, since rebase merging keeps them all on `main`.

## Before committing

Run `scripts/check.sh`. The `.githooks/pre-commit` hook runs it when `core.hooksPath` points at `.githooks`.

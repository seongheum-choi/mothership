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
- **Language:** everything in this repository is in English: code comments, docs, commit messages, and pull request titles and bodies. Linear issues may be written in Korean; translate an issue's title when it becomes a pull request title, keeping the key (`SH-123: ...`).

## Layout

One file, one responsibility. A file's logic, everything before `#[cfg(test)]` minus blank and `//` lines, stays at 200 lines or fewer; when it grows past that, split it along responsibilities rather than trimming lines. `scripts/check.sh` enforces this.

| Module | Responsibility |
| --- | --- |
| `main.rs` | Startup and signals: SIGTERM shuts down, SIGUSR1 and SIGUSR2 start and end a drain for a restart |
| `app/` | Shared state, whether the instance drains, and the HTTP router (`mod.rs`), `/status` and the running turns it lists (`status.rs`), what each agent process gets: the `Launch` a surface's `Place` becomes, its environment, its settings and the host settings beside them, its session's directory (`agent.rs`), its temporary directory: where it is, short enough for the sandbox's sockets and Claude Code's `TMPDIR`, made private, and the rules keeping other sessions out (`tmp.rs`) |
| `host/` | Runs mothership starts for a session that its sandbox cannot: watching the session's directory for requests, a run's environment and profile (`mod.rs`), Codex runs, the confined command and environment a Codex runner turn starts with, what they may write in Codex's home and each session's own part of it (`codex.rs`), the repository's named commands and their prompt (`command.rs`), the confined process, its directory, output and process group (`process.rs`), request and answer files read and written without following links (`request.rs`), the Seatbelt profile made from the session's settings (`seatbelt.rs`), the bubblewrap mounts made from the same (`bwrap.rs`), the seccomp filter that refuses a bubblewrap run Unix sockets (`seccomp.rs`) |
| `config/` | Settings: `Config`, its loading and derived paths (`mod.rs`), the environment over `<home>/.env` and list formats (`vars.rs`), per-surface settings, the GitHub App and whether a caller is on a trusted list (`surfaces.rs`), the runner, model, fallback and effort defaults and the fields a narrower scope may set (`model.rs`), the repository list, runner and model defaults reloaded when `repos.json` or `.env` changes (`reload.rs`) |
| `repos/` | Repositories: `Repo` and lookups by name or clone (`mod.rs`), reading and validating `<home>/repos.json` (`load.rs`), each entry's directory, clone and files on disk (`disk.rs`), which repository a Linear issue belongs to (`select.rs`) |
| `modes/` | Working modes: `Mode`, loading `<home>/modes/*.md` and which mode a Linear issue's labels pick (`mod.rs`), the mode file's frontmatter and its YAML subset (`frontmatter.rs`) |
| `directive.rs` | `[key=value]` directives in issue text and messages |
| `store.rs` | `state.json` and atomic private writes |
| `signature.rs` | Webhook signatures: HMAC-SHA256 hex, constant-time compare, strict hex decode |
| `session/` | Conversations: `Surface` trait, updates and the worker `Registry`, which holds prompts while the instance drains and tracks each running turn for `/status` (`mod.rs`), one conversation's worker and its turn loop, moving a turn to Codex at Claude's usage limit (`turn.rs`), one agent process of a turn (`process.rs`), what the service log says about each turn: what it runs and how it ended, through which every turn's outcome reaches the surface (`log.rs`), the background tasks it still runs when it answers and how long it may keep running for them (`background.rs`), a turn's runner, model, fallback model and effort from the session's messages, its mode, its repository and the instance defaults, and the usage limit switch (`choice/mod.rs`), each scope's candidate and the line naming the sources (`choice/scope.rs`), the `[runner=…]`/`[model=…]`/`[effort=…]` directives (`choice/directives.rs`), the issue's `runner:`/`model:`/`effort:` labels (`choice/labels.rs`), agent events turned into surface updates and the progress file tail (`relay.rs`) |
| `agent/` | Agent runtimes: the process, `Launch` and runner-neutral `Event`s (`mod.rs`), Claude Code and its stream-json protocol (`claude.rs`), the Codex CLI and its JSONL events (`codex.rs`), when Claude Code's usage limit moves turns to Codex (`limit.rs`) |
| `linear/` | Linear surface: `Surface` impl and routes (`mod.rs`), webhook intake (`webhook.rs`), starting or holding a turn, and running the prompts held for a restart (`start.rs`), workspace pinning (`pin.rs`), repository and mode choice (`routing.rs`), the repository of a session from before routing (`prerouting.rs`), the issue's worktree (`worktree.rs`), closed-issue cleanup (`cleanup.rs`), activity rendering (`activity.rs`), OAuth install (`oauth.rs`), GraphQL and token refresh (`api.rs`), pull request and Gerrit change links on the session (`pr.rs`), sessions mothership opens itself for a pull request (`create.rs`), Linear MCP config (`mcp.rs`) |
| `zulip/` | Zulip surface: webhook intake and the `Ticket` (`mod.rs`), conversation keys, topic context and mention stripping (`conversation.rs`), agent launch and posting the answer (`surface.rs`), REST client (`api.rs`) |
| `github/` | GitHub surface: `GitHub` state and webhook intake (`mod.rs`), what an event says (`feedback.rs`), the prompt that quotes it (`prompt.rs`), which session it continues (`route.rs`), the issue and new session for a pull request no session owns, or the reply that there is none (`open.rs`), delivery dedup (`guard.rs`), `gh-reply` and origin resolution (`reply.rs`), the GitHub App user token agents get as `GH_TOKEN` and its refresh (`user_token.rs`), its device flow login at startup (`device.rs`) |
| `gerrit/` | Gerrit review: `Gerrit` state, webhook intake, and what agents get to push and answer, its URL, credentials and git credential helper (`mod.rs`), what a `comment-added` event says and whether it is heard (`event.rs`), which repositories a project is (`project.rs`), the review's message and comments read over REST (`api.rs`), whether a review is heard and which session its topic continues (`route.rs`), the prompt quoting the review (`prompt.rs`), `gerrit-reply` and its review tag (`reply.rs`) |
| `sessions.rs` | Session lookups shared by surfaces: a pre-routing session's repository, the session that owns a pull request or a Gerrit topic |
| `worktree.rs` | Git worktrees per issue |
| `sandbox.rs` | Home directory read restrictions, `MOTHERSHIP_HOME` edit restrictions and Bash's network allowlist |
| `review/` | Review systems: `ReviewBackend` and the delivery instructions it gives agents (`mod.rs`), review feedback continuing a Linear session, with its hourly prompt cap per session (`intake.rs`) |
| `tunnel.rs` | Extension point for ingress |

## How a prompt becomes a turn

Read this path first when a change or a bug touches more than one module.

1. **Intake.** A webhook (`linear/webhook.rs`, `zulip/mod.rs` with `zulip/conversation.rs`) settles which conversation it belongs to; Linear also picks the session's repository and mode (`linear/routing.rs`) before `linear/start.rs` submits. GitHub and Gerrit feedback finds the Linear session that owns its pull request or topic (`sessions.rs`) and goes through `review::intake::submit`, which applies the hourly cap; a GitHub mention on a pull request no session owns opens a session for it instead (`github/open.rs`).
2. **Queue.** `session::Registry::submit` hands the prompt to the conversation's worker, starting one if none runs, or holds it while the instance drains. A prompt that arrives mid-turn is fed to the running agent.
3. **Launch.** `session/turn.rs` asks the surface for a `Launch`; each surface fills an `app::Place` with what is its own (workspace, system prompt, readable paths, deny rules: the mode's, or chat's read-only repositories) and `App::launch` adds what every agent gets (runner and models from `session/choice`, sandbox settings, MCP configs, plugins, environment). The turn takes the workspace lock and starts the host run watcher (`host/`).
4. **Run.** `session/process.rs` spawns the agent (`agent/`), turns its events into surface updates (`session/relay.rs`), and moves a Claude turn to Codex at the usage limit.
5. **Finish.** Every turn's outcome goes through `Turn::finish` (`session/log.rs`), which logs it and reports it to the surface; the surface settles a model a message chose (`session::choice::settle`), and Linear links the pull requests the turn mentioned.

To follow one session in the service log, search for its key in brackets (`[<Linear session id>]`, `[zulip:…]`): Linear's intake line names the issue, and each turn logs `turn on <issue>: <runner> <model> in <workspace>, …` and how it ended with its length. `GET /status` lists the turns running now; `<home>/progress/<key as a file name>/progress.log` holds what the agent's tools reported.

## Git

- Pull requests merge by rebase only; merge commits and squash merges are turned off, and a ruleset keeps `main` history linear.
- Bring a branch up to date with `git rebase origin/main`, never by merging `main` into it, then push with `git push --force-with-lease`.
- Keep each commit buildable and self-explanatory, since rebase merging keeps them all on `main`.

## Before committing

Run `scripts/check.sh`. The `.githooks/pre-commit` hook runs it when `core.hooksPath` points at `.githooks`.

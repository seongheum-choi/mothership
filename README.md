# mothership

An agent deployer. mothership runs Claude Code agents for Linear agent sessions and Zulip conversations. Each Linear issue gets its own git worktree, and the agent's progress goes back to wherever the request came from.

Inspired by [Cyrus](https://github.com/cyrusagents/cyrus) (Apache-2.0); the home directory read restrictions follow its approach. This is an independent implementation in Rust.

Licensed under the [Apache License 2.0](LICENSE).

## How it works

| Surface | Request | Workspace | What the requester sees |
| --- | --- | --- | --- |
| Linear | Agent session created on an issue (delegation or @mention), or a prompt in it | `<WORKTREES_DIR>/<ISSUE-ID>` on Linear's `branchName` | Agent activities: thoughts, tool actions, progress lines, final response |
| Zulip | @mention in a channel, or a direct message to the outgoing-webhook bot | `<home>/zulip-workspaces/<thread>`; the repo is read-only | :eyes: on receipt, then the reply and :check: |

- **One conversation, one worker:** turns run back to back, and each one resumes the same Claude session (`--resume`).
- **Prompts during a turn:** a prompt that arrives while the agent works goes straight into the running process (`--input-format stream-json`), and Claude folds it into the current turn. Input closes at the turn's result, so anything later starts the next turn.
- **Stop:** a Linear stop signal kills the agent's whole process group.
- **Worktrees:** a new branch is cut from `origin/<BASE_BRANCH>` with no upstream, so a bare `git push` never targets the base branch. An existing local or remote branch is continued, and an existing checkout is reused. Only one agent runs per worktree at a time.
- **Home directory:** agents may read only their workspace, the main clone, and plugin directories. Every other entry under `$HOME` gets a `Read` deny rule. Claude Code applies deny rules under `bypassPermissions` too, and checks reading shell commands against them.
- **Progress:** every agent process gets `MOTHERSHIP_PROGRESS_FILE`. Lines that tools or skills append to it, from any depth of the process tree, show up in Linear as thoughts. Long-running skill workflows use it to report their phases.
- **Environment:** `MOTHERSHIP_AGENT` (the `AGENT_NAME` setting) and `MOTHERSHIP_SURFACE` (`linear` or `zulip`) let hooks and skills tell where they run. The keys named in `AGENT_ENV` are forwarded on top, so secrets like `CLAUDE_CODE_OAUTH_TOKEN` can live only in `<home>/.env` instead of the daemon config.

Agents run as `claude -p --input-format stream-json --output-format stream-json`. The `--settings` deny rules, `--mcp-config` (Linear's hosted MCP with the app token, plus `MCP_CONFIGS`), and `--plugin-dir` for each plugin are generated per turn.

## Extending

| To add | Where |
| --- | --- |
| A chat or tracker (Slack, GitHub comments) | Implement `session::Surface` and add its routes in `app.rs` |
| A tunnel (ngrok, ...) | A `tunnel::Tunnel` variant and its command |
| A review system (Gerrit) | A `review::ReviewBackend` variant |
| An agent runtime (Codex) | A runner that turns its output into `agent::Event`s |
| Skills for every session | Drop a Claude Code plugin directory into `<home>/plugins/`; it loads on the next turn |

## Settings

Settings come from the process environment or `<home>/.env`, and the environment wins. `<home>` is `~/.mothership` unless `MOTHERSHIP_HOME` says otherwise.

| Key | Default | |
| --- | --- | --- |
| `BASE_URL` | required | Public URL; the OAuth redirect is `<BASE_URL>/callback` |
| `LINEAR_CLIENT_ID`, `LINEAR_CLIENT_SECRET`, `LINEAR_WEBHOOK_SECRET` | required | |
| `REPO_PATH` | required | Main clone that worktrees are cut from |
| `BIND` | `127.0.0.1:3456` | |
| `AGENT_NAME` | `mothership` | Exported as `MOTHERSHIP_AGENT` |
| `AGENT_ENV` | | Comma-separated keys whose values (from the environment or `.env`) are forwarded into every agent process, e.g. `CLAUDE_CODE_OAUTH_TOKEN`; a listed key with no value is warned about at startup |
| `BASE_BRANCH` | `main` | |
| `WORKTREES_DIR` | `<home>/worktrees` | |
| `CLAUDE_BIN`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` | `claude`, `opus`, `sonnet` | On macOS with the native installer, point `CLAUDE_BIN` at `~/.local/share/claude/ClaudeCode.app/Contents/MacOS/claude`; launched through `~/.local/bin/claude`, privacy prompts name a version number ("2.1.x") that changes with every update |
| `CHAT_PERMISSION_MODE` | `auto` | Issue sessions always use `bypassPermissions` |
| `MCP_CONFIGS` | | Comma-separated extra MCP config files |
| `REVIEW_BACKEND` | `github` | |
| `APPEND_SYSTEM_PROMPT_FILE` | | Replaces the review backend's instructions in issue sessions |
| `ZULIP_SITE`, `ZULIP_BOT_EMAIL`, `ZULIP_API_KEY`, `ZULIP_WEBHOOK_TOKEN` | | Turn on Zulip (`POST /zulip-webhook`) |
| `CLOUDFLARE_TOKEN`, `CLOUDFLARED_BIN` | | Supervise a remotely-managed Cloudflare tunnel |
| `LINEAR_ACCESS_TOKEN`, `LINEAR_REFRESH_TOKEN` | | Seed tokens, used only while `state.json` has none |

State (Linear tokens, and each conversation's workspace and Claude session id) is kept in `<home>/state.json` with mode 0600.

## Linear auth

Open `<BASE_URL>/oauth/authorize` to install the app with `actor=app`. It works only while no token is stored, because the endpoint is public. To install again, clear `linear` in `state.json` and restart. Tokens refresh by themselves when Linear rejects them.

## Tunnel

Any tunnel that forwards to `BIND` works. With `CLOUDFLARE_TOKEN` set, mothership runs `cloudflared tunnel run` itself and restarts it 5 seconds after it exits. The tunnel's hostname → `http://localhost:<port>` route is configured in the Cloudflare dashboard.

## Develop

See [AGENTS.md](AGENTS.md) for the code standards.

```sh
mise install          # Rust 1.99 (mise.toml)
scripts/check.sh      # fmt, clippy (pedantic, warnings are errors), tests
git config core.hooksPath .githooks   # run the check before every commit
cargo run
```

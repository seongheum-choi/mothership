# mothership

A minimal Linear agent that replaces Cyrus. A Linear agent session gets a git worktree and a `claude -p` run in it, and the run's output is relayed back to the session as agent activities.

Stage 1 scope: Linear webhooks, a single repository, the Claude runner, and an optional Cloudflare tunnel. GitHub, Zulip, Codex, and Gerrit come later.

## Flow

1. `POST /linear-webhook` (or `/webhook`, the path Cyrus-era Linear apps use) checks the `Linear-Signature` HMAC and `webhookTimestamp`, answers 200 straight away, and handles the event in the background.
2. `AgentSessionEvent/created` and `prompted` queue a prompt for that session. A `stop` signal kills the running turn.
3. Each turn posts an ephemeral "Working on it…" thought, then sets up `<WORKTREES_DIR>/<ISSUE-ID>` on Linear's `branchName`, cut from `origin/<BASE_BRANCH>`. It then runs:
   `claude -p --output-format stream-json --verbose --permission-mode bypassPermissions --strict-mcp-config --mcp-config <linear MCP> [MCP_CONFIGS] --append-system-prompt … [--resume <id>]`
4. Assistant text is posted as `thought`, a tool call as an ephemeral `action`, `TodoWrite` as a checklist `thought`, and the result as `response` or `error`.
5. Prompts that arrive during a turn are combined into the next turn, which resumes the same Claude session.

State (Linear OAuth tokens and session → worktree/Claude session id) is kept in `<MOTHERSHIP_HOME>/state.json` with mode 0600.

## Settings

Settings come from the process environment or `~/.mothership/.env` (`MOTHERSHIP_HOME` overrides the directory). The process environment wins.

| Key | Required | Default |
| --- | --- | --- |
| `BASE_URL` | yes | public URL; the OAuth redirect is `<BASE_URL>/callback` |
| `LINEAR_CLIENT_ID`, `LINEAR_CLIENT_SECRET`, `LINEAR_WEBHOOK_SECRET` | yes | |
| `REPO_PATH` | yes | main clone that worktrees are cut from |
| `BIND` | | `127.0.0.1:3456` |
| `BASE_BRANCH` | | `main` |
| `WORKTREES_DIR` | | `<MOTHERSHIP_HOME>/worktrees` |
| `CLAUDE_BIN`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` | | `claude`, `opus`, `sonnet` |
| `MCP_CONFIGS` | | comma-separated extra MCP config files |
| `APPEND_SYSTEM_PROMPT_FILE` | | replaces the default "commit, push, open a PR" instructions |
| `CLOUDFLARE_TOKEN` | | runs `cloudflared tunnel run` with this remotely-managed tunnel token |
| `CLOUDFLARED_BIN` | | `cloudflared` |
| `LINEAR_ACCESS_TOKEN`, `LINEAR_REFRESH_TOKEN` | | seed tokens, used only when `state.json` has none |

## Linear auth

Open `<BASE_URL>/oauth/authorize` to install the app with `actor=app`. To move over from Cyrus without installing again, copy `linearToken` and `linearRefreshToken` from `~/.cyrus/config.json` into the seed variables. Tokens refresh by themselves on a 401.

## Tunnel

mothership has no tunnel code apart from the Cloudflare one. ngrok, or a `cloudflared` you run yourself, only needs to forward to `BIND`. With `CLOUDFLARE_TOKEN` set, mothership supervises `cloudflared` itself and restarts it 5 seconds after it exits. The hostname → `http://localhost:<port>` route is configured in the Cloudflare dashboard, the same contract as Cyrus' `CLOUDFLARE_TOKEN`.

## Develop

```sh
mise install   # Rust 1.99 (mise.toml)
cargo test
cargo run
```

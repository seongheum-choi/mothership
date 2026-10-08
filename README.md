# mothership

An agent deployer. mothership runs Claude Code agents for Linear agent sessions and Zulip conversations, across one or more repositories. Each Linear issue gets its own git worktree, and the agent's progress goes back to wherever the request came from.

Inspired by [Cyrus](https://github.com/cyrusagents/cyrus) (Apache-2.0); the home directory read restrictions follow its approach. This is an independent implementation in Rust.

Licensed under the [Apache License 2.0](LICENSE).

## How it works

| Surface | Request | Workspace | What the requester sees |
| --- | --- | --- | --- |
| Linear | Agent session created on an issue (delegation or @mention), or a prompt in it | `<WORKTREES_DIR>/<ISSUE-ID>` on Linear's `branchName`, cut from the issue's [repository](#repositories); a non-git repository's own directory | Agent activities: which repository and why, thoughts, tool actions, progress lines, final response |
| GitHub | A review, review comment or PR comment on a branch a Linear session works on | That session's worktree | The Linear session continues; the agent answers on GitHub with `gh` |
| Zulip | @mention in a channel, or a direct message to the outgoing-webhook bot | `<home>/zulip-workspaces/<thread>`; every repository is read-only | :eyes: on receipt, then the reply and :check: |

- **One conversation, one worker:** turns run back to back, and each one resumes the same Claude session (`--resume`).
- **Prompts during a turn:** a prompt that arrives while the agent works goes straight into the running process (`--input-format stream-json`), and Claude folds it into the current turn. Input closes at the turn's result, so anything later starts the next turn.
- **PR feedback:** `POST /github-webhook` takes the GitHub App's `pull_request_review` (submitted, with text), `pull_request_review_comment` (created) and `issue_comment` (created, on a PR) events, checked against `X-Hub-Signature-256`. Only comments and reviews whose `author_association` is `OWNER`, or whose author is in `GITHUB_TRUSTED_LOGINS`, are heard, only when their text mentions the agent's GitHub account (`@<GITHUB_MENTION_LOGIN>`, in any letter case), and only on same-repository PRs in the origins of the git repositories in `repos.json`; a repeated `X-GitHub-Delivery` id is answered 200 and dropped. The PR's head branch picks the newest Linear session recorded on that branch, or on the branch it is stacked on (`en-593-3` → `en-593`). That session gets the author, file and lines, URLs and the text quoted in a `<github_comment>` block as a prompt, and posts its results in Linear as usual. The agent replies with `gh-reply` (installed in its own `<home>/gh-reply` directory and put first on agents' `PATH`; it refuses an empty reply), which appends `<!-- mothership -->`; text with that marker is dropped, as are comments by bots (`type: Bot`) and feedback on branches no session knows. A session that gets 10 GitHub prompts within an hour is stopped and told so in Linear, and further GitHub feedback for it waits for the hour to pass. `issue_comment` events carry no branch, so the server looks the PR up with `gh api`. With `GITHUB_WEBHOOK_SECRET` set, startup fails when no git repository has a GitHub origin or `<home>/gh-reply` cannot be written.
- **Pull request links:** when a Linear turn ends, every GitHub pull request URL (`https://github.com/<owner>/<repo>/pull/<n>`) in its final response or interim thoughts, with fragment and query dropped, is added to the session's external URLs (`agentSessionUpdate` `addedExternalUrls`, labelled `<owner>/<repo>#<n>`), so the session shows the link and Linear can tie the session to the pull request once it is synced. Tool inputs are not searched, since they name pull requests the agent reads as often as ones it opens. Each URL is sent once per session and recorded in `state.json`; a failed update is logged, does not affect the turn, and is retried at the next turn that mentions the URL.
- **Stop:** a Linear stop signal kills the agent's whole process group.
- **Closed issues:** when an issue moves to a completed or canceled state or is deleted, its running sessions stop and its worktree under `WORKTREES_DIR` is removed from the session's own repository, ignored files such as `.env` or `build/` included; the branch stays, and a worktree with uncommitted changes is kept with a warning. A worktree that an open session of another issue records (a stacked branch) or that a new prompt reopened meanwhile stays. A non-git repository's directory is never removed. GitHub feedback on a closed session's PR is logged and ignored until the session is prompted again in Linear. When the app is unassigned or undelegated, running sessions stop with a note and the worktree stays. This needs the Linear app's `Issue` data-change and app notification webhooks.
- **Worktrees:** a new branch is cut from `origin/<BASE_BRANCH>` with no upstream, so a bare `git push` never targets the base branch. An existing local or remote branch is continued, and an existing checkout is reused. Only one agent runs per worktree at a time.
- **Home directory:** agents may read only their workspace, their repository's main clone (every repository's, in Zulip), and plugin directories. Every other entry under `$HOME` gets a `Read` deny rule, except the paths in `SANDBOX_READ` and `SANDBOX_WRITE` and the progress directory. Claude Code applies deny rules under `bypassPermissions` too.
- **Bash sandbox:** Bash runs in Claude Code's sandbox (Seatbelt on macOS, bubblewrap on Linux), which enforces the deny rules on every process it starts and allows writes only to the working directory, `SANDBOX_WRITE` and `<home>/progress`. Commands cannot opt out of it, and `CLAUDE_CODE_OAUTH_TOKEN` is hidden from Bash. Its limits:
  - Without Seatbelt (macOS) or bubblewrap and socat (Linux), the session fails (`failIfUnavailable`) rather than run unconfined.
  - The sandbox judges a symlink by its target, so `SANDBOX_READ` and `SANDBOX_WRITE` list each path's resolved target next to it, as read at startup.
  - The Read, Edit and Write tools, hooks and MCP servers run outside it; only the deny rules bind them, and nothing stops the Edit and Write tools from writing in `$HOME`.
  - The other `AGENT_ENV` values, such as `GH_TOKEN`, are visible to Bash.
  - Under `bypassPermissions` the network stays open: direct connections are blocked, but everything through the sandbox's proxy is allowed.
  - `gh` cannot reach the keyring or read `~/.config/gh`, so it needs `GH_TOKEN` in `AGENT_ENV`.
- **Progress:** every agent process gets `MOTHERSHIP_PROGRESS_FILE`. Lines that tools or skills append to it, from any depth of the process tree, show up in Linear as thoughts. Long-running skill workflows use it to report their phases.
- **Environment:** `MOTHERSHIP_AGENT` (the `AGENT_NAME` setting) and `MOTHERSHIP_SURFACE` (`linear` or `zulip`) let hooks and skills tell where they run. The keys named in `AGENT_ENV` are forwarded on top, so secrets like `CLAUDE_CODE_OAUTH_TOKEN` can live only in `<home>/.env` instead of the daemon config.

Agents run as `claude -p --input-format stream-json --output-format stream-json`. The `--settings` deny rules, `--mcp-config` (Linear's hosted MCP with the app token, plus `MCP_CONFIGS`), and `--plugin-dir` for each plugin are generated per turn.

## Extending

| To add | Where |
| --- | --- |
| A chat or tracker (Slack, GitHub issues) | Implement `session::Surface` and add its routes in `app.rs` |
| A tunnel (ngrok, ...) | A `tunnel::Tunnel` variant and its command |
| A review system (Gerrit) | A `review::ReviewBackend` variant |
| An agent runtime (Codex) | A runner that turns its output into `agent::Event`s |
| Skills for every session | Drop a Claude Code plugin directory into `<home>/plugins/`; it loads on the next turn |
| A working mode picked by Linear labels | A [`<home>/modes/<name>.md`](#modes) file |

### Bundled plugins

`plugins/` holds plugins that ship with mothership. None load until installed, by copying or symlinking the directory into `<home>/plugins/`:

```sh
mkdir -p ~/.mothership/plugins
ln -s "$PWD/plugins/unshipped-work" ~/.mothership/plugins/   # run from the mothership checkout
```

| Plugin | What it does |
| --- | --- |
| `unshipped-work` | Stop hook. In an issue worktree with uncommitted tracked changes or commits no remote has, the agent's first Stop is sent back to commit and push, or to say why the work stays unshipped. The second Stop always passes. Directories that are not linked worktrees, such as Zulip workspaces, are left alone. |

## Settings

Settings come from the process environment or `<home>/.env`, and the environment wins. `<home>` is `~/.mothership` unless `MOTHERSHIP_HOME` says otherwise.

| Key | Default | |
| --- | --- | --- |
| `BASE_URL` | required | Public URL; the OAuth redirect is `<BASE_URL>/callback` |
| `LINEAR_CLIENT_ID`, `LINEAR_CLIENT_SECRET`, `LINEAR_WEBHOOK_SECRET` | required | |
| `REPO_PATH` | required without `repos.json` | Main clone that worktrees are cut from; ignored when [`<home>/repos.json`](#repositories) exists |
| `LINEAR_WORKSPACE` | recommended | The Linear workspace (URL key or ID) this instance serves; startup, `/callback` and token refresh refuse a token from any other. Unset, the first workspace to install the app is pinned |
| `BIND` | `127.0.0.1:3456` | |
| `AGENT_NAME` | `mothership` | Exported as `MOTHERSHIP_AGENT` |
| `AGENT_ENV` | | Comma-separated keys whose values (from the environment or `.env`) are forwarded into every agent process, e.g. `CLAUDE_CODE_OAUTH_TOKEN`; a listed key with no value is warned about at startup |
| `SANDBOX_READ` | | Comma-separated home paths (files or directories, `~/` allowed) every agent may read besides its workspace and repositories, e.g. `~/.gitconfig,~/.rustup,~/.cargo`. Symlink targets are added; home itself is refused at startup. Without it, git config and toolchains under `$HOME` are unreadable and `git commit` and `cargo` fail Git also needs the targets of any `[include]` in `~/.gitconfig`, and a mise-managed toolchain needs `~/.config/mise` and `~/.local/share/mise` here plus `~/.local/state/mise` and `~/.cache/mise` in `SANDBOX_WRITE`. |
| `SANDBOX_WRITE` | | Comma-separated paths Bash may write, and every agent may read, besides the working directory, e.g. `~/.cargo/registry,~/.cargo/git`. Symlink targets are added; home itself is refused at startup |
| `BASE_BRANCH` | `main` | Ignored when `repos.json` exists |
| `WORKTREES_DIR` | `<home>/worktrees` | |
| `CLAUDE_BIN`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` | `claude`, `opus`, `sonnet` | On macOS with the native installer, point `CLAUDE_BIN` at `~/.local/share/claude/ClaudeCode.app/Contents/MacOS/claude`; launched through `~/.local/bin/claude`, privacy prompts name a version number ("2.1.x") that changes with every update |
| `CHAT_PERMISSION_MODE` | `auto` | Issue sessions always use `bypassPermissions` |
| `MCP_CONFIGS` | | Comma-separated extra MCP config files, for every repository |
| `REVIEW_BACKEND` | `github` | |
| `APPEND_SYSTEM_PROMPT_FILE` | | Replaces the review backend's instructions in issue sessions of git repositories without their own `prompt_file` |
| `GITHUB_WEBHOOK_SECRET` | | Turns on GitHub PR feedback (`POST /github-webhook`); the GitHub App's webhook secret |
| `GITHUB_MENTION_LOGIN` | required with `GITHUB_WEBHOOK_SECRET` | The agent's GitHub login (`alean-impala`); PR feedback that does not mention `@<login>` is ignored |
| `GITHUB_TRUSTED_LOGINS` | | Comma-separated GitHub logins heard besides the repository owner. On an organisation repository GitHub reports even the owner's own account as `MEMBER`, so list it here |
| `ZULIP_SITE`, `ZULIP_BOT_EMAIL`, `ZULIP_API_KEY`, `ZULIP_WEBHOOK_TOKEN` | | Turn on Zulip (`POST /zulip-webhook`) |
| `CLOUDFLARE_TOKEN`, `CLOUDFLARED_BIN` | | Supervise a remotely-managed Cloudflare tunnel |
| `LINEAR_ACCESS_TOKEN`, `LINEAR_REFRESH_TOKEN` | | Seed tokens, used only while `state.json` has none |

State (Linear tokens, and each conversation's repository, workspace and Claude session id) is kept in `<home>/state.json` with mode 0600.

### Repositories

`<home>/repos.json` lists the repositories one mothership works on. Without it, `REPO_PATH` and `BASE_BRANCH` describe a single repository, named after its directory. One mothership serves one Linear workspace, so each instance (each `MOTHERSHIP_HOME`) has its own `repos.json`. The file is read at startup, and a mistake in it stops startup: overlapping or nested paths, a `git` entry whose path is not a git repository, a missing `prompt_file` or `mcp_configs` file.

```json
[
  {
    "name": "mothership",
    "path": "~/src/mothership",
    "base_branch": "main",
    "labels": ["backend"],
    "linear_teams": ["EN"],
    "linear_projects": ["Mothership"],
    "prompt_file": "~/.mothership/prompts/mothership.md",
    "mcp_configs": ["~/.mothership/mcp/github.json"]
  },
  { "name": "vault", "path": "~/notes/vault", "git": false, "linear_projects": ["Notes"] }
]
```

| Field | Default | |
| --- | --- | --- |
| `name` | required | Letters, digits, `-`, `_`, `.`; unique |
| `path` | required | Main clone (absolute or `~/`), or the directory a non-git repository works in |
| `base_branch` | `main` | New issue branches start from `origin/<base_branch>`; not allowed with `"git": false` |
| `labels`, `linear_teams`, `linear_projects` | | Route issues here. Each value matches a name (case-insensitive) or an id, and a project's slug or a team's key (`EN`). No value may be claimed by two repositories |
| `prompt_file` | | Replaces the review backend's instructions (and `APPEND_SYSTEM_PROMPT_FILE`); in a non-git repository it follows the no-commit instructions |
| `mcp_configs` | | MCP config files added for sessions here, after `MCP_CONFIGS` |
| `git` | `true` | `false`: no worktree; sessions work in `path` directly, and only one turn runs there at a time while other sessions' turns wait; they are told not to commit or open pull requests |

A Linear session picks its repository when it starts and keeps it for every later turn. The first rule that matches decides:

1. `[repo=<name>]` in the @mention or reply, then in the issue description
2. A label: `repo:<name>`, or one listed in `labels`
3. The issue's project
4. The issue's team

With a single repository none of this is read: every session works there, as before `repos.json`.

The project, team and labels come from a GraphQL lookup, because webhook payloads carry none of them. The session's first thought names the repository and the rule. If no rule decides, a `[repo=…]` or `repo:` label names an unknown repository, the lookup fails, or one rule points at two repositories, the session asks and waits. A reply carrying `[repo=<name>]` starts the work, with the issue context it was created with; it is read before the description, so it also corrects a wrong `[repo=…]` there.

A session that started before `repos.json` keeps the repository its worktree was cut from. A session whose repository has been removed from `repos.json` stops with an error instead of starting over somewhere else.

### Modes

A mode changes how a Linear session works: `<home>/modes/<name>.md` holds instructions for the system prompt, with optional YAML frontmatter. `contrib/modes/` has examples (`implement`, `research`, `debug`); copy or symlink the ones you want into `<home>/modes/`.

```markdown
---
labels: [Research]
model: opus
permission_mode: dontAsk
deny:
  - Edit
  - Bash(git push:*)
---
Answer the question in the issue; do not change anything.
```

| Field | Default | |
| --- | --- | --- |
| `labels` | | Linear label names that turn the mode on, case-insensitive |
| `model` | `CLAUDE_MODEL` | Model for the session's turns |
| `permission_mode` | `bypassPermissions` | Claude Code `--permission-mode` |
| `deny` | | Claude Code permission deny rules, added to the home directory ones |

Deny rules are a safety net, not a guarantee: a Bash command such as `sed -i` or `git -c user.name=x commit` gets past `Edit` and `Bash(git commit:*)`. For a read-only mode, use `permission_mode: dontAsk`. It refuses every tool call that Claude Code does not judge read-only, including Linear MCP tools and reads outside the worktree; the final reply still reaches the Linear session. `plan` is not enough: in `-p` sessions it refuses MCP writes but lets Bash change files (Claude Code 2.1.293).

A session picks its mode when it starts, from the labels of the same GraphQL lookup repository routing uses, and keeps it for every later turn; the first thought names the mode and the label. An issue with no mode label works as without modes. When labels pick two modes, or a mode file does not parse, the session says so and waits; a reply after fixing the labels or the file starts it. Every mode file is read to pick one, so a single file that does not parse stops every new Linear session, labelled or not, until it is fixed. Mode files are read again every turn, so edits apply without a restart, and a file that no longer parses fails the turn rather than running it without the mode.

The repository's instructions (the review backend's, `prompt_file`, or a `"git": false` repository's no-commit rule) come after the mode's and still decide how work is delivered, so a mode cannot bring commits or pull requests back into a non-git repository. Mode files say how to work, not how to deliver.

## Linear auth

Open `<BASE_URL>/oauth/authorize` to install the app with `actor=app`. It works only while no token is stored, because the endpoint is public. To install again, clear `linear` in `state.json` and restart. Tokens refresh by themselves when Linear rejects them.

One instance serves one Linear workspace, and its repository settings belong to that instance, so run a separate instance per workspace (a company one and a personal one, say). At startup, and whenever a token is stored or refreshed, mothership asks Linear which workspace the token belongs to and pins it. Webhooks whose `organizationId` is not the pinned workspace get `403` and a warning naming the organizations and the event type, never the body. Until a token is stored, every webhook is refused. Set `LINEAR_WORKSPACE` so that a stray install from the wrong workspace cannot become the pinned one; mothership warns at startup when it is unset.

## Tunnel

Any tunnel that forwards to `BIND` works. With `CLOUDFLARE_TOKEN` set, mothership runs `cloudflared tunnel run` itself and restarts it 5 seconds after it exits. The tunnel's hostname → `http://localhost:<port>` route is configured in the Cloudflare dashboard.

## Operating on macOS (launchd)

[`contrib/launchd/mothership.plist`](contrib/launchd/mothership.plist) is a user-agent template: it logs to `~/.mothership/logs/`, restarts the binary (`RunAtLoad`, `KeepAlive`, `ThrottleInterval`, `ProcessType=Background`), and carries only `PATH` in `EnvironmentVariables`. Secrets stay in `~/.mothership/.env`. Edit every `/Users/YOU` path and the binary path first — launchd does not expand `~`.

```sh
cp contrib/launchd/mothership.plist ~/Library/LaunchAgents/com.mothership.agent.plist
mkdir -p ~/.mothership/logs

# Install and start (fails if already loaded; to reload after edits, bootout first)
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mothership.agent.plist

# Restart in place
launchctl kickstart -k gui/$UID/com.mothership.agent

# Logs
tail -f ~/.mothership/logs/mothership.out.log ~/.mothership/logs/mothership.err.log

# Remove
launchctl bootout gui/$UID/com.mothership.agent
```

Before a restart, check the server is not mid-turn, or `kickstart -k` will kill a running agent:

```sh
curl -s localhost:3456/status   # restart only when this reports {"status":"idle"}
```

On Linux, run it as a systemd user service instead — see [`contrib/systemd/mothership.service`](contrib/systemd/mothership.service), whose header lists the matching `systemctl --user` commands.

## Develop

See [AGENTS.md](AGENTS.md) for the code standards.

```sh
mise install          # Rust 1.99 (mise.toml)
scripts/check.sh      # fmt, clippy (pedantic, warnings are errors), tests
git config core.hooksPath .githooks   # run the check before every commit
cargo run
```

`cargo test` includes [`tests/integration`](tests/integration), which runs the built binary against local stand-ins for Linear (through `LINEAR_API_URL`), Zulip, GitHub webhooks, `claude` and `gh`. `cargo test --test integration -- <name>` runs only the scenarios whose name contains `<name>`; a failed one keeps its directory and prints the end of mothership's log.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `scripts/check.sh` with the same toolchain and a gitleaks secret scan on every pull request and every push to `main`.

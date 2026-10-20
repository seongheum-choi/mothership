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

- **One conversation, one worker:** turns run back to back, and each one resumes the same Claude session (`--resume`), or Codex thread.
- **Prompts during a turn:** a prompt that arrives while the agent works goes straight into the running process (`--input-format stream-json`), and Claude folds it into the current turn. Input closes at the turn's result, so anything later starts the next turn.
- **PR feedback:** `POST /github-webhook` takes the GitHub App's `pull_request_review` (submitted, with text), `pull_request_review_comment` (created) and `issue_comment` (created, on a PR) events, checked against `X-Hub-Signature-256`. Only comments and reviews whose `author_association` is `OWNER`, or whose author is in `GITHUB_TRUSTED_LOGINS`, are heard, only when their text mentions the agent's GitHub account (`@<GITHUB_MENTION_LOGIN>`, in any letter case), and only on same-repository PRs in the origins of the git repositories in `repos.json`; a repeated `X-GitHub-Delivery` id is answered 200 and dropped. The PR's head branch picks the newest Linear session recorded on that branch, or on the branch it is stacked on (`en-593-3` → `en-593`). That session gets the author, file and lines, URLs and the text quoted in a `<github_comment>` block as a prompt, and posts its results in Linear as usual. The agent replies with `gh-reply` (installed in its own `<home>/gh-reply` directory and put first on agents' `PATH`; it refuses an empty reply), which appends `<!-- mothership -->`; text with that marker is dropped, as are comments by bots (`type: Bot`) and feedback on branches no session knows. A session that gets 10 GitHub prompts within an hour is stopped and told so in Linear, and further GitHub feedback for it waits for the hour to pass. `issue_comment` events carry no branch, so the server looks the PR up with `gh api`. With `GITHUB_WEBHOOK_SECRET` set, startup fails when no git repository has a GitHub origin or `<home>/gh-reply` cannot be written.
- **Pull request links:** when a Linear turn ends, every GitHub pull request URL (`https://github.com/<owner>/<repo>/pull/<n>`) in its final response or interim thoughts, with fragment and query dropped, is added to the session's external URLs (`agentSessionUpdate` `addedExternalUrls`, labelled `<owner>/<repo>#<n>`), so the session shows the link and Linear can tie the session to the pull request once it is synced. Tool inputs are not searched, since they name pull requests the agent reads as often as ones it opens. Each URL is sent once per session and recorded in `state.json`; a failed update is logged, does not affect the turn, and is retried at the next turn that mentions the URL.
- **Stop:** a Linear stop signal kills the agent's whole process group.
- **Start watchdog:** an agent that announces no session (`system:init`, or Codex's `thread.started`) within `AGENT_START_TIMEOUT` is killed with its process tree, and the turn fails with an error that links [the macOS privacy section](#agent-does-not-start-macos-privacy-prompts), so a hang shows up in minutes rather than as a silent session.
- **Background work:** a Claude turn that ends while background tasks it started (`Agent` calls, background `Bash`) still run keeps its agent and its input open, so Claude Code wakes the agent when they finish and its next answer reaches the session without anyone prompting it; the thread gets a thought saying so. Claude Code reports the running tasks in `background_tasks_changed`; with input closed, `claude -p` stops them about ten minutes after its last answer. After `AGENT_BACKGROUND_TIMEOUT` the input closes anyway and the thread is told. Prompts that arrive meanwhile join the running agent.
- **Closed issues:** when an issue moves to a completed or canceled state or is deleted, its running sessions stop and its worktree under `WORKTREES_DIR` is removed from the session's own repository, ignored files such as `.env` or `build/` included; the branch stays, and a worktree with uncommitted changes is kept with a warning. A worktree that an open session of another issue records (a stacked branch) or that a new prompt reopened meanwhile stays. A non-git repository's directory is never removed. GitHub feedback on a closed session's PR is logged and ignored until the session is prompted again in Linear. When the app is unassigned or undelegated, running sessions stop with a note and the worktree stays. This needs the Linear app's `Issue` data-change and app notification webhooks.
- **Worktrees:** a new branch is cut from `origin/<BASE_BRANCH>` with no upstream, so a bare `git push` never targets the base branch. An existing local or remote branch is continued, and an existing checkout is reused. Only one agent runs per worktree at a time.
- **Home directory:** agents may read only their workspace, their repository's main clone (every repository's, in Zulip), and plugin directories. Every other entry under `$HOME` gets a `Read` deny rule, except the paths in `SANDBOX_READ`, `SANDBOX_WRITE` and `SANDBOX_SKIP_DENY` and the progress directory. Claude Code applies deny rules under `bypassPermissions` too. An `Edit` deny rule, which Claude Code applies to every file-editing tool, covers every entry of `MOTHERSHIP_HOME` except the session's own workspace, the progress directory and `SANDBOX_WRITE`, so an agent cannot change `.env`, `repos.json`, `modes/`, `plugins/` or another session's worktree for later sessions. The sandbox folds `Edit` deny rules into Bash's writes too, and a commit from a worktree writes the clone's `.git` (objects, refs, the worktree's own index under `worktrees/`), so the session's clone keeps its `.git` editable except `config`, `hooks/` and the other worktrees' metadata. dotenv loaders (diesel's, for one) read the first `.env` above the working directory and fail on one they may not read, so at startup mothership puts an empty, readable `.env` in `WORKTREES_DIR` and `<home>/repos` when they are inside `MOTHERSHIP_HOME`; the search stops there before it reaches mothership's own `.env`.
- **Bash sandbox:** Bash runs in Claude Code's sandbox (Seatbelt on macOS, bubblewrap on Linux), which enforces the deny rules on every process it starts and allows writes only to the working directory, `SANDBOX_WRITE` and the session's own directory under `<home>/progress`. Commands cannot opt out of it, and `CLAUDE_CODE_OAUTH_TOKEN` is hidden from Bash. Its limits:
  - Without Seatbelt (macOS) or bubblewrap and socat (Linux), the session fails (`failIfUnavailable`) rather than run unconfined.
  - The sandbox judges a symlink by its target, so `SANDBOX_READ` and `SANDBOX_WRITE` list each path's resolved target next to it, as read at startup.
  - The Read, Edit and Write tools, hooks and MCP servers run outside it; only the deny rules bind them, and nothing stops the Edit and Write tools from writing in `$HOME` outside `MOTHERSHIP_HOME`.
  - The other `AGENT_ENV` values, such as `GH_TOKEN`, are visible to Bash.
  - Under `bypassPermissions` the network stays open: direct connections are blocked, but everything through the sandbox's proxy is allowed.
  - `gh` cannot reach the keyring or read `~/.config/gh`, so it needs `GH_TOKEN` in `AGENT_ENV`, and a config directory of its own: see [sandbox host setup](#sandbox-host-setup).
- **Host runs:** what the sandbox keeps Bash from starting, mothership runs for the session; see [Host runs](#host-runs).
- **Progress:** every agent process gets `MOTHERSHIP_PROGRESS_FILE`, in the session's directory `<home>/progress/<session>/`, which only that session writes. Other sessions' directories are denied as the turn's rules are made, so a session that starts later is not hidden from a turn already running. Lines that tools or skills append to it, from any depth of the process tree, show up in Linear as thoughts. Long-running skill workflows use it to report their phases.
- **Environment:** `MOTHERSHIP_AGENT` (the `AGENT_NAME` setting) and `MOTHERSHIP_SURFACE` (`linear` or `zulip`) let hooks and skills tell where they run. The keys named in `AGENT_ENV` are forwarded on top, so secrets like `CLAUDE_CODE_OAUTH_TOKEN` can live only in `<home>/.env` instead of the daemon config.

Agents run as `claude -p --input-format stream-json --output-format stream-json`. The `--settings` deny rules, `--mcp-config` (Linear's hosted MCP with the app token, plus `MCP_CONFIGS`), and `--plugin-dir` for each plugin are generated per turn.

## Extending

| To add | Where |
| --- | --- |
| A chat or tracker (Slack, GitHub issues) | Implement `session::Surface` and add its routes in `app.rs` |
| A tunnel (ngrok, ...) | A `tunnel::Tunnel` variant and its command |
| A review system (Gerrit) | A `review::ReviewBackend` variant |
| An agent runtime | An `agent::Runner` variant and a module in `src/agent/` that builds its command line and turns its output into `agent::Event`s |
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
| `SANDBOX_SKIP_DENY` | | Comma-separated entries directly under home (`~/Documents,~/Desktop`) that get no deny rule, for macOS hosts where the [privacy approval](#agent-does-not-start-macos-privacy-prompts) cannot be given. The trade-off: the Read tool, and every command in Bash, can then read everything in those folders. Paths are not resolved, and anything deeper than one level is refused at startup |
| `HOST_SANDBOX` | `/usr/bin/sandbox-exec` | What confines [host runs](#host-runs); read at startup |
| `AGENT_START_TIMEOUT` | `120` | Seconds an agent may run without announcing its session before it is killed and the turn fails |
| `AGENT_BACKGROUND_TIMEOUT` | `7200` | Seconds an agent that answered may keep running for its [background work](#how-it-works) before its input closes |
| `BASE_BRANCH` | `main` | Ignored when `repos.json` exists |
| `WORKTREES_DIR` | `<home>/worktrees` | |
| `AGENT_RUNNER` | `claude` | `claude` or [`codex`](#codex); anything else is warned about and runs Claude Code. Read again before every turn, so a change applies to the next turn without a restart. A repository, a mode, a label or a message can choose another; see [Model and effort](#model-and-effort) |
| `CODEX_BIN` | `codex` | The Codex CLI, for `AGENT_RUNNER=codex` and [host runs](#host-runs). A name without a `/` is looked for on the `PATH` an agent runs with — the one `AGENT_ENV` forwards, or mothership's own when it forwards none — so it is found where the turn will run it; a path to a file is taken as it is. Under launchd that `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin`, and a `PATH` in `.env` loses to it, so give the path (`~/.local/bin/codex` for the standalone installer) |
| `CLAUDE_BIN`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` | `claude`, `opus`, `sonnet` | On macOS with the native installer, point `CLAUDE_BIN` at `~/.local/share/claude/ClaudeCode.app/Contents/MacOS/claude`; launched through `~/.local/bin/claude`, privacy prompts name a version number ("2.1.x") that changes with every update |
| `CLAUDE_EFFORT` | | Claude Code `--effort` for every turn: `low`, `medium`, `high`, `xhigh` or `max`; unset (or anything else, which is warned about) leaves the flag off. Like `CLAUDE_MODEL` and `CLAUDE_FALLBACK_MODEL`, it applies from the next turn without a restart; see [Model and effort](#model-and-effort) |
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

State (Linear tokens, and each conversation's repository, workspace and Claude session id or Codex thread id) is kept in `<home>/state.json` with mode 0600.

### Repositories

`<home>/repos.json` lists the repositories one mothership works on. Without it, `REPO_PATH` and `BASE_BRANCH` describe a single repository, named after its directory. One mothership serves one Linear workspace, so each instance (each `MOTHERSHIP_HOME`) has its own `repos.json`. The file is read at startup, and a mistake in it stops startup: overlapping or nested paths, a `git` entry whose path is not the top of a git repository, a missing `prompt_file` or `mcp_configs` file.

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
    "mcp_configs": ["~/.mothership/mcp/github.json"],
    "model": "opus",
    "effort": "high"
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
| `commands` | | Shell commands by name (letters, digits, `-`, `_`) that mothership runs for a session outside its sandbox, under the same rules; see [Host runs](#host-runs) |
| `runner`, `model`, `fallback_model`, `effort` | `AGENT_RUNNER`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL`, `CLAUDE_EFFORT` | For sessions in this repository; see [Model and effort](#model-and-effort). A `runner` other than `claude` or `codex`, or an `effort` other than the five levels, stops startup, and a reload keeps the previous list |
| `git` | `true` | `false`: no worktree; sessions work in `path` directly, and only one turn runs there at a time while other sessions' turns wait; they are told not to commit or open pull requests |

A Linear session picks its repository when it starts and keeps it for every later turn. The first rule that matches decides:

1. `[repo=<name>]` in the @mention or reply, then in the issue description
2. A label: `repo:<name>`, or one listed in `labels`
3. The issue's project
4. The issue's team

A directive inside backticks or a fenced code block is quoted, not given, and is ignored; this holds for `[runner=…]`, `[model=…]` and `[effort=…]` too.

With a single repository none of this is read: every session works there, as before `repos.json`.

The project, team and labels come from a GraphQL lookup, because webhook payloads carry none of them. The session's first thought names the repository and the rule. If no rule decides, a `[repo=…]` or `repo:` label names an unknown repository, the lookup fails, or one rule points at two repositories, the session asks and waits. Replies that do not settle it are kept, except one whose `[model=…]` or `[effort=…]` is refused, which is meant to be sent again corrected. Webhooks are handled concurrently, so replies sent moments apart are usually, but not always, taken in the order they were sent. A reply carrying `[repo=<name>]` starts the work, with the issue context it was created with and every reply kept until then; it is read before the description, so it also corrects a wrong `[repo=…]` there.

A session that started before `repos.json` keeps the repository its worktree was cut from; if that worktree is gone, it goes to the git repository whose main clone has the branch the worktree was first cut on, when exactly one does, ahead of anything on the issue, and is otherwise routed like a new session; either way its worktree is cut again on that branch. That branch is recorded only when the worktree is cut, so a branch the agent switched to later, such as a stacked one, is not where it resumes. A session whose repository has been removed from `repos.json` stops with an error instead of starting over somewhere else.

### Modes

A mode changes how a Linear session works: `<home>/modes/<name>.md` holds instructions for the system prompt, with optional YAML frontmatter. `contrib/modes/` has examples (`implement`, `research`, `debug`); copy or symlink the ones you want into `<home>/modes/`.

```markdown
---
labels: [Research]
runner: claude
model: opus
fallback_model: sonnet
effort: medium
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
| `runner`, `model`, `fallback_model`, `effort` | the repository's, then `AGENT_RUNNER`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL`, `CLAUDE_EFFORT` | For the session's turns; see [Model and effort](#model-and-effort). A `runner` other than `claude` or `codex`, or an `effort` other than the five levels, fails the mode file |
| `permission_mode` | `bypassPermissions` | Claude Code `--permission-mode` |
| `deny` | | Claude Code permission deny rules, added to the home directory ones |

Deny rules are a safety net, not a guarantee: a Bash command such as `sed -i` or `git -c user.name=x commit` gets past `Edit` and `Bash(git commit:*)`. For a read-only mode, use `permission_mode: dontAsk`. It refuses every tool call that Claude Code does not judge read-only, including Linear MCP tools and reads outside the worktree; the final reply still reaches the Linear session. `plan` is not enough: in `-p` sessions it refuses MCP writes but lets Bash change files (Claude Code 2.1.293).

A session picks its mode when it starts, from the labels of the same GraphQL lookup repository routing uses, and keeps it for every later turn; the first thought names the mode and the label. An issue with no mode label works as without modes. When labels pick two modes, or a mode file does not parse, the session says so and waits; a reply after fixing the labels or the file starts it. Every mode file is read to pick one, so a single file that does not parse stops every new Linear session, labelled or not, until it is fixed. Mode files are read again every turn, so edits apply without a restart, and a file that no longer parses fails the turn rather than running it without the mode.

The repository's instructions (the review backend's, `prompt_file`, or a `"git": false` repository's no-commit rule) come after the mode's and still decide how work is delivered, so a mode cannot bring commits or pull requests back into a non-git repository. Mode files say how to work, not how to deliver.

### Reloading settings

Before each Linear, Zulip or GitHub event is routed, mothership compares the modification times of `<home>/repos.json` and `<home>/.env` with the last ones it saw. When either changed, it reads the repository list again (`repos.json`, or `REPO_PATH`/`BASE_BRANCH` without it), so the next session can be routed to a repository added in the meantime, and with GitHub feedback on it resolves the repositories' origins again. A list that no longer loads is logged as a warning (`keeping the current repositories`) and the one in effect stays; the warning repeats only after the file changes again. Turns already running keep the list they started with. Every other `.env` key is read once at startup: when one changes in `.env`, the log names it (not its value) with `restart mothership to apply it`, and the old value stays in effect. A key the process environment sets is not reported, since the environment wins over `.env`.

### Model and effort

The runner and each of `--model`, `--fallback-model` and `--effort` are decided on their own by the narrowest scope that sets them, so `[model=sonnet]` keeps the effort as it was:

| | runner | `--model` | `--fallback-model` | `--effort` |
| --- | --- | --- | --- | --- |
| 1. the session's messages | `[runner=…]` | `[model=…]` | | `[effort=…]` |
| 2. the issue's labels | `runner:<name>` | `model:<name>` | | `effort:<level>` |
| 3. its mode | `runner` | `model` | `fallback_model` | `effort` |
| 4. its repository | `runner` | `model` | `fallback_model` | `effort` |
| 5. the instance | `AGENT_RUNNER` | `CLAUDE_MODEL` | `CLAUDE_FALLBACK_MODEL` | `CLAUDE_EFFORT` |

With no scope setting the effort, no `--effort` is passed and Claude Code picks. A Zulip session has neither labels, a mode nor a repository, so its messages and the instance decide.

A `runner:<name>`, `model:<name>` or `effort:<level>` label is read once, when the session starts, from the same lookup the mode and the repository come from, and the prefix is matched case-insensitively (`Model:sonnet` works) while the value is passed as written, so a `runner:` label needs `claude` or `codex` and an `effort:` label one of the five lowercase levels (`effort: HIGH` is ignored). Because nothing looks an issue's labels up on an instance with no mode files, model labels are only read where `<home>/modes` has at least one file. Unlike a mode label, a mistake here never stops the session: a label with no value, a `runner:` that is not `claude` or `codex`, an `effort:` that is not one of the five levels, and a prefix two labels carry are each logged as ignored, and that field falls through to the mode, the repository and the instance. Mode labels and these prefixed labels are separate, so `model:sonnet` or `runner:codex` does not make a `research` mode ambiguous.

Write `[runner=codex]`, `[model=sonnet]`, `[effort=high]`, or any of them together, anywhere in the @mention that starts a Linear session, in a reply to it, or in a Zulip message; `[runner=default]`, `[model=default]` and `[effort=default]` drop the session's choice again. One in the starting mention applies to the first turn; one in a reply that joins a running turn applies from the next turn, which still resumes the same conversation. A runner other than `claude` or `codex`, or an effort other than the five levels, is answered with an error and runs no turn; a reply refused this way is dropped rather than kept for a session that waits for its repository or mode, so send it again corrected. A model name is passed to Claude Code as written: if the turn on it fails, the error shows in the session and the session goes back to the model it had.

`CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` and `CLAUDE_EFFORT` are read from `.env` again before every turn, so a change applies to the next turn without a restart; a mode's and a repository's values are read again every turn too. `GET /status` shows the instance defaults in effect as `runner`, `model`, `fallback_model` and `effort`, and the commit the binary was built from as `commit` (set by `contrib/deploy.sh`, `null` otherwise).

When a turn's runner or models differ from the session's previous turn, or anything but the instance defaults decided them on its first turn, the turn's first thought says each value and where it came from: `Model: sonnet, effort: high (from your message)`, `Model: haiku (repo \`vault\`), effort: medium (mode \`research\`)`, or `Runner: codex (label \`runner:codex\`), model: opus, effort: default (instance default)`. The runner is named only when something other than the instance chose it or it changed since the previous turn, and the fallback model only when a mode or a repository set it. A first turn on the instance defaults alone says nothing.

### Codex

With `AGENT_RUNNER=codex`, or a session whose [runner scope](#model-and-effort) chose Codex (`[runner=codex]`, a `runner:codex` label, a mode's or a repository's `runner: codex`), each turn runs `codex exec --json --skip-git-repo-check --sandbox workspace-write --cd <workspace> -` (checked against Codex 0.146 and 0.160) with the prompt on stdin, and its JSONL becomes the same thoughts, actions and response as Claude Code's: `reasoning` and `agent_message` items are thoughts, `command_execution` is a `Bash` action, `file_change` an `Edit` action per file, `mcp_tool_call` an `mcp__<server>__<tool>` action, and the last agent message before `turn.completed` is the response. `turn.failed`, or the output ending after an `error` event without a result, fails the turn; `turn.completed`'s token usage is logged. The `thread.started` id is kept where a Claude session id would be, and the next turn runs `codex exec … resume <thread_id> -`.

- **System prompt:** what Claude Code gets through `--append-system-prompt` (the session's instructions, its mode's prompt, the repository's delivery rules) goes in as `-c developer_instructions=…`.
- **Model and effort:** the turn's model is passed as `-m` unless it names a Claude model (`opus`, `sonnet`, `haiku`, `claude-…`, so also the `CLAUDE_MODEL` default); then Codex runs the model its own config picks. Set a Codex model with `[model=…]`, a `model:` label, a mode's `model` or the repository's `model`. The effort is passed as `-c model_reasoning_effort=…`. There is no fallback model.
- **Prompts during a turn:** `codex exec` reads its prompt to the end of stdin and takes nothing more, so a prompt that arrives while it works waits for the turn to end and starts the next one, which resumes the thread.
- **Switching runners:** each session keeps one conversation per runner. A turn on a runner the session has not used yet starts a new conversation there; one on a runner it used before resumes that runner's conversation.

Limits of this first version:

- Codex runs in its own sandbox, `workspace-write` with network access, and `codex exec` never asks for approval (it has no `-a`). Its sandbox may also write `SANDBOX_WRITE` and the progress directory (`--add-dir`), but none of mothership's `Read` and `Edit` deny rules apply: Codex can read anything the mothership user can.
- `CHAT_PERMISSION_MODE` and a mode's `permission_mode` and `deny` do not apply.
- No Linear MCP, `MCP_CONFIGS`, repository `mcp_configs` or `<home>/plugins`; configure MCP servers and skills in Codex's own config (`~/.codex`).
- `CLAUDE_FALLBACK_MODEL` does not apply, and `CLAUDE_CODE_OAUTH_TOKEN` is irrelevant; Codex uses its own login.

#### Host runs

A Claude Code session cannot start Codex from Bash: its sandbox keeps Codex's binary and login (`~/.codex`) out of reach, and Codex's own sandbox cannot start inside another Seatbelt sandbox. Some setup fails there too: Claude Code's sandbox refuses writes to any `.idea` directory, so `pnpm install` fails on a dependency that ships one. So mothership runs these for the session. The agent writes a request into `$MOTHERSHIP_RUNS`, its session's directory `<home>/progress/<session>/`, as a file renamed into place once whole:

```json
{"kind": "codex", "prompt": "…", "schema": {"type": "object"}, "effort": "high", "model": "gpt-5.5"}
{"kind": "command", "name": "pnpm-install"}
```

- **Codex:** `codex exec --sandbox danger-full-access -C <workspace>` with the prompt on stdin. `schema` is passed as `--output-schema`, `effort` (`minimal` to `xhigh`) as `model_reasoning_effort`, `model` as `--model`. The result is the final message, parsed as JSON when a `schema` shaped it.
- **Command:** one of the repository's `commands` in `repos.json`, a map of names to shell commands (`{"pnpm-install": "cd client && pnpm install --frozen-lockfile"}`), run with `/bin/sh -c` in the workspace and the run's environment, whose `PATH` is the one `AGENT_ENV` forwards or else mothership's own (under launchd, only the system directories), so a command names tools such as mise by their path (`cd client && /opt/homebrew/bin/mise exec -- pnpm install --frozen-lockfile`). The result is the last 20 lines of its output, stderr interleaved, and the path of `<id>.log` next to the request, which holds the output up to its last mebibyte. The session's system prompt lists the repository's commands.

While the turn lasts, mothership takes one request at a time and writes `<id>.out.json` next to it, either `{"result": …}` or `{"error": "…"}`. Each run's start and end show up as thoughts. A run runs in a process group of its own; the group is killed when the run exits, and the whole tree too when the turn ends or is stopped (before the workspace is released) or after an hour.

Every run happens under `HOST_SANDBOX -p <profile>`, with a profile made from the turn's own settings:

- the `Read` deny rules deny reading, matched as written and as resolved, globs included;
- writing is allowed only in the workspace and the clone's `.git` (neither under a mode's bare `Edit` rule), `SANDBOX_WRITE`, the session's directory, `/dev` and the run's own directory under `<home>/host-runs`, which is also its `TMPDIR`; not `/private/tmp` or `/private/var/folders`, where runs could reach each other's files;
- a Codex run may also write Codex's state in `CODEX_HOME`: sessions, logs, caches, state databases and a refreshed login, but not its config, hooks, rules, skills, memories or plugins;
- the `Edit` deny rules deny writing again inside those (the clone's `config` and `hooks`, other worktrees), and so do the names Claude Code's sandbox never lets Bash write (shell rc files, `.gitconfig`, `.envrc`, `.npmrc`, `.mcp.json`, `.git/hooks`, `.claude/`, `.vscode/` and the rest), but for `.idea`;
- signals reach only processes in the same sandbox;
- the network and other system services stay open, as through Claude Code's proxy under `bypassPermissions`.

Only a Codex run can read Codex's home and binary; a command runs under the session's own read rules. A run's environment is the agent's (`AGENT_ENV` and the `MOTHERSHIP_*` values) without what the settings hide from Bash (`CLAUDE_CODE_OAUTH_TOKEN`), over `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_ALL` and `PATH`; nothing else of mothership's.

A session can only ask in its own directory. Neither the session nor a run can remove, rename or swap for a link a directory they write in (the session's directory, the workspace, a run's own directory), nor move a `.git` aside, since a sandbox may not touch the directory it writes in and the profile pins these. mothership reads a request or a final message only as a plain file with no other link, of at most a mebibyte, opened without following a link or waiting on a FIFO, and writes its answer fresh under a name nobody can guess before renaming it into place, so nothing a session or a run plants leads mothership elsewhere.

What stays exposed: a Codex run and every command it runs can read Codex's login, since Seatbelt cannot tell Codex from its commands; Codex runs of all sessions share Codex's session history; a process that leaves both the run's tree and its group (`setsid`) outlives it; and a global `AGENTS.md` in Codex's home that links outside what the session may read is not read. Without macOS's `sandbox-exec`, requests are answered with an error.

#### Claude usage limit

A turn on Claude Code that Claude Code ends on the account's usage limit runs again on Codex with the same prompts, including those added while it worked, and its first thought says `Claude usage limit reached → switching to Codex until 2026-10-12 14:05 UTC`. Claude Code retries and tries `CLAUDE_FALLBACK_MODEL` itself first, so only a limit that ends the turn switches. The limit is recognised by what Claude Code 2.1.296 writes: an `assistant` message with `"api_error": "usage_limit_reached"` whose `api_error_params.rate_limit_info.resetsAt` is the reset time, then a `result` with `"is_error": true` and the same `api_error`.

The reset time is kept in `state.json` for the whole instance, since every session shares one account; when two sessions hit the limit at once, the later reset is the one kept. Until then, every new turn that would run on Claude Code runs on Codex, opening with `Claude usage limit until … → running on Codex`, and `GET /status` shows it as `claude_limited_until` with `runner` `codex` when the instance default is Claude Code (`runner` is always the instance default, never a session's). The first turn after it goes back to Claude Code and resumes the session's Claude conversation; a session that worked on Codex meanwhile keeps its Codex thread for the next limit. Without a reset time, only the turn that hit the limit moves. An instance whose `CODEX_BIN` is neither a file nor on the agent's `PATH` reports the limit as a failed turn, as before.

The switch only applies to a runner a label, a mode, a repository or the instance chose. A runner a message chose is pinned: a `[runner=claude]` session that hits the limit fails the turn, with a note that it is pinned and that `[runner=default]` or `[runner=codex]` lets it move, and tries Claude Code again on its next turn; a `[runner=codex]` session stays on Codex before, during and after the limit.

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

### Agent does not start (macOS privacy prompts)

**Symptom:** every session's agent hangs at start: the process is alive at 0% CPU with no output, no transcript, no sockets and no children, and after `AGENT_START_TIMEOUT` the session fails with `The agent did not start within …`. The same command run over ssh works.

**Cause:** the deny rules name `~/Desktop`, `~/Documents` and `~/Downloads`, and Claude Code touches each rule's path. Those folders are protected by macOS privacy controls (TCC). A background process in the GUI launchd domain gets a permission dialog for them, and the call blocks until someone answers it; over ssh there is no UI, so the access is refused at once and the agent carries on. TCC attributes the access to the launchd job's responsible process, the mothership binary, so the dialog is named `mothership`.

**Fix:** approve it once, before the first session:

1. Sign the binary with a fixed identity (next section), or every rebuild is a new program to TCC and asks again.
2. Start mothership under launchd, send it one session, and answer the dialog on the Mac's screen (Screen Sharing works). Alternatively grant the binary Full Disk Access, or Files and Folders → Desktop, Documents and Downloads, in System Settings → Privacy & Security.

A hung agent continues by itself once the dialog is answered. Where nobody can approve it, list the folders in `SANDBOX_SKIP_DENY`, which leaves them readable to agents.

**Checking:** a one-shot LaunchAgent (no `KeepAlive`) whose program runs `launchctl managername` prints `Aqua` when it is in the GUI domain. Running the agent's `claude` command line from it hangs, and the same command without `--settings` works; adding the deny rules back one at a time shows which folder asks.

### Signing and deploying

TCC remembers an approval by the program's identifier and signing certificate. `cargo build` leaves an ad-hoc linker signature with an identifier like `mothership-<hash>` that changes every build (`codesign -dv` shows it), so sign with a self-signed code-signing certificate instead. Create it once, from a GUI session (a locked keychain cannot be unlocked over ssh):

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=Mothership Dev" \
  -keyout dev.key -out dev.crt -addext basicConstraints=critical,CA:TRUE \
  -addext keyUsage=digitalSignature -addext extendedKeyUsage=codeSigning
# -legacy is for OpenSSL 3, whose default p12 encryption macOS cannot import; drop it with /usr/bin/openssl (LibreSSL)
openssl pkcs12 -export -inkey dev.key -in dev.crt -out dev.p12 -legacy
security create-keychain mothership.keychain-db        # choose a password
security import dev.p12 -k mothership.keychain-db -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -k <password> mothership.keychain-db
trash dev.key dev.p12
```

`security find-identity -v` lists no *valid* identity for a self-signed certificate; that is expected. `codesign` still signs with it, and TCC compares the certificate, not its trust. Keep all three extensions: LibreSSL adds none by default, and with only `extendedKeyUsage` `find-identity` still lists the certificate but `codesign` answers `Mothership Dev: no identity found`.

[`contrib/deploy.sh`](contrib/deploy.sh) then pulls, builds, signs (`codesign -s "Mothership Dev" -i mothership`), waits for `/status` to report idle, replaces the binary, kickstarts the agent and fails unless `/status` then reports the commit it built. Its paths, launchd label and keychain are variables at the top of the script; put the keychain password in `~/.mothership/.keychain-pw` (mode 0600) to run it unattended. The first signed binary needs the approval once; later rebuilds keep it. If the first session after a deploy hangs, check the signature with `codesign -dv` first.

With the native installer, `CLAUDE_BIN` pointing at the `ClaudeCode.app` bundle (see [Settings](#settings)) gives Claude Code its own stable identity for its own prompts; the folder dialog is still attributed to mothership.

### Sandbox host setup

The Bash sandbox blocks some things a development host relies on. What has been needed so far:

- **gh:** `~/.config/gh/hosts.yml` holds a plain-text token, so do not open `~/.config/gh`. Give agents a config directory without it: copy `config.yml` to `~/.mothership/gh`, set `GH_CONFIG_DIR=~/.mothership/gh` in `.env`, list `GH_CONFIG_DIR` and `GH_TOKEN` in `AGENT_ENV`, and add the directory to `SANDBOX_WRITE`.
- **gh extensions:** `gh` finds extensions such as `gh stack` under `~/.local/share/gh/extensions`; add that directory to `SANDBOX_READ`. Reading is enough to run one; only `gh extension install` and `upgrade` write there.
- **Temporary files:** the sandbox blocks `/var/folders`, so `mktemp` fails. Set `TMPDIR=~/.mothership/tmp` in `.env`, list it in `AGENT_ENV`, and add the directory to `SANDBOX_WRITE`.
- **mise:** each new worktree's `mise.toml` is untrusted, and the shims stop with `Config files … are not trusted`. Trust the worktree and clone directories once in `~/.config/mise/config.toml`: `[settings]` `trusted_config_paths = ["~/.mothership/worktrees", "~/.mothership/repos"]`.
- **Symlinked directories:** symlink targets are resolved only for the listed path itself, not for symlinks inside a listed directory. If `~/.claude/skills/x` links to `~/settings/ai/skills/x`, list the target directory (`~/settings/ai/skills`) in `SANDBOX_READ` as well, or the skill's scripts fail with `operation not permitted`.
- **Other paths seen so far:** `~/.gitignore_global` (or whatever `core.excludesfile` names) in `SANDBOX_READ`, or git warns on every command; `~/.cache/node` in `SANDBOX_WRITE` for corepack.


## Develop

See [AGENTS.md](AGENTS.md) for the code standards.

```sh
mise install          # Rust 1.99 (mise.toml)
scripts/check.sh      # fmt, clippy (pedantic, warnings are errors), tests
git config core.hooksPath .githooks   # run the check before every commit
cargo run
```

`cargo test` includes [`tests/integration`](tests/integration), which runs the built binary against local stand-ins for Linear (through `LINEAR_API_URL`), Zulip, GitHub webhooks, `claude` and `gh`. `cargo test --test integration -- <name>` runs only the scenarios whose name contains `<name>`; a failed one keeps its directory and prints the end of mothership's log.

An agent working on mothership inside the Bash sandbox cannot pass the scenarios that check a process tree is killed (`linear_stop_kills_the_agent`, `linear_agent_that_never_starts_is_stopped`, `sigterm_stops_running_agents`), since the sandbox blocks signalling those processes; CI is the gate for them.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `scripts/check.sh` with the same toolchain and a gitleaks secret scan on every pull request and every push to `main`.

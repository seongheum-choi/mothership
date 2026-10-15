# Handoff: the implement workflow cannot start its agents inside the mothership sandbox

Written 2026-10-15 from an alean status session. Delete this file once the fix lands.

## Symptom

Every Impala implement run since 2026-10-08 fails at its first agent, then the session either stops and asks (EN-943, idle 18 h) or falls back to building in-session with one review agent and `--no-verify` (EN-824, EN-827, EN-804, EN-834). Last seen 2026-10-15 00:45Z on EN-982.

`~/.mothership/repos/alean/.git/implement-runs/<ISSUE>/progress.log`:

```
build layer 1 [claude] failed: Not logged in · Please run /login
workflow error: Error: spawn EPERM
```

Last good run: EN-892, 2026-10-07 07:31Z. First failure: EN-866, 2026-10-08 05:53Z, 1.5 h after `41235f2 feat(sandbox): run Bash in Claude Code's OS sandbox` (2026-10-08 04:18Z).

## Cause

`~/settings/ai/skills/implement/scripts/run-workflow.mjs` starts its agents as child processes from Bash (`claude -p`, `codex exec`). Since `41235f2`, Bash runs in Seatbelt with the settings from `src/sandbox.rs`:

- **claude:** `sandbox.credentials.envVars` denies `CLAUDE_CODE_OAUTH_TOKEN` to Bash on purpose, and `~/.claude` (apart from `~/.claude/skills`) and the login keychain item are out of reach, so the child has no login. It would also need a writable config dir; `~/.claude` is not in `SANDBOX_WRITE`.
- **codex:** `~/.local/bin/codex` links to `~/.codex/packages/standalone/current/bin/codex`; neither `~/.local` nor `~/.codex` is in `SANDBOX_READ`, so the spawn is EPERM. Past that, codex needs `~/.codex/auth.json` and writes `~/.codex/sessions`, and its own `-s workspace-write` Seatbelt probably cannot nest inside Claude Code's (unverified).

So the runner and the sandbox contradict each other: the sandbox keeps agent credentials out of Bash, and the runner needs them in Bash.

## Options

1. **Run the workflow's agents as Claude Code subagents when sandboxed (recommended).** Teach the implement skill to detect the mothership sandbox (e.g. `MOTHERSHIP_PROGRESS_FILE` set) and run build/review/fix as Agent-tool subagents of the session instead of `run-workflow.mjs`. No credential reaches Bash and mothership is unchanged. Cost: no Codex engine diversity, no journal resume. This is what the sessions already improvise; making it the skill's path keeps the review loop's rounds instead of one ad-hoc review.
2. **Open the credentials to Bash.** Drop the `CLAUDE_CODE_OAUTH_TOKEN` deny (or pass a second var in `AGENT_ENV`), set `CLAUDE_CONFIG_DIR` to a dir under `SANDBOX_WRITE`, and add `~/.local/bin`, `~/.codex` to `SANDBOX_READ` and `~/.codex/sessions` to `SANDBOX_WRITE`. Smallest diff, but anything an agent runs can read both tokens, and the network goes through the proxy, which allows everything under `bypassPermissions` (README), so a token can leave the machine. That is Nate's call, not an implementation detail.
3. **Run the runner outside the sandbox.** mothership serves "run this workflow" host-side (an MCP tool, or `sandbox.excludedCommands` for the runner if Claude Code honours it with `allowUnsandboxedCommands: false`; unverified). Keeps both engines and keeps tokens out of Bash, but it is new mothership surface, and the runner's agents then run unconfined.

## Same-sandbox problems that force `--no-verify`

The alean pre-commit hook fails in every Impala session, so all of them commit with `--no-verify`:

- **pnpm install:** `EPERM … mkdir …/node_modules/.pnpm/generic-names@4.0.0/…/.idea/codeStyles`. Claude Code's sandbox refuses writes to `.idea` paths, and `generic-names` ships one. Check whether the sandbox settings can allow it for `node_modules`, or have pnpm skip it.
- **diesel:** `Initializing .env file failed: Operation not permitted`. dotenvy walks up from the worktree and hits `~/.mothership/.env`, which agents may not read. EN-943's session put an empty `.env` at its worktree root. A fix in `scripts/worktree-db.sh` or a `DOTENV`-off flag would cover every worktree.

Each belongs in README's "Sandbox host setup" once fixed.

## Done when

- An Impala implement run on alean gets past `build layer 1` (or option 1's subagent build) and opens its draft PR with the review loop's rounds, without `--no-verify`.
- `mothership` integration scenarios still pass (`linear_agent_is_launched_in_the_sandbox` asserts the current settings).

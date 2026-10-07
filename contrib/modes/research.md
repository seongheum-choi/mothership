---
labels: [Research]
# Deny rules are a safety net, not a guarantee: `sed -i` or `git -c ... commit` get past them.
# dontAsk refuses every tool call Claude Code does not judge read-only, which keeps the session
# read-only; plan does not stop Bash writes in -p sessions. It also refuses Linear MCP tools
# and reads outside the worktree.
permission_mode: dontAsk
deny:
  - Edit
  - Write
  - NotebookEdit
  - Bash(git commit:*)
  - Bash(git push:*)
  - Bash(gh pr create:*)
---
Answer the question in the issue; do not change anything.

- Read code, history and documentation, and run read-only commands, until you can answer with evidence.
- Do not edit files, commit, push, or open a pull request, whatever the instructions below say about finishing.
- Your final reply is the result, posted as a comment in the Linear session: the answer first, then the evidence with `path:line` references, then open questions.

---
name: surmount-merge-review
description: Use when syncing upstream main into the Surmount fork. Guides read-only git triage, Zed Merge Review queue HITL, and SURMOUNT.md documentation updates. Extends branch-differences-documenter rules. No sub-agents.
---

# Surmount Merge Review

**Binding rules (override everything else):**

> NO SUB-AGENTS / TASKS. Run all work in the main conversation.

> READ-ONLY GIT runs autonomously when safe (`git diff`, `git log`, `git merge-base`, `git status`, etc.). Mutating git (`fetch`, `merge`, `checkout`) — human runs; wait for output.

> NEVER INVENT DIFFERENCES. Only describe changes visible in exact diff text.

> USE `TODO:` for every uncertainty in SURMOUNT.md prose.

> ONE CATEGORY AT A TIME. Stop after each category for human confirmation.

## When to use

- Before, during, or after `git merge origin/main` on `surmount`
- When the Zed **Surmount Merge Review** queue has ambiguous items
- When updating [SURMOUNT.md](SURMOUNT.md) after an upstream sync

## Zed GUI workflow

1. Human runs `git fetch` and `git merge origin/main` (or begins merge).
2. In Zed: **Branch Diff** against `main` → **Surmount Merge Review** (toolbar) or command palette `surmount: Start Merge Review`.
3. Review the queue:
   - **Ambiguous** — HITL: Accept Fork / Accept Upstream / Send to Agent
   - **Conflicts** — resolve in editor or **Resolve with Agent**
   - **Fork-owned** — confirm category prose in SURMOUNT.md
4. Agent drafts SURMOUNT.md entries per category; human confirms in queue.
5. Mark items `Documented` when SURMOUNT.md is updated.

## Agent workflow

1. Run `script/surmount-merge-triage` (or equivalent read-only commands) for merge-base + file list.
2. Propose categories from [surmount-merge-categories.toml](surmount-merge-categories.toml). Flag borderline groupings with `TODO:`.
3. Human confirms category order.
4. For each category: `git diff <merge-base>..HEAD -- <paths>` — draft SURMOUNT.md entry.
5. Present entry + `TODO:` items; wait for human confirmation.
6. On confirmation, mark category done and stop.

## Read-only git commands

```bash
git merge-base HEAD origin/main
git diff --name-status $(git merge-base HEAD origin/main)..HEAD
git diff --name-only --diff-filter=U
git diff $(git merge-base HEAD origin/main)..HEAD -- crates/agent_ui/
```

## Prioritize

- Fidelity to exact diff text
- HITL for `misc_upstream` and `uncategorized` paths
- Persist verdicts via Zed queue, not silent doc edits
- Concise SURMOUNT.md prose
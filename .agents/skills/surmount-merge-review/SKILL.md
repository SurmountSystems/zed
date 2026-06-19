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

> RED/GREEN TDD WHEN DIAGNOSING BUGS: State a hypothesis, add a failing test or assertion that encodes it, fix production code, re-run the same scoped test until green. Prefer `assert!` / `assert_eq!` over debug prints. For this crate: `cargo test -p agent_ui merge_review::tests::<test_name>`. One test at a time; no broad test runs.

## Merge review UI failure checklist

When the human reports crash, hang, or invisible tab after `surmount: Start Merge Review`:

1. **Confirm build includes fixes** — startup log SHA must change after `cargo build --release`; stale binaries reuse old deploy code.
2. **Read logs in order** — see SURMOUNT.md § Merge review tab visibility. Missing `deferred reveal` or `first render` pinpoints deploy vs render.
3. **Hypothesis: invisible behind zoomed agent dock** — logs stop after `tab opened`, no crash. Fix path is `reveal_tab` + `ZoomOut`, not pane focus.
4. **Hypothesis: crash after `tab opened`** — likely `dismiss_zoomed_items_to_reveal` closing agent dock during `ZedTodosSurface` overlay. Do not reintroduce `active_pane().focus_handle().focus()`.
5. **Hypothesis: empty queue** — `populated 0 items`: wrong active repo (e.g. `ref/vibe-palace` submodule). Confirm `start requested` worktree path ends with `/zed`.
6. **Hypothesis: triage mismatch** — run `./script/surmount-merge-triage`; `changed_file_count` should match `populated N items`.
7. **Before asking human to retest** — run `cargo test -p agent_ui merge_review::tests` and ensure GPUI deploy tests are green.

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

1. Run `script/surmount-merge-triage` for merge-base + file list. Expect `changed_file_count` > 0 on `surmount` before merge; zero means wrong repo or a triage/parser bug — investigate before proceeding.
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
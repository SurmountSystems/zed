---
name: surmount-merge-review
description: Use when syncing upstream main into the Surmount fork. Branch Diff spine, per-file Review Diff summaries, session memory, Plan Todos for open questions only. No sub-agents.
---

# Surmount Merge Review

**Binding rules (override everything else):**

> NO SUB-AGENTS / TASKS. Run all work in the main conversation.

> READ-ONLY GIT runs autonomously when safe (`git diff`, `git log`, `git merge-base`, `git status`, etc.). Mutating git (`fetch`, `merge`, `checkout`) — human runs; wait for output.

> NEVER INVENT DIFFERENCES. Only describe changes visible in exact diff text.

> USE `todo_write` only when genuinely stuck — those entries become Plan Todos. Do not maintain a parallel uncertain-file queue.

> ONE CATEGORY AT A TIME when drafting SURMOUNT.md section prose. Stop after each section for human confirmation.

> RED/GREEN TDD WHEN DIAGNOSING BUGS: `cargo test -p agent_ui merge_review::tests::<test_name>`. One test at a time.

## When to use

- Before, during, or after `git merge origin/main` on `surmount`
- When Branch Diff shows many changed files and you need cumulative context
- When updating [SURMOUNT.md](SURMOUNT.md) after an upstream sync

## Zed GUI workflow (target)

1. Human runs `git fetch` and `git merge origin/main` (or begins merge).
2. **Start Merge Review** (palette or Branch Diff **Merge review** when no session yet): hides left tree + bottom terminal, opens Branch Diff vs `origin/main`, posts plan in the right agent panel. Toast: **Step 1: click a changed file in the list. Step 2: click Review Diff in the toolbar.**
3. Branch Diff **step rail** shows `reviewed/N` plus **Next file →**, **Review Diff**, conflict buttons, and **End merge review**. Exactly one tinted button is always the obvious next step. Agent panel toolbar stays slim (snippet + `?` only).
4. Per file: click it in the Branch Diff list, then tinted **Review Diff**. Prompt includes section + session memory. End with `Summary: …`, `Outcome: keep_fork | take_upstream | synthesize | needs_human`, and optional `Pattern: …`. Zed captures these automatically; the summary toast embeds the next action (click it).
5. **Conflicts:** split Branch Diff (fork left, upstream right). After summary, tinted **Keep fork** or **Take upstream** on the rail (or toast), or call `resolve_merge_conflict` — `git checkout --ours/--theirs` + `git add`. Do not run terminal git yourself during merge review unless the human asks. Never strip conflict markers manually unless synthesizing (`edit_file` only then).
6. If stuck, add `Open question: …` in the reply and/or `todo_write` for a Plan Todo.
7. **Resume merge review** reloads the saved session (Branch Diff + memory); prototype file-list tab is not used.

## Agent workflow

1. Call `merge_review_triage` (ACP read-only tool) for merge-base + file list — never run `script/surmount-merge-triage`, bash, or python. When a Zed merge review session is active, the plan prompt already includes counts; use the tool only if you need to refresh or verify. `changed_file_count` should match in-app populate; zero means wrong repo or triage bug.
2. Propose review order by SURMOUNT section (conflicts first). Use `todo_write` only for unresolved items.
3. Per file: summarize visible hunks — upstream vs fork, overlap with earlier summaries. End with `Outcome:` as above.
4. Conflicts: use `resolve_merge_conflict` (git checkout) — not manual marker stripping unless synthesizing.
5. When asked, draft SURMOUNT.md section prose from confirmed summaries in the session.
6. Mark Plan Todos completed when the human resolves them.

## Read-only merge review tools

- `merge_review_triage` — merge-base, changed files, conflicts (replaces triage script)
- `merge_review_diff` with `path` — per-file hunks (replaces `git diff --merge-base`)
- Do not use the terminal for these during merge review unless the human explicitly asks.

## Prioritize

- Fidelity to exact diff text
- Shrinking uncertainty via summaries, not upfront "ambiguous" spam
- Session memory (`running_notes`) over re-asking the human
- Plan Todos for the human queue, not a side tab or SURMOUNT.md `TODO:` spray
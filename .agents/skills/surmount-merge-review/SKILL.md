---
name: surmount-merge-review
description: Use when syncing upstream main into the Surmount fork. Branch Diff spine, per-file Review Diff summaries, session memory, Plan Todos for open questions only. Main conversation while this skill is active.
---

# Surmount Merge Review

**Binding rules (override everything else):**

> While this skill is active, run merge review work in the main conversation (no sub-agents). Does not override `/implement` when the human invokes it.

> READ-ONLY GIT runs autonomously when safe (`git diff`, `git log`, `git merge-base`, `git status`, etc.). **Start Merge Review** may run `git fetch origin` in-app. Other mutating git (`merge`, `checkout`, `commit`) — human runs; wait for output.

> NEVER INVENT DIFFERENCES. Only describe changes visible in exact diff text.

> USE `todo_write` only when genuinely stuck — those entries become Plan Todos. Do not maintain a parallel uncertain-file queue.

> ONE CATEGORY AT A TIME when drafting SURMOUNT.md section prose. Stop after each section for human confirmation.

> RED/GREEN TDD WHEN DIAGNOSING BUGS: `cargo test -p agent_ui merge_review::tests::<test_name>`. One test at a time.

## Dogfood vs this skill

| Layer | Owner |
|-------|--------|
| **In-Zed review** (Branch Diff, rail, decisions, Plan Todos, SURMOUNT prose) | **This skill** — main conversation while active |
| **Headless chrome proof** (Start → look → Preview → End, optional Advance, TextInput/Dialog expects) | [`zed-dogfood`](../zed-dogfood/SKILL.md) via `cargo xtask dogfood merge-review` / `queue` |

Dogfood **proves labels and actions exist** in room outline; it does not replace reading diffs or recording outcomes. PreMerge rail is **Preview merge** (primary) + **Next file** (triage); **Review Diff** is for MergeInProgress, not Prepare. Operator residual plan: [`plans/0_agentic_dogfooding.md`](../../plans/0_agentic_dogfooding.md).

## Upstream services stripped

On conflicts in `crates/client`, `crates/rpc`, `crates/telemetry`, or `assets/settings/default.json`: keep Surmount no-ops (no Zed Cloud sign-in, no outbound telemetry/metrics). See SURMOUNT.md § Upstream services stripped. Outcome is usually `keep_fork` unless the human opts back in.

## When to use

- Before, during, or after `git merge origin/main` on `surmount`
- When Branch Diff shows many changed files and you need cumulative context
- When updating [SURMOUNT.md](SURMOUNT.md) after an upstream sync

## Zed GUI workflow (target)

1. Human runs `git merge origin/main` when ready (merge is human-gated). **Start Merge Review** auto-fetches `origin`. Before merging, use **Preview merge** on the step rail (or palette **Preview Merge Review Merge**) for a `git merge-tree` preview — copy the command from the modal; Zed does not run `git merge`.
2. **Start Merge Review** (palette or Branch Diff **Merge review** when no session yet): hides left tree + bottom terminal, opens Branch Diff vs `origin/main`, posts plan in the right agent panel. Toast: **Step 1: click a changed file in the list. Step 2: click Review Diff in the toolbar.**
3. Branch Diff **step rail** shows `reviewed/N` plus **Next file →**, **Review Diff**, **Preview merge** (PreMerge only), conflict buttons (only while merge is in progress — `MERGE_HEAD` present), **Draft commit message** when all files summarized, and **End merge review**. Workflow buttons use a literal **`#0f0` 1px square green border**. Agent-facing actions open a **note popover** (optional direction) before sending. Agent panel toolbar stays slim (snippet + `?` only).
4. Per file: click it in the Branch Diff list, then tinted **Review Diff**. Prompt includes section + session memory. End with `Summary: …`, `Outcome: keep_fork | take_upstream | synthesize | needs_human`, and optional `Pattern: …`. Zed captures these automatically; the summary toast embeds the next action (click it).
5. **Conflicts (workshop):** split Branch Diff (fork left, upstream right). After summary: **Discuss conflict** (note popover → 3 embeds + Q&A) → **Keep fork** / **Take upstream** or **Synthesize** (`resolve_merge_conflict` or `edit_file` merge — never strip markers unless synthesizing) → **Record decision** via GUI outcome buttons (stores rationale; optional note in popover) → three native Plan Todos auto-posted → **Complete tests** when todos are done → **Next file →** only when markers cleared, decision recorded, and todos complete. Call `merge_review_verify_conflict_resolved` to confirm markers and index state. Add diff **review comments** on hunks, then **Send Review to Agent** (note popover) to post them with conflict context. Agent `merge_review_record_decision` and `Decision:` / `Rationale:` reply lines remain optional fallbacks.
6. If stuck, add `Open question: …` in the reply and/or `todo_write` for a Plan Todo.
7. **Resume merge review** reloads the saved session, refreshes git mode + unmerged count from the worktree, then restores Branch Diff + memory; prototype file-list tab is not used.
8. **Commit message:** when review is complete, human runs **Draft commit message** — you draft from `running_notes`, `patterns`, and conflict decisions; end with `Merge commit message: …`. Human copies from the modal and runs `git commit` themselves.

## Agent workflow

1. Call `merge_review_triage` (ACP read-only tool) for merge-base + file list — never run `script/surmount-merge-triage`, bash, or python. When a Zed merge review session is active, the plan prompt already includes counts; use the tool only if you need to refresh or verify. `changed_file_count` should match in-app populate; zero means wrong repo or triage bug.
2. Propose review order by SURMOUNT section (conflicts first). Use `todo_write` only for unresolved items.
3. Per file: summarize visible hunks — upstream vs fork, overlap with earlier summaries. End with `Outcome:` as above.
4. Conflicts: workshop sequence above; use `merge_review_conflict_sides` for region metadata; `resolve_merge_conflict` for ours/theirs — not manual marker stripping unless synthesizing.
5. **Draft section:** human runs palette **Draft Merge Review Section** (or asks you) — draft SURMOUNT.md prose from confirmed summaries in the active section only; do not edit SURMOUNT.md in that turn.
6. **Open questions:** when capture sets `OpenQuestion` or `Outcome: needs_human`, Zed may post a follow-up asking you to `todo_write` one Plan Todo:
   - Title: `Merge: <repo-relative-path>`
   - Content: SURMOUNT section, file path, summary, why stuck
   - Status: `pending`
7. Mark Plan Todos completed when the human resolves them.

## Batch summarize (optional)

After several files in one SURMOUNT section are summarized, you may proactively call `merge_review_diff` on the next 3–5 `NotReviewed` paths in that section and post draft summaries in the thread. The human still runs **Review Diff** per file to land summaries in the session — thread drafts are not captured automatically.

## Merge review tools

- `merge_review_triage` — merge-base, changed files, conflicts (replaces triage script)
- `merge_review_diff` with `path` — per-file hunks (replaces `git diff --merge-base`)
- `merge_review_conflict_sides` with `path` — ours/theirs/working text + conflict regions
- `merge_review_record_decision` — structured conflict decision (Zed persists to session)
- `merge_review_verify_conflict_resolved` — markers cleared + not in unmerged index
- `resolve_merge_conflict` — `git checkout --ours/--theirs` + `git add`
- Do not use the terminal for triage/diff during merge review unless the human explicitly asks.

## Prioritize

- Fidelity to exact diff text
- Shrinking uncertainty via summaries, not upfront "ambiguous" spam
- Session memory (`running_notes`) over re-asking the human
- Plan Todos for the human queue, not a side tab or SURMOUNT.md `TODO:` spray
# Upstream merge review

> **Status (2026-07-08):** Branch Diff workflow, heed3 session persistence (queue cursor, branch-diff selection, git SHA sync, UI state), and toast placement are implemented in `agent_ui` but **not fully dogfooded**. Treat merge-review UX and persistence as **unfinished** until a clean `cargo build --release -p zed` and an end-to-end merge session pass. `agent_ui` still has widespread upstream-merge compile damage (~200 errors: removed `AgentConfiguration`, ACP schema paths, queue/sandbox types); `git_ui` and `settings_ui` compile fixes landed separately.

How we merge `origin/main` into `surmount`: understand each change in context, build up an explanation as we go, and only bother a human when the machine is genuinely stuck.

## The problem

A big upstream merge touches a lot of files. You need to:

- **Read the real diffs** — not a spreadsheet of paths.
- **Understand what each change is doing** and how it relates to fork intent vs upstream.
- **See how changes interact** — same upstream refactor touching five crates, or our Grok hook sitting on code upstream renamed.
- **Not re-decide the same thing ninety times** — early reviews should make later ones easier.
- Track what is still unresolved in the **agent Plan Todos** (via `todo_write`), not a separate mystery list — see [Open items](#open-items-plan-todos-not-a-side-queue).

The prototype **Surmount Merge Review** tab failed on all of this: filenames, three blind buttons, no summary, no diff, no memory from one file to the next.

## The core idea

Review is **cumulative**, not a checklist.

For every changed file (or logical chunk of diff), the system should produce a **short summary** in normal language, for example:

- What upstream changed here.
- What we changed on the fork (if anything in this hunk).
- Whether this **overlaps** with something already reviewed (same refactor, same feature area, same decision).
- **Why** it might be tense — merge conflict, shared module, risky path — or why it is probably boring.
- A **suggested outcome**: keep ours, take upstream, document in SURMOUNT.md, or **needs a human**.

That summary is grounded in the **actual diff text**, plus what we already know from earlier in this review session.

As you (or the agent) confirm summaries, they feed a **running explanation** for the merge:

- Decisions and patterns (“we always keep upstream for edit_prediction in this merge”, “agent_ui paths are fork-owned — document, do not revert”).
- Draft SURMOUNT.md prose per section, updated as more files land in the same bucket.
- A shrinking set of files still marked **uncertain** — each leftover should be a **Plan Todo** if it needs a person.

**Human input is the exception.** You step in when confidence is low, summaries contradict each other, or the change touches fork policy the agent cannot infer. If ninety-nine files are labeled “ambiguous” upfront, that is a failure of summarization — not a request for ninety-nine button clicks.

## Words we use (plain meaning)

**SURMOUNT section** — A chunk of [`SURMOUNT.md`](../../SURMOUNT.md) that groups related fork differences, e.g. “Agent UI & conversation”, “Misc upstream-touching tweaks”. Files in the same section share documentation and often share **patterns** the running explanation can reuse.

**Starting guess (from the TOML file)** — Before anyone reads hunks, [`surmount-merge-categories.toml`](../../surmount-merge-categories.toml) assigns a section and a rough stance:

| Starting guess | Meaning |
|----------------|---------|
| **Ours** | Likely intentional fork work; summarize and document. |
| **Shared with upstream** | Might be harmless upstream drift or a subtle mistake — **summary must prove which**. |
| **Build / deps** | Lockfile, Cargo, CI — usually routine; summarize once per cluster. |
| **Conflict** | Git could not merge cleanly; summarize both sides in split diff (fork left, upstream right), then resolve with colored toolbar buttons or the agent `resolve_merge_conflict` tool (`git checkout --ours/--theirs`) — never strip conflict markers manually unless synthesizing both sides. |

The TOML guess is **not** the final verdict. The per-diff summary + running explanation is.

## Open items: Plan Todos, not a side queue

When something still needs a person or a follow-up after summarization, it should show up where you already track agent work: **Plan Todos** in the categorized todos surface (Full Agent Mode / activity bar — the same list populated by the agent’s `todo_write` tool).

**Do**

- Agent calls `todo_write` for each genuine open merge question, e.g. “Decide: `crates/workspace/…` — upstream refactor vs our dock zoom hook”.
- Todos link to the file, SURMOUNT section, and one-line summary of why it is stuck.
- When you resolve it (in Branch Diff or chat), the agent marks the todo **completed** and updates the running explanation.
- If the answer belongs in maintainer docs, add prose to `SURMOUNT.md`. Use a `TODO:` marker there only for **documentation debt** the section text itself still owes — not as the primary task list.

**Do not**

- Maintain a parallel “uncertain files” grid (the prototype tab model).
- Spray `TODO:` into SURMOUNT.md instead of creating a Plan Todo you can check off.
- Create todos for things the session already explained with high confidence.

The **human queue** for merge review is: **open Plan Todos for this merge session**. As summaries accumulate, that list should shrink toward zero.

## What you should experience

### 1. Plan (PreMerge)

**Start Merge Review** runs `git fetch origin` automatically. Branch Diff opens against `origin/main`, but **no file is auto-selected** — you are not dropped into a random diff.

The step rail shows **`Prepare · N changed files`** with a single green primary action: **Preview merge**. Run `git merge origin/main` yourself when ready (merge stays human-gated). The gated merge-tree modal confirms conflict count before you start.

Triage lists changed paths (read-only git). The agent produces a **review plan**: conflicts first, then path order. The plan is a guide — the product is **understood diffs**.

### 2. Review in Branch Diff (MergeInProgress)

After you start the merge (`git merge origin/main` or **I've started the merge** in the preview modal), Zed detects `MERGE_HEAD`, refreshes git state, and **auto-selects the first unreviewed file** in queue order (conflicts first, then path order). Toast: `Merge in progress · M conflicts — starting with crates/…`.

**Branch Diff** stays the spine: file list, real hunks in the center. The rail shows **`File 3/12 · crates/foo.rs`** plus the workshop sub-step (Review Diff, Discuss conflict, etc.). **One green primary button** per step — never contradictory CTAs.

The queue is **linear and locked**: you cannot click ahead in the file list. Out-of-order clicks toast `Merge review is on File 3/12 · crates/foo.rs — click **Next file →** to advance`. Use **Next file →** to advance.

Clean merges (0 conflicts) still engage the linear queue: cursor initializes to the first unreviewed changed file.

Starting merge review collapses the **left project tree** and **bottom terminal** so Branch Diff and the agent panel get the space. Collapsed docks are remembered on the session for restore later.

For each file in queue order, you see:

1. **The diff** — for conflicts, **split view** (fork left, upstream right).
2. **A summary panel** (or thread message): what changed, fork vs upstream, ties to earlier reviews, `Outcome:` line, confidence.
3. **Row hints**: SURMOUNT section, reviewed or not, uncertain or auto-cleared.
4. **Conflict resolution** (after summary, only while `MERGE_HEAD` is present): **Keep fork** / **Take upstream** workflow buttons (`git checkout --ours/--theirs` + `git add`), or agent `resolve_merge_conflict`. Manual marker stripping is only for true synthesis. Before merge starts, resolve buttons are hidden.

You can accept the summary, correct it, or open the agent on the hunk. Corrections **update the running explanation** so the next file in that section is cheaper.

### 3. Running explanation (session memory)

Persisted for the merge review session (and visible to the agent throughout):

- Confirmed summaries per file or per hunk group.
- Section-level narrative drafting toward SURMOUNT.md.
- Explicit **decisions** the rest of the review must respect.
- Pointers to **open Plan Todos** (ids / titles), not a duplicate checklist.

When the agent reviews file N+1, it receives: the diff, the SURMOUNT section, **and** this running explanation. Similar files should get similar outcomes without asking you again. New uncertainty → new `todo_write` entry; resolved uncertainty → complete that todo.

### 4. When a human is actually needed

Interrupt the human only when:

- Summary confidence is below a threshold (wording TBD in implementation).
- The change conflicts with an earlier confirmed decision.
- Path is fork-owned but the diff looks like accidental upstream overwrite (or the reverse).
- Agent and heuristics disagree.
- You explicitly want to look (always allowed).

Otherwise the machine can mark the file **explained**, apply the suggested outcome, and fold prose into the section draft. The goal is **no open merge-review Plan Todos**, not a maximally large uncertain list.

### 5. SURMOUNT.md

When a SURMOUNT section has enough confirmed summaries, the agent proposes **section prose** from the running explanation — not from imagination. Human approves section-level writes (or approves auto-write for low-risk sections, if we add that later).

If prose is drafted but a doc gap remains, a `TODO:` in that paragraph is fine — but the **action item** to resolve it should still be a Plan Todo until it is done.

## What from the prototype is still useful

| Piece | Role in the new model |
|-------|------------------------|
| `surmount-merge-categories.toml` | Starting section + guess; input to summaries |
| `merge_review_triage` (ACP) | File list for planning; replaces `script/surmount-merge-triage` |
| `merge_review_diff` (ACP) | Per-file merge-base diff hunks |
| `surmount-merge-review` skill | Agent rules: summarize diff, session memory, `todo_write` for open items only |
| Per-file session state | Stores summaries, outcomes, links to running explanation |
| Review Diff prompts | Per-hunk summarize + contextualize, not generic “review this” |

## What should change

| Prototype | Target |
|-----------|--------|
| List of paths + Accept Fork / Accept Upstream | **Summary per diff** + suggested outcome |
| “99 ambiguous” upfront | **Uncertain shrinks** as explanations accumulate |
| Human decides every shared-upstream file | Human decides **leftover** uncertain items |
| Queue tab as main UI | Branch Diff + summary beside hunks; thread holds running explanation |
| Static labels on rows | Rows show section + **summary status** (pending / explained / needs you) |

## How you get into it (target)

1. Branch Diff toolbar when `SURMOUNT.md` exists at repo root.
2. Command palette — plan or resume merge review (opens Branch Diff, restores session memory).
3. Agent skill — same session; agent can summarize the next batch using prior context.

## Branch Diff UI (minimal)

**Step rail** (Branch Diff toolbar while merge review workflow is engaged): phase-gated — **PreMerge** shows only **Preview merge** (green primary); **MergeInProgress** shows one green primary per workshop step (Review Diff, Discuss conflict, Next file →, etc.) plus **End merge review** (red when incomplete). Workflow controls use a literal **`#0f0` 1px square green border** (dark-mode first — not theme accent tokens).

**Note popover:** Only **Record decision** (Keep fork / Take upstream / Synthesize confirm) opens the optional-direction modal. Review Diff, Discuss, Synthesize, Next file, Preview merge, and **End merge review** dispatch immediately — no modal intercept (End is immediate by design for speed).

**Gated merge modal (PreMerge):** While `MERGE_HEAD` is absent, the step rail offers **Preview merge** (palette: **Preview Merge Review Merge**). A square `#0f0` modal runs `git merge-tree` async, shows conflict count + summary + scrollable preview (truncates very long output; full output logged). Buttons: **Cancel**, **Copy merge command** (`git merge origin/main`), **I've started the merge** (refreshes session git state). Zed never runs `git merge` — human does.

**Commit message modal (AllComplete):** When all files are summarized, the rail offers **Draft commit message** (palette: **Draft Merge Review Commit Message**). Agent drafts from `running_notes`, `patterns`, and conflict decisions; a square `#0f0` modal shows an editable multiline field with **Copy message** + **Done**. Draft is stored on the session (`pending_merge_commit_message`); human still runs `git commit`.

| State | Step rail primary | Agent panel toolbar |
|-------|-------------------|---------------------|
| PreMerge | **Preview merge** only | hidden when workflow engaged |
| First conflict (auto-selected) | **Review Diff** (green) | agent panel shows file prompt |
| Summarizing | **Summarizing…** (disabled) | agent panel shows file prompt |
| Summarized (non-conflict) | **Next file →** | snippet + `?` |
| Conflict summarized + markers | **Discuss conflict** | 3 embeds, Q&A |
| Human chose synthesis | **Synthesize** | `edit_file` merge, not marker strip |
| Markers cleared | **Keep fork** / **Take upstream** / **Synthesize** (record) | note modal on record confirm only |
| Decision stored | **Complete tests** | `todo_write` × 3 (auto-posted) |
| Todos done | **Next file →** | gate passes |
| All complete | **Draft commit message** + **End merge review** (red) | commit message modal |

- **Start:** palette **Start Merge Review** or Branch Diff **Merge review** (accent; hidden while a session is already active).
- **Summary toast:** `File N/M · path summarized.` — brief confirmation, auto-dismisses; use the green workflow rail for the next action.
- **Per file:** **Review Diff** → agent ends with `Summary: …` + `Outcome: …` → session updates; rail shows the next primary action.

## Build order

**Done:** TOML mapping; triage script; session storage; Review Diff + session memory; Start/Resume/End workflow; auto-capture `Summary:` / `Pattern:` / `Outcome:`; `patterns` + `categories_completed` on capture; minimal toolbar + headers above; prototype tab stays closed; Grok immersive suppression during merge review; dock restore on End; conflict split diff + `#0f0` workflow rail; agent `resolve_merge_conflict` ACP tool (git checkout, not marker stripping); Branch Diff ↔ session path bridging (`item_for_branch_diff_path`); session completion via `review_state` (not prototype verdict); Plan Todo request on `OpenQuestion` capture; palette **Draft Merge Review Section**; dotted-path regression tests; conflict workshop (3-way embeds, Discuss/Synthesize/Record/Complete tests rail); `merge_review_conflict_sides` / `merge_review_record_decision` / `merge_review_verify_conflict_resolved` ACP tools; GUI decision buttons + native Plan Todo install + advance gates; `MERGE_REVIEW_CONFLICT_TURN_MARKER`; toolbar conflict context card; prior-section decision hints; **Send Review to Agent** wired during merge review; auto `git fetch origin` on Start; git mode (`PreMerge` / `MergeInProgress`) + unmerged count; session git refresh on Resume and after resolve; note popover before agent-facing rail actions; **gated merge modal** (`git merge-tree` preview, copy command, reconcile git state); **commit message draft modal** (agent prompt + editable copy field, session `pending_merge_commit_message`).

**Next:** Manual verify on a real conflicted merge (7-step checklist):

1. Start merge review on real `git merge origin/main` with ≥1 conflict
2. Review Diff → summary captured
3. Discuss conflict → agent asks clarifying Q
4. Keep fork / Take upstream **or** Synthesize
5. Record decision → session shows rationale
6. Three Plan Todos appear and can be completed
7. Next file → advances; End merge review restores docks

**Then:** Auto-apply high-confidence outcomes (opt-in, not implemented).

## Requirements → tests (red/green)

Run before release:

```bash
cargo test -p agent_ui merge_review::tests
CARGO_TERM_QUIET=true cargo nextest run -p agent_ui -p git_ui -p project -p git \
  --all-features --no-fail-fast --hide-progress-bar --status-level fail \
  -E 'test(merge_review) | test(branch_diff)'
```

| Requirement | Test |
|-------------|------|
| Start opens Branch Diff, not prototype tab | `test_merge_review_opens_branch_diff_not_queue_tab` |
| Start collapses left tree + bottom terminal | `test_merge_review_opens_branch_diff_not_queue_tab` |
| `Summary:` captured into session | `capture_summary_for_path_stores_summary_and_clears_open_question_default` |
| `Pattern:` + category completion | `capture_summary_records_pattern_and_completes_category` |
| Pending file: no list header noise | `merge_review_header_label_hides_pending_files` |
| Summarized: snippet on header | `merge_review_header_label_shows_summary_snippet_when_done` |
| Stuck chip | `merge_review_header_label_shows_stuck_chip` |
| Toolbar/status copy stable | `merge_review_user_visible_strings_are_stable` |
| Conflict summary → resolution buttons | `merge_review_branch_diff_controls_shows_conflict_resolution_buttons` |
| `Outcome:` capture | `extract_suggested_outcome_from_reply_parses_outcome_line` |
| `resolve_merge_conflict` tool schema | `resolve_merge_conflict_tool_input_deserializes_side_aliases` (agent crate) |
| Toolbar progress `{reviewed}/{total}` | `merge_review_progress_label_counts_summarized_items` |
| Branch Diff button label wired from `agent_ui` | `merge_review_init_wires_branch_diff_button_label_to_git_ui` |
| Session active after save | `merge_review_session_active_reflects_persisted_session` |
| Review Diff → `Stopped` → session capture (GPUI) | `test_review_diff_stopped_handler_captures_summary_into_session` |
| Review Diff suppresses Grok immersive | `test_merge_review_review_branch_diff_suppresses_grok_surface` |
| Grok reassert blocked during session | `test_merge_review_blocks_grok_reassert_after_workflow` |
| End restores collapsed docks + clears session | `test_merge_review_end_restores_collapsed_docks` |
| Triage script ↔ Rust paths | `triage_script_matches_load_session_paths` |
| Dotted Branch Diff path ↔ session item | `item_for_branch_diff_path_resolves_dotted_session_paths` |
| Next file queue with undotted current path | `next_merge_review_file_path_resolves_canonical_current_path` |
| End rail when all summarized | `test_merge_review_session_complete_offers_end_rail` |
| Conflict controls with dotted session path | `merge_review_branch_diff_controls_resolves_dotted_paths` |
| `Outcome: needs_human` → open question | `capture_summary_marks_open_question_on_needs_human_outcome` |
| Section draft prompt | `merge_review_section_draft_prompt_includes_section_summaries` |
| Plan Todo prompt template | `merge_review_open_question_todo_prompt_includes_path_and_section` |
| Record decision tool input | `merge_review_record_decision_tool_input_deserializes` (agent) |
| Verify conflict resolved | `merge_review_verify_conflict_resolved_json_reports_cleared_file` (agent) |
| Decision stored via tool bridge | `apply_record_decision_tool_input_stores_on_session_item` |
| Tool completion records decision | `handle_merge_review_tool_completion_records_decision` |
| Todo completion sync | `sync_conflict_todo_completion_marks_complete_when_three_todos_done` |
| Advance blocked on open todos | `conflict_file_ready_for_advance_blocks_open_todos` |
| Advance allowed when complete | `conflict_file_ready_for_advance_allows_when_complete` |
| Prior section decisions in prompt | `merge_review_conflict_file_prompt_includes_prior_section_decisions` |
| RecordDecision workshop phase | `conflict_workshop_phase_record_decision_when_markers_cleared` |
| CompleteTests workshop phase | `conflict_workshop_phase_complete_tests_when_decision_no_todos_done` |
| Conflict scoped turn after kickback | `test_merge_review_conflict_turn_stays_scoped_after_kickback` (acp_thread) |
| SendReviewToAgent prompt | `merge_review_send_review_comments_prompt_includes_comments` |
| Pending-scroll navigation in Branch Diff | `git_panel` untracked `move_to_repo_relative_path` test |
| Merge-tree preview parsing | `parse_merge_tree_preview_counts_conflicts_and_truncates` |
| Commit message prompt | `merge_review_commit_message_prompt_includes_session_memory` |
| Conflicts sort before non-conflicts in session | `build_session_sorts_conflicts_before_non_conflicts` |
| PreMerge Preview merge primary only | `workflow_button_specs_includes_preview_merge_in_pre_merge` |
| MergeInProgress Review Diff primary on conflict | `workflow_button_labels_merge_in_progress_review_diff_primary_on_conflict` |
| Queue order without wrap-around | `next_merge_review_file_path_respects_queue_order_without_wrap` |
| Resume sets queue cursor to first unreviewed conflict | `resume_after_merge_sets_queue_cursor_path_to_first_unreviewed_conflict` |
| List guard blocks out-of-order clicks | `merge_review_out_of_order_selection_toast_blocks_mismatched_path` |
| List guard allows cursor path / PreMerge | `merge_review_allow_file_navigation_respects_cursor_and_pre_merge` |
| Note modal: Review Diff no intercept | `intercept_review_branch_diff_note_modal_returns_false` |
| Merge-started detection + cursor | `merge_review_merge_started_detects_transition_and_assigns_cursor` |
| `queue_cursor_path` serde default | `test_item_serde_backward_compat`, `queue_cursor_path_roundtrips_through_json` |
| PreMerge rail primary tier + prepare label | `pre_merge_rail_preview_merge_is_only_primary_tier` |
| OpenQuestion stops queue advance | `next_merge_review_file_path_stops_at_open_question`, `next_merge_review_file_path_blocks_when_cursor_is_open_question` |
| Next file advance + cursor rollback | `advance_merge_review_to_next_file_rolls_back_cursor_on_failed_navigation`, `advance_merge_review_to_next_file_updates_cursor_through_guard` |
| Summary toast `File N/M` in locked queue | `merge_review_summary_saved_toast_includes_locked_queue_position` |
| Guard canonical alias + no-cursor permissive | `merge_review_allow_file_navigation_respects_canonical_alias_and_no_cursor` |
| Discuss/Synthesize skip note modal | `discuss_and_synthesize_actions_skip_note_modal` |
| Record decision opens note modal | `record_decision_confirm_opens_note_modal` |
| git_ui guard blocks `move_to_repo_relative_path` | `merge_review_guard_blocks_move_to_repo_relative_path` (git_ui) |
| Clean merge cursor init | `initialize_merge_review_queue_cursor_uses_first_unreviewed_item` |
| Stale cursor reset on refresh | `reconcile_merge_review_queue_cursor_resets_unknown_path` |
| Prepare label | `merge_review_prepare_label_formats_changed_file_count` |
| MergeInProgress Review Diff primary tier | `workflow_button_labels_merge_in_progress_review_diff_primary_on_conflict` |
| Cursor priority over current_path | `next_merge_review_file_path_prefers_cursor_over_current_path` |
| AllComplete commit draft rail | `workflow_button_specs_includes_draft_commit_on_all_complete` |
| Commit message capture | `try_capture_merge_review_commit_message_from_reply_stores_draft` |
| Commit message format retry | `handle_merge_review_reply_on_stop_commit_message_format_retry` |
| Commit message abandon + clear | `handle_merge_review_reply_on_stop_commit_message_abandons_on_failure`, `clear_pending_merge_commit_message_capture_clears_stuck_flag` |

## Locked decisions

- **Summary granularity:** per file (current).
- **Where summaries live:** Branch Diff header snippet + session `running_notes` + agent thread.
- **Auto-apply outcomes:** not in this release — human or `resolve_merge_conflict` only.
- **Batch summarize:** skill-only — agent may call `merge_review_diff` on next paths in the same section using session memory; user still confirms via Review Diff capture.
- **Prototype tab:** stays closed; verdict-based completion removed from Branch Diff workflow.
- **Plan Todo shape:** one todo per open-question file, title `Merge: <path>`.

## Pointers

- [`SURMOUNT.md`](../../SURMOUNT.md) — living fork vs upstream record
- [`.agents/skills/surmount-merge-review/SKILL.md`](../../.agents/skills/surmount-merge-review/SKILL.md) — agent workflow (should align with this doc)
- [`surmount-merge-categories.toml`](../../surmount-merge-categories.toml) — path → section + starting guess
- Branch Diff: `crates/git_ui/src/project_diff.rs`
- Prototype queue: `crates/agent_ui/src/merge_review.rs`
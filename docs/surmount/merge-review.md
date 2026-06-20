# Upstream merge review

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
| **Conflict** | Git could not merge cleanly; summarize both sides, then fix in the editor. |

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

### 1. Plan

You run `git fetch` and `git merge origin/main`.

Triage lists changed paths (read-only git). The agent produces a **review plan**: order of SURMOUNT sections, conflicts first, clusters of related files. The plan is a guide, not the product — the product is **understood diffs**.

### 2. Review in Branch Diff (main UI)

**Branch Diff** against `origin/main` stays the spine: file list, real hunks in the center.

For each file (or when you open it), you see:

1. **The diff** (as today).
2. **A summary panel** (or thread message): what changed, fork vs upstream, ties to earlier reviews, suggested outcome, confidence.
3. **Row hints**: SURMOUNT section, reviewed or not, uncertain or auto-cleared.

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
| `script/surmount-merge-triage` | File list for planning and batch summarize |
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

## Build order

**Done:** TOML mapping, triage script, skill shell, per-path session storage, basic Review Diff prompts.

**Shipped:** **Plan merge review** opens Branch Diff against `origin/main` and posts a plan in the agent thread (prototype file-list tab no longer opens by default).

**Next:** Per-diff summary generation; running explanation in session; show summary + status on Branch Diff rows; remove prototype tab.

**Then:** Auto-apply high-confidence outcomes; section-level SURMOUNT.md draft from accumulated summaries; human queue = open Plan Todos only.

## Still to decide

- **Summary granularity** — one summary per file vs per conflict hunk vs per logical edit in large files.
- **Where summaries live** — beside diff, agent thread only, or both synced.
- **Confidence thresholds** — when auto-clear is allowed without human (policy + tests).
- **Batch summarize** — agent summarizes next 10 files using memory before human opens any (faster shrink of uncertain set).
- **Fate of the prototype tab** — summary dashboard (“12 explained, 3 Plan Todos open”) vs delete.
- **Todo shape** — one todo per file vs per hunk vs per SURMOUNT section cluster.

## Pointers

- [`SURMOUNT.md`](../../SURMOUNT.md) — living fork vs upstream record
- [`.agents/skills/surmount-merge-review/SKILL.md`](../../.agents/skills/surmount-merge-review/SKILL.md) — agent workflow (should align with this doc)
- [`surmount-merge-categories.toml`](../../surmount-merge-categories.toml) — path → section + starting guess
- Branch Diff: `crates/git_ui/src/project_diff.rs`
- Prototype queue: `crates/agent_ui/src/merge_review.rs`
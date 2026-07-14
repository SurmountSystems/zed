# Plan 0 — Agentic dogfooding (TOON)

> The first step towards recursive self improvement (RSI level 1)

**North star:** AI agents use this Zed fork the way a human uses the editor — by **looking**, **acting**, and **looking again** — through a **token-efficient TOON** control plane over agent-stdio. Dogfood is not only CI green; it is how agents **inhabit** Surmount Zed.

**Operator skill:** [`.agents/skills/zed-dogfood/SKILL.md`](../.agents/skills/zed-dogfood/SKILL.md)  
**Wire + gates:** `crates/zed/src/zed/agent_stdio.rs`, `tooling/xtask/src/tasks/dogfood.rs`  
**Maintainer notes:** [`SURMOUNT.md`](../SURMOUNT.md) § Agent stdio  
**First product adventure:** merge review ([`docs/surmount/merge-review.md`](../docs/surmount/merge-review.md))  
**Finished workstreams (archive):** [`0_agentic_dogfooding_done.md`](./0_agentic_dogfooding_done.md) — W0–W3, R1, queue runner + settle rule; doctrine + session shapes live there and in the skill.

---

## Done so far (pointer only)

| Closed | Summary |
|--------|---------|
| **W0** | Plan + honest merge-review status blurb |
| **W1** | Trusted Surmount root open; trust seed; no empty startup window |
| **W2** | `merge-review` workshop: Start → chrome expects → Preview → End (`--start-only` / `--step-wait-ms`) |
| **W3** | Compact/room/rich discipline; field caps; runner previews + stderr filter; skill Evidence table — **no** whole-tree cap, no new bloat methods |
| **R1** | Live green `merge-review` workshop (Start → Preview → End) |
| **Queue** | `cargo xtask dogfood queue` + `merge_review_ux.queue` probe (27/27); settle look-before-Start |

Do not re-open W3 as code. Residual token risk is **parent agent habits**. R1 + queue runner archived in `_done.md` — re-run `merge-review` / queue for regression only.

**Design (finished):** [`plans/finished/merge_review_ux_a11y_design-c4e8a1f2.md`](./finished/merge_review_ux_a11y_design-c4e8a1f2.md) — merge-review a11y / focus / Prepare rail (review + summary alongside).

---

## Remaining work

### R2 — Advance + product a11y (**proven green**; residual stretch only)

G3 **Advance** product chrome + dogfood gates landed. Evidence (release binary): default `merge-review` green; `--with-advance` path/cursor delta ok; `merge_review_ux.queue` 35/35 with AND `expect:` (incl. TextInput, Dialog, Next file).

| Sev | Issue | Status |
|-----|--------|--------|
| H | Focus stuck on `[Window]` | **AC-A** done (Start → Preview merge; Preview → Dialog; ToggleFocus → TextInput). **AC-B** ProjectDiff Branch Diff surface is `Role::Group` + Focusable `track_focus` so room `# focus:` can leave Window after Next file |
| H | No Next file on Prepare rail | Done — Preview primary + Next file →; no Review Diff in PreMerge |
| H | Branch Diff file rows unlabeled | Done — path `aria_label` ≤80 on headers |
| H | NextFile no usable chrome | Done — labels + PreMerge Next file + advance delta |
| M | Branch Diff landmark `0x0` | Done — e.g. `@0,580 648x1` |
| M | Agent panel missing after ToggleFocus | Done — force open + `Role::TextInput` “Agent message” |
| M | Preview no `[Dialog]` | Done — gated merge modal Dialog + focus |
| M | Expand `@y<0` | Done — omit off-viewport Expand from a11y tree |
| L | inventory `active_window: (none)` | Done — `yes (headless-fallback)` |
| Env | fish/neofetch ARG_MAX noise | Soft — skill known-noise note |

- [x] Path labels (global aria ≤80) + Branch Diff landmark bounds >0
- [x] Focus **AC-A** product handle after Start/Next/Preview (Next = product log + ProjectDiff Focusable; room outline after Next reports Group Branch Diff via AC-B)
- [x] PreMerge: Next file available + Preview primary; **no** Review Diff; unit tests rewritten
- [x] Preview `Role::Dialog` (4a); Expand off-screen omit (4b)
- [x] PR5: ToggleFocus → TextInput “Agent message” (composer `Role::TextInput`)
- [x] Dogfood: settle look-before-Start; AND `expect:`; `--with-advance` = path/cursor delta; no Review Diff gate in PreMerge
- [x] Default `merge-review` Start→Preview→End green (no Dialog/Advance required on default CLI)
- [x] **AC-B stretch:** ProjectDiff merge-base root is id + `Role::Group` “Branch Diff” + Focusable `track_focus` (empty = owned handle; non-empty Next file = editor handle via same binding). Room `# focus:` is Group after Next file (proven dogfood); Editor still has no native AccessKit focus registration — residual covered by Group surface. `--with-advance` hard-fails if focus is solely Window after multi-file NextFile success.

**Non-goal:** full per-file ACP loop; synthetic dogfood chrome; OR-only hit gates. Conflict **Decide** has an opt-in fixture path (**R3**).

---

### R3 — Conflict fixture path

Resolve / Discuss / Synthesize headless path via **opt-in** fixture (not live Surmount `MERGE_HEAD` only). Clean Surmount = **PreMerge** (no conflict chrome) by design.

- [x] Small conflict fixture: tempfile builder (`--with-conflict`) + bare offline **origin** + docs under `tooling/xtask/dogfood_fixtures/merge_review_conflict/README.md`
- [x] Soft-gate decision chrome (conflict-specific; not `Review Diff` alone)
- [x] `git::ReviewDiff` → dispatch stderr + rail **Summarizing…** when ACP posts (soft if offline)
- [x] Conflict prompts self-contained (embeds + “do not open skill/tool host paths”); Start skips fetch when no origin remote
- G3 full **Decide** (Discuss → Record → Next file with agent summary) still optional product depth beyond dogfood chrome/dispatch.

---

### R4 — Reliability & CI (was W4)

| Item | Status / action |
|------|-----------------|
| Nightly preflight + golden | Already `.github/workflows/dogfood_preflight.yml` (cron + dispatch) |
| Opt-in `merge-review` | **workflow_dispatch** input `run_merge_review` default **false**; 180s; Surmount checkout fixture; room detail; never on PR |
| Known-noise stderr | Skill residual/noise: in-memory DB / auth stubs must not fail gates |

- [x] Confirm nightly still matches plan (Linux release + preflight + golden) — `dogfood_preflight.yml`: cron `17 6 * * *`, `ubuntu-latest`, release `-p zed`, preflight + golden (schedule / dispatch `run_golden`); **not** on PR path
- [x] Optional: dispatch `run_merge_review` (default false) — long timeout, Surmount workspace fixture, room detail; off PR critical path (not on schedule by default)
- [x] Keep known-noise stderr table in skill; do not fail on in-memory DB / auth noise

**CI doctrine:** merge-review stays off PR critical path (full release build is heavy). Nightly preflight/golden; merge-review only via explicit dispatch.

---

### R5 — Agent skill + product coupling (was W5)

| Contract | Owner |
|----------|--------|
| Operator wire (TOON, detail tiers, evidence) | `zed-dogfood` skill |
| In-Zed review behavior | `surmount-merge-review` skill |
| Proof chrome exists | `cargo xtask dogfood merge-review` |
| No OS–agent coupling | Methods OS-agnostic; non-empty snapshot Linux-primary |

- [x] `zed-dogfood` remains the binding operator contract (queue AND/`hit` OR, settle, `--with-advance`, UX script, CI matrix)
- [x] `surmount-merge-review` describes **in-Zed** review; dogfood proves chrome only — cross-links in both skills + merge-review.md
- [x] No OS–agent coupling wording; non-empty snapshot is Linux-primary (methods stay everywhere)

---

### R6 — Operational gaps (track, not always code)

| Gap | Why it matters |
|-----|----------------|
| Parent agent permissions | Terminal allow-list must include `cargo build --release -p zed`, `cargo xtask dogfood`, and `target/release/zed` so the agent can inhabit without human proxy |
| macOS/Windows non-empty snapshot | Cross-platform inhabit is Linux-first for paint; methods stay OS-agnostic (Plan 0 non-goal for full golden) |

---

## Execution order

```text
R1 + queue closed ──► Plan 0 inhabit #1 met
        │
        ├─► R2 product a11y + AC-B Group focus + --with-advance  **Done**
        │
        ├─► R3 Conflict fixture (--with-conflict soft gate)  **Done (opt-in)**
        │
        └─► R4 nightly preflight/golden + dispatch run_merge_review  **Done (opt-in)**
R5 skill audit ── **Done**
```

| # | Work | Owner | Blocks Plan 0? |
|---|------|-------|----------------|
| 1 | R1 + queue runner + settle rule | **Done** (archive) | Was yes |
| 2 | R2 a11y/focus/Prepare + Advance + AC-B | **Done** (dogfood green; Group focus after NextFile) | Soft |
| 3 | R5 skill cross-link / coupling audit | **Done** | Soft |
| 4 | R4 nightly checkbox confirm | **Done** (matches plan) | Soft |
| 5 | R4 opt-in merge-review CI | **Done** (`run_merge_review` dispatch default false) | No |
| 6 | R3 conflict fixture | **Done** (`--with-conflict` tempfile + soft decision chrome) | No |

---

## Success criteria (Plan 0 done)

1. **Inhabit:** Start → look → preview → end merge-review headless on Linux with non-empty room looks and stable expects (**R1 done**). R2 product a11y + optional Advance expects **proven green** (AC-B after Next file optional stretch).
2. **Tokens:** Default multi-step loops use compact looks; room/rich deliberate (W3 closed — discipline).
3. **No Python drivers:** All operator automation is `cargo xtask dogfood …`.
4. **Docs honest:** merge-review + SURMOUNT + this plan match dogfood reality.
5. **Evidence culture:** UI claims cite look/stderr evidence (previews, expect hits, filtered stderr), not vibes.

### G3 merge-review milestones (status)

| Milestone | Done when | Status |
|-----------|-----------|--------|
| **Start** | Headless Start populates queue, Branch Diff, plan | **R1 green** |
| **See** | Room look: merge toolbar / Base ref chrome | **R1 green** |
| **Preview** | `PreviewMergeReviewMerge` + chrome | **R1 green** |
| **Advance** | Path labels + Next file chrome; path/cursor delta; Review Diff when MergeInProgress | **R2 green** (`--with-advance` + queue) |
| **Decide** | Conflict/decision actions with fixtures | **R3** |
| **End** | `EndMergeReview` restores layout | **R1 green** |

---

## Explicit non-work

- Do not “fix” token bloat with whole-tree caps or fatter TOON fields (W3 closed).
- Do not add shell/Python adventure drivers.
- Do not put full release merge-review on PR CI.
- Do not ask the human to run dogfood — the agent runs inhabit gates; only escalate if the binary cannot be built or the harness is broken.
- Full macOS/Windows headless a11y golden; per-control paint; replacing ACP tools with TOON.

---

## Related plans

| Plan | Topic |
|------|--------|
| **0** (this) | Residual agentic dogfooding / TOON inhabit |
| **0 done** | [`0_agentic_dogfooding_done.md`](./0_agentic_dogfooding_done.md) — closed W0–W3, R1, queue |
| Later | Merge-review product completion may split to `plans/1_merge_review_workshop.md` once inhabit is boringly reliable |

Pointer from strategy: [`PLAN.md`](../PLAN.md) remains Grok-native strategic overview; **this file** is the operational residual plan for agents living inside Zed via TOON.

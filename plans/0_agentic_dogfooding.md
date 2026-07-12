# Plan 0 — Agentic dogfooding (TOON)

> The first step towards recursive self improvement (RSI level 1)

**North star:** AI agents use this Zed fork the way a human uses the editor — by **looking**, **acting**, and **looking again** — through a **token-efficient TOON** control plane over agent-stdio. Dogfood is not only CI green; it is how agents **inhabit** Surmount Zed.

**Operator skill:** [`.agents/skills/zed-dogfood/SKILL.md`](../.agents/skills/zed-dogfood/SKILL.md)  
**Wire + gates:** `crates/zed/src/zed/agent_stdio.rs`, `tooling/xtask/src/tasks/dogfood.rs`  
**Maintainer notes:** [`SURMOUNT.md`](../SURMOUNT.md) § Agent stdio  
**First product adventure:** merge review ([`docs/surmount/merge-review.md`](../docs/surmount/merge-review.md))  
**Finished workstreams (archive):** [`0_agentic_dogfooding_done.md`](./0_agentic_dogfooding_done.md) — W0–W3 closed; doctrine + session shapes live there and in the skill.

---

## Done so far (pointer only)

| Closed | Summary |
|--------|---------|
| **W0** | Plan + honest merge-review status blurb |
| **W1** | Trusted Surmount root open; trust seed; no empty startup window |
| **W2** | `merge-review` workshop: Start → chrome expects → Preview → End (`--start-only` / `--step-wait-ms`) |
| **W3** | Compact/room/rich discipline; field caps; runner previews + stderr filter; skill Evidence table — **no** whole-tree cap, no new bloat methods |
| **R1** | Live green `merge-review` workshop on current release binary (Start → Preview → End; room looks + expects) |

Do not re-open W3 as code. Residual token risk is **parent agent habits** (paste full trees / full stderr). R1 is closed — re-run `merge-review` for regression only.

---

## Remaining work

### R2 — Advance step (optional code depth)

G3 **Advance** still open: `surmount::MergeReviewNextFile` + Review Diff path observable in look / stderr.

| Gate before coding | Prefer |
|--------------------|--------|
| Role/label chrome (`Next file`, `Review Diff`) | `--expect` after action |
| Product stderr after dispatch | merge-review stderr filter |
| Needs agent/thread focus without fixture | **block** until assertable |

- [ ] Assertable focus/chrome signal identified (look and/or filtered stderr)
- [ ] Optional xtask step: after Start (and optional Preview), dispatch Next file / Review Diff; poll expects
- [ ] Keep behind `--with-advance` (or equivalent) until non-flaky; then fold into default workshop if stable
- Files: `tooling/xtask/src/tasks/dogfood.rs` (`run_merge_review_workshop_steps`); skill session table; this plan

**Non-goal:** full per-file review loop or ACP tool simulation in dogfood.

---

### R3 — Conflict fixture path (defer; not Plan 0 wire blocker)

Resolve / Discuss / Synthesize untested headless (needs `MERGE_HEAD` + conflicted paths).

- [ ] Small conflict fixture (not the live Surmount tree as the only path)
- [ ] Dogfood or product gate for decision actions when fixture exists
- Leave open; do not block Plan 0 “done” on G3 **Decide** if Start → Preview → End (+ optional Advance) is green.

---

### R4 — Reliability & CI (was W4)

| Item | Status / action |
|------|-----------------|
| Nightly preflight + golden | Already `.github/workflows/dogfood_preflight.yml` (cron + dispatch) |
| Scheduled `merge-review` | **R1 green** — optional when non-flaky: long timeout, Surmount workspace fixture, room detail; expects only |
| Known-noise stderr | Skill residual/noise: in-memory DB / auth stubs must not fail gates |

- [ ] Confirm nightly still matches plan (Linux release + preflight + golden)
- [ ] Optional: scheduled `merge-review` adventure when expects are non-flaky
- [ ] Keep known-noise stderr table in skill; do not fail on in-memory DB / auth noise

**CI doctrine:** merge-review stays off PR critical path (full release build is heavy). Nightly / workflow_dispatch only.

---

### R5 — Agent skill + product coupling (was W5)

| Contract | Owner |
|----------|--------|
| Operator wire (TOON, detail tiers, evidence) | `zed-dogfood` skill |
| In-Zed review behavior | `surmount-merge-review` skill |
| Proof chrome exists | `cargo xtask dogfood merge-review` |
| No OS–agent coupling | Methods OS-agnostic; non-empty snapshot Linux-primary |

- [ ] `zed-dogfood` remains the binding operator contract
- [ ] `surmount-merge-review` describes **in-Zed** review; dogfood proves chrome only — cross-link skills if missing
- [ ] No OS–agent coupling wording; non-empty snapshot is Linux-primary (methods stay everywhere)

---

### R6 — Operational gaps (track, not always code)

| Gap | Why it matters |
|-----|----------------|
| Parent agent permissions | Terminal allow-list must include `cargo build --release -p zed`, `cargo xtask dogfood`, and `target/release/zed` so the agent can inhabit without human proxy |
| macOS/Windows non-empty snapshot | Cross-platform inhabit is Linux-first for paint; methods stay OS-agnostic (Plan 0 non-goal for full golden) |

---

## Execution order

```text
R1 closed (agent dogfood green) ──► Plan 0 success #1 met for inhabit
        │
        ├─► R2 Advance (only if focus assertable)
        │
        ├─► R3 Conflict fixture (later)
        │
        └─► R4 schedule merge-review only if non-flaky
R5 skill audit ── parallel anytime (docs)
```

| # | Work | Owner | Blocks Plan 0? |
|---|------|-------|----------------|
| 1 | R1 live merge-review workshop | **Done** (agent dogfood) | Was yes |
| 2 | R5 skill cross-link / coupling audit | Agent (docs) | Soft |
| 3 | R4 nightly checkbox confirm | Agent after CI reality check | Soft |
| 4 | R2 Advance step | Agent (xtask) if focus assertable | No (stretch) |
| 5 | R4 scheduled merge-review | Human approve + workflow edit | No |
| 6 | R3 conflict fixture | Later product path | No |

---

## Success criteria (Plan 0 done)

1. **Inhabit:** Start → look → preview → end merge-review headless on Linux with non-empty room looks and stable expects (**R1 done**). Advance (R2) is stretch toward full G3 table.
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
| **Advance** | Next file + Review Diff observable | **R2** |
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
| **0 done** | [`0_agentic_dogfooding_done.md`](./0_agentic_dogfooding_done.md) — closed W0–W3 |
| Later | Merge-review product completion may split to `plans/1_merge_review_workshop.md` once inhabit is boringly reliable |

Pointer from strategy: [`PLAN.md`](../PLAN.md) remains Grok-native strategic overview; **this file** is the operational residual plan for agents living inside Zed via TOON.

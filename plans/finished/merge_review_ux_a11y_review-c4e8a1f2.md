# Design Document Review: Merge-Review / Agent-Stdio UX & A11y (Dogfood TOON)

**Reviewed:** `.tmp/design/grok-design-doc-c4e8a1f2.md` (revision 3)  
**Summary:** `.tmp/design/grok-design-summary-c4e8a1f2.md`  
**Plan 0:** `plans/0_agentic_dogfooding.md`  
**Method:** Re-checked design body for prior open items only; ignored review-file status flips.

### Summary

**Verdict: approve.** Revision 3 closes every prior open issue in the design document itself. The design is complete enough for implementation: realistic PR order, closed Key Decisions, AND `expect:` dogfood gates, AC-A/AC-B focus split, PR4a/PR4b split, PR5 diagnosis-first, and a delta-based `--with-advance` predicate.

### Prior open issues — verified fixed in design body

| Prior issue | Design evidence (rev 3) |
|-------------|-------------------------|
| `--with-advance` false-pass on static `Next file` | K8; predicate table: pre-capture path fingerprint → NextFile → success only on **path/cursor delta** (outline different path, log `advanced to next file {path}` ≠ pre-capture, or stderr cursor change); failure if only rail chrome; single-file = skip/fail explicit, not green |
| PR1 hard-coded `expect:crates/` | Per-PR table + PR1 bash: pin basename/path fragment **from post-Start look**; queue fixture comments same |
| PR4b overclaim without hard-fail | **Done when** + dogfood: prefer automated Expand negative-Y fail; else **manual residual** — do not claim Medium closed on vibes |
| PR2 FocusHandle optional | Focus policy + PR2 Changes: **must** add/track `FocusHandle` on Preview button or rail primary if not focusable — required deliverable |
| Plan 0 High-focus residual | Living plan residual cell: **AC-A** product focus_handle; **AC-B** outline non-Window stretch; dogfood bullet requires Advance **path/cursor delta** |

### Strengths
- Normative dogfood semantics (`expect:` AND gate vs `hit:` diagnostic OR) prevent R1 chrome false-pass.
- PreMerge K2 + named unit-test rewrite table is implementable without silent assertion deletion.
- Surmount constraints held: native GPUI, xtask-only dogfood, role/label expects, no OS–agent coupling, R3 conflicts deferred, default `merge-review` stays Start→Preview→End.
- PR graph is parallelizable (PR1/PR3/PR4a/PR4b) with clear soft deps into PR6.

### Implementation notes (non-blocking; not open design issues)
- Open Questions #3 (Expand layout vs omit) and #5 (PR5 diagnosis A/B) are correctly deferred to implementation evidence.
- Single-file Advance “skip with log **or** fail with message” leaves a small implementer choice; either is fine if not reported as green success.
- Summary title still says “rev 2” while body/doc are rev 3 — cosmetic only.

### Verdict

**approve** — no remaining design blockers. Proceed to PR1+ when ready.

OPEN_COUNT=0

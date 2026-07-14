# Design summary — Merge-Review / Agent-Stdio UX & A11y (rev 2)

**Doc:** `.tmp/design/grok-design-doc-c4e8a1f2.md` (rev 3)  
**Review:** `.tmp/design/grok-design-review-c4e8a1f2.md`

## Problem
Live TOON queue proves R1 + queue runner, but chrome is not agent-selectable (focus Window, unlabeled paths, Prepare-only Preview, 0x0 landmark, no Dialog, Expand off-screen, agent panel gap).

## Solution spine (rev 3)
1. AND `expect:` gates — never OR `hit:` as pass.
2. K2: PreMerge Preview primary + Next file available; no Review Diff; rewrite tests.
3. Focus AC-A required; AC-B stretch; PR2 adds FocusHandle if primary not focusable.
4. PR4a Dialog ∥ PR4b Expand (manual residual until bounds parser).
5. PR5 diagnosis A/B before code.
6. Global path aria ≤80; landmark bounds >0; PR1 path expect **pinned from look** (not hard-coded `crates/`).
7. **`--with-advance`:** post-NextFile **path/cursor delta** required — not mere `Next file` label.
8. PR7 plan hygiene only.

## PR order
PR1 → PR2 AC-A → PR3 → PR4a ∥ PR4b → PR5 → PR6 (delta Advance) → PR7.

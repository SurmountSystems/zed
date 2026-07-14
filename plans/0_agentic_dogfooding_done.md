# Plan 0 — Finished work (archive)

> Token-efficient archive of **completed** Plan 0 workstreams. Living residual work lives only in [`0_agentic_dogfooding.md`](./0_agentic_dogfooding.md).

**Operator skill:** [`.agents/skills/zed-dogfood/SKILL.md`](../.agents/skills/zed-dogfood/SKILL.md)  
**Wire + gates:** `crates/zed/src/zed/agent_stdio.rs`, `tooling/xtask/src/tasks/dogfood.rs`  
**Maintainer notes:** [`SURMOUNT.md`](../SURMOUNT.md) § Agent stdio

---

## North star (context)

AI agents inhabit Surmount Zed via **look → act → look** over a token-efficient **TOON** control plane on agent-stdio. Dogfood is Rust xtask + TOON, not shell/Python drivers.

---

## Goals (landed doctrine)

### G1 — Token-efficient inhabit loop

```text
spawn → ready → look → act → wait? → look → … → shutdown
```

| Preference | Meaning |
|------------|---------|
| TOON over JSON blobs | Small fielded gestures; blank-line documents |
| Retained process | One Zed per workflow |
| Detail tiers | `compact` default for multi-step loops; `rich` for click; `room` for place/landmarks |
| Role/label expects | Stable chrome (`Button`, `Merge review`), not bounds/HSLA |
| Look → act → look | Empty snapshot often means settle, not “UI missing” |

### G2 — Operator contract is Rust

| Do | Do not |
|----|--------|
| `cargo xtask dogfood preflight \| golden \| smoke \| merge-review` | Shell/Python adventure drivers |
| `ZED_BIN` / `--bin` → `target/release/zed` | Parallel protocol clients in random languages |
| Extend `tooling/xtask/src/tasks/dogfood.rs` | GUI automation frameworks for dogfood |

### G4 — Token budget (wire + discipline)

| Layer | Intent | Status |
|-------|--------|--------|
| **compact look** | Default multi-step loops | Skill session discipline |
| **room look** | After open / Start / dock change; empty-paint debug | Skill |
| **rich look** | Once for click targets | Skill |
| **Field caps** | `OUTLINE_STRING_MAX = 80` in `gpui` a11y | Intentional; do not “fix” by dumping full labels |
| **Whole tree** | **No** node/line cap | Parent must not paste full `snapshot@text` |
| **stderr** | Product filter only | Runner + skill Evidence table |
| **Runner previews** | `extract_snapshot_preview` | Prefer `preview=` over full logs |

**Agent discipline residual (not missing wire):** prefer runner previews and expect hits; filtered merge-review stderr; escalate detail only when stuck; do not invent whole-tree caps or bloat TOON fields.

---

## Closed workstreams

### W0 — Documentation spine

- [x] `plans/0_agentic_dogfooding.md` operational plan
- [x] Status kept current; long protocol tables live in skill + SURMOUNT
- [x] `docs/surmount/merge-review.md` status blurb: Start proven + workshop code path; full green still required for Plan 0 done

### W1 — Workspace open quality

Agents land in a **real Surmount project**, not empty project + untrusted single file.

- [x] Dogfood open path: Surmount **workspace root** (`resolve_surmount_workspace`); `method:open` uses `ExistingWindow`
- [x] Restricted Mode / trust: agent-stdio seeds `session.trust_all_worktrees: true` when settings.json is absent
- [x] One workspace window: skip empty `open_new` at startup; first open creates the window

### W2 — Merge-review adventure (Start / Preview / End)

Extend `cargo xtask dogfood merge-review` without shell drivers:

| Step | Action / expect | Status |
|------|-----------------|--------|
| Start | `surmount::StartMergeReview` | Done |
| Expect chrome | default `--expect "Merge review"` when CLI empty | Done |
| Preview | `surmount::PreviewMergeReviewMerge` + expect `Preview merge` (unless `--start-only`) | Done |
| End | `surmount::EndMergeReview` + non-empty post-end look (unless `--start-only`) | Done |
| Advance / conflict | Review Diff / Next file / conflict fixture | Residual **R2/R3** living plan |

- [x] Default chrome expects + Preview/End workshop in xtask (`--start-only` / `--step-wait-ms`)
- [x] **R1 live green:** `cargo xtask dogfood merge-review` Start → Preview → End on release binary (room looks + expects)

### W3 — Token efficiency of the wire

- [x] Default agent loops to **compact**; room after open/Start/dock; rich for click — skill session discipline
- [x] Per-field truncation intentional (`OUTLINE_STRING_MAX` = 80); detail tiers + room landmarks only — **no** whole-tree node cap
- [x] Runner prints **previews** (`extract_snapshot_preview`); merge-review filters product stderr; skill **Evidence / token discipline** binds parent agents
- [x] No bloat methods — optional focused-window-only look only if it later **reduces** turns (explicit non-goal until then)

**W3 residual = agent behavior, not missing wire.** No further Rust for this slice.

### Queue runner + UX probe settle (landed)

- [x] `cargo xtask dogfood queue` in `tooling/xtask/src/tasks/dogfood.rs` — steps `open|wait|action|look|expect|hit|lines|inventory|theme|click|stderr:merge|poll`, `--script`, tracking `[queue i/n]`
- [x] Fixture `tooling/xtask/dogfood_queues/merge_review_ux.queue` — live UX probe **27/27** steps
- [x] Skill queue docs in `.agents/skills/zed-dogfood/SKILL.md`
- [x] **Settle rule (proven):** post-open **look (force-draw)** required before `surmount::StartMergeReview` or Start can no-op without chrome

Residual product a11y from that probe → living plan **R2** (design [`plans/finished/merge_review_ux_a11y_design-c4e8a1f2.md`](./finished/merge_review_ux_a11y_design-c4e8a1f2.md)).

---

## Working baseline (when archived)

- Agent-stdio is a **default** feature; release `zed` speaks TOON.
- Methods: `snapshot`/`look`, `inventory`, `click`, `theme`/`feel`, `actions`, `open`, `wait`, `action`, `keys`, `shutdown`.
- Gates: preflight, golden, smoke; **merge-review adventure** in xtask (Start → expects → Preview → End).
- Linux headless force-draw + a11y outline (primary dogfood platform).
- Earlier live dogfood: `StartMergeReview` → populated queue → Branch Diff vs `origin/main` → plan posted; room look showed Toolbar "Merge review" and Base: origin/main.
- Nightly workflow exists: `.github/workflows/dogfood_preflight.yml` (release build + preflight + golden).

---

## Protocol doctrine (still binding; full detail in skill)

1. **TOON is touch** — each request is a gesture against retained state.
2. **Structure from a11y** — never invent per-control CSS from outline.
3. **Global ambience only** via `theme`/`feel`.
4. **Poll on the runner** — no server wait-until.
5. **Named actions** — double-colon names (`surmount::StartMergeReview`).

### Session shapes (landed)

| Shape | Command | Use |
|-------|---------|-----|
| Alive? | `dogfood preflight` | Binary + ready event |
| Protocol smoke | `dogfood golden` / `smoke` | open → wait → look (+ optional action/keys) |
| Merge workshop | `dogfood merge-review` | Surmount root → Start → chrome expects → Preview → End |
| Start-only | `dogfood merge-review --start-only` | Skip Preview/End when debugging open/populate |
| Queue | `dogfood queue --script …` | Agent TOON step runner; UX probes |

### Parent-agent rules (landed)

- Autonomy first: next work is code/docs/gates the agent can land.
- Drive dogfood with **Rust xtask** only — the **agent executes** dogfood; do not ask the human to run headless Zed.
- Evidence = short observations (expect hits, 3–10 outline lines, filtered stderr), not full TOON transcripts.
- When the release binary is missing/stale, the agent rebuilds and runs dogfood (`.rules` dogfood exception); do not park inhabit on human proxy ops.
- **R1 closed:** live `merge-review` Start→Preview→End green on release binary; skill Agent verify lists the regression command.

### Token-efficient look policy

```text
default loop:  detail:compact
after open / StartMergeReview / dock change:  one detail:room
need click target:  detail:rich once, then click, then compact
```

---

## Non-goals already accepted for Plan 0

- Full macOS/Windows headless a11y golden (track separately if needed).
- Per-control paint / CSS-from-outline.
- Replacing ACP agent tools with TOON.
- Agent-authored arbitrary Python for dogfood.
- Shipping merge-review as “done” without live green workshop path.

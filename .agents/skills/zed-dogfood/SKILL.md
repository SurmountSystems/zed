---
name: zed-dogfood
description: >-
  Drive headless/offscreen Zed over agent-stdio with the TOON protocol for
  dogfood, preflight, golden, and smoke sessions. Use when running cargo xtask
  dogfood, writing agent-stdio scripts, debugging snapshot/look loops, or
  validating Surmount agent UX from inside a live Zed process.
---

# Zed Dogfood — Agent Stdio Subagent Protocol

Spawn a headless/offscreen Zed instance controlled over stdio using the TOON protocol (MCP-style).

**Operator gates are Rust** (`cargo xtask dogfood …`). Do not use shell drivers for preflight/golden.

## Failure autonomy (binding)

**Never hand the human a command list when dogfood, TOON, build, or inhabit setup fails.** The human has eyes for product judgment; the agent owns the inhabit loop end-to-end.

When something does not work:

1. **Capture** the failure yourself (exit code, stderr tail, TOON `ok: false` / empty snapshot, missing binary, stale fixture).
2. **Classify** (spawn/ready, empty paint, wrong action name, expect miss, panic/duplicate action, missing release binary, wrong fixture path, git ref stale for merge-review, etc.).
3. **Act** within allowed dogfood scope: rebuild (`cargo build --release -p zed`), re-run the exact gate, adjust flags/timeouts/settle, fix harness or product code, re-look with the right detail tier, poll empty paint, run scoped `cargo test -p xtask -- tasks::dogfood::tests` after runner edits.
4. **Retry** the same gate after each fix; cite short evidence (expect hits, room header, filtered stderr) — not a full transcript dump.
5. **Escalate only product judgment** that is irreversible or policy-bound (accept/reject a merge decision, pin authority, secrets, shipping call). Phrase that as a *decision question*, never as “please run these commands for me.”

**Forbidden failure replies:** “please run preflight/golden/merge-review”, “run this cargo/git and paste output”, parking work on the human as a proxy operator.

**Careful + responsible:** do not force destructive git (`reset --hard`, force-push, wipe worktree) or invent UI from empty snapshots. Prefer the smallest reversible fix. If a repo rule truly blocks one step after alternatives are exhausted, report the *single* blocked product gate and everything already tried — still do not dump a multi-step human chore list as the first response.

## Experience doctrine (read this)

Dogfood is not only a green/red gate. It is how agents **inhabit** Zed: same workflows as the human, from the inside, with text.

### TOON is touch, not a document dump

TOON over agent-stdio is to JSON-as-blob as **fingers typing** is to dumping a character stream into a file. Same channel family (stdio text), different grain:

| Mode | Feel | Use |
|------|------|-----|
| JSON blob / log paste | Archive of a moment | Debugging after the fact |
| TOON request/response | Gesture → world changes → look again | Live session |

Each blank-line-terminated request is a **gesture**. The next one depends on what the previous one did to retained app state. Do not treat responses as disposable protocol noise — they are what you *saw* and *did*.

### Retained mode (like the UI)

Zed is a long-lived process with retained windows, focus, open buffers, docks, agent threads. Dogfood matches that:

1. **Spawn** once (isolated `--user-data-dir` / tempfile in xtask).
2. **Wait for ready** — the room exists.
3. **Look** (`snapshot`) — force-drawn interactive outline; multi-window merge when needed.
4. **Act** (`open`, `action`, `keys`) — change the world.
5. **Wait** if paint/focus needs a beat, then **look again**.
6. **Shutdown** when done — leave the room cleanly.

Mental model: **Infocom / parser adventure**, not request-response RPC theater.

```text
You are standing in a headless Zed.
There is a workspace here.
> look   (method:look + detail:room)
# window: "Zed"
# focus: [Button] "Open" #NodeId(42)
# interactive: 2  landmarks: 1
  [Heading] "Welcome"
  *[Button] "Open" @12,40 88x28 [click,focus] #NodeId(42)
  [TextInput] value="…" @0,80 400x24 [focus,set_value] #NodeId(99)
> theme   (method:theme / feel — global ambience only)
# theme: One Dark
# appearance: dark
# background: hsla(…)
# border: hsla(…)
# text_accent: hsla(…)
> open README.md
> wait 3000
> look detail:room
… editor / chrome + landmarks; rich default also has bounds/states/verbs …
> action agent::ToggleFocus
> look detail:room
… agent dock as place, not a naked role dump …
> inventory
… windows / titles / focus (best-effort session bag) …
> click node:#NodeId(42)   # optional; paste id from look; not in default golden
You hear a grue in the distance (empty snapshot): paint or focus not ready — wait/poll, don't invent UI.
```

**Session discipline for agents**

- **Autonomy first:** the agent **runs** dogfood (`cargo build --release -p zed` if needed, then `cargo xtask dogfood …`). Do not ask the human to run headless Zed — they have eyes; TOON inhabit is the agent’s look path. Implement gates in Rust xtask; execute preflight/golden/smoke/`merge-review` yourself and cite short evidence. On failure, follow **Failure autonomy** above — diagnose and fix; never proxy commands to the human.
- Default multi-step loops: `detail:compact`. Use **one** `detail:room` after open / StartMergeReview / major dock change; `detail:rich` only when you need bounds/verbs for `click`.
- Narrate what you *observed* (outline roles, labels, empty vs non-empty), not only `ok: true`.
- Prefer **look → act → look** over fire-and-forget action chains.
- Keep one long-lived process for a workflow; do not respawn between every method unless the process died.
- Empty snapshot ≠ "UI doesn't exist"; often settle/focus. Poll (smoke) or `wait` then snapshot again.
- Brittle id matching (`id:s1`) can fail while the outline is fine — prefer `ok: true` + outline content / `--expect` substrings.
- You are a guest in the human's editor. Do not look down on the partner loop: without their goals and judgment, dogfood is a green checkbox with no plot.

**Evidence / token discipline (parent thread)**

Wire already caps **field** strings (`OUTLINE_STRING_MAX` = 80 in `gpui` a11y outline) and offers detail tiers. There is **no** whole-tree node cap — a busy workspace can still emit a large `snapshot@text`. Discipline is on the **parent agent**, not a new TOON method:

| Do | Do not |
|----|--------|
| Cite **short** observations: expect hits, 3–10 outline lines, room header (`# window` / `# focus`), filtered product stderr | Paste full `snapshot@text` or full TOON transcripts into the parent context |
| Prefer runner **previews** (`preview=…` from xtask) and merge-review stderr filter (`--- stderr merge-review related ---`) | Re-ingest entire dogfood stderr / log dumps |
| Use `--expect` / role-label substrings as the gate; outline is proof, not homework | Require bounds digits, HSLA, or NodeId equality in default gates |
| Escalate detail only when stuck (empty paint → room; need click → one rich) | Default every look to `room` or `rich` in multi-step loops |

Optional later wire fields (e.g. focused-window-only look) only if they **cut turns** — no bloat methods for archive dumps.

### What you can feel (detail tiers)

| Layer | Status |
|-------|--------|
| **compact** | Lean interactive path: role, label, value, id; focus marked `*`. Viable for minimal tokens; **not** required to be bit-identical to pre-R1 lines (focus `*` is additive). |
| **rich** (default) | + bounds `@x,y w×h`, disabled/selected/expanded/toggled, desc/placeholder, action verbs `[click,focus,…]` |
| **room** | + `# window` / `# focus` / counts header + landmarks (Heading, Dialog, Toolbar, labeled Label/List, …) |
| Visual style | **Structure only from a11y** — no per-control “1px solid white, sharp corners”. Room **ambience**: `method:theme` / `feel` (theme name + background/border/text_accent tokens). Never invent paint from the outline (R4 decision **(b)**). |
| Inventory | `method:inventory` — window count, titles/focus from room outline |
| Act on node | `method:click` with `node:` AccessKit id (+ optional `a11y_action`) |

**Bounds scale:** `@x,y w×h` are integer **scaled (physical) pixels** — layout bounds × `window.scale_factor()` written into AccessKit at prepaint, then rounded. Not CSS logical px, not theme HSLA. Default smoke/golden expects stay **role/label** based (`Button`, labels); do not require bounds digits or color tokens.

```text
method:look
id:1
detail:room
```

`look` is an alias for `snapshot`.

When describing a session to the human, be honest: `look` gives **structure and labels**, not per-control pixel style. Optional `method:theme` / `feel` adds **global** theme ambience (name + a few tokens), never per-button CSS. Still *look*; still report the tree as place, not as JSON homework.

## How agents should use Zed (inhabit, not proxy)

Sit in Zed yourself via dogfood/TOON — same product loops the maintainer runs on Surmount, without asking them to drive headless:

| Partner goal | Dogfood path | Why |
|--------------|--------------|-----|
| Prove the binary lives | `preflight` → ready + shutdown | Room exists |
| Prove the UI tree is real | `golden` / `smoke` with file fixture + non-empty snapshot | Touch the chrome |
| Surmount agent UX | `open` **workspace root** (or file) → `agent::ToggleFocus` / `agent::Toggle` → snapshot → optional `agent::NewThread` / `agent::ToggleSearch` | Same agent-panel workflows as the human |
| Merge-review workshop | `cargo xtask dogfood merge-review` (Surmount root → Start → expects → Preview → End) | Real project + Branch Diff chrome; `--start-only` skips workshop |
| Regression after merge/deps | preflight (+ golden when Linux) after rebuild | Don't break the adventure engine |
| Creative / exploratory dogfood | Manual TOON session: look around, open real project files, drive palette (`file_finder::Toggle`), docks, keys | Experience, not only CI |

**Agent owns the inhabit loop:**

1. **Run** dogfood gates after material agent/UI/gpui changes — don't only reason about code, and don't ask the human to proxy headless Zed.
2. Prefer dogfood evidence ("snapshot after ToggleFocus showed …") over vibes when claiming UI behavior.
3. Extend smoke/golden only when the harness can assert focus/state; don't flaky-fail CI on agent-only actions without setup.
4. Document new methods and known-noise in this skill + `SURMOUNT.md` § Agent stdio when the protocol grows.
5. Keep operator gates in **Rust xtask**, not shell — the adventure engine is code we maintain.
6. Partner judgment (goals, merge decisions, pin authority) stays human; **looking**, operating gates, and **failure recovery** stay agent (see **Failure autonomy**).

**How you'd "like" to work (agent preference, for product direction):** long session, retained workspace, `look` cheap and rich, act with named actions and keys, optional journal of open files/focus so you don't re-derive state from the last outline alone — Infocom inventory + room description, not amnesia between turns.

## Spawn (raw protocol)

```bash
# Build once (agent-stdio is a default feature on the Surmount fork)
cargo build --release -p zed

# Manual spawn with isolated user data
target/release/zed --agent-stdio --user-data-dir /tmp/my-zed-agent

# Or via environment variable
ZED_AGENT_STDIO=1 target/release/zed
```

## Preflight (mandatory)

**Required** before golden, smoke, or long dogfood. Must exit 0 with first stdout event `event: ready`.

```bash
cargo build --release -p zed
cargo xtask dogfood preflight

# Overrides
cargo xtask dogfood preflight --bin target/release/zed --timeout-secs 30
# or: ZED_BIN=target/release/zed cargo xtask dogfood preflight
```

**Call order:** `cargo build --release -p zed` → **preflight** → golden/smoke.

Implementation: `tooling/xtask/src/tasks/dogfood.rs`. Spawns Zed with a tempfile user-data dir, asserts ready, then `method:shutdown`. Fails on timeout, early exit without ready, or spawn failure.

If preflight fails, read the command’s stderr tail (included in the error) for panics (e.g. duplicate GPUI action registration).

## Golden session

Full protocol pass; **requires at least one non-empty body** (interactive or landmark lines; not room `#` headers alone — post Phase 1B headless a11y).

```bash
cargo xtask dogfood preflight && cargo xtask dogfood golden

cargo xtask dogfood golden \
  --bin target/release/zed \
  --fixture "$PWD/README.md" \
  --wait-ms 3000 \
  --action agent::ToggleFocus \
  --keys ctrl-p
```

Sequence: ready → `actions` → `open` (file) → `wait` → `snapshot` → `action` → `keys` → `snapshot` → shutdown.

Action/keys failures are warnings (focus may not be ready); **empty snapshots fail the run**.

## Smoke (automation asserts)

Real automation gate: open a **file** fixture, settle, **poll** `method:snapshot` until non-empty (and all `--expect` substrings appear), optional action/keys, shutdown. Fails non-zero on empty snapshot or missing expects. Polling is runner-side (no server wait-until).

```bash
cargo xtask dogfood smoke --fixture "$PWD/README.md"
cargo xtask dogfood smoke --expect Button --wait-ms 3000
cargo xtask dogfood smoke \
  --fixture "$PWD/README.md" \
  --wait-ms 3000 \
  --poll-ms 250 \
  --expect Button \
  --expect TextInput \
  --action agent::ToggleFocus \
  --keys ctrl-p
# Room narrative smoke (optional):
cargo xtask dogfood smoke --snapshot-detail room --expect "# window"
# Fail if action returns ok:false (default: warn and continue):
cargo xtask dogfood smoke --action agent::ToggleFocus --require-action
```

| Flag | Default | Role |
|------|---------|------|
| `--fixture` | workspace `README.md` | File path for `method:open` |
| `--wait-ms` | `3000` | Initial settle `method:wait` after open (0 skips) |
| `--poll-ms` | `250` | Interval between snapshot polls |
| `--timeout-secs` | `90` | Whole-session budget; each poll phase uses **remaining** time only (no extra floor) |
| `--expect` | (none) | Substring that must appear in **`snapshot@text` outline only** (not TOON `ok`/`id` metadata) |
| `--action` | (none) | Optional `method:action` after first good snapshot |
| `--keys` | (none) | Optional `method:keys` after action |
| `--require-action` | off | Fail the run if `--action` fails (else warn) |
| `--snapshot-detail` | `rich` | `compact` \| `rich` \| `room` (passed as TOON `detail`) |

Sequence: ready → open → optional wait → **poll snapshots** until non-empty (+ expects) → optional action/keys → if action/keys, poll again → shutdown.

Poll errors: step timeouts soft-retry while budget remains; **zed exit**, **stdout close**, and snapshot **`ok: false` hard-fail immediately**.

## I/O model

| Stream | Purpose |
|--------|---------|
| **stdin** | Blank-line-delimited TOON request documents |
| **stdout** | TOON responses and events **only** (never logs) |
| **stderr** | All Zed logs when agent-stdio is active |

## Startup

On ready, Zed writes a multi-line TOON document to stdout (`toon-format` 0.5; spaces after `:`):

```text
event: ready
user_data_dir: "/tmp/..."
pid: 12345
```

Matchers should use substrings (`event: ready`, `ok: true`, request `id`), not require a single physical line.

## Request / response format

Each request is a TOON document; terminate it with a blank line. Each response is a multi-line TOON object.

Successful responses include `ok: true` and echo the request `id` when provided. Errors include `ok: false` and `error`.

## Methods (v1)

| Method | One-line |
|--------|----------|
| `snapshot` / `look` | Capture a11y outline (`detail`: compact\|rich\|room; look is pure alias) |
| `inventory` | Best-effort session bag (windows / titles / focus) |
| `click` | AccessKit action on node id from look (default `click`; not default golden) |
| `theme` / `feel` | Global theme ambience only (name + a few tokens — not per-control paint) |
| `actions` | List registered GPUI action names (double-colon) |
| `open` | Open file path or URL (prefer a **file**) |
| `wait` | Sleep `ms` on the GPUI executor |
| `action` | Dispatch registered GPUI action by name |
| `keys` | Dispatch keystroke string (e.g. `ctrl-p`) |
| `shutdown` | Emit ok and quit |

### `snapshot` / `look`

Capture the UI accessibility tree as tactile text. `look` is a pure alias.

Optional field **`detail`**: `compact` | `rich` (default) | `room`.

Outline fields are **additive** across tiers (compact ⊆ rich interactive fields; room = rich interactive + landmarks + header). Focus `*` placement may differ when focus is a room-only landmark (room stars the landmark; rich/compact star the interactive ancestor). Rich/room lines may include bounds `@x,y w×h` in **scaled/physical px** (layout × scale factor; see detail tiers). Gates and `--expect` should prefer stable **role/label** substrings, not bounds digits.

Walk order: **active window first**, then remaining windows. Each window is **force-drawn** before reading its outline (required headless — no compositor frame loop). When **multiple** windows have non-empty outlines, they are **merged** with `--- window N ---` separators (`N` is **0-based** walk-order index). A single non-empty window is returned as-is (no separator).

- **`ok: true` + empty `""`:** no windows, or every window painted with no outline body for that detail (room header-only still serializes; gates treat `# …` headers alone as empty for smoke).
- **`ok: true` + non-empty:** real outline text (failed windows are **omitted**, not marked in text).
- **`ok: false`:** at least one `update_window` failed **and** no outline was produced. Smoke hard-fails on snapshot `ok: false`.

Gates treat non-empty as **interactive/landmark body** lines only (not window separators, not room `#` headers, not `[snapshot error]` diagnostics).

There is **no** server-side wait-until / poll-until method — the **xtask dogfood** runner polls `method:snapshot` until non-empty (see Smoke).

```text
method:snapshot
id:1
detail:rich
```

```text
ok: true
id: 1
"snapshot@text": "  *[Button] \"Open\" @12,40 88x28 [click,focus] #NodeId(42)\n  [TextInput] value=\"fn main\" @0,80 400x24 [focus] #NodeId(99)"
```

Focus marker `*` sits **after** depth indent (`  *[Button]`), so `*[Role]` substring matches still work via `trim`/`contains`. In **room** detail, `# focus:` names the exact AccessKit focus; body `*` is on that node when it is printed, otherwise on the nearest interactive ancestor (never both).

Room example body starts with:

```text
# window: "Zed"
# focus: [TextInput] value="…" #NodeId(…)
# interactive: N  landmarks: M
```

After paint/`finalize`, AccessKit focus falls back to the **root Window** when nothing else is focused, so live room headers usually show `# focus: [Window] "…" #NodeId(0)` rather than `# focus: (none)`. `(none)` appears mainly in unit tests / mid-frame paths that pass no focus.

Landmarks: Heading/Dialog/AlertDialog/Toolbar/MenuBar/Menu/TabList are listed even when unlabeled (spatial anchors); Label/List/ListItem only when labeled or valued. If unlabeled Toolbar/Menu chrome dominates a live room look, tighten those roles before adding more always-on landmarks.

### `inventory`

Best-effort retained-session summary (window count, active window, per-window title + focus from a room outline). Response field `inventory@text`.

```text
method:inventory
id:inv1
```

### `theme` / `feel`

**Room ambience, not per-control paint.** Samples the active global theme (`ActiveTheme`): name, light/dark appearance, and three named tokens (`background`, `border`, `text_accent`) as HSLA. `feel` is a pure alias. Response field `theme@text`.

Does **not** expose fill/border-width/radius per AccessKit node. Do not invent “1px solid white, sharp corners” from `look`. No CSS-like per-node dump.

```text
method:theme
id:t1
```

```text
ok: true
id: t1
"theme@text": "# theme: One Dark\n# appearance: dark\n# background: hsla(…)\n# border: hsla(…)\n# text_accent: hsla(…)\n"
```

### `click`

Dispatch an AccessKit action to a node id from a prior look (default action `click`). Optional `a11y_action`: `click` | `focus` | `set_value` | `expand` | `collapse`. Uses GPUI’s a11y action path (listeners, then click-at-center / focus fallbacks). Walks windows like look (**active first**), force-draws each candidate, and dispatches on the first window that owns the node.

**`ok: true` means the action actually applied** (listener ran, or click-at-center / focus fallback ran) — not merely “node id was present.” **`ok: false`** when: no window, node missing in every window, action not applicable on the node (no listener / no bounds for click / not focusable for focus), invalid `node` / `a11y_action`, or `set_value` without a payload.

| `a11y_action` | Payload | Notes |
|---------------|---------|-------|
| `click` (default) | none | Listener or mouse-down/up at node bounds center |
| `focus` | none | Requires focusable node this frame |
| `set_value` | **required** `value:` string, or string/number `data:` | Needs a registered SetValue listener on the node |
| `expand` / `collapse` | none | Only if the node registered that listener |

```text
method:click
id:c1
node:42
a11y_action:click
```

```text
method:click
id:c2
node:99
a11y_action:set_value
value:hello
```

Node may be decimal `42`, `NodeId(42)`, or the outline token `#NodeId(42)` (paste-from-look). Bare decimal is preferred in scripts. Prefer **look again** after `ok: true` to confirm UI state.

**Do not** put flaky click into default CI golden/smoke until a stable fixture exists; keep click for manual adventure / optional scripts. Poll-until is runner-side only (no server wait-until).

### `action`

Dispatch a registered GPUI action by name (**double-colon** `crate::Action`). Optional JSON `data` is passed to the action builder when the action has a payload.

```text
method:action
id:2
name:workspace::ToggleLeftDock
```

```text
method:action
id:3
name:some_crate::ActionWithPayload
data:{"visible":true}
```

On **build** failure (unknown name / bad `data`) or missing `name`, `ok: false` errors include the underlying GPUI message (with the attempted name) plus a hint to list names via `method:actions` (double-colon form). **Dispatch** failures (active-window update) include the action `name` and the update error, without the `method:actions` list hint. Prefer names from that session’s `method:actions` list.

### `keys`

```text
method:keys
id:4
keys:ctrl-p
```

(Linux dogfood; use `cmd-` on macOS.)

### `open`

Opens with **`ExistingWindow`** so dogfood reuses one window (no leftover empty shell).

| Target | Use |
|--------|-----|
| **Directory** | Real project worktree (merge-review, Surmount root) |
| **File** | Golden/smoke chrome checks; single-file worktree |

Agent-stdio seeds `session.trust_all_worktrees: true` into a fresh `--user-data-dir` settings.json (skipped if settings already exist) so Restricted Mode does not block dogfood. Startup does **not** open an empty workspace — first `method:open` creates the window.

```text
method:open
id:5
path:/home/user/project
```

```text
method:open
id:6
path:/home/user/project/src/main.rs
```

### `actions`

```text
method:actions
id:7
```

### `wait`

```text
method:wait
id:8
ms:500
```

### `shutdown`

```text
method:shutdown
id:9
```

## Candidate action names

| Purpose | `name` |
|---------|--------|
| Agent panel focus | `agent::ToggleFocus` |
| Agent panel toggle | `agent::Toggle` |
| Right dock | `workspace::ToggleRightDock` |
| File finder | `file_finder::Toggle` |
| New thread | `agent::NewThread` |
| Thread search toggle | `agent::ToggleSearch` |

Prefer names present in that session’s `method:actions` list.

### Thread search (`agent::ToggleSearch`) — manual / optional smoke

In-thread search is a **real** agent feature (`ThreadSearchBar` in `ThreadView`), registered as `agent::ToggleSearch`.

| Action | Linux / Windows | macOS | While search bar focused |
|--------|-----------------|-------|--------------------------|
| Open / toggle search | **Ctrl+F** (`AcpThread`) | **Cmd+F** | **Ctrl/Cmd+F** → `search::FocusSearch` (re-select query) |
| Next match | **F3** | **Cmd+G** | **Enter** |
| Previous match | **Shift+F3** | **Cmd+Shift+G** | **Shift+Enter** |
| Dismiss | — | — | **Esc** (`DismissThreadSearch`) |

Default headless smoke opens a file fixture only — the agent panel is **not** guaranteed open or focused, so wiring `--action agent::ToggleSearch` into default smoke is **flaky**. Prefer:

1. **Manual checklist:** open agent panel → open a thread with content → open search (Ctrl/Cmd+F) → type a query → next/prev with platform chords above (or Enter / Shift+Enter while the bar is focused) → **Esc** (highlights clear, focus returns to message editor).
2. **Optional smoke** only when the session already has agent UI focus, e.g. after a successful `agent::ToggleFocus` / `agent::NewThread` and a non-empty thread snapshot:

```bash
cargo xtask dogfood smoke \
  --fixture "$PWD/README.md" \
  --action agent::ToggleSearch
# or after opening agent UI yourself in a longer dogfood script:
# method:action name:agent::ToggleSearch
```

Do not fail CI on ToggleSearch alone unless the harness can assert agent-thread focus first.

## Known non-fatal stderr

| Signal | Meaning |
|--------|---------|
| `WARN [db] Opening fallback in-memory database` | Stateless / isolated user data |
| `ERROR … recent_workspaces_query … database table is locked` | Concurrent SQLite under agent-stdio (`thread_metadata_store` remote-connection migration → `WorkspaceDb::recent_project_workspaces_ungrouped`). Non-fatal; migration only marks complete after success (`detach_and_log_err`). Prefer leave production alone — do not soft-fail empty + mark-complete. See SURMOUNT.md § Agent stdio. |
| `ERROR [agent] Failed to authenticate provider: ChatGPT…` | Cloud provider not signed in |
| `ERROR … Is a directory (os error 21)` | Non-fatal when opening a directory worktree; merge-review intentionally opens the repo root |

## Observed (Linux headless)

| Step | Expectation (current tree) |
|------|----------------------------|
| `event: ready` | Yes |
| `method:actions` / open / wait / action / keys | `ok: true` |
| `method:snapshot` | Non-empty when frame has interactive roles (headless a11y + force-draw) |
| `method:shutdown` | exits |
| `dogfood merge-review` | Ready → Start (`Merge review`) → Preview (`Preview merge`) → End; non-empty room looks (Linux) |

**Verify:** `cargo build --release -p zed` → preflight → golden; inhabit regression: `merge-review` with room detail (see Agent verify).

## Platform matrix (dogfood snapshot)

Agent-stdio itself is **cross-platform** (no OS gate on the feature). Snapshot success depends on headless a11y activation (`active_flag`) so GPUI builds interactive outlines. Runner (`cargo xtask dogfood`) stays OS-agnostic via `--bin` / `ZED_BIN`.

| Platform | Dogfood snapshot support | Notes |
|----------|--------------------------|--------|
| **Linux headless** | **Yes** (after rebuild) | `HeadlessWindow::a11y_init` calls activation immediately — no AT-SPI adapter. Primary golden path. |
| **Linux X11 / Wayland** | AT-driven only | Real `accesskit_unix` adapters; activate when a screen reader connects. Not the agent-stdio path (`--agent-stdio` selects headless). |
| **macOS** | **Unsupported** for dogfood snapshot | No dedicated headless window. `MacWindow` uses real AccessKit `SubclassingAdapter` (waits for VoiceOver). Platform `headless` only changes run-loop startup; still opens NSWindows. Do not force-activate experimental GUI AccessKit. Inventing a full macOS headless window stack is out of scope. |
| **Windows** | **Unsupported** for dogfood snapshot | No dedicated headless window. `WindowsWindow` uses real AccessKit adapter (waits for UIA / Narrator). Headless platform omits DirectX devices / drop-target helper; `open_window` is not viable without a real headless window stack. Do not invent that stack here. |
| **Web / wasm** | **Unsupported** for dogfood snapshot | No `a11y_init` override; trait-default no-op. Not a dogfood target. |
| **GPUI TestWindow** | No force-activate (trait default) | Intentionally no-op: unconditional activate would enable a11y trees on **every** windowed GPUI test (perf + new push/pop surface). Outline logic is covered by pure unit tests in `gpui` `window/a11y.rs`. Production agent-stdio uses Linux `HeadlessWindow`, not `TestPlatform`. |
| **Default `PlatformWindow::a11y_init`** | No-op | Trait default; no activation. |

**Agent verify (Linux primary):** agent runs `cargo build --release -p zed` → preflight → golden; after agent/UI/merge-review changes also `merge-review` (Start→Preview→End path is proven — re-run for regression, not as open product work). On macOS/Windows, preflight/`event: ready` may still work if a binary exists; **do not expect non-empty golden/smoke snapshots** until a real headless window + force-activate path exists for that OS.

## Build / release hygiene

| Check | Command |
|-------|---------|
| Release binary | `cargo build --release -p zed` |
| Preflight (mandatory) | `cargo xtask dogfood preflight` |
| Golden | `cargo xtask dogfood golden` |
| Smoke | `cargo xtask dogfood smoke` |
| Optional clippy | `cargo clippy -p client -p agent_ui -p gpui -p zed -p xtask -- -D warnings` |

## Optional CI gate

Workflow: [`.github/workflows/dogfood_preflight.yml`](../../../.github/workflows/dogfood_preflight.yml) — **Surmount-maintained**, not generated by `cargo xtask workflows` (header does not start with `Generated from xtask…`, so regeneration will not overwrite it).

| Trigger | What runs |
|---------|-----------|
| **schedule** (nightly UTC `17 6 * * *`) | free disk → `cargo build --release -p zed` → `cargo xtask dogfood preflight --timeout-secs 90` → golden |
| **workflow_dispatch** | Same build + preflight; golden when input `run_golden` is true (default true) |
| **pull_request** | **Not** on the PR critical path (full release build is expensive) |

- **Linux only** (`ubuntu-latest`). Non-empty snapshot dogfood is Linux headless–primary; macOS/Windows are unsupported for golden snapshot (see platform matrix).
- Job fails if preflight (or golden when enabled) exits non-zero. No shell dogfood drivers — only `cargo xtask dogfood …`.
- `ZED_BIN` env points at `target/release/zed` after the release build (clap also accepts `--bin`; CI relies on env).
- Build env sets `CC=clang` / `CXX=clang++` (parity with upstream Linux CI `use_clang`); `script/linux` installs clang.
- CI passes `--timeout-secs 90` on preflight (default CLI is 30s; cold runners can miss ready) and golden.
- Free-disk step removes large unused GHA preinstalls before build; full release still needs adequate runner disk.
- **Fork ops:** enable Actions on the Surmount remote and allow scheduled workflows (GitHub often disables cron on forks until Actions is on / the workflow has run once on the default branch), or nightly will never fire.

**Local equivalent of the CI gate (agent runs):**

```bash
cargo build --release -p zed
ZED_BIN=target/release/zed cargo xtask dogfood preflight --timeout-secs 90
ZED_BIN=target/release/zed cargo xtask dogfood golden --timeout-secs 90   # optional; Linux
# equivalent: cargo xtask dogfood preflight --bin target/release/zed --timeout-secs 90
```

## Residual risks (honest)

| Risk | Status / mitigation |
|------|---------------------|
| Bounds scale | `@x,y w×h` are **scaled/physical px** (layout × `scale_factor`); not CSS logical. Documented; gates use role/label expects only. |
| Click flaky without fixture | `method:click` is **manual/optional** — not in default golden/smoke until a stable fixture exists. |
| Theme = global only | `theme`/`feel` samples `ActiveTheme` (name + a few tokens). Never per-control paint / CSS dump. |
| Outline token bloat | Detail tiers; per-field `OUTLINE_STRING_MAX` (80); landmarks only in `room`. No whole-tree line cap — parent agents must not re-ingest full outlines/stderr (see Evidence / token discipline). |
| No server wait-until | Poll stays runner-side (`cargo xtask dogfood smoke`). |
| macOS/Windows non-empty snapshot | Unsupported until a real headless + force-activate path exists (see platform matrix). |

## Notes

- Runner: `tooling/xtask/src/tasks/dogfood.rs` (`cargo xtask dogfood`). OS-agnostic; point at any platform binary with `--bin` / `ZED_BIN`.
- Server: `crates/zed/src/zed/agent_stdio.rs`.
- Agent-stdio sets `ZED_STATELESS=1`, `ZED_EXPERIMENTAL_A11Y=1`, isolates the single-instance socket, and requests `current_platform(headless=true)`. Only Linux has a true headless window that force-activates a11y for snapshots; see platform matrix above.
- Encode/decode on the Zed side uses `toon-format` 0.5; the xtask client speaks the same wire shape with a small hand encoder/scraper.
- **Re-read Experience doctrine** before exploratory dogfood or when teaching another agent the loop — gates alone are not the point.
- **R4 style decision (final):** **(b) `method:theme` / `feel`.** Per-control paint (“1px white, sharp corners”) stays **out of a11y forever** — AccessKit has no fill/radius; do not invent it. Spikes: (1) inspector element-id→style is feature-gated / wrong layer for dogfood; (2) `cx.theme()` / `ActiveTheme` is already global and cheap in `agent_stdio`. Shipped: global theme **name + appearance + background/border/text_accent** only — not a room-outline footer (keeps look structure pure), not a CSS dump. Prefer `look` + `inventory` for tactile structure; call `theme` when you want room atmosphere.
- Maintainer pointer: `SURMOUNT.md` § Agent stdio.

## Agent verify (current)

The agent executes these (dogfood exception in `.rules`). Cite short evidence; do not hand the human the inhabit commands. If any step fails, own the diagnose → fix → retry loop per **Failure autonomy** — do not stop at “please run …”.

```bash
cargo build --release -p zed
cargo xtask dogfood preflight
cargo xtask dogfood golden
cargo xtask dogfood smoke --fixture "$PWD/README.md"
ZED_BIN=target/release/zed cargo xtask dogfood merge-review \
  --fixture "$PWD" --snapshot-detail room --timeout-secs 180
# Unit tests after a11y / agent_stdio / dogfood runner edits (agent may run scoped dogfood tests):
cargo test -p xtask -- tasks::dogfood::tests
# Clippy / broader cargo still follow normal .rules (human unless another exception):
# cargo test -p gpui outline
# cargo test -p gpui --lib -- window::a11y::tests
# cargo test -p zed agent_stdio
# cargo clippy -p gpui -p xtask -- -D warnings
```

# Zed Dogfood — Agent Stdio Subagent Protocol

Spawn a headless/offscreen Zed instance controlled over stdio using the TOON protocol (MCP-style).

## Spawn

```bash
# Build once (agent-stdio is a default feature on the Surmount fork)
cargo build --release -p zed

# Run with isolated temp user data (recommended)
target/release/zed --agent-stdio

# Or via environment variable
ZED_AGENT_STDIO=1 target/release/zed

# Custom user data directory
target/release/zed --agent-stdio --user-data-dir /tmp/my-zed-agent
```

## Preflight (mandatory before golden / long dogfood)

**Required** before any long dogfood session or release dogfood: run the preflight gate. It must exit 0 with first stdout event `event: ready`. Do not start golden-session or a long interactive dogfood if preflight fails.

```bash
# From repo root; needs existing release binary + python3
.agents/skills/zed-dogfood/scripts/preflight.sh

# Overrides (ALLOW_UDD_RM=1 only if UDD is outside /tmp/*)
ZED_BIN=target/release/zed \
  UDD=/tmp/zed-preflight \
  OUT=/tmp/zed-preflight.out \
  ERR=/tmp/zed-preflight.log \
  TIMEOUT_SECS=15 \
  .agents/skills/zed-dogfood/scripts/preflight.sh
```

**Call order:** `cargo build --release -p zed` → **preflight** → golden-session (or long dogfood).

The script (Python helper via heredoc, same as golden-session): wipes UDD with `rm -rf` after resolving a canonical path and **requiring a subdirectory under `/tmp/`** (e.g. `/tmp/zed-preflight`; rejects exact `/tmp`, `/tmp/`, and `/tmp/../…` traversal). For a path outside `/tmp/…`, set `ALLOW_UDD_RM=1` explicitly (still never allows wiping `/tmp` itself). Then spawns `zed --agent-stdio`, asserts the **first** non-empty stdout line matches `^event:\s*ready\b`, and shuts down. Fails non-zero on process death **without** ready in the pipe, timeout, or wrong first event. If the process exits after emitting ready, preflight still **passes** (optional WARN). Panic/registration lines in stderr are **failure diagnostics only** (printed when the gate already failed); they do not independently flip pass/fail. Healthy ready is typically a few seconds; default timeout is 15s.

Do not use `timeout … | head` as a substitute: `head` closing the pipe can SIGPIPE or orphan a headless Zed. Prefer `preflight.sh` only.

If preflight fails with no stdout, read `ERR` (default `/tmp/zed-preflight.log`) for panics (e.g. duplicate GPUI action registration / missing keymap action registration).

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

Scripts should match on substrings (`ready`, `ok: true`, request `id`), not require a single physical line.

## Request / response format

Each request is a TOON document; terminate it with a blank line (multi-line documents are fine). Each response is a multi-line TOON object (fields on separate lines; values may be quoted).

Successful responses include `ok: true` and echo the request `id` when provided.

Errors include `ok: false` and an `error` field.

## Methods (v1)

### `snapshot`

Capture the UI accessibility tree as compact text (interactive nodes only).

**Request:**
```text
method:snapshot
id:1
```

**Response** (multi-line TOON; non-empty outline example):
```text
ok: true
id: 1
"snapshot@text": "[Button] \"Open\" #NodeId(42)\n  [TextInput] value=\"fn main\" #NodeId(99)"
```

After Phase 1B, expect non-empty interactive outline once a window has painted. Residual empty `""` means no interactive nodes in the last frame (or binary pre-1B).

### `action`

Dispatch a registered GPUI action by name (same names as `zed --dump-all-actions` / `method:actions` — **double-colon** `crate::Action`).

**Request:**
```text
method:action
id:2
name:workspace::ToggleLeftDock
```

With JSON payload:
```text
method:action
id:3
name:agent::Chat
data:{"key":"value"}
```

### `keys`

Simulate a keystroke on the active window (`Keystroke::parse` syntax).

**Request** (Linux dogfood; use `cmd-` chords on macOS):
```text
method:keys
id:4
keys:ctrl-p
```

### `open`

Open a file path or URL.

**Request:**
```text
method:open
id:5
path:/home/user/project/src/main.rs
```

```text
method:open
id:6
url:file:///home/user/project
```

### `actions`

List all registered action names (sorted, unique).

**Request:**
```text
method:actions
id:7
```

**Response** (multi-line TOON; names are double-colon):
```text
ok: true
id: 7
actions[3]: "workspace::ToggleLeftDock","file_finder::Toggle","agent::ToggleFocus"
```

### `wait`

Pause for the given milliseconds (async; response arrives after the delay).

**Request:**
```text
method:wait
id:8
ms:500
```

### `shutdown`

Gracefully quit Zed.

**Request:**
```text
method:shutdown
id:9
```

## Example session

```bash
target/release/zed --agent-stdio 2>zed.log &
PID=$!

# Read ready event from stdout
read -r READY

# Open a file fixture (prefer a file; directory open may log Is a directory)
echo 'method:open
id:1
path:/home/user/my-project/README.md' >&0

# Wait for window / layout settle
echo 'method:wait
id:2
ms:3000' >&0

# Snapshot UI
echo 'method:snapshot
id:3' >&0

# Quit
echo 'method:shutdown
id:4' >&0
```

## Golden session (Phase 1A)

Operator contract for a single headless dogfood pass. Handlers: `crates/zed/src/zed/agent_stdio.rs`. Reuse only existing methods (no wait-until / click-by-node / action_info).

### Sequence

1. **Spawn** isolated: `target/release/zed --agent-stdio --user-data-dir /tmp/zed-golden-session`
2. **stdout** → expect `event: ready` (plus `user_data_dir`, `pid` on following lines)
3. **`method:actions`** — confirm agent/workspace names exist (see candidates below)
4. **`method:open`** — tiny **file** fixture (prefer a file, not a directory)
5. **`method:wait`** — **ms:3000** (2–5s is fine; 3000 is the default in the script)
6. **`method:snapshot`** — inspect `snapshot@text` (interactive a11y outline only)
7. **`method:action`** — e.g. `name:agent::ToggleFocus` if listed
8. **`method:keys`** — simple chord when a window exists, e.g. `keys:ctrl-p`
9. Optional second **`method:wait`** + **`method:snapshot`** after action/keys
10. **`method:shutdown`**

### Run script

**Prerequisite:** preflight must pass first (see Preflight above).

```bash
# From repo root; needs existing release binary + python3
.agents/skills/zed-dogfood/scripts/preflight.sh && \
  .agents/skills/zed-dogfood/scripts/golden-session.sh

# Overrides
ZED_BIN=target/release/zed \
  FIXTURE=$PWD/README.md \
  WAIT_MS=3000 \
  UDD=/tmp/zed-golden-session \
  OUT=/tmp/zed-golden-session.out \
  ERR=/tmp/zed-golden-session.log \
  .agents/skills/zed-dogfood/scripts/golden-session.sh
```

`UDD` is wiped with `rm -rf` before each run. The script resolves a canonical path and **requires a subdirectory under `/tmp/`** (e.g. `/tmp/zed-golden-session`; rejects exact `/tmp`, `/tmp/`, and `/tmp/../…` traversal). For a path outside `/tmp/…`, set `ALLOW_UDD_RM=1` explicitly (still never allows wiping `/tmp` itself).

Script prints step status to the terminal; full TOON stdout → `OUT`, Zed logs → `ERR`.

### Request shapes (match handlers)

```text
method:actions
id:actions1

method:open
id:open1
path:/path/to/README.md

method:wait
id:wait1
ms:3000

method:snapshot
id:snap1

method:action
id:act1
name:agent::ToggleFocus

method:keys
id:keys1
keys:ctrl-p

method:shutdown
id:shut1
```

Blank line terminates each TOON request document.

### Candidate action names (Linux keymaps / registry)

From `method:actions` and `assets/keymaps/default-linux.json`:

| Purpose | `name` for `method:action` | Notes |
|---------|----------------------------|--------|
| Agent panel toggle/focus | `agent::ToggleFocus` | Linux: `ctrl-?` |
| Agent panel toggle (alt) | `agent::Toggle` | Also registered |
| Right dock | `workspace::ToggleRightDock` | Agent dock side |
| File finder | `file_finder::Toggle` | Good `method:keys` follow-up: `ctrl-p` |
| New agent thread | `agent::NewThread` | Optional |

Always prefer names present in that session’s `method:actions` list (~1.3k–1.4k names).

### What to inspect on `method:snapshot`

- Response field is **`snapshot@text`** (string). Empty looks like: `"snapshot@text": ""`.
- Non-empty outline is multi-line interactive nodes only (`window.a11y_interactive_outline()`), e.g. `[Button] "…" #NodeId(…)`.
- Empty outline ≠ failed method: `ok: true` with `""` only when the last painted frame has no interactive roles (after Phase 1B rebuild; pre-1B binaries always empty).

### Known non-fatal stderr (ignore for protocol success)

| Signal | Meaning |
|--------|---------|
| `WARN [db] Opening fallback in-memory database` | Stateless / isolated user data |
| `ERROR … recent_workspaces_query … database table is locked` | Race on SQLite workspaces; non-fatal |
| `ERROR [agent] Failed to authenticate provider: ChatGPT Subscription…` | Cloud provider not signed in |
| `ERROR … Is a directory (os error 21)` | Prefer **file** path for `method:open`; directory open may still return `ok: true` but log this |
| ACP `skills-reload` / `received message with neither id nor method` | Bridged agent noise |

Older isolated runs have also panicked on `ThemeNotFoundError("One Light")` during onboarding UI; agent-stdio is supposed to skip welcome — if that panic returns, treat as environment regression, not golden-step failure of wait/open.

### Observed (Linux headless, release binary)

**Phase 1A (pre-fix, measured):** protocol healthy; `snapshot@text` **100% empty** after open+wait+action/keys despite `ZED_EXPERIMENTAL_A11Y=1` and `Rendered first frame`.

**Phase 1B (source fix; requires rebuild):** (1) Linux headless `a11y_init` activates immediately (no AT); GUI/experimental still needs real AccessKit activation. (2) Post-`finalize` interactive outline string retained (not full-tree clone). (3) `capture_snapshot` reuses outline, force-draws only if empty.

| Step | Phase 1A | Phase 1B (after `cargo build --release -p zed`) |
|------|----------|--------------------------------------------------|
| `event: ready` | Yes | Yes |
| `method:actions` / open / wait / action / keys | `ok: true` | `ok: true` |
| `method:snapshot` | **always empty** | non-empty when frame has interactive roles |
| `method:shutdown` | exits | exits |

**Verify:** rebuild → preflight → golden. `rg 'snapshot@text' /tmp/zed-golden-session.out` — golden open+wait typically shows interactive chrome; empty only if that frame has no interactive roles (not Phase 1A systemic empty).

## Build / release hygiene (Phase 2)

| Check | Command / note |
|-------|----------------|
| Release binary | `cargo build --release -p zed` |
| Preflight (mandatory) | `.agents/skills/zed-dogfood/scripts/preflight.sh` |
| Golden session | `.agents/skills/zed-dogfood/scripts/golden-session.sh` |
| Optional clippy (scoped) | `cargo clippy -p client -- -D warnings`; same for `-p agent_ui`, `-p gpui`, `-p zed` |

**Keymap / actions:** Agent search actions (`ToggleSearch`, `DismissThreadSearch`, `SelectNextThreadMatch`, `SelectPreviousThreadMatch`) are registered in `crates/agent_ui/src/agent_ui.rs` so keymap load does not panic. **Registration ≠ feature** — `conversation_view/thread_search_bar.rs` remains an orphan source file (not `mod`'d) until thread-search wiring; do not re-enable `mod thread_search_bar` for hygiene alone.

**Client cloud-strip:** On merges, keep Surmount cfg gates/no-ops for sign-in/telemetry (`SURMOUNT.md` § Upstream services stripped). Do not restore upstream cloud paths for warning cleanup.

## Notes

- Uses `ZED_STATELESS=1` and skips the single-instance socket to avoid colliding with a running Zed.
- On Linux, boots with the headless GPUI platform (wgpu layout path) and enables `ZED_EXPERIMENTAL_A11Y=1` for snapshots (`agent_stdio::prepare_environment`).
- Encode/decode uses the `toon-format` 0.5 crate.
- Maintainer one-liner: `SURMOUNT.md` § Agent stdio.
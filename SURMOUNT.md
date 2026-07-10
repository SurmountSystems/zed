# Surmount — upstream differences

Living record of technical differences between Zed upstream `main` and the `surmount` branch. Maintained chunk-by-chunk via the branch-differences-documenter skill (`.agents/skills/branch-differences-documenter/`).

Only describe differences explicitly visible in supplied diffs. Open questions use `TODO:` markers.

## Merge review workflow

**Direction (review in progress):** [docs/surmount/merge-review.md](docs/surmount/merge-review.md) — per-diff summaries and a running explanation in Branch Diff; human only for leftover uncertainty; not the prototype file-list tab.

Triage via `merge_review_triage` ACP tool (matches in-app populate; `script/surmount-merge-triage` is legacy for humans/CI); agent skill `.agents/skills/surmount-merge-review/SKILL.md`; categories in `surmount-merge-categories.toml`. Per-file summaries land in the merge review session from Review Diff replies; update this file per section after human confirms drafted prose.

Keep `surmount-merge-categories.toml` in sync when categories here change.

## Surmount maintainer docs

Living table of contents for fork-specific design docs (not published to zed.dev). Add new rows here when adding files under `docs/surmount/`.

| Doc | Summary |
|-----|---------|
| [docs/surmount/README.md](docs/surmount/README.md) | Index for this folder |
| [docs/surmount/merge-review.md](docs/surmount/merge-review.md) | Merging upstream main: summarize each diff, cumulative review memory, SURMOUNT.md from that |
| This file § [Menhera dependency pins](#menhera-dependency-pins-fork-maintenance) | Menhera-cooldown pin/verify/unpin (fork maintenance) |
| This file § [Upstream services stripped](#upstream-services-stripped-merge-policy) | Zed Cloud / telemetry merge resolution table |
| [`.agents/skills/surmount-merge-review/SKILL.md`](.agents/skills/surmount-merge-review/SKILL.md) | Operator skill for real upstream sync (`/surmount-merge-review`) |

### Merge review visibility (Linux grok-first cold start)

**Symptom:** Palette **Start Merge Review** appears to do nothing, or Branch Diff is hidden behind the maximized agent dock.

**Expected behavior:** `open_merge_review_workflow` opens Branch Diff against `origin/main`, zooms out the agent dock (`PanelEvent::ZoomOut`), focuses the center pane, and posts the plan in the agent thread. The prototype **Surmount Merge Review** file-list tab does not open.

**Expected log sequence:**

1. `surmount merge review: start requested`
2. `surmount merge review: populated N items`
3. `surmount merge review: opened Branch Diff against origin/main`
4. `surmount merge review: posted plan to agent thread`

**Regression tests:** See requirements matrix in [docs/surmount/merge-review.md](docs/surmount/merge-review.md#requirements--tests-redgreen). Quick slice:

```bash
cargo test -p agent_ui merge_review::tests
CARGO_TERM_QUIET=true cargo nextest run -p agent_ui -p git_ui -p project -p git \
  --all-features --no-fail-fast --hide-progress-bar --status-level fail \
  -E 'test(merge_review) | test(branch_diff)'
```

## Agent stdio (headless dogfood)

`agent-stdio` is a **default** feature on the `zed` crate (`crates/zed/Cargo.toml`). A plain `cargo build --release -p zed` includes `--agent-stdio` / `ZED_AGENT_STDIO=1` and the TOON stdio control plane. On upstream merges, keep `default = ["agent-stdio"]` unless deliberately dropping dogfood support.

Operator contract: [`.agents/skills/zed-dogfood/SKILL.md`](.agents/skills/zed-dogfood/SKILL.md) (Experience doctrine + **detail tiers** compact/rich/room; methods table: `snapshot`/`look`, `inventory`, `click`, `theme`/`feel`, `actions`, `open`, `wait`, `action`, `keys`, `shutdown`; no fake per-control style). **Rust gates only** — `cargo xtask dogfood preflight` (ready event) then `cargo xtask dogfood golden` / `smoke` (`tooling/xtask/src/tasks/dogfood.rs`; optional `--snapshot-detail`). Smoke: open fixture, poll snapshot until non-empty body (interactive or landmark lines; not room `#` headers alone; + optional `--expect`), optional action/keys. Call order: `cargo build --release -p zed` → preflight → golden/smoke. Handlers: `crates/zed/src/zed/agent_stdio.rs`. Outline formatting: `gpui` `format_a11y_outline` / `OutlineDetail`.

**Residual risks:** bounds = scaled/physical px (not CSS logical); `click` manual/optional (not default golden); `theme`/`feel` = global `ActiveTheme` only (not per-control paint); no server wait-until (runner polls); macOS/Windows non-empty snapshot unsupported until headless force-activate exists.

**Optional CI gate:** [`.github/workflows/dogfood_preflight.yml`](.github/workflows/dogfood_preflight.yml) (Surmount-maintained hand-written workflow — **not** produced by `cargo xtask workflows`). Runs on **schedule (nightly UTC)** and **workflow_dispatch** only (not PR critical path: full `cargo build --release -p zed` is ~15+ min). Linux `ubuntu-latest` with `CC`/`CXX=clang`, disk free-up before build, and `--timeout-secs 90` on dogfood steps: build release zed → `cargo xtask dogfood preflight` via `ZED_BIN` → optional `cargo xtask dogfood golden` (nightly always; dispatch when `run_golden` input is true). Snapshot dogfood is **Linux-primary**; do not require macOS/Windows for non-empty snapshot. **Fork ops:** GitHub disables scheduled workflows on forks until Actions is enabled on the Surmount remote (and often until the workflow has run once on the default branch); enable Actions / allow schedules or nightly will never fire. Local equivalent of the CI gate:

```bash
cargo build --release -p zed
ZED_BIN=target/release/zed cargo xtask dogfood preflight --timeout-secs 90
ZED_BIN=target/release/zed cargo xtask dogfood golden --timeout-secs 90   # optional
# equivalent: cargo xtask dogfood preflight --bin target/release/zed --timeout-secs 90
```

**Linux headless (AccessKit activate + outline retain):** `event: ready` then methods respond `ok: true` (except snapshot when every `update_window` fails with no outline → `ok: false`). Prefer `method:open` on a **file**. Settle: `method:wait` **ms:3000** after open. **`method:snapshot` / `look`:** optional `detail` compact|rich|room (default rich). Linux headless `a11y_init` activates immediately; finalize retains **three** outline strings (compact/rich/room) in one tree walk; force-draw before read; multi-window merge with `--- window N ---`. Rich lines include focus `*`, bounds (`@x,y w×h` = **scaled/physical px**: layout × `scale_factor`, rounded — not CSS logical px), states, action verbs; room adds header + landmarks. **`compact`** is a lean role/label/value/id path (viable for minimal tokens; not required bit-identical to pre-R1). Default smoke/golden stay non-empty + optional **role/label** expects — not bounds digits or theme HSLA. **`inventory` / `click`:** session summary and a11y node actions (NodeId from look); click is manual/optional, not default golden. **`method:theme` / `feel` (R4 decision b):** global theme ambience only (name, appearance, background/border/text_accent via `ActiveTheme`) — not per-control paint, not a CSS dump; look/room stay structure-only. **`method:action`:** build errors include action name + `method:actions` hint. Poll-until-snapshot in **xtask dogfood**. **Non-goals:** full AccessKit JSON dump, force-a11y on all GPUI tests, macOS/Windows non-empty golden requirement, shell dogfood drivers. **Cross-platform snapshot matrix:** Linux headless yes; macOS/Windows unsupported for non-empty golden — see skill. No OS–agent coupling on methods.

**Known non-fatal dogfood stderr (do not treat as session failure):** in-memory DB WARN; `thread_metadata_store` remote-connection migration (`WorkspaceDb::recent_project_workspaces_ungrouped` → `recent_workspaces_query`) can ERROR with `database table is locked` under concurrent SQLite access in agent-stdio (dogfood skill known-noise table); migration uses `detach_and_log_err` and only writes the migration key after success, so leave production code alone rather than soft-failing empty + mark-complete; provider auth noise.

**Thread search:** Wired on `ThreadView` (`ThreadSearchBar`); action `agent::ToggleSearch` (Ctrl/Cmd+F in `AcpThread` keymaps). Match visibility tracks expanded thinking/tool/raw-input/confirmation. Dogfood: manual checklist or optional `--action agent::ToggleSearch` — see zed-dogfood skill (not in default smoke).

## Upstream services stripped (merge policy)

Surmount is a local-first fork. It does **not** phone home to Zed Cloud for sign-in, metrics, crash upload, or usage telemetry. When merging upstream `main`, **keep Surmount stubs** — do not re-enable upstream paths without an explicit maintainer decision.

| Area | Upstream | Surmount | Merge resolution |
|------|----------|----------|------------------|
| Zed Cloud sign-in | Browser OAuth + RSA-encrypted access token (`rpc::auth` keypair, `Client::authenticate_with_browser`) | Disabled; returns error directing users to local agents/API keys | Keep Surmount stub; drop `rsa` reintroduction |
| Telemetry settings | User-toggleable `telemetry.metrics` / `telemetry.diagnostics` | Forced off in release (`TelemetrySettings::from_settings`, `metrics_enabled` / `diagnostics_enabled`) | Keep forced-off release path; defaults in `assets/settings/default.json` stay `false` |
| Event pipeline | `telemetry::send_event` → queue → HTTP flush + `telemetry.log` | `send_event` no-op; `report_event` / `flush_events_inner` no-op in release | Keep no-ops in non-test builds |
| `rpc::auth` | RSA encrypt/decrypt + `random_token` | `random_token` only (collab dev token helper) | Keep slim `auth.rs`; no `rsa` workspace dep |
| Sign-in UI | Title bar, AI onboarding, welcome flow, collab panel, Zed Cloud model settings | Hidden via `client::zed_cloud_ui_enabled()` (false in release); `title_bar.show_sign_in` forced off | Keep Surmount hides; do not restore Sign In buttons or auto `authenticate()` on startup |

**Touch files on conflict:** `crates/rpc/src/auth.rs`, `crates/rpc/Cargo.toml`, `Cargo.toml` (workspace deps), `crates/client/src/client.rs` (`zed_cloud_ui_enabled`, `authenticate_with_browser`, `sign_in`, `TelemetrySettings`), `crates/client/src/telemetry.rs`, `crates/telemetry/src/telemetry.rs`, `crates/title_bar/`, `crates/ai_onboarding/`, `crates/onboarding/`, `crates/collab_ui/src/collab_panel.rs`, `crates/language_models/src/provider/cloud.rs`, `crates/zed/src/main.rs` (`authenticate`), `assets/settings/default.json`.

### Grok Build authentication (goal, not implemented)

Stripping Zed Cloud sign-in is **not** a rejection of agent authentication. Surmount's target is **Grok Build–parity auth inside Zed**: whatever Grok Build uses to reach xAI (API keys, session tokens, CLI login state under `~/.grok`, provider env vars, etc.) should work natively in the agent panel without browser OAuth to zed.dev.

**Today:** users configure local agents and provider API keys (see `docs/src/ai/use-api-access.md`); `client::zed_cloud_ui_enabled()` stays false in release.

**Goal (document only for now):** first-class Grok Build auth UX and credential plumbing in Zed — discover/login state from the Grok CLI layout, surface status in agent settings, and keep it separate from the disabled Zed Cloud `SignIn` path. Do not restore RSA/`authenticate_with_browser` when implementing this; add a Surmount-specific path (likely `agent_servers`, `agent_settings`, native Grok profile). Do not implement OAuth or re-enable Zed Cloud `SignIn` under this goal.

## Menhera dependency pins (fork maintenance)

Maintainer `~/.cargo/config.toml` redirects crates-io to **menhera-cooldown** (`sparse+https://index.crates.menhera.org/10d/`, ~10 calendar days behind crates.io). Upstream bumps can land versions menhera has not indexed yet; cargo then fails even when search shows a newer crate (search returns latest indexed, not the exact pin).

**Authoritative pin notes:** [`.cargo/config.toml`](.cargo/config.toml) (comments) and workspace pins in root [`Cargo.toml`](Cargo.toml). AGENTS.md § Menhera-cooldown dependency pins is binding for agents.

### Current pins (as of 2026-07-08)

| Crate | Upstream target | Surmount pin | crates.io publish | Menhera earliest (~+10d) |
|-------|-----------------|--------------|-------------------|--------------------------|
| `wgpu` | `29.0.4` | `29.0.3` | 2026-07-02 | **2026-07-12** (binding) |
| `agent-client-protocol` | `1.0.1` | `=1.0.0` | 2026-06-29 | 2026-07-09 |

**Binding unpin date** = latest of the per-crate +10d dates → **2026-07-12** (wgpu).

- **Before 2026-07-12:** do **not** unpin; leave workspace pins as listed above.
- **On/after 2026-07-12:** calendar alone is **not** enough — still require human `cargo info` evidence that each upstream target resolves on menhera-cooldown before any version change.
- Agents must **not** bump or unpin these versions. Never change pin versions without pasted human verify output.

### Verify → unpin → rebuild → dogfood (human-only checklist)

1. **Calendar:** on/after **2026-07-12**.
2. **Verify** (exact-version checks; not `cargo search`):
   ```bash
   cargo info wgpu@29.0.4 --registry menhera-cooldown
   cargo info agent-client-protocol@1.0.1 --registry menhera-cooldown
   ```
3. **Unpin only if both resolve:** bump versions in root `Cargo.toml` only — keep table form and features. Target lines:
   ```toml
   agent-client-protocol = { version = "1.0.1", features = ["unstable"] }
   wgpu = "29.0.4"
   ```
   Do **not** drop `features = ["unstable"]` (required by ACP-using crates). Then remove or rewrite the pin comment block in [`.cargo/config.toml`](.cargo/config.toml) and the pin pointer comments on those lines in root `Cargo.toml`.
4. **Rebuild:** normal workspace / release build that exercises the bumped crates.
5. **Dogfood preflight** (agent-stdio still healthy after dep churn):
   ```bash
   cargo build --release -p zed
   cargo xtask dogfood preflight
   ```
6. **Optional strip sanity** after dep churn:
   ```bash
   cargo clippy -p client -p rpc -p telemetry -- -D warnings
   ```

**Feature/default-feature conflicts are not menhera lag** — e.g. `toon-format` `default-features = false` in `crates/zed/Cargo.toml` avoids `ratatui` → `unicode-width` clash. Do not invent unpin dates for those; fix features/edges directly.

## Upstream merge process (pointer)

Actual upstream sync is **not** ad-hoc merge docs. Use:

- Skill: [`.agents/skills/surmount-merge-review/SKILL.md`](.agents/skills/surmount-merge-review/SKILL.md) (`/surmount-merge-review`)
- Design/workflow: [docs/surmount/merge-review.md](docs/surmount/merge-review.md)
- On `client` / `rpc` / `telemetry` / `assets/settings/default.json` conflicts: [Upstream services stripped](#upstream-services-stripped-merge-policy) above
- On registry lag after merge: [Menhera dependency pins](#menhera-dependency-pins-fork-maintenance)

Do not invent a parallel merge process. This fork-maintenance section documents policy only; it does not run `git merge` or unpin crates.

## Documentation map

| Layer | Location | Audience |
|-------|----------|----------|
| Technical diff record | This file (`SURMOUNT.md`) | Maintainers, agents, merge work |
| Surmount feature design | `docs/surmount/` ([TOC](#surmount-maintainer-docs)) | Maintainers before implementation |
| User-facing AI guides | `docs/src/ai/` | End users (zed.dev) |
| Doc writing style | `docs/.rules` | Anyone editing user docs |
| Agent-binding rules | `.rules` / `AGENTS.md` (symlink) | Agents and contributors |
| Strategic intent | `PLAN.md` | Planning and priorities |
| Crate-specific rules | `crates/<crate>/.rules` when they exist | Agents working in that crate |
| Grok user guide | `docs/src/ai/external-agents.md#grok-build-xai` | End users |
| Grok maintainer API | This file, [Maintainer reference](#maintainer-reference) below | Contributors extending the categorized todos surface |

## Maintainer reference

Technical notes for extending Grok's categorized todos surface. User-facing setup and keybindings live in `docs/src/ai/external-agents.md#grok-build-xai`.

### Architecture

The categorized todos surface shows four categories for Grok threads (bridged ACP and native profile):

1. **Agent Approvals** — pending tool calls with read-only vs destructive risk chips and Allow/Reject actions
2. **Plan Todos** — `todo_write` / `enter_plan_mode` entries with status icons and proposed-plan accept
3. **Background Monitors** — `monitor` tool calls with lazy terminal output on expand
4. **Grok Memory** — read-only panel showing what Grok Build already remembers for this project: snippets from `MEMORY.md` (workspace and `~/.grok/memory/`), plus learned facts from Grok's `worktrees.db`. Each snippet has a copy button. Only appears on Grok threads (bridged or native profile); other agents have no Grok memory files to display. **Replacement plan:** [Native memory replacement](#native-memory-replacement--scoped-plan) via `memory_palace`.

Both paths share one implementation: `AcpThread` state, `acp_thread` risk rules, and `agent_ui` render helpers. Native Grok (`is_grok_build_profile`) and bridged ACP threads use the same collectors and `ZedTodosComponent`.

Default UI locations:

- **Activity bar** — compact collapsed-by-default summary in `ThreadView`
- **Full Agent Mode** — overlay via `ZedTodosDockPrototype` (`agent::OpenFullGrokSurface` / `OpenZedTodosSurface`)

### Key files

| Area | Location |
|------|----------|
| Thread UI + dock prototype | `crates/agent_ui/src/conversation_view/thread_view.rs` |
| Public re-exports | `crates/agent_ui/src/agent_ui.rs` |
| Risk classification + collectors | `crates/acp_thread/src/acp_thread.rs` |
| Native Grok tools + system fragments | `crates/agent/src/thread.rs`, `crates/agent/src/tools.rs` |
| ACP session capture | `crates/acp_tools/src/acp_tools.rs` → `captures/acp-capture/` |
| Serialized panel/todos persistence | `crates/agent_ui/src/thread_metadata_store.rs` (`agent_panels` LMDB db) |
| Actions | `crates/zed_actions/src/lib.rs` (`NewGrokThread`, `OpenFullGrokSurface`, `OpenZedTodosSurface`) |

### Public API (`agent_ui` re-exports)

```rust
use agent_ui::{
    ZedTodos, ZedTodosComponent, ZedTodosDockPrototype,
    render_risk_chip, render_approval_row, render_background_task_row,
    render_grok_memory_items, render_zed_todos_categorized_surface,
    // Collectors and action builders live on ZedTodosComponent:
    // collect_pending_approval_tool_calls, collect_background_monitor_tool_calls,
    // pending_approval_options_for_tool_call, pending_approval_counts,
    // build_allow_once_action, build_allow_always_action, build_granular_allow_action,
    // build_deny_action, build_plan_accept_button,
    // format_categorized_approval_action_label, approval_action_check_icon_color,
};
```

`ZedTodosComponent` owns expansion state (`ZedTodos`: four section bools + per-monitor `HashSet`). All toggles go through the component; gate expensive children (Markdown, `TerminalView`) on expanded flags.

### Integrating a custom dock or panel

1. Own a `ZedTodosComponent` in your GPUI view.
2. Hold a `WeakEntity<AcpThread>` (or use `ZedTodosDockPrototype::new_for_thread` as a reference).
3. In `render`, call collectors on the thread:
   - `ZedTodosComponent::collect_pending_approval_tool_calls`
   - `ZedTodosComponent::collect_background_monitor_tool_calls`
   - `thread.plan()`, `thread.grok_memory()` for Grok threads
4. Build rows with `render_approval_row` + `build_*_action` helpers (supply `cx.listener` or weak-thread closures for authorize/clear_plan).
5. Use `ZedTodosComponent::render_plan_entry_row` and `render_background_task_row` for plan and monitor sections.
6. For a read-only summary, `render_zed_todos_categorized_surface` assembles all four blocks from state flags.

`ZedTodosDockPrototype` in `thread_view.rs` is the canonical full-fidelity reference (plan accept, clear, all four categories, matching activity-bar behavior).

### Risk classification

`acp_thread::ApprovalRisk` is the single source of truth. Use `tool_call.approval_risk()`, `approval_risk_for_tool_call`, or `approval_risk_for_operation` for proposed plans. Labels are `"RO"` or `"Destructive"`; chips use Success vs Warning colors via `render_risk_chip`.

### ACP capture harness

`acp_tools` can write structured session artifacts under `captures/acp-capture/` (messages, observed tool-call schemas, plan/todo samples). Use this baseline when matching native tool input shapes to bridged Grok TUI behavior.

### Plan proposed heuristic

A plan is **proposed** when all `todo_write` entries are pending (none in progress or completed). `Plan::is_proposed()` drives the banner, risk chip, and accept button in `render_plan_summary`. Accept clears the plan as the adoption signal.

## High-level features

Fork adds a native-first Grok Build stack on top of upstream Zed's agent panel. **170 files** differ from `main` (+35k/−9.5k lines). Category detail below.

| Feature | Summary |
|---------|---------|
| [Native Grok agent](#native-agent-core) | Rust/GPUI agent runtime, `GrokNativeServer`, native tools, system fragments |
| [Datastore](#datastore--persistence) | heed3 + rkyv LMDB; legacy SQLite KVP terminated |
| [Classified todos surface](#agent-ui--conversation) | `ZedTodosComponent`, Full Agent Mode, approvals/plans/monitors |
| [Agent skills](#agent-skills-system) | Multi-root `.agents` + `.grok` skills, `agent_skills` crate |
| [Agent tools](#agent-tools--permissions) | `todo_write`, `monitor`, plan tools, permissions UI |
| [xAI provider](#xai--llm-provider) | `x_ai` crate, language model provider wiring |
| [Bridged ACP Grok](#native-agent-core) | Optional `~/.grok/bin/grok` stdio path (legacy compatibility) |
| [memory_palace / ref](#memory_palace--ref) | Memory-palace reference subtree + native crate; [native memory replacement plan](#native-memory-replacement--scoped-plan) |
| [Linux grok-first cold start](#linux-grok-first-cold-start) | Auth skip, ACP transport filter, prewarm, launch timing |
| [Decisions](#decisions--policy) | `PLAN.md`, `.rules` binding constraints |

## Native Grok Build completion charter

**North star:** Replicate all Grok Build TUI features as a complete, non-bridged, first-class native Rust + GPUI implementation inside Zed. The external `~/.grok/bin/grok` stdio path is legacy compatibility only — not the source of truth for new work. Strategic intent also lives in `PLAN.md`; this section is the maintainer-facing status matrix against that intent.

**No half-measures:** Sub-agent tracking with personas, all skills, background monitors, plan discipline, categorized todos surface, memory, and session continuity must reach full parity with captured ACP harness fidelity before the bridged path can be demoted.

### Pillar status

| Pillar | Implementation | Tests | Primary locations | Open work |
|--------|----------------|-------|-------------------|-----------|
| **1. Full native GPUI replication** | **Partial** — categorized todos surface, Full Agent Mode, thread UI, native tools registered; `GrokNativeServer` remains a contract skeleton routed via `agent_ui` | `cargo test -p agent native_grok` (charter scope; see below) | `agent_ui/`, `acp_thread/`, `agent_servers/native_agent_server.rs` | Wire `grok-native` as default when binary absent; complete native run loop independent of ACP stdio |
| **2. Non-bridged first-class (subagents, skills, personas)** | **Partial** — `spawn_agent`, `skill`, persona/capability fragments, `SubagentContext`, weak subagent handles; UI collectors share bridged + native | `contract_subagent_spawn_*`, `contract_tool_calling_*`, persona tests in `mod.rs` / `templates.rs` | `agent/thread.rs`, `agent/tools/spawn_agent_tool.rs`, `agent_skills/` | Full subagent tree UI in categorized surface; native path default over bridged on Linux |
| **3. IDE diagnostics as primary context** | **Partial** — `build_project_diagnostics_context`, `<diagnostics>` injection, system-fragment prohibition on `cargo check`/`clippy` | `mod.rs` `GROK_BUILD_SYSTEM_FRAGMENTS` assertions; `thread.rs` diagnostics builder | `agent/thread.rs`, `project` LSP summaries | ACP/bridged threads: same diagnostics push on `EndTurn`; cross-language coverage tests beyond Rust |
| **4. Completion notifications (desktop + in-Zed)** | **Partial** — `dispatch_grok_completion_system_notification` on exact completion phrase; toast + pop-up when unfocused; prompt rule in fragments | `test_native_grok_profile_triggers_system_notification_on_exact_completion_phrase`; fragment assertions in `mod.rs` | `agent_ui/conversation_view.rs` | Linux desktop notification via ashpd when window unfocused; test must assert toast/pop-up fired (currently type-pin only) |
| **5. Planning workspace (no markdown pollution)** | **Partial** — `todo_write` / `enter_plan_mode` drive ZedTodos plan section; git worktree support on thread create; **not** a hidden TUI planning workspace | Plan proposed heuristic tests; `thread_worktree_archive` worktree plan tests | `acp_thread/`, `agent_ui/thread_view.rs`, `thread_worktree_archive.rs` | Replace opaque Grok planning workspace with ZedTodos + `memory_palace` session captures; no `PLAN.md`-on-disk dependency for native path |
| **6. heed3 + rkyv MVCC persistence** | **Partial** — `thread_metadata_store` LMDB (`agent_panels`, `agent_kv`), `prompt_store` on heed3; legacy SQLite KVP terminated for agent panel state | `thread_metadata_store` roundtrip tests; `surmount_auth_skip_is_disabled_under_cfg_test` | `thread_metadata_store.rs`, `prompt_store.rs`, `memory_palace/` | TODO: migrate remaining `db::kvp` agent paths; retire thread SQLite artifacts to palace/heed3; eliminate heed2 if any linger |
| **7. Linux grok-first cold start** | **Done** (2026-06-17) — auth skip, ACP transport filter, prewarm, launch timing | `agent_servers` `dropped_at_transport`; `agent_ui` `launch_elapsed_ms`; `project` auth-skip test | See [Linux grok-first cold start](#linux-grok-first-cold-start) | Re-apply on upstream merges; bridged ACP prewarm until native default ships |

### Verification scopes (keep separate)

Two independent regression surfaces — do not conflate them:

| Scope | Pillars | When to run | Command |
|-------|---------|-------------|---------|
| **Charter** | 1–6 (native completion) | Any change to `agent/`, `acp_thread/`, native tools, fragments, contracts, `memory_palace/` | `cargo test -p agent native_grok` |
| **Cold start** | 7 only | Changes to immersive startup, auth skip, ACP transport filter, `zed.rs` panel init | [Linux grok-first cold start](#linux-grok-first-cold-start) commands |

**Charter command** matches ~69 tests: `native_grok_contracts` (`contract_*`), `native_grok_surface_tdd`, `test_native_grok_*` in `mod.rs`, and production tests whose names include `native_grok` (tool registration, diagnostics injection, persona propagation, etc.). This is the single command to run after charter work — includes the two you already verified:

- `contract_tool_calling_all_grok_native_tools_are_registered`
- `test_native_grok_build_profile_injects_three_behavioral_rules_and_turn_id`

```bash
cargo test -p agent native_grok
```

Equivalent wrapper: `./script/test-surmount-charter`

**Cold-start command** (separate from charter — `agent_ui` immersive startup, not native completion):

```bash
cargo test -p agent_ui test_grok_ -- --nocapture
```

When touching cold-start files also run `cargo test -p agent_servers dropped_at_transport` and `cargo test -p project surmount_auth`. Wrapper: `./script/test-surmount-cold-start`.

TODO: Add CI jobs `surmount-charter` and `surmount-cold-start` mirroring the two scopes above.

### Bridged vs native honesty

Code still maintains a substantial bridged ACP surface (`agent_servers/acp.rs`, default Linux `grok` synthesis). PLAN.md states bridged is legacy; SURMOUNT tracks this as **transitional** until pillar 1 ships native default routing. Do not remove bridged paths without native parity tests green.

## Categories

Entries are added as diffs are reviewed. Each section states observable changes and effects.

### Datastore / persistence

**heed3 + rkyv LMDB backend (`thread_metadata_store.rs`)** — Adds `HeedThreadMetadataDb` backed by heed3/LMDB at `paths::data_dir()/agent_kv`. Separate named databases: `threads`, `archived_worktrees`, `thread_to_archived`, `agent_panels`, `agent_kv`. Values use `RkyvCodec` for archived zero-copy reads. `ThreadMetadataStore` now holds optional `kv_db` alongside the existing SQLite `ThreadMetadataDb`. SQLite remains for a one-release transition with `migrate_from_sqlite()` on first open.

**KVP migration off SQLite KVP** — Agent-panel and categorized-surface state no longer uses `db::kvp::KeyValueStore`. Migration completion flags (`THREAD_REMOTE_CONNECTION_MIGRATION_KEY`, `THREAD_ID_MIGRATION_KEY`, etc.) move to `save_global_json` / `load_global_json` on the heed3 backend. `draft_prompt_store` read/write/delete now goes through `ThreadMetadataStore` `agent_kv` helpers (`load_agent_kv_string`, `set_agent_kv_string`, `delete_agent_kv`); `write`/`delete` take `&mut App`.

**Agent panel + categorized todos serialized state** — `agent_panels` LMDB database stores `SerializedAgentPanel` / `SerializedZedTodos` (categorized persistent todos: approvals, plans, monitors). Tests assert roundtrip for panel and ZedTodos state through the rkyv codec.

**Native Grok artifacts in thread SQLite (`db.rs`)** — `DbThread` and `SharedThread` gain `native_grok_artifacts: Option<serde_json::Value>` (plans, monitors, memory, `current_turn_id`, etc.). `SharedThread` also gains `profile: Option<AgentProfileId>` (preserved on import; was previously dropped). Schema versions bumped: `SharedThread` 1.0.0→1.1.0, `DbThread` 0.3.0→0.4.0. Legacy JSON without the field deserializes as `None`.

**Thread list performance index (`db.rs`)** — Adds SQLite index `idx_threads_updated_created ON threads(updated_at DESC, created_at DESC)` for `list_threads()` at launch.

**Grok TUI session import scaffold (`grok_persistence.rs`)** — New module: `GrokSessionStore` trait, `GrokSession`/`GrokSessionArtifacts` types (including `turn_id` for native thread restore), `migrate_grok_tui_session()` delegating to `GrokTuiSessionStore::load_raw_artifacts` with injectable file reader for tests. Most symbols `#[allow(dead_code)]` pending import wiring.

**Thread store artifact tests (`thread_store.rs`)** — GPUI test confirms `native_grok_artifacts` (turn id, plan slug) survives `ThreadStore` save/load.

### Native agent core

**`thread.rs` (+686 lines)** — Native Grok Build profile: `GROK_BUILD_SYSTEM_FRAGMENTS`, `is_grok_build_profile`, native artifact fields on threads, TurnId-aware prompt building, subagent persona/capability propagation, diagnostics injection from LSP, authorization loop wiring for native tools.

**`acp_thread.rs` (+1489 lines)** — Plan proposed heuristic, approval risk classification (`ApprovalRisk`, RO vs Destructive), Grok memory artifacts on thread, background monitor collectors, continuation prompts after `EndTurn`, categorized todos collectors used by `ZedTodosComponent`.

**`agent_server_store.rs` (+1526 lines)** — Grok binary discovery cache, co-equal indicator API, `GrokTuiSessionStore` for `~/.grok/sessions` artifact reads, default Linux `grok` command synthesis, native `grok-native` profile scaffolding, session resume/import hooks.

**`agent_servers`** — `GrokNativeServer` / `GrokNativeConnection` skeleton implementing `AgentServer`/`AgentConnection` for `grok-native` id (full native launch routed via `agent_ui`).

**`agent.rs`** — Native agent server integration, skills path handling, Grok-specific thread/profile wiring.

**`acp_tools`** — ACP capture harness writing artifacts to `captures/acp-capture/` for tool-schema fidelity work.

**`native_agent_server.rs`, `scheduler.rs`, `verification.rs`, `templates/`** — Native Grok orchestration, verification/self-check fragments, template updates for Grok Build mode.

**`grok_persistence.rs`** — See [Datastore](#datastore--persistence).

### Agent UI & conversation

**Classified todos surface** — `ZedTodosComponent` + `ZedTodosDockPrototype` in `thread_view.rs`; Full Agent Mode overlay (`OpenFullGrokSurface`, `OpenZedTodosSurface`) and activity-bar integration. See [Maintainer reference](#maintainer-reference). User docs: `docs/src/ai/external-agents.md#grok-build-xai`.

**`agent_panel.rs` (large)** — Serialized panel/todos state (`SerializedAgentPanel`, `SerializedZedTodos`), Grok thread discoverability, co-equal chip rendering, Full Agent Mode toolbar, heed3 restore path on startup, global last-used-agent persistence via metadata store.

**`conversation_view/` + `thread_view.rs`** — Grok plan summary rendering, approval rows, persona badges, background monitors, memory section, `NewGrokThread` entry points.

**`entry_view_state.rs`** — `reconcile_with_thread` and gap-filling `sync_entry` recover when thread entry count outpaces cached `EntryViewState` (GPUI test included).

**`thread_switcher.rs`, `draft_prompt_store.rs`, `thread_metadata_store.rs`** — Thread switching and metadata; draft prompts and panel state on heed3 path (see Datastore).

**`docs/src/ai/`** — Grok user guide rewritten; `external-agents.md` Grok section trimmed to end-user content.

### Agent tools & permissions

**Native Grok tools (`tools.rs`, `tools/`)** — `TodoWriteTool`, `MonitorTool`, `UpdatePlanTool`, `enter_plan_mode` shapes matching ACP capture harness; `spawn_agent_tool` with persona; `skill_tool`; terminal/delete/find path tools; tool permission evals.

**`tool_permissions_setup.rs`** — Settings UI page additions for agent tool permissions.

**`update_plan_tool.rs`** — Plan/todo input shapes and proposed-plan detection for native fidelity.

### Agent skills system

**`agent_skills` crate** — Multi-root skill discovery: `.agents/skills/`, `~/.grok/skills/`, `~/.grok/bundled/skills/` with `GrokUser`/`GrokProjectLocal` scope types; precedence `.agents > .grok/user > bundled`.

**`.agents/skills/`** — Project agent skills (`branch-differences-documenter`, `surmount-merge-review`, `refactor-debug`, `hygiene`, `gpui-test`, etc.).

**`skill_tool.rs`, `agent_skills.rs`** — Agent runtime skill loading and invocation.

**`prompt_store`** — Large expansion; `rules_to_skills_migration.rs` migrates rules content toward skills model.

**`crates/agent_skills/README.md`** — Documents Grok + `.agents` multi-root layout.

### xAI / LLM provider

**`crates/x_ai/`** — xAI provider crate adjustments for Grok models.

**`language_models/src/provider/x_ai.rs`** — Provider wiring in language models layer.

**`agent_configuration/` modals** — LLM provider and profile configuration UI touches.

**`settings_content/language_model.rs`** — Settings schema additions for xAI/Grok models.

### Keymaps & agent actions

**`zed_actions`** — `NewGrokThread`, `OpenZedTodosSurface`, `OpenFullGrokSurface` actions.

**`assets/keymaps/`** — `ctrl-alt-x` / `cmd-alt-x` → `NewGrokThread`; `ctrl-alt-shift-t` / `cmd-alt-shift-t` → `OpenFullGrokSurface` (Linux/macOS). Windows: palette/button/menu only.

**`keymap_file.rs`, `keymap_editor/`** — Keymap loading/editor tweaks for new actions.

### Linux grok-first cold start

Surmount on Linux with a discovered `~/.grok/bin/grok` binary cold-starts into maximized Full Agent Mode (ZedTodos left, Grok thread right). These fork-specific choices reduce startup latency and log noise; **re-apply deliberately on upstream merges** — upstream will re-enable auth and add ACP handlers that may overlap.

| Decision | Location | Rationale |
|----------|----------|-----------|
| Skip Zed cloud GitHub sign-in | `crates/zed/src/main.rs` (`authenticate` spawn) | Grok-first users do not need collab/GitHub auth at cold start; avoids network work on the critical path |
| Skip background LM provider auth | `crates/agent/src/agent.rs` (`authenticate_all_language_model_providers`) | ChatGPT Subscription (`openai-subscribed`), Copilot Chat, OpenAI, etc. are not warmed on grok-first launch |
| Auth gate helper | `project::surmount_skips_upstream_auth_on_cold_start()` | Single predicate: `grok_build_default_agent_available() && !cfg!(test)` |
| Prewarm Grok ACP subprocess | `crates/zed/src/zed.rs` (`initialize_agent_panel`) | `new_external_agent_thread(grok)` before immersive open so `RootThreadUpdated` arrives sooner |
| Synchronous immersive open | `crates/zed/src/zed.rs`, `agent_ui::AgentPanel::open_full_grok_immersive_from_workspace` | Cold start calls this directly (not `dispatch_action`) so dock open + workspace zoom land in the same update cycle |
| Drop orphan `skills-reload` responses | `crates/agent_servers/src/acp.rs` (stdio transport filter) | Grok agent emits unsolicited JSON-RPC responses with `id: "skills-reload"`; SDK warns every ~2s without filter |
| Swallow `_x.ai/*` extension notifications | `crates/agent_servers/src/acp.rs` (same filter) | `_x.ai/settings/update`, `_x.ai/announcements/update` have no Zed handlers yet; avoids INFO reject spam |
| Launch phase timing | `crates/agent_ui/src/agent_panel.rs` | `grok_immersive_launch_started_at` + `launch_elapsed_ms` in diagnostics; INFO logs at `surface_pending`, `zoom_applied_sync`, and startup complete |

Tests (cold-start scope only — not charter): `agent_servers` `dropped_at_transport`; `agent_ui` `test_grok_*` (`launch_elapsed_ms`); `project` `surmount_auth_skip_is_disabled_under_cfg_test`. Run via `./script/test-surmount-cold-start`.

#### Immersive startup pitfalls (maintainer notes)

**Product invariant:** Linux grok-first cold start must always land in fully maximized Full Agent Mode (agent dock open, workspace zoomed, ZedTodos categorized surface) — never editor-first, never side-panel-only. Code comments mark this as `SURMOUNT INVARIANT` in `agent_panel.rs` and `zed.rs`.

**Why sync zoom matters:** Layout maximization is driven by `workspace.zoomed_position`, not `panel.zoomed` alone. Emitting `PanelEvent::ZoomIn` from nested `panel.update` does not reliably set `workspace.zoomed_position` on the same frame — logs can show `zoom_applied_sync` with `zoomed=false workspace_zoomed=false` while internal flags later go green. Fix: `Workspace::zoom_dock_panel` (uses `set_panel_zoomed_no_serialize` to avoid double-lease) called from `sync_grok_immersive_zoom_from_workspace`; log marker is `grok immersive launch phase: zoom_applied_sync` and must show `zoomed=true workspace_zoomed=true`.

**Entry point:** `AgentPanel::open_full_grok_immersive_from_workspace` (workspace context) opens dock, arms startup via `arm_grok_immersive_startup`, then `sync_grok_immersive_zoom_from_workspace` → `Workspace::zoom_dock_panel`. Wired from `OpenFullGrokSurface` action and `initialize_agent_panel` — cold start must call the workspace helper directly, not `window.dispatch_action` alone.

**Double-lease:** Never `workspace.read()` from inside nested `panel.update` during immersive startup while a `workspace.update_in` lease is active. `agent_dock_open_hint` avoids probing the dock in that window — set it in `open_full_grok_immersive_from_workspace` only **after** `workspace.open_panel`. Do **not** call `schedule_grok_immersive_reveal_until_ready` from `arm_grok_immersive_startup` (its diagnostics read the dock). `reassert_grok_immersive_maximized` must not set `agent_dock_open_hint` before the dock is actually reopened. Tests: `test_grok_set_active_with_surface_after_startup_avoids_double_lease`, `test_grok_open_full_immersive_from_workspace_matches_cold_start_path`.

**Awaiting ACP thread:** When `active_agent_thread` is still `None`, `open_zed_todos_surface` returns early. `schedule_grok_awaiting_thread_surface_retry` spaces retries with `background_executor().timer(500ms)` — never chain `window.defer` for this path (GPUI can drain the whole chain in one frame, burning all 48 attempts instantly). Never call `schedule_grok_immersive_startup_completion` synchronously from `ensure_grok_categorized_surface`. `set_active(false)` during startup clears `agent_dock_open_hint`; `reassert_grok_immersive_maximized` must not set the hint before `open_panel` runs. Tests: `test_grok_awaiting_thread_does_not_retry_synchronously_on_arm`, `test_grok_set_active_false_clears_dock_hint_during_startup`, `test_grok_reassert_does_not_set_dock_hint_before_reopen`, `test_grok_dock_close_during_startup_reopens_panel`.

**Pre-first-frame stalls (environmental):** Hang traces may show ~19–106s blocked at `Workspace::new_local` (`workspace.rs:1884` async open: path canonicalization, serialized workspace restore, worktree creation) and GPU adapter selection before `Rendered first frame`. ACP registry CDN timeout (30s) and context-server init (60s) stack on power-saver runs. Sync zoom may still log `zoom_applied_sync` on first frame; categorized surface opens only after `RootThreadUpdated` or timer-spaced `schedule_grok_awaiting_thread_surface_retry` once the thread exists.

**Measured cold start (2026-06-18, same binary):**

| Power mode | Start → first frame | `elapsed_ms` at startup complete | Wall clock |
|------------|---------------------|----------------------------------|------------|
| High performance | ~19s | ~8281ms | ~39s |
| Power saver | ~106s | ~4518ms (after first frame) | ~2m21s |

Grok immersive `elapsed_ms` measures from `surface_pending`; most user-visible delay on power saver is pre-first-frame workspace/GPU work, not categorized-surface attach.

**Awaiting-thread ensure dedup:** While `active_agent_thread` is `None`, `ensure_grok_categorized_surface` must return before `open_zed_todos_surface` (timer + `RootThreadUpdated` only). `defer_ensure_grok_categorized_surface` and serialized panel load skip ensure during `grok_immersive_startup_in_progress`. Tests: `test_grok_ensure_skips_surface_open_while_awaiting_thread`, `test_grok_awaiting_thread_does_not_retry_synchronously_on_arm`, `test_grok_open_full_immersive_from_workspace_matches_cold_start_path`, `test_grok_cold_start_startup_complete_matches_last_known_good`.

**Palette `agent: toggle` spin-close:** `Workspace::toggle_panel_focus` may call `close_panel` when `close_panel_on_toggle` is set and focus leaves the panel. Reasserting after that toggle is too late — the dock is already closed. Fix: when `grok_immersive_must_stay_maximized()` is true, short-circuit toggle/toggle-focus to `open_panel` + `reassert_grok_immersive_maximized` without calling `toggle_panel_focus`. Test: `test_grok_toggle_must_not_close_immersive_maximized_mode`.

**`set_zoomed(false)` during startup:** `AgentPanel::set_zoomed` ignores unzoom requests while `grok_immersive_startup_in_progress` or `grok_immersive_must_stay_maximized` and schedules reveal instead.

**Diagnostics vs visuals:** Internal flags (`zoomed`, `grok_workspace_zoom_overlay_synced`, `startup_logged`) can go green while the user still sees editor-first if zoom was deferred or dock restore ran before immersive skip. Trust `zoom_applied_sync` timestamp in logs (should match `surface_pending`), not flags alone.

**Key symbols:** `open_full_grok_immersive_from_workspace`, `sync_grok_immersive_zoom_from_workspace`, `Workspace::zoom_dock_panel`, `arm_grok_immersive_startup`, `agent_dock_open_hint`, `grok_immersive_must_stay_maximized`, `reassert_grok_immersive_maximized`, `defer_ensure_grok_categorized_surface`.

**Last known good production log (2026-06-18):** All three INFO lines share the same second on cold start; `zoom_applied_sync` must show `zoomed=true workspace_zoomed=true` (not `false`). Surface opens ~2s later; startup complete shows all layout flags green.

```
grok immersive launch phase: surface_pending
initialize_agent_panel: grok immersive launch phase: opening full grok surface
grok immersive launch phase: zoom_applied_sync (active=true zoomed=true workspace_zoomed=true ... dock_open=true ... pending=true startup_in_progress=true ... zoom_defer_scheduled=false zoom_apply_in_flight=false shows_startup_splash=true)
open_zed_todos_surface: opened categorized surface (...)
grok immersive startup complete: agent panel dock revealed and zoomed (elapsed_ms=Some(~2254), ... ready=true visual_ready=true logged=true ... pending=false startup_in_progress=false)
```

**Regression tests:** Phase helpers `assert_grok_sync_zoom_phase`, `assert_grok_startup_awaiting_thread_phase`, `assert_grok_startup_surface_open_phase`, `assert_grok_startup_complete_phase` encode LKG flag shapes. `test_grok_open_full_immersive_from_workspace_matches_cold_start_path` asserts full `assert_grok_sync_zoom_phase` including `Workspace::zoomed_dock_position` with zero parks (production `zed.rs` path). `test_grok_open_full_surface_must_maximize_before_thread_ready` covers palette/button via `window.dispatch_action` (panel-level LKG flags). Also: `test_grok_cold_start_startup_complete_matches_last_known_good`, `test_grok_startup_last_known_good_sequence`, `test_grok_toggle_must_not_close_immersive_maximized_mode`, `test_grok_build_must_default_to_fully_maximized_categorized_surface_on_editor_open`.

### Decisions & policy

From `PLAN.md` (new file, explicit statements only):

- **Primary goal:** Full native Rust + GPUI re-implementation of Grok Build; external binary is legacy compatibility only.
- **Non-negotiable:** Linux-first for platform UX porting only; agents are not OS-scoped (see AGENTS.md § No OS–agent coupling); native Rust/GPUI; TDD; efficiency/latency first-class; edit existing files.
- **Persistence:** heed3 + rkyv LMDB only; SQLite KVP legacy path terminated.
- **Diagnostics:** Zed LSP diagnostics are primary agent context (not shell cargo builds).
- **Contradictions:** Track in todos/living docs; escalate for human resolution — agents must not silently resolve.

From `.rules` / `AGENTS.md` (binding, already enforced):

- Native-first Grok; bridged stdio path optional.
- Grok default command synthesized in `agent_server_store` on all platforms when the binary is discovered (not Linux-only).
- Fork documentation map points to this file.
- No ephemeral slice identifiers in code or docs.

### Workspace & build config

**`Cargo.toml`** — Workspace member `memory_palace`; deps `heed3`, `rkyv`, `paths`, `sha2`.

**`Cargo.lock`, `supply-chain/`** — Lockfile and audit config updates (large `supply-chain/config.toml` addition).

**`.cargo/audit.toml`, `.config/nextest.toml`** — Build/test tooling config.

**`.gitmodules`** — `ref/vibe-palace` submodule.

**`.rules` / `AGENTS.md`** — Surmount agent binding rules (symlinked).

**`PLAN.md`** — Strategic intent document (see Decisions).

### Testing appendix

Maps to [Native Grok Build completion charter](#native-grok-build-completion-charter) pillars 1–6. Run `cargo test -p agent native_grok` after changes. Cold-start tests (`agent_ui` `test_grok_*`) are a [separate scope](#verification-scopes-keep-separate).

**`agent/src/tests/native_grok_contracts.rs`** — Native Grok contract tests: tool surface parity, TurnId serialization, monitor/plan shapes, performance validation hooks.

**`agent/src/tests/mod.rs` (+2k lines)** — Native Grok Build TDD: monitor fidelity, plan discipline, persona on spawn, prompt fragments, artifact roundtrips.

**`agent_servers/e2e_tests.rs`** — Agent server end-to-end coverage.

**`collab/tests/integration/agent_sharing_tests.rs`** — Agent sharing integration touches.

**`agent_ui/test_support.rs`** — Test support helpers for agent UI GPUI tests.

**`tools/evals/`** — Tool evaluation fixtures including Grok/zode prompts.

### Misc upstream-touching tweaks

~50 non-agent crates have small diffs (editor, git_ui, onboarding, collab, languages, terminal, search, debugger, extensions, workspace, sidebar, etc.). Most changes are minor (single-digit to low tens of lines).

**`zed.rs`** — Agent panel early creation/loading; persisted state restore via heed3 path (comment explicitly notes no KVP).

TODO: Human review needed to classify which misc files are intentional surmount work vs upstream merge drift. Candidates for intentional touch: `onboarding`, `ai_onboarding`, `workspace/welcome`, `sidebar`, `edit_prediction_*`.

### memory_palace & ref

**`ref/vibe-palace`** — Git submodule (memory-palace reference implementation).

**`ref/README.md`** — Documents the ref subtree purpose.

**`crates/memory_palace/`** — Native `memory_palace` crate (heed3 + rkyv, Linux-first). See crate README.

### Native memory replacement — scoped plan

Goal: make `memory_palace` the **source of truth** for Grok persistent memory inside Zed, replacing the current read-only bridge over Grok Build's filesystem (`MEMORY.md`, `~/.grok/memory/`, `worktrees.db` facts). Bridged TUI users keep optional import/sync; native Grok threads never depend on the external binary for memory.

#### Today (bridge)

| Layer | Location | Behavior |
|-------|----------|----------|
| Read API | `project::GrokMemoryArtifacts`, `grok_memory_artifacts_for_cwd` | RO loads from workspace/global `MEMORY.md` + sqlite CLI over `worktrees.db` |
| Thread accessors | `acp_thread::grok_memory`, `agent::Thread::grok_memory` | Delegate to project read path |
| Prompt injection | `agent/src/thread.rs` | `## Grok Persistent Memory` + `## Grok Learned Facts` sections |
| UI | `render_grok_memory_items` in `thread_view.rs` | RO chips, copy buttons, empty-state TUI hint |
| Tests | `native_grok_contracts.rs`, `agent_server_store` injectable `_with` helpers | Hermetic path predicates |

No native write path exists; Grok TUI or manual `MEMORY.md` edits are the writers.

#### WIP foundation (`memory_palace`)

`crates/memory_palace/src/memory_palace.rs` — `MemoryPalace` on heed3 + rkyv: `MemoryRecord` with kinds `SessionCapture`, `Observation`, `Decision`, `Skill`; substring `retrieve_relevant`; `get_context_for_prompt`; per-project/global layout under `paths::data_dir()`.

`ref/vibe-palace` — Full Go reference (vault, capture, semantic search, palace graph, 57 MCP tools). **`ref/README.md` explicitly rejects MCP and Obsidian for the Zed port**; use ref for capability intent and data-model ideas, not as a line-for-line target.

#### Alignment with ref PRD (what to port vs defer)

| Ref capability | Zed port stance |
|----------------|-----------------|
| Phase 1 storage engine | **Port** — heed3 + rkyv (already chosen; matches `agent_kv` / PromptStore direction) |
| Phase 5 session capture | **Port** — hook native thread turn-end / completion to `capture_session` |
| Phase 18 Zed transcript adapter | **Absorb in-process** — no separate archive CLI; read Zed thread state directly |
| Phase 3 context injection | **Port** — replace `grok_memory` prompt sections with `get_context_for_prompt` |
| Phase 8 migration/import | **Port once** — one-time ingest from `~/.grok` + workspace `MEMORY.md` |
| Phase 4 semantic search (HNSW/embeddings) | **Defer** — keep substring search until palace core is stable |
| Phase 6–7 palace graph / knowledge graph | **Defer** — `links` field reserved; no wing/room classifier yet |
| Phase 9–17 CLI, vault git, templates, hooks | **Reject** — no standalone binary, no Obsidian vault, no MCP exposure |
| MCP tool surface (57 tools) | **Reject** — express as native agent tools + categorized todos UI |

#### Target architecture

```
paths::data_dir()/memory_palace/
  global/          # cross-project facts (replaces ~/.grok/memory/MEMORY.md)
  projects/<hash>/ # per-worktree palace (replaces cwd/MEMORY.md + slug dirs)
```

- **Facade crate boundary:** expand `memory_palace` with `MemoryPalaceStore` (open per cwd, lazy global) and a **view model** that mirrors today's `GrokMemoryArtifacts` fields enough for UI/prompt consumers, or replace `GrokMemoryArtifacts` with `MemoryArtifacts` and adapter during transition.
- **Ownership:** `memory_palace` owns storage + query; `project` or `agent` owns lifecycle (open on project load, close on worktree archive). Prefer `project` if bridged and native threads both need cwd-scoped access without pulling in `agent`.
- **Concurrency:** follow `HeedThreadMetadataDb` patterns — short read txns, single writer per env, `map_size` tuned per palace.

#### Work phases

**Phase A — Crate hardening (memory_palace)**

- Unit tests for open/store/retrieve/kind-filter/prompt-context (Linux-first).
- `MemoryPalace::open_for_project(cwd)` + `open_global()` under `paths::data_dir()`.
- Stable `MemoryRecord` schema + migration version key in LMDB metadata db.
- Optional: secondary index db keyed by `(kind, id)` if full scans become hot (not needed for MVP).

**Phase B — Read-path swap (dual-read transition)**

- Add `memory_artifacts_for_cwd` in `memory_palace` (or thin `project` wrapper) returning UI/prompt-ready structs.
- Wire `Thread::grok_memory` and `acp_thread::grok_memory` to palace first, **fallback to filesystem bridge** when palace empty and `~/.grok` artifacts exist.
- Update `render_grok_memory_items` to render palace records (kind label, id, copy) — keep RO chips; drop TUI-only empty-state copy once native write exists.
- Extend `native_grok_contracts` with palace injectable fixtures (tempdir LMDB), keep existing filesystem contract tests until bridge removal.

**Phase C — Write path**

- Native agent tool (e.g. `remember` / `record_observation`) gated to `is_grok_build_profile`, writes through `MemoryPalaceStore`.
- Turn-end hook: on thread idle/completion, append `SessionCapture` summary (bounded size, same discipline as ref capture chunking).
- Risk classification: memory writes = ReadOnly surface item or auto-approved RO tool (match approvals/plans/monitors policy).
- **Do not** write `MEMORY.md` by default once palace is authoritative; optional export for TUI roundtrip behind explicit user action.

**Phase D — Import / co-equality**

- One-shot importer: workspace `MEMORY.md`, `~/.grok/memory/**/*.md`, facts from `worktrees.db` → typed `MemoryRecord`s with provenance metadata.
- Idempotent import marker in global palace (skip re-import).
- Bridged threads: continue RO filesystem read until user runs import or native thread supersedes.

**Phase E — Session capture depth (post-MVP)**

- In-process transcript summarization on native threads (ref Phase 5 + 18 intent without external archive pipeline).
- Decision/skill records linked via `links` when plan accept or skill activation occurs.
- PromptStore unification: memory fragments that today live in prompt_store could share heed3 patterns (separate milestone; do not block Phase A–D).

#### Files to touch (by phase)

| Phase | Files |
|-------|-------|
| A | `crates/memory_palace/src/memory_palace.rs`, `crates/memory_palace/Cargo.toml` |
| B | `crates/project/src/agent_server_store.rs` (bridge + facade), `crates/acp_thread/src/acp_thread.rs`, `crates/agent/src/thread.rs`, `crates/agent_ui/.../thread_view.rs`, `crates/agent/src/tests/native_grok_contracts.rs` |
| C | `crates/agent/src/tools.rs`, new or existing tool module, `crates/agent/src/thread.rs` (turn hooks) |
| D | `crates/memory_palace/` import module, optional `crates/agent/src/grok_persistence.rs` extension |
| E | `crates/agent/src/db.rs` / thread persistence, `crates/agent_ui/src/thread_metadata_store.rs` (if capture state needed) |

#### Success criteria

- Native Grok thread with no `~/.grok` install still gets persistent memory across Zed restarts.
- Categorized todos **Grok Memory** section shows palace records, not filesystem previews.
- System prompt memory sections come from palace query, not `MEMORY.md` slurp.
- Existing injectable tests pass; new palace tests cover store/retrieve/import.
- Bridged users can import TUI memory once and continue in native path without manual duplication.

#### Open decisions (TODO: human)

1. **Facade naming** — keep `GrokMemoryArtifacts` as adapter vs rename to agent-neutral `MemoryArtifacts`.
2. **Global vs per-project split** — mirror Grok's global `MEMORY.md` + per-slug dirs exactly, or simplify to cwd-hash only.
3. **Bridged write-back** — whether Zed ever writes `MEMORY.md` for TUI roundtrip or import-only one direction.
4. **Semantic search timing** — trigger after Phase E stable, or when substring search fails real usage.

## Remaining human review

Items agents cannot close without maintainer judgment:

1. **Misc category** — Which ~50 peripheral crate diffs are intentional surmount work vs merge drift?
2. **Category entries** — Review all sections above for accuracy against your intent (diff-derived, not verified line-by-line).
3. **PLAN.md vs code** — PLAN states bridged path is "legacy only"; code still maintains substantial bridged ACP surface. Track explicitly if this is transitional.
4. **Upstream merge** — Re-run `git diff main...surmount` before each merge attempt; this doc reflects one point-in-time snapshot.

## Vector Store Decision

**GitHub link for LanceDB:**

**https://github.com/lancedb/lancedb**

(The core Lance format is at https://github.com/lancedb/lance — both are actively maintained and written in Rust.)

### Full Summary: Why LanceDB Was Selected

After evaluating your requirements across multiple rounds (embedded Rust vector store, significantly better performance than `sqlite-vec`, strong results on speed/throughput, reliability/durability, parallelism/concurrency, space + time efficiency, local-first operation, and explicit avoidance of heavy LSM-tree/RocksDB-style storage engines), **LanceDB** emerged as the clearest best fit.

Here is the consolidated reasoning:

#### 1. Matches Your Core Technical Priorities Extremely Well
- **Performance (time)**: Delivers low query latency on large datasets (typically 1–5 ms range on million-scale high-dimensional vectors with proper indexing and tuning, sometimes single-digit ms). Uses optimized **IVF-PQ** ANN indexes with a refine step for excellent speed/recall trade-offs. Heavy use of Rust SIMD for distance computations and other hot paths. Columnar layout + zero-copy Arrow integration gives strong scan and random-access performance.
- **Space efficiency**: Columnar storage + Product Quantization in indexes provides good compression. Recent Lance format improvements (v2.2) show significant storage savings vs. Parquet equivalents in many workloads while preserving (or improving) access speed. Much more efficient than pure in-memory HNSW stores for anything beyond moderate sizes.
- **Reliability & correctness**: Entirely Rust-based core. Built-in automatic **versioning** (cheap time-travel and reproducibility — a major advantage for AI/ML pipelines). Durable file-based persistence with manifests/commits. Good crash-recovery characteristics via the storage layer.
- **Parallelism & concurrency**: Native async Rust SDK. Strong support for concurrent reads. Storage format (fragments + indices) designed for concurrent access. Integrates with DataFusion for parallel query execution where applicable. Benefits from Rust’s fearless concurrency model without data races.
- **Local-first embedded**: Runs fully in-process with no server. Connects to a simple local directory path (`connect("data/my_vectors")`). Data lives as ordinary files on disk — no heavy embedded database engine required.

#### 2. Avoids the Storage Model You Explicitly Rejected
- **No LSM-tree / RocksDB dependency**: LanceDB’s storage is the **Lance columnar lakehouse format** itself — a modern file format (with table + versioning layers) that writes directly to the filesystem (or object stores). 
- It does **not** use RocksDB, LevelDB, Sled, or any classic LSM-style engine with memtables, multi-level SST files, or background compaction machinery.
- This keeps the embedded footprint lighter and avoids the write amplification / compaction overhead characteristics you wanted to steer clear of, while still providing durability and good random access (often dramatically better than Parquet for the access patterns that matter for vectors and multimodal data).

#### 3. Better Than the Main Alternatives on Your Full Criteria
- **vs. sqlite-vec**: Far superior ANN indexing (IVF-PQ vs. basic extension functions), columnar efficiency, random access, versioning, and scalability. `sqlite-vec` is fine for tiny on-device cases but falls short on performance and modern vector workloads.
- **vs. SurrealDB**: Avoids the RocksDB backend (and general-purpose DB overhead). LanceDB is more specialized and efficient for pure vector + multimodal workloads.
- **vs. PolarisDB**: LanceDB wins on scale, random-access performance, versioning, ecosystem maturity, and production features. PolarisDB is a credible lighter pure-Rust WAL-based option (also non-LSM) and could be worth a quick look if you want something even more minimal, but it has less public large-scale benchmarking and fewer advanced capabilities.
- **vs. lighter/experimental options** (nano-vectordb-rs, SahomeDB, iqdb, early SatoriDB, etc.): LanceDB offers the best combination of performance, reliability features, and efficiency without feeling under-powered for serious use. The others are either too basic, less mature, or still carry extra storage layers you preferred to avoid.

#### 4. Additional Advantages That Align With Real-World Use
- Excellent support for the typical vector/AI access patterns: bulk appends + versioning, fast similarity search, metadata filtering, multimodal data (vectors + blobs + structured fields), and schema evolution.
- Strong Rust ergonomics (async, zero-copy with Arrow) plus good interoperability if you ever need to bridge to other tools.
- Proven in production AI workloads while remaining fully embeddable and local-first.
- Future-proof storage (works seamlessly on local disk today and can scale to object storage later with the same format).

#### Minor Caveats (for transparency)
- Write concurrency has practical limits under very high contention (retries on commit help, but it’s not an OLTP-style engine).
- Best suited to append-oriented + versioning workflows rather than extremely high-frequency point updates (the latter is where some LSM designs can shine, but that’s not the typical vector store pattern).

**Bottom line**: LanceDB gives you the best combination of raw performance, space/time efficiency, reliability features (especially versioning), concurrency characteristics, and lightweight embedded operation **without** relying on the heavy LSM-style storage engines you wanted to avoid. It is purpose-built for exactly the kind of local-first, high-performance vector workloads you described.


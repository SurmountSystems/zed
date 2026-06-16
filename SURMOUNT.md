# Surmount — upstream differences

Living record of technical differences between Zed upstream `main` and the `surmount` branch. Maintained chunk-by-chunk via the branch-differences-documenter skill (`.agents/skills/branch-differences-documenter/`).

Only describe differences explicitly visible in supplied diffs. Open questions use `TODO:` markers.

## Documentation map

| Layer | Location | Audience |
|-------|----------|----------|
| Technical diff record | This file (`SURMOUNT.md`) | Maintainers, agents, merge work |
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
| [Decisions](#decisions--policy) | `PLAN.md`, `.rules` binding constraints |

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

**`.agents/skills/`** — Project agent skills (`branch-differences-documenter`, `refactor-debug`, `hygiene`, `gpui-test`, etc.).

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

### Decisions & policy

From `PLAN.md` (new file, explicit statements only):

- **Primary goal:** Full native Rust + GPUI re-implementation of Grok Build; external binary is legacy compatibility only.
- **Non-negotiable:** Linux-first; native Rust/GPUI; TDD; efficiency/latency first-class; edit existing files.
- **Persistence:** heed3 + rkyv LMDB only; SQLite KVP legacy path terminated.
- **Diagnostics:** Zed LSP diagnostics are primary agent context (not shell cargo builds).
- **Contradictions:** Track in todos/living docs; escalate for human resolution — agents must not silently resolve.

From `.rules` / `AGENTS.md` (binding, already enforced):

- Native-first Grok; bridged stdio path optional.
- `~/.grok/bin/grok` Linux default synthesized in store.
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
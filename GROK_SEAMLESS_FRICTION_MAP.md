# Grok Build Seamless Integration Friction Map
## Seamless Integration Friction Auditor Report (2026-05-19)

**Mission accomplished (read-only exploration + synthesis):** This document is the actionable "friction map" identifying gaps preventing users from treating Zed's `grok` ACP path as a true co-equal environment for Grok Build work alongside (or instead of) the standalone TUI.

**References (all explored in this session):**
- Zed ACP `grok` path: full code in `crates/project/src/agent_server_store.rs` (discovery), `crates/agent_servers/src/custom.rs` (GROK_ID, env, logo), `crates/acp_thread/src/acp_thread.rs` (AgentPersona, is_monitor, Plan, ToolCall), `crates/agent_ui/src/conversation_view/thread_view.rs` (G-04/05/06 visuals: render_background_*, render_persona_badge, render_plan_*, activity bar, lazy TerminalView + HashSet expands), agent/agent.rs + agent_skills/.
- Local TUI capabilities (this exact Linux machine with real `~/.grok/bin/grok` v0.1.212): `grok --help`, `grok agent --help`, `grok inspect --json` (skills in ~/.grok/skills + bundled/, agents/personas, permissions), `grok sessions --help`, `grok memory`, `~/.grok/sessions/` (per-cwd dirs + sqlite + worktrees.db), config.toml, auth.
- Just-completed verification report (embedded in AGENTS.md "Real Binary Verification Report"): confirmed binary + ACP stdio + plan mode + all G-01/04/05/06/07/14 paths + honest gaps (no live GUI due to headless, TUI immersive vs Zed in-editor, ACP JSON vs direct, persona fixed vs dynamic, sessions/skills not bridged).
- Co-equal Zed / Grok Build Priority rule (AGENTS.md:383-389): must prioritize friction-reducing bridging work (skills, sessions, memory, plan/persona/monitor parity) over pure external features; Zed ACP + future native must be co-equal peer, not viewer/wrapper. "lean toward the latter [bridging or native foundations]".

All exploration used list_dir, grep (target-excluded, *.rs), read_file (key paths + full AGENTS.md 511 lines), run_terminal_command (read-only: which/ls/grok --help/inspect/sessions/memory, find ~/.grok/*).

---

## Current State of External `grok` ACP Path in Zed (Post G-01..G-14 + G-04/05/06 + Verification)

- **Discovery**: Rock-solid, zero-config on Linux (and Mac post G-08). `default_command_for_grok` + `discover_grok_command_with` probes ~/.grok/bin/grok + XDG + ~/bin + PATH (full path preferred for robustness), OnceLock caches both command and CONCRETE flag, `has_discovered_grok_binary()` O(1) for UI. Falls back to bare "grok" (still works). Matches exact TUI install script. Windows: todo!() only. Verified on real machine (symlink resolved, tests pass, clippy clean).
- **Visuals from G-04/G-05/G-06 (native GPUI, efficiency-first)**: 
  - Monitor (G-04): `is_monitor()` (MONITOR_TOOL_NAME + meta), `expanded_background_monitors: HashSet<ToolCallId>`, `render_background_tasks_summary` (cheap count), `render_background_task_items` (real ToolCall content + status icons + elapsed + *lazy* TerminalView only on per-item expand). O(1) collapsed paths, TDD `background_monitor_tdd`. Activity bar integration.
  - Plans (G-05): `Plan` + `progress_fraction`/`total_entries` (TDD), CircularProgress reuse, unified Spinner/Check/Circle icons, hover CopyButton on todos, enriched history. Cheap summaries.
  - Personas (G-06): `AgentPersona` enum (General/Implementer/Reviewer/Researcher/Explorer + from_name/serde TDD), wired to Subagent* structs + Thread/AcpThread, `render_persona_badge` (Chip + icon + color, O(1)). Shown in activity bar, tool cards, titlebars (collapsed cheap).
  All in existing files only; follows Efficiency Auditor register exactly; verified via real binary + tests + static render review.
- **Personas**: 5 + General fallback. Matches grok inspect output (general-purpose/explore/plan + skill-driven implementer/reviewer etc.). Subagent spawning via ACP works.
- **Plans**: Full support for ACP `Plan` updates + status. Grok's plan mode flows through.
- **Monitor/Background**: Binary emits "monitor" tool for long-running; Zed renders visually with lazy PTY. Parity on capability.
- **Skills**: When using ACP "grok", the *binary itself* loads/uses `~/.grok/skills/*` + `~/.grok/bundled/skills/*` (from `grok inspect`: help, create-skill, check, best-of-n, docx/xlsx/pptx, design/implement/review/pr-babysit etc.). Zed's native agent (non-grok) uses separate `~/.agents/skills` + project `.agents/skills` + builtin (agent_skills crate: global_skills_dir only points to .agents). No code shares the locations. Manual duplication required.
- **Sessions**: Zed: per-ACP-connection `AcpThread` + `SessionId` (in-memory maps in connection.rs/acp_thread.rs). Supports resume via trait `supports_resume_session`/`resume_session` (generic). Grok TUI: rich `grok sessions list/search`, `grok -r [ID]`, `-c` continue most recent, `--restore-code`, per-cwd URL-encoded dirs under `~/.grok/sessions/`, sqlite index, worktrees.db. No import/restore of grok session files or IDs into Zed grok threads. No "continue this TUI session visually" flow.
- **Memory/Persistent State**: TUI: `--experimental-memory`, `grok memory clear`, cross-session files (in skills/implement etc.). Grok state: auth.json, config.toml, models_cache, upload_queue, worktrees.db. Zed: own ThreadStore/context + rules (CLAUDE.md/AGENTS.md etc.). Zero sharing or sync.
- **Plan Approval Flows + Permission Modes**: Grok CLI: `--permission-mode plan|acceptEdits|auto|...`, `--no-plan`, plan skills with approval loops. Zed: `CustomAgentServer::default_mode`, `AcpSessionModes`, `SetSessionModeRequest` via ACP (surfaces grok's modes). Generic permission/awaiting UI + plan cards, but no Grok-specific "proposed plan awaiting approval" banner/state machine yet (risk noted in P4 planning).
- **Command Surface / Other**: Full toolset, /commands, MCP, reasoning-effort (via SessionConfigOption or native xAI), env passthrough (XAI + GROK_*). Keybinding (ctrl-alt-x Linux, cmd-alt-x Mac). Docs in external-agents.md mention zero-config + visuals. TUI extras like --best-of-n, --check, --agents JSON, --verbatim, worktree mgmt are binary-driven (work in ACP) but lack dedicated Zed toggles/UI.
- **Verified on Machine**: All above exercised (real grok + tests + clippy x3 + render path review). Honest gaps called out in verification report (headless no full E2E GUI, ACP overhead on bursts, TUI immersive vs panel, fixed personas, no bridging).

Zed ACP "grok" path is *production-ready for Linux visual use* with latest binary fidelity + superior in-editor rendering (diffs, terminals, activity bar). But **not yet co-equal for seamless movement**.

---

## Comparison vs Real `grok` TUI UX / Capabilities (Local + Public)

**TUI Strengths (Immersive Agent-Native)**:
- Full alternate-screen rich TUI with Grok-specific chrome, direct internal state (no ACP JSON serialization per update).
- First-class session mgmt (`sessions` cmd, resume/continue/restore-code, share, trace).
- Skills as first-class extensible system (user + bundled, invocable via / or auto by Grok).
- Memory as explicit feature (--experimental-memory + memory cmd).
- Advanced orchestration exposed directly (--best-of-n N, --check, --agents JSON for custom personas/subagents, --permission-mode plan, effort levels, sandbox).
- Background monitors / PTYs feel native to the TUI (direct, full-width, controls).
- Plan mode + approval is core workflow (skills like "implement" run reviewer loops until zero issues).
- Persistent: worktrees, sessions DB, cross-session memory files tied to grok install.

**Zed ACP Path Strengths (Visual Desktop Peer)**:
- Native GPUI: embedded diffs, lazy TerminalViews, activity bar summaries, persona badges, CircularProgress, hover actions — all efficient (O(1) collapsed per Auditor).
- In-editor context ( @ mentions, project files, Zed keybindings, multi-pane).
- Unified across agents + rich history in agent panel.
- Zero extra install for visuals (binary provides brains).
- Low-overhead discovery/caching.

**Behavioral/UX Diffs Causing Friction**:
- TUI: "I am the agent environment" (full focus, alt-screen).
- Zed: "The agent lives in my editor" (side panel + activity bar). Great for some, jarring switch for others.
- State: completely separate worlds. Starting work in TUI (quick `grok -p "..."`) then wanting rich visual review/continue in Zed = manual context copy or restart.
- Investment lock-in: skills written for TUI not usable in Zed native; sessions not portable.

---

## Highest-Impact Friction Points for Seamless Movement (Ruthless, User-Work-Focused)

1. **Skills Silo (Critical Path - Highest Leverage)**: Users' real power in Grok Build comes from the skill ecosystem (check/best-of-n/implement/review/pr-babysit + custom). TUI users create/edit in ~/.grok/skills/. When they open Zed + pick "grok" ACP, binary *does* use them for that session. But any Zed-native Grok work (or future native agent) or desire for unified skill catalog across "Zed agent" vs "grok agent" fails. Switching = duplication or loss of investment. Violates co-equal rule directly.

2. **Sessions / Continuity / History (Critical Path)**: Ongoing Grok Build projects live in TUI sessions (full traces, commits, worktrees). No way to "open this session ID in Zed's visual Grok thread" or "import this grok session's history + diffs". `grok -r` has no Zed counterpart. Users cannot fluidly move mid-project. High day-to-day pain.

3. **Memory & Cross-Session Persistent State**: Experimental memory + learned patterns from past reviews/worktrees not portable. Zed context (CLAUDE.md etc.) is separate. Users re-teach the agent when switching.

4. **Persona Consistency + Advanced Orchestration/Approval**: Hardcoded 5 personas vs Grok's dynamic (from skills/agents JSON, implementer/reviewer scaling, best-of-n). Plan approval UX (TUI loops vs Zed generic banners) + "plan mode discipline" may drift. Subagent hierarchy visual good in Zed but command surface for --agents/--best-of-n less direct.

5. **Monitor/Background Task + Permission UX**: Excellent gated visuals in Zed (better than raw TUI text for some), but context switch cost high (TUI full-screen PTY monitor vs click-to-expand in activity bar). Permission mode selection exists but "Grok plan approval flow" not specially polished.

6. **Command Surface / Onboarding / Status**: Not all TUI power flags have obvious Zed equivalents for the grok agent. No "Grok co-equal health" indicator (skills shared? last session imported?). Discovery great, but install hinting / status could be tighter.

Lower: auth (shared via ~/.grok), MCP (binary), raw performance (ACP hop is the known tradeoff, gated well).

---

## Prioritized Short List of *Minimal* Integrations / Enhancements (Make Switching Feel Natural)

Focus on **bridging**, not new features. All respect: existing files only (per CLAUDE.md), TDD, efficiency (caching, no repeated scans/syscalls, O(1) paths), Linux-first, real `todo!` + clippy exact, full error prop, reference Efficiency Auditor (collapsed cheap), update AGENTS.md table/log/risk. Tackle **before** deep native.

**Prio 1 — Must Precede Deep Native (Directly Enables Co-Equal per Rule)**:

- **G-15: Skills Directory Bridging (`.grok/skills` + bundled ↔ `.agents/skills`)** (P1, pre-P4)
  - Minimal: In `crates/agent_skills/agent_skills.rs`, add `grok_user_skills_dir()` / `grok_bundled_skills_dir()` (or multi-root vec). Update `global_skills_dir()` callers + `load_skills_from_directory` / scan/watch logic to union both locations (Grok source variant with lower or configurable precedence; user .agents still wins for Zed-native).
  - For ACP "grok" path: binary continues to own its skills (no change); bridging primarily benefits native Zed agent + "grok model" users who want TUI skills available.
  - Efficient: reuse existing SKILL_IO_CONCURRENCY, watcher (non-recursive), MAX sizes. Cache roots. TDD: multi-root load precedence, no double-count, watch fires on either.
  - Impact: Users' TUI skill investments (check, best-of-n, implement/review loops) "just work" when using Zed too. Huge for continuity.
  - Code locations: agent_skills.rs (dirs + load), agent.rs (init + load_global + project), thread.rs (prompt injection). No new crates.

- **G-16: Grok Session Resume / Import / Continuity (TUI <-> Zed)** (P1, pre-P4)
  - Minimal: In agent panel / `NewExternalAgentThread` (for agent=="grok"), add optional `resume_session_id: Option<String>`. Wire to ACP `resume_session` (if grok binary ACP server supports via protocol or flag).
  - Surface: "Grok Sessions" list (shell `grok sessions list --json --cwd .` or parse ~/.grok/sessions/*/session_search.sqlite lightly + cache; show recent for CWD). "Resume in Visual" action that opens grok ACP thread with that ID.
  - Also: on ACP thread creation for grok, expose current session ID in UI / copy button so user can `grok -r <that-id>` in terminal to switch back.
  - TDD + perf: cache CLI output or sqlite reads; no work on non-grok agents. Error surfacing for invalid IDs.
  - Impact: Users can start in TUI (`grok ...`), then "continue visually in Zed", or vice-versa. Eliminates restart friction.

**Prio 2 — High for Parity**:

- **G-17: Memory & Persistent State Bridging** (P2)
  - Detect ~/.grok memory artifacts (or env), surface as attachable "Grok Memory" context or auto-include for grok ACP/native. Basic importer for key session metadata.
  - Wire grok config (sandbox, etc.) respect in ACP env.

- **G-18: Persona Extensibility + Grok Plan Approval Polish** (P2)
  - Extend `AgentPersona` (acp_thread.rs) to support dynamic/from-grok-config or fuller enum matching TUI skills (add "Architect", "Verifier" etc.).
  - In thread_view.rs: dedicated cheap "Grok Plan Proposed — Approve?" banner + action for plan-mode sessions (reuses existing disclosure/awaiting + PlanEntry patterns, gated).

- **G-19: Grok Command Surface + Co-Equal Status** (P2)
  - Expose common grok flags (--best-of-n, --check, effort) as first-class in grok agent settings UI + favorite configs.
  - Add small "Co-Equal with TUI" status (in selector or onboarding): indicators for "Skills bridged", "Sessions resumable", "Memory shared". Link to docs.

**Verification for all**: Extend Real Binary Verification (use grok CLI + ACP harness + new E2E test rec in verification report). Run on this machine's ~/.grok. After: `./script/clippy -p agent_skills -p agent -p agent_ui -p acp_thread -p project -p agent_servers`.

These are *minimal* (leverage ACP + existing renderers + binary for heavy lifting) yet deliver "switching feels natural".

---

## Concrete Recommendations for Phase 3 Roadmap & AGENTS.md Updates

**Reference Co-equal Zed / Grok Build Priority (AGENTS.md 383)**: "future agents must prioritize: Work that reduces friction for users moving their Grok Build work between the terminal TUI and Zed (especially skills bridging, session continuity/import/restore, shared personas, plan discipline, monitor behavior, and memory). ... When there is a choice ... lean toward the latter [bridging]. ... long-term goal is for Zed to stand as an equal (or superior) environment..."

**Key Recommendation**: **P3 Bridging Layer must be completed *before* any deep native P4 implementation begins.**

- Rationale (ruthless): Implementing full native Grok (P4-0..P4-4) *without* first making the current ACP path co-equal via bridging would leave users with *three* fragmented environments (TUI, ACP-grok, native-grok). This *increases* friction. Per rule, bridging is higher-leverage for "users continuing real Grok Build work inside Zed". ACP path (with latest binary) stays the fidelity anchor; bridging makes it first-class peer *immediately*; native then becomes the efficiency/visual upgrade that inherits the shared state layer (perfect skills sharing, session import built-in, etc.).

**Revised Roadmap Structure (update "Phase 3 Native Grok Roadmap" section)**:
- **P0-P2 (current)**: Discovery, visuals (G-04/05/06 done/verified), Mac/Win, reasoning.
- **P3: Co-Equal Bridging (New, G-15-G-19, 2-4 weeks total, parallelizable slices)**: The short list above. All changes in *existing* files. TDD + efficiency audits mandatory. Use/extend verification harness + real binary on Linux. Deliverables: skills multi-root, session resume UI+wire, persona/plan polish, status. Update docs (external-agents.md + this map). Makes ACP "grok" a drop-in co-equal peer.
- **P4: Full Native (G-12, now explicitly post-P3)**: Update P4-0..P4-4 descriptions to *depend on* P3 artifacts (shared skill loader, session import helpers, persona registry, approval state). Native gets "perfect" sharing + lower overhead than ACP. Hybrid flag + capture baseline still required. Re-audit all risks with bridging in place. `todo!` locations remain valid but now assume P3 foundations.

**Backlog Table Updates (add/prioritize, honest status)**:
- Elevate G-10 (skills) and G-11 (sessions) to P1 "pre-native co-equal".
- Add:
  | G-15 | Skills bridging (multi-root ~/.grok/skills + .agents/skills for co-equal use) | P1 (pre-P4) | Not Started | Efficient union load/watch; source tagging + precedence; TDD + Auditor O(1) paths; benefits native + unifies for ACP grok users. | agent_skills, agent, thread | 2026-05-19 |
  | G-16 | Grok-specific session resume/import (TUI IDs <-> Zed ACP threads) | P1 (pre-P4) | Not Started | resume_session wire + UI list (grok CLI json or sqlite cache) + ID copy for roundtrip; cached/perf; TDD. | acp_thread, agent_ui, agent_servers | 2026-05-19 |
  | G-17 | Memory + persistent state bridging (grok memory/worktrees <-> Zed) | P2 | Not Started | Detect/include/import; respect performance. | agent + acp | 2026-05-19 |
  | G-18 | Persona extensibility + Grok plan approval banner/polish | P2 | Not Started | Dynamic personas + cheap approval UI (reuse patterns). | acp_thread, thread_view | 2026-05-19 |
  | G-19 | Grok command surface + "Co-Equal Status" indicator | P2 | Not Started | More config options + health pill in selector. | agent_ui + custom | 2026-05-19 |
- Mark G-12 (native) as "Blocked on P3 Bridging" + note "per Co-equal rule and Friction Auditor 2026-05-19".

**Other AGENTS.md Updates**:
- In "Current known limitations / future work": replace generic notes with "See new 'Seamless Integration Friction Map' section + GROK_SEAMLESS_FRICTION_MAP.md for prioritized bridging work (G-15+). Skills/session gaps are now P1 pre-native."
- Add this entire report (or link) as new subsection after "Co-equal Zed / Grok Build Priority" and before "Grok Model Parameters": "### Seamless Integration Friction Map & Bridging Recommendations (Friction Auditor, 2026-05-19) [full content or summary + ref to GROK_SEAMLESS_FRICTION_MAP.md]".
- Implementation Log: append "**2026-05-19 (Seamless Integration Friction Auditor - explore/plan)**: Completed  deep read-only exploration (code + real TUI binary + ~/.grok state + verification report). Produced this friction map, prioritized G-15..G-19 bridging list, and mandated P3 Bridging prerequisite before P4 native (citing Co-equal rule). Updated backlog/roadmap/Phase 3 section + added this map file. All per rules (no code changes, honest, existing-files preference for future impl)."
- Performance Risk Register: add "Bridging risks: sqlite/CLI parse for sessions must be strictly cached + off hot paths (model after discovery OnceLock); multi-root skill scans must not increase fs pressure (use existing concurrency bound)."
- "How to continue": add "7. Before touching P4 native code, ensure G-15/G-16 (skills+session bridging) are at least prototyped so native inherits co-equal state layer."
- Swarm discipline: reinforce that bridging slices follow same parallel subagent model + Auditor register.

**Deliverable Files**:
- This GROK_SEAMLESS_FRICTION_MAP.md (primary clear actionable plan document).
- AGENTS.md updated with the above references, table, Phase 3 rewrite, log entry (synthesized in place).

**Next Swarm Steps (Autonomous, per Discipline)**: Launch specialized subagents for G-15 (Skills Bridging Specialist, in agent_skills/agent), G-16 (Session Resume Specialist), in parallel with Documentation Maintainer for AGENTS.md edit + testability. Use real binary verification extended. Do not start P4 scaffolding until P3 slices approved in table.

This map is ruthless: only what actually matters for continuing real work. Visuals (G-04+) are table stakes; state bridging is what makes Zed co-equal.

**End of Friction Auditor Report**. Update AGENTS.md and reference this file in all future Grok sessions.
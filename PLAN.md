Zed + Grok Build Integration Plan

Primary Goal
Zed must deliver a complete, fully independent native Rust + GPUI re-implementation of Grok Build.

This means:
- A full native Grok Build, rewritten in Rust, fully compatible with Grok Build's sqlite formats, delivered through native GPUI visuals inside Zed.
- All Grok Build functionality implemented with full Rust replacements for every bridge to the external binary, plus full test coverage.
- This explicitly includes:
  - A non-bridged, fully independent implementation covering full sub-agent tracking with personas and capability modes, rich native GPUI visuals (no TUI), and all skills and personas.
  - Modifications to Zed's ACP and agent layer so that IDE-based diagnostics (errors and warnings from rust-analyzer and other language servers) become the primary, always-available, non-blocking, cross-language context for the agent. The agent must be instructed to rely on Zed-sourced lints first instead of forcing local cargo or shell builds.
  - Zed provides real system (desktop + in-Zed) notifications when all autonomous work finishes.

The external grok binary and its ACP interface are now legacy compatibility only. They may be used optionally by users who still run the standalone TUI, but they are no longer the source of truth or a required path for new work.

Authentication direction
- Zed Cloud browser sign-in, telemetry, and collab upsell UI are stripped in Surmount (see SURMOUNT.md § Upstream services stripped).
- **Goal:** replicate Grok Build authentication inside Zed (xAI/API keys, `~/.grok` CLI login state, env-based provider config) as the primary agent auth story — not zed.dev OAuth.
- Not implemented yet; current path is local agents + per-provider API keys until native Grok auth UX lands.

Guiding Constraints (non-negotiable)
- Full native replacement is the only primary path. Incremental bridging is rejected.
- Linux-first for *platform UX* porting (cold start, keymaps, notifications), then Mac/Windows — not for agent availability; agents and agent-panel workflows stay cross-platform (see AGENTS.md § No OS–agent coupling).
- Native Rust + GPUI only. Efficiency and latency are first-class requirements.
- TDD with production-quality test coverage.
- All changes follow existing CLAUDE.md discipline (edit existing files, full words, proper error handling, no creative low-level additions).
- Token efficiency is paramount. Avoid low-value comments, organizational summaries, dated history, and verbose process notes in code and living documents. Rely on cargo fmt, cargo clippy, and rust-analyzer for enforceable style where possible.

Long-term Branch Reality
The surmount branch is a long-running high-expansion fork that will face substantial merge conflicts with upstream Zed. We will make full use of tooling and automation to resolve them. All decisions must be documented clearly and consistently so they create no extra contradictions during future merges.

Zero interest in legacy unperformant code
The old SQLite-backed key-value storage path (legacy name: KVP) for agent panel and persistent task surface state has known concurrency and deserialization problems. The only supported path is the new heed3 + rkyv zerocopy MVCC LMDB backend. The legacy path is to be terminated, not carried forward. All documentation and decisions must stay clear and consistent to minimize future merge conflict surface.

Contradiction handling
If contradictory information appears in documentation, code, or decisions, it must be explicitly tracked in todos and living docs and escalated for human resolution. Agents must not silently resolve contradictions.

Current Phase
Active work on full native replacement. The external bridged path remains available as high-fidelity compatibility for users who continue using the standalone TUI.

Major Milestones (high-level)
- The approvals, plans, monitors, and memory surface is reusable across docks and panels with unified risk classification.
- Skills multi-root bridging is complete for co-equal use between the TUI and Zed.
- Session resume/roundtrip and memory bridging are scaffolded.
- Native Grok profile path with profile guard and prompt injection is in place.

Approvals, Plans, Monitors, and Memory Surface (Reusable Component)
The surface is now a first-class native GPUI primitive. Any dock or panel can own it, drive it against any thread (bridged or native), collect categorized items, classify risk, toggle sections, and wire actions without duplicating logic.

See docs/src/ai/external-agents.md for copy-paste-ready patterns (state ownership, collectors, toggles, row helpers, and action builders).

Acceleration Plan
- Run at high parallelism with focused specialists when independent work exists.
- Maintain clear separation: AGENTS.md for rules + detailed backlog; this file for strategic overview.
- Complete bridging items before expanding deep native work.
- Regular efficiency reviews.
- Keep swarm roles aligned with user preference for planning, testing, UI, bridging, and documentation.

Risks & Mitigations
- Headless environment limits live GUI testing, mitigate with capture harness and future display-capable verification.
- Token and orchestration fidelity for native, mitigate with real captured schemas.
- Performance regression on hot paths, mitigate with gated designs and collapsed paths.
- User confusion from multiple experiences, mitigate with clear labeling ("Grok (Bridged)" vs "Grok (Native)").

Success Criteria for Co-equal Experience
- A user who has invested in Grok Build skills, sessions, plans, and monitors can continue that work inside Zed with no manual duplication and with rich native visuals.
- Switching between the standalone TUI and Zed feels low-friction.
- The bridged path in Zed is already excellent for daily use.
- Capture artifacts and todo markers exist so native implementation starts with high fidelity.

PromptStore / LMDB Unification (high-level)
PromptStore is the last production user of the old heed 0.21 crate. The migration to heed3 + rkyv zero-copy is in progress to simplify the dependency graph and improve read-path performance on the metadata that powers agent skills, rules, and memory fragments.

High-level approach:
- Define rkyv-archivable forms for the stored types.
- Add dual read/write paths during transition while preserving fail-open behavior and the public API.
- Remove the old heed dependency only at the end.
- Goal: measurable reduction in allocations and latency on hot list and search paths.

Native Grok Build Completion Plan (high-level)
After bridging work is closed, remaining work focuses on making the native path fully independent and superior:

Core Native Loop
Full native implementation of run loop, tool dispatch, sub-agent and persona propagation, background monitor scheduler, and plan discipline that matches or exceeds captured fidelity from the external binary. Native background task manager feeding the existing lazy terminal and monitor surface.

IDE Diagnostics as First-Class Context
Surface language server diagnostics (rust-analyzer and others) directly into the agent prompt and context on every turn. This is non-blocking and works across languages. Prefer Zed-sourced lints over shell-based build commands.

Completion Notifications and Autonomy Discipline
Real system (desktop + in-Zed) notifications when a native thread reaches true completion. Strengthen enforcement so the model cannot silently stop while items remain in the approvals, plans, monitors, and memory surface.

Polish, Testing, Migration and Documentation
Full test coverage (unit + integration + harness against captured artifacts). Low-friction session import/export that preserves full state. Clear user-facing distinction between bridged and native paths. Performance validation that native is noticeably lighter.

How to Continue This Work
1. Read this file and the Grok section of AGENTS.md.
2. Run the project's standard clippy command on touched packages.
3. Launch focused specialists on independent slices from the backlog when capacity exists.
4. Update both this file (strategic view) and AGENTS.md (detailed table + risks) after every significant deliverable.
5. Respect the bridging gate before expanding deep native work.
6. Keep token efficiency and prevention of low-value comments and history bloat as ongoing discipline.

Last Updated: Strategic core only (detailed per-slice execution history moved to git + todos for context efficiency).

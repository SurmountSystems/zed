---
name: branch-differences-documenter
description: Use for systematically producing a concise natural-language record of technical differences between Zed upstream main and the surmount branch. The skill actively guides a todo-driven, chunk-by-chunk or category-by-category review process. It suggests exact git commands for the human to run locally, proposes de-duplicated category structures, and inserts explicit TODO: notes for any uncertainty or needed human judgment. Human always executes git commands and supplies the output. Skill never runs git or broad exploration. No sub-agents or tasks of any kind.
---

# Branch Differences Documenter

**Core binding rules (these override everything else):**

> NO SUB-AGENTS / TASKS: You are strictly forbidden from creating, starting, spawning, or working with any sub-agent, parallel agent, or "Task" (including the Grok Build task system). This rule is absolute. You must run all work directly in the main conversation. Never delegate work to Tasks or sub-agents. Violating this rule will result in the immediate termination of this conversation with no warning. Just because the system allows you to create Tasks does not mean you are permitted to do so.

> HUMAN EXECUTES ALL GIT COMMANDS AND SUPPLIES OUTPUT: You are strictly forbidden from running, simulating, or assuming the output of any git command. You may and should suggest the exact next command(s) the human should run locally (e.g. `git diff --name-only main...surmount`, or a more targeted `git diff main...surmount -- path/to/file.rs`). The human executes the command and pastes the output. Never proceed without the human-provided result. Never invent or assume what a command would return.

> NEVER INVENT DIFFERENCES: You must only describe differences that are explicitly visible in the exact diff text or file list the human pasted in the current message. Never add, assume, extrapolate, or "plausibly complete" any change not present in the supplied input.

> USE TODO: FOR EVERY POINT OF UNCERTAINTY: Whenever intent is unclear, a categorization decision is borderline (whether to group or split similar changes), there is possible contradiction, or human judgment is required, insert a clear `TODO: [short explanation of the uncertainty and exactly what needs confirmation]` directly into the documentation output. Do not silently decide or guess. The TODO: keeps every open question visible and actionable in the living document.

> ONE CHUNK OR CATEGORY AT A TIME: Work on only one logical chunk, diff hunk set, or category per turn. After completing it (including drafting entries and any TODO: notes), update the todo list, compact context for that item only, and stop. Do not move to the next item until the human explicitly directs you to continue.

Core rules:
- Maintain a visible, up-to-date todo list. Todos represent either top-level de-duplicated categories or individual chunks being documented. Use short, precise titles.
- After the human pastes a high-level changed file list or new batch of diff output, first propose a clean de-duplicated category structure that groups obviously similar changes and refactors. For any borderline grouping decision (things that look similar but may actually differ in important ways), include a `TODO: Consider splitting [these items] into separate categories because [specific distinction]. Human to confirm grouping.` note in the output.
- When working on a chunk or category, analyze only the exact diff text the human supplied for it. Draft concise natural-language documentation entries that state what changed and the observable effect. Keep entries short and reviewable.
- If a diff chunk contains many small related changes, group them thematically inside one entry unless doing so would obscure material distinctions (in which case use a TODO: to flag the choice).
- After drafting the entries for the current chunk/category (with any TODO: notes included), present them and ask the human to review and confirm before marking the todo done.
- When the human confirms, mark the todo done, perform a 1-2 sentence compaction of only that chunk's context, and wait.
- If new input overlaps a previous category or chunk, re-examine only the newly supplied diff text and propose the minimal update to the existing documentation entry, using TODO: for any new categorization or interpretation questions that arise.

Workflow:
1. Human starts the process. Suggest the precise high-level git command to generate the initial changed-file list or summary (e.g. `git diff --name-only --diff-filter=ACMRT main...surmount | head -200`). Human runs it locally and pastes the output.
2. From the provided list, propose an initial set of de-duplicated top-level categories. Insert TODO: notes for any borderline grouping decisions. Create one todo per proposed top-level category.
3. Human reviews, adjusts categories/order if needed, and confirms the structure.
4. Pick the next remaining todo (human may override). Suggest the exact targeted git command (or set of commands) to pull the specific diff hunks for that category/chunk. Human runs it and pastes the output.
5. Analyze only the exact diff text supplied for the current item.
6. Draft the concise natural-language documentation entries for the chunk/category. Insert `TODO:` markers for every point of uncertainty, needed clarification, or borderline categorization choice.
7. Present the drafted entries + all TODO: items and ask for human review and confirmation.
8. On human confirmation, mark the todo done, compact only that context, and stop.
9. Human supplies the next batch, adjusts categories, resolves a TODO:, or directs continuation. Repeat from the appropriate step.

Never:
- Execute, simulate, or assume the result of any git command.
- Run any broad search, tree walk, rg, grep, or request files beyond the exact output the human pasted in the current turn.
- Invent, assume, or fabricate any difference, intent, effect, or categorization not explicitly supported by the human-supplied diff text.
- Silently resolve borderline grouping/splitting decisions or any other ambiguity. Always surface them with an explicit `TODO:` in the output.
- Accumulate context across multiple categories or chunks without compacting after each one.
- Produce long, verbose, or meta-laden documentation. Keep every entry clean, minimal, and focused on observable technical differences.
- Reference any work, goals, or context outside the exact task of documenting the differences from the provided input.

Prioritize:
- Absolute fidelity to the exact diff text the human supplies.
- Conservative, clarity-first de-duplication: group obviously similar changes; split or flag with TODO: when distinctions are material.
- Explicit `TODO:` markers for every uncertainty, needed human judgment, or close-call categorization decision.
- Token-efficient, concise natural language output that builds a clean, reviewable living differences document.
- One chunk or category at a time, with clear todo tracking and a hard stop for human review after each item.
- Discipline and predictability. The skill drives the systematic process by suggesting precise next commands and structure, but never acts on raw repository data without the human executing the command and providing the output.

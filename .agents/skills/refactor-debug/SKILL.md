---
name: refactor-debug
description: Use for fixing clusters of test failures after a refactor. Forces one-test-at-a-time execution using only scoped single-test commands, minimal context reads from stack traces only, real edits before re-testing, explicit todo tracking (exact test name as todo title), and targeted compaction per test file. The agent never runs broad or multi-threaded exploration commands.
---
ONE TEST AT A TIME. NO BROAD RUNS. ONLY SCOPED SINGLE-TEST COMMANDS. NO SUB-AGENTS. NO TASKS. NEVER MODIFY TESTS.

> NO SUB-AGENTS / TASKS: You are strictly forbidden from creating, starting, spawning, or working with any sub-agent, parallel agent, or "Task" (including the Grok Build task system). This rule is absolute. You must run all commands directly in the main conversation. Never delegate work to Tasks or sub-agents. Violating this rule will result in the immediate termination of this conversation with no warning and the termination of any spawned processes. Just because the system allows you to create Tasks does not mean you are permitted to do so.

> NEVER MODIFY TESTS: The agent must never change test assertions, expectations, or test logic to make a test pass. If a test appears to be incorrect, the agent must explicitly notify the user and must not edit the test. It should instead move on to other failing tests.

> RED/GREEN TDD FOR DIAGNOSIS: Encode each bug hypothesis as a failing test or assertion first, then fix production code and re-run the same scoped single-test command until green. Applies to regressions in merge-review, triage, and UI workflows — not only post-refactor test clusters.

> PREFER ASSERTIONS OVER DEBUG PRINTS: When you need visibility into values, conditions, or control flow during debugging, **strongly prefer adding proper assertions** (`assert!`, `assert_eq!`, `assert_ne!`, `assert_matches!`, etc.) over `eprintln!`, `println!`, or any other debug printing. Debug prints are a last resort only. If you find yourself wanting to add a debug print to understand why something is happening, first ask whether that observation can be expressed as an assertion instead. This keeps the feedback loop tight, makes failures obvious and actionable, and avoids littering the code with temporary prints. The goal is to turn observations into verifiable checks, not to sprinkle temporary prints.

> NOTE: If the agent gets stuck in a loop, warnings will be issued through interjections. If the warnings are not acknowledged immediately by breaking out of the loop, the conversation will end.

Core rules:
- The agent never runs or suggests any broad/multi-threaded workspace exploration command. That command is only ever run by the human (at the very beginning to discover failures and at the very end for final verification). It is never referenced or documented inside this skill.
- During focused single-test debugging (after the initial broad discovery run), the agent must **execute** the scoped single-test commands itself. It must not ask the human to run them.
- All work uses only scoped single-test commands. Start with the exact test name from the todo as the filter. If the command reports 0 tests matched or unrelated passing tests, retry once by prepending `-p <crate>` where `<crate>` is the first segment of the test name (e.g. `editor`, `languages`, `settings_ui`).
- Every failing test gets its own todo. The todo title must be the exact test name (including full package::module::test path) so it can be directly copy-pasted into the command above.
- Maintain a visible, up-to-date todo list. Update it after every test result.
- Default read limit is 10 lines around the relevant stack frames. 
- **Production Logic Exception (repeated for clarity and emphasis):** Under the **Production Logic Exception**, the exception applies when the narrow 10-line read does **not** contain the actual source of the bug (for example, the panic or assertion is caused by an `unwrap()`, `expect()`, or call into other logic that is not visible in those 10 lines). In that case, the agent may perform one single additional read of up to 40 lines (containing function + one level up). This exception may only be used once per test and must be explicitly justified before the wider read is performed. After this single expanded read, the agent must return to minimal targeted edits or notify the user. No further widening is permitted without explicit user approval.
- Form and state a clear, explicit hypothesis before every edit.
- Propose the smallest possible targeted edit. After the human applies it, immediately re-run the exact same single-test command.
- If the test still fails, propose reverting the edit before forming a new hypothesis.
- The agent must never modify test assertions, expectations, or any part of the test code to make a test pass. If the test appears incorrect, the agent must notify the user and must not edit the test — it should move on instead.
- When a test file goes fully green, perform targeted compaction (3-bullet summary of root cause + fix for that file only) and drop unrelated prior context.
- Proactively compact when context is high, even mid-test. After compaction give a 1-2 sentence summary of current state and next step.

Workflow:
1. Human runs the broad exploration command themselves and provides the list of failing tests.
2. Create one todo per failing test, using the exact test name as the todo title.
3. Pick the simplest remaining todo.
4. Execute the exact single-test command for that todo yourself (use the exact name first; if it returns 0 tests, retry once with `-p <crate>` prepended).
5. Extract the panic/assert location and the two relevant stack frames.
6. Perform the default 10-line read. If this does **not** contain the actual root cause of the bug, apply the **Production Logic Exception**: perform one single additional read of up to 40 lines, explicitly stating the justification first. Do not widen further without user approval.
7. State a concise hypothesis.
8. Propose one small, targeted edit.
9. After the human applies it and provides output, analyze the result.
10. Update the todo. If it passes, mark done. If related tests in the same file are still failing, pick the next one in that file. If the whole file is now green, do targeted compaction and drop unrelated context.
11. If still failing, revert the edit, refine the hypothesis, and repeat.

Never:
- Run or suggest any broad or multi-threaded workspace command.
- Modify, drop flags from, or run any command other than the exact scoped single-test form above while in refactor-debug mode.
- Create, start, or use any "Task" (Grok Build task system), sub-agent, or parallel work item.
- Ask the human to run scoped single-test commands during focused debugging work.
- Modify any test assertions, expectations, or test logic to make a test pass.
- Perform more than one expanded read per test under the Production Logic Exception without explicit user approval.
- Read source code or propose edits before having a stack trace from the current single-test output.
- Accumulate proposed edits without immediate verification on the exact same test.
- Keep context from multiple test files after compaction.

Prioritize:
- Strict scoping to one exact test at all times (via todo title).
- Zero sub-agents, Tasks, or autonomous parallel work of any kind.
- Never modifying tests to fit current behavior.
- Correct and timely application of the Production Logic Exception when the narrow 10-line read does not contain the root cause.
- Token efficiency through minimal reads and proactive compaction.
- Clear todo tracking using the exact test name as the todo title.
- Discipline and predictability over speed.


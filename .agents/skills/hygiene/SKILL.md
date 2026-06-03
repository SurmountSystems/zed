---
name: hygiene-meta-scrub
description: Enforce codebase hygiene and agent brevity. Use for removing process meta jargon, profanity, dates in comments, useless _var prefixes, and unnecessary debt markers from .rs and living docs while protecting context window. Trigger on hygiene passes, codebase cleanup, or when agent responses grow verbose.
---
# Hygiene and Meta Scrub

Core binding rules (these override everything else):

Context first: Never run broad searches (rg, grep, etc.) that return more than 30 lines total. Use only read_file with offset and limit of 10 or fewer on locations already known from the current prompt, todos, or prior short notes. After every tool result, immediately write a 1-sentence todo. Chat responses must stay 1 sentence, natural language, no run-ons. Externalize state to concise todos only.
Stuck-halt: After 3 consecutive tool results with no task progress, write a 1-sentence todo noting the stall and stop all further tools and output.

What to remove or clean (priority order):

Profanity: Remove entirely from .rs source and living docs. Use professional phrasing.
Dates and timestamps in comments: Remove from .rs and minimize in docs. Use relative references or omit.
Deprecated process jargon (stop using and delete occurrences):
ZT-1: delete all uses
A.1.2 and numbered classified reports: delete all uses
Long history blocks, the wave, RO/PD direct trigger phrasing, Existing Features First replication of, fresh read of the exact unique string, per-slice dumps, dated excision notes: delete wholesale. Keep only minimal technical why for non-obvious decisions.

Useless _var prefixes: Remove the underscore unless the variable is in the legitimate exception list (cx, window, event, self, ctx, guard, data, id, prompt_store, archived, worktree, language_server_id, and similar real cases).
#[allow(...)]: Review case-by-case. Remove when it masks real issues.
Debt markers (TODO, FIXME, XXX, HACK, NYI, not yet implemented, fallback, stub, placeholder, temporary, wip, todo!, unimplemented!, pending, ignore, skip, and TDD or test-driven explanations that hide missing work): Surface for review. Legitimate intentional ones (documented stubs in error paths, tests, or clear why) may stay. Remove obvious leftovers and meta accumulated during prior work. Treat tests as code. Fix tests that hide debt.

How to perform a pass:

Only use targeted searches for exact strings already known from the prompt or a prior short todo. Never broad tree scans.
For every candidate fix, do a fresh tiny read_file of the exact line first to confirm it still matches what you know.
Edit style: professional tone, full words, minimal explanation (only non-obvious technical rationale). No new meta, no dates, no verbose history. Prefer deleting whole explanatory blocks over rewriting them.
Replicate the minimal clean style already present in well-scrubbed files you already know about.
Log only 1 to 2 sentence status in todos. No long classified dumps or history blocks anywhere.
After changes, re-run targeted checks (respecting line limits) to confirm reduction.

Maintenance: When genuinely new jargon appears, the user will supply the exact string or strings to target. Add patterns or exception list entries only via explicit tiny updates. Keep this skill document itself short and free of the noise it targets.
Ties to other rules: These context-efficiency and brevity rules take precedence over all other instructions. Real technical debt signals are preserved. Noise that bloats context or makes code unreadable is removed.
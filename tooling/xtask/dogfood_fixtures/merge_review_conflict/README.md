# Merge-review conflict dogfood fixture

This directory documents the **minimal conflicted git tree** used by

```bash
cargo xtask dogfood merge-review --with-conflict
```

## How the fixture is built (xtask, not shell)

`tooling/xtask/src/tasks/dogfood.rs` builds an equivalent tree under a **tempfile** at run time:

1. `git init -b main` + identity config (`commit.gpgsign=false`)
2. Commit `conflict.txt` = `base line` + `SURMOUNT.md` + minimal `surmount-merge-categories.toml`
3. Pin `refs/remotes/origin/main` at base (Start defaults to `origin/main`)
4. Branch `theirs`: commit `theirs line`
5. Back on `main`: commit `ours line`
6. `git merge --no-ff theirs` → leaves **`MERGE_HEAD`** and unmerged `conflict.txt`

Default `merge-review` (no flag) never requires this fixture or live Surmount `MERGE_HEAD`.

## Optional static copy

You may recreate the same layout under this folder for manual inhabit. Dogfood does **not** require a checked-in `.git` here — the runner prefers tempfile so CI stays clean.

## Decision chrome gate

When the fixture is active, the runner soft-gates post-Start outline for **conflict-specific** labels (`Use Both` / `Resolve with Agent` / `Summarize this conflict` / Discuss-rail). Always-on chrome such as `Review Diff` alone does **not** count. Dynamic `Use …` branch buttons are logged optionally. Missing chrome logs `[conflict] skip:` and does not fail the default adventure.

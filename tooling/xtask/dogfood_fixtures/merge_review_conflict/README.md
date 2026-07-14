# Merge-review conflict dogfood fixture

This directory documents the **minimal conflicted git tree** used by

```bash
cargo xtask dogfood merge-review --with-conflict
```

## PreMerge vs MergeInProgress (why clean Surmount has no conflict UI)

| Mode | Git | Rail |
|------|-----|------|
| **PreMerge** | No `MERGE_HEAD` (clean Surmount tree) | Prepare · Preview merge · Next file |
| **MergeInProgress** | `MERGE_HEAD` present | Review Diff / Summarize this conflict · Use HEAD/theirs/Both · Resolve with Agent |

Default `merge-review` on a clean Surmount root is **PreMerge** — conflict chrome is not a bug, it is absent by design. Use `--with-conflict` (or a real `git merge`) for the workshop.

## How the fixture is built (xtask, not shell)

`tooling/xtask/src/tasks/dogfood.rs` builds an equivalent tree under a **tempfile** at run time:

1. `git init -b main` + identity config (`commit.gpgsign=false`)
2. Commit `conflict.txt` = `base line` + `SURMOUNT.md` + minimal `surmount-merge-categories.toml`
3. Pin `refs/remotes/origin/main` at base (Start defaults to `origin/main`)
4. Branch `theirs`: commit `theirs line`
5. Back on `main`: commit `ours line`
6. `git merge --no-ff theirs` → leaves **`MERGE_HEAD`** and unmerged `conflict.txt`
7. Sibling bare clone `origin.git` (outside the worktree) + `git remote add origin …` so **`git fetch origin` succeeds offline** without dirtying Branch Diff

Default `merge-review` (no flag) never requires this fixture or live Surmount `MERGE_HEAD`.

## What `--with-conflict` gates

1. **Decision chrome** (soft): conflict-specific labels (`Use Both` / `Resolve with Agent` / `Summarize this conflict` / Discuss-rail). `Review Diff` alone does **not** count.
2. **Review Diff path** (soft if ACP offline): action `git::ReviewDiff` → stderr dispatch + rail **Summarizing…** when possible.

Preview/End stay on the default adventure only (not the conflict fixture path).

## Optional static copy

You may recreate the same layout under this folder for manual inhabit. Dogfood does **not** require a checked-in `.git` here — the runner prefers tempfile so CI stays clean.

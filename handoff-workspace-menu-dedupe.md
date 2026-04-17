# Handoff: split and dedupe duplicate sidebar workspace entries

## Scope

This is **only** about the project group menu in the agents sidebar (the `…` menu on a project group header) — specifically the custom rows added by `render_project_header_ellipsis_menu` in `crates/sidebar/src/sidebar.rs`.

Do not change any other UI, data model, or crate. No changes to `WorktreePaths`, `Workspace`, `ProjectGroupKey`, icons, or the `Focus` affordance.

## Current behavior

Each menu row corresponds to one retained `Workspace` in the project group. Each row renders one chunk per folder in that workspace:

- `IconName::GitWorktree`
- worktree name (`"main"` when `main_path == folder_path`, otherwise `project::linked_worktree_short_name(main_path, folder_path)`)

Relevant code:
- `crates/sidebar/src/sidebar.rs`
  - `WorkspaceMenuFolderDetail`
  - `WorkspaceMenuEntry`
  - `workspace_menu_details`
  - `workspace_menu_entries`
  - `render_project_header_ellipsis_menu` (the `custom_entry` loop)

Source of the chunks:

```rust
worktree_paths
    .ordered_pairs() // (main_worktree_path, folder_path)
    .filter_map(|(main_path, folder_path)| { ... })
```

## Problem

Two different kinds of duplication can land on one row:

1. **Within a row:** the same worktree name appears more than once because multiple folders in this workspace map to the same worktree (same `(main_path, folder_path)` result, or same short name).
2. **Across rows:** two retained workspaces in the same project group end up looking identical (or near-identical) because their folder lists produce the same worktree names.

I currently mask (1) with a `HashSet` dedupe in `workspace_menu_details`. (2) is not handled at all.

## What to do

1. Remove the `HashSet` dedupe I added in `workspace_menu_details`.
2. Implement a real dedupe at the sidebar row layer:
   - **Within a row:** collapse repeated worktree names that refer to the same worktree. Keep distinct worktrees distinct even if they happen to share a short name (disambiguate at display, e.g. with the project name, following the prior art in `crates/agent_ui/src/thread_metadata_store.rs::worktree_info_from_thread_paths`).
   - **Across rows:** if two workspace rows would render identically after the within-row rule, split them so each row is uniquely identifiable (again, use project name or other stable disambiguator rather than inventing new text).
3. Add tests in `crates/sidebar/src/sidebar_tests.rs` covering:
   - A workspace whose folders produce repeated worktree names for the same worktree → one chunk.
   - A workspace whose folders produce the same short name from different main repos → two chunks, visibly distinguished.
   - Two workspaces in one project group that would render the same → two rows, visibly distinguished.

## Constraints

- Sidebar-only. No changes outside `crates/sidebar/` except reusing existing helpers read-only.
- Don’t touch the inline `Focus` affordance, `IconName::Focus`, or `assets/icons/focus.svg`.
- Don’t change `WorktreePaths`, `Workspace::root_paths`, or `ProjectGroupKey`.
- Prefer reusing `worktree_info_from_thread_paths` (or extracting a small shared helper from it) over inventing a new disambiguation scheme.

## Acceptance

- The `HashSet` dedupe in `workspace_menu_details` is gone.
- Repeated chunks within a row only collapse when they refer to the same worktree.
- No two rows in the same project group menu render identically.
- New tests in `crates/sidebar/src/sidebar_tests.rs` cover the three cases above.
- `cargo fmt -p sidebar` and `cargo check -p sidebar` pass.

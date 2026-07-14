use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::Editor;
use git::commit::parse_git_diff_name_status;
use git_ui::project_diff::ProjectDiff;
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{
    Action, App, AsyncApp, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Render, Role, ScrollHandle, SharedString, WeakEntity, Window,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use serde::{Deserialize, Serialize};
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, Tooltip,
    WithScrollbar, prelude::*,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Item, ItemHandle, ModalView, Toast, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    dock::DockPosition,
    item::{ItemBufferKind, ItemEvent},
    notifications::{NotificationId, NotifyTaskExt},
};

use crate::agent_panel::AgentPanel;
use agent::{ResolveMergeConflictSide, resolve_merge_conflict_with_git};
use editor::actions::SendReviewToAgent;
use project::git_store::branch_diff::DiffBase;
use std::path::PathBuf;
use zed_actions::agent::OpenZedTodosSurface;
use zed_actions::surmount::{
    ConfirmMergeReviewDecisionKeepFork, ConfirmMergeReviewDecisionSynthesize,
    ConfirmMergeReviewDecisionTakeUpstream, DiscussMergeReviewConflict,
    DraftMergeReviewCommitMessage, DraftMergeReviewSection, EndMergeReview,
    InjectMergeReviewDogfoodSummary, MarkMergeReviewOpenQuestion, MergeReviewNextFile,
    OpenMergeReview, OpenMergeReviewConflictTodos, PreviewMergeReviewMerge,
    RecordMergeReviewConflictDecision, ResolveMergeReviewConflictOurs,
    ResolveMergeReviewConflictTheirs, StartMergeReview, SynthesizeMergeReviewConflict,
};

pub const MANIFEST_FILE: &str = "surmount-merge-categories.toml";
pub const SESSION_STORAGE_KEY: &[u8] = b"surmount_merge_review_session";
const CLARIFYING_NOTE_KEY: &[u8] = b"surmount_merge_review_clarifying_note";
pub(crate) const MERGE_REVIEW_READY_TOAST: &str =
    "Merge review ready — click **Preview merge** when you are ready to run `git merge`.";
pub(crate) const MERGE_REVIEW_STEP_PREPARE: &str = "Prepare";
pub(crate) const MERGE_REVIEW_STEP_REVIEW_READY: &str = "Review Diff";
pub(crate) const MERGE_REVIEW_STEP_SUMMARIZING: &str = "Summarizing…";
pub(crate) const MERGE_REVIEW_STEP_NEXT_FILE: &str = "Next file";
pub(crate) const MERGE_REVIEW_STEP_CONFLICT_RESOLVE: &str = "Resolve conflict";
pub(crate) const MERGE_REVIEW_STEP_CONFLICT_REVIEW: &str = "Summarize this conflict";
/// Shown when all conflicts are resolved but non-conflict changed files remain unreviewed.
pub(crate) const MERGE_REVIEW_STEP_CONFLICT_CLEANUP: &str =
    "Conflicts resolved — review remaining files";
pub(crate) const MERGE_REVIEW_OUT_OF_ORDER_TOAST_SUFFIX: &str =
    " — click **Next file →** to advance";
pub(crate) const MERGE_REVIEW_BRANCH_DIFF_BUTTON: &str = "Merge review";
pub(crate) const MERGE_REVIEW_END_BRANCH_DIFF_BUTTON: &str = "End merge review";
pub(crate) const MERGE_REVIEW_ENDED_TOAST: &str = "Merge review ended.";
pub const DEFAULT_UPSTREAM_REF: &str = "origin/main";
const MAX_ITEMS_PER_EXPANDED_CATEGORY: usize = 40;
const MAX_INITIAL_ACTIONABLE_ITEMS: usize = 25;

pub fn initial_actionable_visible_count(total: usize, show_all: bool) -> usize {
    if show_all {
        total
    } else {
        total.min(MAX_INITIAL_ACTIONABLE_ITEMS)
    }
}

fn element_id_for_path(prefix: &str, path: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{prefix}-{}", hasher.finish())
}

pub fn pending_action_items(session: &MergeReviewSession) -> Vec<&MergeReviewItem> {
    session
        .items
        .iter()
        .filter(|item| {
            matches!(item.verdict, ReviewVerdict::Pending)
                && matches!(
                    item.disposition,
                    ReviewDisposition::Ambiguous | ReviewDisposition::Conflict
                )
        })
        .collect()
}

pub fn category_groups(
    session: &MergeReviewSession,
) -> Vec<(String, String, Vec<MergeReviewItem>)> {
    let mut categories: HashMap<String, Vec<MergeReviewItem>> = HashMap::default();
    for item in &session.items {
        categories
            .entry(item.category_id.clone())
            .or_default()
            .push(item.clone());
    }
    let mut groups = categories
        .into_iter()
        .map(|(category_id, items)| {
            let section = items
                .first()
                .map(|item| item.surmount_section.clone())
                .unwrap_or(category_id.clone());
            (category_id, section, items)
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| left.1.cmp(&right.1));
    groups
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition {
    AutoClear,
    ForkOwned,
    Ambiguous,
    BuildConfig,
    Conflict,
    Confirmed,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    #[default]
    Pending,
    AcceptForkChange,
    AcceptUpstream,
    NeedsAgentReview,
    Documented,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MergeReviewState {
    #[default]
    NotReviewed,
    Summarized,
    OpenQuestion,
}

impl MergeReviewState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotReviewed => "Not reviewed yet",
            Self::Summarized => "Summarized",
            Self::OpenQuestion => "Open question",
        }
    }

    fn compact_label(self) -> &'static str {
        match self {
            Self::NotReviewed => "Pending",
            Self::Summarized => "Done",
            Self::OpenQuestion => "Stuck",
        }
    }
}

pub fn merge_review_session_active(cx: &App) -> bool {
    load_session(cx).is_some()
}

pub fn merge_review_workflow_engaged(cx: &App) -> bool {
    load_session(cx).is_some_and(|session| session.focus_layout_active)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReviewItem {
    pub path: String,
    pub category_id: String,
    pub surmount_section: String,
    pub disposition: ReviewDisposition,
    pub verdict: ReviewVerdict,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub notes: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub review_state: MergeReviewState,
    #[serde(default)]
    pub suggested_outcome: Option<MergeReviewSuggestedOutcome>,
    #[serde(default)]
    pub open_question_todo_id: Option<String>,
    #[serde(default)]
    pub conflict_decision: Option<MergeReviewConflictDecision>,
    #[serde(default)]
    pub conflict_test_todo_ids: Vec<String>,
    #[serde(default)]
    pub conflict_todos_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReviewConflictDecision {
    pub outcome: MergeReviewSuggestedOutcome,
    pub rationale: String,
    #[serde(default)]
    pub surmount_note: Option<String>,
    #[serde(default)]
    pub test_assertions: Vec<String>,
    #[serde(default)]
    pub test_paths: Vec<String>,
    #[serde(default)]
    pub recorded_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeReviewSuggestedOutcome {
    KeepFork,
    TakeUpstream,
    Synthesize,
    NeedsHuman,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MergeReviewSession {
    pub merge_base: String,
    pub upstream_ref: String,
    pub items: Vec<MergeReviewItem>,
    pub categories_completed: HashSet<String>,
    #[serde(default)]
    pub running_notes: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub pending_summary_path: Option<String>,
    #[serde(default)]
    pub pending_summary_format_retries: u8,
    /// Left/bottom dock positions collapsed when merge review focus layout was applied.
    #[serde(default)]
    pub docks_collapsed: Vec<String>,
    /// True while Branch Diff focus layout is active; stale persisted sessions stay false on cold start.
    #[serde(default)]
    pub focus_layout_active: bool,
    #[serde(default)]
    pub pending_conflict_discuss_path: Option<String>,
    #[serde(default)]
    pub pending_conflict_synthesize_path: Option<String>,
    #[serde(default)]
    pub pending_conflict_record_path: Option<String>,
    #[serde(default)]
    pub pending_conflict_todos_path: Option<String>,
    #[serde(default)]
    pub worktree_root: Option<String>,
    #[serde(default)]
    pub git_mode: git_ui::project_diff::MergeReviewGitMode,
    #[serde(default)]
    pub unmerged_count: u32,
    #[serde(default)]
    pub upstream_sha: Option<String>,
    #[serde(default)]
    pub merge_head_sha: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Agent-drafted merge commit message (human still runs `git commit`).
    #[serde(default)]
    pub pending_merge_commit_message: Option<String>,
    #[serde(default)]
    pub pending_merge_commit_message_capture: bool,
    #[serde(default)]
    pub pending_merge_commit_message_format_retries: u8,
    /// Linear queue position while merge is in progress; `None` in PreMerge or legacy sessions.
    #[serde(default)]
    pub queue_cursor_path: Option<String>,
    /// Branch Diff active file (repo-relative); kept in sync with local git and ProjectDiff.
    #[serde(default)]
    pub branch_diff_selected_path: Option<String>,
    #[serde(default)]
    pub expanded_categories: HashSet<String>,
    #[serde(default)]
    pub show_all_actionable: bool,
}

pub fn clarifying_note_pending(cx: &App) -> bool {
    crate::thread_metadata_store::ThreadMetadataStore::global(cx)
        .read(cx)
        .load_global_json(CLARIFYING_NOTE_KEY)
        .is_some()
}

pub fn stash_merge_review_clarifying_note(cx: &mut App, note: Option<String>) {
    let store = crate::thread_metadata_store::ThreadMetadataStore::global(cx);
    store.update(cx, |store, _| {
        if let Ok(json) = serde_json::to_string(&note) {
            store.save_global_json(CLARIFYING_NOTE_KEY, &json).log_err();
        }
    });
}

pub fn take_merge_review_clarifying_note(cx: &mut App) -> Option<String> {
    let json = crate::thread_metadata_store::ThreadMetadataStore::global(cx)
        .read(cx)
        .load_global_json(CLARIFYING_NOTE_KEY)?;
    crate::thread_metadata_store::ThreadMetadataStore::global(cx).update(cx, |store, _| {
        store.delete_global(CLARIFYING_NOTE_KEY).log_err();
    });
    serde_json::from_str(&json).ok().flatten()
}

pub fn intercept_review_branch_diff_note_modal(
    _workspace: &mut Workspace,
    _window: &mut Window,
    _cx: &mut Context<Workspace>,
    _diff_text: &str,
    _base_ref: &str,
    _file_path: Option<&str>,
) -> bool {
    false
}

pub fn merge_review_append_user_note(prompt: String, user_note: Option<&str>) -> String {
    let Some(note) = user_note.map(str::trim).filter(|note| !note.is_empty()) else {
        return prompt;
    };
    format!("Human direction:\n{note}\n\n{prompt}")
}

pub async fn detect_merge_review_git_mode(
    worktree_root: &Path,
) -> git_ui::project_diff::MergeReviewGitMode {
    use git_ui::project_diff::MergeReviewGitMode;
    match run_git(
        worktree_root,
        &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
    )
    .await
    {
        Ok(_) => MergeReviewGitMode::MergeInProgress,
        Err(_) => MergeReviewGitMode::PreMerge,
    }
}

pub async fn count_unmerged_paths(worktree_root: &Path) -> u32 {
    run_git(
        worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await
    .map(|output| output.split('\0').filter(|path| !path.is_empty()).count() as u32)
    .unwrap_or(0)
}

pub async fn refresh_merge_review_git_state(
    session: &mut MergeReviewSession,
    worktree_root: &Path,
) -> Result<()> {
    let upstream_ref = session.upstream_ref.clone();
    session.git_mode = detect_merge_review_git_mode(worktree_root).await;
    session.unmerged_count = count_unmerged_paths(worktree_root).await;
    session.upstream_sha = run_git(worktree_root, &["rev-parse", &upstream_ref])
        .await
        .ok()
        .map(|sha| sha.trim().to_string());
    session.head_sha = run_git(worktree_root, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|sha| sha.trim().to_string());
    session.merge_head_sha =
        if session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
            run_git(
                worktree_root,
                &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
            )
            .await
            .ok()
            .map(|sha| sha.trim().to_string())
        } else {
            None
        };
    if let Ok(manifest) = load_manifest_from_worktree(worktree_root) {
        let merge_base = run_git(worktree_root, &["merge-base", "HEAD", &upstream_ref])
            .await
            .ok()
            .map(|sha| sha.trim().to_string());
        let name_status = run_git(
            worktree_root,
            &["diff", "--merge-base", &upstream_ref, "--name-status", "-z"],
        )
        .await
        .unwrap_or_default();
        let conflict_output = run_git(
            worktree_root,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        )
        .await
        .unwrap_or_default();
        let paths = reconcile_review_paths_from_git(worktree_root, &name_status, &conflict_output);
        if let Some(merge_base) = merge_base {
            session.merge_base = merge_base;
        }
        reconcile_merge_review_session_items(session, &manifest, &paths);
    } else {
        let conflict_output = run_git(
            worktree_root,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        )
        .await
        .unwrap_or_default();
        let conflict_paths = conflict_output
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect::<HashSet<_>>();
        for item in &mut session.items {
            if conflict_paths.contains(&item.path) {
                item.disposition = ReviewDisposition::Conflict;
            }
        }
    }
    reconcile_merge_review_branch_diff_selection(session);
    reconcile_merge_review_queue_cursor(session);
    Ok(())
}

pub fn spawn_refresh_merge_review_git_state(window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(worktree_root) = load_session(cx)
        .and_then(|session| session.worktree_root)
        .map(PathBuf::from)
    else {
        return;
    };
    let workspace_weak = cx.entity().downgrade();
    window
        .spawn(cx, async move |cx| {
            let mut session = workspace_weak
                .update(cx, |_, cx| load_session(cx))
                .ok()
                .flatten()
                .context("merge review session missing during git refresh")?;
            let prev_mode = session.git_mode;
            let prev_unmerged = session.unmerged_count;
            let upstream_ref = session.upstream_ref.clone();
            refresh_merge_review_git_state(&mut session, worktree_root.as_path())
                .await
                .log_err();
            reconcile_merge_review_queue_cursor(&mut session);
            let merge_started = merge_review_merge_started(prev_mode, prev_unmerged, &session);
            let auto_select = if merge_started {
                initialize_merge_review_queue_cursor(&mut session);
                session
                    .queue_cursor_path
                    .clone()
                    .map(|path| (session.unmerged_count, path))
            } else {
                None
            };
            workspace_weak
                .update_in(cx, |workspace, window, cx| {
                    save_session(cx, &session).log_err();
                    notify_merge_review_ui_changed(workspace, cx);
                    if let Some((conflict_count, start_path)) = auto_select {
                        spawn_select_merge_review_queue_file_when_ready(
                            workspace_weak.clone(),
                            upstream_ref,
                            conflict_count,
                            start_path,
                            window,
                            cx,
                        );
                    } else {
                        workspace.show_toast(
                            merge_review_toast(
                                "Refreshed merge review git state — continue when `git merge` is in progress.",
                            ),
                            cx,
                        );
                    }
                })
                .ok();
            anyhow::Ok(())
        })
        .detach();
}

pub const MERGE_TREE_PREVIEW_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTreePreview {
    pub conflict_count: u32,
    pub merged_count: u32,
    pub added_count: u32,
    pub removed_count: u32,
    pub summary_line: String,
    pub display_text: String,
    pub truncated: bool,
}

pub fn merge_review_merge_command(upstream_ref: &str) -> String {
    format!("git merge {upstream_ref}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMergeRunOutput {
    pub display_text: String,
    pub merge_started: bool,
    pub exited_clean: bool,
}

pub fn format_git_merge_run_display(stdout: &str, stderr: &str, exit_code: Option<i32>) -> String {
    let mut parts = Vec::new();
    if let Some(code) = exit_code {
        parts.push(format!("exit code: {code}"));
    }
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if !stdout.is_empty() {
        parts.push(stdout.to_string());
    }
    if !stderr.is_empty() {
        parts.push(stderr.to_string());
    }
    if parts.is_empty() {
        "(no output)".to_string()
    } else {
        parts.join("\n\n")
    }
}

pub async fn run_git_merge_capture(
    worktree_root: &Path,
    upstream_ref: &str,
) -> Result<GitMergeRunOutput> {
    let output = smol::process::Command::new("git")
        .current_dir(worktree_root)
        .args(["merge", upstream_ref])
        .output()
        .await
        .context("spawning git merge")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code();
    let merge_head = smol::process::Command::new("git")
        .current_dir(worktree_root)
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .await
        .ok()
        .filter(|head| head.status.success());
    let merge_started = merge_head.is_some();
    let exited_clean = output.status.success();
    Ok(GitMergeRunOutput {
        display_text: format_git_merge_run_display(&stdout, &stderr, exit_code),
        merge_started,
        exited_clean,
    })
}

pub fn truncate_merge_tree_preview(text: &str, max_chars: usize) -> (String, bool) {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return (text.to_string(), false);
    }
    let truncated: String = text.chars().take(max_chars).collect();
    (format!("{truncated}\n…"), true)
}

pub fn parse_merge_tree_preview(raw: &str) -> MergeTreePreview {
    let mut conflict_count = 0u32;
    let mut merged_count = 0u32;
    let mut added_count = 0u32;
    let mut removed_count = 0u32;
    for line in raw.lines() {
        let trimmed = line.trim();
        match trimmed {
            "changed in both" => conflict_count += 1,
            "merged" => merged_count += 1,
            "added in remote" | "added in both" => added_count += 1,
            "removed in remote" | "removed in both" => removed_count += 1,
            _ => {
                if trimmed.starts_with("CONFLICT ") {
                    conflict_count += 1;
                }
            }
        }
    }
    let summary_line = if conflict_count > 0 {
        format!(
            "{conflict_count} file(s) would conflict; {merged_count} clean merge(s), \
             {added_count} add(s), {removed_count} remove(s)."
        )
    } else {
        format!(
            "No conflicts detected; {merged_count} merge(s), {added_count} add(s), \
             {removed_count} remove(s)."
        )
    };
    let (display_text, truncated) = truncate_merge_tree_preview(raw, MERGE_TREE_PREVIEW_MAX_CHARS);
    MergeTreePreview {
        conflict_count,
        merged_count,
        added_count,
        removed_count,
        summary_line,
        display_text,
        truncated,
    }
}

pub async fn fetch_merge_tree_preview(
    worktree_root: &Path,
    upstream_ref: &str,
) -> Result<MergeTreePreview> {
    let merge_base = run_git(worktree_root, &["merge-base", "HEAD", upstream_ref])
        .await?
        .trim()
        .to_string();
    let output = run_git(
        worktree_root,
        &["merge-tree", &merge_base, "HEAD", upstream_ref],
    )
    .await?;
    let preview = parse_merge_tree_preview(&output);
    if preview.truncated {
        log::info!("surmount merge review: full merge-tree output:\n{output}");
    }
    Ok(preview)
}

pub struct MergeReviewGatedMergeModal {
    upstream_ref: SharedString,
    worktree_root: PathBuf,
    merge_command: SharedString,
    summary_line: SharedString,
    conflict_count: Option<u32>,
    preview_text: SharedString,
    preview_truncated: bool,
    merge_running: bool,
    merge_result_text: Option<SharedString>,
    merge_started: bool,
    loading: bool,
    error: Option<SharedString>,
    workspace: WeakEntity<Workspace>,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl MergeReviewGatedMergeModal {
    fn new_loading(
        upstream_ref: SharedString,
        worktree_root: PathBuf,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            merge_command: merge_review_merge_command(upstream_ref.as_ref()).into(),
            upstream_ref,
            worktree_root,
            summary_line: "Loading merge-tree preview…".into(),
            conflict_count: None,
            preview_text: SharedString::default(),
            preview_truncated: false,
            merge_running: false,
            merge_result_text: None,
            merge_started: false,
            loading: true,
            error: None,
            workspace,
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn apply_preview(&mut self, preview: MergeTreePreview) {
        self.loading = false;
        self.error = None;
        self.conflict_count = Some(preview.conflict_count);
        self.summary_line = preview.summary_line.into();
        self.preview_text = preview.display_text.into();
        self.preview_truncated = preview.truncated;
    }

    fn apply_error(&mut self, message: impl Into<SharedString>) {
        self.loading = false;
        self.error = Some(message.into());
        self.summary_line = "Merge-tree preview failed.".into();
    }

    fn copy_merge_command(&self, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.merge_command.to_string()));
    }

    fn run_merge_command(&mut self, cx: &mut Context<Self>) {
        if self.merge_running || self.loading {
            return;
        }
        self.merge_running = true;
        self.error = None;
        cx.notify();
        let worktree_root = self.worktree_root.clone();
        let upstream_ref = self.upstream_ref.to_string();
        let workspace = self.workspace.clone();
        let modal_weak = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            let result = run_git_merge_capture(worktree_root.as_path(), &upstream_ref).await;
            let merge_started = result
                .as_ref()
                .ok()
                .is_some_and(|output| output.merge_started);
            modal_weak
                .update_in(cx, |modal, _window, cx| {
                    modal.merge_running = false;
                    match result {
                        Ok(output) => {
                            modal.merge_started = output.merge_started;
                            modal.merge_result_text = Some(output.display_text.into());
                            if output.merge_started {
                                modal.summary_line = if output.exited_clean {
                                    "Merge completed cleanly.".into()
                                } else {
                                    "Merge started — resolve conflicts in Branch Diff.".into()
                                };
                            } else {
                                modal.error =
                                    Some("git merge did not start — see output below.".into());
                            }
                        }
                        Err(error) => {
                            modal.error = Some(format!("{error:#}").into());
                        }
                    }
                    cx.notify();
                })
                .log_err();
            if merge_started {
                workspace
                    .update_in(cx, |_, window, cx| {
                        spawn_refresh_merge_review_git_state(window, cx);
                    })
                    .ok();
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn reconcile_git_state(&self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        cx.spawn(async move |_, cx| {
            workspace
                .update_in(cx, |_, window, cx| {
                    spawn_refresh_merge_review_git_state(window, cx);
                })
                .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for MergeReviewGatedMergeModal {}

impl Focusable for MergeReviewGatedMergeModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MergeReviewGatedMergeModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for MergeReviewGatedMergeModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_border = gpui::hsla(0., 0., 0.35, 1.);
        let scroll_label = if self.merge_running {
            SharedString::from("Running git merge…")
        } else if let Some(result) = self.merge_result_text.clone() {
            result
        } else if self.preview_text.is_empty() {
            SharedString::from("(no preview text — check git output in logs)")
        } else {
            self.preview_text.clone()
        };
        let conflict_hint = self.conflict_count.map(|count| {
            if count > 0 {
                format!("{count} file(s) may conflict")
            } else {
                "No conflicts detected in preview".to_string()
            }
        });
        let show_preview_hint =
            self.merge_result_text.is_none() && !self.loading && self.error.is_none();
        let dialog_label: SharedString = format!("Preview merge · {}", self.upstream_ref).into();
        div()
            .id("merge-review-gated-merge-modal")
            .role(Role::Dialog)
            .aria_label(dialog_label.clone())
            .key_context("MergeReviewGatedMergeModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .child(
                v_flex()
                    .w(rems(32.))
                    .h(rems(32.))
                    .border_1()
                    .border_color(panel_border)
                    .bg(gpui::hsla(0., 0., 0.12, 1.))
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(panel_border)
                            .child(Label::new(dialog_label).color(Color::Default)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .p_2()
                            .gap_2()
                            .overflow_hidden()
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                )
                            })
                            .when(self.loading, |this| {
                                this.child(
                                    Label::new("Running git merge-tree…")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .when(!self.loading, |this| {
                                this.child(
                                    Label::new(self.summary_line.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Default),
                                )
                                .when(show_preview_hint, |this| {
                                    this.when_some(conflict_hint, |this, hint| {
                                        this.child(
                                            Label::new(hint)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    })
                                })
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                                    .child(
                                        v_flex()
                                            .id("merge-review-gated-preview")
                                            .size_full()
                                            .max_h(rems(14.))
                                            .p_2()
                                            .rounded_md()
                                            .bg(gpui::hsla(0., 0., 0.08, 1.))
                                            .track_scroll(&self.scroll_handle)
                                            .overflow_y_scroll()
                                            .child(
                                                Label::new(scroll_label)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    ),
                            )
                            .when(show_preview_hint && self.preview_truncated, |this| {
                                this.child(
                                    Label::new("Preview truncated — full output logged.")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .p_2()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("merge-review-gated-cancel", "Cancel")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-gated-copy", "Copy merge command")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.copy_merge_command(cx);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-gated-run", "Run merge")
                                    .style(ButtonStyle::Filled)
                                    .disabled(
                                        self.merge_running || self.loading || self.merge_started,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.run_merge_command(cx);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-gated-started", "I've started the merge")
                                    .style(ButtonStyle::Outlined)
                                    .disabled(self.merge_running)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reconcile_git_state(cx);
                                    })),
                            ),
                    ),
            )
    }
}

pub fn open_merge_review_gated_merge_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !merge_review_workflow_engaged(cx) {
        workspace.show_toast(
            merge_review_toast("Start merge review before previewing the merge."),
            cx,
        );
        return;
    }
    let Some(session) = load_session(cx) else {
        workspace.show_toast(merge_review_toast("No active merge review session."), cx);
        return;
    };
    if session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
        workspace.show_toast(
            merge_review_toast("Merge already in progress — resolve conflicts in Branch Diff."),
            cx,
        );
        return;
    };
    let Some(worktree_root) = session
        .worktree_root
        .clone()
        .map(PathBuf::from)
        .or_else(|| project_worktree_root(workspace.project(), cx))
    else {
        workspace.show_toast(
            merge_review_toast("No worktree root for merge preview."),
            cx,
        );
        return;
    };
    let upstream_ref: SharedString = session.upstream_ref.as_str().into();
    let workspace_weak = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let modal_weak = cx.weak_entity();
        let upstream_for_task = upstream_ref.clone();
        let worktree_root_for_task = worktree_root.clone();
        window
            .spawn(cx, async move |cx| {
                match fetch_merge_tree_preview(
                    worktree_root_for_task.as_path(),
                    upstream_for_task.as_ref(),
                )
                .await
                {
                    Ok(preview) => {
                        modal_weak
                            .update_in(cx, |modal: &mut MergeReviewGatedMergeModal, _window, cx| {
                                modal.apply_preview(preview);
                                cx.notify();
                            })
                            .log_err();
                    }
                    Err(error) => {
                        log::warn!("surmount merge review: merge-tree preview failed: {error:#}");
                        modal_weak
                            .update_in(cx, |modal: &mut MergeReviewGatedMergeModal, _window, cx| {
                                modal.apply_error(error.to_string());
                                cx.notify();
                            })
                            .log_err();
                    }
                }
                anyhow::Ok(())
            })
            .detach();
        MergeReviewGatedMergeModal::new_loading(
            upstream_ref,
            worktree_root.clone(),
            workspace_weak,
            cx,
        )
    });
    // Safe to focus now: ModalLayer already snapshotted previous_focus_handle in
    // show_modal (via toggle_modal); this re-asserts AC-A alongside ModalLayer's
    // deferred focus onto Focusable modals.
    if let Some(modal) = workspace.active_modal::<MergeReviewGatedMergeModal>(cx) {
        modal.focus_handle(cx).focus(window, cx);
        log::info!("surmount merge review: focused Preview merge modal after open");
    }
}

pub struct MergeReviewCommitMessageModal {
    message_editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    last_loaded_draft: Option<String>,
    waiting_for_draft: bool,
}

impl MergeReviewCommitMessageModal {
    fn new(
        initial_draft: Option<String>,
        waiting_for_draft: bool,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            if let Some(draft) = initial_draft.as_deref() {
                editor.set_text(draft, window, cx);
            } else {
                editor.set_placeholder_text(
                    "Merge commit message draft will appear here…",
                    window,
                    cx,
                );
            }
            editor
        });
        let modal = Self {
            message_editor,
            workspace,
            focus_handle: cx.focus_handle(),
            last_loaded_draft: initial_draft,
            waiting_for_draft,
        };
        modal.spawn_draft_poll(window, cx);
        modal
    }

    fn spawn_draft_poll(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.waiting_for_draft {
            return;
        }
        let modal_weak = cx.weak_entity();
        window
            .spawn(cx, async move |cx| {
                for _ in 0..120 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    let draft = cx
                        .update(|_, cx| {
                            load_session(cx).and_then(|session| {
                                session
                                    .pending_merge_commit_message
                                    .clone()
                                    .filter(|_| !session.pending_merge_commit_message_capture)
                            })
                        })
                        .ok()
                        .flatten();
                    if let Some(draft) = draft {
                        modal_weak
                            .update_in(cx, |modal, window, cx| {
                                modal.apply_draft(draft, window, cx);
                            })
                            .log_err();
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .detach();
    }

    fn apply_draft(&mut self, draft: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.last_loaded_draft.as_deref() == Some(draft.as_str()) {
            return;
        }
        self.last_loaded_draft = Some(draft.clone());
        self.waiting_for_draft = false;
        self.message_editor.update(cx, |editor, cx| {
            editor.set_text(draft.as_str(), window, cx);
        });
        cx.notify();
    }

    fn message_text(&self, cx: &App) -> String {
        self.message_editor.read(cx).text(cx)
    }

    fn copy_message(&self, cx: &mut App) {
        let text = self.message_text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn dismiss(&self, cx: &mut Context<Self>) {
        clear_pending_merge_commit_message_capture(cx);
        cx.emit(DismissEvent);
    }

    fn done(&self, cx: &mut Context<Self>) {
        let text = self.message_text(cx).trim().to_string();
        let workspace = self.workspace.clone();
        clear_pending_merge_commit_message_capture(cx);
        cx.emit(DismissEvent);
        if text.is_empty() {
            return;
        }
        cx.spawn(async move |_, cx| {
            workspace
                .update(cx, |_workspace, cx| {
                    if let Some(mut session) = load_session(cx) {
                        session.pending_merge_commit_message = Some(text);
                        save_session(cx, &session).log_err();
                    }
                })
                .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for MergeReviewCommitMessageModal {}

impl Focusable for MergeReviewCommitMessageModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MergeReviewCommitMessageModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for MergeReviewCommitMessageModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let green = crate::merge_review_step_rail::merge_review_workflow_green_border();
        div()
            .key_context("MergeReviewCommitMessageModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                this.dismiss(cx);
            }))
            .child(
                v_flex()
                    .w(rems(32.))
                    .h(rems(32.))
                    .border_1()
                    .border_color(green)
                    .bg(gpui::hsla(0., 0., 0.12, 1.))
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(green)
                            .child(Label::new("Merge commit message draft").color(Color::Default)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .p_2()
                            .min_h_0()
                            .child(self.message_editor.clone()),
                    )
                    .when(self.waiting_for_draft, |this| {
                        this.child(
                            div().px_2().child(
                                Label::new("Agent drafting message…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .child(
                        h_flex()
                            .p_2()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("merge-review-commit-cancel", "Cancel")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.dismiss(cx);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-commit-copy", "Copy message")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.copy_message(cx);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-commit-done", "Done")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.done(cx);
                                    })),
                            ),
                    ),
            )
    }
}

pub fn extract_merge_commit_message_from_reply(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start_idx = lines.iter().position(|line| {
        line.trim()
            .to_ascii_lowercase()
            .starts_with("merge commit message:")
    })?;
    let first = lines[start_idx].trim();
    let mut message = first
        .split_once(':')
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default();
    for line in lines.iter().skip(start_idx + 1) {
        if message.is_empty() && line.trim().is_empty() {
            continue;
        }
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(line);
    }
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn try_capture_merge_review_commit_message_from_reply(
    reply_text: &str,
    cx: &mut App,
) -> Option<String> {
    let Some(mut session) = load_session(cx) else {
        return None;
    };
    if !session.pending_merge_commit_message_capture {
        return None;
    };
    let draft = extract_merge_commit_message_from_reply(reply_text)?;
    session.pending_merge_commit_message = Some(draft.clone());
    session.pending_merge_commit_message_capture = false;
    save_session(cx, &session).log_err();
    log::info!("surmount merge review: captured merge commit message draft");
    Some(draft)
}

pub fn merge_review_commit_message_prompt(session: &MergeReviewSession) -> String {
    let patterns = if session.patterns.is_empty() {
        "(none)".to_string()
    } else {
        session
            .patterns
            .iter()
            .map(|pattern| format!("- {pattern}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let notes_excerpt = if session.running_notes.is_empty() {
        "(none)".to_string()
    } else {
        session.running_notes.chars().take(3000).collect()
    };
    let conflict_decisions = session
        .items
        .iter()
        .filter_map(|item| {
            item.conflict_decision.as_ref().map(|decision| {
                format!(
                    "- `{}`: {:?} — {}",
                    item.path, decision.outcome, decision.rationale
                )
            })
        })
        .collect::<Vec<_>>();
    let decisions_block = if conflict_decisions.is_empty() {
        "(none)".to_string()
    } else {
        conflict_decisions.join("\n")
    };
    format!(
        "Draft a merge commit message for merging upstream `{upstream}` into the Surmount fork.\n\
         Merge base: {merge_base}\n\
         Reviewed {reviewed}/{total} files in this session.\n\n\
         Patterns from this merge:\n{patterns}\n\n\
         Running notes excerpt:\n{notes_excerpt}\n\n\
         Conflict decisions:\n{decisions_block}\n\n\
         Write a concise merge commit (subject line + optional body).\n\
         End with exactly one line: Merge commit message: <subject and body>\n\
         The human runs `git commit` — do not commit or use todo_write in this turn.",
        upstream = session.upstream_ref,
        merge_base = session.merge_base,
        reviewed = session.reviewed_count(),
        total = session.items.len(),
        patterns = patterns,
        notes_excerpt = notes_excerpt,
        decisions_block = decisions_block,
    )
}

pub fn request_merge_review_commit_message_draft(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(mut session) = load_session(cx) else {
        workspace.show_toast(merge_review_toast("No active merge review session."), cx);
        return false;
    };
    if !session_summarized_complete(&session) {
        workspace.show_toast(
            merge_review_toast("Finish reviewing all files before drafting the commit message."),
            cx,
        );
        return false;
    };
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        workspace.show_toast(
            merge_review_toast("Open the agent panel before drafting a commit message."),
            cx,
        );
        return false;
    };
    let prompt = merge_review_commit_message_prompt(&session);
    session.pending_merge_commit_message_capture = true;
    session.pending_merge_commit_message_format_retries = 0;
    save_session(cx, &session).log_err();
    workspace.open_panel::<AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.send_merge_review_follow_up(prompt, window, cx);
    });
    log::info!("surmount merge review: requested merge commit message draft");
    true
}

pub fn open_merge_review_commit_message_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !merge_review_workflow_engaged(cx) {
        workspace.show_toast(
            merge_review_toast("Start merge review before drafting a commit message."),
            cx,
        );
        return;
    }
    let Some(session) = load_session(cx) else {
        workspace.show_toast(merge_review_toast("No active merge review session."), cx);
        return;
    };
    if !session_summarized_complete(&session) {
        workspace.show_toast(
            merge_review_toast("Finish reviewing all files before drafting the commit message."),
            cx,
        );
        return;
    };
    let mut initial_draft = session.pending_merge_commit_message.clone();
    let mut waiting_for_draft =
        initial_draft.is_none() && session.pending_merge_commit_message_capture;
    if initial_draft.is_none() && !session.pending_merge_commit_message_capture {
        request_merge_review_commit_message_draft(workspace, window, cx);
        waiting_for_draft = true;
    }
    if let Some(session) = load_session(cx) {
        initial_draft = session
            .pending_merge_commit_message
            .clone()
            .or(initial_draft);
        if initial_draft.is_none() {
            waiting_for_draft = session.pending_merge_commit_message_capture;
        }
    }
    let workspace_weak = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        MergeReviewCommitMessageModal::new(
            initial_draft,
            waiting_for_draft,
            workspace_weak,
            window,
            cx,
        )
    });
}

#[derive(Clone)]
pub enum MergeReviewNoteContinuation {
    DiscussConflict,
    SynthesizeConflict,
    ConfirmDecision(MergeReviewSuggestedOutcome),
    SendReviewComments,
    DraftSection,
    ReviewBranchDiff {
        diff_text: SharedString,
        base_ref: SharedString,
        file_path: Option<SharedString>,
    },
}

pub struct MergeReviewNoteModal {
    title: SharedString,
    note_input: Entity<InputField>,
    continuation: MergeReviewNoteContinuation,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl MergeReviewNoteModal {
    fn new(
        title: SharedString,
        placeholder: &str,
        continuation: MergeReviewNoteContinuation,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let note_input = cx.new(|cx| {
            InputField::new(window, cx, placeholder)
                .label("Optional direction")
                .tab_index(1)
                .tab_stop(true)
        });
        Self {
            title,
            note_input,
            continuation,
            workspace,
            focus_handle: cx.focus_handle(),
        }
    }

    fn note_text(&self, cx: &App) -> Option<String> {
        let text = self.note_input.read(cx).text(cx);
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn submit(&self, cx: &mut Context<Self>) {
        let note = self.note_text(cx);
        let continuation = self.continuation.clone();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        cx.spawn(async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    run_merge_review_note_continuation(workspace, window, cx, continuation, note);
                })
                .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for MergeReviewNoteModal {}

impl Focusable for MergeReviewNoteModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MergeReviewNoteModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for MergeReviewNoteModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let green = crate::merge_review_step_rail::merge_review_workflow_green_border();
        div()
            .key_context("MergeReviewNoteModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                this.submit(cx);
            }))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .child(
                v_flex()
                    .w(rems(28.))
                    .border_1()
                    .border_color(green)
                    .bg(gpui::hsla(0., 0., 0.12, 1.))
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(green)
                            .child(Label::new(self.title.clone()).color(Color::Default)),
                    )
                    .child(div().p_2().child(self.note_input.clone()))
                    .child(
                        h_flex()
                            .p_2()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("merge-review-note-cancel", "Cancel")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("merge-review-note-continue", "Continue")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit(cx);
                                    })),
                            ),
                    ),
            )
    }
}

pub fn open_merge_review_note_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    title: impl Into<SharedString>,
    placeholder: &str,
    continuation: MergeReviewNoteContinuation,
) {
    let title = title.into();
    let workspace_weak = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        MergeReviewNoteModal::new(title, placeholder, continuation, workspace_weak, window, cx)
    });
}

fn run_merge_review_note_continuation(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    continuation: MergeReviewNoteContinuation,
    note: Option<String>,
) {
    match continuation {
        MergeReviewNoteContinuation::DiscussConflict => {
            dispatch_merge_review_conflict_workshop(workspace, window, cx, false, note.as_deref());
        }
        MergeReviewNoteContinuation::SynthesizeConflict => {
            dispatch_merge_review_conflict_workshop(workspace, window, cx, true, note.as_deref());
        }
        MergeReviewNoteContinuation::ConfirmDecision(outcome) => {
            confirm_merge_review_decision(workspace, window, cx, outcome, note.as_deref());
        }
        MergeReviewNoteContinuation::SendReviewComments => {
            send_merge_review_comments_to_agent(workspace, window, cx, note.as_deref());
        }
        MergeReviewNoteContinuation::DraftSection => {
            draft_merge_review_section(workspace, window, cx, note.as_deref());
        }
        MergeReviewNoteContinuation::ReviewBranchDiff {
            diff_text,
            base_ref,
            file_path,
        } => {
            stash_merge_review_clarifying_note(cx, note);
            window.dispatch_action(
                zed_actions::agent::ReviewBranchDiff {
                    diff_text,
                    base_ref,
                    file_path,
                }
                .boxed_clone(),
                cx,
            );
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestFile {
    #[serde(default, rename = "version")]
    _version: u32,
    rules: Vec<CategoryRuleDef>,
}

#[derive(Debug, Clone, Deserialize)]
struct CategoryRuleDef {
    category_id: String,
    surmount_section: String,
    disposition: String,
    #[serde(default)]
    risk: String,
    paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CategoryRule {
    pub category_id: String,
    pub surmount_section: String,
    pub disposition: ReviewDisposition,
    pub risk: String,
    glob_set: GlobSet,
}

#[derive(Debug, Clone, Default)]
pub struct CategoryManifest {
    pub rules: Vec<CategoryRule>,
}

impl CategoryManifest {
    pub fn from_toml(content: &str) -> Result<Self> {
        let manifest: ManifestFile =
            toml::from_str(content).context("parsing surmount merge categories manifest")?;
        let mut rules = Vec::new();
        for rule in manifest.rules {
            let mut builder = GlobSetBuilder::new();
            for path in &rule.paths {
                let pattern = Glob::new(path).with_context(|| format!("invalid glob: {path}"))?;
                builder.add(pattern);
            }
            let glob_set = builder.build().context("building category glob set")?;
            rules.push(CategoryRule {
                category_id: rule.category_id,
                surmount_section: rule.surmount_section,
                disposition: parse_disposition(&rule.disposition)?,
                risk: rule.risk,
                glob_set,
            });
        }
        Ok(Self { rules })
    }

    pub fn classify_path(&self, path: &str) -> MergeReviewItem {
        let normalized = path.replace('\\', "/");
        for rule in &self.rules {
            if rule.glob_set.is_match(&normalized) {
                return MergeReviewItem {
                    path: normalized,
                    category_id: rule.category_id.clone(),
                    surmount_section: rule.surmount_section.clone(),
                    disposition: rule.disposition,
                    verdict: ReviewVerdict::Pending,
                    lines_added: 0,
                    lines_removed: 0,
                    notes: None,
                    summary: None,
                    review_state: MergeReviewState::default(),
                    suggested_outcome: None,
                    open_question_todo_id: None,
                    conflict_decision: None,
                    conflict_test_todo_ids: Vec::new(),
                    conflict_todos_complete: false,
                };
            }
        }
        MergeReviewItem {
            path: normalized,
            category_id: "uncategorized".into(),
            surmount_section: "Uncategorized".into(),
            disposition: ReviewDisposition::Ambiguous,
            verdict: ReviewVerdict::Pending,
            lines_added: 0,
            lines_removed: 0,
            notes: None,
            summary: None,
            review_state: MergeReviewState::default(),
            suggested_outcome: None,
            open_question_todo_id: None,
            conflict_decision: None,
            conflict_test_todo_ids: Vec::new(),
            conflict_todos_complete: false,
        }
    }
}

fn parse_disposition(value: &str) -> Result<ReviewDisposition> {
    Ok(match value {
        "auto_clear" => ReviewDisposition::AutoClear,
        "fork_owned" => ReviewDisposition::ForkOwned,
        "ambiguous" => ReviewDisposition::Ambiguous,
        "build_config" => ReviewDisposition::BuildConfig,
        "conflict" => ReviewDisposition::Conflict,
        "confirmed" => ReviewDisposition::Confirmed,
        "deferred" => ReviewDisposition::Deferred,
        other => anyhow::bail!("unknown disposition: {other}"),
    })
}

/// True when `worktree_root` is (or lives under) a Surmount fork root with `SURMOUNT.md`.
///
/// Accepts either a directory containing `SURMOUNT.md`, or a **single-file worktree**
/// whose work directory is a file under such a root (common when agent-stdio / open-file
/// opens `SURMOUNT.md` and GPUI sets work_directory to the file path itself).
pub fn is_surmount_workspace(worktree_root: &Path) -> bool {
    if worktree_root.join("SURMOUNT.md").is_file() {
        return true;
    }
    if worktree_root.is_file() {
        if worktree_root.file_name().and_then(|name| name.to_str()) == Some("SURMOUNT.md") {
            return true;
        }
        if let Some(parent) = worktree_root.parent() {
            return parent.join("SURMOUNT.md").is_file();
        }
    }
    false
}

/// Directory used for git + manifest I/O when `work_directory` may be a single file.
pub fn normalize_surmount_worktree_root(work_directory: &Path) -> PathBuf {
    if work_directory.is_file() {
        work_directory
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| work_directory.to_path_buf())
    } else {
        work_directory.to_path_buf()
    }
}

fn is_reviewable_changed_path(worktree_root: &Path, path: &str) -> bool {
    !worktree_root.join(path).is_dir()
}

pub(crate) fn merge_review_toast(message: impl Into<String>) -> Toast {
    Toast::new(
        NotificationId::named(SharedString::from("surmount-merge-review")),
        message.into(),
    )
}

pub(crate) fn surmount_repository(
    project: &Entity<Project>,
    cx: &App,
) -> Option<Entity<Repository>> {
    project.read(cx).repositories(cx).values().find_map(|repo| {
        let snapshot = repo.read(cx).snapshot();
        let root = snapshot.work_directory_abs_path.as_ref();
        is_surmount_workspace(root).then(|| repo.clone())
    })
}

fn dock_position_storage_label(position: DockPosition) -> &'static str {
    match position {
        DockPosition::Left => "left",
        DockPosition::Bottom => "bottom",
        DockPosition::Right => "right",
    }
}

fn dock_position_from_storage_label(label: &str) -> Option<DockPosition> {
    match label {
        "left" => Some(DockPosition::Left),
        "bottom" => Some(DockPosition::Bottom),
        "right" => Some(DockPosition::Right),
        _ => None,
    }
}

const MERGE_REVIEW_FOCUS_DOCKS: [DockPosition; 2] = [DockPosition::Left, DockPosition::Bottom];

fn collapse_docks_for_merge_review_focus(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<String> {
    let mut collapsed = Vec::new();
    for position in MERGE_REVIEW_FOCUS_DOCKS {
        if !workspace.is_dock_at_position_open(position, cx) {
            continue;
        }
        workspace
            .dock_at_position(position)
            .update(cx, |dock, cx| dock.set_open(false, window, cx));
        collapsed.push(dock_position_storage_label(position).to_string());
    }
    if !collapsed.is_empty() {
        workspace.focus_center_pane(window, cx);
        cx.notify();
    }
    collapsed
}

pub fn restore_merge_review_collapsed_docks(
    workspace: &mut Workspace,
    session: &MergeReviewSession,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mut restored = Vec::new();
    for label in &session.docks_collapsed {
        let Some(position) = dock_position_from_storage_label(label) else {
            continue;
        };
        if workspace.is_dock_at_position_open(position, cx) {
            continue;
        }
        workspace.toggle_dock(position, window, cx);
        restored.push(label.as_str());
    }
    if restored.is_empty() {
        return;
    }
    log::info!(
        "surmount merge review: restored docks for focus layout: {}",
        restored.join(", ")
    );
    workspace.focus_center_pane(window, cx);
    cx.notify();
}

fn unzoom_agent_dock_for_merge_review(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(zoomed_position) = workspace.zoomed_dock_position() else {
        return;
    };
    log::info!("surmount merge review: unzooming {zoomed_position:?} dock for branch diff focus");
    // Must not emit PanelEvent::ZoomOut here: restore runs inside workspace.update,
    // and Dock's ZoomOut handler calls workspace.update again (double-lease panic).
    workspace.unzoom_dock_panel::<AgentPanel>(window, cx);
    workspace.focus_center_pane(window, cx);
}

/// Re-applies merge-review focus layout when a persisted session is active but the
/// workspace restored with a zoomed agent dock and no visible Branch Diff.
pub fn restore_merge_review_workspace_layout(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(mut session) = load_session(cx) else {
        return;
    };
    if !session.focus_layout_active {
        return;
    }
    log::info!("surmount merge review: restoring workspace layout from persisted session");
    if let Some(stale_path) = reconcile_stale_pending_summary_capture(cx) {
        workspace.show_toast(
            merge_review_toast(format!(
                "Resumed merge review — click Review Diff to retry {stale_path}."
            )),
            cx,
        );
    }
    ensure_merge_review_focus_layout(workspace, &mut session, window, cx);
    if let Err(error) = save_session(cx, &session) {
        log::error!("surmount merge review: failed to persist session during restore: {error:#}");
    }
    let upstream_ref = session.upstream_ref.clone();
    if !reveal_branch_diff_for_merge_review(workspace, &upstream_ref, window, cx) {
        let project = workspace.project().clone();
        if let Some(repository) = surmount_repository(&project, cx) {
            ProjectDiff::open_against_base_ref(
                workspace,
                project,
                repository,
                upstream_ref.as_str().into(),
                window,
                cx,
            );
            let _ = reveal_branch_diff_for_merge_review(workspace, &upstream_ref, window, cx);
        }
    }
    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        panel.update(cx, |panel, cx| {
            panel.prepare_for_merge_review(window, cx);
        });
    }
    restore_merge_review_branch_diff_selection(workspace, &upstream_ref, window, cx);
}

fn restore_merge_review_branch_diff_selection(
    workspace: &mut Workspace,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = load_session(cx) else {
        return;
    };
    let Some(path) = session
        .branch_diff_selected_path
        .as_deref()
        .or(session.queue_cursor_path.as_deref())
    else {
        return;
    };
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        let workspace_weak = cx.entity().downgrade();
        spawn_select_merge_review_queue_file_when_ready(
            workspace_weak,
            upstream_ref.to_string(),
            session.unmerged_count,
            path.to_string(),
            window,
            cx,
        );
        return;
    };
    if diff.update(cx, |diff, cx| {
        diff.move_to_repo_relative_path(path, window, cx)
    }) {
        prepare_merge_review_selected_file(&diff, path, window, cx);
        workspace.focus_center_pane(window, cx);
    }
}

fn ensure_merge_review_focus_layout(
    workspace: &mut Workspace,
    session: &mut MergeReviewSession,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        panel.update(cx, |panel, cx| {
            panel.prepare_for_merge_review(window, cx);
        });
    }
    unzoom_agent_dock_for_merge_review(workspace, window, cx);
    let collapsed = collapse_docks_for_merge_review_focus(workspace, window, cx);
    if collapsed.is_empty() {
        return;
    }
    log::info!(
        "surmount merge review: collapsed docks for focus layout: {}",
        collapsed.join(", ")
    );
    for dock in collapsed {
        if !session
            .docks_collapsed
            .iter()
            .any(|existing| existing == &dock)
        {
            session.docks_collapsed.push(dock);
        }
    }
}

fn merge_review_project_diff_for_upstream(
    workspace: &Workspace,
    upstream_ref: &str,
    cx: &App,
) -> Option<Entity<ProjectDiff>> {
    workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    })
}

fn reveal_branch_diff_for_merge_review(
    workspace: &mut Workspace,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(diff) = merge_review_project_diff_for_upstream(workspace, upstream_ref, cx) else {
        return false;
    };
    unzoom_agent_dock_for_merge_review(workspace, window, cx);
    workspace.activate_item(&diff, true, true, window, cx);
    workspace.focus_center_pane(window, cx);
    true
}

/// After Start chrome is ready: focus the rail primary **Preview merge** control.
fn focus_merge_review_primary_after_start(
    workspace: &Workspace,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(diff) = merge_review_project_diff_for_upstream(workspace, upstream_ref, cx) else {
        log::warn!("surmount merge review: no Branch Diff to focus primary Preview merge control");
        return;
    };
    let handle = diff.read(cx).merge_review_primary_focus_handle();
    handle.focus(window, cx);
    log::info!("surmount merge review: focused primary Preview merge control after Start");
}

/// After Next file success: focus ProjectDiff (editor when non-empty — path-labeled headers).
fn focus_merge_review_after_next_file(
    diff: &Entity<ProjectDiff>,
    window: &mut Window,
    cx: &mut App,
) {
    let handle = diff.focus_handle(cx);
    handle.focus(window, cx);
    log::info!("surmount merge review: focused ProjectDiff after Next file");
}

fn select_merge_review_queue_cursor_file(
    workspace: &mut Workspace,
    session: &MergeReviewSession,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let first_path = session
        .queue_cursor_path
        .clone()
        .or_else(|| first_unreviewed_merge_review_queue_path(session));
    let Some(first_path) = first_path.as_deref() else {
        return false;
    };
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        return false;
    };
    let selected = diff.update(cx, |diff, cx| {
        diff.move_to_repo_relative_path(first_path, window, cx)
    });
    if selected {
        prepare_merge_review_selected_file(&diff, first_path, window, cx);
        if let Some(mut session) = load_session(cx) {
            set_merge_review_branch_diff_selection(&mut session, first_path);
            save_session(cx, &session).log_err();
        }
        log::info!("surmount merge review: selected queue file {first_path}");
        // MergeInProgress auto-select intentionally focuses the selected file surface
        // (ProjectDiff editor), not the PreMerge Preview rail primary. That path is
        // outside Start→Preview AC-A; PreMerge Start does not spawn queue select.
        focus_merge_review_after_next_file(&diff, window, cx);
    }
    selected
}

fn spawn_select_merge_review_queue_file_when_ready(
    workspace_weak: WeakEntity<Workspace>,
    upstream_ref: String,
    conflict_count: u32,
    start_path: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    window
        .spawn(cx, async move |cx| {
            use std::time::Duration;

            // Fallback poll when Branch Diff is not ready yet. No clean readiness event on
            // ProjectDiff today — see review Issue 12 (wontfix until a hook exists).
            const MAX_SELECT_ATTEMPTS: u32 = 24;
            for attempt in 0..MAX_SELECT_ATTEMPTS {
                if attempt > 0 {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                }
                let Some(workspace) = workspace_weak.upgrade() else {
                    break;
                };
                let selected = workspace
                    .update_in(cx, |workspace, window, cx| {
                        let Some(session) = load_session(cx) else {
                            return false;
                        };
                        select_merge_review_queue_cursor_file(
                            workspace,
                            &session,
                            &upstream_ref,
                            window,
                            cx,
                        )
                    })
                    .ok()
                    .unwrap_or(false);
                if selected {
                    let Some(workspace) = workspace_weak.upgrade() else {
                        break;
                    };
                    workspace.update_in(cx, |workspace, _window, cx| {
                        let toast = if conflict_count > 0 {
                            format!(
                                "Merge in progress · {conflict_count} conflicts — starting with {start_path}"
                            )
                        } else {
                            format!("Merge in progress — starting with {start_path}")
                        };
                        workspace.show_toast(merge_review_toast(toast), cx);
                    })?;
                    break;
                }
            }
            anyhow::Ok(())
        })
        .detach();
}

fn prepare_merge_review_selected_file(
    diff: &Entity<ProjectDiff>,
    path: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let is_conflict = load_session(cx)
        .and_then(|session| item_for_branch_diff_path(&session, path).map(|item| item.disposition))
        .is_some_and(|disposition| disposition == ReviewDisposition::Conflict);
    if is_conflict {
        diff.update(cx, |diff, cx| {
            diff.ensure_split_diff_for_merge_review(window, cx)
        });
    }
}

fn project_worktree_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .worktree_root_names(cx)
        .next()
        .and_then(|name| project.read(cx).worktree_for_root_name(name, cx))
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

pub fn resolve_active_merge_review_conflict(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    side: ResolveMergeConflictSide,
) -> bool {
    let Some(session) = load_session(cx) else {
        log::warn!("surmount merge review: conflict resolve ignored (no session)");
        return false;
    };
    if !session.focus_layout_active {
        log::warn!("surmount merge review: conflict resolve ignored (workflow not engaged)");
        return false;
    }
    let upstream_ref = session.upstream_ref.clone();
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        log::warn!("surmount merge review: conflict resolve ignored (Branch Diff not open)");
        return false;
    };
    let Some(path) = diff.read(cx).active_file_repo_path(cx) else {
        log::warn!("surmount merge review: conflict resolve ignored (no active file)");
        return false;
    };
    let Some(item) = item_for_branch_diff_path(&session, &path) else {
        log::warn!("surmount merge review: conflict resolve ignored (path not in session)");
        return false;
    };
    if item.disposition != ReviewDisposition::Conflict {
        log::warn!("surmount merge review: conflict resolve ignored (not a conflict file)");
        return false;
    }
    if session.git_mode != git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
        workspace.show_toast(
            merge_review_toast(
                "Start the merge first (git merge origin/main) before resolving conflicts.",
            ),
            cx,
        );
        return false;
    }
    let Some(worktree_root) = project_worktree_root(diff.read(cx).project(), cx) else {
        log::warn!("surmount merge review: conflict resolve ignored (no worktree)");
        return false;
    };
    let path_for_git = path.clone();
    let resolve_side = side.clone();
    let workspace_weak = cx.entity().downgrade();
    window
        .spawn(cx, async move |cx| {
            match resolve_merge_conflict_with_git(&worktree_root, &path_for_git, side, true).await
            {
                Ok(message) => {
                    log::info!("surmount merge review: {message}");
                    git_ui::project_diff::ProjectDiff::refresh(diff.downgrade(), cx)
                    .await
                    .log_err();
                    let Some(workspace) = workspace_weak.upgrade() else {
                        return;
                    };
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            notify_merge_review_ui_changed(workspace, cx);
                            maybe_auto_confirm_merge_review_decision_after_resolve(
                                workspace,
                                window,
                                cx,
                                &path_for_git,
                                resolve_side,
                            );
                            spawn_refresh_merge_review_git_state(window, cx);
                            workspace.show_toast(
                                merge_review_toast(format!(
                                    "{message} Next: click **Next file →** or stage and commit when all conflicts are resolved."
                                )),
                                cx,
                            );
                        })
                        .ok();
                }
                Err(error) => {
                    log::warn!("surmount merge review: conflict resolve failed: {error:#}");
                    let Some(workspace) = workspace_weak.upgrade() else {
                        return;
                    };
                    workspace
                        .update_in(cx, |workspace, _window, cx| {
                            workspace.show_toast(
                                merge_review_toast(format!("Conflict resolve failed: {error}")),
                                cx,
                            );
                        })
                        .ok();
                }
            }
        })
        .detach();
    true
}

pub fn load_manifest_from_worktree(worktree_root: &Path) -> Result<CategoryManifest> {
    let manifest_path = worktree_root.join(MANIFEST_FILE);
    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    CategoryManifest::from_toml(&content)
}

pub fn build_session(
    manifest: &CategoryManifest,
    merge_base: String,
    upstream_ref: String,
    changed_paths: impl IntoIterator<Item = (String, bool)>,
) -> MergeReviewSession {
    let mut items = changed_paths
        .into_iter()
        .map(|(path, is_conflict)| {
            let mut item = manifest.classify_path(&path);
            if is_conflict {
                item.disposition = ReviewDisposition::Conflict;
            }
            item
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_conflict = left.disposition == ReviewDisposition::Conflict;
        let right_conflict = right.disposition == ReviewDisposition::Conflict;
        right_conflict
            .cmp(&left_conflict)
            .then_with(|| left.path.cmp(&right.path))
    });
    MergeReviewSession {
        merge_base,
        upstream_ref,
        items,
        categories_completed: HashSet::default(),
        running_notes: String::new(),
        patterns: Vec::new(),
        pending_summary_path: None,
        pending_summary_format_retries: 0,
        docks_collapsed: Vec::new(),
        focus_layout_active: false,
        pending_conflict_discuss_path: None,
        pending_conflict_synthesize_path: None,
        pending_conflict_record_path: None,
        pending_conflict_todos_path: None,
        worktree_root: None,
        git_mode: git_ui::project_diff::MergeReviewGitMode::PreMerge,
        unmerged_count: 0,
        upstream_sha: None,
        merge_head_sha: None,
        head_sha: None,
        pending_merge_commit_message: None,
        pending_merge_commit_message_capture: false,
        pending_merge_commit_message_format_retries: 0,
        queue_cursor_path: None,
        branch_diff_selected_path: None,
        expanded_categories: HashSet::default(),
        show_all_actionable: false,
    }
}

pub fn maybe_sync_merge_review_todos_from_active_thread(workspace: &Workspace, cx: &mut App) {
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    let Some(thread) = panel.read(cx).active_agent_thread(cx) else {
        return;
    };
    let plan_entries = thread
        .read(cx)
        .plan()
        .entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.status.clone()))
        .collect::<Vec<_>>();
    let Some(mut session) = load_session(cx) else {
        return;
    };
    if sync_merge_review_conflict_todos_from_plan(&mut session, &plan_entries) {
        save_session(cx, &session).log_err();
    }
}

impl MergeReviewSession {
    /// Legacy prototype-tab metric (`Accept Fork` / `Accept Upstream` buttons).
    /// Branch Diff workflow completion uses [`session_summarized_complete`] instead.
    pub fn reviewed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| !matches!(item.verdict, ReviewVerdict::Pending))
            .count()
    }

    pub fn ambiguous_items(&self) -> impl Iterator<Item = &MergeReviewItem> {
        self.items.iter().filter(|item| {
            matches!(item.disposition, ReviewDisposition::Ambiguous)
                && matches!(item.verdict, ReviewVerdict::Pending)
        })
    }

    pub fn conflict_items(&self) -> impl Iterator<Item = &MergeReviewItem> {
        self.items
            .iter()
            .filter(|item| matches!(item.disposition, ReviewDisposition::Conflict))
    }

    pub fn set_verdict(&mut self, path: &str, verdict: ReviewVerdict) {
        if let Some(item) = self.items.iter_mut().find(|item| item.path == path) {
            item.verdict = verdict;
            if matches!(
                verdict,
                ReviewVerdict::Documented
                    | ReviewVerdict::AcceptForkChange
                    | ReviewVerdict::AcceptUpstream
            ) {
                item.disposition = ReviewDisposition::Confirmed;
            }
        }
    }
}

pub fn item_for_path<'a>(
    session: &'a MergeReviewSession,
    path: &str,
) -> Option<&'a MergeReviewItem> {
    let normalized = path.replace('\\', "/");
    session.items.iter().find(|item| item.path == normalized)
}

pub fn item_for_branch_diff_path<'a>(
    session: &'a MergeReviewSession,
    branch_diff_path: &str,
) -> Option<&'a MergeReviewItem> {
    let canonical = canonical_merge_review_path(session, branch_diff_path)
        .unwrap_or_else(|| branch_diff_path.replace('\\', "/"));
    item_for_path(session, &canonical)
}

pub fn session_path_for_branch_diff(
    session: &MergeReviewSession,
    branch_diff_path: &str,
) -> Option<String> {
    canonical_merge_review_path(session, branch_diff_path)
        .or_else(|| item_for_path(session, branch_diff_path).map(|item| item.path.clone()))
}

pub fn session_summarized_complete(session: &MergeReviewSession) -> bool {
    !session.items.is_empty()
        && session
            .items
            .iter()
            .all(|item| item.review_state == MergeReviewState::Summarized)
}

pub fn session_memory_for_prompt(session: &MergeReviewSession, section: &str) -> String {
    let mut parts = Vec::new();
    if !session.running_notes.is_empty() {
        parts.push(format!("Running notes:\n{}", session.running_notes));
    }
    if !session.patterns.is_empty() {
        parts.push(format!(
            "Patterns from earlier files:\n{}",
            session.patterns.join("\n")
        ));
    }
    let section_summaries = session
        .items
        .iter()
        .filter(|item| item.surmount_section == section)
        .filter_map(|item| {
            item.summary
                .as_ref()
                .map(|summary| format!("{}: {summary}", item.path))
        })
        .collect::<Vec<_>>();
    if !section_summaries.is_empty() {
        parts.push(format!(
            "Earlier summaries in this section:\n{}",
            section_summaries.join("\n")
        ));
    }
    parts.join("\n\n")
}

pub fn store_file_summary(
    session: &mut MergeReviewSession,
    path: &str,
    summary: String,
    open_question: bool,
) -> Result<()> {
    let normalized =
        session_path_for_branch_diff(session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let item = session
        .items
        .iter_mut()
        .find(|item| item.path == normalized)
        .with_context(|| format!("unknown merge review path: {normalized}"))?;
    item.summary = Some(summary.clone());
    item.review_state = if open_question {
        MergeReviewState::OpenQuestion
    } else {
        MergeReviewState::Summarized
    };
    if !session.running_notes.is_empty() {
        session.running_notes.push('\n');
    }
    session
        .running_notes
        .push_str(&format!("{normalized}: {summary}"));
    refresh_categories_completed(session);
    Ok(())
}

fn push_pattern_if_new(session: &mut MergeReviewSession, pattern: String) {
    if session.patterns.iter().any(|existing| existing == &pattern) {
        return;
    }
    session.patterns.push(pattern);
}

fn refresh_categories_completed(session: &mut MergeReviewSession) {
    let mut totals: HashMap<String, usize> = HashMap::default();
    let mut reviewed: HashMap<String, usize> = HashMap::default();
    for item in &session.items {
        *totals.entry(item.category_id.clone()).or_default() += 1;
        if item.review_state != MergeReviewState::NotReviewed {
            *reviewed.entry(item.category_id.clone()).or_default() += 1;
        }
    }
    for (category_id, total) in totals {
        if reviewed.get(&category_id).copied().unwrap_or(0) == total {
            session.categories_completed.insert(category_id);
        }
    }
}

pub const MERGE_REVIEW_FORMAT_RETRY_MARKER: &str =
    "missing the required Summary: and Outcome: lines";
pub const MERGE_REVIEW_COMMIT_MESSAGE_FORMAT_RETRY_MARKER: &str =
    "missing the required Merge commit message: line";

const MAX_MERGE_REVIEW_FORMAT_RETRIES: u8 = 2;
const MAX_MERGE_REVIEW_COMMIT_MESSAGE_FORMAT_RETRIES: u8 = 2;

pub fn clear_pending_merge_commit_message_capture(cx: &mut App) {
    let Some(mut session) = load_session(cx) else {
        return;
    };
    if !session.pending_merge_commit_message_capture
        && session.pending_merge_commit_message_format_retries == 0
    {
        return;
    }
    session.pending_merge_commit_message_capture = false;
    session.pending_merge_commit_message_format_retries = 0;
    save_session(cx, &session).log_err();
}

pub fn merge_review_commit_message_format_retry_prompt() -> String {
    format!(
        "Your merge commit message reply is {MERGE_REVIEW_COMMIT_MESSAGE_FORMAT_RETRY_MARKER}. \
         Write a concise merge commit (subject line + optional body).\n\
         End with exactly one line: Merge commit message: <subject and body>\n\
         The human runs `git commit` — do not commit or use todo_write in this turn."
    )
}

pub fn extract_summary_from_agent_reply(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Summary:")
            .or_else(|| trimmed.strip_prefix("summary:"))
            .or_else(|| trimmed.strip_prefix("**Summary:**"))
            .or_else(|| trimmed.strip_prefix("**summary:**"))
            .or_else(|| trimmed.strip_prefix("- Summary:"))
            .or_else(|| trimmed.strip_prefix("- summary:"))
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(str::to_string)
    })
}

fn reply_has_substance_for_format_retry(text: &str) -> bool {
    let non_empty_lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    non_empty_lines >= 2 && text.len() > 80
}

pub fn canonical_merge_review_path(session: &MergeReviewSession, path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    if item_for_path(session, &normalized).is_some() {
        return Some(normalized);
    }
    if !normalized.starts_with('.') {
        let dotted = format!(".{normalized}");
        if item_for_path(session, &dotted).is_some() {
            return Some(dotted);
        }
    } else if let Some(undotted) = normalized.strip_prefix('.') {
        if item_for_path(session, undotted).is_some() {
            return Some(undotted.to_string());
        }
    }
    None
}

pub fn merge_review_format_retry_prompt(path: &str) -> String {
    format!(
        "Your merge review reply for `{path}` is {MERGE_REVIEW_FORMAT_RETRY_MARKER}. \
         Restate in 3–6 sentences, then end with exactly these two lines:\n\
         Summary: …\n\
         Outcome: keep_fork | take_upstream | synthesize | needs_human\n\
         This is a scoped single-file turn — do not use todo_write or plan entries."
    )
}

#[derive(Debug)]
pub enum MergeReviewCaptureOnStop {
    Captured(String),
    FormatRetryRequested { path: String, prompt: String },
    Abandoned(String),
    CommitMessageCaptured,
    CommitMessageFormatRetryRequested { prompt: String },
    CommitMessageCaptureAbandoned,
}

pub fn handle_merge_review_reply_on_stop(
    reply_text: Option<&str>,
    end_turn: bool,
    cx: &mut App,
) -> Option<MergeReviewCaptureOnStop> {
    if !merge_review_workflow_engaged(cx) {
        return None;
    }
    let Some(session) = load_session(cx) else {
        return None;
    };
    let has_pending_capture = session.pending_summary_path.is_some()
        || session.pending_conflict_discuss_path.is_some()
        || session.pending_conflict_synthesize_path.is_some()
        || session.pending_conflict_record_path.is_some()
        || session.pending_conflict_todos_path.is_some()
        || session.pending_merge_commit_message_capture;
    if !has_pending_capture {
        return None;
    }
    if end_turn {
        if session.pending_merge_commit_message_capture {
            if let Some(text) = reply_text {
                if try_capture_merge_review_commit_message_from_reply(text, cx).is_some() {
                    return Some(MergeReviewCaptureOnStop::CommitMessageCaptured);
                }
                if reply_has_substance_for_format_retry(text) {
                    let mut session = load_session(cx)?;
                    if session.pending_merge_commit_message_format_retries
                        < MAX_MERGE_REVIEW_COMMIT_MESSAGE_FORMAT_RETRIES
                    {
                        session.pending_merge_commit_message_format_retries += 1;
                        save_session(cx, &session).log_err();
                        let prompt = merge_review_commit_message_format_retry_prompt();
                        return Some(
                            MergeReviewCaptureOnStop::CommitMessageFormatRetryRequested { prompt },
                        );
                    }
                }
            }
            clear_pending_merge_commit_message_capture(cx);
            return Some(MergeReviewCaptureOnStop::CommitMessageCaptureAbandoned);
        }
        if session.pending_conflict_discuss_path.is_some()
            || session.pending_conflict_synthesize_path.is_some()
        {
            if let Some(text) = reply_text {
                capture_conflict_workshop_reply(cx, text);
            } else {
                clear_pending_conflict_workshop(cx);
            }
            return None;
        }
        if let Some(text) = reply_text {
            if let Some(captured) = try_capture_merge_review_summary_from_reply(text, cx) {
                return Some(MergeReviewCaptureOnStop::Captured(captured));
            }
            if reply_has_substance_for_format_retry(text) {
                let mut session = load_session(cx)?;
                let path = session.pending_summary_path.clone()?;
                if session.pending_summary_format_retries < MAX_MERGE_REVIEW_FORMAT_RETRIES {
                    session.pending_summary_format_retries += 1;
                    save_session(cx, &session).log_err();
                    let prompt = merge_review_format_retry_prompt(&path);
                    return Some(MergeReviewCaptureOnStop::FormatRetryRequested { path, prompt });
                }
            }
        }
    }
    reconcile_stale_pending_summary_capture(cx).map(MergeReviewCaptureOnStop::Abandoned)
}

pub fn extract_suggested_outcome_from_reply(text: &str) -> Option<MergeReviewSuggestedOutcome> {
    if let Some(line) = text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Outcome:")
            .or_else(|| trimmed.strip_prefix("outcome:"))
            .map(str::trim)
    }) {
        return parse_suggested_outcome_label(line);
    }
    let summary = extract_summary_from_agent_reply(text)?;
    parse_suggested_outcome_from_summary(&summary)
}

fn parse_suggested_outcome_label(label: &str) -> Option<MergeReviewSuggestedOutcome> {
    let normalized = label.to_ascii_lowercase().replace(' ', "_");
    match normalized.as_str() {
        "keep_fork" | "keep_ours" | "keep_fork_version" | "ours" => {
            Some(MergeReviewSuggestedOutcome::KeepFork)
        }
        "take_upstream" | "take_theirs" | "upstream" | "theirs" => {
            Some(MergeReviewSuggestedOutcome::TakeUpstream)
        }
        "synthesize" | "combine" | "merge_both" | "use_both" => {
            Some(MergeReviewSuggestedOutcome::Synthesize)
        }
        "needs_human" | "human" | "open_question" => Some(MergeReviewSuggestedOutcome::NeedsHuman),
        _ => None,
    }
}

fn parse_suggested_outcome_from_summary(summary: &str) -> Option<MergeReviewSuggestedOutcome> {
    let lower = summary.to_ascii_lowercase();
    if lower.contains("take upstream") || lower.contains("accept upstream") {
        Some(MergeReviewSuggestedOutcome::TakeUpstream)
    } else if lower.contains("keep fork") || lower.contains("keep ours") {
        Some(MergeReviewSuggestedOutcome::KeepFork)
    } else if lower.contains("synthesize") || lower.contains("combine both") {
        Some(MergeReviewSuggestedOutcome::Synthesize)
    } else if lower.contains("needs human") || lower.contains("open question") {
        Some(MergeReviewSuggestedOutcome::NeedsHuman)
    } else {
        None
    }
}

pub fn extract_decision_outcome_from_reply(text: &str) -> Option<MergeReviewSuggestedOutcome> {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("Decision:")
                .or_else(|| trimmed.strip_prefix("decision:"))
                .map(str::trim)
        })
        .and_then(parse_suggested_outcome_label)
}

pub fn extract_rationale_from_reply(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Rationale:")
            .or_else(|| trimmed.strip_prefix("rationale:"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub fn extract_tests_from_reply(text: &str) -> Vec<String> {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("Tests:")
                .or_else(|| trimmed.strip_prefix("tests:"))
                .map(str::trim)
        })
        .map(|line| {
            line.split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn extract_surmount_note_from_reply(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Surmount:")
            .or_else(|| trimmed.strip_prefix("surmount:"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub fn extract_conflict_decision_from_reply(text: &str) -> Option<MergeReviewConflictDecision> {
    let outcome = extract_decision_outcome_from_reply(text)?;
    let rationale = extract_rationale_from_reply(text).unwrap_or_default();
    if rationale.is_empty() {
        return None;
    }
    Some(MergeReviewConflictDecision {
        outcome,
        rationale,
        surmount_note: extract_surmount_note_from_reply(text),
        test_assertions: extract_tests_from_reply(text),
        test_paths: Vec::new(),
        recorded_at: None,
    })
}

pub fn extract_pattern_from_agent_reply(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Pattern:")
            .or_else(|| trimmed.strip_prefix("pattern:"))
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string)
    })
}

fn reply_indicates_open_question(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("Open question:") || trimmed.starts_with("open question:")
    })
}

pub(crate) fn is_surmount_merge_review_project(project: &Entity<Project>, cx: &App) -> bool {
    surmount_repository(project, cx).is_some()
}

pub fn branch_diff_review_prompt(
    project: &Entity<Project>,
    cx: &App,
    base_ref: &str,
    file_path: Option<&str>,
) -> Option<String> {
    if !is_surmount_merge_review_project(project, cx) {
        return None;
    }
    let session = load_session(cx)?;
    let Some(file_path) = file_path else {
        return Some(surmount_merge_review_prompt(base_ref, None));
    };
    let item = item_for_branch_diff_path(&session, file_path)?;
    let prompt_path = session_path_for_branch_diff(&session, file_path)
        .unwrap_or_else(|| file_path.replace('\\', "/"));
    if item.disposition == ReviewDisposition::Conflict {
        Some(merge_review_conflict_file_prompt(
            &session,
            item,
            &prompt_path,
        ))
    } else {
        Some(merge_review_file_prompt(&session, item, &prompt_path))
    }
}

pub fn merge_review_conflict_file(
    project: &Entity<Project>,
    cx: &App,
    branch_diff_path: &str,
) -> bool {
    if !is_surmount_merge_review_project(project, cx) {
        return false;
    }
    let Some(session) = load_session(cx) else {
        return false;
    };
    item_for_branch_diff_path(&session, branch_diff_path)
        .is_some_and(|item| item.disposition == ReviewDisposition::Conflict)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeReviewConflictSides {
    pub ours_text: String,
    pub theirs_text: String,
    pub working_text: String,
}

pub async fn load_merge_review_conflict_sides(
    worktree_root: &Path,
    path: &str,
) -> Result<MergeReviewConflictSides> {
    let normalized = path.replace('\\', "/");
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    anyhow::ensure!(
        path_is_unmerged(worktree_root, &normalized).await?,
        "{normalized} is not an unmerged conflict path"
    );
    let ours_text = run_git(worktree_root, &["show", &format!(":2:{normalized}")])
        .await
        .context("reading ours stage (:2:)")?;
    let theirs_text = run_git(worktree_root, &["show", &format!(":3:{normalized}")])
        .await
        .context("reading theirs stage (:3:)")?;
    let working_text = std::fs::read_to_string(worktree_root.join(&normalized))
        .with_context(|| format!("reading working tree file {normalized}"))?;
    Ok(MergeReviewConflictSides {
        ours_text,
        theirs_text,
        working_text,
    })
}

pub fn merge_review_conflict_review_content_blocks(
    prompt: &str,
    path: &str,
    sides: &MergeReviewConflictSides,
) -> Vec<agent_client_protocol::schema::v1::ContentBlock> {
    use acp_thread::MentionUri;
    use agent_client_protocol::schema::v1 as acp;

    let normalized = path.replace('\\', "/");
    vec![
        acp::ContentBlock::Text(acp::TextContent::new(prompt)),
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                sides.ours_text.clone(),
                MentionUri::GitDiff {
                    base_ref: format!("fork ours (stage 2) · {normalized}"),
                }
                .to_uri()
                .to_string(),
            )),
        )),
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                sides.theirs_text.clone(),
                MentionUri::GitDiff {
                    base_ref: format!("upstream theirs (stage 3) · {normalized}"),
                }
                .to_uri()
                .to_string(),
            )),
        )),
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                sides.working_text.clone(),
                MentionUri::MergeConflict {
                    file_path: normalized,
                }
                .to_uri()
                .to_string(),
            )),
        )),
    ]
}

pub fn worktree_root_for_merge_review(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    let root = surmount_repository(project, cx)
        .or_else(|| project.read(cx).active_repository(cx))?
        .read(cx)
        .snapshot()
        .work_directory_abs_path
        .to_path_buf();
    if !is_surmount_workspace(root.as_ref()) {
        return None;
    }
    Some(normalize_surmount_worktree_root(root.as_ref()))
}

async fn path_is_unmerged(worktree_root: &Path, path: &str) -> Result<bool> {
    let output = run_git(
        worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await?;
    Ok(output.split('\0').any(|entry| entry == path))
}

pub fn path_has_conflict_markers(worktree_root: &Path, path: &str) -> bool {
    std::fs::read_to_string(worktree_root.join(path.replace('\\', "/")))
        .map(|text| text.lines().any(|line| line.starts_with("<<<<<<< ")))
        .unwrap_or(false)
}

pub fn conflict_workshop_phase_for_item(
    session: &MergeReviewSession,
    item: &MergeReviewItem,
    path: &str,
    worktree_root: Option<&Path>,
) -> Option<git_ui::project_diff::MergeReviewConflictWorkshopPhase> {
    use git_ui::project_diff::MergeReviewConflictWorkshopPhase;

    if item.disposition != ReviewDisposition::Conflict {
        return None;
    }
    let canonical =
        session_path_for_branch_diff(session, path).unwrap_or_else(|| path.replace('\\', "/"));
    if session.pending_conflict_discuss_path.as_deref() == Some(canonical.as_str())
        || session.pending_conflict_synthesize_path.as_deref() == Some(canonical.as_str())
    {
        return Some(MergeReviewConflictWorkshopPhase::Discussing);
    }
    if item.review_state != MergeReviewState::Summarized {
        return Some(MergeReviewConflictWorkshopPhase::NotSummarized);
    }
    let markers_present = worktree_root
        .map(|root| path_has_conflict_markers(root, &canonical))
        .unwrap_or(true);
    if markers_present {
        return Some(MergeReviewConflictWorkshopPhase::DiscussReady);
    }
    if item.conflict_decision.is_none() {
        return Some(MergeReviewConflictWorkshopPhase::RecordDecision);
    }
    if !item.conflict_todos_complete {
        return Some(MergeReviewConflictWorkshopPhase::CompleteTests);
    }
    Some(MergeReviewConflictWorkshopPhase::ReadyToAdvance)
}

pub fn set_pending_conflict_workshop(cx: &mut App, path: &str, synthesize: bool) -> Result<()> {
    let mut session = load_session(cx).context("no merge review session")?;
    let capture_path =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    if synthesize {
        session.pending_conflict_synthesize_path = Some(capture_path);
        session.pending_conflict_discuss_path = None;
    } else {
        session.pending_conflict_discuss_path = Some(capture_path);
        session.pending_conflict_synthesize_path = None;
    }
    save_session(cx, &session)
}

pub fn clear_pending_conflict_workshop(cx: &mut App) {
    let Some(mut session) = load_session(cx) else {
        return;
    };
    if session.pending_conflict_discuss_path.is_none()
        && session.pending_conflict_synthesize_path.is_none()
    {
        return;
    }
    session.pending_conflict_discuss_path = None;
    session.pending_conflict_synthesize_path = None;
    save_session(cx, &session).log_err();
}

pub fn capture_conflict_workshop_reply(cx: &mut App, reply_text: &str) -> bool {
    let Some(mut session) = load_session(cx) else {
        return false;
    };
    let synthesize = session.pending_conflict_synthesize_path.is_some();
    let path = session
        .pending_conflict_synthesize_path
        .clone()
        .or_else(|| session.pending_conflict_discuss_path.clone());
    let Some(path) = path else {
        return false;
    };
    let label = if synthesize { "Synthesize" } else { "Discuss" };
    let excerpt = reply_text.trim();
    if !excerpt.is_empty() {
        if !session.running_notes.is_empty() {
            session.running_notes.push('\n');
        }
        session
            .running_notes
            .push_str(&format!("[{path}] {label}: {excerpt}"));
    }
    if synthesize {
        capture_suggested_outcome_from_reply(&mut session, &path, reply_text);
    }
    session.pending_conflict_discuss_path = None;
    session.pending_conflict_synthesize_path = None;
    save_session(cx, &session).log_err();
    true
}

pub fn merge_review_conflict_discuss_prompt(
    session: &MergeReviewSession,
    item: &MergeReviewItem,
    path: &str,
) -> String {
    format!(
        "{}\n\nAsk clarifying questions if needed. Do not edit files yet.",
        merge_review_conflict_file_prompt(session, item, path)
    )
}

pub fn merge_review_conflict_synthesize_prompt(
    _session: &MergeReviewSession,
    item: &MergeReviewItem,
    path: &str,
) -> String {
    format!(
        "Synthesize a merge resolution for `{path}` under human direction.\n\
         SURMOUNT section: {}\n\
         Prior summary: {}\n\n\
         Three embedded resources follow (self-contained — cite only these):\n\
         1. Fork (ours)  2. Upstream (theirs)  3. Working tree with markers.\n\
         Never open skill files, tool `.rs` sources, host absolute paths, or \
         `fs/read_text_file` outside this worktree — the package is complete.\n\
         Use edit_file only to merge both sides into a clean hunk — never strip conflict markers \
         as the resolution mechanism unless synthesizing both sides.\n\
         After editing, confirm markers are cleared. End with Summary: … and Outcome: synthesize.\n\
         This is a scoped single-file turn — do not use todo_write or plan entries.",
        item.surmount_section,
        item.summary.as_deref().unwrap_or("(none)"),
    )
}

pub fn conflict_todo_titles(path: &str) -> [String; 3] {
    [
        format!("Merge conflict decide: {path}"),
        format!("Merge conflict resolve: {path}"),
        format!("Merge conflict tests: {path}"),
    ]
}

pub fn conflict_todo_stable_id(path: &str, slot: usize) -> String {
    let sanitized = path.replace(['/', '\\'], "--");
    format!("merge-conflict-{sanitized}-{slot}")
}

pub fn sync_conflict_todo_completion_for_item(
    item: &mut MergeReviewItem,
    plan_entries: &[(String, agent_client_protocol::schema::v1::PlanEntryStatus)],
) -> bool {
    if item.conflict_todos_complete || item.conflict_decision.is_none() {
        return false;
    }
    let tracked_ids = if item.conflict_test_todo_ids.len() == 3 {
        item.conflict_test_todo_ids.clone()
    } else {
        conflict_todo_titles(&item.path)
            .into_iter()
            .enumerate()
            .map(|(slot, _)| conflict_todo_stable_id(&item.path, slot))
            .collect()
    };
    let all_complete = tracked_ids.iter().all(|id| {
        plan_entries.iter().any(|(entry_id, status)| {
            entry_id == id
                && *status == agent_client_protocol::schema::v1::PlanEntryStatus::Completed
        })
    });
    if all_complete {
        item.conflict_todos_complete = true;
        return true;
    }
    false
}

pub fn sync_merge_review_conflict_todos_from_plan(
    session: &mut MergeReviewSession,
    plan_entries: &[(String, agent_client_protocol::schema::v1::PlanEntryStatus)],
) -> bool {
    let mut changed = false;
    for item in session.items.iter_mut() {
        if item.disposition == ReviewDisposition::Conflict
            && sync_conflict_todo_completion_for_item(item, plan_entries)
        {
            changed = true;
        }
    }
    changed
}

fn conflict_decisions_in_section(session: &MergeReviewSession, section: &str) -> String {
    let lines = session
        .items
        .iter()
        .filter(|item| item.surmount_section == section && item.conflict_decision.is_some())
        .filter_map(|item| {
            let decision = item.conflict_decision.as_ref()?;
            Some(format!(
                "- {}: {:?} — {}",
                item.path, decision.outcome, decision.rationale
            ))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "Prior conflict decisions in this section:\n{}\n\n",
            lines.join("\n")
        )
    }
}

pub fn apply_record_decision_tool_input(
    session: &mut MergeReviewSession,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input
        .get("path")
        .and_then(|value| value.as_str())
        .context("missing path")?;
    let outcome = input
        .get("outcome")
        .and_then(|value| value.as_str())
        .context("missing outcome")?;
    let rationale = input
        .get("rationale")
        .and_then(|value| value.as_str())
        .context("missing rationale")?;
    let suggested_outcome = match outcome {
        "keep_fork" => MergeReviewSuggestedOutcome::KeepFork,
        "take_upstream" => MergeReviewSuggestedOutcome::TakeUpstream,
        "synthesize" => MergeReviewSuggestedOutcome::Synthesize,
        other => anyhow::bail!("unsupported outcome: {other}"),
    };
    let test_assertions = input
        .get("test_assertions")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let surmount_note = input
        .get("surmount_note")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let decision = MergeReviewConflictDecision {
        outcome: suggested_outcome,
        rationale: rationale.to_string(),
        surmount_note,
        test_assertions,
        test_paths: Vec::new(),
        recorded_at: None,
    };
    store_conflict_decision(session, path, decision)?;
    session.pending_conflict_record_path = None;
    Ok(path.replace('\\', "/"))
}

pub fn handle_merge_review_tool_completion(
    tool_name: &str,
    raw_input: &serde_json::Value,
    cx: &mut App,
) -> Option<String> {
    if !merge_review_workflow_engaged(cx) {
        return None;
    }
    let Some(mut session) = load_session(cx) else {
        return None;
    };
    match tool_name {
        "merge_review_record_decision" => {
            let path = apply_record_decision_tool_input(&mut session, raw_input).ok()?;
            save_session(cx, &session).ok()?;
            Some(path)
        }
        "todo_write" => {
            capture_merge_review_todo_ids_from_tool_input(&mut session, raw_input);
            save_session(cx, &session).ok()?;
            None
        }
        _ => None,
    }
}

fn capture_merge_review_todo_ids_from_tool_input(
    session: &mut MergeReviewSession,
    raw_input: &serde_json::Value,
) {
    let Some(todos) = raw_input.get("todos").and_then(|value| value.as_array()) else {
        return;
    };
    if let Some(path) = session.pending_conflict_todos_path.clone() {
        let titles = conflict_todo_titles(&path);
        let ids = todos
            .iter()
            .filter_map(|todo| {
                let content = todo.get("content")?.as_str()?;
                let id = todo.get("id")?.as_str()?;
                titles
                    .iter()
                    .any(|title| content.contains(title.as_str()))
                    .then(|| id.to_string())
            })
            .collect::<Vec<_>>();
        if !ids.is_empty() {
            if let Some(item) = session.items.iter_mut().find(|item| item.path == path) {
                for id in ids {
                    if !item.conflict_test_todo_ids.contains(&id) {
                        item.conflict_test_todo_ids.push(id);
                    }
                }
            }
            session.pending_conflict_todos_path = None;
        }
    }
    if let Some(path) = session
        .items
        .iter()
        .find(|item| {
            item.review_state == MergeReviewState::OpenQuestion
                && item.open_question_todo_id.is_none()
        })
        .map(|item| item.path.clone())
    {
        if let Some(id) = todos.iter().find_map(|todo| {
            let content = todo.get("content")?.as_str()?;
            let expected = format!("Merge: {path}");
            content
                .contains(expected.as_str())
                .then(|| todo.get("id")?.as_str().map(str::to_string))
        }) {
            if let Some(item) = session.items.iter_mut().find(|item| item.path == path) {
                item.open_question_todo_id = id;
            }
        }
    }
}

pub fn store_conflict_decision(
    session: &mut MergeReviewSession,
    path: &str,
    decision: MergeReviewConflictDecision,
) -> Result<()> {
    let normalized =
        session_path_for_branch_diff(session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let item = session
        .items
        .iter_mut()
        .find(|item| item.path == normalized)
        .context("merge review item not found for conflict decision")?;
    item.conflict_decision = Some(decision.clone());
    let note = format!(
        "Conflict decision {normalized}: {:?} — {}",
        decision.outcome, decision.rationale
    );
    push_pattern_if_new(session, note.clone());
    if !session.running_notes.is_empty() {
        session.running_notes.push('\n');
    }
    session.running_notes.push_str(&note);
    Ok(())
}

pub fn conflict_file_ready_for_advance(
    session: &MergeReviewSession,
    path: &str,
    worktree_root: &Path,
) -> Result<(), &'static str> {
    let normalized =
        session_path_for_branch_diff(session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let Some(item) = item_for_path(session, &normalized) else {
        return Err("file not in merge review session");
    };
    if item.disposition != ReviewDisposition::Conflict {
        return Ok(());
    }
    if item.review_state != MergeReviewState::Summarized {
        return Err("conflict file not summarized yet");
    }
    if path_has_conflict_markers(worktree_root, &normalized) {
        return Err("conflict markers still present — discuss or resolve first");
    }
    if item.conflict_decision.is_none() {
        return Err("record a conflict decision before advancing");
    }
    if !item.conflict_todos_complete {
        return Err("complete conflict test todos before advancing");
    }
    Ok(())
}

pub fn set_pending_summary_capture(cx: &mut App, path: &str) -> Result<()> {
    let mut session = load_session(cx).context("no merge review session")?;
    let capture_path =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    session.pending_summary_path = Some(capture_path);
    session.pending_summary_format_retries = 0;
    save_session(cx, &session)
}

pub fn clear_pending_summary_capture(cx: &mut App) {
    let Some(mut session) = load_session(cx) else {
        return;
    };
    if session.pending_summary_path.is_none() {
        return;
    }
    session.pending_summary_path = None;
    save_session(cx, &session).log_err();
}

pub fn merge_review_agent_still_summarizing(workspace: &Workspace, cx: &App) -> bool {
    if !merge_review_workflow_engaged(cx) {
        return false;
    }
    let Some(session) = load_session(cx) else {
        return false;
    };
    if session.pending_summary_path.is_none() {
        return false;
    }
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return false;
    };
    let Some(thread) = panel.read(cx).active_agent_thread(cx) else {
        return false;
    };
    matches!(
        thread.read(cx).status(),
        acp_thread::ThreadStatus::Generating
    )
}

/// Drop the Summarizing rail state when pending capture exists but the agent is idle
/// (e.g. after restart before session reconcile, or a failed turn).
pub fn finalize_merge_review_branch_diff_controls(
    controls: &mut git_ui::project_diff::MergeReviewBranchDiffControls,
    review_diff_in_flight: bool,
    workspace: &Workspace,
    cx: &App,
) {
    if !controls.awaiting_agent_summary {
        return;
    }
    if review_diff_in_flight || merge_review_agent_still_summarizing(workspace, cx) {
        return;
    }
    controls.awaiting_agent_summary = false;
    if controls.current_file_done {
        return;
    }
    controls.review_diff_ready = true;
    if load_session(cx).is_some() {
        controls.step_label = if controls.is_conflict_file {
            MERGE_REVIEW_STEP_CONFLICT_REVIEW.into()
        } else {
            MERGE_REVIEW_STEP_REVIEW_READY.into()
        };
    }
}

/// Clears `pending_summary_path` when it is stale (restart, cancelled turn, or missing
/// `Summary:` line). Returns the path that was cleared.
pub fn reconcile_stale_pending_summary_capture(cx: &mut App) -> Option<String> {
    let Some(mut session) = load_session(cx) else {
        return None;
    };
    let path = session.pending_summary_path.clone()?;
    if item_for_path(&session, &path)
        .is_some_and(|item| item.review_state == MergeReviewState::Summarized)
    {
        session.pending_summary_path = None;
        save_session(cx, &session).log_err();
        return Some(path);
    }
    session.pending_summary_path = None;
    save_session(cx, &session).log_err();
    log::info!("surmount merge review: cleared stale pending summary for {path}");
    Some(path)
}

fn last_assistant_reply_text(thread: &acp_thread::AcpThread, cx: &App) -> Option<String> {
    thread.entries().iter().rev().find_map(|entry| match entry {
        acp_thread::AgentThreadEntry::AssistantMessage(message) => Some(message.to_markdown(cx)),
        _ => None,
    })
}

fn capture_summary_for_path(
    session: &mut MergeReviewSession,
    path: &str,
    reply_text: &str,
) -> bool {
    let Some(summary) = extract_summary_from_agent_reply(reply_text) else {
        return false;
    };
    let open_question = reply_indicates_open_question(reply_text)
        || extract_suggested_outcome_from_reply(reply_text)
            == Some(MergeReviewSuggestedOutcome::NeedsHuman);
    if !capture_summary_for_path_with_summary(session, path, summary, open_question) {
        return false;
    }
    capture_patterns_from_reply(session, reply_text);
    capture_suggested_outcome_from_reply(session, path, reply_text);
    true
}

fn capture_suggested_outcome_from_reply(
    session: &mut MergeReviewSession,
    path: &str,
    reply_text: &str,
) {
    let Some(outcome) = extract_suggested_outcome_from_reply(reply_text) else {
        return;
    };
    let normalized = path.replace('\\', "/");
    let Some(item) = session
        .items
        .iter_mut()
        .find(|item| item.path == normalized)
    else {
        return;
    };
    item.suggested_outcome = Some(outcome);
}

fn capture_summary_for_path_with_summary(
    session: &mut MergeReviewSession,
    path: &str,
    summary: String,
    open_question: bool,
) -> bool {
    if store_file_summary(session, path, summary, open_question).is_err() {
        return false;
    }
    true
}

fn capture_patterns_from_reply(session: &mut MergeReviewSession, reply_text: &str) {
    if let Some(pattern) = extract_pattern_from_agent_reply(reply_text) {
        push_pattern_if_new(session, pattern);
    }
}

pub fn set_merge_review_queue_cursor(session: &mut MergeReviewSession, path: &str) {
    let normalized = path.replace('\\', "/");
    session.queue_cursor_path =
        Some(canonical_merge_review_path(session, &normalized).unwrap_or(normalized));
}

pub fn set_merge_review_branch_diff_selection(session: &mut MergeReviewSession, path: &str) {
    let normalized = path.replace('\\', "/");
    session.branch_diff_selected_path =
        Some(canonical_merge_review_path(session, &normalized).unwrap_or(normalized));
}

pub fn reconcile_merge_review_branch_diff_selection(session: &mut MergeReviewSession) {
    let Some(path) = session.branch_diff_selected_path.clone() else {
        return;
    };
    let canonical =
        canonical_merge_review_path(session, &path).unwrap_or_else(|| path.replace('\\', "/"));
    if session.items.iter().any(|item| item.path == canonical) {
        session.branch_diff_selected_path = Some(canonical);
    } else {
        session.branch_diff_selected_path = None;
    }
}

fn persist_merge_review_branch_diff_selection(cx: &mut App, path: &str) {
    let Some(mut session) = load_session(cx) else {
        return;
    };
    set_merge_review_branch_diff_selection(&mut session, path);
    save_session(cx, &session).log_err();
}

pub fn first_unreviewed_merge_review_conflict_path(session: &MergeReviewSession) -> Option<String> {
    session
        .items
        .iter()
        .find(|item| {
            item.disposition == ReviewDisposition::Conflict
                && item.review_state == MergeReviewState::NotReviewed
        })
        .map(|item| item.path.clone())
}

pub fn first_unreviewed_merge_review_item_path(session: &MergeReviewSession) -> Option<String> {
    session
        .items
        .iter()
        .find(|item| item.review_state == MergeReviewState::NotReviewed)
        .map(|item| item.path.clone())
}

pub fn first_unreviewed_merge_review_queue_path(session: &MergeReviewSession) -> Option<String> {
    first_unreviewed_merge_review_conflict_path(session)
        .or_else(|| first_unreviewed_merge_review_item_path(session))
}

pub fn merge_review_queue_cursor_valid(session: &MergeReviewSession, cursor: &str) -> bool {
    let canonical =
        canonical_merge_review_path(session, cursor).unwrap_or_else(|| cursor.replace('\\', "/"));
    session.items.iter().any(|item| item.path == canonical)
}

pub fn reconcile_merge_review_queue_cursor(session: &mut MergeReviewSession) {
    let Some(cursor) = session.queue_cursor_path.clone() else {
        return;
    };
    if merge_review_queue_cursor_valid(session, &cursor) {
        return;
    }
    session.queue_cursor_path = None;
    initialize_merge_review_queue_cursor(session);
}

fn merge_review_resume_branch_diff_path(session: &MergeReviewSession) -> Option<String> {
    session
        .branch_diff_selected_path
        .clone()
        .or_else(|| session.queue_cursor_path.clone())
}

pub fn initialize_merge_review_queue_cursor(session: &mut MergeReviewSession) {
    if session.git_mode != git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
        return;
    }
    if session
        .queue_cursor_path
        .as_ref()
        .is_some_and(|cursor| merge_review_queue_cursor_valid(session, cursor))
    {
        return;
    }
    if let Some(path) = first_unreviewed_merge_review_queue_path(session) {
        set_merge_review_queue_cursor(session, &path);
    }
}

/// True when git refresh observes merge engagement: PreMerge→MergeInProgress, or
/// first appearance of unmerged paths while already in MergeInProgress.
pub fn merge_review_merge_started(
    prev_mode: git_ui::project_diff::MergeReviewGitMode,
    prev_unmerged: u32,
    session: &MergeReviewSession,
) -> bool {
    (prev_mode == git_ui::project_diff::MergeReviewGitMode::PreMerge
        && session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress)
        || (session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress
            && prev_unmerged == 0
            && session.unmerged_count > 0)
}

pub fn merge_review_progress_label_for_toast(session: &MergeReviewSession) -> String {
    if session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
        if let Some(cursor) = session.queue_cursor_path.as_deref() {
            return merge_review_queue_position_label(session, cursor);
        }
    }
    merge_review_progress_label(session)
}

pub fn merge_review_prepare_label(session: &MergeReviewSession) -> String {
    format!(
        "{} · {} changed files",
        MERGE_REVIEW_STEP_PREPARE,
        session.items.len()
    )
}

pub fn merge_review_queue_position_label(session: &MergeReviewSession, path: &str) -> String {
    let canonical =
        canonical_merge_review_path(session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let total = session.items.len();
    let position = session
        .items
        .iter()
        .position(|item| item.path == canonical)
        .map(|index| index + 1)
        .unwrap_or(0);
    format!("File {position}/{total} · {canonical}")
}

pub fn merge_review_queue_locked(cx: &App) -> bool {
    load_session(cx).is_some_and(|session| {
        session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress
            && session.queue_cursor_path.is_some()
    })
}

pub fn merge_review_out_of_order_selection_toast(path: &str, cx: &App) -> Option<String> {
    if !merge_review_queue_locked(cx) {
        return None;
    }
    let session = load_session(cx)?;
    let cursor = session.queue_cursor_path.as_ref()?;
    let canonical =
        canonical_merge_review_path(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let cursor_canonical =
        canonical_merge_review_path(&session, cursor).unwrap_or_else(|| cursor.replace('\\', "/"));
    if canonical == cursor_canonical {
        return None;
    }
    Some(format!(
        "Merge review is on {}{}",
        merge_review_queue_position_label(&session, cursor),
        MERGE_REVIEW_OUT_OF_ORDER_TOAST_SUFFIX
    ))
}

pub fn merge_review_allow_file_navigation(
    repo_path: &str,
    workspace: Entity<Workspace>,
    cx: &mut App,
) -> bool {
    if let Some(toast) = merge_review_out_of_order_selection_toast(repo_path, cx) {
        // Defer toast: this guard runs from ProjectDiff::move_to_* under an active
        // Workspace lease (queue select / restore). Nested workspace.update panics.
        let toast = merge_review_toast(toast);
        cx.defer(move |cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(toast, cx);
            });
        });
        false
    } else {
        persist_merge_review_branch_diff_selection(cx, repo_path);
        true
    }
}

fn merge_review_queue_search_start_index(
    session: &MergeReviewSession,
    current_path: Option<&str>,
) -> Option<usize> {
    session
        .queue_cursor_path
        .as_deref()
        .and_then(|cursor| {
            let normalized = canonical_merge_review_path(session, cursor)
                .unwrap_or_else(|| cursor.replace('\\', "/"));
            session
                .items
                .iter()
                .position(|item| item.path == normalized)
        })
        .or_else(|| {
            current_path.and_then(|current| {
                let normalized = canonical_merge_review_path(session, current)
                    .unwrap_or_else(|| current.replace('\\', "/"));
                session
                    .items
                    .iter()
                    .position(|item| item.path == normalized)
            })
        })
}

pub fn merge_review_next_file_blocked_message(
    session: &MergeReviewSession,
    current_path: Option<&str>,
) -> Option<String> {
    if session.items.is_empty() {
        return Some("Merge review queue is empty.".into());
    }
    let start_index = merge_review_queue_search_start_index(session, current_path);
    if let Some(index) = start_index {
        if session.items[index].review_state == MergeReviewState::OpenQuestion {
            let label = merge_review_queue_position_label(&session, &session.items[index].path);
            return Some(format!(
                "Cannot advance — {label} has an open question. Resolve it before **Next file →**."
            ));
        }
    }
    let search_from = start_index.map(|index| index + 1).unwrap_or(0);
    for item in session.items.iter().skip(search_from) {
        match item.review_state {
            MergeReviewState::OpenQuestion => {
                let label = merge_review_queue_position_label(&session, &item.path);
                return Some(format!(
                    "Cannot advance — resolve open question at {label} first."
                ));
            }
            MergeReviewState::NotReviewed => return None,
            MergeReviewState::Summarized => {}
        }
    }
    if start_index.is_none() {
        for item in &session.items {
            match item.review_state {
                MergeReviewState::OpenQuestion => {
                    let label = merge_review_queue_position_label(&session, &item.path);
                    return Some(format!(
                        "Cannot advance — resolve open question at {label} first."
                    ));
                }
                MergeReviewState::NotReviewed => return None,
                MergeReviewState::Summarized => {}
            }
        }
    }
    if session_summarized_complete(session) {
        Some("All files reviewed — click **End merge review**.".into())
    } else {
        Some("No more files in the merge review queue.".into())
    }
}

pub fn next_merge_review_file_path(
    session: &MergeReviewSession,
    current_path: Option<&str>,
) -> Option<String> {
    // Re-land before blocked checks: UI lag on an earlier path must still open the
    // cursor even when nothing exists *after* the cursor (last queue item).
    // Only when UI reports a *different* path — None/current==cursor walks past
    // so successive Next-file does not stick on the same path forever.
    if let Some(cursor) = session.queue_cursor_path.as_deref() {
        let normalized = canonical_merge_review_path(session, cursor)
            .unwrap_or_else(|| cursor.replace('\\', "/"));
        if let Some(item) = session.items.iter().find(|item| item.path == normalized) {
            if item.review_state == MergeReviewState::NotReviewed {
                if let Some(current) = current_path {
                    let current_normalized = canonical_merge_review_path(session, current)
                        .unwrap_or_else(|| current.replace('\\', "/"));
                    if current_normalized != normalized {
                        return Some(normalized);
                    }
                }
            }
        }
    }
    if merge_review_next_file_blocked_message(session, current_path).is_some() {
        return None;
    }
    let start_index = merge_review_queue_search_start_index(session, current_path);
    let search_from = start_index.map(|index| index + 1).unwrap_or(0);
    for item in session.items.iter().skip(search_from) {
        if item.review_state == MergeReviewState::NotReviewed {
            return Some(item.path.clone());
        }
    }
    if start_index.is_none() {
        for item in &session.items {
            if item.review_state == MergeReviewState::NotReviewed {
                return Some(item.path.clone());
            }
        }
    }
    None
}

pub fn advance_merge_review_to_next_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    advance_merge_review_to_next_file_with_sync(workspace, window, cx, true)
}

pub fn advance_merge_review_to_next_file_with_sync(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    sync_thread_todos: bool,
) -> bool {
    if sync_thread_todos {
        maybe_sync_merge_review_todos_from_active_thread(workspace, cx);
    }
    let Some(session) = load_session(cx) else {
        log::warn!("surmount merge review: Next file ignored (no session)");
        return false;
    };
    if !session.focus_layout_active {
        log::warn!("surmount merge review: Next file ignored (workflow not engaged)");
        return false;
    }
    let upstream_ref = session.upstream_ref.clone();
    let ui_path = workspace
        .items_of_type::<ProjectDiff>(cx)
        .find_map(|diff| {
            matches!(
                diff.read(cx).diff_base(cx),
                DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
            )
            .then(|| diff.read(cx).active_file_repo_path(cx))
        })
        .flatten()
        .and_then(|path| canonical_merge_review_path(&session, &path).or(Some(path)));
    // Prefer queue cursor as logical current so successive Next walks past a cursor
    // when Branch Diff active path is missing or still lagging.
    let logical_current = session
        .queue_cursor_path
        .clone()
        .or_else(|| ui_path.clone());
    if let Some(current_path) = logical_current.as_deref() {
        let fallback_worktree = worktree_root_for_merge_review(&workspace.project(), cx);
        let worktree_root = session
            .worktree_root
            .as_deref()
            .map(Path::new)
            .or_else(|| fallback_worktree.as_deref().map(Path::new));
        if let Some(worktree_root) = worktree_root {
            if let Err(reason) =
                conflict_file_ready_for_advance(&session, current_path, worktree_root)
            {
                log::warn!("surmount merge review: Next file blocked for {current_path}: {reason}");
                workspace.show_toast(merge_review_toast(reason), cx);
                return false;
            }
        }
    }
    let Some(next_path) = next_merge_review_file_path(&session, logical_current.as_deref()) else {
        let message = merge_review_next_file_blocked_message(&session, logical_current.as_deref())
            .unwrap_or_else(|| "Could not advance to the next file.".into());
        log::warn!("surmount merge review: Next file blocked: {message}");
        workspace.show_toast(merge_review_toast(message), cx);
        return false;
    };
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        log::warn!("surmount merge review: Next file ignored (Branch Diff not open)");
        workspace.show_toast(
            merge_review_toast(
                "Could not open the next file — open Branch Diff for this merge review first.",
            ),
            cx,
        );
        return false;
    };
    let previous_cursor = session.queue_cursor_path;
    if let Some(mut session) = load_session(cx) {
        set_merge_review_queue_cursor(&mut session, &next_path);
        set_merge_review_branch_diff_selection(&mut session, &next_path);
        save_session(cx, &session).log_err();
    }
    let selected = diff.update(cx, |diff, cx| {
        diff.move_to_repo_relative_path(&next_path, window, cx)
    });
    if selected {
        log::info!("surmount merge review: advanced to next file {next_path}");
        prepare_merge_review_selected_file(&diff, &next_path, window, cx);
        workspace.activate_item(&diff, true, true, window, cx);
        focus_merge_review_after_next_file(&diff, window, cx);
        notify_merge_review_ui_changed(workspace, cx);
    } else {
        log::warn!(
            "surmount merge review: Next file could not open {next_path} in Branch Diff multibuffer"
        );
        if let Some(mut session) = load_session(cx) {
            session.queue_cursor_path = previous_cursor;
            save_session(cx, &session).log_err();
        }
        workspace.show_toast(
            merge_review_toast(format!(
                "Could not open {next_path} in Branch Diff — ensure the file is in the diff list."
            )),
            cx,
        );
    }
    selected
}

pub fn merge_review_summary_saved_toast(path: &str, advanced_to_next: bool, cx: &App) -> String {
    let position = load_session(cx)
        .map(|session| merge_review_queue_position_label(&session, path))
        .unwrap_or_else(|| "?/?".to_string());
    if advanced_to_next {
        format!("{position} summarized — advanced. Click green Review Diff in the rail.")
    } else if load_session(cx).is_some_and(|session| session_summarized_complete(&session)) {
        format!("{position} summarized — all files done. Click End merge review in the rail.")
    } else {
        format!("{position} summarized — click green Next file → in the rail.")
    }
}

pub fn merge_review_summary_capture_toast(
    saved_path: &str,
    _advanced_to_next: bool,
    cx: &App,
) -> Toast {
    let position_label = load_session(cx)
        .map(|session| merge_review_queue_position_label(&session, saved_path))
        .unwrap_or_else(|| "?/?".to_string());
    merge_review_toast(format!("{position_label} summarized.")).autohide()
}

pub fn try_capture_merge_review_summary_from_reply(
    reply_text: &str,
    cx: &mut App,
) -> Option<String> {
    let Some(mut session) = load_session(cx) else {
        return None;
    };
    let Some(path) = session.pending_summary_path.clone() else {
        return None;
    };
    if !capture_summary_for_path(&mut session, &path, reply_text) {
        log::info!("surmount merge review: reply for {path} had no Summary line to capture");
        return None;
    }
    session.pending_summary_path = None;
    session.pending_summary_format_retries = 0;
    if save_session(cx, &session).is_err() {
        return None;
    }
    log::info!("surmount merge review: captured summary for {path}");
    Some(path)
}

/// Set pending capture for `path` and apply `reply_text` through production summary capture.
///
/// Pending path is set via `set_pending_summary_capture` (same canonicalization as Review Diff).
/// Returns the captured path on success. Missing `Summary:` (or other capture failure) returns
/// `Err` with item not `Summarized` and `pending_summary_path` still set (retries reset to 0);
/// format-retry increment stays on the on-stop path.
pub fn apply_merge_review_agent_reply_for_path(
    path: &str,
    reply_text: &str,
    cx: &mut App,
) -> Result<String> {
    set_pending_summary_capture(cx, path)
        .context("persist pending summary for agent reply inject")?;

    try_capture_merge_review_summary_from_reply(reply_text, cx)
        .with_context(|| format!("summary capture failed for {path}"))
}

/// Fixed agent-shaped reply for dogfood synthetic capture (hard Decide spine, no live Grok).
pub const MERGE_REVIEW_DOGFOOD_SYNTHETIC_SUMMARY_REPLY: &str = "\
Dogfood synthetic conflict summary for fixture path.\n\
The ours and theirs sides diverge on a single line; product capture only needs format lines.\n\
Summary: Dogfood fixture conflict between ours and theirs on the active file.\n\
Outcome: synthesize\n\
Decision: synthesize\n\
Rationale: synthetic inject exercises production Summary capture without a live model.\n\
Tests: markers cleared after product resolve path when used.\n";

/// Apply a dogfood synthetic Summary:/Outcome: reply through the **same** capture path as agent stop.
///
/// Uses `pending_summary_path` when set (after Review Diff); otherwise the queue cursor / first
/// unreviewed conflict path. Returns the captured path.
pub fn apply_merge_review_dogfood_synthetic_summary(cx: &mut App) -> Result<String> {
    let session = load_session(cx).context("no merge review session")?;
    anyhow::ensure!(
        session.focus_layout_active,
        "merge review workflow not engaged"
    );

    let path = if let Some(pending) = session.pending_summary_path.clone() {
        pending
    } else if let Some(cursor) = session
        .queue_cursor_path
        .as_ref()
        .and_then(|p| canonical_merge_review_path(&session, p))
    {
        cursor
    } else {
        first_unreviewed_merge_review_queue_path(&session)
            .context("no pending or unreviewed merge-review path for synthetic summary")?
    };

    let captured = apply_merge_review_agent_reply_for_path(
        &path,
        MERGE_REVIEW_DOGFOOD_SYNTHETIC_SUMMARY_REPLY,
        cx,
    )?;
    log::info!("surmount merge review: dogfood synthetic summary capture ok path={captured}");
    Ok(captured)
}

fn inject_merge_review_dogfood_summary(
    workspace: &mut Workspace,
    _: &InjectMergeReviewDogfoodSummary,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match apply_merge_review_dogfood_synthetic_summary(cx) {
        Ok(path) => {
            notify_merge_review_ui_changed(workspace, cx);
            workspace.show_toast(
                merge_review_toast(format!("Dogfood: captured synthetic summary for {path}")),
                cx,
            );
        }
        Err(error) => {
            log::warn!("surmount merge review: dogfood synthetic summary inject failed: {error:#}");
            workspace.show_toast(
                merge_review_toast(format!("Dogfood synthetic summary failed: {error:#}")),
                cx,
            );
        }
    }
}

pub fn try_capture_merge_review_summary(
    thread: &acp_thread::AcpThread,
    cx: &mut App,
) -> Option<String> {
    let Some(text) = last_assistant_reply_text(thread, cx) else {
        return None;
    };
    try_capture_merge_review_summary_from_reply(&text, cx)
}

pub fn notify_merge_review_ui_changed(workspace: &Workspace, cx: &mut App) {
    notify_merge_review_ui_changed_with_sync(workspace, cx, true)
}

pub fn notify_merge_review_ui_changed_with_sync(
    workspace: &Workspace,
    cx: &mut App,
    sync_thread_todos: bool,
) {
    if sync_thread_todos {
        maybe_sync_merge_review_todos_from_active_thread(workspace, cx);
    }
    let project_diffs = workspace
        .items_of_type::<ProjectDiff>(cx)
        .collect::<Vec<_>>();
    for project_diff in project_diffs {
        project_diff.update(cx, |_, cx| cx.notify());
    }
}

pub fn save_session(cx: &mut App, session: &MergeReviewSession) -> Result<()> {
    let json = serde_json::to_string(session)?;
    crate::thread_metadata_store::ThreadMetadataStore::global(cx).update(cx, |store, _| {
        store.save_global_json(SESSION_STORAGE_KEY, &json)
    })?;
    Ok(())
}

pub fn load_session(cx: &App) -> Option<MergeReviewSession> {
    let json = crate::thread_metadata_store::ThreadMetadataStore::global(cx)
        .read(cx)
        .load_global_json(SESSION_STORAGE_KEY)?;
    serde_json::from_str(&json).ok()
}

pub fn clear_merge_review_session(cx: &mut App) -> Result<()> {
    crate::thread_metadata_store::ThreadMetadataStore::global(cx)
        .update(cx, |store, _| store.delete_global(SESSION_STORAGE_KEY))?;
    Ok(())
}

/// Ends the merge review session immediately (no note modal).
///
/// Per plan, only **Record decision** opens the optional note popover; **End merge review** is
/// immediate by design so finishing the workflow stays fast.
pub fn end_merge_review_workflow(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = load_session(cx) else {
        workspace.show_toast(merge_review_toast("No active merge review session."), cx);
        return;
    };
    log::info!(
        "surmount merge review: ending session ({}/{} summarized, {} docks to restore)",
        session.reviewed_count(),
        session.items.len(),
        session.docks_collapsed.len()
    );
    restore_merge_review_collapsed_docks(workspace, &session, window, cx);
    if let Err(error) = clear_merge_review_session(cx) {
        log::error!("surmount merge review: failed to clear session: {error:#}");
    }
    notify_merge_review_ui_changed(workspace, cx);
    workspace.show_toast(merge_review_toast(MERGE_REVIEW_ENDED_TOAST), cx);
}

pub struct MergeReviewView {
    session: MergeReviewSession,
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    first_render_logged: bool,
}

impl MergeReviewView {
    pub fn new(
        session: MergeReviewSession,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session,
            focus_handle: cx.focus_handle(),
            _workspace: workspace,
            first_render_logged: false,
        }
    }

    fn persist_session(&self, cx: &mut Context<Self>) {
        if save_session(cx, &self.session).is_err() {
            log::error!("failed to persist merge review session");
        }
    }

    fn set_item_verdict(&mut self, path: &str, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        self.session.set_verdict(path, verdict);
        self.persist_session(cx);
        cx.notify();
    }

    pub fn open_merge_review_workflow(
        workspace: &mut Workspace,
        session: MergeReviewSession,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let item_count = session.items.len();
        let ambiguous_count = session.ambiguous_items().count();
        let conflict_count = session.conflict_items().count();
        log::info!(
            "surmount merge review: opening workflow ({} items, {} shared-upstream guesses, {} conflicts)",
            item_count,
            ambiguous_count,
            conflict_count
        );
        let mut session = session;
        session.focus_layout_active = true;
        if let Some(repo) = surmount_repository(&project, cx).or_else(|| {
            project.read(cx).active_repository(cx).filter(|repo| {
                is_surmount_workspace(repo.read(cx).snapshot().work_directory_abs_path.as_ref())
            })
        }) {
            session.worktree_root = Some(
                repo.read(cx)
                    .snapshot()
                    .work_directory_abs_path
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if let Err(error) = save_session(cx, &session) {
            log::error!("surmount merge review: failed to persist session: {error:#}");
        }
        ensure_merge_review_focus_layout(workspace, &mut session, window, cx);
        if let Err(error) = save_session(cx, &session) {
            log::error!(
                "surmount merge review: failed to persist session after focus layout: {error:#}"
            );
        }
        let upstream_ref: SharedString = session.upstream_ref.clone().into();
        let repository = surmount_repository(&project, cx).or_else(|| {
            project.read(cx).active_repository(cx).filter(|repo| {
                let snapshot = repo.read(cx).snapshot();
                is_surmount_workspace(snapshot.work_directory_abs_path.as_ref())
            })
        });
        if let Some(repository) = repository {
            ProjectDiff::open_against_base_ref(
                workspace,
                project,
                repository,
                upstream_ref,
                window,
                cx,
            );
            log::info!(
                "surmount merge review: opened Branch Diff against {}",
                session.upstream_ref
            );
        } else {
            log::warn!("surmount merge review: no surmount repository for Branch Diff");
            workspace.show_toast(
                merge_review_toast(
                    "Merge review: could not find the zed repo — open the project root, not a submodule."
                ),
                cx,
            );
            return;
        }

        let session_for_plan = session;
        let upstream_for_find = session_for_plan.upstream_ref.clone();
        let _ambiguous_for_toast = ambiguous_count;
        let workspace_weak = cx.entity().downgrade();
        let workspace_weak_for_err = workspace_weak.clone();
        window
            .spawn(cx, async move |cx| {
                use std::time::Duration;

                let mut branch_diff_ready = false;
                const MAX_BRANCH_DIFF_ATTEMPTS: u32 = 24;
                for attempt in 0..MAX_BRANCH_DIFF_ATTEMPTS {
                    if attempt > 0 {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                    }
                    let Some(workspace) = workspace_weak.upgrade() else {
                        break;
                    };
                    branch_diff_ready = workspace
                        .update_in(cx, |workspace, window, cx| {
                            reveal_branch_diff_for_merge_review(
                                workspace,
                                &upstream_for_find,
                                window,
                                cx,
                            )
                        })
                        .ok()
                        .unwrap_or(false);
                    if branch_diff_ready {
                        break;
                    }
                }

                let Some(workspace) = workspace_weak.upgrade() else {
                    return anyhow::Ok(());
                };
                workspace.update_in(cx, |workspace, window, cx| {
                    if !branch_diff_ready {
                        log::error!(
                            "surmount merge review: Branch Diff tab did not appear for {}",
                            upstream_for_find
                        );
                        workspace.show_toast(
                            merge_review_toast(
                                "Merge review: Branch Diff did not open — check git panel repo is zed root.",
                            ),
                            cx,
                        );
                    }
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.start_merge_review_plan(&session_for_plan, window, cx);
                        });
                        log::info!("surmount merge review: posted plan to agent thread");
                    }
                    if branch_diff_ready {
                        reveal_branch_diff_for_merge_review(
                            workspace,
                            &upstream_for_find,
                            window,
                            cx,
                        );
                    }
                })?;

                if branch_diff_ready {
                    let Some(workspace) = workspace_weak.upgrade() else {
                        return anyhow::Ok(());
                    };
                    let item_count = session_for_plan.items.len();
                    let upstream_for_focus = upstream_for_find.clone();
                    workspace.update_in(cx, |workspace, window, cx| {
                        let toast = if item_count > 0 {
                            format!(
                                "{MERGE_REVIEW_READY_TOAST} ({item_count} changed files.)"
                            )
                        } else {
                            MERGE_REVIEW_READY_TOAST.to_string()
                        };
                        workspace.show_toast(merge_review_toast(toast), cx);
                        // Product focus AC-A: rail primary Preview merge after chrome ready.
                        focus_merge_review_primary_after_start(
                            workspace,
                            &upstream_for_focus,
                            window,
                            cx,
                        );
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak_for_err, window, cx);
    }

    // `action` is forwarded on multi-window redirect (recursion is intentional).
    #[allow(clippy::only_used_in_recursion)]
    fn start_review(
        workspace: &mut Workspace,
        action: &StartMergeReview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repo) = surmount_repository(&project, cx) else {
            // Multi-window headless: cold-start "empty project" may receive the action
            // while SURMOUNT.md is open in another workspace. Prefer that window.
            let candidates: Vec<_> = workspace
                .app_state()
                .workspace_store
                .read(cx)
                .workspaces()
                .filter_map(|weak| weak.upgrade())
                .filter(|other| other.entity_id() != cx.entity().entity_id())
                .collect();
            for other in candidates {
                let other_project = other.read(cx).project().clone();
                if surmount_repository(&other_project, cx).is_some() {
                    log::info!(
                        "surmount merge review: redirecting StartMergeReview to Surmount workspace"
                    );
                    other.update(cx, |other_workspace, cx| {
                        Self::start_review(other_workspace, action, window, cx);
                    });
                    return;
                }
            }
            log::warn!("surmount merge review: no repository with SURMOUNT.md at root");
            workspace.show_toast(
                merge_review_toast(
                    "Merge review: open the zed project root (SURMOUNT.md), not a submodule.",
                ),
                cx,
            );
            return;
        };
        let worktree_root = normalize_surmount_worktree_root(
            repo.read(cx).snapshot().work_directory_abs_path.as_ref(),
        );
        log::info!(
            "surmount merge review: start requested (worktree={})",
            worktree_root.display()
        );
        workspace.show_toast(merge_review_toast("Starting merge review…"), cx);
        let manifest = match load_manifest_from_worktree(&worktree_root) {
            Ok(manifest) => manifest,
            Err(error) => {
                log::error!("failed to load surmount merge manifest: {error:#}");
                workspace.show_toast(
                    merge_review_toast(format!("Merge review failed: {error:#}")),
                    cx,
                );
                return;
            }
        };
        let workspace_handle = cx.entity().downgrade();
        let workspace_handle_for_task = workspace_handle.clone();
        let project_for_workflow = project.clone();
        let task = window.spawn(cx, async move |cx| {
            let started = std::time::Instant::now();
            // Skip fetch when there is no origin remote (dogfood pin-only fixtures, offline trees).
            match run_git(&worktree_root, &["remote", "get-url", "origin"]).await {
                Ok(_) => {
                    if let Err(error) = run_git(&worktree_root, &["fetch", "origin"]).await {
                        log::warn!("surmount merge review: git fetch origin failed: {error:#}");
                    }
                }
                Err(_) => {
                    log::info!(
                        "surmount merge review: skip git fetch origin (no origin remote; using local refs)"
                    );
                }
            }
            let mut session = populate_session_from_git(
                project,
                &worktree_root,
                manifest,
                DEFAULT_UPSTREAM_REF,
                cx,
            )
            .await?;
            log::info!(
                "surmount merge review: populated {} items ({} ambiguous, {} conflicts) in {:?}",
                session.items.len(),
                session.ambiguous_items().count(),
                session.conflict_items().count(),
                started.elapsed()
            );
            if session.items.is_empty() {
                log::warn!(
                    "surmount merge review: empty queue — merge-base {} vs {}",
                    session.merge_base,
                    session.upstream_ref
                );
            }
            let merge_in_progress_auto_select =
                if session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
                    initialize_merge_review_queue_cursor(&mut session);
                    merge_review_resume_branch_diff_path(&session)
                        .map(|path| (session.upstream_ref.clone(), session.unmerged_count, path))
                } else {
                    None
                };
            if let Some(workspace) = workspace_handle_for_task.upgrade() {
                workspace.update_in(cx, |workspace, window, cx| {
                    if let Some((upstream_ref, conflict_count, start_path)) =
                        merge_in_progress_auto_select
                    {
                        save_session(cx, &session).log_err();
                        Self::open_merge_review_workflow(
                            workspace,
                            session,
                            project_for_workflow.clone(),
                            window,
                            cx,
                        );
                        spawn_select_merge_review_queue_file_when_ready(
                            workspace_handle_for_task.clone(),
                            upstream_ref,
                            conflict_count,
                            start_path,
                            window,
                            cx,
                        );
                    } else {
                        Self::open_merge_review_workflow(
                            workspace,
                            session,
                            project_for_workflow,
                            window,
                            cx,
                        );
                    }
                })?;
            } else {
                log::error!("surmount merge review: workspace dropped before deploy");
            }
            anyhow::Ok(())
        });
        task.detach_and_notify_err(workspace_handle, window, cx);
    }

    fn open_review(
        workspace: &mut Workspace,
        _: &OpenMergeReview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(session) = load_session(cx) {
            let project = workspace.project().clone();
            if let Some(worktree_root) = session.worktree_root.clone() {
                let mut session = session;
                let workspace_weak = cx.entity().downgrade();
                window
                    .spawn(cx, async move |cx| {
                        refresh_merge_review_git_state(&mut session, Path::new(&worktree_root))
                            .await
                            .log_err();
                        reconcile_merge_review_queue_cursor(&mut session);
                        if session.git_mode
                            == git_ui::project_diff::MergeReviewGitMode::MergeInProgress
                        {
                            initialize_merge_review_queue_cursor(&mut session);
                        }
                        let resume_auto_select = merge_review_resume_branch_diff_path(&session)
                            .map(|path| {
                                (session.upstream_ref.clone(), session.unmerged_count, path)
                            });
                        workspace_weak.update_in(cx, |workspace, window, cx| {
                            save_session(cx, &session).log_err();
                            Self::open_merge_review_workflow(
                                workspace, session, project, window, cx,
                            );
                            if let Some((upstream_ref, conflict_count, start_path)) =
                                resume_auto_select
                            {
                                spawn_select_merge_review_queue_file_when_ready(
                                    workspace_weak.clone(),
                                    upstream_ref,
                                    conflict_count,
                                    start_path,
                                    window,
                                    cx,
                                );
                            }
                        })?;
                        anyhow::Ok(())
                    })
                    .detach();
            } else {
                Self::open_merge_review_workflow(workspace, session, project, window, cx);
            }
        } else {
            workspace.show_toast(
                merge_review_toast("No saved merge review session — run Start Merge Review first."),
                cx,
            );
        }
    }

    fn end_review(
        workspace: &mut Workspace,
        _: &EndMergeReview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        end_merge_review_workflow(workspace, window, cx);
    }

    fn next_file(
        workspace: &mut Workspace,
        _: &MergeReviewNextFile,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        advance_merge_review_to_next_file(workspace, window, cx);
    }

    fn resolve_conflict_ours(
        workspace: &mut Workspace,
        _: &ResolveMergeReviewConflictOurs,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !resolve_active_merge_review_conflict(
            workspace,
            window,
            cx,
            ResolveMergeConflictSide::Ours,
        ) {
            workspace.show_toast(
                merge_review_toast(
                    "Could not resolve — pick a conflicted file in Branch Diff first.",
                ),
                cx,
            );
        }
    }

    fn resolve_conflict_theirs(
        workspace: &mut Workspace,
        _: &ResolveMergeReviewConflictTheirs,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !resolve_active_merge_review_conflict(
            workspace,
            window,
            cx,
            ResolveMergeConflictSide::Theirs,
        ) {
            workspace.show_toast(
                merge_review_toast(
                    "Could not resolve — pick a conflicted file in Branch Diff first.",
                ),
                cx,
            );
        }
    }
}

pub fn reconcile_merge_review_session_items(
    session: &mut MergeReviewSession,
    manifest: &CategoryManifest,
    git_paths: &[(String, bool)],
) {
    let mut existing = session
        .items
        .drain(..)
        .map(|item| (item.path.clone(), item))
        .collect::<HashMap<_, _>>();
    let mut items = git_paths
        .iter()
        .map(|(path, is_conflict)| {
            if let Some(mut item) = existing.remove(path) {
                if *is_conflict {
                    item.disposition = ReviewDisposition::Conflict;
                } else if item.disposition == ReviewDisposition::Conflict {
                    let classified = manifest.classify_path(path);
                    item.disposition = classified.disposition;
                }
                item
            } else {
                let mut item = manifest.classify_path(path);
                if *is_conflict {
                    item.disposition = ReviewDisposition::Conflict;
                }
                item
            }
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_conflict = left.disposition == ReviewDisposition::Conflict;
        let right_conflict = right.disposition == ReviewDisposition::Conflict;
        right_conflict
            .cmp(&left_conflict)
            .then_with(|| left.path.cmp(&right.path))
    });
    session.items = items;
    refresh_categories_completed(session);
}

fn reconcile_review_paths_from_git(
    worktree_root: &Path,
    name_status: &str,
    conflict_output: &str,
) -> Vec<(String, bool)> {
    let conflict_paths = conflict_output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    parse_git_diff_name_status(name_status)
        .filter(|(path, _)| is_reviewable_changed_path(worktree_root, path))
        .map(|(path, _)| {
            let is_conflict = conflict_paths.contains(path);
            (path.to_string(), is_conflict)
        })
        .collect()
}

async fn load_merge_review_session_from_git(
    worktree_root: &Path,
    manifest: &CategoryManifest,
    upstream_ref: &str,
) -> Result<MergeReviewSession> {
    let merge_base = run_git(worktree_root, &["merge-base", "HEAD", upstream_ref])
        .await?
        .trim()
        .to_string();
    let name_status = run_git(
        worktree_root,
        &["diff", "--merge-base", upstream_ref, "--name-status", "-z"],
    )
    .await?;
    let conflict_output = run_git(
        worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await
    .unwrap_or_default();
    let paths = reconcile_review_paths_from_git(worktree_root, &name_status, &conflict_output);
    let path_count = paths.len();
    let mut session = build_session(manifest, merge_base, upstream_ref.to_string(), paths);
    session.git_mode = detect_merge_review_git_mode(worktree_root).await;
    session.unmerged_count = count_unmerged_paths(worktree_root).await;
    session.upstream_sha = run_git(worktree_root, &["rev-parse", upstream_ref])
        .await
        .ok()
        .map(|sha| sha.trim().to_string());
    session.head_sha = run_git(worktree_root, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|sha| sha.trim().to_string());
    session.merge_head_sha =
        if session.git_mode == git_ui::project_diff::MergeReviewGitMode::MergeInProgress {
            run_git(
                worktree_root,
                &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
            )
            .await
            .ok()
            .map(|sha| sha.trim().to_string())
        } else {
            None
        };
    session.worktree_root = Some(worktree_root.display().to_string());
    debug_assert_eq!(
        session.items.len(),
        path_count,
        "every changed path must become a review item"
    );
    Ok(session)
}

async fn populate_session_from_git(
    project: Entity<Project>,
    worktree_root: &Path,
    manifest: CategoryManifest,
    upstream_ref: &str,
    _cx: &mut AsyncApp,
) -> Result<MergeReviewSession> {
    let _ = &project;
    load_merge_review_session_from_git(worktree_root, &manifest, upstream_ref).await
}

fn render_merge_review_item_row(
    item: &MergeReviewItem,
    cx: &mut Context<MergeReviewView>,
) -> impl IntoElement {
    let path = item.path.clone();
    let disposition_label = format!("{:?}", item.disposition);
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .child(Label::new(item.path.clone()).size(LabelSize::Small))
        .child(
            Label::new(disposition_label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .when(
            matches!(
                item.disposition,
                ReviewDisposition::Ambiguous | ReviewDisposition::Conflict
            ) && matches!(item.verdict, ReviewVerdict::Pending),
            |this| {
                let path_fork = path.clone();
                let path_upstream = path.clone();
                let path_agent = path.clone();
                this.child(
                    Button::new(
                        element_id_for_path("merge-review-fork", &path_fork),
                        "Accept Fork",
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_item_verdict(&path_fork, ReviewVerdict::AcceptForkChange, cx);
                    })),
                )
                .child(
                    Button::new(
                        element_id_for_path("merge-review-upstream", &path_upstream),
                        "Accept Upstream",
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_item_verdict(&path_upstream, ReviewVerdict::AcceptUpstream, cx);
                    })),
                )
                .child(
                    Button::new(
                        element_id_for_path("merge-review-agent", &path_agent),
                        "Send to Agent",
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_item_verdict(&path_agent, ReviewVerdict::NeedsAgentReview, cx);
                    })),
                )
            },
        )
}

async fn run_git(worktree_root: &Path, args: &[&str]) -> Result<String> {
    let output = smol::process::Command::new("git")
        .current_dir(worktree_root)
        .args(args)
        .output()
        .await
        .context("spawning git command")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

impl EventEmitter<()> for MergeReviewView {}

impl Focusable for MergeReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MergeReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.first_render_logged {
            self.first_render_logged = true;
            log::info!(
                "surmount merge review: first render ({} items, {} actionable)",
                self.session.items.len(),
                pending_action_items(&self.session).len()
            );
        }
        let total = self.session.items.len();
        let reviewed = self.session.reviewed_count();
        let ambiguous_count = self.session.ambiguous_items().count();
        let conflict_count = self.session.conflict_items().count();

        let mut body = v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .gap_3()
            .p_4()
            .child(
                Label::new(format!(
                    "Surmount merge review — {reviewed}/{total} reviewed, {ambiguous_count} ambiguous, {conflict_count} conflicts"
                ))
                .size(LabelSize::Large),
            )
            .child(
                Label::new(format!(
                    "merge-base: {} vs {}",
                    self.session.merge_base, self.session.upstream_ref
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.session.items.is_empty(), |this| {
                this.child(
                    Label::new(
                        "No changed files in this diff range. Run after git merge origin/main, \
                         or confirm the active repository is the zed worktree (not a submodule).",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            });

        let action_items = pending_action_items(&self.session)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if !action_items.is_empty() {
            let visible_count = initial_actionable_visible_count(
                action_items.len(),
                self.session.show_all_actionable,
            );
            body = body.child(Label::new(format!(
                "Needs human review ({})",
                action_items.len()
            )));
            for item in action_items.iter().take(visible_count) {
                body = body.child(render_merge_review_item_row(item, cx));
            }
            if !self.session.show_all_actionable
                && action_items.len() > MAX_INITIAL_ACTIONABLE_ITEMS
            {
                let remaining = action_items.len() - MAX_INITIAL_ACTIONABLE_ITEMS;
                body = body.child(
                    Button::new(
                        "merge-review-show-all-actionable",
                        format!("Show all {remaining} more"),
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.session.show_all_actionable = true;
                        this.persist_session(cx);
                        cx.notify();
                    })),
                );
            }
        }

        for (category_id, section, items) in category_groups(&self.session) {
            let pending_in_category = items
                .iter()
                .filter(|item| {
                    matches!(item.verdict, ReviewVerdict::Pending)
                        && matches!(
                            item.disposition,
                            ReviewDisposition::Ambiguous | ReviewDisposition::Conflict
                        )
                })
                .count();
            let expanded = self.session.expanded_categories.contains(&category_id);
            let toggle_id = category_id.clone();
            let summary = format!(
                "{section} — {} files ({} need review)",
                items.len(),
                pending_in_category
            );
            body = body.child(
                Button::new(
                    element_id_for_path("merge-review-category", &category_id),
                    summary,
                )
                .style(ButtonStyle::Transparent)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.session.expanded_categories.contains(&toggle_id) {
                        this.session.expanded_categories.remove(&toggle_id);
                    } else {
                        this.session.expanded_categories.insert(toggle_id.clone());
                    }
                    this.persist_session(cx);
                    cx.notify();
                })),
            );
            if expanded {
                let visible = items.iter().take(MAX_ITEMS_PER_EXPANDED_CATEGORY);
                for item in visible {
                    body = body.child(render_merge_review_item_row(item, cx));
                }
                if items.len() > MAX_ITEMS_PER_EXPANDED_CATEGORY {
                    body = body.child(
                        Label::new(format!(
                            "… and {} more in this category",
                            items.len() - MAX_ITEMS_PER_EXPANDED_CATEGORY
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    );
                }
            }
        }

        body
    }
}

impl Item for MergeReviewView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Surmount Merge Review".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitMergeConflict).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some("Review upstream merge differences for the Surmount fork".into())
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Surmount Merge Review")
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::None
    }
}

pub fn merge_review_summary_snippet(summary: &str) -> String {
    const MAX_CHARS: usize = 120;
    if summary.chars().count() <= MAX_CHARS {
        summary.to_string()
    } else {
        let mut snippet: String = summary.chars().take(MAX_CHARS).collect();
        snippet.push('…');
        snippet
    }
}

pub struct MergeReviewToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
}

impl MergeReviewToolbar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { project_diff: None }
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self
            .project_diff
            .as_ref()
            .and_then(|project_diff| project_diff.upgrade())
        {
            project_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }

    fn mark_open_question(project_diff: &Entity<ProjectDiff>, cx: &mut App) {
        let Some(path) = project_diff.read(cx).active_file_repo_path(cx) else {
            return;
        };
        let Some(mut session) = load_session(cx) else {
            return;
        };
        let summary = item_for_branch_diff_path(&session, &path)
            .and_then(|item| item.summary.clone())
            .unwrap_or_else(|| "Needs a decision.".into());
        if store_file_summary(&mut session, &path, summary, true)
            .log_err()
            .is_none()
        {
            return;
        }
        if save_session(cx, &session).log_err().is_none() {
            return;
        }
        project_diff.update(cx, |_, cx| cx.notify());
    }
}

impl EventEmitter<ToolbarItemEvent> for MergeReviewToolbar {}

impl ToolbarItemView for MergeReviewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let show = load_session(cx).is_some()
            && active_pane_item
                .and_then(|item| item.act_as::<ProjectDiff>(cx))
                .is_some_and(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Merge { .. }));
        self.project_diff = if show {
            active_pane_item
                .and_then(|item| item.act_as::<ProjectDiff>(cx))
                .map(|entity| entity.downgrade())
        } else {
            None
        };
        if show {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for MergeReviewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self
            .project_diff
            .as_ref()
            .and_then(|project_diff| project_diff.upgrade())
        else {
            return div();
        };
        let Some(session) = load_session(cx) else {
            return div();
        };
        let workflow_engaged = merge_review_workflow_engaged(cx);
        let Some(path) = project_diff.read(cx).active_file_repo_path(cx) else {
            if workflow_engaged {
                return div();
            }
            return h_flex().gap_2().items_center().child(
                Label::new("Select a changed file in Branch Diff")
                    .size(LabelSize::Small)
                    .color(Color::Warning),
            );
        };
        let Some(item) = item_for_branch_diff_path(&session, &path) else {
            if workflow_engaged {
                return div();
            }
            return h_flex().gap_2().items_center().child(
                Label::new("Select a changed file in Branch Diff")
                    .size(LabelSize::Small)
                    .color(Color::Warning),
            );
        };
        let snippet = item
            .summary
            .as_deref()
            .map(merge_review_summary_snippet)
            .unwrap_or_default();
        let show_stuck = item.review_state != MergeReviewState::NotReviewed;
        let worktree_root = session.worktree_root.as_deref().map(Path::new);
        let conflict_phase = (item.disposition == ReviewDisposition::Conflict
            && item.review_state == MergeReviewState::Summarized)
            .then(|| conflict_workshop_phase_for_item(&session, item, &path, worktree_root))
            .flatten();
        let conflict_context = item
            .conflict_decision
            .as_ref()
            .map(|decision| format!("{:?}: {}", decision.outcome, decision.rationale));
        h_flex()
            .gap_2()
            .items_center()
            .max_w(px(360.))
            .when_some(conflict_phase, |this, phase| {
                use git_ui::project_diff::MergeReviewConflictWorkshopPhase;
                let phase_label = match phase {
                    MergeReviewConflictWorkshopPhase::DiscussReady => "Discuss",
                    MergeReviewConflictWorkshopPhase::Discussing => "Discussing",
                    MergeReviewConflictWorkshopPhase::RecordDecision => "Record",
                    MergeReviewConflictWorkshopPhase::CompleteTests => "Tests",
                    MergeReviewConflictWorkshopPhase::ReadyToAdvance => "Ready",
                    MergeReviewConflictWorkshopPhase::NotSummarized => "Summarize",
                };
                let todo_note = if item.conflict_todos_complete {
                    "todos done"
                } else if !item.conflict_test_todo_ids.is_empty()
                    || item.conflict_decision.is_some()
                {
                    "todos pending"
                } else {
                    ""
                };
                this.child(
                    Label::new(format!("Conflict · {phase_label} {todo_note}"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when_some(conflict_context, |this, context| {
                this.child(
                    Label::new(context)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .when(
                item.review_state == MergeReviewState::OpenQuestion,
                |this| {
                    this.child(
                        Label::new(item.review_state.compact_label())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                },
            )
            .when(
                item.review_state == MergeReviewState::Summarized && !snippet.is_empty(),
                |this| {
                    this.child(
                        Label::new(snippet.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                },
            )
            .when(
                item.review_state == MergeReviewState::OpenQuestion && !snippet.is_empty(),
                |this| {
                    this.child(
                        Label::new(snippet)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                },
            )
            .when(show_stuck, |this| {
                this.child(
                    IconButton::new("merge-review-stuck", IconName::CircleHelp)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Mark file stuck"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&MarkMergeReviewOpenQuestion, window, cx);
                        })),
                )
            })
    }
}

fn merge_review_progress_label(session: &MergeReviewSession) -> String {
    let reviewed = session
        .items
        .iter()
        .filter(|item| item.review_state != MergeReviewState::NotReviewed)
        .count();
    format!("{reviewed}/{}", session.items.len())
}

fn merge_review_header_label_for_item(item: &MergeReviewItem) -> Option<String> {
    match item.review_state {
        MergeReviewState::NotReviewed => None,
        MergeReviewState::OpenQuestion => Some(item.review_state.compact_label().to_string()),
        MergeReviewState::Summarized => {
            let snippet = item
                .summary
                .as_deref()
                .map(merge_review_summary_snippet)
                .filter(|snippet| !snippet.is_empty())?;
            Some(snippet)
        }
    }
}

fn merge_review_header_label_for_path(path: &str, cx: &App) -> Option<SharedString> {
    let session = load_session(cx)?;
    let item = item_for_branch_diff_path(&session, path)?;
    merge_review_header_label_for_item(item).map(Into::into)
}

fn merge_review_branch_diff_button_label() -> &'static str {
    MERGE_REVIEW_BRANCH_DIFF_BUTTON
}

fn merge_review_end_branch_diff_button_label() -> &'static str {
    MERGE_REVIEW_END_BRANCH_DIFF_BUTTON
}

fn conflict_outcome_hint_for_toolbar(
    outcome: MergeReviewSuggestedOutcome,
) -> git_ui::project_diff::MergeReviewConflictOutcomeHint {
    use git_ui::project_diff::MergeReviewConflictOutcomeHint;
    match outcome {
        MergeReviewSuggestedOutcome::KeepFork => MergeReviewConflictOutcomeHint::KeepFork,
        MergeReviewSuggestedOutcome::TakeUpstream => MergeReviewConflictOutcomeHint::TakeUpstream,
        MergeReviewSuggestedOutcome::Synthesize => MergeReviewConflictOutcomeHint::Synthesize,
        MergeReviewSuggestedOutcome::NeedsHuman => MergeReviewConflictOutcomeHint::NeedsHuman,
    }
}

fn merge_review_conflict_cleanup_banner(session: &MergeReviewSession) -> Option<SharedString> {
    if session.git_mode != git_ui::project_diff::MergeReviewGitMode::MergeInProgress
        || session.unmerged_count > 0
    {
        return None;
    }
    let has_unreviewed_non_conflict = session.items.iter().any(|item| {
        item.review_state == MergeReviewState::NotReviewed
            && item.disposition != ReviewDisposition::Conflict
    });
    if has_unreviewed_non_conflict {
        Some(MERGE_REVIEW_STEP_CONFLICT_CLEANUP.into())
    } else {
        None
    }
}

fn merge_review_step_label_or_conflict_cleanup(
    session: &MergeReviewSession,
    step_label: SharedString,
) -> SharedString {
    if !step_label.is_empty() {
        return step_label;
    }
    merge_review_conflict_cleanup_banner(session).unwrap_or(step_label)
}

fn conflict_workshop_step_label(
    phase: Option<git_ui::project_diff::MergeReviewConflictWorkshopPhase>,
) -> SharedString {
    use git_ui::project_diff::MergeReviewConflictWorkshopPhase;
    match phase {
        Some(MergeReviewConflictWorkshopPhase::DiscussReady) => "Discuss conflict".into(),
        Some(MergeReviewConflictWorkshopPhase::Discussing) => "Discussing conflict…".into(),
        Some(MergeReviewConflictWorkshopPhase::RecordDecision) => "Record decision".into(),
        Some(MergeReviewConflictWorkshopPhase::CompleteTests) => "Complete conflict tests".into(),
        Some(MergeReviewConflictWorkshopPhase::ReadyToAdvance) => {
            MERGE_REVIEW_STEP_NEXT_FILE.into()
        }
        _ => MERGE_REVIEW_STEP_CONFLICT_RESOLVE.into(),
    }
}

fn merge_review_branch_diff_controls(
    file_selected: bool,
    selected_path: Option<&str>,
    cx: &App,
) -> git_ui::project_diff::MergeReviewBranchDiffControls {
    let mut controls = git_ui::project_diff::MergeReviewBranchDiffControls {
        workflow_active: false,
        progress_label: SharedString::default(),
        step_label: SharedString::default(),
        review_diff_ready: file_selected,
        current_file_done: false,
        is_conflict_file: false,
        show_conflict_resolution: false,
        suggested_outcome: None,
        awaiting_agent_summary: false,
        conflict_workshop_phase: None,
        git_mode: git_ui::project_diff::MergeReviewGitMode::PreMerge,
        unmerged_count: 0,
    };
    if !merge_review_workflow_engaged(cx) {
        return controls;
    }
    controls.workflow_active = true;
    let Some(session) = load_session(cx) else {
        return controls;
    };
    controls.git_mode = session.git_mode;
    controls.unmerged_count = session.unmerged_count;
    if session.git_mode == git_ui::project_diff::MergeReviewGitMode::PreMerge {
        controls.progress_label = merge_review_prepare_label(&session).into();
        controls.step_label = SharedString::default();
        controls.review_diff_ready = false;
        return controls;
    }
    let Some(path) = selected_path else {
        if let Some(cursor) = session.queue_cursor_path.as_deref() {
            controls.progress_label = merge_review_queue_position_label(&session, cursor).into();
        } else {
            controls.progress_label = merge_review_progress_label(&session).into();
        }
        controls.step_label =
            merge_review_step_label_or_conflict_cleanup(&session, SharedString::default());
        return controls;
    };
    let canonical_path =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    controls.progress_label = merge_review_queue_position_label(&session, &canonical_path).into();
    if session.pending_summary_path.as_deref() == Some(canonical_path.as_str()) {
        controls.awaiting_agent_summary = true;
        controls.review_diff_ready = false;
        controls.step_label = MERGE_REVIEW_STEP_SUMMARIZING.into();
        if let Some(item) = item_for_branch_diff_path(&session, path) {
            controls.is_conflict_file = item.disposition == ReviewDisposition::Conflict;
        }
        return controls;
    }
    let Some(item) = item_for_branch_diff_path(&session, path) else {
        controls.step_label =
            merge_review_step_label_or_conflict_cleanup(&session, SharedString::default());
        return controls;
    };
    controls.is_conflict_file = item.disposition == ReviewDisposition::Conflict;
    controls.suggested_outcome = item
        .suggested_outcome
        .map(conflict_outcome_hint_for_toolbar);
    let worktree_root = session.worktree_root.as_deref().map(Path::new);
    controls.conflict_workshop_phase =
        conflict_workshop_phase_for_item(&session, item, path, worktree_root);
    if controls.is_conflict_file && item.review_state == MergeReviewState::Summarized {
        controls.show_conflict_resolution = session.git_mode
            == git_ui::project_diff::MergeReviewGitMode::MergeInProgress
            && matches!(
                controls.conflict_workshop_phase,
                Some(git_ui::project_diff::MergeReviewConflictWorkshopPhase::DiscussReady)
                    | Some(git_ui::project_diff::MergeReviewConflictWorkshopPhase::Discussing)
                    | None
            );
        controls.current_file_done = true;
        controls.review_diff_ready = false;
        controls.step_label = conflict_workshop_step_label(controls.conflict_workshop_phase);
        if controls.conflict_workshop_phase
            != Some(git_ui::project_diff::MergeReviewConflictWorkshopPhase::NotSummarized)
        {
            return controls;
        }
    }
    if item.review_state == MergeReviewState::Summarized {
        controls.current_file_done = true;
        controls.review_diff_ready = false;
        controls.step_label = MERGE_REVIEW_STEP_NEXT_FILE.into();
        return controls;
    }
    if file_selected {
        controls.review_diff_ready = true;
        controls.step_label = if controls.is_conflict_file {
            MERGE_REVIEW_STEP_CONFLICT_REVIEW.into()
        } else {
            MERGE_REVIEW_STEP_REVIEW_READY.into()
        };
    } else {
        controls.step_label =
            merge_review_step_label_or_conflict_cleanup(&session, SharedString::default());
    }
    controls
}

pub fn init(cx: &mut App) {
    git_ui::project_diff::set_merge_review_header_label(merge_review_header_label_for_path);
    git_ui::project_diff::set_merge_review_session_active(merge_review_session_active);
    git_ui::project_diff::set_merge_review_branch_diff_controls(merge_review_branch_diff_controls);
    git_ui::project_diff::set_merge_review_branch_diff_button_label(
        merge_review_branch_diff_button_label,
    );
    git_ui::project_diff::set_merge_review_end_branch_diff_button_label(
        merge_review_end_branch_diff_button_label,
    );
    git_ui::project_diff::set_merge_review_step_rail_renderer(
        crate::merge_review_step_rail::render_merge_review_step_rail,
    );
    git_ui::project_diff::set_merge_review_finalize_controls(
        finalize_merge_review_branch_diff_controls,
    );
    git_ui::project_diff::set_merge_review_file_navigation_guard(
        merge_review_allow_file_navigation,
    );
    cx.observe_new(|_workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !merge_review_workflow_engaged(cx) {
            return;
        }
        let workspace_weak = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let _ = workspace_weak.update_in(cx, |workspace, window, cx| {
                    restore_merge_review_workspace_layout(workspace, window, cx);
                });
            })
            .detach();
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let git_store = workspace.project().read(cx).git_store().clone();
        // subscribe_in already leases Workspace for the callback. Do not call
        // workspace.update / update_in again here (entity_map double-lease panic).
        cx.subscribe_in(
            &git_store,
            window,
            move |_workspace, _, event, window, cx| {
                let git_changed = matches!(
                    event,
                    GitStoreEvent::ConflictsUpdated
                        | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, _)
                );
                if !git_changed || !merge_review_session_active(cx) {
                    return;
                }
                spawn_refresh_merge_review_git_state(window, cx);
            },
        )
        .detach();
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(MergeReviewView::start_review);
        workspace.register_action(MergeReviewView::open_review);
        workspace.register_action(MergeReviewView::end_review);
        workspace.register_action(MergeReviewView::next_file);
        workspace.register_action(MergeReviewView::resolve_conflict_ours);
        workspace.register_action(MergeReviewView::resolve_conflict_theirs);
        workspace.register_action(inject_merge_review_dogfood_summary);
        workspace.register_action(|workspace, _: &DiscussMergeReviewConflict, window, cx| {
            dispatch_merge_review_conflict_workshop(workspace, window, cx, false, None);
        });
        workspace.register_action(|workspace, _: &SynthesizeMergeReviewConflict, window, cx| {
            dispatch_merge_review_conflict_workshop(workspace, window, cx, true, None);
        });
        workspace.register_action(
            |workspace, _: &RecordMergeReviewConflictDecision, window, cx| {
                record_merge_review_conflict_decision(workspace, window, cx);
            },
        );
        workspace.register_action(
            |workspace, _: &ConfirmMergeReviewDecisionKeepFork, window, cx| {
                open_merge_review_note_modal(
                    workspace,
                    window,
                    cx,
                    "Keep fork",
                    "Rationale or caveats to record?",
                    MergeReviewNoteContinuation::ConfirmDecision(
                        MergeReviewSuggestedOutcome::KeepFork,
                    ),
                );
            },
        );
        workspace.register_action(
            |workspace, _: &ConfirmMergeReviewDecisionTakeUpstream, window, cx| {
                open_merge_review_note_modal(
                    workspace,
                    window,
                    cx,
                    "Take upstream",
                    "Rationale or caveats to record?",
                    MergeReviewNoteContinuation::ConfirmDecision(
                        MergeReviewSuggestedOutcome::TakeUpstream,
                    ),
                );
            },
        );
        workspace.register_action(
            |workspace, _: &ConfirmMergeReviewDecisionSynthesize, window, cx| {
                open_merge_review_note_modal(
                    workspace,
                    window,
                    cx,
                    "Synthesize",
                    "Rationale or caveats to record?",
                    MergeReviewNoteContinuation::ConfirmDecision(
                        MergeReviewSuggestedOutcome::Synthesize,
                    ),
                );
            },
        );
        workspace.register_action(|workspace, _: &OpenMergeReviewConflictTodos, window, cx| {
            open_merge_review_conflict_todos(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &SendReviewToAgent, window, cx| {
            send_merge_review_comments_to_agent(workspace, window, cx, None);
        });
        workspace.register_action(|workspace, _: &DraftMergeReviewSection, window, cx| {
            draft_merge_review_section(workspace, window, cx, None);
        });
        workspace.register_action(|workspace, _: &PreviewMergeReviewMerge, window, cx| {
            open_merge_review_gated_merge_modal(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DraftMergeReviewCommitMessage, window, cx| {
            open_merge_review_commit_message_modal(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &MarkMergeReviewOpenQuestion, _window, cx| {
            let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
                return;
            };
            MergeReviewToolbar::mark_open_question(&project_diff, cx);
        });
    })
    .detach();
}

pub fn merge_review_plan_prompt(session: &MergeReviewSession) -> String {
    let section_lines = category_groups(session)
        .iter()
        .map(|(_, section, items)| format!("- {section}: {} files", items.len()))
        .collect::<Vec<_>>()
        .join("\n");
    let shared_upstream_guess_count = session
        .items
        .iter()
        .filter(|item| item.disposition == ReviewDisposition::Ambiguous)
        .count();
    format!(
        "Merge review session for upstream `{upstream}` into the Surmount fork.\n\
         merge-base: {merge_base}\n\
         {total} files changed; {shared_upstream_guess_count} paths start as shared-with-upstream guesses; \
         {conflicts} merge conflicts.\n\n\
         SURMOUNT sections in this merge:\n{section_lines}\n\n\
         Branch Diff against `{upstream}` is open in the editor.\n\n\
         Native tools (do not run `script/surmount-merge-triage`, bash, or python): \
         `merge_review_triage` for the file list, `merge_review_diff` for per-path hunks, \
         `resolve_merge_conflict` for conflict resolution.\n\n\
         Stay inside this worktree. Do not open skill files, tool source paths, or absolute paths \
         outside the open project — especially not host Surmount paths when the worktree is a dogfood fixture.\n\
         Never call `fs/read_text_file` on skill md, tool `.rs`, or absolute paths outside this worktree.\n\n\
         Your tasks:\n\
         1. Propose an order to review by SURMOUNT section (conflicts first).\n\
         2. When I select a file, summarize the actual diff hunks: what changed, fork vs upstream, \
            how it relates to earlier summaries in this session.\n\
         3. Use todo_write only for items you cannot resolve with high confidence — those become Plan Todos.\n\
         4. As summaries accumulate, apply the same reasoning to similar files without asking me again.\n\
         5. Only cite diff text; do not invent changes. Draft SURMOUNT.md prose per section when asked.\n\
         6. For merge conflicts: use `resolve_merge_conflict` with git checkout --ours/--theirs; do not strip conflict markers manually unless both sides must be synthesized.\n\
         7. Read individual file paths only — never treat the repository root directory as a file.",
        upstream = session.upstream_ref,
        merge_base = session.merge_base,
        total = session.items.len(),
        conflicts = session.conflict_items().count(),
    )
}

fn starting_guess_label(disposition: ReviewDisposition) -> &'static str {
    match disposition {
        ReviewDisposition::ForkOwned => "ours — likely intentional fork work",
        ReviewDisposition::Ambiguous => {
            "shared with upstream — show whether this is harmless drift or a mistake"
        }
        ReviewDisposition::BuildConfig => "build / deps — usually routine",
        ReviewDisposition::Conflict => "merge conflict — summarize both sides",
        ReviewDisposition::AutoClear => "likely routine — still cite the diff",
        ReviewDisposition::Confirmed => "already confirmed in this session",
        ReviewDisposition::Deferred => "deferred — explain why it still matters",
    }
}

pub fn merge_review_conflict_file_prompt(
    session: &MergeReviewSession,
    item: &MergeReviewItem,
    path: &str,
) -> String {
    let memory = session_memory_for_prompt(session, &item.surmount_section);
    let prior_decisions = conflict_decisions_in_section(session, &item.surmount_section);
    let memory_section = if memory.is_empty() && prior_decisions.is_empty() {
        String::new()
    } else {
        format!("{prior_decisions}{memory}\n\n")
    };
    format!(
        "This file has an active merge conflict while merging upstream `{upstream}` into the Surmount fork.\n\
         File: {path}\n\
         SURMOUNT section: {section}\n\
         Starting guess: {guess}\n\n\
         {memory_section}\
         Three embedded resources follow (self-contained — cite only these):\n\
         1. Fork (ours, git index stage 2)\n\
         2. Upstream (theirs, git index stage 3)\n\
         3. Working tree with conflict markers\n\n\
         Compare fork vs upstream carefully. Ask clarifying questions if either side is unclear.\n\
         The package is complete: cite only the three embeds (ours / theirs / working).\n\
         Never open skill files, tool `.rs` sources, host Surmount absolute paths, or any path \
         outside this open worktree — do not call `fs/read_text_file` on those.\n\
         Prefer `merge_review_conflict_sides` for structured region metadata if needed.\n\
         After markers are cleared, call `merge_review_record_decision` or `merge_review_verify_conflict_resolved`.\n\
         Write 3–6 concise sentences on what diverged and what resolution fits the fork.\n\
         End with a single line: Summary: …\n\
         Add Outcome: keep_fork | take_upstream | synthesize | needs_human\n\
         Optionally add Decision: …, Rationale: …, and Tests: … (assertion descriptions for follow-up todos).\n\
         Do not resolve yet — never strip conflict markers unless the human directs synthesis.\n\
         This is a scoped single-file turn — do not use todo_write or plan entries.\n\
         Only cite visible conflict text from the embeds.",
        upstream = session.upstream_ref,
        path = path,
        section = item.surmount_section,
        guess = starting_guess_label(item.disposition),
        memory_section = memory_section,
    )
}

pub fn merge_review_file_prompt(
    session: &MergeReviewSession,
    item: &MergeReviewItem,
    path: &str,
) -> String {
    let memory = session_memory_for_prompt(session, &item.surmount_section);
    let memory_section = if memory.is_empty() {
        String::new()
    } else {
        format!("{memory}\n\n")
    };
    format!(
        "Summarize this file for merging upstream `{upstream}` into the Surmount fork.\n\
         File: {path}\n\
         SURMOUNT section: {section}\n\
         Starting guess: {guess}\n\n\
         {memory_section}\
         Read the diff hunks below. Write 3–6 concise sentences: what changed, fork vs upstream, \
         overlap with earlier files in this section, suggested outcome.\n\
         End with a single line: Summary: …\n\
         Add Outcome: keep_fork | take_upstream | synthesize | needs_human\n\
         If this file reveals a reusable rule for similar paths in this merge, add: Pattern: …\n\
         For merge conflicts: compare fork (left) vs upstream (right) in split diff, then the human or you applies resolution via `resolve_merge_conflict` (git checkout --ours/--theirs) — never strip conflict markers manually unless synthesizing both sides.\n\
         This is a scoped single-file turn — do not use todo_write or plan entries.\n\
         Only cite visible diff text.",
        upstream = session.upstream_ref,
        path = path,
        section = item.surmount_section,
        guess = starting_guess_label(item.disposition),
        memory_section = memory_section,
    )
}

pub fn merge_review_open_question_todo_title(path: &str) -> String {
    format!("Merge: {path}")
}

pub fn merge_review_open_question_todo_prompt(item: &MergeReviewItem) -> String {
    let summary = item.summary.as_deref().unwrap_or("Needs a decision.");
    format!(
        "Create exactly one Plan Todo via todo_write for this merge-review open question.\n\
         Title: {}\n\
         Content: SURMOUNT section: {}\n\
         File: {}\n\
         Summary: {summary}\n\
         Status: pending\n\
         Do not edit files — only todo_write.",
        merge_review_open_question_todo_title(&item.path),
        item.surmount_section,
        item.path,
    )
}

pub fn merge_review_section_draft_prompt(session: &MergeReviewSession, section: &str) -> String {
    let summaries = session
        .items
        .iter()
        .filter(|item| item.surmount_section == section)
        .filter(|item| item.review_state == MergeReviewState::Summarized)
        .filter_map(|item| {
            item.summary
                .as_ref()
                .map(|summary| format!("- `{}`: {summary}", item.path))
        })
        .collect::<Vec<_>>();
    let patterns = if session.patterns.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nPatterns from this merge:\n{}",
            session
                .patterns
                .iter()
                .map(|pattern| format!("- {pattern}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let notes_excerpt = if session.running_notes.is_empty() {
        String::new()
    } else {
        let excerpt = session.running_notes.chars().take(2000).collect::<String>();
        format!("\n\nRunning notes excerpt:\n{excerpt}")
    };
    format!(
        "Draft SURMOUNT.md prose for section \"{section}\" from confirmed merge-review summaries.\n\
         Merge base: {}\n\
         Upstream: {}\n\n\
         Confirmed file summaries:\n{}\n\
         {patterns}{notes_excerpt}\n\n\
         Write 2–4 paragraphs suitable for SURMOUNT.md. Cite only the summaries above. \
         Do not use todo_write or edit files in this turn.",
        session.merge_base,
        session.upstream_ref,
        if summaries.is_empty() {
            "(none yet — ask which section to draft)".to_string()
        } else {
            summaries.join("\n")
        },
    )
}

pub fn request_merge_review_open_question_todo(
    workspace: &mut Workspace,
    path: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = load_session(cx) else {
        return;
    };
    let Some(item) = item_for_branch_diff_path(&session, path) else {
        return;
    };
    if item.review_state != MergeReviewState::OpenQuestion {
        return;
    }
    if item.open_question_todo_id.is_some() {
        return;
    }
    let prompt = merge_review_open_question_todo_prompt(item);
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    workspace.open_panel::<AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.send_merge_review_follow_up(prompt, window, cx);
    });
    log::info!("surmount merge review: requested Plan Todo for open question ({path})");
}

pub fn merge_review_send_review_comments_prompt(
    session: &MergeReviewSession,
    path: &str,
    comments: &str,
) -> String {
    let Some(item) =
        item_for_path(session, path).or_else(|| item_for_branch_diff_path(session, path))
    else {
        return format!(
            "This file has an active merge conflict while merging upstream into the Surmount fork.\n\
             File: {path}\n\n\
             Review comments on this diff:\n{comments}\n\n\
             Ask clarifying questions if needed. Do not edit files yet.\n\
             This is a scoped single-file turn — do not use todo_write or plan entries."
        );
    };
    let base = if item.disposition == ReviewDisposition::Conflict {
        merge_review_conflict_discuss_prompt(session, item, path)
    } else {
        merge_review_file_prompt(session, item, path)
    };
    format!(
        "{base}\n\nReview comments on this diff:\n{comments}\n\n\
         Ask clarifying questions if needed. Do not edit files yet."
    )
}

pub fn send_merge_review_comments_to_agent(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    user_note: Option<&str>,
) {
    if !merge_review_workflow_engaged(cx) {
        workspace.show_toast(
            merge_review_toast("Start merge review before sending comments to the agent."),
            cx,
        );
        return;
    }
    let Some(session) = load_session(cx) else {
        return;
    };
    let upstream_ref = session.upstream_ref.clone();
    let Some(project_diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|diff| {
        matches!(
            diff.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        workspace.show_toast(
            merge_review_toast("Open Branch Diff against the merge base first."),
            cx,
        );
        return;
    };
    let Some(path) = project_diff.read(cx).active_file_repo_path(cx) else {
        workspace.show_toast(merge_review_toast("Pick a file in Branch Diff first."), cx);
        return;
    };
    let comments = project_diff.update(cx, |diff, cx| diff.take_review_comments_for_agent(cx));
    if comments.trim().is_empty() {
        workspace.show_toast(merge_review_toast("No review comments to send."), cx);
        return;
    };
    let prompt = merge_review_append_user_note(
        merge_review_send_review_comments_prompt(&session, &path, &comments),
        user_note,
    );
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    workspace.open_panel::<AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.send_merge_review_follow_up(prompt, window, cx);
    });
    notify_merge_review_ui_changed(workspace, cx);
    log::info!("surmount merge review: sent review comments to agent ({path})");
}

pub fn dispatch_merge_review_conflict_workshop(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    synthesize: bool,
    user_note: Option<&str>,
) {
    let Some(session) = load_session(cx) else {
        return;
    };
    let upstream_ref = session.upstream_ref.as_str();
    let Some(path) = workspace
        .items_of_type::<ProjectDiff>(cx)
        .find_map(|diff| {
            matches!(
                diff.read(cx).diff_base(cx),
                DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
            )
            .then(|| diff.read(cx).active_file_repo_path(cx))
        })
        .flatten()
    else {
        workspace.show_toast(
            merge_review_toast("Pick a conflicted file in Branch Diff first."),
            cx,
        );
        return;
    };
    if !merge_review_conflict_file(&workspace.project(), cx, &path) {
        workspace.show_toast(
            merge_review_toast("Active file is not a merge-review conflict."),
            cx,
        );
        return;
    };
    if let Err(error) = set_pending_conflict_workshop(cx, &path, synthesize) {
        log::error!("surmount merge review: conflict workshop pending failed: {error:#}");
        return;
    }
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    workspace.open_panel::<AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.prepare_for_merge_review(window, cx);
    });
    let project = workspace.project().clone();
    let Some(worktree_root) = worktree_root_for_merge_review(&project, cx) else {
        workspace.show_toast(
            merge_review_toast("Could not find Surmount worktree for conflict workshop."),
            cx,
        );
        return;
    };
    let file_path = path;
    let user_note = user_note.map(str::to_string);
    let workspace_weak = cx.entity().downgrade();
    let merge_review_active = merge_review_workflow_engaged(cx);
    window
        .spawn(cx, async move |cx| {
            let sides_result =
                load_merge_review_conflict_sides(worktree_root.as_path(), &file_path).await;
            workspace_weak
                .update_in(cx, |workspace, window, cx| {
                    let sides = match sides_result {
                        Ok(sides) => sides,
                        Err(error) => {
                            log::error!(
                                "surmount merge review: conflict workshop failed for {file_path}: {error:#}"
                            );
                            workspace.show_toast(
                                merge_review_toast(&format!(
                                    "Conflict workshop failed for {file_path}"
                                )),
                                cx,
                            );
                            return;
                        }
                    };
                    let project = workspace.project().clone();
                    let prompt = project
                        .read_with(cx, |_project, cx| {
                            let session = load_session(cx)?;
                            let item = item_for_branch_diff_path(&session, &file_path)?;
                            let base = if synthesize {
                                merge_review_conflict_synthesize_prompt(&session, item, &file_path)
                            } else {
                                merge_review_conflict_discuss_prompt(&session, item, &file_path)
                            };
                            Some(merge_review_append_user_note(base, user_note.as_deref()))
                        })
                        .unwrap_or_else(|| {
                            if synthesize {
                                format!("Synthesize merge conflict resolution for {file_path}")
                            } else {
                                format!("Discuss merge conflict for {file_path}")
                            }
                        });
                    let content_blocks =
                        merge_review_conflict_review_content_blocks(&prompt, &file_path, &sides);
                    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                        return;
                    };
                    let active_thread_ready = merge_review_active
                        && panel.read(cx).active_thread_view(cx).is_some();
                    panel.update(cx, |panel, cx| {
                        if !active_thread_ready {
                            log::warn!(
                                "surmount merge review: conflict workshop skipped (no active thread, file={file_path})"
                            );
                            return;
                        }
                        panel.submit_merge_review_branch_diff(
                            content_blocks,
                            Some(file_path.as_str()),
                            window,
                            cx,
                        );
                    });
                    if merge_review_active {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    }
                })
                .ok();
        })
        .detach();
}

fn conflict_decision_rationale(
    outcome: MergeReviewSuggestedOutcome,
    item: &MergeReviewItem,
    via_git_resolve: bool,
) -> String {
    if via_git_resolve {
        return match outcome {
            MergeReviewSuggestedOutcome::KeepFork => {
                "Resolved by keeping fork version (git checkout --ours).".into()
            }
            MergeReviewSuggestedOutcome::TakeUpstream => {
                "Resolved by taking upstream version (git checkout --theirs).".into()
            }
            MergeReviewSuggestedOutcome::Synthesize => {
                "Resolved by synthesizing both sides under human direction.".into()
            }
            MergeReviewSuggestedOutcome::NeedsHuman => "Needs human judgment.".into(),
        };
    }
    if let Some(summary) = item
        .summary
        .as_deref()
        .filter(|summary| !summary.is_empty())
    {
        return format!("Human confirmed {outcome:?} after workshop review. Summary: {summary}");
    }
    format!("Human confirmed {outcome:?} via merge review conflict workshop.")
}

fn conflict_todo_entry_content(
    slot: usize,
    path: &str,
    item: &MergeReviewItem,
    decision: &MergeReviewConflictDecision,
) -> String {
    let [decide, resolve, tests] = conflict_todo_titles(path);
    let title = match slot {
        0 => decide,
        1 => resolve,
        2 => tests,
        _ => unreachable!(),
    };
    let mut content = format!("{title}\nSURMOUNT section: {}", item.surmount_section);
    if slot == 0 {
        content.push_str(&format!(
            "\nOutcome: {:?}\nRationale: {}",
            decision.outcome, decision.rationale
        ));
    }
    if slot == 2 && !decision.test_assertions.is_empty() {
        content.push_str("\nTests: ");
        content.push_str(&decision.test_assertions.join("; "));
    }
    content
}

pub fn install_merge_review_conflict_todos_for_path(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    path: &str,
) -> bool {
    let Some(mut session) = load_session(cx) else {
        return false;
    };
    let normalized =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let Some(item) = session.items.iter().find(|item| item.path == normalized) else {
        return false;
    };
    let Some(decision) = item.conflict_decision.clone() else {
        return false;
    };
    if item.conflict_test_todo_ids.len() == 3 {
        return false;
    }
    let item_snapshot = item.clone();
    let worktree_root = session.worktree_root.as_deref().map(Path::new);
    let markers_present = worktree_root
        .map(|root| path_has_conflict_markers(root, &normalized))
        .unwrap_or(false);
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return false;
    };
    let Some(thread) = panel.read(cx).active_agent_thread(cx) else {
        return false;
    };
    use agent_client_protocol::schema::v1 as acp;
    let mut plan_entries = thread
        .read(cx)
        .plan()
        .entries
        .iter()
        .map(|entry| {
            acp::PlanEntry::new(
                entry.content.read(cx).source().to_string(),
                entry.priority.clone(),
                entry.status.clone(),
            )
            .meta(Some(acp::Meta::from_iter([(
                "id".into(),
                serde_json::Value::String(entry.id.clone()),
            )])))
        })
        .collect::<Vec<_>>();
    let mut todo_ids = Vec::with_capacity(3);
    for slot in 0..3 {
        let id = conflict_todo_stable_id(&normalized, slot);
        let status = match slot {
            0 => acp::PlanEntryStatus::Completed,
            1 if markers_present => acp::PlanEntryStatus::Pending,
            1 => acp::PlanEntryStatus::Completed,
            _ => acp::PlanEntryStatus::Pending,
        };
        plan_entries.push(
            acp::PlanEntry::new(
                conflict_todo_entry_content(slot, &normalized, &item_snapshot, &decision),
                acp::PlanEntryPriority::Medium,
                status,
            )
            .meta(Some(acp::Meta::from_iter([(
                "id".into(),
                serde_json::Value::String(id.clone()),
            )]))),
        );
        todo_ids.push(id);
    }
    thread.update(cx, |thread, cx| {
        thread.update_plan(acp::Plan::new(plan_entries), cx);
    });
    if let Some(item) = session
        .items
        .iter_mut()
        .find(|item| item.path == normalized)
    {
        item.conflict_test_todo_ids = todo_ids;
    }
    session.pending_conflict_todos_path = None;
    save_session(cx, &session).log_err();
    workspace.open_panel::<AgentPanel>(window, cx);
    window.dispatch_action(Box::new(OpenZedTodosSurface), cx);
    log::info!("surmount merge review: installed conflict Plan Todos ({normalized})");
    true
}

fn confirm_merge_review_decision_for_path(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    path: &str,
    outcome: MergeReviewSuggestedOutcome,
    via_git_resolve: bool,
    human_note: Option<&str>,
) -> bool {
    let Some(mut session) = load_session(cx) else {
        return false;
    };
    let normalized =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    let item = session
        .items
        .iter()
        .find(|item| item.path == normalized)
        .cloned();
    let Some(item) = item else {
        workspace.show_toast(merge_review_toast("File not in merge review session."), cx);
        return false;
    };
    if item.disposition != ReviewDisposition::Conflict {
        workspace.show_toast(
            merge_review_toast("Active file is not a merge-review conflict."),
            cx,
        );
        return false;
    };
    if item.conflict_decision.is_some() {
        workspace.show_toast(
            merge_review_toast("Conflict decision already recorded."),
            cx,
        );
        return false;
    };
    let worktree_root = session.worktree_root.as_deref().map(Path::new);
    if worktree_root
        .map(|root| path_has_conflict_markers(root, &normalized))
        .unwrap_or(true)
    {
        workspace.show_toast(
            merge_review_toast("Clear conflict markers before confirming a decision."),
            cx,
        );
        return false;
    }
    let mut rationale = conflict_decision_rationale(outcome, &item, via_git_resolve);
    if let Some(note) = human_note.map(str::trim).filter(|note| !note.is_empty()) {
        rationale = format!("{rationale}\nHuman note: {note}");
    }
    let decision = MergeReviewConflictDecision {
        outcome,
        rationale,
        surmount_note: None,
        test_assertions: item
            .summary
            .as_deref()
            .map(extract_tests_from_reply)
            .unwrap_or_default(),
        test_paths: Vec::new(),
        recorded_at: None,
    };
    if store_conflict_decision(&mut session, &normalized, decision).is_err() {
        workspace.show_toast(merge_review_toast("Failed to store conflict decision."), cx);
        return false;
    }
    session.pending_conflict_record_path = None;
    save_session(cx, &session).log_err();
    install_merge_review_conflict_todos_for_path(workspace, window, cx, &normalized);
    notify_merge_review_ui_changed(workspace, cx);
    spawn_refresh_merge_review_git_state(window, cx);
    true
}

pub fn confirm_merge_review_decision(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    outcome: MergeReviewSuggestedOutcome,
    human_note: Option<&str>,
) {
    let Some(session) = load_session(cx) else {
        return;
    };
    let upstream_ref = session.upstream_ref.as_str();
    let Some(path) = workspace
        .items_of_type::<ProjectDiff>(cx)
        .find_map(|diff| {
            matches!(
                diff.read(cx).diff_base(cx),
                DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
            )
            .then(|| diff.read(cx).active_file_repo_path(cx))
        })
        .flatten()
    else {
        workspace.show_toast(
            merge_review_toast("Pick a conflicted file in Branch Diff first."),
            cx,
        );
        return;
    };
    confirm_merge_review_decision_for_path(
        workspace, window, cx, &path, outcome, false, human_note,
    );
}

pub fn maybe_auto_confirm_merge_review_decision_after_resolve(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    path: &str,
    side: ResolveMergeConflictSide,
) {
    let outcome = match side {
        ResolveMergeConflictSide::Ours => MergeReviewSuggestedOutcome::KeepFork,
        ResolveMergeConflictSide::Theirs => MergeReviewSuggestedOutcome::TakeUpstream,
    };
    confirm_merge_review_decision_for_path(workspace, window, cx, path, outcome, true, None);
}

pub fn record_merge_review_conflict_decision(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let outcome = load_session(cx)
        .and_then(|session| {
            workspace.items_of_type::<ProjectDiff>(cx).find_map(|diff| {
                let path = diff.read(cx).active_file_repo_path(cx)?;
                item_for_branch_diff_path(&session, &path).and_then(|item| item.suggested_outcome)
            })
        })
        .unwrap_or(MergeReviewSuggestedOutcome::KeepFork);
    confirm_merge_review_decision(workspace, window, cx, outcome, None);
}

pub fn set_pending_conflict_record(cx: &mut App, path: &str) -> Result<()> {
    let mut session = load_session(cx).context("no merge review session")?;
    let capture_path =
        session_path_for_branch_diff(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    session.pending_conflict_record_path = Some(capture_path);
    save_session(cx, &session)
}

pub fn open_merge_review_conflict_todos(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = load_session(cx) else {
        return;
    };
    let upstream_ref = session.upstream_ref.as_str();
    let Some(path) = workspace
        .items_of_type::<ProjectDiff>(cx)
        .find_map(|diff| {
            matches!(
                diff.read(cx).diff_base(cx),
                DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
            )
            .then(|| diff.read(cx).active_file_repo_path(cx))
        })
        .flatten()
    else {
        return;
    };
    maybe_sync_merge_review_todos_from_active_thread(workspace, cx);
    if !install_merge_review_conflict_todos_for_path(workspace, window, cx, &path) {
        workspace.open_panel::<AgentPanel>(window, cx);
        window.dispatch_action(Box::new(OpenZedTodosSurface), cx);
    }
}

pub fn draft_merge_review_section(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    user_note: Option<&str>,
) {
    let Some(session) = load_session(cx) else {
        workspace.show_toast(merge_review_toast("No active merge review session."), cx);
        return;
    };
    let section = workspace
        .items_of_type::<ProjectDiff>(cx)
        .find_map(|diff| {
            matches!(
                diff.read(cx).diff_base(cx),
                DiffBase::Merge { base_ref } if base_ref.as_ref() == session.upstream_ref
            )
            .then(|| diff.read(cx).active_file_repo_path(cx))
        })
        .flatten()
        .and_then(|path| {
            item_for_branch_diff_path(&session, &path).map(|item| item.surmount_section.clone())
        })
        .or_else(|| session.categories_completed.iter().next().cloned());
    let Some(section) = section else {
        workspace.show_toast(
            merge_review_toast(
                "Pick a summarized file in Branch Diff or complete a category first.",
            ),
            cx,
        );
        return;
    };
    let prompt = merge_review_append_user_note(
        merge_review_section_draft_prompt(&session, &section),
        user_note,
    );
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    workspace.open_panel::<AgentPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.send_merge_review_follow_up(prompt, window, cx);
    });
    log::info!("surmount merge review: requested section draft for {section}");
}

pub fn surmount_merge_review_prompt(base_ref: &str, category_id: Option<&str>) -> String {
    let category_hint = category_id
        .map(|id| format!("SURMOUNT section id: {id}. "))
        .unwrap_or_default();
    format!(
        "{category_hint}Summarize this diff for merging upstream `{base_ref}` into the Surmount fork. \
         Explain what changed, fork vs upstream, and suggested outcome. \
         Use todo_write for open questions (Plan Todos), not a parallel list. \
         Prefer upstream for unrelated files; preserve fork intent for fork-owned paths. \
         Only cite visible diff text."
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    const TEST_MANIFEST: &str = r#"
version = 1

[[rules]]
category_id = "agent_ui"
surmount_section = "Agent UI & conversation"
disposition = "fork_owned"
risk = "high"
paths = ["crates/agent_ui/**"]

[[rules]]
category_id = "misc_upstream"
surmount_section = "Misc upstream-touching tweaks"
disposition = "ambiguous"
risk = "medium"
paths = ["crates/editor/**"]

[[rules]]
category_id = "workspace_build"
surmount_section = "Workspace & build config"
disposition = "build_config"
risk = "medium"
paths = ["Cargo.toml"]
"#;

    #[test]
    fn classifies_fork_owned_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let item = manifest.classify_path("crates/agent_ui/src/agent_panel.rs");
        assert_eq!(item.category_id, "agent_ui");
        assert_eq!(item.disposition, ReviewDisposition::ForkOwned);
    }

    #[test]
    fn classifies_ambiguous_misc_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let item = manifest.classify_path("crates/editor/src/editor.rs");
        assert_eq!(item.category_id, "misc_upstream");
        assert_eq!(item.disposition, ReviewDisposition::Ambiguous);
    }

    #[test]
    fn classifies_build_config_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let item = manifest.classify_path("Cargo.toml");
        assert_eq!(item.category_id, "workspace_build");
        assert_eq!(item.disposition, ReviewDisposition::BuildConfig);
    }

    #[test]
    fn uncategorized_paths_default_to_ambiguous() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let item = manifest.classify_path("crates/unknown/src/lib.rs");
        assert_eq!(item.category_id, "uncategorized");
        assert_eq!(item.disposition, ReviewDisposition::Ambiguous);
    }

    #[test]
    fn session_tracks_verdict_updates() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        assert_eq!(session.reviewed_count(), 0);
        session.set_verdict(
            "crates/editor/src/editor.rs",
            ReviewVerdict::AcceptForkChange,
        );
        assert_eq!(session.reviewed_count(), 1);
        assert_eq!(session.items[0].disposition, ReviewDisposition::Confirmed);
    }

    #[test]
    fn session_roundtrips_through_json() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
                ("crates/editor/src/editor.rs".into(), true),
            ],
        );
        let json = serde_json::to_string(&session).unwrap();
        let restored: MergeReviewSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, restored);
        assert_eq!(restored.conflict_items().count(), 1);
    }

    #[test]
    fn parses_null_delimited_name_status_like_git_z() {
        let sample = concat!(
            "M\x00.agents/skills/surmount-merge-review/SKILL.md\x00",
            "M\x00crates/agent_ui/src/merge_review.rs\x00",
            "A\x00surmount-merge-categories.toml\x00",
        );
        let paths = parse_git_diff_name_status(sample)
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 3);
        assert!(paths.iter().any(|p| p.contains("merge_review")));
    }

    #[test]
    fn build_session_item_count_matches_input_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let paths = [
            ("crates/agent_ui/src/agent_panel.rs".into(), false),
            ("crates/editor/src/editor.rs".into(), false),
            ("Cargo.toml".into(), true),
        ];
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            paths.iter().cloned(),
        );
        assert_eq!(session.items.len(), paths.len());
        assert_eq!(session.conflict_items().count(), 1);
    }

    #[test]
    fn pending_action_items_excludes_fork_owned_without_hitl() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
                ("crates/editor/src/editor.rs".into(), false),
            ],
        );
        let pending = pending_action_items(&session);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].category_id, "misc_upstream");
    }

    #[test]
    fn category_groups_cover_all_session_items() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
                ("crates/editor/src/editor.rs".into(), false),
                ("Cargo.toml".into(), false),
            ],
        );
        let groups = category_groups(&session);
        let grouped_count: usize = groups.iter().map(|(_, _, items)| items.len()).sum();
        assert_eq!(grouped_count, session.items.len());
    }

    #[test]
    fn submodule_worktree_without_surmount_md_is_not_surmount() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let vibe_palace = workspace_root.join("ref/vibe-palace");
        if vibe_palace.is_dir() {
            assert!(
                !is_surmount_workspace(&vibe_palace),
                "active repo pointing at ref/vibe-palace must not pass surmount detection"
            );
        }
    }

    #[test]
    fn loads_repo_manifest_when_present() {
        let manifest = load_manifest_from_worktree(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap(),
        )
        .expect("workspace manifest");
        let item = manifest.classify_path("crates/acp_thread/src/acp_thread.rs");
        assert_eq!(item.category_id, "native_agent_core");
    }

    #[test]
    fn initial_actionable_visible_count_caps_at_twenty_five() {
        assert_eq!(initial_actionable_visible_count(99, false), 25);
        assert_eq!(initial_actionable_visible_count(10, false), 10);
        assert_eq!(initial_actionable_visible_count(99, true), 99);
    }

    #[test]
    fn test_item_serde_backward_compat() {
        let json = r#"{
            "merge_base": "abc123",
            "upstream_ref": "origin/main",
            "items": [{
                "path": "crates/editor/src/editor.rs",
                "category_id": "misc_upstream",
                "surmount_section": "Misc upstream-touching tweaks",
                "disposition": "ambiguous",
                "verdict": "pending",
                "lines_added": 0,
                "lines_removed": 0,
                "notes": null
            }],
            "categories_completed": []
        }"#;
        let session: MergeReviewSession = serde_json::from_str(json).unwrap();
        let item = &session.items[0];
        assert_eq!(item.summary, None);
        assert_eq!(item.review_state, MergeReviewState::NotReviewed);
        assert!(session.running_notes.is_empty());
        assert!(session.patterns.is_empty());
        assert_eq!(session.queue_cursor_path, None);
    }

    #[test]
    fn queue_cursor_path_roundtrips_through_json() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        set_merge_review_queue_cursor(&mut session, "lib.rs");
        let json = serde_json::to_string(&session).unwrap();
        let loaded: MergeReviewSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.queue_cursor_path.as_deref(), Some("lib.rs"));
    }

    #[test]
    fn branch_diff_selected_path_roundtrips_through_json() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        set_merge_review_branch_diff_selection(&mut session, "crates/editor/src/editor.rs");
        session
            .expanded_categories
            .insert("native_agent_core".into());
        session.show_all_actionable = true;
        let loaded: MergeReviewSession =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(
            loaded.branch_diff_selected_path.as_deref(),
            Some("crates/editor/src/editor.rs")
        );
        assert!(loaded.expanded_categories.contains("native_agent_core"));
        assert!(loaded.show_all_actionable);
    }

    #[test]
    fn reconcile_merge_review_session_items_preserves_summaries_and_drops_stale_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/editor/src/editor.rs".into(), false),
                ("crates/agent_ui/src/lib.rs".into(), false),
            ],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Kept upstream gutter tweak.".into(),
            false,
        )
        .unwrap();
        let git_paths = vec![
            ("crates/editor/src/editor.rs".into(), true),
            ("crates/new/src/file.rs".into(), false),
        ];
        reconcile_merge_review_session_items(&mut session, &manifest, &git_paths);
        assert_eq!(session.items.len(), 2);
        let editor = session
            .items
            .iter()
            .find(|item| item.path == "crates/editor/src/editor.rs")
            .expect("editor path");
        assert_eq!(
            editor.summary.as_deref(),
            Some("Kept upstream gutter tweak.")
        );
        assert_eq!(editor.disposition, ReviewDisposition::Conflict);
        assert!(
            session
                .items
                .iter()
                .any(|item| item.path == "crates/new/src/file.rs")
        );
        assert!(
            !session
                .items
                .iter()
                .any(|item| item.path == "crates/agent_ui/src/lib.rs")
        );
    }

    #[test]
    fn reconcile_merge_review_branch_diff_selection_clears_missing_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        session.branch_diff_selected_path = Some("removed.rs".into());
        reconcile_merge_review_branch_diff_selection(&mut session);
        assert_eq!(session.branch_diff_selected_path, None);
    }

    #[test]
    fn merge_review_resume_branch_diff_path_prefers_branch_diff_selection() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), false), ("beta.rs".into(), false)],
        );
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        set_merge_review_branch_diff_selection(&mut session, "beta.rs");
        assert_eq!(
            merge_review_resume_branch_diff_path(&session).as_deref(),
            Some("beta.rs")
        );
    }

    #[test]
    fn test_session_memory_for_prompt_includes_prior_summaries() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        session
            .patterns
            .push("Upstream renamed this module.".into());
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Editor tweak only affects gutter.".into(),
            false,
        )
        .unwrap();
        let memory = session_memory_for_prompt(&session, "Misc upstream-touching tweaks");
        assert!(memory.contains("Editor tweak only affects gutter."));
        assert!(memory.contains("Upstream renamed this module."));
        assert!(memory.contains("crates/editor/src/editor.rs"));
    }

    #[test]
    fn test_store_file_summary_keeps_plain_text() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let summary: String = "Upstream changed scroll behavior; fork hook unchanged.".into();
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            summary.clone(),
            false,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(item.summary.as_deref(), Some(summary.as_str()));
        assert_eq!(item.review_state, MergeReviewState::Summarized);
        assert!(session.running_notes.contains(&summary));
    }

    #[test]
    fn test_store_file_summary_open_question() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Unclear whether to keep fork keymap.".into(),
            true,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(item.review_state, MergeReviewState::OpenQuestion);
    }

    #[test]
    fn test_extract_summary_from_agent_reply() {
        let text = "Upstream added a helper.\nSummary: Keep our fork hook; upstream change is unrelated.\n";
        assert_eq!(
            extract_summary_from_agent_reply(text).as_deref(),
            Some("Keep our fork hook; upstream change is unrelated.")
        );
        let markdown = "Analysis here.\n**Summary:** Fork-only skill doc.\nOutcome: keep_fork\n";
        assert_eq!(
            extract_summary_from_agent_reply(markdown).as_deref(),
            Some("Fork-only skill doc.")
        );
        assert!(extract_summary_from_agent_reply("No summary here.").is_none());
    }

    #[gpui::test]
    fn handle_merge_review_reply_on_stop_requests_format_retry(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        let path = session.items[0].path.clone();
        session.pending_summary_path = Some(path.clone());
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let reply = "This is a new fork-only merge review skill.\n\
                         Outcome: keep_fork\n\
                         Pattern: agent skills under .agents/skills/ stay fork-owned.\n";
            let result =
                handle_merge_review_reply_on_stop(Some(reply), true, cx).expect("format retry");
            match result {
                MergeReviewCaptureOnStop::FormatRetryRequested {
                    path: retry_path,
                    prompt,
                } => {
                    assert_eq!(retry_path, path);
                    assert!(prompt.contains(MERGE_REVIEW_FORMAT_RETRY_MARKER));
                    let session = load_session(cx).expect("session");
                    assert_eq!(session.pending_summary_format_retries, 1);
                    assert_eq!(session.pending_summary_path.as_deref(), Some(path.as_str()));
                }
                other => panic!("expected format retry, got {other:?}"),
            }
        });
    }

    #[gpui::test]
    fn apply_merge_review_dogfood_synthetic_summary_captures_through_production_path(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let path = session.items[0].path.clone();
        session.pending_summary_path = Some(path.clone());
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let captured =
                apply_merge_review_dogfood_synthetic_summary(cx).expect("synthetic capture");
            assert_eq!(captured, path);
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &path).expect("item");
            assert_eq!(item.review_state, MergeReviewState::Summarized);
            assert!(
                item.summary
                    .as_ref()
                    .is_some_and(|s| s.contains("Dogfood fixture conflict")),
                "summary={:?}",
                item.summary
            );
            assert!(session.pending_summary_path.is_none());
            assert_eq!(
                item.suggested_outcome,
                Some(MergeReviewSuggestedOutcome::Synthesize)
            );
            // Workshop phase binds on Conflict disposition; fixture is Ambiguous so
            // `conflict_workshop_phase_for_item` is None. Summarized is the binding contract.
            assert!(
                MERGE_REVIEW_DOGFOOD_SYNTHETIC_SUMMARY_REPLY.contains("Summary:")
                    && MERGE_REVIEW_DOGFOOD_SYNTHETIC_SUMMARY_REPLY.contains("Outcome:"),
                "synthetic reply must include Summary:/Outcome: format lines"
            );
        });
    }

    #[gpui::test]
    fn apply_merge_review_agent_reply_for_path_rejects_reply_without_summary(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let path = session.items[0].path.clone();
        let prior_state = session.items[0].review_state;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let err = apply_merge_review_agent_reply_for_path(
                &path,
                "Agent prose without the required format lines.\nOutcome: synthesize\n",
                cx,
            )
            .expect_err("missing Summary: must not capture");
            assert!(
                err.to_string().contains("summary capture failed"),
                "err={err}"
            );
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &path).expect("item");
            assert_eq!(item.review_state, prior_state);
            assert_ne!(item.review_state, MergeReviewState::Summarized);
            assert!(item.summary.is_none());
            assert_eq!(session.pending_summary_path.as_deref(), Some(path.as_str()));
            assert!(item.suggested_outcome.is_none());
        });
    }

    #[gpui::test]
    fn apply_merge_review_agent_reply_for_path_canonicalizes_alias_for_suggested_outcome(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.items[0].path = ".cargo/audit.toml".into();
        let session_path = session.items[0].path.clone();
        let alias = "cargo/audit.toml";
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let reply = "\
Alias inject needs Summary and Outcome.\n\
Summary: Fixture conflict via undotted branch-diff alias.\n\
Outcome: synthesize\n";
            let captured =
                apply_merge_review_agent_reply_for_path(alias, reply, cx).expect("capture");
            assert_eq!(captured, session_path);
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &session_path).expect("item");
            assert_eq!(item.review_state, MergeReviewState::Summarized);
            assert!(
                item.summary
                    .as_ref()
                    .is_some_and(|s| s.contains("undotted branch-diff alias")),
                "summary={:?}",
                item.summary
            );
            assert_eq!(
                item.suggested_outcome,
                Some(MergeReviewSuggestedOutcome::Synthesize)
            );
            assert!(session.pending_summary_path.is_none());
        });
    }

    #[gpui::test]
    fn apply_merge_review_dogfood_synthetic_summary_resolves_queue_cursor_when_no_pending(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(2);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.pending_summary_path = None;
        let path = session.items[1].path.clone();
        session.queue_cursor_path = Some(path.clone());
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let captured =
                apply_merge_review_dogfood_synthetic_summary(cx).expect("synthetic capture");
            assert_eq!(captured, path);
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &path).expect("item");
            assert_eq!(item.review_state, MergeReviewState::Summarized);
            assert!(session.pending_summary_path.is_none());
            assert_eq!(
                item.suggested_outcome,
                Some(MergeReviewSuggestedOutcome::Synthesize)
            );
        });
    }

    #[gpui::test]
    fn apply_merge_review_agent_reply_for_path_rejects_empty_summary_line(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let path = session.items[0].path.clone();
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let err = apply_merge_review_agent_reply_for_path(
                &path,
                "Prose only.\nSummary:   \nOutcome: synthesize\n",
                cx,
            )
            .expect_err("empty Summary: body must not capture");
            assert!(
                err.to_string().contains("summary capture failed"),
                "err={err}"
            );
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &path).expect("item");
            assert_ne!(item.review_state, MergeReviewState::Summarized);
            assert!(item.summary.is_none());
            assert_eq!(session.pending_summary_path.as_deref(), Some(path.as_str()));
            assert!(item.suggested_outcome.is_none());
        });
    }

    #[gpui::test]
    fn apply_merge_review_agent_reply_for_path_rejects_unknown_path(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let known = session.items[0].path.clone();
        let unknown = "crates/missing/not_in_session.rs";
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let err = apply_merge_review_agent_reply_for_path(
                unknown,
                "\
Summary: Should not attach to any item.\n\
Outcome: synthesize\n",
                cx,
            )
            .expect_err("unknown path must not capture");
            assert!(
                err.to_string().contains("summary capture failed"),
                "err={err}"
            );
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, &known).expect("item");
            assert_ne!(item.review_state, MergeReviewState::Summarized);
            assert!(item.summary.is_none());
            assert_eq!(session.pending_summary_path.as_deref(), Some(unknown));
            assert!(item.suggested_outcome.is_none());
        });
    }

    #[test]
    fn canonical_merge_review_path_matches_dotted_session_paths() {
        let session = test_session_with_ambiguous_items(1);
        let mut item = session.items[0].clone();
        item.path = ".agents/skills/surmount-merge-review/SKILL.md".into();
        let mut session = session;
        session.items = vec![item];
        assert_eq!(
            canonical_merge_review_path(&session, "agents/skills/surmount-merge-review/SKILL.md")
                .as_deref(),
            Some(".agents/skills/surmount-merge-review/SKILL.md")
        );
    }

    #[test]
    fn item_for_branch_diff_path_resolves_dotted_session_paths() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [(".cargo/audit.toml".into(), false)],
        );
        let item = item_for_branch_diff_path(&session, "cargo/audit.toml").expect("item");
        assert_eq!(item.path, ".cargo/audit.toml");
        assert_eq!(
            session_path_for_branch_diff(&session, "cargo/audit.toml").as_deref(),
            Some(".cargo/audit.toml")
        );
    }

    #[test]
    fn merge_review_open_question_todo_prompt_includes_path_and_section() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/foo.rs".into(), false)],
        );
        store_file_summary(
            &mut session,
            "crates/foo.rs",
            "Unclear whether to keep fork hook.".into(),
            true,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/foo.rs").unwrap();
        let prompt = merge_review_open_question_todo_prompt(item);
        assert!(prompt.contains("todo_write"));
        assert!(prompt.contains("Merge: crates/foo.rs"));
        assert!(prompt.contains("Unclear whether to keep fork hook."));
    }

    #[test]
    fn merge_review_section_draft_prompt_includes_section_summaries() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/editor/src/editor.rs".into(), false),
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
            ],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Take upstream editor refactor.".into(),
            false,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt = merge_review_section_draft_prompt(&session, &item.surmount_section);
        assert!(prompt.contains("Take upstream editor refactor."));
        assert!(prompt.contains("SURMOUNT.md"));
    }

    #[test]
    fn capture_summary_marks_open_question_on_needs_human_outcome() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        let reply = "Summary: Unclear which side owns this hook.\nOutcome: needs_human\n";
        assert!(capture_summary_for_path(&mut session, "lib.rs", reply));
        let item = item_for_path(&session, "lib.rs").unwrap();
        assert_eq!(item.review_state, MergeReviewState::OpenQuestion);
    }

    #[test]
    fn conflict_item_lookup_accepts_undotted_branch_diff_path() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [(".cargo/audit.toml".into(), true)],
        );
        session.items[0].review_state = MergeReviewState::Summarized;
        let item = item_for_branch_diff_path(&session, "cargo/audit.toml").expect("conflict item");
        assert_eq!(item.disposition, ReviewDisposition::Conflict);
        assert_eq!(item.review_state, MergeReviewState::Summarized);
    }

    #[test]
    fn store_file_summary_normalizes_undotted_branch_diff_path() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [(".cargo/audit.toml".into(), false)],
        );
        store_file_summary(
            &mut session,
            "cargo/audit.toml",
            "Routine audit bump.".into(),
            false,
        )
        .expect("store");
        let item = item_for_path(&session, ".cargo/audit.toml").expect("item");
        assert_eq!(item.review_state, MergeReviewState::Summarized);
    }

    #[test]
    fn session_summarized_complete_requires_all_summarized_not_verdict() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/foo.rs".into(), false),
                ("crates/bar.rs".into(), false),
            ],
        );
        assert!(!session_summarized_complete(&session));
        session.items[0].review_state = MergeReviewState::Summarized;
        assert!(!session_summarized_complete(&session));
        session.items[1].review_state = MergeReviewState::Summarized;
        assert!(session_summarized_complete(&session));
        session.items[0].verdict = ReviewVerdict::Pending;
        assert!(session_summarized_complete(&session));
        session.items[0].review_state = MergeReviewState::OpenQuestion;
        assert!(!session_summarized_complete(&session));
    }

    #[test]
    fn next_merge_review_file_path_resolves_canonical_current_path() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                (".cargo/audit.toml".into(), false),
                ("crates/foo.rs".into(), false),
            ],
        );
        session.items[0].review_state = MergeReviewState::Summarized;
        assert_eq!(
            next_merge_review_file_path(&session, Some("cargo/audit.toml")).as_deref(),
            Some("crates/foo.rs")
        );
    }

    #[test]
    fn test_extract_pattern_from_agent_reply() {
        let text = "Summary: Take upstream.\nPattern: Upstream-only refactors in editor/ are safe to accept.\n";
        assert_eq!(
            extract_pattern_from_agent_reply(text).as_deref(),
            Some("Upstream-only refactors in editor/ are safe to accept.")
        );
        assert!(extract_pattern_from_agent_reply("Summary: Done.").is_none());
    }

    #[test]
    fn capture_summary_records_pattern_and_completes_category() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
                ("crates/agent_ui/src/merge_review.rs".into(), false),
            ],
        );
        let reply =
            "Summary: Fork-owned UI tweak.\nPattern: agent_ui paths stay fork-owned this merge.\n";
        assert!(capture_summary_for_path(
            &mut session,
            "crates/agent_ui/src/agent_panel.rs",
            reply
        ));
        assert_eq!(session.patterns.len(), 1);
        assert!(
            session
                .patterns
                .first()
                .is_some_and(|p| p.contains("agent_ui"))
        );
        assert!(!session.categories_completed.contains("agent_ui"));

        let reply2 = "Summary: Another fork hook.\n";
        assert!(capture_summary_for_path(
            &mut session,
            "crates/agent_ui/src/merge_review.rs",
            reply2
        ));
        assert!(session.categories_completed.contains("agent_ui"));
    }

    #[test]
    fn capture_summary_for_path_stores_summary_and_clears_open_question_default() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        session.pending_summary_path = Some("crates/editor/src/editor.rs".into());
        let reply = "Looks like upstream refactored naming.\nSummary: Take upstream; our fork has no local edits here.\n";
        assert!(capture_summary_for_path(
            &mut session,
            "crates/editor/src/editor.rs",
            reply
        ));
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(item.review_state, MergeReviewState::Summarized);
        assert!(
            item.summary
                .as_ref()
                .is_some_and(|s| s.contains("Take upstream"))
        );
    }

    #[test]
    fn capture_summary_for_path_marks_open_question_when_reply_asks() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let reply = "Summary: Unclear which side owns the zoom hook.\nOpen question: Confirm whether to keep fork dock behavior.\n";
        assert!(capture_summary_for_path(
            &mut session,
            "crates/editor/src/editor.rs",
            reply
        ));
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(item.review_state, MergeReviewState::OpenQuestion);
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "integration test runs triage script"
    )]
    fn triage_script_matches_load_session_paths() {
        let fixture = GitMergeFixture::new();
        fixture.diverge_fork_and_upstream();
        let session = load_session_from_fixture(&fixture);
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/surmount-merge-triage");
        let output = std::process::Command::new("bash")
            .arg(script)
            .current_dir(fixture.path())
            .output()
            .expect("run triage script");
        assert!(
            output.status.success(),
            "triage script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("parse triage json");
        let script_paths = json["changed_files"]
            .as_array()
            .expect("changed_files array")
            .iter()
            .map(|entry| entry["path"].as_str().expect("path").to_string())
            .collect::<std::collections::HashSet<_>>();
        let session_paths = session
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(script_paths, session_paths);
        assert_eq!(
            json["merge_base"].as_str().expect("merge_base"),
            session.merge_base
        );
    }

    #[test]
    fn test_merge_review_file_prompt_includes_memory() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Prior editor summary.".into(),
            false,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt = merge_review_file_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(prompt.contains("Prior editor summary."));
        assert!(prompt.contains("Running notes:"));
    }

    #[test]
    fn test_merge_review_file_prompt_includes_section() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt = merge_review_file_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(prompt.contains("Misc upstream-touching tweaks"));
        assert!(prompt.contains("crates/editor/src/editor.rs"));
    }

    #[test]
    fn test_merge_review_file_prompt_asks_for_summary_line() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt = merge_review_file_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(prompt.contains("Summary:"));
        assert!(prompt.contains("scoped single-file turn"));
        assert!(!prompt.contains("```json"));
    }

    #[test]
    fn test_merge_review_conflict_file_prompt_includes_three_embeds_and_decision_lines() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), true)],
        );
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt =
            merge_review_conflict_file_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(prompt.contains("active merge conflict"));
        assert!(prompt.contains("Fork (ours, git index stage 2)"));
        assert!(prompt.contains("Upstream (theirs, git index stage 3)"));
        assert!(prompt.contains("Working tree with conflict markers"));
        assert!(prompt.contains("merge_review_conflict_sides"));
        assert!(prompt.contains("Decision:"));
        assert!(prompt.contains("Tests:"));
        // Full ban suite (conflict Review Diff package must stay on embeds).
        assert!(
            prompt.contains("Never open skill files"),
            "conflict prompt must ban skill files"
        );
        assert!(
            prompt.contains("self-contained"),
            "conflict prompt must mark embeds as self-contained"
        );
        assert!(
            prompt.contains("fs/read_text_file"),
            "conflict prompt must ban host fs/read outside the worktree"
        );
        assert!(
            prompt.contains("host Surmount"),
            "conflict prompt must reject host Surmount absolute paths"
        );
        assert!(
            prompt.contains("tool `.rs` sources") || prompt.contains("tool `.rs`"),
            "conflict prompt must ban tool source reads"
        );
        let synthesize =
            merge_review_conflict_synthesize_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(
            synthesize.contains("self-contained"),
            "synthesize prompt must be self-contained"
        );
        assert!(
            synthesize.contains("fs/read_text_file"),
            "synthesize prompt must ban fs/read outside worktree"
        );
        assert!(
            synthesize.contains("Never open skill files"),
            "synthesize prompt must ban skill files"
        );
        let plan = merge_review_plan_prompt(&session);
        assert!(
            plan.contains("fs/read_text_file"),
            "plan prompt must ban fs/read outside worktree"
        );
        assert!(
            plan.contains("Do not open skill files") || plan.contains("skill files"),
            "plan prompt must ban skill files"
        );
    }

    #[test]
    fn merge_review_conflict_review_content_blocks_includes_three_resources() {
        use agent_client_protocol::schema::v1 as acp;

        let sides = MergeReviewConflictSides {
            ours_text: "fork editor\n".into(),
            theirs_text: "upstream editor\n".into(),
            working_text: "<<<<<<< HEAD\nfork editor\n=======\nupstream editor\n>>>>>>> main\n"
                .into(),
        };
        let blocks = merge_review_conflict_review_content_blocks(
            "prompt",
            "crates/editor/src/editor.rs",
            &sides,
        );
        assert_eq!(blocks.len(), 4);
        assert!(matches!(blocks[0], acp::ContentBlock::Text(_)));
        assert!(matches!(blocks[1], acp::ContentBlock::Resource(_)));
        assert!(matches!(blocks[2], acp::ContentBlock::Resource(_)));
        assert!(matches!(blocks[3], acp::ContentBlock::Resource(_)));
    }

    #[test]
    fn load_merge_review_conflict_sides_reads_git_stages_from_conflicted_merge() {
        smol::block_on(async {
            let fixture = GitMergeFixture::new();
            fixture.begin_conflicted_merge();
            let sides =
                load_merge_review_conflict_sides(fixture.path(), "crates/editor/src/editor.rs")
                    .await
                    .expect("conflict sides");
            assert!(sides.ours_text.contains("fork editor"));
            assert!(sides.theirs_text.contains("upstream editor"));
            assert!(sides.working_text.contains("<<<<<<<"));
        });
    }

    #[test]
    fn test_merge_review_summary_snippet_truncates_long_text() {
        let long = "a".repeat(150);
        let snippet = merge_review_summary_snippet(&long);
        assert!(snippet.ends_with('…'));
        assert!(snippet.chars().count() <= 121);
    }

    #[test]
    fn test_is_surmount_workspace_detects_root_manifest() {
        let root = std::env::temp_dir().join(format!(
            "merge-review-surmount-detect-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("SURMOUNT.md"), "# fork").unwrap();
        assert!(is_surmount_workspace(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_is_surmount_workspace_accepts_single_file_worktree_path() {
        let root = std::env::temp_dir().join(format!(
            "merge-review-surmount-file-wt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let surmount = root.join("SURMOUNT.md");
        std::fs::write(&surmount, "# fork").unwrap();
        // Dogfood / open-file can set work_directory to the file path itself.
        assert!(
            is_surmount_workspace(&surmount),
            "file worktree path must count as Surmount"
        );
        assert_eq!(normalize_surmount_worktree_root(&surmount), root);
        // Another file under the same root still detects via parent SURMOUNT.md.
        let other = root.join("README.md");
        std::fs::write(&other, "hi").unwrap();
        assert!(is_surmount_workspace(&other));
        assert_eq!(normalize_surmount_worktree_root(&other), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_is_reviewable_changed_path_skips_directories() {
        let root =
            std::env::temp_dir().join(format!("merge-review-path-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ref/vibe-palace")).unwrap();
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(root.join("crates/foo.rs"), "").unwrap();
        assert!(!is_reviewable_changed_path(&root, "ref/vibe-palace"));
        assert!(is_reviewable_changed_path(&root, "crates/foo.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_review_state_labels_are_plain_language() {
        assert_eq!(MergeReviewState::NotReviewed.label(), "Not reviewed yet");
        assert_eq!(MergeReviewState::Summarized.label(), "Summarized");
        assert_eq!(MergeReviewState::OpenQuestion.label(), "Open question");
    }

    #[test]
    fn merge_review_compact_labels_match_toolbar_chips() {
        assert_eq!(MergeReviewState::NotReviewed.compact_label(), "Pending");
        assert_eq!(MergeReviewState::Summarized.compact_label(), "Done");
        assert_eq!(MergeReviewState::OpenQuestion.compact_label(), "Stuck");
    }

    #[test]
    fn merge_review_header_label_hides_pending_files() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(item.review_state, MergeReviewState::NotReviewed);
        assert_eq!(merge_review_header_label_for_item(item), None);
    }

    #[test]
    fn merge_review_header_label_shows_summary_snippet_when_done() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Upstream-only rename; fork untouched.".into(),
            false,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(
            merge_review_header_label_for_item(item).as_deref(),
            Some("Upstream-only rename; fork untouched.")
        );
    }

    #[test]
    fn merge_review_header_label_shows_stuck_chip() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Unclear zoom hook ownership.".into(),
            true,
        )
        .unwrap();
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        assert_eq!(
            merge_review_header_label_for_item(item).as_deref(),
            Some("Stuck")
        );
    }

    #[test]
    fn merge_review_focus_dock_labels_roundtrip() {
        assert_eq!(
            dock_position_from_storage_label("left"),
            Some(DockPosition::Left)
        );
        assert_eq!(
            dock_position_from_storage_label("bottom"),
            Some(DockPosition::Bottom)
        );
        assert_eq!(dock_position_storage_label(DockPosition::Right), "right");
    }

    #[test]
    fn merge_review_progress_label_counts_summarized_items() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("crates/editor/src/editor.rs".into(), false),
                ("crates/agent_ui/src/agent_panel.rs".into(), false),
            ],
        );
        assert_eq!(merge_review_progress_label(&session), "0/2");
        store_file_summary(
            &mut session,
            "crates/editor/src/editor.rs",
            "Take upstream.".into(),
            false,
        )
        .unwrap();
        assert_eq!(merge_review_progress_label(&session), "1/2");
    }

    #[gpui::test]
    fn merge_review_branch_diff_controls_marks_summarized_file_done(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        store_file_summary(&mut session, "lib.rs", "Keep ours.".into(), false).unwrap();
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "lib.rs");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let controls = merge_review_branch_diff_controls(true, Some("lib.rs"), cx);
            assert!(controls.workflow_active);
            assert!(controls.current_file_done);
            assert!(!controls.review_diff_ready);
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_NEXT_FILE),
                "got {}",
                controls.step_label
            );
            assert!(
                controls.progress_label.contains("File 1/1 · lib.rs"),
                "got {}",
                controls.progress_label
            );
        });
    }

    #[test]
    fn extract_conflict_decision_from_reply_parses_decision_rationale_and_tests() {
        let reply = "Decision: keep_fork\nRationale: Fork hook must stay.\nTests: assert zoom hook; assert dock restore\n";
        let decision = extract_conflict_decision_from_reply(reply).expect("decision");
        assert_eq!(decision.outcome, MergeReviewSuggestedOutcome::KeepFork);
        assert_eq!(decision.rationale, "Fork hook must stay.");
        assert_eq!(decision.test_assertions.len(), 2);
    }

    #[test]
    fn apply_record_decision_tool_input_stores_on_session_item() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let input = serde_json::json!({
            "path": "lib.rs",
            "outcome": "keep_fork",
            "rationale": "Fork hook must stay.",
            "test_assertions": ["assert zoom hook"],
        });
        let path = apply_record_decision_tool_input(&mut session, &input).expect("apply");
        assert_eq!(path, "lib.rs");
        let item = item_for_path(&session, "lib.rs").unwrap();
        assert!(item.conflict_decision.is_some());
        assert!(!session.patterns.is_empty());
    }

    #[test]
    fn sync_conflict_todo_completion_marks_complete_when_three_todos_done() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let decision = MergeReviewConflictDecision {
            outcome: MergeReviewSuggestedOutcome::KeepFork,
            rationale: "keep fork".into(),
            surmount_note: None,
            test_assertions: Vec::new(),
            test_paths: Vec::new(),
            recorded_at: None,
        };
        store_conflict_decision(&mut session, "lib.rs", decision).unwrap();
        if let Some(item) = session.items.iter_mut().find(|item| item.path == "lib.rs") {
            item.conflict_test_todo_ids = (0..3)
                .map(|slot| conflict_todo_stable_id("lib.rs", slot))
                .collect();
        }
        let plan_entries = (0..3)
            .map(|slot| {
                (
                    conflict_todo_stable_id("lib.rs", slot),
                    agent_client_protocol::schema::v1::PlanEntryStatus::Completed,
                )
            })
            .collect::<Vec<_>>();
        assert!(sync_merge_review_conflict_todos_from_plan(
            &mut session,
            &plan_entries
        ));
        let item = item_for_path(&session, "lib.rs").unwrap();
        assert!(item.conflict_todos_complete);
    }

    #[gpui::test]
    fn handle_merge_review_tool_completion_records_decision(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        session.focus_layout_active = true;
        let input = serde_json::json!({
            "path": "lib.rs",
            "outcome": "take_upstream",
            "rationale": "Upstream parser is correct.",
        });
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let path =
                handle_merge_review_tool_completion("merge_review_record_decision", &input, cx)
                    .expect("recorded");
            assert_eq!(path, "lib.rs");
            let loaded = load_session(cx).expect("session");
            let item = item_for_path(&loaded, "lib.rs").unwrap();
            assert_eq!(
                item.conflict_decision.as_ref().map(|d| &d.rationale),
                Some(&"Upstream parser is correct.".to_string())
            );
        });
    }

    #[test]
    fn conflict_workshop_phase_complete_tests_when_decision_no_todos_done() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        store_file_summary(&mut session, "lib.rs", "Conflict summary.".into(), false).unwrap();
        store_conflict_decision(
            &mut session,
            "lib.rs",
            MergeReviewConflictDecision {
                outcome: MergeReviewSuggestedOutcome::KeepFork,
                rationale: "keep fork".into(),
                surmount_note: None,
                test_assertions: vec!["assert hook".into()],
                test_paths: Vec::new(),
                recorded_at: None,
            },
        )
        .unwrap();
        let item = item_for_path(&session, "lib.rs").unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "resolved\n").expect("write");
        let phase = conflict_workshop_phase_for_item(&session, item, "lib.rs", Some(dir.path()));
        use git_ui::project_diff::MergeReviewConflictWorkshopPhase;
        assert_eq!(phase, Some(MergeReviewConflictWorkshopPhase::CompleteTests));
    }

    #[test]
    fn conflict_file_ready_for_advance_allows_when_complete() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let item = session
            .items
            .iter_mut()
            .find(|item| item.path == "lib.rs")
            .unwrap();
        item.review_state = MergeReviewState::Summarized;
        item.conflict_decision = Some(MergeReviewConflictDecision {
            outcome: MergeReviewSuggestedOutcome::KeepFork,
            rationale: "keep".into(),
            surmount_note: None,
            test_assertions: Vec::new(),
            test_paths: Vec::new(),
            recorded_at: None,
        });
        item.conflict_todos_complete = true;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "resolved\n").expect("write");
        assert!(conflict_file_ready_for_advance(&session, "lib.rs", dir.path()).is_ok());
    }

    #[test]
    fn merge_review_send_review_comments_prompt_includes_comments() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let mut session = session;
        store_file_summary(
            &mut session,
            "lib.rs",
            "Conflict in zoom hook.".into(),
            false,
        )
        .unwrap();
        let prompt = merge_review_send_review_comments_prompt(
            &session,
            "lib.rs",
            "File: lib.rs\nKeep fork zoom hook",
        );
        assert!(prompt.contains("active merge conflict"));
        assert!(prompt.contains("Keep fork zoom hook"));
        assert!(prompt.contains("Review comments on this diff"));
    }

    #[test]
    fn conflict_workshop_phase_record_decision_when_markers_cleared() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        store_file_summary(&mut session, "lib.rs", "Conflict summary.".into(), false).unwrap();
        let item = item_for_path(&session, "lib.rs").unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "resolved\n").expect("write");
        let phase = conflict_workshop_phase_for_item(&session, item, "lib.rs", Some(dir.path()));
        use git_ui::project_diff::MergeReviewConflictWorkshopPhase;
        assert_eq!(
            phase,
            Some(MergeReviewConflictWorkshopPhase::RecordDecision)
        );
    }

    #[test]
    fn conflict_file_ready_for_advance_blocks_open_todos() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let item = session
            .items
            .iter_mut()
            .find(|item| item.path == "lib.rs")
            .unwrap();
        item.review_state = MergeReviewState::Summarized;
        item.conflict_decision = Some(MergeReviewConflictDecision {
            outcome: MergeReviewSuggestedOutcome::KeepFork,
            rationale: "keep".into(),
            surmount_note: None,
            test_assertions: vec!["assert hook".into()],
            test_paths: Vec::new(),
            recorded_at: None,
        });
        item.conflict_test_todo_ids = vec!["todo-1".into()];
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "resolved\n").expect("write");
        assert_eq!(
            conflict_file_ready_for_advance(&session, "lib.rs", dir.path()),
            Err("complete conflict test todos before advancing")
        );
    }

    #[test]
    fn merge_review_conflict_file_prompt_includes_prior_section_decisions() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("a.rs".into(), true), ("b.rs".into(), true)],
        );
        store_conflict_decision(
            &mut session,
            "a.rs",
            MergeReviewConflictDecision {
                outcome: MergeReviewSuggestedOutcome::KeepFork,
                rationale: "Fork-owned dock zoom hook".into(),
                surmount_note: None,
                test_assertions: Vec::new(),
                test_paths: Vec::new(),
                recorded_at: None,
            },
        )
        .unwrap();
        let item = item_for_path(&session, "b.rs").unwrap();
        let prompt = merge_review_conflict_file_prompt(&session, item, "b.rs");
        assert!(prompt.contains("Fork-owned dock zoom hook"));
        assert!(prompt.contains("merge_review_record_decision"));
    }

    #[test]
    fn extract_suggested_outcome_from_reply_parses_outcome_line() {
        let reply = "Summary: Upstream renamed the module.\nOutcome: take_upstream\n";
        assert_eq!(
            extract_suggested_outcome_from_reply(reply),
            Some(MergeReviewSuggestedOutcome::TakeUpstream)
        );
        let mut session = build_session(
            &CategoryManifest::from_toml(TEST_MANIFEST).unwrap(),
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let captured = "Summary: Keep fork zoom hook.\nOutcome: keep_fork\n";
        assert!(capture_summary_for_path(&mut session, "lib.rs", captured));
        let item = item_for_path(&session, "lib.rs").unwrap();
        assert_eq!(
            item.suggested_outcome,
            Some(MergeReviewSuggestedOutcome::KeepFork)
        );
    }

    #[gpui::test]
    fn merge_review_branch_diff_controls_shows_conflict_resolution_buttons(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        let reply = "Summary: Take upstream parser change.\nOutcome: take_upstream\n";
        assert!(capture_summary_for_path(&mut session, "lib.rs", reply));
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            session.focus_layout_active = true;
            save_session(cx, &session).expect("save session");
            let controls = merge_review_branch_diff_controls(true, Some("lib.rs"), cx);
            assert!(controls.is_conflict_file);
            assert!(controls.show_conflict_resolution);
            assert_eq!(
                controls.suggested_outcome,
                Some(git_ui::project_diff::MergeReviewConflictOutcomeHint::TakeUpstream)
            );
            assert_eq!(
                controls.conflict_workshop_phase,
                Some(git_ui::project_diff::MergeReviewConflictWorkshopPhase::DiscussReady)
            );
            assert!(
                controls.step_label.contains("Discuss conflict"),
                "got {}",
                controls.step_label
            );
        });
    }

    #[gpui::test]
    fn merge_review_summary_saved_toast_names_next_step(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let toast = merge_review_summary_saved_toast("lib.rs", true, cx);
            assert!(toast.contains("lib.rs"), "{toast}");
            // Production uses lowercase after the em-dash: "summarized — advanced. Click green…"
            assert!(toast.contains("advanced"), "{toast}");
            assert!(toast.contains("Review Diff"), "{toast}");
        });
    }

    #[gpui::test]
    fn merge_review_summary_saved_toast_includes_locked_queue_position(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let toast = merge_review_summary_saved_toast("alpha.rs", false, cx);
            assert!(toast.contains("File 1/2 · alpha.rs"), "{toast}");
            assert!(toast.contains("Next file"), "{toast}");
        });
    }

    #[test]
    fn merge_review_append_user_note_prefixes_prompt() {
        let prompt = merge_review_append_user_note("Base prompt.".into(), Some("focus on hook"));
        assert!(prompt.starts_with("Human direction:\nfocus on hook"));
        assert!(prompt.contains("Base prompt."));
        assert_eq!(
            merge_review_append_user_note("Base.".into(), Some("  ")),
            "Base."
        );
    }

    #[gpui::test]
    fn clarifying_note_stash_round_trip(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        cx.update(|cx| {
            assert!(!clarifying_note_pending(cx));
            stash_merge_review_clarifying_note(cx, Some("focus hooks".into()));
            assert!(clarifying_note_pending(cx));
            assert_eq!(
                take_merge_review_clarifying_note(cx).as_deref(),
                Some("focus hooks")
            );
            assert!(!clarifying_note_pending(cx));
        });
    }

    #[test]
    fn merge_review_workflow_green_border_is_literal_0f0() {
        use crate::merge_review_step_rail::merge_review_workflow_green_border;
        let green = merge_review_workflow_green_border();
        assert!(
            green.s > 0.9 && green.l > 0.4,
            "workflow border should be saturated green (#0f0), got {green:?}"
        );
    }

    #[gpui::test]
    fn capture_conflict_workshop_reply_appends_running_notes(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        session.pending_conflict_discuss_path = Some("lib.rs".into());
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            assert!(capture_conflict_workshop_reply(
                cx,
                "Should we keep the fork hook?"
            ));
            let loaded = load_session(cx).expect("session");
            assert!(loaded.running_notes.contains("[lib.rs] Discuss:"));
            assert!(
                loaded
                    .running_notes
                    .contains("Should we keep the fork hook?")
            );
            assert!(loaded.pending_conflict_discuss_path.is_none());
        });
    }

    #[test]
    fn merge_review_step_rail_labels_are_stable() {
        use crate::merge_review_step_rail::{
            RAIL_BTN_DRAFT_COMMIT_MESSAGE, RAIL_BTN_END, RAIL_BTN_KEEP_FORK, RAIL_BTN_NEXT_FILE,
            RAIL_BTN_PREVIEW_MERGE, RAIL_BTN_REVIEW_DIFF, RAIL_BTN_REVIEW_WORKING,
            RAIL_BTN_TAKE_UPSTREAM,
        };

        assert_eq!(RAIL_BTN_REVIEW_DIFF, "Review Diff");
        assert_eq!(RAIL_BTN_REVIEW_WORKING, "Summarizing…");
        assert_eq!(RAIL_BTN_NEXT_FILE, "Next file →");
        assert_eq!(RAIL_BTN_KEEP_FORK, "Keep fork");
        assert_eq!(RAIL_BTN_TAKE_UPSTREAM, "Take upstream");
        assert_eq!(RAIL_BTN_END, "End merge review");
        assert_eq!(RAIL_BTN_PREVIEW_MERGE, "Preview merge");
        assert_eq!(RAIL_BTN_DRAFT_COMMIT_MESSAGE, "Draft commit message");
    }

    /// Paints production rail container + button a11y helpers into a live outline.
    /// Fails if role/aria_label are removed from those helpers (used by real render).
    #[gpui::test]
    async fn merge_review_rail_paints_toolbar_and_button_labels(cx: &mut gpui::TestAppContext) {
        use crate::merge_review_step_rail::{
            MERGE_REVIEW_RAIL_A11Y_LABEL, RAIL_BTN_END, RAIL_BTN_REVIEW_DIFF,
            merge_review_step_rail_container, with_merge_review_rail_button_a11y,
        };
        use crate::test_support::init_test;
        use gpui::{
            Context, IntoElement, OutlineDetail, ParentElement, Render, StatefulInteractiveElement,
            Window,
        };

        init_test(cx);

        struct RailProbe;

        impl Render for RailProbe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                merge_review_step_rail_container("1/3 · Reviewing")
                    .child(
                        with_merge_review_rail_button_a11y(
                            "merge-review-rail-review-diff",
                            RAIL_BTN_REVIEW_DIFF,
                        )
                        .on_click(|_, _, _| {}),
                    )
                    .child(
                        with_merge_review_rail_button_a11y("merge-review-rail-end", RAIL_BTN_END)
                            .on_click(|_, _, _| {}),
                    )
            }
        }

        let window = cx.add_window(|_, _| RailProbe);
        let outline = cx
            .update_window(window.into(), |_, window, cx| {
                let _ = window.draw(cx);
                window.a11y_outline(OutlineDetail::Room)
            })
            .unwrap();

        assert!(
            outline.contains(&format!("[Toolbar] \"{MERGE_REVIEW_RAIL_A11Y_LABEL}\"")),
            "room outline must include merge-review toolbar landmark: {outline}"
        );
        assert!(
            outline.contains(&format!("[Button] \"{RAIL_BTN_REVIEW_DIFF}\"")),
            "room outline must include Review Diff button: {outline}"
        );
        assert!(
            outline.contains(&format!("[Button] \"{RAIL_BTN_END}\"")),
            "room outline must include End merge review button: {outline}"
        );
        assert!(
            outline.contains("interactive:") && !outline.contains("interactive: 0"),
            "rail buttons must count as interactive: {outline}"
        );
        let landmark_line = outline
            .lines()
            .find(|line| line.contains("landmarks:"))
            .unwrap_or("");
        assert!(
            !landmark_line.contains("landmarks: 0"),
            "toolbar/status should contribute landmarks: {outline}"
        );
    }

    #[test]
    fn parse_merge_tree_preview_counts_conflicts_and_truncates() {
        let sample = "changed in both\n  base file\nmerged\nadded in remote\nremoved in remote\n";
        let preview = parse_merge_tree_preview(sample);
        assert_eq!(preview.conflict_count, 1);
        assert_eq!(preview.merged_count, 1);
        assert_eq!(preview.added_count, 1);
        assert_eq!(preview.removed_count, 1);
        assert!(preview.summary_line.contains("1 file(s) would conflict"));

        let long = "x".repeat(MERGE_TREE_PREVIEW_MAX_CHARS + 50);
        let truncated = parse_merge_tree_preview(&long);
        assert!(truncated.truncated);
        assert!(truncated.display_text.ends_with('…'));
    }

    #[test]
    fn merge_review_merge_command_uses_upstream_ref() {
        assert_eq!(
            merge_review_merge_command("origin/main"),
            "git merge origin/main"
        );
    }

    #[test]
    fn format_git_merge_run_display_joins_stdout_stderr_and_exit_code() {
        let display =
            format_git_merge_run_display("Already up to date.\n", "hint: pull first\n", Some(0));
        assert!(display.contains("exit code: 0"));
        assert!(display.contains("Already up to date."));
        assert!(display.contains("hint: pull first"));
    }

    #[test]
    fn extract_merge_commit_message_from_reply_parses_prefix() {
        let reply =
            "Here is a draft.\nMerge commit message: Merge origin/main into surmount\n\nBody line.";
        let message = extract_merge_commit_message_from_reply(reply).expect("message");
        assert!(message.starts_with("Merge origin/main into surmount"));
        assert!(message.contains("Body line."));
    }

    #[gpui::test]
    fn try_capture_merge_review_commit_message_from_reply_stores_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.pending_merge_commit_message_capture = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let draft = try_capture_merge_review_commit_message_from_reply(
                "Merge commit message: Merge origin/main into surmount",
                cx,
            );
            assert_eq!(draft.as_deref(), Some("Merge origin/main into surmount"));
            let loaded = load_session(cx).expect("session");
            assert_eq!(
                loaded.pending_merge_commit_message.as_deref(),
                Some("Merge origin/main into surmount")
            );
            assert!(!loaded.pending_merge_commit_message_capture);
        });
    }

    #[gpui::test]
    fn handle_merge_review_reply_on_stop_commit_message_format_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.pending_merge_commit_message_capture = true;
        let reply = "Here is a long draft without the required prefix line.\n\
                     It has enough substance to trigger a format retry prompt \
                     because we need more than eighty characters total in the body.";
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let result = handle_merge_review_reply_on_stop(Some(reply), true, cx).expect("retry");
            assert!(matches!(
                result,
                MergeReviewCaptureOnStop::CommitMessageFormatRetryRequested { .. }
            ));
            let loaded = load_session(cx).expect("session");
            assert!(loaded.pending_merge_commit_message_capture);
            assert_eq!(loaded.pending_merge_commit_message_format_retries, 1);
        });
    }

    #[gpui::test]
    fn handle_merge_review_reply_on_stop_commit_message_abandons_on_failure(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.pending_merge_commit_message_capture = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let result =
                handle_merge_review_reply_on_stop(Some("too short"), true, cx).expect("abandon");
            assert!(matches!(
                result,
                MergeReviewCaptureOnStop::CommitMessageCaptureAbandoned
            ));
            let loaded = load_session(cx).expect("session");
            assert!(!loaded.pending_merge_commit_message_capture);
        });
    }

    #[gpui::test]
    fn clear_pending_merge_commit_message_capture_clears_stuck_flag(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        session.pending_merge_commit_message_capture = true;
        session.pending_merge_commit_message_format_retries = 2;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            clear_pending_merge_commit_message_capture(cx);
            let loaded = load_session(cx).expect("session");
            assert!(!loaded.pending_merge_commit_message_capture);
            assert_eq!(loaded.pending_merge_commit_message_format_retries, 0);
        });
    }

    #[test]
    fn merge_review_commit_message_prompt_includes_session_memory() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), true)],
        );
        session.patterns.push("agent_ui stays fork-owned".into());
        session.running_notes = "Confirmed hook behavior.".into();
        session.items[0].review_state = MergeReviewState::Summarized;
        session.items[0].conflict_decision = Some(MergeReviewConflictDecision {
            outcome: MergeReviewSuggestedOutcome::KeepFork,
            rationale: "Fork hook is intentional".into(),
            surmount_note: None,
            test_assertions: Vec::new(),
            test_paths: Vec::new(),
            recorded_at: None,
        });
        let prompt = merge_review_commit_message_prompt(&session);
        assert!(prompt.contains("agent_ui stays fork-owned"));
        assert!(prompt.contains("Confirmed hook behavior"));
        assert!(prompt.contains("KeepFork"));
        assert!(prompt.contains("Merge commit message:"));
    }

    #[test]
    fn build_session_sorts_conflicts_before_non_conflicts() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("zebra.rs".into(), false),
                ("alpha.rs".into(), true),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), true),
            ],
        );
        assert_eq!(
            session
                .items
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha.rs", "gamma.rs", "beta.rs", "zebra.rs"]
        );
    }

    #[test]
    fn workflow_button_specs_includes_preview_merge_in_pre_merge() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, RAIL_BTN_NEXT_FILE,
            RAIL_BTN_PREVIEW_MERGE, RAIL_BTN_REVIEW_DIFF, workflow_button_labels,
        };
        use git_ui::project_diff::MergeReviewGitMode;

        let labels = workflow_button_labels(
            MergeReviewUiStep::PickFile,
            MergeReviewPrimaryAction::NextFile,
            false,
            None,
            MergeReviewGitMode::PreMerge,
        );
        assert_eq!(
            labels,
            vec![RAIL_BTN_PREVIEW_MERGE, RAIL_BTN_NEXT_FILE],
            "PreMerge must offer Preview merge (primary) and Next file (available)"
        );
        assert!(
            !labels.contains(&RAIL_BTN_REVIEW_DIFF),
            "PreMerge must not offer Review Diff"
        );

        let in_progress = workflow_button_labels(
            MergeReviewUiStep::PickFile,
            MergeReviewPrimaryAction::NextFile,
            false,
            None,
            MergeReviewGitMode::MergeInProgress,
        );
        assert!(
            !in_progress.contains(&RAIL_BTN_PREVIEW_MERGE),
            "Preview merge hidden while merge is in progress"
        );
    }

    #[test]
    fn pre_merge_rail_preview_merge_is_only_primary_tier() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, MergeReviewWorkflowButtonTier,
            RAIL_BTN_NEXT_FILE, RAIL_BTN_PREVIEW_MERGE, workflow_button_specs,
        };
        use git_ui::project_diff::MergeReviewGitMode;

        let specs = workflow_button_specs(
            MergeReviewUiStep::PickFile,
            MergeReviewPrimaryAction::NextFile,
            false,
            None,
            MergeReviewGitMode::PreMerge,
        );
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "merge-review-rail-preview-merge");
        assert_eq!(specs[0].label, RAIL_BTN_PREVIEW_MERGE);
        assert_eq!(specs[0].tier, MergeReviewWorkflowButtonTier::Primary);
        assert_eq!(specs[1].id, "merge-review-rail-next-file");
        assert_eq!(specs[1].label, RAIL_BTN_NEXT_FILE);
        assert_eq!(specs[1].tier, MergeReviewWorkflowButtonTier::Available);
        assert_eq!(
            specs[1].tooltip,
            "Select the next changed file for triage (before merge)"
        );
        assert!(
            specs
                .iter()
                .filter(|spec| spec.tier == MergeReviewWorkflowButtonTier::Primary)
                .count()
                == 1,
            "only Preview merge may be Primary in PreMerge"
        );
    }

    #[gpui::test]
    fn pre_merge_branch_diff_controls_show_prepare_label(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("a.rs".into(), false), ("b.rs".into(), false)],
        );
        session.focus_layout_active = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let controls = merge_review_branch_diff_controls(false, None, cx);
            assert_eq!(
                controls.progress_label.as_ref(),
                "Prepare · 2 changed files"
            );
            assert!(!controls.review_diff_ready);
            assert!(
                controls.step_label.is_empty(),
                "PreMerge must leave step_label empty, got {}",
                controls.step_label
            );
        });
    }

    #[test]
    fn workflow_button_labels_merge_in_progress_review_diff_primary_on_conflict() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, MergeReviewWorkflowButtonTier,
            RAIL_BTN_REVIEW_DIFF, workflow_button_labels, workflow_button_specs,
        };
        use git_ui::project_diff::{MergeReviewConflictWorkshopPhase, MergeReviewGitMode};

        let labels = workflow_button_labels(
            MergeReviewUiStep::ConflictWorkshop {
                phase: MergeReviewConflictWorkshopPhase::NotSummarized,
                emphasize: None,
            },
            MergeReviewPrimaryAction::ReviewDiff,
            false,
            Some(MergeReviewConflictWorkshopPhase::NotSummarized),
            MergeReviewGitMode::MergeInProgress,
        );
        assert_eq!(labels.first().copied(), Some(RAIL_BTN_REVIEW_DIFF));

        let specs = workflow_button_specs(
            MergeReviewUiStep::ConflictWorkshop {
                phase: MergeReviewConflictWorkshopPhase::NotSummarized,
                emphasize: None,
            },
            MergeReviewPrimaryAction::ReviewDiff,
            false,
            Some(MergeReviewConflictWorkshopPhase::NotSummarized),
            MergeReviewGitMode::MergeInProgress,
        );
        assert_eq!(
            specs.first().map(|spec| spec.tier),
            Some(MergeReviewWorkflowButtonTier::Primary)
        );
    }

    #[test]
    fn next_merge_review_file_path_respects_queue_order_without_wrap() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), true),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        assert_eq!(
            next_merge_review_file_path(&session, None).as_deref(),
            Some("beta.rs")
        );
        set_merge_review_queue_cursor(&mut session, "gamma.rs");
        session.items[2].review_state = MergeReviewState::Summarized;
        assert_eq!(next_merge_review_file_path(&session, None), None);
    }

    #[test]
    fn resume_after_merge_sets_queue_cursor_path_to_first_unreviewed_conflict() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("beta.rs".into(), false),
                ("alpha.rs".into(), true),
                ("gamma.rs".into(), true),
            ],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.items[0].review_state = MergeReviewState::Summarized;
        initialize_merge_review_queue_cursor(&mut session);
        assert_eq!(
            session.queue_cursor_path.as_deref(),
            Some("gamma.rs"),
            "must skip reviewed conflicts and land on first unreviewed conflict"
        );
    }

    #[test]
    fn resume_preserves_valid_existing_queue_cursor() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "beta.rs");
        initialize_merge_review_queue_cursor(&mut session);
        assert_eq!(session.queue_cursor_path.as_deref(), Some("beta.rs"));
    }

    #[test]
    fn merge_review_prepare_label_formats_changed_file_count() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("a.rs".into(), false),
                ("b.rs".into(), false),
                ("c.rs".into(), false),
            ],
        );
        assert_eq!(
            merge_review_prepare_label(&session),
            "Prepare · 3 changed files"
        );
    }

    #[test]
    fn merge_review_merge_started_detects_transition_and_assigns_cursor() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        assert!(merge_review_merge_started(
            git_ui::project_diff::MergeReviewGitMode::PreMerge,
            0,
            &session,
        ));
        session.unmerged_count = 2;
        assert!(merge_review_merge_started(
            git_ui::project_diff::MergeReviewGitMode::MergeInProgress,
            0,
            &session,
        ));
        assert!(!merge_review_merge_started(
            git_ui::project_diff::MergeReviewGitMode::MergeInProgress,
            2,
            &session,
        ));

        initialize_merge_review_queue_cursor(&mut session);
        assert_eq!(session.queue_cursor_path.as_deref(), Some("alpha.rs"));
    }

    #[test]
    fn initialize_merge_review_queue_cursor_uses_first_unreviewed_item() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), false), ("beta.rs".into(), false)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.unmerged_count = 0;
        initialize_merge_review_queue_cursor(&mut session);
        assert_eq!(
            session.queue_cursor_path.as_deref(),
            Some("alpha.rs"),
            "clean merge must still engage linear queue at first unreviewed file"
        );
    }

    #[test]
    fn reconcile_merge_review_queue_cursor_resets_unknown_path() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.queue_cursor_path = Some("deleted/path.rs".into());
        reconcile_merge_review_queue_cursor(&mut session);
        assert_eq!(session.queue_cursor_path.as_deref(), Some("alpha.rs"));
        assert_eq!(
            merge_review_queue_position_label(&session, "alpha.rs"),
            "File 1/1 · alpha.rs"
        );
    }

    #[test]
    fn next_merge_review_file_path_stops_at_open_question() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), false),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        session.items[1].review_state = MergeReviewState::OpenQuestion;
        assert_eq!(next_merge_review_file_path(&session, None), None);
    }

    #[test]
    fn next_merge_review_file_path_blocks_when_cursor_is_open_question() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), false),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        set_merge_review_queue_cursor(&mut session, "beta.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        session.items[1].review_state = MergeReviewState::OpenQuestion;
        session.items[2].review_state = MergeReviewState::NotReviewed;
        assert_eq!(
            next_merge_review_file_path(&session, None),
            None,
            "must not skip past OpenQuestion at cursor to gamma.rs"
        );
        let message = merge_review_next_file_blocked_message(&session, None).expect("message");
        assert!(message.contains("open question"), "{message}");
        assert!(message.contains("File 2/3 · beta.rs"), "{message}");
    }

    #[test]
    fn merge_review_next_file_blocked_message_end_of_queue() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), false), ("beta.rs".into(), false)],
        );
        for item in &mut session.items {
            item.review_state = MergeReviewState::Summarized;
        }
        let message =
            merge_review_next_file_blocked_message(&session, None).expect("queue complete");
        assert!(message.contains("End merge review"), "{message}");
    }

    #[test]
    fn next_merge_review_file_path_prefers_cursor_over_current_path() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), false),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        set_merge_review_queue_cursor(&mut session, "beta.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        assert_eq!(
            next_merge_review_file_path(&session, Some("alpha.rs")).as_deref(),
            Some("beta.rs"),
            "cursor at beta.rs (NotReviewed) must re-land on cursor when UI selection lags on alpha.rs"
        );
    }

    #[test]
    fn next_merge_review_file_path_successive_advances_yield_distinct_paths() {
        // Reproduces PreMerge dogfood stuck-cursor: after advancing to beta, a missing
        // ProjectDiff active path (None) must not re-return beta forever.
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), false),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        // Simulate successive Next-file: set cursor each step; UI may report None.
        assert_eq!(
            next_merge_review_file_path(&session, None).as_deref(),
            Some("alpha.rs")
        );
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        assert_eq!(
            next_merge_review_file_path(&session, None).as_deref(),
            Some("beta.rs"),
            "after cursor lands on alpha, next with no UI path must be beta (not re-land alpha)"
        );
        set_merge_review_queue_cursor(&mut session, "beta.rs");
        assert_eq!(
            next_merge_review_file_path(&session, None).as_deref(),
            Some("gamma.rs"),
            "after cursor lands on beta, next with no UI path must be gamma (not stuck on beta)"
        );
        set_merge_review_queue_cursor(&mut session, "gamma.rs");
        // UI lag after gamma is open: current still alpha must re-land on gamma once
        assert_eq!(
            next_merge_review_file_path(&session, Some("alpha.rs")).as_deref(),
            Some("gamma.rs"),
            "UI lag re-land still works when current lags behind cursor"
        );
        // Logical current matches cursor: nothing after gamma
        assert_eq!(
            next_merge_review_file_path(&session, Some("gamma.rs")),
            None,
            "at last queue item, next must be None"
        );
    }

    #[gpui::test]
    fn merge_review_out_of_order_selection_toast_blocks_mismatched_path(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.focus_layout_active = true;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            assert!(merge_review_out_of_order_selection_toast("alpha.rs", cx).is_none());
            let toast = merge_review_out_of_order_selection_toast("beta.rs", cx).expect("toast");
            assert!(toast.contains("File 1/2 · alpha.rs"));
            assert!(toast.contains(MERGE_REVIEW_OUT_OF_ORDER_TOAST_SUFFIX));
        });
    }

    #[gpui::test]
    async fn merge_review_allow_file_navigation_respects_canonical_alias_and_no_cursor(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("agents/skills/surmount-merge-review/SKILL.md".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "agents/skills/surmount-merge-review/SKILL.md");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        cx.update(|_, cx| {
            assert!(merge_review_allow_file_navigation(
                ".agents/skills/surmount-merge-review/SKILL.md",
                workspace.clone(),
                cx
            ));
            session.queue_cursor_path = None;
            save_session(cx, &session).expect("save");
            assert!(merge_review_allow_file_navigation(
                "other.rs",
                workspace.clone(),
                cx
            ));
        });
    }

    #[gpui::test]
    async fn merge_review_allow_file_navigation_respects_cursor_and_pre_merge(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        cx.update(|_, cx| {
            assert!(merge_review_allow_file_navigation(
                "alpha.rs",
                workspace.clone(),
                cx
            ));
            assert!(!merge_review_allow_file_navigation(
                "beta.rs",
                workspace.clone(),
                cx
            ));

            let mut pre_merge = session;
            pre_merge.git_mode = git_ui::project_diff::MergeReviewGitMode::PreMerge;
            pre_merge.queue_cursor_path = Some("alpha.rs".into());
            save_session(cx, &pre_merge).expect("save");
            assert!(merge_review_allow_file_navigation(
                "beta.rs",
                workspace.clone(),
                cx
            ));
        });
    }

    #[gpui::test]
    async fn intercept_review_branch_diff_note_modal_returns_false(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        cx.update(|cx| {
            crate::merge_review::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let intercepted = workspace.update_in(cx, |workspace, window, cx| {
            intercept_review_branch_diff_note_modal(
                workspace,
                window,
                cx,
                "diff",
                "origin/main",
                Some("lib.rs"),
            )
        });
        assert!(!intercepted);
    }

    async fn setup_merge_review_branch_diff_fixture(
        cx: &mut gpui::TestAppContext,
        paths: &[(&str, &str)],
    ) -> (
        gpui::Entity<Workspace>,
        gpui::VisualTestContext,
        gpui::Entity<git_ui::project_diff::ProjectDiff>,
        gpui::Entity<project::Project>,
    ) {
        use crate::test_support::init_test;
        use git_ui::project_diff::ProjectDiff;
        use gpui::VisualTestContext;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use workspace::MultiWorkspace;

        init_test(cx);
        cx.update(|cx| {
            git_ui::init(cx);
        });
        let mut tree = json!({ ".git": {} });
        if let Some(obj) = tree.as_object_mut() {
            for (path, content) in paths {
                obj.insert((*path).into(), (*content).into());
            }
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), tree).await;
        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let index_entries = paths
            .iter()
            .map(|(path, content)| (*path, (*content).to_string()))
            .collect::<Vec<_>>();
        fs.set_head_and_index_for_repo(path!("/project/.git").as_ref(), &index_entries);
        let merge_base_entries = paths
            .iter()
            .map(|(path, content)| {
                let base = format!("{content}base");
                (*path, base)
            })
            .collect::<Vec<_>>();
        fs.set_merge_base_content_for_repo(path!("/project/.git").as_ref(), &merge_base_entries);
        cx.run_until_parked();

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _| mw.workspace().clone())
            .expect("workspace");
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        let diff = cx
            .update(|window, cx| {
                ProjectDiff::spawn_test_branch_diff(
                    project.clone(),
                    workspace.clone(),
                    "origin/main".into(),
                    window,
                    cx,
                )
            })
            .await
            .expect("merge branch diff");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(diff.clone()), None, true, window, cx);
        });
        for _ in 0..40 {
            cx.run_until_parked();
        }
        (workspace, cx, diff, project)
    }

    #[gpui::test]
    async fn advance_merge_review_to_next_file_rolls_back_cursor_on_failed_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let advanced = workspace.update_in(cx, |workspace, window, cx| {
            advance_merge_review_to_next_file_with_sync(workspace, window, cx, false)
        });
        assert!(!advanced, "advance must fail without Branch Diff open");
        cx.update(|_, cx| {
            let session = load_session(cx).expect("session");
            assert_eq!(
                session.queue_cursor_path.as_deref(),
                Some("alpha.rs"),
                "cursor must stay unchanged when Branch Diff is missing"
            );
        });
    }

    #[gpui::test]
    async fn advance_merge_review_to_next_file_rolls_back_cursor_when_move_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        use git_ui::project_diff::{
            clear_merge_review_file_navigation_guard_test_override,
            set_merge_review_file_navigation_guard_for_tests,
        };

        let (workspace, mut cx, diff, _project) = setup_merge_review_branch_diff_fixture(
            cx,
            &[("alpha.rs", "alpha head\n"), ("beta.rs", "beta head\n")],
        )
        .await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            set_merge_review_file_navigation_guard_for_tests(|path, _, _| path == "alpha.rs");
        });
        diff.update_in(&mut cx, |diff, window, cx| {
            diff.move_to_repo_relative_path("alpha.rs", window, cx)
        });
        let advanced = workspace.update_in(&mut cx, |workspace, window, cx| {
            advance_merge_review_to_next_file_with_sync(workspace, window, cx, false)
        });
        assert!(
            !advanced,
            "advance must fail when guard blocks navigation to beta.rs"
        );
        cx.update(|_, cx| {
            let session = load_session(cx).expect("session");
            assert_eq!(
                session.queue_cursor_path.as_deref(),
                Some("alpha.rs"),
                "cursor must roll back when move_to_repo_relative_path returns false"
            );
            clear_merge_review_file_navigation_guard_test_override();
        });
    }

    #[gpui::test]
    async fn advance_merge_review_to_next_file_happy_path(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx, diff, _project) = setup_merge_review_branch_diff_fixture(
            cx,
            &[("alpha.rs", "alpha head\n"), ("beta.rs", "beta head\n")],
        )
        .await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        diff.update_in(&mut cx, |diff, window, cx| {
            assert!(
                diff.move_to_repo_relative_path("alpha.rs", window, cx),
                "fixture must select alpha.rs in Branch Diff"
            );
        });
        let advanced = workspace.update_in(&mut cx, |workspace, window, cx| {
            advance_merge_review_to_next_file_with_sync(workspace, window, cx, false)
        });
        assert!(advanced, "advance must succeed with Branch Diff open");
        cx.update(|_, cx| {
            let session = load_session(cx).expect("session");
            assert_eq!(
                session.queue_cursor_path.as_deref(),
                Some("beta.rs"),
                "happy-path advance must persist cursor on next file"
            );
        });
        // AC-A: Next file success focuses ProjectDiff::focus_handle (editor when non-empty).
        // Path-labeled header controls have no separate FocusHandle yet (PR1 aria only);
        // design allows this editor fallback.
        workspace.update_in(&mut cx, |_workspace, window, cx| {
            assert!(
                diff.focus_handle(cx).is_focused(window),
                "Next file must focus ProjectDiff editor/item focus_handle"
            );
        });
    }

    #[gpui::test]
    async fn advance_merge_review_to_next_file_shows_blocked_toast(cx: &mut gpui::TestAppContext) {
        use workspace::notifications::NotificationId;

        let (workspace, mut cx, diff, _project) = setup_merge_review_branch_diff_fixture(
            cx,
            &[("alpha.rs", "alpha head\n"), ("beta.rs", "beta head\n")],
        )
        .await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::OpenQuestion;
        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        diff.update_in(&mut cx, |diff, window, cx| {
            diff.move_to_repo_relative_path("alpha.rs", window, cx)
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.clear_all_notifications(cx);
            let advanced =
                advance_merge_review_to_next_file_with_sync(workspace, window, cx, false);
            assert!(!advanced, "open question must block advance");
            assert!(
                workspace.notification_ids().iter().any(|id| {
                    matches!(
                        id,
                        NotificationId::Named(name) if name.as_ref() == "surmount-merge-review"
                    )
                }),
                "advance path must show blocked toast"
            );
        });
    }

    #[test]
    fn merge_started_suppresses_generic_refresh_toast() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true)],
        );
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.unmerged_count = 2;
        let merge_started = merge_review_merge_started(
            git_ui::project_diff::MergeReviewGitMode::PreMerge,
            0,
            &session,
        );
        let auto_select = if merge_started {
            initialize_merge_review_queue_cursor(&mut session);
            session
                .queue_cursor_path
                .clone()
                .map(|path| (session.unmerged_count, path))
        } else {
            None
        };
        assert!(merge_started);
        assert!(
            auto_select.is_some(),
            "merge_started must take auto_select path instead of generic refresh toast"
        );
    }

    #[gpui::test]
    fn conflict_cleanup_banner_when_unmerged_clear_but_files_remain(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [
                ("alpha.rs".into(), true),
                ("beta.rs".into(), false),
                ("gamma.rs".into(), false),
            ],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.unmerged_count = 0;
        session.items[0].review_state = MergeReviewState::Summarized;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let controls = merge_review_branch_diff_controls(false, None, cx);
            assert!(
                controls
                    .step_label
                    .contains(MERGE_REVIEW_STEP_CONFLICT_CLEANUP),
                "step_label was {}",
                controls.step_label
            );
        });
    }

    #[test]
    fn all_complete_draft_commit_is_primary_end_is_danger() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, MergeReviewWorkflowButtonTier,
            RAIL_BTN_DRAFT_COMMIT_MESSAGE, workflow_button_specs,
        };
        use git_ui::project_diff::MergeReviewGitMode;

        let specs = workflow_button_specs(
            MergeReviewUiStep::AllComplete,
            MergeReviewPrimaryAction::EndMergeReview,
            false,
            None,
            MergeReviewGitMode::MergeInProgress,
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].label, RAIL_BTN_DRAFT_COMMIT_MESSAGE);
        assert_eq!(specs[0].tier, MergeReviewWorkflowButtonTier::Primary);
        // End merge review is rendered separately and is always Danger (never primary).
    }

    #[gpui::test]
    async fn advance_merge_review_to_next_file_updates_cursor_through_guard(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        session.items[0].review_state = MergeReviewState::Summarized;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let next = next_merge_review_file_path(&session, Some("alpha.rs")).expect("next");
        assert_eq!(next, "beta.rs");
        set_merge_review_queue_cursor(&mut session, &next);
        cx.update(|_, cx| {
            save_session(cx, &session).expect("save");
            assert!(!merge_review_allow_file_navigation(
                "alpha.rs",
                workspace.clone(),
                cx
            ));
            assert!(merge_review_allow_file_navigation(
                "beta.rs",
                workspace.clone(),
                cx
            ));
        });
    }

    #[gpui::test]
    async fn discuss_and_synthesize_actions_skip_note_modal(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use zed_actions::surmount::{DiscussMergeReviewConflict, SynthesizeMergeReviewConflict};

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true)],
        );
        session.focus_layout_active = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            window.dispatch_action(DiscussMergeReviewConflict.boxed_clone(), cx);
            assert!(
                workspace.active_modal::<MergeReviewNoteModal>(cx).is_none(),
                "Discuss conflict must not open note modal"
            );
            window.dispatch_action(SynthesizeMergeReviewConflict.boxed_clone(), cx);
            assert!(
                workspace.active_modal::<MergeReviewNoteModal>(cx).is_none(),
                "Synthesize must not open note modal"
            );
        });
    }

    #[gpui::test]
    async fn record_decision_confirm_opens_note_modal(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use zed_actions::surmount::ConfirmMergeReviewDecisionKeepFork;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true)],
        );
        session.focus_layout_active = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            open_merge_review_note_modal(
                workspace,
                window,
                cx,
                "Keep fork",
                "Rationale or caveats to record?",
                MergeReviewNoteContinuation::ConfirmDecision(MergeReviewSuggestedOutcome::KeepFork),
            );
            assert!(
                workspace.active_modal::<MergeReviewNoteModal>(cx).is_some(),
                "Record decision confirm must open note modal"
            );
        });
        workspace.update_in(cx, |_, window, cx| {
            window.dispatch_action(ConfirmMergeReviewDecisionKeepFork.boxed_clone(), cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.active_modal::<MergeReviewNoteModal>(cx).is_some(),
                "registered ConfirmKeepFork action must open note modal"
            );
        });
    }

    #[test]
    fn workflow_button_specs_includes_draft_commit_on_all_complete() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, RAIL_BTN_DRAFT_COMMIT_MESSAGE,
            workflow_button_labels,
        };
        use git_ui::project_diff::MergeReviewGitMode;

        let labels = workflow_button_labels(
            MergeReviewUiStep::AllComplete,
            MergeReviewPrimaryAction::EndMergeReview,
            false,
            None,
            MergeReviewGitMode::MergeInProgress,
        );
        assert!(labels.contains(&RAIL_BTN_DRAFT_COMMIT_MESSAGE));
    }

    #[test]
    fn merge_review_ui_step_and_primary_action_cover_all_states() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, merge_review_primary_action,
            merge_review_ui_step,
        };
        use git_ui::project_diff::{
            MergeReviewBranchDiffControls, MergeReviewConflictOutcomeHint,
            MergeReviewConflictWorkshopPhase,
        };

        let base = MergeReviewBranchDiffControls {
            workflow_active: true,
            progress_label: "1/3".into(),
            step_label: SharedString::default(),
            review_diff_ready: false,
            current_file_done: false,
            is_conflict_file: false,
            show_conflict_resolution: false,
            suggested_outcome: None,
            awaiting_agent_summary: false,
            conflict_workshop_phase: None,
            git_mode: git_ui::project_diff::MergeReviewGitMode::PreMerge,
            unmerged_count: 0,
        };

        let steps = [
            merge_review_ui_step(&base, false, false, true),
            merge_review_ui_step(&base, true, true, false),
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    show_conflict_resolution: true,
                    is_conflict_file: true,
                    suggested_outcome: Some(MergeReviewConflictOutcomeHint::KeepFork),
                    conflict_workshop_phase: Some(MergeReviewConflictWorkshopPhase::DiscussReady),
                    ..base.clone()
                },
                false,
                true,
                false,
            ),
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    current_file_done: true,
                    review_diff_ready: false,
                    ..base.clone()
                },
                false,
                true,
                false,
            ),
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    review_diff_ready: true,
                    ..base.clone()
                },
                false,
                true,
                false,
            ),
            merge_review_ui_step(&base, false, false, false),
        ];

        for step in steps {
            let _primary = merge_review_primary_action(step);
        }

        assert_eq!(
            merge_review_ui_step(&base, false, false, true),
            MergeReviewUiStep::AllComplete
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::AllComplete),
            MergeReviewPrimaryAction::EndMergeReview
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::ReviewReady),
            MergeReviewPrimaryAction::ReviewDiff
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::SummarizedNext),
            MergeReviewPrimaryAction::NextFile
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::ConflictWorkshop {
                phase: MergeReviewConflictWorkshopPhase::DiscussReady,
                emphasize: Some(MergeReviewConflictOutcomeHint::TakeUpstream)
            }),
            MergeReviewPrimaryAction::DiscussConflict
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::ConflictWorkshop {
                phase: MergeReviewConflictWorkshopPhase::RecordDecision,
                emphasize: Some(MergeReviewConflictOutcomeHint::TakeUpstream)
            }),
            MergeReviewPrimaryAction::ConfirmTakeUpstream
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::ConflictWorkshop {
                phase: MergeReviewConflictWorkshopPhase::RecordDecision,
                emphasize: None,
            }),
            MergeReviewPrimaryAction::ConfirmKeepFork
        );
        assert_eq!(
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    awaiting_agent_summary: true,
                    review_diff_ready: true,
                    ..base
                },
                false,
                true,
                false,
            ),
            MergeReviewUiStep::ReviewWorking
        );
        assert_eq!(
            merge_review_primary_action(MergeReviewUiStep::ReviewWorking),
            MergeReviewPrimaryAction::ReviewDiffWorking
        );
    }

    #[gpui::test]
    fn test_merge_review_session_complete_offers_end_rail(cx: &mut gpui::TestAppContext) {
        use crate::merge_review_step_rail::merge_review_session_complete;
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false), ("bar.rs".into(), false)],
        );
        session.focus_layout_active = true;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            assert!(
                !merge_review_session_complete(cx),
                "incomplete session must not offer End"
            );
            session.items[0].review_state = MergeReviewState::Summarized;
            session.items[1].review_state = MergeReviewState::Summarized;
            save_session(cx, &session).expect("save");
            assert!(
                merge_review_session_complete(cx),
                "all summarized must offer End"
            );
        });
    }

    #[gpui::test]
    fn merge_review_branch_diff_controls_resolves_dotted_paths(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [(".cargo/audit.toml".into(), true)],
        );
        store_file_summary(
            &mut session,
            ".cargo/audit.toml",
            "Take upstream advisory bump.".into(),
            false,
        )
        .unwrap();
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
            let controls = merge_review_branch_diff_controls(true, Some("cargo/audit.toml"), cx);
            assert!(controls.show_conflict_resolution);
            assert!(controls.current_file_done);
        });
    }

    #[gpui::test]
    async fn finalize_unsticks_summarizing_rail_when_agent_idle(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path();
        std::fs::create_dir(project_root.join(".git")).expect(".git");
        std::fs::write(project_root.join("SURMOUNT.md"), "# Surmount").expect("SURMOUNT.md");
        std::fs::write(
            project_root.join("surmount-merge-categories.toml"),
            TEST_MANIFEST,
        )
        .expect("manifest");

        let (workspace, mut cx) =
            setup_zoomed_agent_workspace_with_surmount(cx, project_root).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [(".cargo/audit.toml".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        session.pending_summary_path = Some(".cargo/audit.toml".into());
        set_merge_review_queue_cursor(&mut session, ".cargo/audit.toml");

        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
        });
        workspace.read_with(&cx, |workspace, cx| {
            let mut controls =
                merge_review_branch_diff_controls(true, Some("cargo/audit.toml"), cx);
            assert!(
                controls.awaiting_agent_summary,
                "pending capture must arm summarizing state"
            );
            finalize_merge_review_branch_diff_controls(&mut controls, false, workspace, cx);
            assert!(
                !controls.awaiting_agent_summary,
                "idle agent must unstick summarizing rail"
            );
            assert!(controls.review_diff_ready);
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_REVIEW_READY),
                "step_label was {}",
                controls.step_label
            );
            let session = load_session(cx).expect("session");
            assert!(
                session.pending_summary_path.is_some(),
                "finalize adjusts UI only; session reconcile clears storage separately"
            );
        });
    }

    #[gpui::test]
    fn reconcile_stale_pending_summary_capture_clears_on_restore(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(3);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let file_path = session.items[0].path.clone();
        session.pending_summary_path = Some(file_path.clone());
        set_merge_review_queue_cursor(&mut session, &file_path);
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let cleared = reconcile_stale_pending_summary_capture(cx).expect("cleared");
            assert_eq!(cleared, file_path);
            let session = load_session(cx).expect("session");
            assert!(session.pending_summary_path.is_none());
            let controls = merge_review_branch_diff_controls(true, Some(&file_path), cx);
            assert!(!controls.awaiting_agent_summary);
            assert!(controls.review_diff_ready);
        });
    }

    #[gpui::test]
    fn branch_diff_controls_show_summarizing_while_pending_capture(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(3);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let file_path = session.items[0].path.clone();
        session.pending_summary_path = Some(file_path.clone());
        set_merge_review_queue_cursor(&mut session, &file_path);
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let controls = merge_review_branch_diff_controls(true, Some(&file_path), cx);
            assert!(controls.awaiting_agent_summary);
            assert!(!controls.review_diff_ready);
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_SUMMARIZING),
                "step_label was {}",
                controls.step_label
            );
        });
    }

    #[gpui::test]
    fn branch_diff_controls_show_review_ready_before_pending_capture(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;

        init_test(cx);
        let mut session = test_session_with_ambiguous_items(3);
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        let file_path = session.items[0].path.clone();
        set_merge_review_queue_cursor(&mut session, &file_path);
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let controls = merge_review_branch_diff_controls(true, Some(&file_path), cx);
            assert!(!controls.awaiting_agent_summary);
            assert!(controls.review_diff_ready);
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_REVIEW_READY),
                "step_label was {}",
                controls.step_label
            );
        });
    }

    #[gpui::test]
    fn merge_review_summary_capture_toast_is_confirmation_only(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("lib.rs".into(), false)],
        );
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
            let capture = merge_review_summary_capture_toast("lib.rs", false, cx);
            let expected = merge_review_toast("File 1/1 · lib.rs summarized.");
            assert!(
                capture == expected,
                "capture toast names the summarized file"
            );
            let with_rail_duplicate = expected.on_click("Next file →", |_, _| {});
            assert!(
                capture != with_rail_duplicate,
                "rail is the canonical CTA — toast must not duplicate Next file"
            );
        });
    }

    #[test]
    fn merge_review_user_visible_strings_are_stable() {
        assert_eq!(
            MERGE_REVIEW_READY_TOAST,
            "Merge review ready — click **Preview merge** when you are ready to run `git merge`."
        );
        assert_eq!(MERGE_REVIEW_STEP_PREPARE, "Prepare");
        assert_eq!(MERGE_REVIEW_STEP_REVIEW_READY, "Review Diff");
        assert_eq!(MERGE_REVIEW_STEP_NEXT_FILE, "Next file");
        assert_eq!(MERGE_REVIEW_STEP_CONFLICT_REVIEW, "Summarize this conflict");
        assert_eq!(
            MERGE_REVIEW_OUT_OF_ORDER_TOAST_SUFFIX,
            " — click **Next file →** to advance"
        );
        assert_eq!(MERGE_REVIEW_STEP_CONFLICT_RESOLVE, "Resolve conflict");
        assert_eq!(
            merge_review_branch_diff_button_label(),
            MERGE_REVIEW_BRANCH_DIFF_BUTTON
        );
        assert_eq!(
            git_ui::project_diff::merge_review_end_branch_diff_button_label(),
            MERGE_REVIEW_END_BRANCH_DIFF_BUTTON
        );
        assert_eq!(MERGE_REVIEW_ENDED_TOAST, "Merge review ended.");
    }

    #[gpui::test]
    fn merge_review_init_wires_branch_diff_button_label_to_git_ui(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        cx.update(|cx| {
            git_ui::init(cx);
            crate::merge_review::init(cx);
            assert_eq!(
                git_ui::project_diff::merge_review_branch_diff_button_label(),
                MERGE_REVIEW_BRANCH_DIFF_BUTTON
            );
            assert_eq!(
                git_ui::project_diff::merge_review_end_branch_diff_button_label(),
                MERGE_REVIEW_END_BRANCH_DIFF_BUTTON
            );
        });
    }

    #[gpui::test]
    fn clear_merge_review_session_removes_persisted_session(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        cx.update(|cx| {
            crate::merge_review::init(cx);
            let session = test_session_with_ambiguous_items(2);
            save_session(cx, &session).expect("save session");
            assert!(merge_review_session_active(cx));
            clear_merge_review_session(cx).expect("clear session");
            assert!(!merge_review_session_active(cx));
        });
    }

    #[test]
    fn persisted_session_without_workflow_does_not_engage_grok_suppression() {
        let session = test_session_with_ambiguous_items(2);
        assert!(!session.focus_layout_active);
    }

    #[gpui::test]
    fn merge_review_workflow_engaged_requires_focus_layout_flag(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        cx.update(|cx| {
            let mut session = test_session_with_ambiguous_items(2);
            save_session(cx, &session).expect("save session");
            assert!(merge_review_session_active(cx));
            assert!(!merge_review_workflow_engaged(cx));
            session.focus_layout_active = true;
            save_session(cx, &session).expect("save engaged session");
            assert!(merge_review_workflow_engaged(cx));
        });
    }

    #[gpui::test]
    fn merge_review_session_active_reflects_persisted_session(cx: &mut gpui::TestAppContext) {
        use crate::test_support::init_test;

        init_test(cx);
        cx.update(|cx| {
            assert!(!merge_review_session_active(cx));
            let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
            let session = build_session(
                &manifest,
                "abc".into(),
                "origin/main".into(),
                [("crates/editor/src/editor.rs".into(), false)],
            );
            save_session(cx, &session).expect("save session");
            assert!(merge_review_session_active(cx));
        });
    }

    #[test]
    fn merge_review_file_prompt_mentions_optional_pattern_line() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), false)],
        );
        let item = item_for_path(&session, "crates/editor/src/editor.rs").unwrap();
        let prompt = merge_review_file_prompt(&session, item, "crates/editor/src/editor.rs");
        assert!(prompt.contains("Pattern:"), "{prompt}");
    }

    #[test]
    fn merge_review_plan_prompt_mentions_branch_diff_and_todos() {
        let session = test_session_with_ambiguous_items(3);
        let prompt = merge_review_plan_prompt(&session);
        assert!(prompt.contains("Branch Diff"), "{prompt}");
        assert!(prompt.contains("todo_write"), "{prompt}");
        assert!(prompt.contains("origin/main"), "{prompt}");
        assert!(
            prompt.contains("never treat the repository root directory as a file"),
            "{prompt}"
        );
    }

    #[test]
    fn reconcile_review_paths_skips_directory_paths() {
        let root =
            std::env::temp_dir().join(format!("merge-review-reconcile-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ref/vibe-palace")).unwrap();
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(root.join("crates/foo.rs"), "").unwrap();

        let name_status = concat!("M\x00ref/vibe-palace\x00", "M\x00crates/foo.rs\x00",);
        let paths = reconcile_review_paths_from_git(&root, name_status, "");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0, "crates/foo.rs");
        assert!(!paths[0].1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_review_paths_marks_unmerged_paths_as_conflicts() {
        let root = std::env::temp_dir().join(format!(
            "merge-review-reconcile-conflict-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("crates/editor/src")).unwrap();
        std::fs::write(root.join("crates/editor/src/editor.rs"), "").unwrap();

        let name_status = concat!(
            "M\x00crates/agent_ui/src/lib.rs\x00",
            "M\x00crates/editor/src/editor.rs\x00",
        );
        let conflicts = "crates/editor/src/editor.rs\0";
        let paths = reconcile_review_paths_from_git(&root, name_status, conflicts);
        assert_eq!(paths.len(), 2);
        let by_path = paths.into_iter().collect::<HashMap<_, _>>();
        assert!(!by_path["crates/agent_ui/src/lib.rs"]);
        assert!(by_path["crates/editor/src/editor.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_review_paths_includes_added_deleted_and_modified() {
        let root = std::env::temp_dir().join(format!(
            "merge-review-reconcile-status-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("crates/new")).unwrap();
        std::fs::write(root.join("crates/new/file.rs"), "").unwrap();

        let name_status = concat!(
            "M\x00crates/modified.rs\x00",
            "A\x00crates/new/file.rs\x00",
            "D\x00crates/removed.rs\x00",
            "R100\x00crates/renamed.rs\x00",
        );
        let paths = reconcile_review_paths_from_git(&root, name_status, "");
        let path_names = paths.into_iter().map(|(path, _)| path).collect::<Vec<_>>();
        assert!(path_names.contains(&"crates/modified.rs".to_string()));
        assert!(path_names.contains(&"crates/new/file.rs".to_string()));
        assert!(path_names.contains(&"crates/removed.rs".to_string()));
        assert!(!path_names.iter().any(|path| path.contains("renamed")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_session_marks_conflicts_over_manifest_disposition() {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let session = build_session(
            &manifest,
            "base".into(),
            "origin/main".into(),
            [("crates/editor/src/editor.rs".into(), true)],
        );
        let item = session
            .items
            .iter()
            .find(|item| item.path == "crates/editor/src/editor.rs")
            .expect("editor path");
        assert_eq!(item.disposition, ReviewDisposition::Conflict);
        assert_eq!(session.conflict_items().count(), 1);
    }

    struct GitMergeFixture {
        _root: tempfile::TempDir,
    }

    impl GitMergeFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp git repo");
            git_cmd(root.path(), &["init", "-b", "surmount"]);
            git_cmd(root.path(), &["config", "user.email", "test@zed.dev"]);
            git_cmd(root.path(), &["config", "user.name", "test"]);
            Self { _root: root }
        }

        fn path(&self) -> &Path {
            self._root.path()
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dirs");
            }
            std::fs::write(path, content).expect("write file");
        }

        fn commit_all(&self, message: &str) {
            git_cmd(self.path(), &["add", "-A"]);
            git_cmd(self.path(), &["commit", "-m", message]);
        }

        fn pin_origin_main_to_head(&self) {
            let sha = git_output(self.path(), &["rev-parse", "HEAD"]);
            git_cmd(
                self.path(),
                &["update-ref", "refs/remotes/origin/main", sha.trim()],
            );
        }

        fn seed_surmount_base(&self) {
            self.write("SURMOUNT.md", "# Surmount fork\n");
            self.write("surmount-merge-categories.toml", TEST_MANIFEST);
            self.write("crates/agent_ui/src/lib.rs", "fork base\n");
            self.write("crates/editor/src/editor.rs", "shared base\n");
            self.commit_all("surmount base");
            self.pin_origin_main_to_head();
        }

        fn diverge_fork_and_upstream(&self) {
            self.seed_surmount_base();
            git_cmd(self.path(), &["checkout", "-b", "upstream-tip"]);
            self.write("crates/editor/src/editor.rs", "upstream editor\n");
            self.commit_all("upstream editor");
            let upstream_sha = git_output(self.path(), &["rev-parse", "HEAD"]);
            git_cmd(
                self.path(),
                &[
                    "update-ref",
                    "refs/remotes/origin/main",
                    upstream_sha.trim(),
                ],
            );
            git_cmd(self.path(), &["checkout", "surmount"]);
            self.write("crates/agent_ui/src/lib.rs", "fork tip\n");
            self.write("crates/editor/src/editor.rs", "fork editor\n");
            self.commit_all("surmount tip");
        }

        fn begin_conflicted_merge(&self) {
            self.diverge_fork_and_upstream();
            let output = git_cmd_allow_fail(self.path(), &["merge", "origin/main"]);
            assert!(
                !output.status.success(),
                "test setup requires a conflicted merge, got: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    #[track_caller]
    fn git_cmd(repo: &Path, args: &[&str]) {
        let output = git_cmd_allow_fail(repo, args);
        assert!(
            output.status.success(),
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[track_caller]
    #[allow(clippy::disallowed_methods, reason = "test git fixture helper")]
    fn git_cmd_allow_fail(repo: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("spawn git")
    }

    fn git_output(repo: &Path, args: &[&str]) -> String {
        let output = git_cmd_allow_fail(repo, args);
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn load_session_from_fixture(fixture: &GitMergeFixture) -> MergeReviewSession {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        smol::block_on(load_merge_review_session_from_git(
            fixture.path(),
            &manifest,
            "origin/main",
        ))
        .expect("load merge review session from git")
    }

    #[test]
    fn real_git_load_session_lists_diverged_paths_with_manifest_classes() {
        let fixture = GitMergeFixture::new();
        fixture.diverge_fork_and_upstream();
        let session = load_session_from_fixture(&fixture);

        assert_eq!(session.upstream_ref, "origin/main");
        assert!(!session.merge_base.is_empty());
        let merge_base = git_output(fixture.path(), &["merge-base", "HEAD", "origin/main"]);
        assert_eq!(session.merge_base, merge_base.trim());

        let items = session
            .items
            .iter()
            .map(|item| (item.path.clone(), item.disposition))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            items.get("crates/agent_ui/src/lib.rs"),
            Some(&ReviewDisposition::ForkOwned)
        );
        assert_eq!(
            items.get("crates/editor/src/editor.rs"),
            Some(&ReviewDisposition::Ambiguous)
        );
        assert_eq!(session.conflict_items().count(), 0);
        assert_eq!(session.items.len(), 2);
    }

    #[test]
    fn real_git_load_session_marks_unmerged_paths_as_conflicts() {
        let fixture = GitMergeFixture::new();
        fixture.begin_conflicted_merge();
        let session = load_session_from_fixture(&fixture);

        let editor = session
            .items
            .iter()
            .find(|item| item.path == "crates/editor/src/editor.rs")
            .expect("conflicted editor path");
        assert_eq!(editor.disposition, ReviewDisposition::Conflict);
        assert_eq!(session.conflict_items().count(), 1);

        let unmerged = git_output(
            fixture.path(),
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        );
        assert!(unmerged.contains("crates/editor/src/editor.rs"));
    }

    #[test]
    fn real_git_load_session_is_empty_when_branches_match() {
        let fixture = GitMergeFixture::new();
        fixture.seed_surmount_base();
        let session = load_session_from_fixture(&fixture);
        assert!(session.items.is_empty());
        assert!(!session.merge_base.is_empty());
    }

    #[test]
    fn real_git_reconcile_paths_match_populated_session_items() {
        let fixture = GitMergeFixture::new();
        fixture.diverge_fork_and_upstream();
        let name_status = git_output(
            fixture.path(),
            &["diff", "--merge-base", "origin/main", "--name-status", "-z"],
        );
        let conflicts = git_output(
            fixture.path(),
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        );
        let reconciled = reconcile_review_paths_from_git(fixture.path(), &name_status, &conflicts);
        let session = load_session_from_fixture(&fixture);
        let session_paths = session
            .items
            .iter()
            .map(|item| {
                (
                    item.path.clone(),
                    matches!(item.disposition, ReviewDisposition::Conflict),
                )
            })
            .collect::<HashMap<_, _>>();
        let reconciled_paths = reconciled.into_iter().collect::<HashMap<_, _>>();
        assert_eq!(session_paths, reconciled_paths);
    }

    fn test_session_with_ambiguous_items(count: usize) -> MergeReviewSession {
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let paths = (0..count)
            .map(|index| (format!("crates/editor/src/file_{index}.rs"), false))
            .collect::<Vec<_>>();
        build_session(&manifest, "abc123".into(), "origin/main".into(), paths)
    }

    async fn setup_zoomed_agent_workspace(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<Workspace>, gpui::VisualTestContext) {
        setup_zoomed_agent_workspace_at(cx, Path::new("/project"), false).await
    }

    async fn setup_zoomed_agent_workspace_with_surmount(
        cx: &mut gpui::TestAppContext,
        project_root: &Path,
    ) -> (gpui::Entity<Workspace>, gpui::VisualTestContext) {
        setup_zoomed_agent_workspace_at(cx, project_root, true).await
    }

    async fn setup_zoomed_agent_workspace_at(
        cx: &mut gpui::TestAppContext,
        project_root: &Path,
        with_git: bool,
    ) -> (gpui::Entity<Workspace>, gpui::VisualTestContext) {
        use agent_settings::AgentSettings;
        use fs::FakeFs;
        use gpui::VisualTestContext;
        use project::Project;
        use serde_json::json;
        use settings::{NotifyWhenAgentWaiting, Settings};
        use workspace::MultiWorkspace;

        use crate::test_support::{init_test, register_test_sidebar};

        init_test(cx);
        cx.update(|cx| {
            git_ui::init(cx);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::PrimaryScreen,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
            crate::merge_review::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        if with_git {
            fs.insert_tree_from_real_fs(project_root, project_root)
                .await;
            let dot_git = project_root.join(".git");
            fs.set_head_for_repo(
                dot_git.as_path(),
                &[("lib.rs", "fn main() {}\n".into())],
                "deadbeef",
            );
            fs.set_index_for_repo(dot_git.as_path(), &[("lib.rs", "fn main() {}\n".into())]);
        } else {
            fs.insert_tree(project_root, json!({ "file.txt": "" }))
                .await;
        }
        let project = Project::test(fs.clone(), [project_root], cx).await;
        cx.executor().run_until_parked();

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        register_test_sidebar(true, &mut cx);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.open_panel::<AgentPanel>(window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.zoom_dock_panel::<AgentPanel>(window, cx);
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_some(),
                "test setup must leave agent dock zoomed"
            );
        });

        (workspace, cx)
    }

    /// Regression: restore must unzoom without emitting `PanelEvent::ZoomOut`, which
    /// would nested-`workspace.update` from Dock and panic (entity_map double-lease).
    #[gpui::test]
    async fn test_restore_merge_review_layout_unzooms_without_nested_workspace_update(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;

        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.docks_collapsed = vec!["left".into()];
        cx.update(|_window, cx| {
            save_session(cx, &session).expect("save merge review session");
        });

        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_some(),
                "precondition: agent dock zoomed before restore"
            );
        });

        // Same lease as production restore paths (observe_new, ActiveItemChanged).
        workspace.update_in(&mut cx, |workspace, window, cx| {
            restore_merge_review_workspace_layout(workspace, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "restore must clear workspace zoomed_position"
            );
        });

        // Idempotent second restore inside the same update style must not panic either.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            restore_merge_review_workspace_layout(workspace, window, cx);
            restore_merge_review_workspace_layout(workspace, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "repeated restore must keep agent dock unzoomed"
            );
        });
    }

    /// Regression for cold-start panic:
    /// `cannot update workspace::Workspace while it is already being updated`
    /// when a persisted merge-review session restores while `initialize_agent_panel`
    /// still calls `open_full_grok_immersive_from_workspace` (logs: restoring layout →
    /// opening full grok surface → suppressing → selected queue file → panic).
    #[gpui::test]
    async fn test_cold_start_merge_review_open_full_grok_avoids_nested_workspace_update(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;

        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.queue_cursor_path = Some("Cargo.lock".into());
        session.branch_diff_selected_path = Some("Cargo.lock".into());
        session.docks_collapsed = vec!["left".into()];
        cx.update(|_window, cx| {
            save_session(cx, &session).expect("save merge review session");
        });

        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_some(),
                "precondition: agent dock zoomed (simulates immersive race)"
            );
        });

        // Same outer lease as production cold start stacking restore + open_full_grok +
        // ActiveItemChanged re-restore while Workspace is already leased.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            restore_merge_review_workspace_layout(workspace, window, cx);
            AgentPanel::open_full_grok_immersive_from_workspace(workspace, window, cx);
            // ActiveItemChanged handler re-enters restore when dock is still zoomed.
            if merge_review_workflow_engaged(cx) && workspace.zoomed_dock_position().is_some() {
                restore_merge_review_workspace_layout(workspace, window, cx);
            }
            // focus_center_pane path after queue select must not re-lease Workspace.
            workspace.focus_center_pane(window, cx);
            restore_merge_review_workspace_layout(workspace, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "merge-review cold start must leave agent dock unzoomed"
            );
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                panic!("agent panel must exist");
            };
            let diagnostics = panel.read(cx).grok_immersive_diagnostics_for_tests(cx);
            assert!(
                !diagnostics.categorized_pending && !diagnostics.startup_in_progress,
                "open_full_grok must suppress immersive startup under merge review (got {diagnostics})"
            );
        });
    }

    /// Regression: GitStore subscribe_in must not nested-`workspace.update` while the
    /// subscription already leases Workspace (entity_map double-lease panic on cold
    /// start when queue select triggers repo open / status updates).
    #[gpui::test]
    async fn test_merge_review_git_store_subscription_avoids_nested_workspace_update(
        cx: &mut gpui::TestAppContext,
    ) {
        use project::git_store::{GitStoreEvent, RepositoryEvent, RepositoryId};

        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;

        let mut session = test_session_with_ambiguous_items(1);
        session.focus_layout_active = true;
        session.worktree_root = Some("/project".into());
        cx.update(|_window, cx| {
            save_session(cx, &session).expect("save merge review session");
        });

        // Install the fixed production-shaped handler against this workspace
        // (observe_new only runs for workspaces created after merge_review::init).
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            cx.subscribe_in(
                &git_store,
                window,
                move |_workspace, _, event, window, cx| {
                    let git_changed = matches!(
                        event,
                        GitStoreEvent::ConflictsUpdated
                            | GitStoreEvent::RepositoryUpdated(
                                _,
                                RepositoryEvent::StatusesChanged,
                                _,
                            )
                    );
                    if !git_changed || !merge_review_session_active(cx) {
                        return;
                    }
                    // Must not re-lease Workspace here (old path used workspace_weak.update_in).
                    spawn_refresh_merge_review_git_state(window, cx);
                },
            )
            .detach();
        });

        // Emit status events while holding a Workspace lease, matching cold-start race
        // where ActiveItemChanged + git open status updates stack.
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            git_store.update(cx, |_git_store, cx| {
                cx.emit(GitStoreEvent::RepositoryUpdated(
                    RepositoryId(0),
                    RepositoryEvent::StatusesChanged,
                    true,
                ));
                cx.emit(GitStoreEvent::ConflictsUpdated);
            });
        });
        cx.run_until_parked();

        // Nested lease would have already panicked. Session remains engaged.
        cx.update(|_window, cx| {
            assert!(
                merge_review_workflow_engaged(cx),
                "session must stay engaged after git-store events"
            );
        });
    }

    /// Regression: file-navigation guard toast must not nested-`workspace.update`
    /// while ProjectDiff::move_to_* runs under an active Workspace lease.
    #[gpui::test]
    async fn test_merge_review_file_navigation_toast_defers_workspace_update(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::init_test;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), true), ("beta.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "alpha.rs");
        cx.update(|cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        // Same outer lease as restore → queue select → diff.move_to_repo_relative_path.
        // Before the fix, the blocked toast nested workspace.update and panicked here.
        let blocked = workspace.update_in(cx, |_, _window, cx| {
            !merge_review_allow_file_navigation("beta.rs", cx.entity(), cx)
        });
        assert!(blocked, "out-of-order path must be blocked");
        cx.run_until_parked();

        // Deferred toast must land after the outer lease ends without panicking.
        cx.update(|_window, cx| {
            assert!(
                merge_review_workflow_engaged(cx),
                "session must stay engaged after deferred navigation toast"
            );
        });
    }

    fn branch_diff_base_refs(workspace: &Workspace, cx: &App) -> Vec<String> {
        workspace
            .items_of_type::<ProjectDiff>(cx)
            .filter_map(|item| match item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => Some(base_ref.to_string()),
                DiffBase::Head => None,
            })
            .collect()
    }

    #[gpui::test]
    async fn test_merge_review_opens_branch_diff_not_queue_tab(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().expect("temp project dir");
        let project_root = temp.path();
        std::fs::create_dir(project_root.join(".git")).expect("create .git");
        std::fs::write(project_root.join("SURMOUNT.md"), "# Surmount fork").expect("SURMOUNT.md");
        std::fs::write(
            project_root.join("surmount-merge-categories.toml"),
            TEST_MANIFEST,
        )
        .expect("manifest");
        std::fs::write(project_root.join("lib.rs"), "fn main() {}\n").expect("lib.rs");

        let (workspace, mut cx) =
            setup_zoomed_agent_workspace_with_surmount(cx, project_root).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let session = test_session_with_ambiguous_items(5);

        let has_surmount_repo = project.read_with(&cx, |project, cx| {
            project.repositories(cx).values().any(|repo| {
                let snapshot = repo.read(cx).snapshot();
                is_surmount_workspace(snapshot.work_directory_abs_path.as_ref())
            })
        });
        assert!(
            has_surmount_repo,
            "test setup must expose a repository whose root contains SURMOUNT.md"
        );
        workspace.read_with(&cx, |workspace, _cx| {
            assert!(
                workspace.zoomed_dock_position().is_some(),
                "test setup must start with agent dock zoomed"
            );
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_dock(DockPosition::Left, window, cx);
            workspace.toggle_dock(DockPosition::Bottom, window, cx);
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(workspace, session, project, window, cx);
        });

        for _ in 0..40 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                !workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .is_open(),
                "merge review must collapse the left project tree dock"
            );
            assert!(
                !workspace
                    .dock_at_position(DockPosition::Bottom)
                    .read(cx)
                    .is_open(),
                "merge review must collapse the bottom terminal dock"
            );
            assert!(
                workspace
                    .items_of_type::<MergeReviewView>(cx)
                    .next()
                    .is_none(),
                "prototype Surmount Merge Review tab must not open"
            );
            let mut base_refs = branch_diff_base_refs(workspace, cx);
            base_refs.sort();
            assert_eq!(
                base_refs,
                vec!["origin/main".to_string()],
                "workflow must open exactly one Branch Diff against origin/main"
            );
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "workflow must zoom out maximized agent dock so Branch Diff is visible"
            );
            let active_base_ref =
                workspace.active_item_as::<ProjectDiff>(cx).map(|item| {
                    match item.read(cx).diff_base(cx) {
                        DiffBase::Merge { base_ref } => base_ref.to_string(),
                        DiffBase::Head => String::new(),
                    }
                });
            assert_eq!(
                active_base_ref.as_deref(),
                Some("origin/main"),
                "workflow must activate the Branch Diff tab"
            );
        });
        cx.update(|_, cx| {
            let session = load_session(cx).expect("merge review session must persist");
            assert_eq!(session.upstream_ref, "origin/main");
            assert_eq!(session.items.len(), 5);
            assert!(
                session.docks_collapsed.len() <= MERGE_REVIEW_FOCUS_DOCKS.len(),
                "session only records docks that were open when focus layout applied"
            );
            assert!(
                merge_review_session_active(cx),
                "merge review session must stay active after workflow opens"
            );
        });
        workspace.read_with(&cx, |workspace, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist");
            let diagnostics = panel.read(cx).grok_immersive_diagnostics_for_tests(cx);
            assert!(
                !diagnostics.has_zed_todos_surface,
                "merge review must not open grok categorized surface (got {diagnostics})"
            );
            assert!(
                !diagnostics.categorized_pending,
                "merge review must not leave grok surface pending (got {diagnostics})"
            );
            assert!(
                !diagnostics.startup_in_progress,
                "merge review must not restart grok immersive startup (got {diagnostics})"
            );
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "agent dock must stay unzoomed after plan posts"
            );
        });
    }

    #[gpui::test]
    async fn test_merge_review_review_branch_diff_suppresses_grok_surface(
        cx: &mut gpui::TestAppContext,
    ) {
        use zed_actions::agent::ReviewBranchDiff;

        let temp = tempfile::tempdir().expect("temp project dir");
        let project_root = temp.path();
        std::fs::create_dir(project_root.join(".git")).expect("create .git");
        std::fs::write(project_root.join("SURMOUNT.md"), "# Surmount fork").expect("SURMOUNT.md");
        std::fs::write(
            project_root.join("surmount-merge-categories.toml"),
            TEST_MANIFEST,
        )
        .expect("manifest");
        std::fs::write(project_root.join("lib.rs"), "fn main() {}\n").expect("lib.rs");

        let (workspace, mut cx) =
            setup_zoomed_agent_workspace_with_surmount(cx, project_root).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let mut session = test_session_with_ambiguous_items(3);
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::MergeInProgress;
        set_merge_review_queue_cursor(&mut session, "lib.rs");

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(workspace, session, project, window, cx);
        });
        for _ in 0..40 {
            cx.run_until_parked();
        }

        cx.update(|window, cx| {
            window.dispatch_action(
                Box::new(ReviewBranchDiff {
                    diff_text: "diff --git a/lib.rs b/lib.rs\n".into(),
                    base_ref: "origin/main".into(),
                    file_path: Some("lib.rs".into()),
                }),
                cx,
            );
        });
        for _ in 0..40 {
            cx.run_until_parked();
        }

        cx.update(|_, cx| {
            assert!(
                merge_review_session_active(cx),
                "merge review session must remain active after Review Diff"
            );
            let session = load_session(cx).expect("session");
            assert_eq!(
                session.pending_summary_path.as_deref(),
                Some("lib.rs"),
                "Review Diff must arm pending summary capture for the selected file"
            );
            let controls = merge_review_branch_diff_controls(true, Some("lib.rs"), cx);
            assert!(
                controls.awaiting_agent_summary,
                "Review Diff in flight must show awaiting-agent-summary controls"
            );
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_SUMMARIZING),
                "step_label was {}",
                controls.step_label
            );
        });
        workspace.read_with(&cx, |workspace, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist");
            let diagnostics = panel.read(cx).grok_immersive_diagnostics_for_tests(cx);
            assert!(
                !diagnostics.has_zed_todos_surface,
                "Review Diff must not open grok categorized surface (got {diagnostics})"
            );
            assert!(
                !diagnostics.categorized_pending,
                "Review Diff must not leave grok surface pending (got {diagnostics})"
            );
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "Review Diff must keep agent dock unzoomed"
            );
            let thread = panel
                .read(cx)
                .active_agent_thread(cx)
                .expect("Review Diff must keep the merge review agent thread active");
            let entries = thread.read(cx).entries().len();
            let generating = thread.read(cx).status() != acp_thread::ThreadStatus::Idle;
            assert!(
                entries > 0 || generating,
                "Review Diff must submit the file prompt to the active thread (entries={entries}, generating={generating})"
            );
            let discipline_kickback = thread.read(cx).entries().iter().any(|entry| {
                let acp_thread::AgentThreadEntry::UserMessage(message) = entry else {
                    return false;
                };
                message.chunks.iter().any(|block| {
                    matches!(
                        block,
                        agent_client_protocol::schema::v1::ContentBlock::Text(text)
                            if text.text.contains("Autonomous Work Discipline rules")
                    )
                })
            });
            assert!(
                !discipline_kickback,
                "Review Diff merge review summaries must not inject Grok discipline kickback"
            );
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist");
            assert!(
                panel.read(cx).focus_handle(cx).contains_focused(window, cx),
                "Review Diff must focus the agent panel so the user sees the review"
            );
        });
    }

    #[gpui::test]
    async fn test_merge_review_end_restores_collapsed_docks(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().expect("temp project dir");
        let project_root = temp.path();
        std::fs::create_dir(project_root.join(".git")).expect("create .git");
        std::fs::write(project_root.join("SURMOUNT.md"), "# Surmount fork").expect("SURMOUNT.md");
        std::fs::write(
            project_root.join("surmount-merge-categories.toml"),
            TEST_MANIFEST,
        )
        .expect("manifest");
        std::fs::write(project_root.join("lib.rs"), "fn main() {}\n").expect("lib.rs");

        let (workspace, mut cx) =
            setup_zoomed_agent_workspace_with_surmount(cx, project_root).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let session = test_session_with_ambiguous_items(3);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            for position in MERGE_REVIEW_FOCUS_DOCKS {
                if !workspace.is_dock_at_position_open(position, cx) {
                    workspace
                        .dock_at_position(position)
                        .update(cx, |dock, cx| dock.set_open(true, window, cx));
                }
            }
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        workspace.read_with(&cx, |workspace, cx| {
            for position in MERGE_REVIEW_FOCUS_DOCKS {
                assert!(
                    workspace.dock_at_position(position).read(cx).is_open(),
                    "test setup must open {position:?} dock before merge review"
                );
            }
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(workspace, session, project, window, cx);
        });
        for _ in 0..40 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                !workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .is_open(),
                "left dock must collapse during merge review"
            );
            assert!(
                !workspace
                    .dock_at_position(DockPosition::Bottom)
                    .read(cx)
                    .is_open(),
                "bottom dock must collapse during merge review"
            );
        });
        cx.update(|_, cx| {
            let session = load_session(cx).expect("session must persist");
            assert_eq!(
                session.docks_collapsed,
                vec!["left".to_string(), "bottom".to_string()],
                "session must record both focus docks for restore"
            );
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            end_merge_review_workflow(workspace, window, cx);
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .is_open(),
                "ending merge review must restore left dock"
            );
            assert!(
                workspace
                    .dock_at_position(DockPosition::Bottom)
                    .read(cx)
                    .is_open(),
                "ending merge review must restore bottom dock"
            );
        });
        cx.update(|_, cx| {
            assert!(
                !merge_review_session_active(cx),
                "ending merge review must clear persisted session"
            );
        });
    }

    #[gpui::test]
    async fn test_merge_review_blocks_grok_reassert_after_workflow(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().expect("temp project dir");
        let project_root = temp.path();
        std::fs::create_dir(project_root.join(".git")).expect("create .git");
        std::fs::write(project_root.join("SURMOUNT.md"), "# Surmount fork").expect("SURMOUNT.md");
        std::fs::write(
            project_root.join("surmount-merge-categories.toml"),
            TEST_MANIFEST,
        )
        .expect("manifest");
        std::fs::write(project_root.join("lib.rs"), "fn main() {}\n").expect("lib.rs");

        let (workspace, mut cx) =
            setup_zoomed_agent_workspace_with_surmount(cx, project_root).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let session = test_session_with_ambiguous_items(3);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(workspace, session, project, window, cx);
        });
        for _ in 0..40 {
            cx.run_until_parked();
        }

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist");
            panel.update(cx, |panel, cx| {
                panel.reassert_grok_immersive_maximized_for_tests(window, cx);
                panel.schedule_grok_immersive_reveal_until_ready_for_tests(window, cx);
            });
        });
        for _ in 0..20 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist");
            let diagnostics = panel.read(cx).grok_immersive_diagnostics_for_tests(cx);
            assert!(
                merge_review_session_active(cx),
                "merge review session must remain active"
            );
            assert!(
                !diagnostics.has_zed_todos_surface,
                "grok reassert must not reopen categorized surface (got {diagnostics})"
            );
            assert!(
                !diagnostics.categorized_pending,
                "grok reassert must not mark surface pending (got {diagnostics})"
            );
            assert!(
                workspace.zoomed_dock_position().is_none(),
                "grok reassert must not re-zoom agent dock during merge review"
            );
        });
    }

    #[gpui::test]
    async fn test_review_diff_stopped_handler_captures_summary_into_session(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::rc::Rc;

        use acp_thread::StubAgentConnection;
        use agent_client_protocol::schema::v1 as acp;

        use crate::test_support::{StubAgentServer, send_message};

        const FILE_PATH: &str = "lib.rs";
        const SUMMARY: &str = "Take upstream; fork has no local edits here.";

        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;

        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
            let mut session = build_session(
                &manifest,
                "abc".into(),
                "origin/main".into(),
                [(FILE_PATH.into(), false)],
            );
            session.focus_layout_active = true;
            save_session(cx, &session).expect("save session");
            set_pending_summary_capture(cx, FILE_PATH).expect("pending capture");
        });

        let stub_connection = StubAgentConnection::new();
        stub_connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(format!("Review complete.\nSummary: {SUMMARY}\n").into()),
        )]);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            if workspace.zoomed_dock_position().is_some() {
                workspace.zoom_dock_panel::<AgentPanel>(window, cx);
            }
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }

        let panel = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel must exist")
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::new(stub_connection.clone()).with_connection_agent_id()),
                window,
                cx,
            );
        });
        for _ in 0..20 {
            cx.run_until_parked();
        }
        panel.read_with(&cx, |panel, cx| {
            assert!(
                panel.active_agent_thread(cx).is_some(),
                "stub thread must connect before sending"
            );
        });
        send_message(&panel, &mut cx);

        for _ in 0..40 {
            cx.run_until_parked();
        }

        cx.update(|_, cx| {
            let session = load_session(cx).expect("session");
            let item = item_for_path(&session, FILE_PATH).expect("item");
            assert_eq!(item.review_state, MergeReviewState::Summarized);
            assert!(
                item.summary
                    .as_ref()
                    .is_some_and(|summary| summary.contains(SUMMARY))
            );
            assert!(session.pending_summary_path.is_none());
            assert_eq!(merge_review_progress_label(&session), "1/1");
            assert_eq!(
                merge_review_header_label_for_item(item).as_deref(),
                Some(merge_review_summary_snippet(SUMMARY).as_str())
            );
        });
    }

    #[gpui::test]
    async fn test_merge_review_without_surmount_skips_branch_diff(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let session = test_session_with_ambiguous_items(3);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(workspace, session, project, window, cx);
        });

        for _ in 0..20 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                branch_diff_base_refs(workspace, cx).is_empty(),
                "workflow without SURMOUNT.md must not open Branch Diff"
            );
        });
    }

    #[gpui::test]
    async fn merge_review_primary_focus_handle_is_focused_after_start_helper(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx, diff, _project) =
            setup_merge_review_branch_diff_fixture(cx, &[("alpha.rs", "alpha head\n")]).await;

        // AC-A chrome-ready order (production open_merge_review_workflow):
        // reveal_branch_diff focuses center, then focus_merge_review_primary_after_start wins.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            assert!(
                reveal_branch_diff_for_merge_review(workspace, "origin/main", window, cx),
                "fixture Branch Diff must be revealable for origin/main"
            );
            focus_merge_review_primary_after_start(workspace, "origin/main", window, cx);
            let handle = diff.read(cx).merge_review_primary_focus_handle();
            assert!(
                handle.is_focused(window),
                "primary Preview merge focus_handle must win after reveal + Start chrome-ready focus"
            );
        });
    }

    /// PreMerge session + open gated Preview merge modal (shared by focus + Dialog paint tests).
    async fn open_preview_merge_modal_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, gpui::VisualTestContext) {
        use crate::test_support::init_test;
        use gpui::VisualTestContext;
        use project::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use workspace::MultiWorkspace;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _| mw.workspace().clone())
            .expect("workspace");
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let manifest = CategoryManifest::from_toml(TEST_MANIFEST).unwrap();
        let mut session = build_session(
            &manifest,
            "abc".into(),
            "origin/main".into(),
            [("alpha.rs".into(), false)],
        );
        session.focus_layout_active = true;
        session.git_mode = git_ui::project_diff::MergeReviewGitMode::PreMerge;
        session.worktree_root = Some(path!("/project").into());
        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save");
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            open_merge_review_gated_merge_modal(workspace, window, cx);
        });
        cx.run_until_parked();
        (workspace, cx)
    }

    #[gpui::test]
    async fn merge_review_preview_modal_focus_handle_is_focused_on_open(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = open_preview_merge_modal_fixture(cx).await;

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let modal = workspace
                .active_modal::<MergeReviewGatedMergeModal>(cx)
                .expect("Preview merge modal must be active");
            assert!(
                modal.focus_handle(cx).is_focused(window),
                "Preview merge modal focus_handle must be focused after open (AC-A)"
            );
        });
    }

    /// Paints the open Preview merge modal and asserts Dialog role + aria name.
    ///
    /// Dialog root intentionally combines `Role::Dialog` with `track_focus` (AC-A),
    /// so GPUI classifies it as **interactive** (`Action::Focus`) and excludes it from
    /// the landmark tally. Room still emits the interactive `[Dialog]` line dogfood
    /// expects; AC-B can use `# focus: [Dialog] "Preview merge · …"`.
    #[gpui::test]
    async fn merge_review_preview_modal_paints_dialog_role(cx: &mut gpui::TestAppContext) {
        use gpui::OutlineDetail;

        let (workspace, mut cx) = open_preview_merge_modal_fixture(cx).await;

        workspace.update_in(&mut cx, |workspace, _, cx| {
            assert!(
                workspace
                    .active_modal::<MergeReviewGatedMergeModal>(cx)
                    .is_some(),
                "Preview merge modal must be active for Dialog paint assert"
            );
        });

        let outline = cx.update(|window, cx| {
            let _ = window.draw(cx);
            window.a11y_outline(OutlineDetail::Room)
        });

        const DIALOG_LABEL: &str = "Preview merge · origin/main";
        let dialog_line = outline
            .lines()
            .find(|line| line.contains("[Dialog]") && line.contains(DIALOG_LABEL));
        let dialog_line = dialog_line.unwrap_or_else(|| {
            panic!(
                "room outline must include interactive [Dialog] line with Preview merge label: {outline}"
            )
        });
        // track_focus → Action::Focus; room prints supported actions as [focus].
        assert!(
            dialog_line.contains("[focus]"),
            "Preview Dialog root should advertise Focus (interactive dual-use): {dialog_line}"
        );
        // After open, product focus is on the modal (AC-A); room header should name Dialog.
        assert!(
            outline.lines().any(|line| {
                line.starts_with("# focus:")
                    && line.contains("[Dialog]")
                    && line.contains(DIALOG_LABEL)
            }),
            "room # focus header should name Preview Dialog after open: {outline}"
        );
    }
}

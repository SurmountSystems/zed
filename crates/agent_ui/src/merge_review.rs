use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashMap;
use git::commit::parse_git_diff_name_status;
use git_ui::project_diff::ProjectDiff;
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{
    Action, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, WeakEntity, Window,
};
use project::{Project, git_store::Repository};
use serde::{Deserialize, Serialize};
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Item, ItemHandle, Panel, Toast, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    dock::{DockPosition, PanelEvent},
    item::{ItemBufferKind, ItemEvent},
    notifications::{NotificationId, NotifyTaskExt},
};

use crate::agent_panel::AgentPanel;
use agent::{ResolveMergeConflictSide, resolve_merge_conflict_with_git};
use project::git_store::branch_diff::DiffBase;
use std::path::PathBuf;
use zed_actions::surmount::{
    EndMergeReview, MarkMergeReviewOpenQuestion, MergeReviewNextFile, OpenMergeReview,
    ResolveMergeReviewConflictOurs, ResolveMergeReviewConflictTheirs, StartMergeReview,
};

pub const MANIFEST_FILE: &str = "surmount-merge-categories.toml";
pub const SESSION_STORAGE_KEY: &[u8] = b"surmount_merge_review_session";
pub(crate) const MERGE_REVIEW_READY_TOAST: &str =
    "Step 1: click a changed file in the list. Step 2: click the green Review Diff button.";
pub(crate) const MERGE_REVIEW_STEP_PICK_FILE: &str = "Step 1 · Pick a file in the list";
pub(crate) const MERGE_REVIEW_STEP_REVIEW_READY: &str =
    "Step 2 · Click Review Diff to summarize this file";
pub(crate) const MERGE_REVIEW_STEP_SUMMARIZING: &str = "Step 2 · Agent summarizing this file…";
pub(crate) const MERGE_REVIEW_STEP_FILE_DONE: &str = "Step 3 · Pick next file in the list";
pub(crate) const MERGE_REVIEW_STEP_CONFLICT_RESOLVE: &str =
    "Step 4 · Apply resolution (colored buttons)";
pub(crate) const MERGE_REVIEW_STEP_CONFLICT_REVIEW: &str =
    "Step 2 · Review Diff — compare fork (left) vs upstream (right)";
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

pub fn is_surmount_workspace(worktree_root: &Path) -> bool {
    worktree_root.join("SURMOUNT.md").is_file()
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
    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        panel.update(cx, |panel, cx| {
            panel.set_zoomed(false, window, cx);
            cx.emit(PanelEvent::ZoomOut);
        });
    }
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

fn reveal_branch_diff_for_merge_review(
    workspace: &mut Workspace,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        return false;
    };
    unzoom_agent_dock_for_merge_review(workspace, window, cx);
    workspace.activate_item(&diff, true, true, window, cx);
    workspace.focus_center_pane(window, cx);
    true
}

fn select_first_merge_review_file(
    workspace: &mut Workspace,
    session: &MergeReviewSession,
    upstream_ref: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(first_path) = session.items.first().map(|item| item.path.as_str()) else {
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
        log::info!("surmount merge review: selected first file {first_path}");
        workspace.focus_center_pane(window, cx);
    }
    selected
}

fn prepare_merge_review_selected_file(
    diff: &Entity<ProjectDiff>,
    path: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let is_conflict = load_session(cx)
        .and_then(|session| item_for_path(&session, path).map(|item| item.disposition))
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
    let Some(item) = item_for_path(&session, &path) else {
        log::warn!("surmount merge review: conflict resolve ignored (path not in session)");
        return false;
    };
    if item.disposition != ReviewDisposition::Conflict {
        log::warn!("surmount merge review: conflict resolve ignored (not a conflict file)");
        return false;
    }
    let Some(worktree_root) = project_worktree_root(diff.read(cx).project(), cx) else {
        log::warn!("surmount merge review: conflict resolve ignored (no worktree)");
        return false;
    };
    let path_for_git = path.clone();
    let workspace_weak = cx.entity().downgrade();
    window
        .spawn(cx, async move |cx| {
            match resolve_merge_conflict_with_git(&worktree_root, &path_for_git, side, true).await
            {
                Ok(message) => {
                    log::info!("surmount merge review: {message}");
                    git_ui::project_diff::ProjectDiff::refresh(
                        diff.downgrade(),
                        git_ui::project_diff::RefreshReason::StatusesChanged,
                        cx,
                    )
                    .await
                    .log_err();
                    let Some(workspace) = workspace_weak.upgrade() else {
                        return;
                    };
                    workspace
                        .update_in(cx, |workspace, _window, cx| {
                            notify_merge_review_ui_changed(workspace, cx);
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
    items.sort_by(|left, right| left.path.cmp(&right.path));
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
    }
}

impl MergeReviewSession {
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
    let normalized = path.replace('\\', "/");
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

const MAX_MERGE_REVIEW_FORMAT_RETRIES: u8 = 2;

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
    if session.pending_summary_path.is_none() {
        return None;
    }
    if end_turn {
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
    let item = item_for_path(&session, file_path)?;
    Some(merge_review_file_prompt(&session, item, file_path))
}

pub fn set_pending_summary_capture(cx: &mut App, path: &str) -> Result<()> {
    let mut session = load_session(cx).context("no merge review session")?;
    let capture_path =
        canonical_merge_review_path(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
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
    if let Some(session) = load_session(cx) {
        let progress = merge_review_progress_label(&session);
        controls.step_label = if controls.is_conflict_file {
            format!("{progress} · {MERGE_REVIEW_STEP_CONFLICT_REVIEW}").into()
        } else {
            format!("{progress} · {MERGE_REVIEW_STEP_REVIEW_READY}").into()
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
    let open_question = reply_indicates_open_question(reply_text);
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

pub fn next_merge_review_file_path(
    session: &MergeReviewSession,
    current_path: Option<&str>,
) -> Option<String> {
    if session.items.is_empty() {
        return None;
    }
    let start_index = current_path.and_then(|current| {
        let normalized = canonical_merge_review_path(session, current)
            .unwrap_or_else(|| current.replace('\\', "/"));
        session
            .items
            .iter()
            .position(|item| item.path == normalized)
    });
    let len = session.items.len();
    for offset in 1..=len {
        let index = start_index
            .map(|start| (start + offset) % len)
            .unwrap_or(offset - 1);
        let item = &session.items[index];
        if item.review_state == MergeReviewState::NotReviewed {
            return Some(item.path.clone());
        }
    }
    None
}

pub fn advance_merge_review_to_next_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(session) = load_session(cx) else {
        log::warn!("surmount merge review: Next file ignored (no session)");
        return false;
    };
    if !session.focus_layout_active {
        log::warn!("surmount merge review: Next file ignored (workflow not engaged)");
        return false;
    }
    let upstream_ref = session.upstream_ref.clone();
    let current_path = workspace
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
    let Some(next_path) = next_merge_review_file_path(&session, current_path.as_deref()) else {
        log::warn!("surmount merge review: Next file ignored (empty session queue)");
        return false;
    };
    let Some(diff) = workspace.items_of_type::<ProjectDiff>(cx).find(|item| {
        matches!(
            item.read(cx).diff_base(cx),
            DiffBase::Merge { base_ref } if base_ref.as_ref() == upstream_ref
        )
    }) else {
        log::warn!("surmount merge review: Next file ignored (Branch Diff not open)");
        return false;
    };
    let selected = diff.update(cx, |diff, cx| {
        diff.move_to_repo_relative_path(&next_path, window, cx)
    });
    if selected {
        log::info!("surmount merge review: advanced to next file {next_path}");
        prepare_merge_review_selected_file(&diff, &next_path, window, cx);
        workspace.activate_item(&diff, true, true, window, cx);
        workspace.focus_center_pane(window, cx);
        notify_merge_review_ui_changed(workspace, cx);
    } else {
        log::warn!(
            "surmount merge review: Next file could not open {next_path} in Branch Diff multibuffer"
        );
    }
    selected
}

pub fn merge_review_summary_saved_toast(path: &str, advanced_to_next: bool, cx: &App) -> String {
    let progress = load_session(cx)
        .map(|session| merge_review_progress_label(&session))
        .unwrap_or_else(|| "?/?".to_string());
    if advanced_to_next {
        format!("Saved {path} ({progress}). Advanced to next file — click green Review Diff.")
    } else if load_session(cx)
        .is_some_and(|session| session.reviewed_count() == session.items.len())
    {
        format!("Saved {path} ({progress}). All files reviewed — click End merge review.")
    } else {
        format!("Saved {path} ({progress}). Click green **Next file →** to continue.")
    }
}

pub fn merge_review_summary_capture_toast(
    saved_path: &str,
    advanced_to_next: bool,
    cx: &App,
) -> Toast {
    use crate::merge_review_step_rail::{
        RAIL_BTN_END, RAIL_BTN_KEEP_FORK, RAIL_BTN_NEXT_FILE, RAIL_BTN_REVIEW_DIFF,
        RAIL_BTN_TAKE_UPSTREAM,
    };
    use git_ui::project_diff::ReviewDiff;

    let progress = load_session(cx)
        .map(|session| merge_review_progress_label(&session))
        .unwrap_or_else(|| "?/?".to_string());
    let session = load_session(cx);
    let session_complete = session.as_ref().is_some_and(|session| {
        !session.items.is_empty() && session.reviewed_count() == session.items.len()
    });

    let (button_label, action): (&'static str, Box<dyn Action>) = if session_complete {
        (RAIL_BTN_END, Box::new(EndMergeReview))
    } else if let Some(item) = session
        .as_ref()
        .and_then(|session| item_for_path(session, saved_path))
        && item.disposition == ReviewDisposition::Conflict
        && item.review_state == MergeReviewState::Summarized
        && !advanced_to_next
    {
        match item.suggested_outcome {
            Some(MergeReviewSuggestedOutcome::KeepFork) => {
                (RAIL_BTN_KEEP_FORK, Box::new(ResolveMergeReviewConflictOurs))
            }
            _ => (
                RAIL_BTN_TAKE_UPSTREAM,
                Box::new(ResolveMergeReviewConflictTheirs),
            ),
        }
    } else if advanced_to_next {
        (RAIL_BTN_REVIEW_DIFF, Box::new(ReviewDiff))
    } else {
        (RAIL_BTN_NEXT_FILE, Box::new(MergeReviewNextFile))
    };

    merge_review_toast(format!("Saved {saved_path} ({progress}).")).on_click(
        button_label,
        move |_, cx| {
            cx.dispatch_action(action.as_ref());
        },
    )
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
    expanded_categories: HashSet<String>,
    show_all_actionable: bool,
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
            expanded_categories: HashSet::default(),
            show_all_actionable: false,
            first_render_logged: false,
        }
    }

    fn set_item_verdict(&mut self, path: &str, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        self.session.set_verdict(path, verdict);
        if save_session(cx, &self.session).is_err() {
            log::error!("failed to persist merge review session");
        }
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
                for attempt in 0..120 {
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
                    let session_paths = session_for_plan
                        .items
                        .first()
                        .map(|item| item.path.clone());
                    let upstream_select = upstream_for_find.clone();
                    for attempt in 0..80 {
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
                                select_first_merge_review_file(
                                    workspace,
                                    &session_for_plan,
                                    &upstream_select,
                                    window,
                                    cx,
                                )
                            })
                            .ok()
                            .unwrap_or(false);
                        if selected {
                            break;
                        }
                    }
                    let Some(workspace) = workspace_weak.upgrade() else {
                        return anyhow::Ok(());
                    };
                    workspace.update_in(cx, |workspace, _window, cx| {
                        let toast = if session_paths.is_some() {
                            format!(
                                "{} First file selected — green Review Diff is ready.",
                                MERGE_REVIEW_READY_TOAST
                            )
                        } else {
                            MERGE_REVIEW_READY_TOAST.to_string()
                        };
                        workspace.show_toast(merge_review_toast(toast), cx);
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak_for_err, window, cx);
    }

    fn start_review(
        workspace: &mut Workspace,
        _: &StartMergeReview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repo) = surmount_repository(&project, cx) else {
            log::warn!("surmount merge review: no repository with SURMOUNT.md at root");
            workspace.show_toast(
                merge_review_toast(
                    "Merge review: open the zed project root (SURMOUNT.md), not a submodule.",
                ),
                cx,
            );
            return;
        };
        let worktree_root = repo
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_path_buf();
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
            let session = populate_session_from_git(
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
            if let Some(workspace) = workspace_handle_for_task.upgrade() {
                workspace.update_in(cx, |workspace, window, cx| {
                    Self::open_merge_review_workflow(
                        workspace,
                        session,
                        project_for_workflow,
                        window,
                        cx,
                    );
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
            Self::open_merge_review_workflow(workspace, session, project, window, cx);
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
        if !advance_merge_review_to_next_file(workspace, window, cx) {
            workspace.show_toast(
                merge_review_toast(
                    "Could not open the next file — ensure Branch Diff is open for this merge.",
                ),
                cx,
            );
        }
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
    let session = build_session(manifest, merge_base, upstream_ref.to_string(), paths);
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
            let visible_count =
                initial_actionable_visible_count(action_items.len(), self.show_all_actionable);
            body = body.child(Label::new(format!(
                "Needs human review ({})",
                action_items.len()
            )));
            for item in action_items.iter().take(visible_count) {
                body = body.child(render_merge_review_item_row(item, cx));
            }
            if !self.show_all_actionable && action_items.len() > MAX_INITIAL_ACTIONABLE_ITEMS {
                let remaining = action_items.len() - MAX_INITIAL_ACTIONABLE_ITEMS;
                body = body.child(
                    Button::new(
                        "merge-review-show-all-actionable",
                        format!("Show all {remaining} more"),
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_all_actionable = true;
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
            let expanded = self.expanded_categories.contains(&category_id);
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
                    if this.expanded_categories.contains(&toggle_id) {
                        this.expanded_categories.remove(&toggle_id);
                    } else {
                        this.expanded_categories.insert(toggle_id.clone());
                    }
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
        let summary = item_for_path(&session, &path)
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
                Label::new(MERGE_REVIEW_STEP_PICK_FILE)
                    .size(LabelSize::Small)
                    .color(Color::Warning),
            );
        };
        let Some(item) = item_for_path(&session, &path) else {
            if workflow_engaged {
                return div();
            }
            return h_flex().gap_2().items_center().child(
                Label::new(MERGE_REVIEW_STEP_PICK_FILE)
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
        h_flex()
            .gap_2()
            .items_center()
            .max_w(px(360.))
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
    let item = item_for_path(&session, path)?;
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
    };
    if !merge_review_workflow_engaged(cx) {
        return controls;
    }
    controls.workflow_active = true;
    let Some(session) = load_session(cx) else {
        controls.step_label = MERGE_REVIEW_STEP_PICK_FILE.into();
        return controls;
    };
    let progress = merge_review_progress_label(&session);
    controls.progress_label = progress.clone().into();
    let Some(path) = selected_path else {
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_PICK_FILE}").into();
        return controls;
    };
    let canonical_path =
        canonical_merge_review_path(&session, path).unwrap_or_else(|| path.replace('\\', "/"));
    if session.pending_summary_path.as_deref() == Some(canonical_path.as_str()) {
        controls.awaiting_agent_summary = true;
        controls.review_diff_ready = false;
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_SUMMARIZING}").into();
        if let Some(item) = item_for_path(&session, &canonical_path) {
            controls.is_conflict_file = item.disposition == ReviewDisposition::Conflict;
        }
        return controls;
    }
    let Some(item) = item_for_path(&session, &canonical_path) else {
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_PICK_FILE}").into();
        return controls;
    };
    controls.is_conflict_file = item.disposition == ReviewDisposition::Conflict;
    controls.suggested_outcome = item
        .suggested_outcome
        .map(conflict_outcome_hint_for_toolbar);
    if controls.is_conflict_file && item.review_state == MergeReviewState::Summarized {
        controls.show_conflict_resolution = true;
        controls.current_file_done = true;
        controls.review_diff_ready = false;
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_CONFLICT_RESOLVE}").into();
        return controls;
    }
    if item.review_state == MergeReviewState::Summarized {
        controls.current_file_done = true;
        controls.review_diff_ready = false;
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_FILE_DONE}").into();
        return controls;
    }
    if file_selected {
        controls.review_diff_ready = true;
        controls.step_label = if controls.is_conflict_file {
            format!("{progress} · {MERGE_REVIEW_STEP_CONFLICT_REVIEW}").into()
        } else {
            format!("{progress} · {MERGE_REVIEW_STEP_REVIEW_READY}").into()
        };
    } else {
        controls.step_label = format!("{progress} · {MERGE_REVIEW_STEP_PICK_FILE}").into();
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
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(MergeReviewView::start_review);
        workspace.register_action(MergeReviewView::open_review);
        workspace.register_action(MergeReviewView::end_review);
        workspace.register_action(MergeReviewView::next_file);
        workspace.register_action(MergeReviewView::resolve_conflict_ours);
        workspace.register_action(MergeReviewView::resolve_conflict_theirs);
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
        cx.update(|cx| {
            crate::merge_review::init(cx);
            session.focus_layout_active = true;
            save_session(cx, &session).expect("save session");
            let controls = merge_review_branch_diff_controls(true, Some("lib.rs"), cx);
            assert!(controls.workflow_active);
            assert!(controls.current_file_done);
            assert!(!controls.review_diff_ready);
            assert!(
                controls.step_label.contains(MERGE_REVIEW_STEP_FILE_DONE),
                "got {}",
                controls.step_label
            );
            assert!(
                controls.step_label.starts_with("1/1"),
                "got {}",
                controls.step_label
            );
        });
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
            assert!(
                controls
                    .step_label
                    .contains(MERGE_REVIEW_STEP_CONFLICT_RESOLVE),
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
            assert!(toast.contains("Advanced"), "{toast}");
            assert!(toast.contains("Review Diff"), "{toast}");
        });
    }

    #[test]
    fn merge_review_step_rail_labels_are_stable() {
        use crate::merge_review_step_rail::{
            RAIL_BTN_END, RAIL_BTN_KEEP_FORK, RAIL_BTN_NEXT_FILE, RAIL_BTN_REVIEW_DIFF,
            RAIL_BTN_REVIEW_WORKING, RAIL_BTN_TAKE_UPSTREAM,
        };

        assert_eq!(RAIL_BTN_REVIEW_DIFF, "Review Diff");
        assert_eq!(RAIL_BTN_REVIEW_WORKING, "Summarizing…");
        assert_eq!(RAIL_BTN_NEXT_FILE, "Next file →");
        assert_eq!(RAIL_BTN_KEEP_FORK, "Keep fork");
        assert_eq!(RAIL_BTN_TAKE_UPSTREAM, "Take upstream");
        assert_eq!(RAIL_BTN_END, "End merge review");
    }

    #[test]
    fn merge_review_ui_step_and_primary_action_cover_all_states() {
        use crate::merge_review_step_rail::{
            MergeReviewPrimaryAction, MergeReviewUiStep, merge_review_primary_action,
            merge_review_ui_step,
        };
        use git_ui::project_diff::{MergeReviewBranchDiffControls, MergeReviewConflictOutcomeHint};

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
        };

        let steps = [
            merge_review_ui_step(&base, false, false, true),
            merge_review_ui_step(&base, true, true, false),
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    show_conflict_resolution: true,
                    is_conflict_file: true,
                    suggested_outcome: Some(MergeReviewConflictOutcomeHint::KeepFork),
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
            merge_review_primary_action(MergeReviewUiStep::ConflictResolve {
                emphasize: Some(MergeReviewConflictOutcomeHint::TakeUpstream)
            }),
            MergeReviewPrimaryAction::TakeUpstream
        );
        assert_eq!(
            merge_review_ui_step(
                &MergeReviewBranchDiffControls {
                    awaiting_agent_summary: true,
                    review_diff_ready: true,
                    ..base.clone()
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
        let mut session = test_session_with_ambiguous_items(3);
        session.focus_layout_active = true;
        let file_path = session.items[0].path.clone();
        session.pending_summary_path = Some(file_path.clone());

        cx.update(|_, cx| {
            crate::merge_review::init(cx);
            save_session(cx, &session).expect("save session");
        });
        workspace.read_with(&cx, |workspace, cx| {
            let mut controls = merge_review_branch_diff_controls(true, Some(&file_path), cx);
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
        let file_path = session.items[0].path.clone();
        session.pending_summary_path = Some(file_path.clone());
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
        let file_path = session.items[0].path.clone();
        session.pending_summary_path = Some(file_path.clone());
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
        let file_path = session.items[0].path.clone();
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
    fn merge_review_summary_capture_toast_always_has_action(cx: &mut gpui::TestAppContext) {
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
            let capture = merge_review_summary_capture_toast("lib.rs", true, cx);
            let plain = merge_review_toast("Saved lib.rs (0/1).");
            if capture == plain {
                panic!("capture toast must embed a primary action button");
            }
            let capture = merge_review_summary_capture_toast("lib.rs", false, cx);
            if capture == plain {
                panic!("capture toast must embed a primary action button");
            }
        });
    }

    #[test]
    fn merge_review_user_visible_strings_are_stable() {
        assert_eq!(
            MERGE_REVIEW_READY_TOAST,
            "Step 1: click a changed file in the list. Step 2: click the green Review Diff button."
        );
        assert_eq!(
            MERGE_REVIEW_STEP_PICK_FILE,
            "Step 1 · Pick a file in the list"
        );
        assert_eq!(
            MERGE_REVIEW_STEP_REVIEW_READY,
            "Step 2 · Click Review Diff to summarize this file"
        );
        assert_eq!(
            MERGE_REVIEW_STEP_FILE_DONE,
            "Step 3 · Pick next file in the list"
        );
        assert_eq!(
            MERGE_REVIEW_STEP_CONFLICT_REVIEW,
            "Step 2 · Review Diff — compare fork (left) vs upstream (right)"
        );
        assert_eq!(
            MERGE_REVIEW_STEP_CONFLICT_RESOLVE,
            "Step 4 · Apply resolution (colored buttons)"
        );
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
        build_session(
            &manifest,
            "abc123".into(),
            "origin/main".into(),
            paths.into_iter(),
        )
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
            workspace.add_panel(panel.clone(), window, cx);
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
        let session = test_session_with_ambiguous_items(3);

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
                        agent_client_protocol::schema::ContentBlock::Text(text)
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
        use agent_client_protocol::schema as acp;

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
}

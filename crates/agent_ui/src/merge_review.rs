use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashMap;
use git::commit::parse_git_diff_name_status;
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{
    Action, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, WeakEntity, Window,
};
use project::Project;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use ui::{Button, ButtonStyle, Color, Icon, IconName, Label, prelude::*};
use git_ui::project_diff::ProjectDiff;
use workspace::{
    Item, ItemHandle, Toast, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    item::{ItemBufferKind, ItemEvent},
    notifications::{NotificationId, NotifyTaskExt},
};

use crate::agent_panel::AgentPanel;
use project::git_store::branch_diff::DiffBase;
use zed_actions::surmount::{MarkMergeReviewOpenQuestion, OpenMergeReview, StartMergeReview};

pub const MANIFEST_FILE: &str = "surmount-merge-categories.toml";
pub const SESSION_STORAGE_KEY: &[u8] = b"surmount_merge_review_session";
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
    session.running_notes.push_str(&format!("{normalized}: {summary}"));
    Ok(())
}

pub fn extract_summary_from_agent_reply(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("Summary:")
            .or_else(|| trimmed.strip_prefix("summary:"))
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(str::to_string)
    })
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
        if let Err(error) = save_session(cx, &session) {
            log::error!("surmount merge review: failed to persist session: {error:#}");
        }
        let upstream_ref: SharedString = session.upstream_ref.clone().into();
        if let Some(repository) = project.read(cx).active_repository(cx) {
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
            log::warn!("surmount merge review: no active repository for Branch Diff");
        }
        workspace.open_panel::<AgentPanel>(window, cx);
        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| {
                panel.start_merge_review_plan(&session, window, cx);
            });
            log::info!("surmount merge review: posted plan to agent thread");
        }
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<MergeReviewView>(),
                format!(
                    "Branch Diff open — merge plan in agent ({ambiguous_count} shared-upstream guesses)"
                ),
            ),
            cx,
        );
    }

    fn start_review(
        workspace: &mut Workspace,
        _: &StartMergeReview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(worktree_root) = project.read(cx).active_repository(cx).map(|repo| {
            repo.read(cx)
                .snapshot()
                .work_directory_abs_path
                .to_path_buf()
        }) else {
            log::warn!(
                "surmount merge review: no active git repository; select the zed repo in the git panel"
            );
            return;
        };
        log::info!(
            "surmount merge review: start requested (worktree={})",
            worktree_root.display()
        );
        if !is_surmount_workspace(&worktree_root) {
            log::warn!(
                "surmount merge review: skipped — SURMOUNT.md not found in {}",
                worktree_root.display()
            );
            return;
        }
        let manifest = match load_manifest_from_worktree(&worktree_root) {
            Ok(manifest) => manifest,
            Err(error) => {
                log::error!("failed to load surmount merge manifest: {error:#}");
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
        }
    }
}

async fn populate_session_from_git(
    project: Entity<Project>,
    worktree_root: &Path,
    manifest: CategoryManifest,
    upstream_ref: &str,
    _cx: &mut AsyncApp,
) -> Result<MergeReviewSession> {
    let worktree_root = worktree_root.to_path_buf();
    let upstream_ref = upstream_ref.to_string();
    let merge_base = run_git(&worktree_root, &["merge-base", "HEAD", &upstream_ref]).await?;
    let name_status = run_git(
        &worktree_root,
        &["diff", "--merge-base", &upstream_ref, "--name-status", "-z"],
    )
    .await?;
    let conflict_output = run_git(
        &worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await
    .unwrap_or_default();
    let conflict_paths = conflict_output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let _ = &project;
    let paths = parse_git_diff_name_status(&name_status)
        .map(|(path, _)| {
            let is_conflict = conflict_paths.contains(path);
            (path.to_string(), is_conflict)
        })
        .collect::<Vec<_>>();
    let path_count = paths.len();
    let session = build_session(&manifest, merge_base, upstream_ref, paths);
    debug_assert_eq!(
        session.items.len(),
        path_count,
        "every changed path must become a review item"
    );
    Ok(session)
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
                    Button::new(element_id_for_path("merge-review-fork", &path_fork), "Accept Fork")
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
                    Button::new("merge-review-show-all-actionable", format!("Show all {remaining} more"))
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
        Self {
            project_diff: None,
        }
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
                .is_some_and(|item| {
                    matches!(item.read(cx).diff_base(cx), DiffBase::Merge { .. })
                });
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
        let Some(path) = project_diff.read(cx).active_file_repo_path(cx) else {
            return div();
        };
        let Some(item) = item_for_path(&session, &path) else {
            return div();
        };
        let summary_label = item
            .summary
            .as_deref()
            .map(merge_review_summary_snippet)
            .unwrap_or_else(|| "Not summarized yet".to_string());
        h_flex()
            .gap_2()
            .items_center()
            .max_w_96()
            .child(
                Label::new(item.surmount_section.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(item.review_state.label())
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .child(
                Label::new(summary_label)
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .child(
                Button::new("merge-review-open-question", "Open question")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&MarkMergeReviewOpenQuestion, window, cx);
                    })),
            )
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(MergeReviewView::start_review);
        workspace.register_action(MergeReviewView::open_review);
        workspace.register_action(
            |workspace, _: &MarkMergeReviewOpenQuestion, _window, cx| {
                let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
                    return;
                };
                MergeReviewToolbar::mark_open_question(&project_diff, cx);
            },
        );
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
         Your tasks:\n\
         1. Propose an order to review by SURMOUNT section (conflicts first).\n\
         2. When I select a file, summarize the actual diff hunks: what changed, fork vs upstream, \
            how it relates to earlier summaries in this session.\n\
         3. Use todo_write only for items you cannot resolve with high confidence — those become Plan Todos.\n\
         4. As summaries accumulate, apply the same reasoning to similar files without asking me again.\n\
         5. Only cite diff text; do not invent changes. Draft SURMOUNT.md prose per section when asked.",
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
         Use todo_write only if genuinely stuck — plain one-line questions become Plan Todos.\n\
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
        session.patterns.push("Upstream renamed this module.".into());
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
        assert!(extract_summary_from_agent_reply("No summary here.").is_none());
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
    fn test_review_state_labels_are_plain_language() {
        assert_eq!(MergeReviewState::NotReviewed.label(), "Not reviewed yet");
        assert_eq!(MergeReviewState::Summarized.label(), "Summarized");
        assert_eq!(MergeReviewState::OpenQuestion.label(), "Open question");
    }

    #[test]
    fn merge_review_plan_prompt_mentions_branch_diff_and_todos() {
        let session = test_session_with_ambiguous_items(3);
        let prompt = merge_review_plan_prompt(&session);
        assert!(prompt.contains("Branch Diff"), "{prompt}");
        assert!(prompt.contains("todo_write"), "{prompt}");
        assert!(prompt.contains("origin/main"), "{prompt}");
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
        use agent_settings::AgentSettings;
        use fs::FakeFs;
        use gpui::VisualTestContext;
        use project::Project;
        use serde_json::json;
        use settings::{NotifyWhenAgentWaiting, Settings};
        use std::path::Path;
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
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| multi_workspace.workspace().clone())
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

    #[gpui::test]
    async fn test_merge_review_opens_branch_diff_not_queue_tab(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let session = test_session_with_ambiguous_items(5);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::open_merge_review_workflow(
                workspace,
                session,
                project,
                window,
                cx,
            );
        });

        for _ in 0..12 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace.items_of_type::<MergeReviewView>(cx).next().is_none(),
                "prototype Surmount Merge Review tab must not open"
            );
        });
        cx.update(|_, cx| {
            assert!(
                load_session(cx).is_some(),
                "merge review session must persist after workflow opens"
            );
        });
    }
}

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashMap;
use git::commit::parse_git_diff_name_status;
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, WeakEntity, Window,
};
use project::Project;
use serde::{Deserialize, Serialize};
use ui::{Button, ButtonStyle, Color, Icon, IconName, Label, prelude::*};
use workspace::{
    Item, Toast, Workspace,
    dock::PanelEvent,
    item::{ItemBufferKind, ItemEvent},
    notifications::{NotificationId, NotifyTaskExt},
};

use crate::agent_panel::AgentPanel;
use zed_actions::surmount::{OpenMergeReview, StartMergeReview};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeReviewRevealDiagnostics {
    pub had_zoomed_dock: bool,
    pub emitted_zoom_out: bool,
    pub activated_item: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MergeReviewSession {
    pub merge_base: String,
    pub upstream_ref: String,
    pub items: Vec<MergeReviewItem>,
    pub categories_completed: HashSet<String>,
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

    fn reveal_tab(
        workspace: &mut Workspace,
        view: Entity<Self>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> MergeReviewRevealDiagnostics {
        let had_zoomed_dock = workspace.zoomed_dock_position().is_some();
        log::info!(
            "surmount merge review: reveal_tab begin (zoomed_dock={had_zoomed_dock:?})"
        );
        workspace.activate_item(&view, true, true, window, cx);
        let mut emitted_zoom_out = false;
        if had_zoomed_dock {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                panel.update(cx, |_, cx| cx.emit(PanelEvent::ZoomOut));
                emitted_zoom_out = true;
                log::info!("surmount merge review: reveal_tab emitted PanelEvent::ZoomOut");
            } else {
                log::warn!("surmount merge review: reveal_tab zoomed dock but no AgentPanel");
            }
        }
        workspace.focus_center_pane(window, cx);
        let diagnostics = MergeReviewRevealDiagnostics {
            had_zoomed_dock,
            emitted_zoom_out,
            activated_item: true,
        };
        log::info!(
            "surmount merge review: reveal_tab complete (zoomed_dock_after={:?}, diagnostics={diagnostics:?})",
            workspace.zoomed_dock_position()
        );
        diagnostics
    }

    #[cfg(test)]
    pub(crate) fn test_surface_state(&self) -> (bool, bool) {
        (self.first_render_logged, self.show_all_actionable)
    }

    fn set_item_verdict(&mut self, path: &str, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        self.session.set_verdict(path, verdict);
        if save_session(cx, &self.session).is_err() {
            log::error!("failed to persist merge review session");
        }
        cx.notify();
    }

    pub fn deploy(
        workspace: &mut Workspace,
        session: MergeReviewSession,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        log::info!(
            "surmount merge review: deploying tab ({} items)",
            session.items.len()
        );
        let workspace_handle = cx.entity().downgrade();
        let existing = workspace.items_of_type::<Self>(cx).next();
        let ambiguous_count = session.ambiguous_items().count();
        let view = if let Some(existing) = existing {
            existing.update(cx, |view, cx| {
                view.session = session;
                view.show_all_actionable = false;
                cx.notify();
            });
            existing
        } else {
            let view = cx.new(|cx| Self::new(session, workspace_handle.clone(), window, cx));
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        };
        let view_for_reveal = view.clone();
        window.defer(cx, move |window, cx| {
            log::info!("surmount merge review: deferred reveal starting");
            if let Some(workspace) = workspace_handle.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    let diagnostics =
                        Self::reveal_tab(workspace, view_for_reveal, window, cx);
                    log::info!(
                        "surmount merge review: deferred reveal complete ({diagnostics:?})"
                    );
                });
            } else {
                log::error!(
                    "surmount merge review: workspace dropped before deferred reveal"
                );
            }
        });
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<MergeReviewView>(),
                format!(
                    "Surmount merge review opened ({ambiguous_count} items need review)"
                ),
            ),
            cx,
        );
        log::info!(
            "surmount merge review: tab opened and center pane focused ({ambiguous_count} ambiguous)"
        );
        let session_to_save = view.read(cx).session.clone();
        cx.defer(move |cx| {
            if let Err(error) = save_session(cx, &session_to_save) {
                log::error!("surmount merge review: failed to persist session: {error:#}");
            }
        });
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
                    Self::deploy(workspace, session, window, cx);
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
            Self::deploy(workspace, session, window, cx);
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

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(MergeReviewView::start_review);
        workspace.register_action(MergeReviewView::open_review);
    })
    .detach();
}

pub fn surmount_merge_review_prompt(base_ref: &str, category_id: Option<&str>) -> String {
    let category_hint = category_id
        .map(|id| format!("Category: {id}. "))
        .unwrap_or_default();
    format!(
        "{category_hint}Review this diff for merging upstream `{base_ref}` into the Surmount fork. \
         Use SURMOUNT.md categories. Draft concise documentation for observable changes only. \
         Mark uncertainty with TODO:. Emit todo_write items only for ambiguous files that need human judgment. \
         Prefer upstream for unrelated files; preserve fork intent for fork-owned paths."
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

    /// Regression (2026-06-19): center pane focus dismisses zoom by closing the agent dock;
    /// merge review must unzoom via ZoomOut and keep the dock open.
    #[gpui::test]
    async fn test_merge_review_deploy_unzooms_without_closing_agent_dock(
        cx: &mut gpui::TestAppContext,
    ) {
        use agent_settings::AgentSettings;
        use settings::Settings;

        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;
        let dock_position = workspace.read_with(&cx, |_, cx| {
            AgentSettings::get_global(cx).dock.into()
        });

        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(
                workspace.zoomed_dock_position(),
                Some(dock_position),
                "precondition: agent dock must start zoomed"
            );
            assert!(
                workspace.dock_at_position(dock_position).read(cx).is_open(),
                "precondition: agent dock must start open"
            );
        });

        let session = test_session_with_ambiguous_items(5);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::deploy(workspace, session, window, cx);
        });

        for _ in 0..12 {
            cx.run_until_parked();
        }

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace.items_of_type::<MergeReviewView>(cx).next().is_some(),
                "merge review tab must exist in workspace"
            );
            assert_eq!(
                workspace.zoomed_dock_position(),
                None,
                "merge review reveal must unzoom the dock without leaving it zoomed"
            );
            assert!(
                workspace.dock_at_position(dock_position).read(cx).is_open(),
                "agent dock must stay open (must not use dismiss_zoomed pane focus)"
            );
        });
    }

    #[gpui::test]
    async fn test_merge_review_deploy_completes_first_render(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = setup_zoomed_agent_workspace(cx).await;
        let session = test_session_with_ambiguous_items(30);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            MergeReviewView::deploy(workspace, session, window, cx);
        });

        for _ in 0..12 {
            cx.run_until_parked();
        }

        let view = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<MergeReviewView>(cx).next()
            })
            .expect("merge review tab");
        view.read_with(&cx, |view, _cx| {
            let (first_render_logged, show_all_actionable) = view.test_surface_state();
            assert!(
                first_render_logged,
                "first render must complete without crashing"
            );
            assert!(!show_all_actionable, "deploy must reset show_all_actionable");
            assert_eq!(
                initial_actionable_visible_count(pending_action_items(&view.session).len(), false),
                25,
                "first render must cap actionable rows"
            );
        });
    }
}

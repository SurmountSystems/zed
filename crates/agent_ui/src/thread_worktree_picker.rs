use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use collections::HashSet;
use fuzzy::StringMatchCandidate;
use git::repository::Worktree as GitWorktree;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription, Task, Window, rems,
};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::Project;
use project::git_store::RepositoryEvent;
use ui::{Divider, HighlightedLabel, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use util::paths::PathExt;

use crate::{CreateWorktree, NewWorktreeBranchTarget, SwitchWorktree};

#[derive(Clone)]
struct MergedWorktree {
    name: String,
    paths: Vec<PathBuf>,
    representative: GitWorktree,
}

fn merge_worktrees(all_repo_worktrees: Vec<Vec<GitWorktree>>) -> Vec<MergedWorktree> {
    let mut by_name: BTreeMap<String, MergedWorktree> = BTreeMap::new();

    for repo_worktrees in &all_repo_worktrees {
        let main_worktree_path = repo_worktrees
            .iter()
            .find(|wt| wt.is_main)
            .map(|wt| wt.path.clone());

        for worktree in repo_worktrees {
            let name = worktree.directory_name(main_worktree_path.as_deref());

            if let Some(existing) = by_name.get_mut(&name) {
                existing.paths.push(worktree.path.clone());
            } else {
                by_name.insert(
                    name.clone(),
                    MergedWorktree {
                        name,
                        paths: vec![worktree.path.clone()],
                        representative: worktree.clone(),
                    },
                );
            }
        }
    }

    by_name.into_values().collect()
}

pub(crate) struct ThreadWorktreePicker {
    picker: Entity<Picker<ThreadWorktreePickerDelegate>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ThreadWorktreePicker {
    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project_worktree_paths: HashSet<PathBuf> = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|wt| wt.read(cx).abs_path().to_path_buf())
            .collect();

        let repositories: Vec<_> = project
            .read(cx)
            .repositories(cx)
            .values()
            .cloned()
            .collect();
        let has_multiple_repositories = repositories.len() > 1;

        let current_branch_name = project.read(cx).active_repository(cx).and_then(|repo| {
            repo.read(cx)
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string())
        });

        let worktree_requests: Vec<_> = repositories
            .iter()
            .map(|repo| repo.update(cx, |repo, _| repo.worktrees()))
            .collect();

        let default_branch_requests: Vec<_> = repositories
            .iter()
            .map(|repo| repo.update(cx, |repo, _| repo.default_branch(false)))
            .collect();

        let repo_current_branches: Vec<Option<String>> = repositories
            .iter()
            .map(|repo| {
                repo.read(cx)
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string())
            })
            .collect();

        let initial_matches = vec![ThreadWorktreeEntry::CreateFromCurrentBranch];

        let delegate = ThreadWorktreePickerDelegate {
            matches: initial_matches,
            all_worktrees: Vec::new(),
            project_worktree_paths,
            selected_index: 0,
            project,
            current_branch_name,
            default_branch_name: None,
            has_multiple_repositories,
            show_default_branch_option: false,
        };

        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .modal(false)
                .max_height(Some(rems(20.).into()))
        });

        let mut subscriptions = Vec::new();

        {
            let picker_handle = picker.downgrade();
            cx.spawn_in(window, async move |_this, cx| {
                let mut all_repo_worktrees: Vec<Vec<GitWorktree>> = Vec::new();
                for request in worktree_requests {
                    match request.await {
                        Ok(Ok(worktrees)) => {
                            let filtered: Vec<_> =
                                worktrees.into_iter().filter(|wt| !wt.is_bare).collect();
                            all_repo_worktrees.push(filtered);
                        }
                        Ok(Err(err)) => {
                            log::warn!("ThreadWorktreePicker: git worktree list failed: {err}");
                            all_repo_worktrees.push(Vec::new());
                        }
                        Err(_) => {
                            log::warn!("ThreadWorktreePicker: worktree request was cancelled");
                            all_repo_worktrees.push(Vec::new());
                        }
                    }
                }

                let mut default_branches: Vec<Option<String>> = Vec::new();
                for request in default_branch_requests {
                    let branch = request.await.ok().and_then(Result::ok).flatten();
                    default_branches.push(branch.map(|b| b.to_string()));
                }

                let (default_branch_name, show_default_branch_option) =
                    if !has_multiple_repositories {
                        let name = default_branches.into_iter().next().flatten();
                        let show = name.is_some();
                        (name, show)
                    } else {
                        let any_differs = default_branches
                            .iter()
                            .zip(repo_current_branches.iter())
                            .any(|(default, current)| {
                                if let Some(default_name) = default {
                                    current
                                        .as_ref()
                                        .is_none_or(|current_name| current_name != default_name)
                                } else {
                                    false
                                }
                            });
                        (None, any_differs)
                    };

                let merged = merge_worktrees(all_repo_worktrees);

                picker_handle.update_in(cx, |picker, window, cx| {
                    picker.delegate.all_worktrees = merged;
                    picker.delegate.default_branch_name = default_branch_name;
                    picker.delegate.show_default_branch_option = show_default_branch_option;
                    picker.refresh(window, cx);
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }

        for repo in &repositories {
            let picker_entity = picker.downgrade();
            let all_repos = repositories.clone();
            subscriptions.push(cx.subscribe_in(
                repo,
                window,
                move |_this, _repo, event: &RepositoryEvent, window, cx| {
                    if matches!(event, RepositoryEvent::GitWorktreeListChanged) {
                        let worktree_requests: Vec<_> = all_repos
                            .iter()
                            .map(|r| r.update(cx, |repo, _| repo.worktrees()))
                            .collect();
                        let picker = picker_entity.clone();
                        cx.spawn_in(window, async move |_, cx| {
                            let mut all_repo_worktrees: Vec<Vec<GitWorktree>> = Vec::new();
                            for request in worktree_requests {
                                match request.await {
                                    Ok(Ok(worktrees)) => {
                                        all_repo_worktrees.push(
                                            worktrees
                                                .into_iter()
                                                .filter(|wt| !wt.is_bare)
                                                .collect(),
                                        );
                                    }
                                    _ => {
                                        all_repo_worktrees.push(Vec::new());
                                    }
                                }
                            }
                            let merged = merge_worktrees(all_repo_worktrees);
                            picker.update_in(cx, |picker, window, cx| {
                                picker.delegate.all_worktrees = merged;
                                picker.refresh(window, cx);
                            })?;
                            anyhow::Ok(())
                        })
                        .detach_and_log_err(cx);
                    }
                },
            ));
        }

        subscriptions.push(cx.subscribe(&picker, |_, _, _, cx| {
            cx.emit(DismissEvent);
        }));

        Self {
            focus_handle: picker.focus_handle(cx),
            picker,
            _subscriptions: subscriptions,
        }
    }
}

impl Focusable for ThreadWorktreePicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ThreadWorktreePicker {}

impl Render for ThreadWorktreePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(34.))
            .elevation_3(cx)
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|_, _, _, cx| {
                cx.emit(DismissEvent);
            }))
    }
}

#[derive(Clone)]
enum ThreadWorktreeEntry {
    CreateFromCurrentBranch,
    CreateFromDefaultBranch {
        /// None in multi-repo case (each repo resolves independently)
        default_branch_name: Option<String>,
    },
    Separator,
    Worktree {
        merged: MergedWorktree,
        positions: Vec<usize>,
    },
    CreateNamed {
        name: String,
        from_branch: CreateNamedBase,
        disabled_reason: Option<String>,
    },
}

#[derive(Clone)]
enum CreateNamedBase {
    CurrentBranch,
    SpecificBranch(String),
    DefaultBranch,
}

pub(crate) struct ThreadWorktreePickerDelegate {
    matches: Vec<ThreadWorktreeEntry>,
    all_worktrees: Vec<MergedWorktree>,
    project_worktree_paths: HashSet<PathBuf>,
    selected_index: usize,
    project: Entity<Project>,
    current_branch_name: Option<String>,
    default_branch_name: Option<String>,
    has_multiple_repositories: bool,
    show_default_branch_option: bool,
}

impl ThreadWorktreePickerDelegate {
    fn build_fixed_entries(&self) -> Vec<ThreadWorktreeEntry> {
        let mut entries = Vec::new();

        entries.push(ThreadWorktreeEntry::CreateFromCurrentBranch);

        if !self.has_multiple_repositories {
            if let Some(ref default_branch) = self.default_branch_name {
                let is_different = self
                    .current_branch_name
                    .as_ref()
                    .is_none_or(|current| current != default_branch);
                if is_different {
                    entries.push(ThreadWorktreeEntry::CreateFromDefaultBranch {
                        default_branch_name: Some(default_branch.clone()),
                    });
                }
            }
        } else if self.show_default_branch_option {
            entries.push(ThreadWorktreeEntry::CreateFromDefaultBranch {
                default_branch_name: None,
            });
        }

        entries
    }

    fn sync_selected_index(&mut self, has_query: bool) {
        if !has_query {
            return;
        }

        if let Some(index) = self
            .matches
            .iter()
            .position(|entry| matches!(entry, ThreadWorktreeEntry::Worktree { .. }))
        {
            self.selected_index = index;
        } else if let Some(index) = self
            .matches
            .iter()
            .position(|entry| matches!(entry, ThreadWorktreeEntry::CreateNamed { .. }))
        {
            self.selected_index = index;
        } else {
            self.selected_index = 0;
        }
    }
}

impl PickerDelegate for ThreadWorktreePickerDelegate {
    type ListItem = AnyElement;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a worktree for this thread…".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::Start
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        !matches!(self.matches.get(ix), Some(ThreadWorktreeEntry::Separator))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let repo_worktrees = self.all_worktrees.clone();

        let normalized_query = query.replace(' ', "-");
        let has_named_worktree = self
            .all_worktrees
            .iter()
            .any(|merged| merged.name == normalized_query);
        let create_named_disabled_reason: Option<String> = if has_named_worktree {
            Some("A worktree with this name already exists".into())
        } else {
            None
        };

        let show_default_branch_create = if !self.has_multiple_repositories {
            self.default_branch_name.as_ref().is_some_and(|default| {
                self.current_branch_name
                    .as_ref()
                    .is_none_or(|current| current != default)
            })
        } else {
            self.show_default_branch_option
        };
        let default_branch_name = self.default_branch_name.clone();
        let has_multiple_repositories = self.has_multiple_repositories;

        if query.is_empty() {
            let mut matches = self.build_fixed_entries();

            if !repo_worktrees.is_empty() {
                let project_paths = &self.project_worktree_paths;

                let mut sorted = repo_worktrees;
                sorted.sort_by(|a, b| {
                    let a_is_current = a.paths.iter().any(|p| project_paths.contains(p));
                    let b_is_current = b.paths.iter().any(|p| project_paths.contains(p));
                    b_is_current
                        .cmp(&a_is_current)
                        .then_with(|| a.name.cmp(&b.name))
                });

                matches.push(ThreadWorktreeEntry::Separator);
                for merged in sorted {
                    matches.push(ThreadWorktreeEntry::Worktree {
                        merged,
                        positions: Vec::new(),
                    });
                }
            }

            self.matches = matches;
            self.sync_selected_index(false);
            return Task::ready(());
        }

        let candidates: Vec<_> = repo_worktrees
            .iter()
            .enumerate()
            .map(|(ix, merged)| StringMatchCandidate::new(ix, &merged.name))
            .collect();

        let executor = cx.background_executor().clone();

        let task = cx.background_executor().spawn(async move {
            fuzzy::match_strings(
                &candidates,
                &query,
                true,
                true,
                10000,
                &Default::default(),
                executor,
            )
            .await
        });

        let repo_worktrees_clone = repo_worktrees;
        cx.spawn_in(window, async move |picker, cx| {
            let fuzzy_matches = task.await;

            picker
                .update_in(cx, |picker, _window, cx| {
                    let mut new_matches: Vec<ThreadWorktreeEntry> = Vec::new();

                    for candidate in &fuzzy_matches {
                        new_matches.push(ThreadWorktreeEntry::Worktree {
                            merged: repo_worktrees_clone[candidate.candidate_id].clone(),
                            positions: candidate.positions.clone(),
                        });
                    }

                    if !new_matches.is_empty() {
                        new_matches.push(ThreadWorktreeEntry::Separator);
                    }
                    new_matches.push(ThreadWorktreeEntry::CreateNamed {
                        name: normalized_query.clone(),
                        from_branch: CreateNamedBase::CurrentBranch,
                        disabled_reason: create_named_disabled_reason.clone(),
                    });
                    if show_default_branch_create {
                        if has_multiple_repositories {
                            new_matches.push(ThreadWorktreeEntry::CreateNamed {
                                name: normalized_query.clone(),
                                from_branch: CreateNamedBase::DefaultBranch,
                                disabled_reason: create_named_disabled_reason.clone(),
                            });
                        } else if let Some(ref default_branch) = default_branch_name {
                            new_matches.push(ThreadWorktreeEntry::CreateNamed {
                                name: normalized_query.clone(),
                                from_branch: CreateNamedBase::SpecificBranch(
                                    default_branch.clone(),
                                ),
                                disabled_reason: create_named_disabled_reason.clone(),
                            });
                        }
                    }

                    picker.delegate.matches = new_matches;
                    picker.delegate.sync_selected_index(true);

                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index) else {
            return;
        };

        match entry {
            ThreadWorktreeEntry::Separator => return,

            ThreadWorktreeEntry::CreateFromCurrentBranch => {
                window.dispatch_action(
                    Box::new(CreateWorktree {
                        worktree_name: None,
                        branch_target: NewWorktreeBranchTarget::CurrentBranch,
                    }),
                    cx,
                );
            }

            ThreadWorktreeEntry::CreateFromDefaultBranch {
                default_branch_name,
            } => {
                let branch_target = match default_branch_name {
                    Some(name) => NewWorktreeBranchTarget::ExistingBranch { name: name.clone() },
                    None => NewWorktreeBranchTarget::DefaultBranch,
                };
                window.dispatch_action(
                    Box::new(CreateWorktree {
                        worktree_name: None,
                        branch_target,
                    }),
                    cx,
                );
            }

            ThreadWorktreeEntry::Worktree { merged, .. } => {
                let is_current = merged
                    .paths
                    .iter()
                    .any(|p| self.project_worktree_paths.contains(p));

                if !is_current {
                    window.dispatch_action(
                        Box::new(SwitchWorktree {
                            paths: merged.paths.clone(),
                            display_name: merged.name.clone(),
                        }),
                        cx,
                    );
                }
            }

            ThreadWorktreeEntry::CreateNamed {
                name,
                from_branch,
                disabled_reason: None,
            } => {
                let branch_target = match from_branch {
                    CreateNamedBase::SpecificBranch(branch) => {
                        NewWorktreeBranchTarget::ExistingBranch {
                            name: branch.clone(),
                        }
                    }
                    CreateNamedBase::CurrentBranch => NewWorktreeBranchTarget::CurrentBranch,
                    CreateNamedBase::DefaultBranch => NewWorktreeBranchTarget::DefaultBranch,
                };
                window.dispatch_action(
                    Box::new(CreateWorktree {
                        worktree_name: Some(name.clone()),
                        branch_target,
                    }),
                    cx,
                );
            }

            ThreadWorktreeEntry::CreateNamed {
                disabled_reason: Some(_),
                ..
            } => {
                return;
            }
        }

        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let project = self.project.read(cx);
        let is_create_disabled = project.repositories(cx).is_empty() || project.is_via_collab();

        let no_git_reason: SharedString = "Requires a Git repository in the project".into();

        let create_new_list_item = |id: SharedString,
                                    label: SharedString,
                                    disabled_tooltip: Option<SharedString>,
                                    selected: bool| {
            let is_disabled = disabled_tooltip.is_some();
            ListItem::new(id)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2p5()
                        .child(
                            Icon::new(IconName::Plus)
                                .map(|this| {
                                    if is_disabled {
                                        this.color(Color::Disabled)
                                    } else {
                                        this.color(Color::Muted)
                                    }
                                })
                                .size(IconSize::Small),
                        )
                        .child(
                            Label::new(label).when(is_disabled, |this| this.color(Color::Disabled)),
                        ),
                )
                .when_some(disabled_tooltip, |this, reason| {
                    this.tooltip(Tooltip::text(reason))
                })
                .into_any_element()
        };

        match entry {
            ThreadWorktreeEntry::Separator => Some(
                div()
                    .py(DynamicSpacing::Base04.rems(cx))
                    .child(Divider::horizontal())
                    .into_any_element(),
            ),

            ThreadWorktreeEntry::CreateFromCurrentBranch => {
                let branch_label = if self.has_multiple_repositories {
                    "current branches".to_string()
                } else {
                    self.current_branch_name
                        .clone()
                        .unwrap_or_else(|| "HEAD".to_string())
                };

                let label = format!("Create new worktree based on {branch_label}");

                let disabled_tooltip = is_create_disabled.then(|| no_git_reason.clone());

                let item = create_new_list_item(
                    "create-from-current".to_string().into(),
                    label.into(),
                    disabled_tooltip,
                    selected,
                );

                Some(item.into_any_element())
            }

            ThreadWorktreeEntry::CreateFromDefaultBranch {
                default_branch_name,
            } => {
                let label = match default_branch_name {
                    Some(name) => format!("Create new worktree based on {name}"),
                    None => "Create new worktree based on each repo's default branch".to_string(),
                };

                let disabled_tooltip = is_create_disabled.then(|| no_git_reason.clone());

                let item = create_new_list_item(
                    "create-from-main".to_string().into(),
                    label.into(),
                    disabled_tooltip,
                    selected,
                );

                Some(item.into_any_element())
            }

            ThreadWorktreeEntry::Worktree { merged, positions } => {
                let display_name = &merged.name;
                let first_line = display_name.lines().next().unwrap_or(display_name);
                let positions: Vec<_> = positions
                    .iter()
                    .copied()
                    .filter(|&pos| pos < first_line.len())
                    .collect();
                let path = merged
                    .representative
                    .path
                    .compact()
                    .to_string_lossy()
                    .to_string();
                let sha = merged
                    .representative
                    .sha
                    .chars()
                    .take(7)
                    .collect::<String>();

                let is_current = merged
                    .paths
                    .iter()
                    .any(|p| self.project_worktree_paths.contains(p));

                let entry_icon = if is_current {
                    IconName::Check
                } else {
                    IconName::GitWorktree
                };

                Some(
                    ListItem::new(SharedString::from(format!("worktree-{ix}")))
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2p5()
                                .child(
                                    Icon::new(entry_icon)
                                        .color(if is_current {
                                            Color::Accent
                                        } else {
                                            Color::Muted
                                        })
                                        .size(IconSize::Small),
                                )
                                .child(
                                    v_flex()
                                        .w_full()
                                        .min_w_0()
                                        .child(
                                            HighlightedLabel::new(first_line.to_owned(), positions)
                                                .truncate(),
                                        )
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .min_w_0()
                                                .gap_1p5()
                                                .when_some(
                                                    merged
                                                        .representative
                                                        .branch_name()
                                                        .map(|b| b.to_string()),
                                                    |this, branch| {
                                                        this.child(
                                                            Label::new(branch)
                                                                .size(LabelSize::Small)
                                                                .color(Color::Muted),
                                                        )
                                                        .child(
                                                            Label::new("\u{2022}")
                                                                .alpha(0.5)
                                                                .color(Color::Muted)
                                                                .size(LabelSize::Small),
                                                        )
                                                    },
                                                )
                                                .when(!sha.is_empty(), |this| {
                                                    this.child(
                                                        Label::new(sha)
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .child(
                                                        Label::new("\u{2022}")
                                                            .alpha(0.5)
                                                            .color(Color::Muted)
                                                            .size(LabelSize::Small),
                                                    )
                                                })
                                                .child(
                                                    Label::new(path)
                                                        .truncate_start()
                                                        .color(Color::Muted)
                                                        .size(LabelSize::Small)
                                                        .flex_1(),
                                                ),
                                        ),
                                ),
                        )
                        .into_any_element(),
                )
            }

            ThreadWorktreeEntry::CreateNamed {
                name,
                from_branch,
                disabled_reason,
            } => {
                let branch_label = match from_branch {
                    CreateNamedBase::SpecificBranch(branch) => branch.clone(),
                    CreateNamedBase::CurrentBranch => self
                        .current_branch_name
                        .clone()
                        .unwrap_or_else(|| "HEAD".to_string()),
                    CreateNamedBase::DefaultBranch => "each repo's default branch".to_string(),
                };
                let label = format!("Create \"{name}\" based on {branch_label}");
                let element_id = match from_branch {
                    CreateNamedBase::SpecificBranch(branch) => {
                        format!("create-named-from-{branch}")
                    }
                    CreateNamedBase::CurrentBranch => "create-named-from-current".to_string(),
                    CreateNamedBase::DefaultBranch => "create-named-from-default".to_string(),
                };

                let item = create_new_list_item(
                    element_id.into(),
                    label.into(),
                    disabled_reason.clone().map(SharedString::from),
                    selected,
                );

                Some(item.into_any_element())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            release_channel::init("0.0.0".parse().unwrap(), cx);
            crate::agent_panel::init(cx);
        });
    }

    fn make_worktree(path: &str, branch: &str, is_main: bool) -> GitWorktree {
        GitWorktree {
            path: PathBuf::from(path),
            ref_name: Some(format!("refs/heads/{branch}").into()),
            sha: "abc1234".into(),
            is_main,
            is_bare: false,
        }
    }

    fn make_merged(worktrees: Vec<GitWorktree>) -> Vec<MergedWorktree> {
        merge_worktrees(vec![worktrees])
    }

    fn build_delegate(
        project: Entity<Project>,
        all_worktrees: Vec<MergedWorktree>,
        project_worktree_paths: HashSet<PathBuf>,
        current_branch_name: Option<String>,
        default_branch_name: Option<String>,
        has_multiple_repositories: bool,
        show_default_branch_option: bool,
    ) -> ThreadWorktreePickerDelegate {
        ThreadWorktreePickerDelegate {
            matches: vec![ThreadWorktreeEntry::CreateFromCurrentBranch],
            all_worktrees,
            project_worktree_paths,
            selected_index: 0,
            project,
            current_branch_name,
            default_branch_name,
            has_multiple_repositories,
            show_default_branch_option,
        }
    }

    fn entry_names(delegate: &ThreadWorktreePickerDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|entry| match entry {
                ThreadWorktreeEntry::CreateFromCurrentBranch => {
                    "CreateFromCurrentBranch".to_string()
                }
                ThreadWorktreeEntry::CreateFromDefaultBranch {
                    default_branch_name,
                } => match default_branch_name {
                    Some(name) => format!("CreateFromDefaultBranch({name})"),
                    None => "CreateFromDefaultBranch(per-repo)".to_string(),
                },
                ThreadWorktreeEntry::Separator => "---".to_string(),
                ThreadWorktreeEntry::Worktree { merged, .. } => {
                    format!("Worktree({})", merged.representative.path.display())
                }
                ThreadWorktreeEntry::CreateNamed {
                    name,
                    from_branch,
                    disabled_reason,
                } => {
                    let branch = match from_branch {
                        CreateNamedBase::CurrentBranch => "from current".to_string(),
                        CreateNamedBase::SpecificBranch(b) => format!("from {b}"),
                        CreateNamedBase::DefaultBranch => "from default".to_string(),
                    };
                    if disabled_reason.is_some() {
                        format!("CreateNamed({name}, {branch}, disabled)")
                    } else {
                        format!("CreateNamed({name}, {branch})")
                    }
                }
            })
            .collect()
    }

    type PickerWindow = gpui::WindowHandle<Picker<ThreadWorktreePickerDelegate>>;

    async fn make_picker(
        cx: &mut TestAppContext,
        all_worktrees: Vec<MergedWorktree>,
        project_worktree_paths: HashSet<PathBuf>,
        current_branch_name: Option<String>,
        default_branch_name: Option<String>,
        has_multiple_repositories: bool,
        show_default_branch_option: bool,
    ) -> PickerWindow {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        cx.add_window(|window, cx| {
            let delegate = build_delegate(
                project,
                all_worktrees,
                project_worktree_paths,
                current_branch_name,
                default_branch_name,
                has_multiple_repositories,
                show_default_branch_option,
            );
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .modal(false)
        })
    }

    #[gpui::test]
    async fn test_empty_query_entries(cx: &mut TestAppContext) {
        init_test(cx);

        let worktrees = vec![
            make_worktree("/repo", "main", true),
            make_worktree("/repo-feature", "feature", false),
            make_worktree("/repo-bugfix", "bugfix", false),
        ];
        let project_paths: HashSet<PathBuf> = [PathBuf::from("/repo")].into_iter().collect();

        let picker = make_picker(
            cx,
            make_merged(worktrees),
            project_paths,
            Some("main".into()),
            Some("main".into()),
            false,
            false,
        )
        .await;

        picker
            .update(cx, |picker, window, cx| picker.refresh(window, cx))
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();

        assert_eq!(
            names,
            vec![
                "CreateFromCurrentBranch",
                "---",
                "Worktree(/repo)",
                "Worktree(/repo-bugfix)",
                "Worktree(/repo-feature)",
            ]
        );

        picker
            .update(cx, |picker, _window, cx| {
                picker.delegate.current_branch_name = Some("feature".into());
                picker.delegate.default_branch_name = Some("main".into());
                cx.notify();
            })
            .unwrap();
        picker
            .update(cx, |picker, window, cx| picker.refresh(window, cx))
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();

        assert!(names.contains(&"CreateFromDefaultBranch(main)".to_string()));
    }

    #[gpui::test]
    async fn test_query_filtering_and_create_entries(cx: &mut TestAppContext) {
        init_test(cx);

        let picker = make_picker(
            cx,
            make_merged(vec![
                make_worktree("/repo", "main", true),
                make_worktree("/repo-feature", "feature", false),
                make_worktree("/repo-bugfix", "bugfix", false),
                make_worktree("/my-worktree", "experiment", false),
            ]),
            HashSet::default(),
            Some("dev".into()),
            Some("main".into()),
            false,
            false,
        )
        .await;

        picker
            .update(cx, |picker, window, cx| {
                picker.set_query("feat", window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();
        assert!(names.contains(&"Worktree(/repo-feature)".to_string()));
        assert!(
            names.contains(&"CreateNamed(feat, from current)".to_string()),
            "should offer to create from current branch, got: {names:?}"
        );
        assert!(
            names.contains(&"CreateNamed(feat, from main)".to_string()),
            "should offer to create from default branch, got: {names:?}"
        );
        assert!(!names.contains(&"Worktree(/repo-bugfix)".to_string()));

        picker
            .update(cx, |picker, window, cx| {
                picker.set_query("repo-feature", window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();
        assert!(
            names.contains(&"CreateNamed(repo-feature, from current, disabled)".to_string()),
            "exact name match should show disabled create entries, got: {names:?}"
        );

        picker
            .update(cx, |picker, window, cx| {
                picker.set_query("my worktree", window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();
        assert!(
            names.contains(&"CreateNamed(my-worktree, from current, disabled)".to_string()),
            "spaces should normalize to hyphens and detect existing worktree, got: {names:?}"
        );
    }

    #[gpui::test]
    async fn test_multi_repo_shows_merged_worktrees_and_enables_create_named(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let repo1_worktrees = vec![
            make_worktree("/code/zed", "main", true),
            make_worktree("/code/worktrees/feature-branch/zed", "feature", false),
        ];
        let repo2_worktrees = vec![
            make_worktree("/code/ex", "main", true),
            make_worktree("/code/worktrees/feature-branch/ex", "feature", false),
        ];
        let merged = merge_worktrees(vec![repo1_worktrees, repo2_worktrees]);

        let picker = make_picker(
            cx,
            merged,
            HashSet::default(),
            Some("main".into()),
            None,
            true,
            true,
        )
        .await;

        picker
            .update(cx, |picker, window, cx| picker.refresh(window, cx))
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();

        assert!(names.contains(&"CreateFromCurrentBranch".to_string()));
        assert!(names.contains(&"CreateFromDefaultBranch(per-repo)".to_string()));
        assert!(
            names.iter().any(|n| n.starts_with("Worktree(")),
            "multi-repo should show merged worktrees, got: {names:?}"
        );

        picker
            .update(cx, |picker, window, cx| {
                picker.set_query("new-thing", window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        let names = picker
            .read_with(cx, |picker, _| entry_names(&picker.delegate))
            .unwrap();
        assert!(
            names.contains(&"CreateNamed(new-thing, from current)".to_string()),
            "multi-repo should allow create named, got: {names:?}"
        );
        assert!(
            names.contains(&"CreateNamed(new-thing, from default)".to_string()),
            "multi-repo should show create from default branch, got: {names:?}"
        );
        assert!(
            !names.contains(&"CreateNamed(new-thing, from current, disabled)".to_string()),
            "create named should not be disabled for multi-repo, got: {names:?}"
        );
    }
}

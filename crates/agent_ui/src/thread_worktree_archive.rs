use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_client_protocol as acp;
use anyhow::{Context as _, Result, anyhow};
use git::repository::{AskPassDelegate, CommitOptions, ResetMode};
use gpui::{App, AsyncApp, Entity, Task, WindowHandle};
use project::{
    LocalProjectFlags, Project, WorktreeId,
    git_store::{Repository, resolve_git_worktree_to_main_repo},
};
use util::ResultExt;
use workspace::{AppState, MultiWorkspace, PathList, Workspace};

use crate::thread_metadata_store::{ArchivedGitWorktree, ThreadMetadataStore};

#[derive(Clone)]
pub struct RootPlan {
    pub root_path: PathBuf,
    pub main_repo_path: PathBuf,
    pub affected_projects: Vec<AffectedProject>,
    pub worktree_repo: Option<Entity<Repository>>,
    pub branch_name: Option<String>,
}

#[derive(Clone)]
pub struct AffectedProject {
    pub project: Entity<Project>,
    pub worktree_id: WorktreeId,
}

fn archived_worktree_ref_name(id: i64) -> String {
    format!("refs/archived-worktrees/{}", id)
}

pub struct PersistOutcome {
    pub archived_worktree_id: i64,
    pub staged_commit_hash: String,
}

pub fn build_root_plan(
    path: &Path,
    workspaces: &[Entity<Workspace>],
    cx: &App,
) -> Option<RootPlan> {
    let path = path.to_path_buf();

    let affected_projects = workspaces
        .iter()
        .filter_map(|workspace| {
            let project = workspace.read(cx).project().clone();
            let worktree = project
                .read(cx)
                .visible_worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == path.as_path())?;
            let worktree_id = worktree.read(cx).id();
            Some(AffectedProject {
                project,
                worktree_id,
            })
        })
        .collect::<Vec<_>>();

    let (linked_snapshot, worktree_repo) = workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>()
        })
        .find_map(|repo| {
            let snapshot = repo.read(cx).snapshot();
            (snapshot.is_linked_worktree()
                && snapshot.work_directory_abs_path.as_ref() == path.as_path())
            .then_some((snapshot, repo))
        })?;

    let branch_name = linked_snapshot
        .branch
        .as_ref()
        .map(|b| b.name().to_string());

    Some(RootPlan {
        root_path: path,
        main_repo_path: linked_snapshot.original_repo_abs_path.to_path_buf(),
        affected_projects,
        worktree_repo: Some(worktree_repo),
        branch_name,
    })
}

pub fn path_is_referenced_by_other_unarchived_threads(
    current_session_id: &acp::SessionId,
    path: &Path,
    cx: &App,
) -> bool {
    ThreadMetadataStore::global(cx)
        .read(cx)
        .entries()
        .filter(|thread| thread.session_id != *current_session_id)
        .filter(|thread| !thread.archived)
        .any(|thread| {
            thread
                .folder_paths
                .paths()
                .iter()
                .any(|other_path| other_path.as_path() == path)
        })
}

pub async fn remove_root(root: RootPlan, cx: &mut AsyncApp) -> Result<()> {
    let release_tasks: Vec<_> = root
        .affected_projects
        .iter()
        .map(|affected| {
            let project = affected.project.clone();
            let worktree_id = affected.worktree_id;
            project.update(cx, |project, cx| {
                let wait = project.wait_for_worktree_release(worktree_id, cx);
                project.remove_worktree(worktree_id, cx);
                wait
            })
        })
        .collect();

    if let Err(error) = remove_root_after_worktree_removal(&root, release_tasks, cx).await {
        rollback_root(&root, cx).await;
        return Err(error);
    }

    Ok(())
}

async fn remove_root_after_worktree_removal(
    root: &RootPlan,
    release_tasks: Vec<Task<Result<()>>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for task in release_tasks {
        task.await?;
    }

    let (repo, _temp_project) = find_or_create_repository(&root.main_repo_path, cx).await?;
    let receiver = repo.update(cx, |repo: &mut Repository, _cx| {
        repo.remove_worktree(root.root_path.clone(), false)
    });
    let result = receiver
        .await
        .map_err(|_| anyhow!("git worktree removal was canceled"))?;
    result
}

/// Finds a live `Repository` entity for the given path, or creates a temporary
/// `Project::local` to obtain one.
///
/// `Repository` entities can only be obtained through a `Project` because
/// `GitStore` (which creates and manages `Repository` entities) is owned by
/// `Project`. When no open workspace contains the repo we need, we spin up a
/// headless `Project::local` just to get a `Repository` handle. The caller
/// keeps the returned `Option<Entity<Project>>` alive for the duration of the
/// git operations, then drops it.
///
/// Future improvement: decoupling `GitStore` from `Project` so that
/// `Repository` entities can be created standalone would eliminate this
/// temporary-project workaround.
async fn find_or_create_repository(
    repo_path: &Path,
    cx: &mut AsyncApp,
) -> Result<(Entity<Repository>, Option<Entity<Project>>)> {
    let repo_path_owned = repo_path.to_path_buf();
    let live_repo = cx.update(|cx| {
        all_open_workspaces(cx)
            .into_iter()
            .flat_map(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .repositories(cx)
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .find(|repo| {
                repo.read(cx).snapshot().work_directory_abs_path.as_ref()
                    == repo_path_owned.as_path()
            })
    });

    if let Some(repo) = live_repo {
        return Ok((repo, None));
    }

    let app_state =
        current_app_state(cx).context("no app state available for temporary project")?;
    let temp_project = cx.update(|cx| {
        Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            LocalProjectFlags::default(),
            cx,
        )
    });

    let repo_path_for_worktree = repo_path.to_path_buf();
    let create_worktree = temp_project.update(cx, |project, cx| {
        project.create_worktree(repo_path_for_worktree, true, cx)
    });
    let _worktree = create_worktree.await?;
    let initial_scan = temp_project.read_with(cx, |project, cx| project.wait_for_initial_scan(cx));
    initial_scan.await;

    let repo_path_for_find = repo_path.to_path_buf();
    let repo = temp_project
        .update(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .find(|repo| {
                    repo.read(cx).snapshot().work_directory_abs_path.as_ref()
                        == repo_path_for_find.as_path()
                })
                .cloned()
        })
        .context("failed to resolve temporary repository handle")?;

    let barrier = repo.update(cx, |repo: &mut Repository, _cx| repo.barrier());
    barrier
        .await
        .map_err(|_| anyhow!("temporary repository barrier canceled"))?;
    Ok((repo, Some(temp_project)))
}

async fn rollback_root(root: &RootPlan, cx: &mut AsyncApp) {
    for affected in &root.affected_projects {
        let task = affected.project.update(cx, |project, cx| {
            project.create_worktree(root.root_path.clone(), true, cx)
        });
        let _ = task.await;
    }
}

pub async fn persist_worktree_state(
    root: &RootPlan,
    folder_paths: &PathList,
    cx: &mut AsyncApp,
) -> Result<PersistOutcome> {
    let worktree_repo = root
        .worktree_repo
        .clone()
        .context("no worktree repo entity for persistence")?;

    // Read original HEAD SHA before creating any WIP commits
    let original_commit_hash = worktree_repo
        .update(cx, |repo, _cx| repo.head_sha())
        .await
        .map_err(|_| anyhow!("head_sha canceled"))?
        .context("failed to read original HEAD SHA")?
        .context("HEAD SHA is None before WIP commits")?;

    // Create WIP commit #1 (staged state)
    let askpass = AskPassDelegate::new(cx, |_, _, _| {});
    let commit_rx = worktree_repo.update(cx, |repo, cx| {
        repo.commit(
            "WIP staged".into(),
            None,
            CommitOptions {
                allow_empty: true,
                ..Default::default()
            },
            askpass,
            cx,
        )
    });
    commit_rx
        .await
        .map_err(|_| anyhow!("WIP staged commit canceled"))??;

    // Read SHA after staged commit
    let staged_sha_result = worktree_repo
        .update(cx, |repo, _cx| repo.head_sha())
        .await
        .map_err(|_| anyhow!("head_sha canceled"))
        .and_then(|r| r.context("failed to read HEAD SHA after staged commit"))
        .and_then(|opt| opt.context("HEAD SHA is None after staged commit"));
    let staged_commit_hash = match staged_sha_result {
        Ok(sha) => sha,
        Err(error) => {
            let rx = worktree_repo.update(cx, |repo, cx| {
                repo.reset("HEAD~1".to_string(), ResetMode::Mixed, cx)
            });
            let _ = rx.await;
            return Err(error);
        }
    };

    // Stage all files including untracked
    let stage_rx = worktree_repo.update(cx, |repo, _cx| repo.stage_all_including_untracked());
    if let Err(error) = stage_rx
        .await
        .map_err(|_| anyhow!("stage all canceled"))
        .and_then(|inner| inner)
    {
        let rx = worktree_repo.update(cx, |repo, cx| {
            repo.reset("HEAD~1".to_string(), ResetMode::Mixed, cx)
        });
        let _ = rx.await;
        return Err(error.context("failed to stage all files including untracked"));
    }

    // Create WIP commit #2 (unstaged/untracked state)
    let askpass = AskPassDelegate::new(cx, |_, _, _| {});
    let commit_rx = worktree_repo.update(cx, |repo, cx| {
        repo.commit(
            "WIP unstaged".into(),
            None,
            CommitOptions {
                allow_empty: true,
                ..Default::default()
            },
            askpass,
            cx,
        )
    });
    if let Err(error) = commit_rx
        .await
        .map_err(|_| anyhow!("WIP unstaged commit canceled"))
        .and_then(|inner| inner)
    {
        let rx = worktree_repo.update(cx, |repo, cx| {
            repo.reset("HEAD~1".to_string(), ResetMode::Mixed, cx)
        });
        let _ = rx.await;
        return Err(error);
    }

    // Read HEAD SHA after WIP commits
    let head_sha_result = worktree_repo
        .update(cx, |repo, _cx| repo.head_sha())
        .await
        .map_err(|_| anyhow!("head_sha canceled"))
        .and_then(|r| r.context("failed to read HEAD SHA after WIP commits"))
        .and_then(|opt| opt.context("HEAD SHA is None after WIP commits"));
    let unstaged_commit_hash = match head_sha_result {
        Ok(sha) => sha,
        Err(error) => {
            let rx = worktree_repo.update(cx, |repo, cx| {
                repo.reset(format!("{}~1", staged_commit_hash), ResetMode::Mixed, cx)
            });
            let _ = rx.await;
            return Err(error);
        }
    };

    // Create DB record
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    let worktree_path_str = root.root_path.to_string_lossy().to_string();
    let main_repo_path_str = root.main_repo_path.to_string_lossy().to_string();
    let branch_name = root.branch_name.clone();

    let db_result = store
        .read_with(cx, |store, cx| {
            store.create_archived_worktree(
                worktree_path_str.clone(),
                main_repo_path_str.clone(),
                branch_name.clone(),
                staged_commit_hash.clone(),
                unstaged_commit_hash.clone(),
                original_commit_hash.clone(),
                cx,
            )
        })
        .await
        .context("failed to create archived worktree DB record");
    let archived_worktree_id = match db_result {
        Ok(id) => id,
        Err(error) => {
            let rx = worktree_repo.update(cx, |repo, cx| {
                repo.reset(format!("{}~1", staged_commit_hash), ResetMode::Mixed, cx)
            });
            let _ = rx.await;
            return Err(error);
        }
    };

    // Link all threads on this worktree to the archived record
    let session_ids: Vec<acp::SessionId> = store.read_with(cx, |store, _cx| {
        store
            .all_session_ids_for_path(folder_paths)
            .cloned()
            .collect()
    });

    for session_id in &session_ids {
        let link_result = store
            .read_with(cx, |store, cx| {
                store.link_thread_to_archived_worktree(
                    session_id.0.to_string(),
                    archived_worktree_id,
                    cx,
                )
            })
            .await;
        if let Err(error) = link_result {
            if let Err(delete_error) = store
                .read_with(cx, |store, cx| {
                    store.delete_archived_worktree(archived_worktree_id, cx)
                })
                .await
            {
                log::error!(
                    "Failed to delete archived worktree DB record during link rollback: {delete_error:#}"
                );
            }
            let rx = worktree_repo.update(cx, |repo, cx| {
                repo.reset(format!("{}~1", staged_commit_hash), ResetMode::Mixed, cx)
            });
            let _ = rx.await;
            return Err(error.context("failed to link thread to archived worktree"));
        }
    }

    // Create git ref on main repo (non-fatal)
    let ref_name = archived_worktree_ref_name(archived_worktree_id);
    let main_repo_result = find_or_create_repository(&root.main_repo_path, cx).await;
    match main_repo_result {
        Ok((main_repo, _temp_project)) => {
            let rx = main_repo.update(cx, |repo, _cx| {
                repo.update_ref(ref_name.clone(), unstaged_commit_hash.clone())
            });
            if let Err(error) = rx
                .await
                .map_err(|_| anyhow!("update_ref canceled"))
                .and_then(|r| r)
            {
                log::warn!(
                    "Failed to create ref {} on main repo (non-fatal): {error}",
                    ref_name
                );
            }
        }
        Err(error) => {
            log::warn!(
                "Could not find main repo to create ref {} (non-fatal): {error}",
                ref_name
            );
        }
    }

    Ok(PersistOutcome {
        archived_worktree_id,
        staged_commit_hash,
    })
}

pub async fn rollback_persist(outcome: &PersistOutcome, root: &RootPlan, cx: &mut AsyncApp) {
    // Undo WIP commits on the worktree repo
    if let Some(worktree_repo) = &root.worktree_repo {
        let rx = worktree_repo.update(cx, |repo, cx| {
            repo.reset(
                format!("{}~1", outcome.staged_commit_hash),
                ResetMode::Mixed,
                cx,
            )
        });
        let _ = rx.await;
    }

    // Delete the git ref on main repo
    if let Ok((main_repo, _temp_project)) =
        find_or_create_repository(&root.main_repo_path, cx).await
    {
        let ref_name = archived_worktree_ref_name(outcome.archived_worktree_id);
        let rx = main_repo.update(cx, |repo, _cx| repo.delete_ref(ref_name));
        let _ = rx.await;
    }

    // Delete the DB record
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    if let Err(error) = store
        .read_with(cx, |store, cx| {
            store.delete_archived_worktree(outcome.archived_worktree_id, cx)
        })
        .await
    {
        log::error!("Failed to delete archived worktree DB record during rollback: {error:#}");
    }
}

pub async fn restore_worktree_via_git(
    row: &ArchivedGitWorktree,
    cx: &mut AsyncApp,
) -> Result<PathBuf> {
    // Find the main repo entity and verify original_commit_hash exists
    let (main_repo, _temp_project) = find_or_create_repository(&row.main_repo_path, cx).await?;

    let commit_exists = main_repo
        .update(cx, |repo, _cx| {
            repo.resolve_commit(row.original_commit_hash.clone())
        })
        .await
        .map_err(|_| anyhow!("resolve_commit was canceled"))?
        .context("failed to check if original commit exists")?;

    if !commit_exists {
        anyhow::bail!(
            "Original commit {} no longer exists in the repository — \
             cannot restore worktree. The git history this archive depends on may have been \
             rewritten or garbage-collected.",
            row.original_commit_hash
        );
    }

    // Check if worktree path already exists on disk
    let worktree_path = &row.worktree_path;
    let app_state = current_app_state(cx).context("no app state available")?;
    let already_exists = app_state.fs.metadata(worktree_path).await?.is_some();

    if already_exists {
        let is_git_worktree =
            resolve_git_worktree_to_main_repo(app_state.fs.as_ref(), worktree_path)
                .await
                .is_some();

        if is_git_worktree {
            // Already a git worktree — another thread on the same worktree
            // already restored it. Reuse as-is.
            return Ok(worktree_path.clone());
        }

        // Path exists but isn't a git worktree. Ask git to adopt it.
        let rx = main_repo.update(cx, |repo, _cx| repo.repair_worktrees());
        rx.await
            .map_err(|_| anyhow!("worktree repair was canceled"))?
            .context("failed to repair worktrees")?;
    } else {
        // Create detached worktree at the unstaged commit
        let rx = main_repo.update(cx, |repo, _cx| {
            repo.create_worktree_detached(worktree_path.clone(), row.unstaged_commit_hash.clone())
        });
        rx.await
            .map_err(|_| anyhow!("worktree creation was canceled"))?
            .context("failed to create worktree")?;
    }

    // Get the worktree's repo entity
    let (wt_repo, _temp_wt_project) = find_or_create_repository(worktree_path, cx).await?;

    // Reset past the WIP commits to recover original state
    let mixed_reset_ok = {
        let rx = wt_repo.update(cx, |repo, cx| {
            repo.reset(row.staged_commit_hash.clone(), ResetMode::Mixed, cx)
        });
        match rx.await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                log::error!("Mixed reset to staged commit failed: {error:#}");
                false
            }
            Err(_) => {
                log::error!("Mixed reset to staged commit was canceled");
                false
            }
        }
    };

    let soft_reset_ok = if mixed_reset_ok {
        let rx = wt_repo.update(cx, |repo, cx| {
            repo.reset(row.original_commit_hash.clone(), ResetMode::Soft, cx)
        });
        match rx.await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                log::error!("Soft reset to original commit failed: {error:#}");
                false
            }
            Err(_) => {
                log::error!("Soft reset to original commit was canceled");
                false
            }
        }
    } else {
        false
    };

    // If either WIP reset failed, fall back to a mixed reset directly to
    // original_commit_hash so we at least land on the right commit.
    if !mixed_reset_ok || !soft_reset_ok {
        log::warn!(
            "WIP reset(s) failed (mixed_ok={mixed_reset_ok}, soft_ok={soft_reset_ok}); \
             falling back to mixed reset to original commit {}",
            row.original_commit_hash
        );
        let rx = wt_repo.update(cx, |repo, cx| {
            repo.reset(row.original_commit_hash.clone(), ResetMode::Mixed, cx)
        });
        match rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(error.context(format!(
                    "fallback reset to original commit {} also failed",
                    row.original_commit_hash
                )));
            }
            Err(_) => {
                return Err(anyhow!(
                    "fallback reset to original commit {} was canceled",
                    row.original_commit_hash
                ));
            }
        }
    }

    // Verify HEAD is at original_commit_hash
    let current_head = wt_repo
        .update(cx, |repo, _cx| repo.head_sha())
        .await
        .map_err(|_| anyhow!("post-restore head_sha was canceled"))?
        .context("failed to read HEAD after restore")?
        .context("HEAD is None after restore")?;

    if current_head != row.original_commit_hash {
        anyhow::bail!(
            "After restore, HEAD is at {current_head} but expected {}. \
             The worktree may be in an inconsistent state.",
            row.original_commit_hash
        );
    }

    // Restore the branch
    if let Some(branch_name) = &row.branch_name {
        // Check if the branch exists and points at original_commit_hash.
        // If it does, switch to it. If not, create a new branch there.
        let rx = wt_repo.update(cx, |repo, _cx| repo.change_branch(branch_name.clone()));
        if matches!(rx.await, Ok(Ok(()))) {
            // Verify the branch actually points at original_commit_hash after switching
            let head_after_switch = wt_repo
                .update(cx, |repo, _cx| repo.head_sha())
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten();

            if head_after_switch.as_deref() != Some(&row.original_commit_hash) {
                // Branch exists but doesn't point at the right commit.
                // Switch back to detached HEAD at original_commit_hash.
                log::warn!(
                    "Branch '{}' exists but points at {:?}, not {}. Creating fresh branch.",
                    branch_name,
                    head_after_switch,
                    row.original_commit_hash
                );
                let rx = wt_repo.update(cx, |repo, cx| {
                    repo.reset(row.original_commit_hash.clone(), ResetMode::Mixed, cx)
                });
                let _ = rx.await;
                // Delete the old branch and create fresh
                let rx = wt_repo.update(cx, |repo, _cx| {
                    repo.create_branch(branch_name.clone(), None)
                });
                let _ = rx.await;
            }
        } else {
            // Branch doesn't exist or can't be switched to — create it.
            let rx = wt_repo.update(cx, |repo, _cx| {
                repo.create_branch(branch_name.clone(), None)
            });
            if let Ok(Err(error)) | Err(error) = rx.await.map_err(|e| anyhow::anyhow!("{e}")) {
                log::warn!(
                    "Could not create branch '{}': {error} — \
                     restored worktree is in detached HEAD state.",
                    branch_name
                );
            }
        }
    }

    Ok(worktree_path.clone())
}

pub async fn cleanup_archived_worktree_record(row: &ArchivedGitWorktree, cx: &mut AsyncApp) {
    // Delete the git ref from the main repo
    if let Ok((main_repo, _temp_project)) = find_or_create_repository(&row.main_repo_path, cx).await
    {
        let ref_name = archived_worktree_ref_name(row.id);
        let rx = main_repo.update(cx, |repo, _cx| repo.delete_ref(ref_name));
        match rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("Failed to delete archive ref: {error}"),
            Err(_) => log::warn!("Archive ref deletion was canceled"),
        }
    }

    // Delete the DB records
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    store
        .read_with(cx, |store, cx| store.delete_archived_worktree(row.id, cx))
        .await
        .log_err();
}

pub fn all_open_workspaces(cx: &App) -> Vec<Entity<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .flat_map(|multi_workspace| {
            multi_workspace
                .read(cx)
                .map(|multi_workspace| multi_workspace.workspaces().to_vec())
                .unwrap_or_default()
        })
        .collect()
}

fn window_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Option<WindowHandle<MultiWorkspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .find(|window| {
            window
                .read(cx)
                .map(|multi_workspace| multi_workspace.workspaces().contains(workspace))
                .unwrap_or(false)
        })
}

fn window_for_workspace_async(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncApp,
) -> Option<WindowHandle<MultiWorkspace>> {
    let workspace = workspace.clone();
    cx.update(|cx| window_for_workspace(&workspace, cx))
}

fn current_app_state(cx: &mut AsyncApp) -> Option<Arc<AppState>> {
    cx.update(|cx| {
        all_open_workspaces(cx)
            .into_iter()
            .next()
            .map(|workspace| workspace.read(cx).app_state().clone())
    })
}

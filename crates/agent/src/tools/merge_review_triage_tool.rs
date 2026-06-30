use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema as acp;
use anyhow::{Context as _, Result};
use git::commit::parse_git_diff_name_status;
use git::status::StatusCode;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// List changed files between HEAD and an upstream ref for Surmount merge review.
///
/// Replaces `script/surmount-merge-triage` and any bash/python wrappers. Read-only git only.
/// Prefer the merge review plan prompt counts when a Zed merge review session is already active.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MergeReviewTriageToolInput {
    /// Upstream git ref, e.g. `origin/main`. Defaults to `origin/main`.
    #[serde(default = "default_upstream_ref")]
    pub upstream_ref: String,
}

fn default_upstream_ref() -> String {
    "origin/main".to_string()
}

/// Return the merge-base diff for one file during Surmount merge review.
///
/// Replaces `git diff --merge-base <upstream> -- <path>` terminal commands. Read-only git only.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MergeReviewDiffToolInput {
    /// Repository-relative path, e.g. `crates/agent_ui/src/merge_review.rs`.
    pub path: String,
    /// Upstream git ref, e.g. `origin/main`. Defaults to `origin/main`.
    #[serde(default = "default_upstream_ref")]
    pub upstream_ref: String,
}

pub struct MergeReviewTriageTool {
    project: Entity<Project>,
}

pub struct MergeReviewDiffTool {
    project: Entity<Project>,
}

impl MergeReviewTriageTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl MergeReviewDiffTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for MergeReviewTriageTool {
    type Input = MergeReviewTriageToolInput;
    type Output = String;

    const NAME: &'static str = "merge_review_triage";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Merge review triage ({})", parsed.upstream_ref).into(),
            Err(_) => "Merge review triage".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let parsed = input.recv().await.map_err(|e| e.to_string())?;
            let worktree_root = project
                .read_with(cx, |project, cx| project_worktree_root(project, cx))
                .ok_or_else(|| "no project worktree for merge review triage".to_string())?;
            let json = merge_review_triage_json(&worktree_root, &parsed.upstream_ref)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
        })
    }
}

impl AgentTool for MergeReviewDiffTool {
    type Input = MergeReviewDiffToolInput;
    type Output = String;

    const NAME: &'static str = "merge_review_diff";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Merge review diff ({})", parsed.path).into(),
            Err(_) => "Merge review diff".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let parsed = input.recv().await.map_err(|e| e.to_string())?;
            let worktree_root = project
                .read_with(cx, |project, cx| project_worktree_root(project, cx))
                .ok_or_else(|| "no project worktree for merge review diff".to_string())?;
            merge_review_diff_text(&worktree_root, &parsed.upstream_ref, &parsed.path)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

fn project_worktree_root(project: &Project, cx: &App) -> Option<PathBuf> {
    project
        .worktree_root_names(cx)
        .next()
        .and_then(|name| project.worktree_for_root_name(name, cx))
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

fn is_surmount_workspace(worktree_root: &Path) -> bool {
    worktree_root.join("SURMOUNT.md").is_file()
}

fn is_reviewable_changed_path(worktree_root: &Path, path: &str) -> bool {
    !worktree_root.join(path).is_dir()
}

fn status_code_label(status: StatusCode) -> &'static str {
    match status {
        StatusCode::Modified => "M",
        StatusCode::Added => "A",
        StatusCode::Deleted => "D",
        _ => "?",
    }
}

fn collect_merge_review_changed_files(
    worktree_root: &Path,
    name_status: &str,
    conflict_output: &str,
) -> Vec<serde_json::Value> {
    let conflict_paths = conflict_output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| path.replace('\\', "/"))
        .collect::<HashSet<_>>();

    parse_git_diff_name_status(name_status)
        .filter(|(path, _)| is_reviewable_changed_path(worktree_root, path))
        .map(|(path, status)| {
            let normalized = path.replace('\\', "/");
            serde_json::json!({
                "path": normalized,
                "status": status_code_label(status),
                "is_conflict": conflict_paths.contains(&normalized),
            })
        })
        .collect()
}

async fn merge_review_triage_json(
    worktree_root: &Path,
    upstream_ref: &str,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    let merge_base = run_git_output(worktree_root, &["merge-base", "HEAD", upstream_ref])
        .await?
        .trim()
        .to_string();
    let name_status = run_git_output(
        worktree_root,
        &["diff", "--merge-base", upstream_ref, "--name-status", "-z"],
    )
    .await?;
    let conflict_output = run_git_output(
        worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await
    .unwrap_or_default();
    let changed_files =
        collect_merge_review_changed_files(worktree_root, &name_status, &conflict_output);
    let changed_file_count = changed_files.len();
    Ok(serde_json::json!({
        "merge_base": merge_base,
        "upstream_ref": upstream_ref,
        "changed_files": changed_files,
        "changed_file_count": changed_file_count,
        "manifest": "surmount-merge-categories.toml",
        "skill": ".agents/skills/surmount-merge-review/SKILL.md",
    }))
}

async fn merge_review_diff_text(
    worktree_root: &Path,
    upstream_ref: &str,
    path: &str,
) -> Result<String> {
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    let normalized = path.replace('\\', "/");
    run_git_output(
        worktree_root,
        &[
            "diff",
            "--merge-base",
            upstream_ref,
            "--",
            normalized.as_str(),
        ],
    )
    .await
}

async fn run_git_output(worktree_root: &Path, args: &[&str]) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collect_merge_review_changed_files_matches_triage_script_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("crates")).expect("mkdir crates");
        fs::write(root.join("crates/a.rs"), "a").expect("write file");
        fs::create_dir_all(root.join("crates/dir_only")).expect("mkdir dir_only");
        let name_status = "M\x00crates/a.rs\x00A\x00crates/b.rs\x00M\x00crates/dir_only\x00";
        let conflicts = "crates/a.rs\0";
        let files = collect_merge_review_changed_files(root, name_status, conflicts);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "crates/a.rs");
        assert_eq!(files[0]["status"], "M");
        assert_eq!(files[0]["is_conflict"], true);
        assert_eq!(files[1]["path"], "crates/b.rs");
        assert_eq!(files[1]["status"], "A");
        assert_eq!(files[1]["is_conflict"], false);
    }

    #[test]
    fn merge_review_triage_tool_input_defaults_upstream_ref() {
        let input: MergeReviewTriageToolInput =
            serde_json::from_value(serde_json::json!({})).expect("defaults");
        assert_eq!(input.upstream_ref, "origin/main");
    }
}

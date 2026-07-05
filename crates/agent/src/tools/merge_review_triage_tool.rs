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

/// Return ours/theirs/base/working text and parsed conflict regions for one conflicted file.
///
/// Uses git index stages (`:2:` ours / `:3:` theirs / `:1:` base) plus the working tree
/// file with conflict markers. Read-only git only.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MergeReviewConflictSidesToolInput {
    /// Repository-relative path, e.g. `crates/editor/src/editor.rs`.
    pub path: String,
    /// Upstream git ref, e.g. `origin/main`. Defaults to `origin/main`.
    #[serde(default = "default_upstream_ref")]
    pub upstream_ref: String,
}

pub struct MergeReviewConflictSidesTool {
    project: Entity<Project>,
}

/// Record a structured merge-conflict decision for Surmount merge review.
///
/// Zed persists the decision in the merge review session after the tool completes.
/// Call only after conflict markers are cleared from the working tree.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MergeReviewRecordDecisionToolInput {
    /// Repository-relative path, e.g. `crates/agent_ui/src/merge_review.rs`.
    pub path: String,
    /// `keep_fork`, `take_upstream`, or `synthesize`.
    pub outcome: MergeReviewRecordDecisionOutcome,
    /// Why this outcome fits the fork.
    pub rationale: String,
    /// Optional SURMOUNT.md note for this decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surmount_note: Option<String>,
    /// Test assertion descriptions for follow-up Plan Todos.
    #[serde(default)]
    pub test_assertions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeReviewRecordDecisionOutcome {
    KeepFork,
    TakeUpstream,
    Synthesize,
}

pub struct MergeReviewRecordDecisionTool {
    project: Entity<Project>,
}

/// Verify a merge-conflict file is resolved (no markers, not in unmerged index).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MergeReviewVerifyConflictResolvedToolInput {
    /// Repository-relative path, e.g. `crates/agent_ui/src/merge_review.rs`.
    pub path: String,
}

pub struct MergeReviewVerifyConflictResolvedTool {
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

impl MergeReviewConflictSidesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl MergeReviewRecordDecisionTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl MergeReviewVerifyConflictResolvedTool {
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

impl AgentTool for MergeReviewConflictSidesTool {
    type Input = MergeReviewConflictSidesToolInput;
    type Output = String;

    const NAME: &'static str = "merge_review_conflict_sides";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Merge review conflict sides ({})", parsed.path).into(),
            Err(_) => "Merge review conflict sides".into(),
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
                .ok_or_else(|| "no project worktree for merge review conflict sides".to_string())?;
            let json = merge_review_conflict_sides_json(
                &worktree_root,
                &parsed.upstream_ref,
                &parsed.path,
            )
            .await
            .map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
        })
    }
}

impl AgentTool for MergeReviewRecordDecisionTool {
    type Input = MergeReviewRecordDecisionToolInput;
    type Output = String;

    const NAME: &'static str = "merge_review_record_decision";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Record conflict decision ({})", parsed.path).into(),
            Err(_) => "Record conflict decision".into(),
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
                .ok_or_else(|| {
                    "no project worktree for merge review record decision".to_string()
                })?;
            let json = merge_review_record_decision_json(&worktree_root, &parsed)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
        })
    }
}

impl AgentTool for MergeReviewVerifyConflictResolvedTool {
    type Input = MergeReviewVerifyConflictResolvedToolInput;
    type Output = String;

    const NAME: &'static str = "merge_review_verify_conflict_resolved";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Verify conflict resolved ({})", parsed.path).into(),
            Err(_) => "Verify conflict resolved".into(),
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
                .ok_or_else(|| {
                    "no project worktree for merge review verify conflict resolved".to_string()
                })?;
            let json = merge_review_verify_conflict_resolved_json(&worktree_root, &parsed.path)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
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

#[derive(Debug, Serialize, PartialEq, Eq)]
struct MergeReviewConflictRegion {
    start_line: usize,
    end_line: usize,
    ours_lines: Vec<String>,
    theirs_lines: Vec<String>,
}

async fn merge_review_conflict_sides_json(
    worktree_root: &Path,
    upstream_ref: &str,
    path: &str,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    let normalized = path.replace('\\', "/");
    anyhow::ensure!(
        path_is_unmerged(worktree_root, &normalized).await?,
        "{normalized} is not an unmerged conflict path"
    );
    let ours_text = run_git_output(worktree_root, &["show", &format!(":2:{normalized}")])
        .await
        .context("reading ours stage (:2:)")?;
    let theirs_text = run_git_output(worktree_root, &["show", &format!(":3:{normalized}")])
        .await
        .context("reading theirs stage (:3:)")?;
    let base_text =
        run_git_output_optional(worktree_root, &["show", &format!(":1:{normalized}")]).await;
    let working_path = worktree_root.join(&normalized);
    let working_text = std::fs::read_to_string(&working_path)
        .with_context(|| format!("reading working tree file {normalized}"))?;
    let regions = parse_conflict_regions(&working_text);
    let mut value = serde_json::json!({
        "path": normalized,
        "upstream_ref": upstream_ref,
        "ours_text": ours_text,
        "theirs_text": theirs_text,
        "working_text": working_text,
        "regions": regions,
    });
    if let Some(base_text) = base_text {
        value["base_text"] = serde_json::Value::String(base_text);
    }
    Ok(value)
}

async fn path_is_unmerged(worktree_root: &Path, path: &str) -> Result<bool> {
    let output = run_git_output(
        worktree_root,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await?;
    Ok(output.split('\0').any(|entry| entry == path))
}

fn path_has_conflict_markers(worktree_root: &Path, path: &str) -> bool {
    std::fs::read_to_string(worktree_root.join(path.replace('\\', "/")))
        .map(|text| text.lines().any(|line| line.starts_with("<<<<<<< ")))
        .unwrap_or(false)
}

async fn merge_review_record_decision_json(
    worktree_root: &Path,
    input: &MergeReviewRecordDecisionToolInput,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    let normalized = input.path.replace('\\', "/");
    anyhow::ensure!(
        is_reviewable_changed_path(worktree_root, &normalized),
        "{normalized} is not a reviewable file path"
    );
    let rationale = input.rationale.trim();
    anyhow::ensure!(!rationale.is_empty(), "rationale must not be empty");
    anyhow::ensure!(
        !path_has_conflict_markers(worktree_root, &normalized),
        "{normalized} still has conflict markers — resolve before recording a decision"
    );
    Ok(serde_json::json!({
        "path": normalized,
        "outcome": input.outcome,
        "rationale": rationale,
        "surmount_note": input.surmount_note,
        "test_assertions": input.test_assertions,
        "recorded": true,
    }))
}

async fn merge_review_verify_conflict_resolved_json(
    worktree_root: &Path,
    path: &str,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        is_surmount_workspace(worktree_root),
        "SURMOUNT.md not found; not a Surmount workspace"
    );
    let normalized = path.replace('\\', "/");
    anyhow::ensure!(
        is_reviewable_changed_path(worktree_root, &normalized),
        "{normalized} is not a reviewable file path"
    );
    let markers_present = path_has_conflict_markers(worktree_root, &normalized);
    let in_unmerged_index = path_is_unmerged(worktree_root, &normalized).await?;
    Ok(serde_json::json!({
        "path": normalized,
        "checks": {
            "markers_present": markers_present,
            "in_unmerged_index": in_unmerged_index,
            "resolved": !markers_present && !in_unmerged_index,
        },
    }))
}

fn parse_conflict_regions(working_text: &str) -> Vec<serde_json::Value> {
    parse_conflict_region_records(working_text)
        .into_iter()
        .map(|region| {
            serde_json::json!({
                "start_line": region.start_line,
                "end_line": region.end_line,
                "ours_lines": region.ours_lines,
                "theirs_lines": region.theirs_lines,
            })
        })
        .collect()
}

fn parse_conflict_region_records(working_text: &str) -> Vec<MergeReviewConflictRegion> {
    let lines: Vec<&str> = working_text.lines().collect();
    let mut regions = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].starts_with("<<<<<<< ") {
            let start_line = index + 1;
            index += 1;
            let mut ours_lines = Vec::new();
            while index < lines.len()
                && !lines[index].starts_with("||||||| ")
                && !lines[index].starts_with("=======")
            {
                ours_lines.push(lines[index].to_string());
                index += 1;
            }
            if index < lines.len() && lines[index].starts_with("||||||| ") {
                index += 1;
                while index < lines.len() && !lines[index].starts_with("=======") {
                    index += 1;
                }
            }
            if index < lines.len() && lines[index].starts_with("=======") {
                index += 1;
            }
            let mut theirs_lines = Vec::new();
            while index < lines.len() && !lines[index].starts_with(">>>>>>> ") {
                theirs_lines.push(lines[index].to_string());
                index += 1;
            }
            if index < lines.len() && lines[index].starts_with(">>>>>>> ") {
                regions.push(MergeReviewConflictRegion {
                    start_line,
                    end_line: index + 1,
                    ours_lines,
                    theirs_lines,
                });
                index += 1;
                continue;
            }
        }
        index += 1;
    }
    regions
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

async fn run_git_output_optional(worktree_root: &Path, args: &[&str]) -> Option<String> {
    run_git_output(worktree_root, args).await.ok()
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

    #[test]
    fn merge_review_conflict_sides_tool_input_defaults_upstream_ref() {
        let input: MergeReviewConflictSidesToolInput = serde_json::from_value(serde_json::json!({
            "path": "crates/editor/src/editor.rs"
        }))
        .expect("defaults");
        assert_eq!(input.path, "crates/editor/src/editor.rs");
        assert_eq!(input.upstream_ref, "origin/main");
    }

    #[test]
    fn merge_review_record_decision_tool_input_deserializes() {
        let input: MergeReviewRecordDecisionToolInput = serde_json::from_value(serde_json::json!({
            "path": "crates/foo.rs",
            "outcome": "keep_fork",
            "rationale": "Fork hook must stay.",
            "test_assertions": ["assert zoom hook"],
        }))
        .expect("deserialize");
        assert_eq!(input.path, "crates/foo.rs");
        assert_eq!(input.outcome, MergeReviewRecordDecisionOutcome::KeepFork);
        assert_eq!(input.rationale, "Fork hook must stay.");
        assert_eq!(input.test_assertions, vec!["assert zoom hook"]);
    }

    #[test]
    fn merge_review_verify_conflict_resolved_json_reports_cleared_file() {
        smol::block_on(async {
            let fixture = ConflictSidesGitFixture::new();
            fixture.begin_conflicted_merge();
            let path = fixture.path();
            let file = "crates/editor/src/editor.rs";
            git_cmd(path, &["checkout", "--ours", file]);
            std::fs::write(path.join(file), "resolved editor without markers\n")
                .expect("write resolved");
            git_cmd(path, &["add", file]);
            let json = merge_review_verify_conflict_resolved_json(path, file)
                .await
                .expect("verify json");
            assert_eq!(json["checks"]["markers_present"], false);
            assert_eq!(json["checks"]["in_unmerged_index"], false);
            assert_eq!(json["checks"]["resolved"], true);
        });
    }

    #[test]
    fn parse_conflict_region_records_extracts_ours_and_theirs() {
        let working = "before\n<<<<<<< HEAD\nfork editor\n=======\nupstream editor\n>>>>>>> origin/main\nafter\n";
        let regions = parse_conflict_region_records(working);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 6);
        assert_eq!(regions[0].ours_lines, vec!["fork editor".to_string()]);
        assert_eq!(regions[0].theirs_lines, vec!["upstream editor".to_string()]);
    }

    #[test]
    fn merge_review_conflict_sides_json_reads_git_stages_from_conflicted_merge() {
        smol::block_on(async {
            let fixture = ConflictSidesGitFixture::new();
            fixture.begin_conflicted_merge();
            let json = merge_review_conflict_sides_json(
                fixture.path(),
                "origin/main",
                "crates/editor/src/editor.rs",
            )
            .await
            .expect("conflict sides json");
            assert_eq!(json["path"], "crates/editor/src/editor.rs");
            assert!(
                json["ours_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("fork editor"))
            );
            assert!(
                json["theirs_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("upstream editor"))
            );
            assert!(
                json["working_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("<<<<<<<"))
            );
            let regions = json["regions"].as_array().expect("regions array");
            assert_eq!(regions.len(), 1);
            assert_eq!(regions[0]["ours_lines"][0], "fork editor");
            assert_eq!(regions[0]["theirs_lines"][0], "upstream editor");
        });
    }

    struct ConflictSidesGitFixture {
        _root: tempfile::TempDir,
    }

    impl ConflictSidesGitFixture {
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
                "test setup requires a conflicted merge"
            );
        }
    }

    fn git_cmd(repo: &Path, args: &[&str]) {
        let output = git_cmd_allow_fail(repo, args);
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

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
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Resolves a merge conflict using git index commands — never by stripping conflict markers manually.
///
/// Use `ours` to keep the Surmount fork version (HEAD / stage 2).
/// Use `theirs` to take the upstream merge version (incoming / stage 3).
///
/// Only use `edit_file` or `write_file` when both sides must be synthesized into a new hunk;
/// do not delete `<<<<<<<`, `=======`, or `>>>>>>>` lines as the resolution mechanism.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResolveMergeConflictToolInput {
    /// Repository-relative path to the conflicted file, e.g. `crates/agent_ui/src/lib.rs`.
    pub path: String,
    /// `ours` keeps the fork (HEAD) version; `theirs` takes the upstream merge version.
    pub side: ResolveMergeConflictSide,
    /// Stage the file after checkout (`git add`). Defaults to true.
    #[serde(default = "default_stage")]
    pub stage: bool,
}

fn default_stage() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResolveMergeConflictSide {
    Ours,
    Theirs,
}

pub struct ResolveMergeConflictTool {
    project: Entity<Project>,
}

impl ResolveMergeConflictTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ResolveMergeConflictTool {
    type Input = ResolveMergeConflictToolInput;
    type Output = String;

    const NAME: &'static str = "resolve_merge_conflict";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed) => format!("Resolve conflict ({})", parsed.side_label()).into(),
            Err(_) => "Resolve merge conflict".into(),
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
                .ok_or_else(|| "no project worktree for merge conflict resolution".to_string())?;
            resolve_merge_conflict_with_git(&worktree_root, &parsed.path, parsed.side, parsed.stage)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

impl ResolveMergeConflictToolInput {
    fn side_label(&self) -> &'static str {
        match self.side {
            ResolveMergeConflictSide::Ours => "keep fork",
            ResolveMergeConflictSide::Theirs => "take upstream",
        }
    }
}

fn project_worktree_root(project: &Project, cx: &App) -> Option<PathBuf> {
    project
        .worktree_root_names(cx)
        .next()
        .and_then(|name| project.worktree_for_root_name(name, cx))
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

pub async fn resolve_merge_conflict_with_git(
    worktree_root: &Path,
    path: &str,
    side: ResolveMergeConflictSide,
    stage: bool,
) -> Result<String> {
    let normalized = path.replace('\\', "/");
    let checkout_flag = match side {
        ResolveMergeConflictSide::Ours => "--ours",
        ResolveMergeConflictSide::Theirs => "--theirs",
    };
    run_git(
        worktree_root,
        &["checkout", checkout_flag, "--", &normalized],
    )
    .await?;
    if stage {
        run_git(worktree_root, &["add", "--", &normalized]).await?;
    }
    Ok(format!(
        "Resolved {normalized} with `git checkout {checkout_flag}`{}.",
        if stage {
            " and staged with `git add`"
        } else {
            ""
        },
    ))
}

async fn run_git(worktree_root: &Path, args: &[&str]) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_merge_conflict_tool_input_deserializes_side_aliases() {
        let ours: ResolveMergeConflictToolInput =
            serde_json::from_value(serde_json::json!({"path": "lib.rs", "side": "ours"}))
                .expect("ours");
        assert_eq!(ours.side, ResolveMergeConflictSide::Ours);
        assert!(ours.stage);

        let theirs: ResolveMergeConflictToolInput = serde_json::from_value(serde_json::json!(
            {"path": "lib.rs", "side": "theirs", "stage": false}
        ))
        .expect("theirs");
        assert_eq!(theirs.side, ResolveMergeConflictSide::Theirs);
        assert!(!theirs.stage);
    }
}

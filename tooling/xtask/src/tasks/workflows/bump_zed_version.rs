use gh_workflow::*;
use serde_json::json;

use crate::tasks::workflows::{
    runners,
    steps::{self, NamedJob, RepositoryTarget, TokenPermissions, named},
    vars::{StepOutput, WorkflowInput},
};

pub fn bump_zed_version() -> Workflow {
    let bump_type = WorkflowInput::string("bump_type", None)
        .description("Version bump type: major, minor, or patch");
    let patch_channel = WorkflowInput::string("patch_channel", Some("both".to_string()))
        .description("For patch bumps only: preview, stable, or both");

    let plan = plan_version_bump(&bump_type, &patch_channel);
    let execute = execute_version_bump(&plan);

    named::workflow()
        .run_name(format!("bump_zed_version ({bump_type})"))
        .on(Event::default().workflow_dispatch(
            WorkflowDispatch::default()
                .add_input(bump_type.name, bump_type.input())
                .add_input(patch_channel.name, patch_channel.input()),
        ))
        .concurrency(Concurrency::new(Expression::new("bump-zed-version")))
        .add_job(plan.name, plan.job)
        .add_job(execute.name, execute.job)
}

fn plan_version_bump(bump_type: &WorkflowInput, patch_channel: &WorkflowInput) -> NamedJob {
    fn compute_matrix(bump_type: &WorkflowInput, patch_channel: &WorkflowInput) -> Step<Run> {
        named::bash(indoc::indoc! {r#"
            matrix=$(script/plan-zed-version-bump "$BUMP_TYPE" "$PATCH_CHANNEL")
            echo "matrix=$matrix" >> "$GITHUB_OUTPUT"
        "#})
        .id("compute-matrix")
        .add_env(("BUMP_TYPE", bump_type.to_string()))
        .add_env(("PATCH_CHANNEL", patch_channel.to_string()))
    }

    let (authenticate, token) = steps::authenticate_as_zippy().into();
    let step = compute_matrix(bump_type, patch_channel);
    let output = StepOutput::new(&step, "matrix");

    named::job(
        Job::default()
            .cond(Expression::new(
                "github.repository_owner == 'zed-industries'",
            ))
            .runs_on(runners::LINUX_SMALL)
            .add_step(authenticate)
            .add_step(steps::checkout_repo().with_token(&token))
            .add_step(step)
            .outputs([("matrix".to_owned(), output.to_string())]),
    )
}

fn execute_version_bump(plan: &NamedJob) -> NamedJob {
    fn run_version_bump(token: &StepOutput) -> Step<Run> {
        named::bash(indoc::indoc! {r#"
            # Only preview requires a new branch
            if [ "$NEW_CHANNEL" = "preview" ]; then
              git push origin "HEAD:refs/heads/${TARGET_BRANCH}"
            fi

            if [ -n "$BUMP" ]; then
              which cargo-set-version > /dev/null || cargo install cargo-edit -f --no-default-features --features "set-version"
              cargo set-version -p zed --bump "$BUMP" 2>&1
            fi

            if [ -n "$NEW_CHANNEL" ]; then
              printf '%s' "$NEW_CHANNEL" > crates/zed/RELEASE_CHANNEL
            fi

            version=$(script/get-crate-version zed)
            channel=$(cat crates/zed/RELEASE_CHANNEL)

            if [ "$TARGET_BRANCH" = "main" ]; then
              title="Bump Zed to v${version}"
            else
              title="Bump Zed ${channel} to v${version}"
            fi
            pr_branch="bump-zed-to-${version}"

            git checkout -b "$pr_branch"
            git commit -am "$title"
            git push origin "$pr_branch"

            gh pr create \
              --title "$title" \
              --body "Release Notes:\n\n- N/A" \
              --base "$TARGET_BRANCH" \
              --head "$pr_branch"
        "#})
        .add_env(("GIT_COMMITTER_NAME", "Zed Zippy"))
        .add_env((
            "GIT_COMMITTER_EMAIL",
            "234243425+zed-zippy[bot]@users.noreply.github.com",
        ))
        .add_env(("GIT_AUTHOR_NAME", "Zed Zippy"))
        .add_env((
            "GIT_AUTHOR_EMAIL",
            "234243425+zed-zippy[bot]@users.noreply.github.com",
        ))
        .add_env(("GITHUB_TOKEN", token))
        .add_env(("TARGET_BRANCH", "${{ matrix.target_branch }}"))
        .add_env(("BUMP", "${{ matrix.bump }}"))
        .add_env(("NEW_CHANNEL", "${{ matrix.new_channel }}"))
    }

    let (authenticate, token) = steps::authenticate_as_zippy()
        .for_repository(RepositoryTarget::current())
        .with_permissions([
            (TokenPermissions::Contents, Level::Write),
            (TokenPermissions::PullRequests, Level::Write),
        ])
        .into();

    named::job(
        Job::default()
            .needs(vec![plan.name.clone()])
            .cond(Expression::new(format!(
                "needs.{}.outputs.matrix != '[]'",
                plan.name
            )))
            .runs_on(runners::LINUX_XL)
            .strategy(Strategy::default().fail_fast(false).matrix(json!({
                "include": format!("${{{{ fromJson(needs.{}.outputs.matrix) }}}}", plan.name)
            })))
            .add_step(authenticate)
            .add_step(
                steps::checkout_repo()
                    .with_token(&token)
                    .with_ref("${{ matrix.checkout_ref }}"),
            )
            .add_step(run_version_bump(&token)),
    )
}

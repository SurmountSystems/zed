use git_ui::project_diff::{
    BranchDiffToolbar, MergeReviewBranchDiffControls, MergeReviewConflictOutcomeHint,
    MergeReviewConflictWorkshopPhase, MergeReviewGitMode, ReviewDiff,
};
use gpui::{
    Action, AnyElement, Context, ElementId, FocusHandle, Hsla, ParentElement, Role, SharedString,
    Window, rgb,
};
use ui::{Color, Icon, IconName, IconSize, KeyBinding, Label, LabelSize, Tooltip, prelude::*};
use zed_actions::surmount::{
    ConfirmMergeReviewDecisionKeepFork, ConfirmMergeReviewDecisionSynthesize,
    ConfirmMergeReviewDecisionTakeUpstream, DiscussMergeReviewConflict,
    DraftMergeReviewCommitMessage, EndMergeReview, MergeReviewNextFile,
    OpenMergeReviewConflictTodos, PreviewMergeReviewMerge, ResolveMergeReviewConflictOurs,
    ResolveMergeReviewConflictTheirs, SynthesizeMergeReviewConflict,
};

pub const RAIL_BTN_REVIEW_DIFF: &str = "Review Diff";
pub const RAIL_BTN_REVIEW_WORKING: &str = "Summarizing…";
pub const RAIL_BTN_NEXT_FILE: &str = "Next file →";
pub const RAIL_BTN_KEEP_FORK: &str = "Keep fork";
pub const RAIL_BTN_TAKE_UPSTREAM: &str = "Take upstream";
pub const RAIL_BTN_END: &str = "End merge review";
pub const RAIL_BTN_DISCUSS_CONFLICT: &str = "Discuss conflict";
pub const RAIL_BTN_DISCUSSING: &str = "Discussing…";
pub const RAIL_BTN_SYNTHESIZE: &str = "Synthesize";
pub const RAIL_BTN_COMPLETE_TESTS: &str = "Complete tests";
pub const RAIL_BTN_PREVIEW_MERGE: &str = "Preview merge";
pub const RAIL_BTN_DRAFT_COMMIT_MESSAGE: &str = "Draft commit message";

/// AccessKit / room-outline landmark label for the merge-review step rail.
pub const MERGE_REVIEW_RAIL_A11Y_LABEL: &str = "Merge review";

/// Merge review workflow control border (`#0f0`), dark-mode first.
pub fn merge_review_workflow_green_border() -> Hsla {
    rgb(0x00ff00).into()
}

pub fn merge_review_workflow_danger_border() -> Hsla {
    rgb(0xff0000).into()
}

pub fn merge_review_workflow_label_color() -> Color {
    Color::Custom(rgb(0xffffff).into())
}

pub fn merge_review_workflow_primary_fill() -> Hsla {
    gpui::hsla(0., 0., 0.12, 1.)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReviewWorkflowButtonTier {
    Primary,
    Available,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReviewUiStep {
    AllComplete,
    ReviewWorking,
    ConflictWorkshop {
        phase: MergeReviewConflictWorkshopPhase,
        emphasize: Option<MergeReviewConflictOutcomeHint>,
    },
    SummarizedNext,
    ReviewReady,
    PickFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReviewPrimaryAction {
    EndMergeReview,
    ReviewDiffWorking,
    KeepFork,
    TakeUpstream,
    NextFile,
    ReviewDiff,
    DiscussConflict,
    DiscussConflictWorking,
    SynthesizeConflict,
    ConfirmKeepFork,
    ConfirmTakeUpstream,
    ConfirmSynthesize,
    CompleteTests,
}

pub fn merge_review_session_complete(cx: &gpui::App) -> bool {
    crate::merge_review::load_session(cx)
        .is_some_and(|session| crate::merge_review::session_summarized_complete(&session))
}

pub fn merge_review_ui_step(
    controls: &MergeReviewBranchDiffControls,
    in_flight: bool,
    file_selected: bool,
    session_complete: bool,
) -> MergeReviewUiStep {
    if session_complete {
        return MergeReviewUiStep::AllComplete;
    }
    if in_flight || controls.awaiting_agent_summary {
        return MergeReviewUiStep::ReviewWorking;
    }
    if let Some(phase) = controls.conflict_workshop_phase {
        if phase != MergeReviewConflictWorkshopPhase::NotSummarized {
            return MergeReviewUiStep::ConflictWorkshop {
                phase,
                emphasize: controls.suggested_outcome,
            };
        }
    } else if controls.show_conflict_resolution {
        return MergeReviewUiStep::ConflictWorkshop {
            phase: MergeReviewConflictWorkshopPhase::DiscussReady,
            emphasize: controls.suggested_outcome,
        };
    }
    if controls.current_file_done {
        return MergeReviewUiStep::SummarizedNext;
    }
    if controls.review_diff_ready && file_selected {
        return MergeReviewUiStep::ReviewReady;
    }
    MergeReviewUiStep::PickFile
}

pub fn merge_review_primary_action(step: MergeReviewUiStep) -> MergeReviewPrimaryAction {
    match step {
        MergeReviewUiStep::AllComplete => MergeReviewPrimaryAction::EndMergeReview,
        MergeReviewUiStep::ReviewWorking => MergeReviewPrimaryAction::ReviewDiffWorking,
        MergeReviewUiStep::ConflictWorkshop { phase, emphasize } => match phase {
            MergeReviewConflictWorkshopPhase::NotSummarized => MergeReviewPrimaryAction::ReviewDiff,
            MergeReviewConflictWorkshopPhase::DiscussReady => {
                MergeReviewPrimaryAction::DiscussConflict
            }
            MergeReviewConflictWorkshopPhase::Discussing => {
                MergeReviewPrimaryAction::DiscussConflictWorking
            }
            MergeReviewConflictWorkshopPhase::RecordDecision => match emphasize {
                Some(MergeReviewConflictOutcomeHint::TakeUpstream) => {
                    MergeReviewPrimaryAction::ConfirmTakeUpstream
                }
                Some(MergeReviewConflictOutcomeHint::Synthesize) => {
                    MergeReviewPrimaryAction::ConfirmSynthesize
                }
                _ => MergeReviewPrimaryAction::ConfirmKeepFork,
            },
            MergeReviewConflictWorkshopPhase::CompleteTests => {
                MergeReviewPrimaryAction::CompleteTests
            }
            MergeReviewConflictWorkshopPhase::ReadyToAdvance => MergeReviewPrimaryAction::NextFile,
        },
        MergeReviewUiStep::SummarizedNext | MergeReviewUiStep::PickFile => {
            MergeReviewPrimaryAction::NextFile
        }
        MergeReviewUiStep::ReviewReady => MergeReviewPrimaryAction::ReviewDiff,
    }
}

/// Toolbar shell for the merge-review step rail (role+label shared with paint tests).
pub fn merge_review_step_rail_container(
    status_label: impl Into<SharedString>,
) -> gpui::Stateful<Div> {
    let status_label = status_label.into();
    h_flex()
        .id("merge-review-step-rail")
        .role(Role::Toolbar)
        .aria_label(MERGE_REVIEW_RAIL_A11Y_LABEL)
        .gap_2()
        .items_center()
        .flex_wrap()
        .child(
            div()
                .id("merge-review-rail-status")
                .role(Role::Label)
                .aria_label(status_label.clone())
                .child(
                    Label::new(status_label)
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                ),
        )
}

/// AccessKit role+label for a merge-review rail control (must match production buttons).
pub fn with_merge_review_rail_button_a11y(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<Div> {
    div().id(id).role(Role::Button).aria_label(label)
}

pub fn render_merge_review_step_rail(
    toolbar: &BranchDiffToolbar,
    controls: &MergeReviewBranchDiffControls,
    review_diff_in_flight: bool,
    file_selected: bool,
    is_ai_enabled: bool,
    focus_handle: &FocusHandle,
    _window: &mut Window,
    cx: &mut Context<BranchDiffToolbar>,
) -> AnyElement {
    if !is_ai_enabled {
        return div().into_any();
    }
    let session_complete = merge_review_session_complete(cx);
    let review_in_progress = review_diff_in_flight || controls.awaiting_agent_summary;
    let step = merge_review_ui_step(
        controls,
        review_in_progress,
        file_selected,
        session_complete,
    );
    let primary = merge_review_primary_action(step);
    let workshop_phase = controls.conflict_workshop_phase;
    let status_label = if controls.step_label.is_empty() {
        controls.progress_label.to_string()
    } else {
        format!("{} · {}", controls.progress_label, controls.step_label)
    };
    let mut rail = merge_review_step_rail_container(status_label);
    for spec in workflow_button_specs(
        step,
        primary,
        review_in_progress,
        workshop_phase,
        controls.git_mode,
    ) {
        rail = rail.child(merge_review_workflow_button(
            toolbar,
            spec.id,
            spec.label,
            spec.tier,
            spec.disabled,
            spec.tooltip,
            spec.action.as_ref(),
            focus_handle,
            cx,
        ));
    }
    rail.child(merge_review_workflow_button(
        toolbar,
        "merge-review-rail-end",
        RAIL_BTN_END,
        MergeReviewWorkflowButtonTier::Danger,
        false,
        "End merge review session and restore docks",
        &EndMergeReview,
        focus_handle,
        cx,
    ))
    .into_any()
}

pub(crate) struct WorkflowButtonSpec {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) tier: MergeReviewWorkflowButtonTier,
    disabled: bool,
    tooltip: &'static str,
    action: Box<dyn Action>,
}

#[cfg(test)]
pub(crate) fn workflow_button_labels(
    step: MergeReviewUiStep,
    primary: MergeReviewPrimaryAction,
    review_in_progress: bool,
    workshop_phase: Option<MergeReviewConflictWorkshopPhase>,
    git_mode: MergeReviewGitMode,
) -> Vec<&'static str> {
    workflow_button_specs(step, primary, review_in_progress, workshop_phase, git_mode)
        .into_iter()
        .map(|spec| spec.label)
        .collect()
}

pub(crate) fn workflow_button_specs(
    step: MergeReviewUiStep,
    primary: MergeReviewPrimaryAction,
    review_in_progress: bool,
    workshop_phase: Option<MergeReviewConflictWorkshopPhase>,
    git_mode: MergeReviewGitMode,
) -> Vec<WorkflowButtonSpec> {
    let tier = |is_primary: bool, disabled: bool| {
        if disabled {
            MergeReviewWorkflowButtonTier::Available
        } else if is_primary {
            MergeReviewWorkflowButtonTier::Primary
        } else {
            MergeReviewWorkflowButtonTier::Available
        }
    };
    let mut specs = Vec::new();
    let push = |specs: &mut Vec<WorkflowButtonSpec>, spec: WorkflowButtonSpec| {
        specs.push(spec);
    };

    if git_mode == MergeReviewGitMode::PreMerge {
        push(
            &mut specs,
            WorkflowButtonSpec {
                id: "merge-review-rail-preview-merge",
                label: RAIL_BTN_PREVIEW_MERGE,
                tier: MergeReviewWorkflowButtonTier::Primary,
                disabled: false,
                tooltip: "Preview git merge-tree before running git merge (human-gated)",
                action: Box::new(PreviewMergeReviewMerge),
            },
        );
        return specs;
    }

    match step {
        MergeReviewUiStep::AllComplete => {
            push(
                &mut specs,
                WorkflowButtonSpec {
                    id: "merge-review-rail-draft-commit",
                    label: RAIL_BTN_DRAFT_COMMIT_MESSAGE,
                    tier: MergeReviewWorkflowButtonTier::Primary,
                    disabled: false,
                    tooltip: "Draft merge commit message from session memory",
                    action: Box::new(DraftMergeReviewCommitMessage),
                },
            );
        }
        MergeReviewUiStep::ReviewWorking => {
            push(
                &mut specs,
                WorkflowButtonSpec {
                    id: "merge-review-rail-review-diff",
                    label: RAIL_BTN_REVIEW_WORKING,
                    tier: MergeReviewWorkflowButtonTier::Primary,
                    disabled: true,
                    tooltip: "Agent summarizing this file",
                    action: Box::new(ReviewDiff),
                },
            );
        }
        MergeReviewUiStep::ReviewReady => {
            push(
                &mut specs,
                WorkflowButtonSpec {
                    id: "merge-review-rail-review-diff",
                    label: RAIL_BTN_REVIEW_DIFF,
                    tier: MergeReviewWorkflowButtonTier::Primary,
                    disabled: false,
                    tooltip: "Scoped agent turn to summarize this diff",
                    action: Box::new(ReviewDiff),
                },
            );
        }
        MergeReviewUiStep::PickFile | MergeReviewUiStep::SummarizedNext => {
            push(
                &mut specs,
                WorkflowButtonSpec {
                    id: "merge-review-rail-next-file",
                    label: RAIL_BTN_NEXT_FILE,
                    tier: MergeReviewWorkflowButtonTier::Primary,
                    disabled: false,
                    tooltip: "Select the next merge-review file",
                    action: Box::new(MergeReviewNextFile),
                },
            );
        }
        MergeReviewUiStep::ConflictWorkshop { phase, .. } => match phase {
            MergeReviewConflictWorkshopPhase::NotSummarized => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-review-diff",
                        label: if review_in_progress {
                            RAIL_BTN_REVIEW_WORKING
                        } else {
                            RAIL_BTN_REVIEW_DIFF
                        },
                        tier: MergeReviewWorkflowButtonTier::Primary,
                        disabled: review_in_progress,
                        tooltip: "Summarize this conflict file before resolving",
                        action: Box::new(ReviewDiff),
                    },
                );
            }
            MergeReviewConflictWorkshopPhase::DiscussReady => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-discuss",
                        label: RAIL_BTN_DISCUSS_CONFLICT,
                        tier: tier(primary == MergeReviewPrimaryAction::DiscussConflict, false),
                        disabled: false,
                        tooltip: "Scoped Q&A about this conflict — sends immediately",
                        action: Box::new(DiscussMergeReviewConflict),
                    },
                );
                if git_mode == MergeReviewGitMode::MergeInProgress {
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-keep-fork",
                            label: RAIL_BTN_KEEP_FORK,
                            tier: tier(primary == MergeReviewPrimaryAction::KeepFork, false),
                            disabled: false,
                            tooltip: "Resolve with git checkout --ours",
                            action: Box::new(ResolveMergeReviewConflictOurs),
                        },
                    );
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-take-upstream",
                            label: RAIL_BTN_TAKE_UPSTREAM,
                            tier: tier(primary == MergeReviewPrimaryAction::TakeUpstream, false),
                            disabled: false,
                            tooltip: "Resolve with git checkout --theirs",
                            action: Box::new(ResolveMergeReviewConflictTheirs),
                        },
                    );
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-synthesize",
                            label: RAIL_BTN_SYNTHESIZE,
                            tier: tier(
                                primary == MergeReviewPrimaryAction::SynthesizeConflict,
                                false,
                            ),
                            disabled: false,
                            tooltip: "Agent synthesizes — add direction in composer, then Send",
                            action: Box::new(SynthesizeMergeReviewConflict),
                        },
                    );
                }
            }
            MergeReviewConflictWorkshopPhase::Discussing => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-discussing",
                        label: RAIL_BTN_DISCUSSING,
                        tier: MergeReviewWorkflowButtonTier::Primary,
                        disabled: true,
                        tooltip: "Discuss turn in progress",
                        action: Box::new(DiscussMergeReviewConflict),
                    },
                );
                if git_mode == MergeReviewGitMode::MergeInProgress {
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-keep-fork",
                            label: RAIL_BTN_KEEP_FORK,
                            tier: MergeReviewWorkflowButtonTier::Available,
                            disabled: false,
                            tooltip: "Resolve with git checkout --ours",
                            action: Box::new(ResolveMergeReviewConflictOurs),
                        },
                    );
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-take-upstream",
                            label: RAIL_BTN_TAKE_UPSTREAM,
                            tier: MergeReviewWorkflowButtonTier::Available,
                            disabled: false,
                            tooltip: "Resolve with git checkout --theirs",
                            action: Box::new(ResolveMergeReviewConflictTheirs),
                        },
                    );
                    push(
                        &mut specs,
                        WorkflowButtonSpec {
                            id: "merge-review-rail-synthesize",
                            label: RAIL_BTN_SYNTHESIZE,
                            tier: MergeReviewWorkflowButtonTier::Available,
                            disabled: false,
                            tooltip: "Agent synthesizes — add direction in composer, then Send",
                            action: Box::new(SynthesizeMergeReviewConflict),
                        },
                    );
                }
            }
            MergeReviewConflictWorkshopPhase::RecordDecision => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-record-keep",
                        label: RAIL_BTN_KEEP_FORK,
                        tier: tier(primary == MergeReviewPrimaryAction::ConfirmKeepFork, false),
                        disabled: false,
                        tooltip: "Record decision: keep fork version",
                        action: Box::new(ConfirmMergeReviewDecisionKeepFork),
                    },
                );
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-record-upstream",
                        label: RAIL_BTN_TAKE_UPSTREAM,
                        tier: tier(
                            primary == MergeReviewPrimaryAction::ConfirmTakeUpstream,
                            false,
                        ),
                        disabled: false,
                        tooltip: "Record decision: take upstream version",
                        action: Box::new(ConfirmMergeReviewDecisionTakeUpstream),
                    },
                );
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-record-synthesize",
                        label: RAIL_BTN_SYNTHESIZE,
                        tier: tier(
                            primary == MergeReviewPrimaryAction::ConfirmSynthesize,
                            false,
                        ),
                        disabled: false,
                        tooltip: "Record decision: synthesize",
                        action: Box::new(ConfirmMergeReviewDecisionSynthesize),
                    },
                );
            }
            MergeReviewConflictWorkshopPhase::CompleteTests => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-complete-tests",
                        label: RAIL_BTN_COMPLETE_TESTS,
                        tier: MergeReviewWorkflowButtonTier::Primary,
                        disabled: false,
                        tooltip: "Open conflict Plan Todos",
                        action: Box::new(OpenMergeReviewConflictTodos),
                    },
                );
            }
            MergeReviewConflictWorkshopPhase::ReadyToAdvance => {
                push(
                    &mut specs,
                    WorkflowButtonSpec {
                        id: "merge-review-rail-next-file",
                        label: RAIL_BTN_NEXT_FILE,
                        tier: MergeReviewWorkflowButtonTier::Primary,
                        disabled: false,
                        tooltip: "Advance to the next merge-review file",
                        action: Box::new(MergeReviewNextFile),
                    },
                );
            }
        },
    }
    let _ = workshop_phase;
    specs
}

fn merge_review_workflow_button(
    _toolbar: &BranchDiffToolbar,
    id: &'static str,
    label: &'static str,
    tier: MergeReviewWorkflowButtonTier,
    disabled: bool,
    tooltip_text: &'static str,
    action: &dyn Action,
    focus_handle: &FocusHandle,
    cx: &mut Context<BranchDiffToolbar>,
) -> AnyElement {
    let green = merge_review_workflow_green_border();
    let danger = merge_review_workflow_danger_border();
    let muted_border = gpui::hsla(0., 0., 0.35, 1.);
    let border_color = if disabled {
        muted_border
    } else {
        match tier {
            MergeReviewWorkflowButtonTier::Danger => danger,
            _ => green,
        }
    };
    let label_color = if disabled {
        Color::Muted
    } else {
        merge_review_workflow_label_color()
    };
    let action_for_click = action.boxed_clone();
    let action_for_tooltip = action.boxed_clone();
    let keybinding = (!disabled).then(|| KeyBinding::for_action_in(action, focus_handle, cx));
    with_merge_review_rail_button_a11y(id, label)
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .border_1()
        .border_color(border_color)
        .when(
            tier == MergeReviewWorkflowButtonTier::Primary && !disabled,
            |this| this.bg(merge_review_workflow_primary_fill()),
        )
        .when(disabled, |this| this.opacity(0.4).cursor_not_allowed())
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(merge_review_workflow_primary_fill().opacity(0.85)))
        })
        .child(
            Label::new(label)
                .size(LabelSize::Default)
                .color(label_color),
        )
        .when(id == "merge-review-rail-review-diff" && !disabled, |this| {
            this.child(
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::Small)
                    .color(label_color),
            )
        })
        .tooltip({
            let focus_handle = focus_handle.clone();
            move |_, cx| {
                Tooltip::with_meta_in(
                    tooltip_text,
                    Some(action_for_tooltip.as_ref()),
                    tooltip_text,
                    &focus_handle,
                    cx,
                )
            }
        })
        .when(!disabled, |this| {
            this.on_click(cx.listener(move |this, _, window, cx| {
                this.dispatch_action(action_for_click.as_ref(), window, cx);
            }))
        })
        .when_some(keybinding, |this, binding| this.child(binding))
        .into_any()
}

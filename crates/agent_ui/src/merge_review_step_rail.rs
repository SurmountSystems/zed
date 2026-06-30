use git_ui::project_diff::{
    BranchDiffToolbar, MergeReviewBranchDiffControls, MergeReviewConflictOutcomeHint, ReviewDiff,
};
use gpui::{Action, AnyElement, Context, FocusHandle, ParentElement, Window};
use ui::{
    Button, ButtonStyle, Color, Divider, Icon, IconName, IconSize, KeyBinding, Label, LabelSize,
    TintColor, Tooltip, prelude::*,
};
use zed_actions::surmount::{
    EndMergeReview, MergeReviewNextFile, ResolveMergeReviewConflictOurs,
    ResolveMergeReviewConflictTheirs,
};

pub const RAIL_BTN_REVIEW_DIFF: &str = "Review Diff";
pub const RAIL_BTN_REVIEW_WORKING: &str = "Summarizing…";
pub const RAIL_BTN_NEXT_FILE: &str = "Next file →";
pub const RAIL_BTN_KEEP_FORK: &str = "Keep fork";
pub const RAIL_BTN_TAKE_UPSTREAM: &str = "Take upstream";
pub const RAIL_BTN_END: &str = "End merge review";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReviewUiStep {
    AllComplete,
    ReviewWorking,
    ConflictResolve {
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
}

pub fn merge_review_session_complete(cx: &gpui::App) -> bool {
    crate::merge_review::load_session(cx).is_some_and(|session| {
        !session.items.is_empty() && session.reviewed_count() == session.items.len()
    })
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
    if controls.show_conflict_resolution {
        return MergeReviewUiStep::ConflictResolve {
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
        MergeReviewUiStep::ConflictResolve { emphasize } => match emphasize {
            Some(MergeReviewConflictOutcomeHint::KeepFork) => MergeReviewPrimaryAction::KeepFork,
            _ => MergeReviewPrimaryAction::TakeUpstream,
        },
        MergeReviewUiStep::SummarizedNext | MergeReviewUiStep::PickFile => {
            MergeReviewPrimaryAction::NextFile
        }
        MergeReviewUiStep::ReviewReady => MergeReviewPrimaryAction::ReviewDiff,
    }
}

pub fn render_merge_review_step_rail(
    toolbar: &BranchDiffToolbar,
    controls: &MergeReviewBranchDiffControls,
    review_diff_in_flight: bool,
    file_selected: bool,
    is_ai_enabled: bool,
    focus_handle: &FocusHandle,
    window: &mut Window,
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
    let show_conflict_buttons = controls.is_conflict_file || controls.show_conflict_resolution;

    h_flex()
        .gap_2()
        .items_center()
        .flex_wrap()
        .child(
            Label::new(controls.progress_label.clone())
                .size(LabelSize::Default)
                .color(Color::Muted),
        )
        .child(Divider::vertical())
        .child(rail_button(
            toolbar,
            "merge-review-rail-next-file",
            RAIL_BTN_NEXT_FILE,
            primary == MergeReviewPrimaryAction::NextFile,
            false,
            TintColor::Accent,
            &MergeReviewNextFile,
            focus_handle,
            window,
            cx,
        ))
        .child({
            let review_primary = matches!(
                primary,
                MergeReviewPrimaryAction::ReviewDiff | MergeReviewPrimaryAction::ReviewDiffWorking
            );
            let review_disabled = review_in_progress
                || matches!(
                    step,
                    MergeReviewUiStep::SummarizedNext
                        | MergeReviewUiStep::ConflictResolve { .. }
                        | MergeReviewUiStep::AllComplete
                );
            let review_label = if review_in_progress {
                RAIL_BTN_REVIEW_WORKING
            } else {
                RAIL_BTN_REVIEW_DIFF
            };
            let review_tint = if review_in_progress {
                TintColor::Accent
            } else {
                TintColor::Success
            };
            rail_button(
                toolbar,
                "merge-review-rail-review-diff",
                review_label,
                review_primary,
                review_disabled,
                review_tint,
                &ReviewDiff,
                focus_handle,
                window,
                cx,
            )
            .start_icon(
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::Small)
                    .color(Color::Success),
            )
        })
        .when(show_conflict_buttons, |this| {
            this.child(rail_button(
                toolbar,
                "merge-review-rail-keep-fork",
                RAIL_BTN_KEEP_FORK,
                primary == MergeReviewPrimaryAction::KeepFork,
                false,
                TintColor::Success,
                &ResolveMergeReviewConflictOurs,
                focus_handle,
                window,
                cx,
            ))
            .child(rail_button(
                toolbar,
                "merge-review-rail-take-upstream",
                RAIL_BTN_TAKE_UPSTREAM,
                primary == MergeReviewPrimaryAction::TakeUpstream,
                false,
                TintColor::Accent,
                &ResolveMergeReviewConflictTheirs,
                focus_handle,
                window,
                cx,
            ))
        })
        .child(rail_button(
            toolbar,
            "merge-review-rail-end",
            RAIL_BTN_END,
            primary == MergeReviewPrimaryAction::EndMergeReview,
            false,
            TintColor::Error,
            &EndMergeReview,
            focus_handle,
            window,
            cx,
        ))
        .into_any()
}

fn rail_button(
    _toolbar: &BranchDiffToolbar,
    id: &'static str,
    label: &'static str,
    primary: bool,
    disabled: bool,
    tint: TintColor,
    action: &dyn Action,
    focus_handle: &FocusHandle,
    _window: &mut Window,
    cx: &mut Context<BranchDiffToolbar>,
) -> Button {
    let mut button = Button::new(id, label);
    if primary {
        button = button.style(ButtonStyle::Tinted(tint));
    } else {
        button = button.style(ButtonStyle::Outlined);
    }
    if disabled {
        button = button.disabled(true);
    } else {
        button = button.key_binding(KeyBinding::for_action_in(action, focus_handle, cx));
    }
    let action_label = label;
    let action_for_click = action.boxed_clone();
    let action_for_tooltip = action.boxed_clone();
    button
        .tooltip({
            let focus_handle = focus_handle.clone();
            move |_, cx| {
                Tooltip::with_meta_in(
                    action_label,
                    Some(action_for_tooltip.as_ref()),
                    action_label,
                    &focus_handle,
                    cx,
                )
            }
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            if !disabled {
                this.dispatch_action(action_for_click.as_ref(), window, cx);
            }
        }))
}

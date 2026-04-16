use crate::{CommonAnimationExt, DiffStat, GradientFade, HighlightedLabel, Tooltip, prelude::*};

use gpui::{
    Animation, AnimationExt, ClickEvent, Hsla, MouseButton, SharedString, pulsating_between,
};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentThreadStatus {
    #[default]
    Completed,
    Running,
    WaitingForConfirmation,
    Error,
}

#[derive(Clone)]
pub struct ThreadItemWorktreeInfo {
    pub name: SharedString,
    pub full_path: SharedString,
    pub highlight_positions: Vec<usize>,
}

#[derive(IntoElement, RegisterComponent)]
pub struct ThreadItem {
    id: ElementId,
    icon: IconName,
    icon_color: Option<Color>,
    icon_visible: bool,
    custom_icon_from_external_svg: Option<SharedString>,
    title: SharedString,
    title_label_color: Option<Color>,
    title_generating: bool,
    highlight_positions: Vec<usize>,
    timestamp: SharedString,
    notified: bool,
    status: AgentThreadStatus,
    selected: bool,
    focused: bool,
    hovered: bool,
    rounded: bool,
    added: Option<usize>,
    removed: Option<usize>,
    project_name: Option<SharedString>,
    worktree: Option<ThreadItemWorktreeInfo>,
    branch_name: Option<SharedString>,
    is_remote: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
    action_slot: Option<AnyElement>,
    base_bg: Option<Hsla>,
}

impl ThreadItem {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon: IconName::ZedAgent,
            icon_color: None,
            icon_visible: true,
            custom_icon_from_external_svg: None,
            title: title.into(),
            title_label_color: None,
            title_generating: false,
            highlight_positions: Vec::new(),
            timestamp: "".into(),
            notified: false,
            status: AgentThreadStatus::default(),
            selected: false,
            focused: false,
            hovered: false,
            rounded: false,
            added: None,
            removed: None,
            project_name: None,
            worktree: None,
            branch_name: None,
            is_remote: false,
            on_click: None,
            on_hover: Box::new(|_, _, _| {}),
            action_slot: None,
            base_bg: None,
        }
    }

    pub fn timestamp(mut self, timestamp: impl Into<SharedString>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = icon;
        self
    }

    pub fn icon_color(mut self, color: Color) -> Self {
        self.icon_color = Some(color);
        self
    }

    pub fn icon_visible(mut self, visible: bool) -> Self {
        self.icon_visible = visible;
        self
    }

    pub fn custom_icon_from_external_svg(mut self, svg: impl Into<SharedString>) -> Self {
        self.custom_icon_from_external_svg = Some(svg.into());
        self
    }

    pub fn notified(mut self, notified: bool) -> Self {
        self.notified = notified;
        self
    }

    pub fn status(mut self, status: AgentThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn title_generating(mut self, generating: bool) -> Self {
        self.title_generating = generating;
        self
    }

    pub fn title_label_color(mut self, color: Color) -> Self {
        self.title_label_color = Some(color);
        self
    }

    pub fn highlight_positions(mut self, positions: Vec<usize>) -> Self {
        self.highlight_positions = positions;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn added(mut self, added: usize) -> Self {
        self.added = Some(added);
        self
    }

    pub fn removed(mut self, removed: usize) -> Self {
        self.removed = Some(removed);
        self
    }

    pub fn project_name(mut self, name: impl Into<SharedString>) -> Self {
        self.project_name = Some(name.into());
        self
    }

    pub fn worktree(mut self, worktree: ThreadItemWorktreeInfo) -> Self {
        self.worktree = Some(worktree);
        self
    }

    pub fn branch_name(mut self, branch_name: impl Into<SharedString>) -> Self {
        self.branch_name = Some(branch_name.into());
        self
    }

    pub fn is_remote(mut self, is_remote: bool) -> Self {
        self.is_remote = is_remote;
        self
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn rounded(mut self, rounded: bool) -> Self {
        self.rounded = rounded;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Box::new(on_hover);
        self
    }

    pub fn action_slot(mut self, element: impl IntoElement) -> Self {
        self.action_slot = Some(element.into_any_element());
        self
    }

    pub fn base_bg(mut self, color: Hsla) -> Self {
        self.base_bg = Some(color);
        self
    }
}

impl RenderOnce for ThreadItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = cx.theme().colors();
        let sidebar_base_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let raw_bg = self.base_bg.unwrap_or(sidebar_base_bg);
        let apparent_bg = color.background.blend(raw_bg);

        let base_bg = if self.selected {
            apparent_bg.blend(color.element_active)
        } else {
            apparent_bg
        };

        let hover_color = color
            .element_active
            .blend(color.element_background.opacity(0.2));
        let hover_bg = apparent_bg.blend(hover_color);

        let gradient_overlay = GradientFade::new(base_bg, hover_bg, hover_bg)
            .width(px(64.0))
            .right(px(-10.0))
            .gradient_stop(0.75)
            .group_name("thread-item");

        let separator_color = Color::Custom(cx.theme().colors().text_muted.opacity(0.3));
        let dot_separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(separator_color)
                .mx_1()
        };

        let icon_id = format!("icon-{}", self.id);
        let icon_visible = self.icon_visible;
        let icon_container = || {
            h_flex()
                .id(icon_id.clone())
                .size_4()
                .flex_none()
                .justify_center()
                .when(!icon_visible, |this| this.invisible())
        };
        let icon_color = self.icon_color.unwrap_or(Color::Muted);
        let agent_icon = if let Some(custom_svg) = self.custom_icon_from_external_svg {
            Icon::from_external_svg(custom_svg)
                .color(icon_color)
                .size(IconSize::Small)
        } else {
            Icon::new(self.icon).color(icon_color).size(IconSize::Small)
        };

        let status_icon = if self.status == AgentThreadStatus::Error {
            Some(
                Icon::new(IconName::Close)
                    .size(IconSize::Small)
                    .color(Color::Error),
            )
        } else if self.status == AgentThreadStatus::WaitingForConfirmation {
            Some(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
        } else if self.notified {
            Some(
                Icon::new(IconName::Circle)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        } else {
            None
        };

        let icon = if self.status == AgentThreadStatus::Running {
            icon_container()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .into_any_element()
        } else if let Some(status_icon) = status_icon {
            icon_container().child(status_icon).into_any_element()
        } else {
            icon_container().child(agent_icon).into_any_element()
        };

        let tooltip_status = self.status;
        let title = self.title;
        let highlight_positions = self.highlight_positions;

        let title_label = if self.title_generating {
            Label::new(title)
                .color(Color::Muted)
                .with_animation(
                    "generating-title",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else if highlight_positions.is_empty() {
            Label::new(title)
                .when_some(self.title_label_color, |label, color| label.color(color))
                .into_any_element()
        } else {
            HighlightedLabel::new(title, highlight_positions)
                .when_some(self.title_label_color, |label, color| label.color(color))
                .into_any_element()
        };

        let has_diff_stats = self.added.is_some() || self.removed.is_some();
        let diff_stat_id = self.id.clone();
        let added_count = self.added.unwrap_or(0);
        let removed_count = self.removed.unwrap_or(0);

        let has_project_name = self.project_name.is_some();
        let has_worktree = self.worktree.is_some();
        let has_branch = self.branch_name.is_some();
        let has_timestamp = !self.timestamp.is_empty();
        let timestamp = self.timestamp;

        let has_any_metadata =
            has_project_name || has_worktree || has_branch || has_diff_stats || has_timestamp;

        let unified_tooltip = {
            let status = tooltip_status;

            Tooltip::element(move |_, _| {
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .when(status == AgentThreadStatus::Error, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Close)
                                        .size(IconSize::Small)
                                        .color(Color::Error),
                                )
                                .child(Label::new("Thread has an Error").size(LabelSize::Small)),
                        )
                    })
                    .when(
                        status == AgentThreadStatus::WaitingForConfirmation,
                        |this| {
                            this.child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::Small)
                                            .color(Color::Warning),
                                    )
                                    .child(
                                        Label::new("Waiting for Confirmation")
                                            .size(LabelSize::Small),
                                    ),
                            )
                        },
                    )
                    .into_any_element()
            })
        };

        v_flex()
            .id(self.id.clone())
            .cursor_pointer()
            .group("thread-item")
            .relative()
            .overflow_hidden()
            .w_full()
            .py_1()
            .px_1p5()
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.focused, |s| s.border_color(color.border_focused))
            .when(self.rounded, |s| s.rounded_sm())
            .when(self.selected, |s| s.bg(color.element_active))
            .hover(|s| s.bg(hover_color))
            .on_hover(self.on_hover)
            .tooltip(unified_tooltip)
            .child(
                h_flex()
                    .min_w_0()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("content")
                            .min_w_0()
                            .flex_1()
                            .gap_1p5()
                            .child(icon)
                            .child(title_label),
                    )
                    .child(gradient_overlay)
                    .when(self.hovered, |this| {
                        this.when_some(self.action_slot, |this, slot| {
                            let overlay = GradientFade::new(base_bg, hover_bg, hover_bg)
                                .width(px(64.0))
                                .right(px(6.))
                                .gradient_stop(0.75)
                                .group_name("thread-item");

                            this.child(
                                h_flex()
                                    .relative()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(overlay)
                                    .child(slot),
                            )
                        })
                    }),
            )
            .when(has_any_metadata, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_0p5()
                        .child(icon_container().mr_1()) // Icon Spacing
                        .when_some(self.project_name, |this, name| {
                            this.child(Label::new(name).size(LabelSize::Small).color(Color::Muted))
                        })
                        .when(has_project_name && (has_worktree || has_branch), |this| {
                            this.child(dot_separator())
                        })
                        .when_some(self.worktree, |this, wt| {
                            let worktree_name_label = if wt.highlight_positions.is_empty() {
                                Label::new(wt.name)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element()
                            } else {
                                HighlightedLabel::new(wt.name, wt.highlight_positions)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element()
                            };

                            this.child(
                                Icon::new(IconName::GitWorktree)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(worktree_name_label)
                        })
                        .when(has_worktree && has_branch, |this| {
                            this.child(
                                Label::new("/")
                                    .size(LabelSize::Small)
                                    .color(separator_color),
                            )
                        })
                        .when_some(self.branch_name, |this, branch| {
                            this.child(
                                Label::new(branch)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        })
                        .when(
                            (has_project_name || has_worktree || has_branch)
                                && (has_diff_stats || has_timestamp),
                            |this| this.child(dot_separator()),
                        )
                        .when(has_diff_stats, |this| {
                            this.child(DiffStat::new(diff_stat_id, added_count, removed_count))
                        })
                        .when(has_diff_stats && has_timestamp, |this| {
                            this.child(dot_separator())
                        })
                        .when(has_timestamp, |this| {
                            this.child(
                                Label::new(timestamp.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
            })
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
    }
}

impl Component for ThreadItem {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn preview(_window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let container = || {
            v_flex()
                .w_72()
                .border_1()
                .border_color(color.border_variant)
                .bg(bg)
        };

        let thread_item_examples = vec![
            single_example(
                "Default",
                container()
                    .child(
                        ThreadItem::new("ti-1", "Linking to the Agent Panel Depending on Settings")
                            .icon(IconName::AiOpenAi)
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Notified",
                container()
                    .child(
                        ThreadItem::new("ti-2", "Refine thread view scrolling behavior")
                            .timestamp("1w")
                            .notified(true),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Waiting for Confirmation",
                container()
                    .child(
                        ThreadItem::new("ti-2b", "Execute shell command in terminal")
                            .timestamp("2h")
                            .status(AgentThreadStatus::WaitingForConfirmation),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Error",
                container()
                    .child(
                        ThreadItem::new("ti-2c", "Failed to connect to language server")
                            .timestamp("5h")
                            .status(AgentThreadStatus::Error),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Running Agent",
                container()
                    .child(
                        ThreadItem::new("ti-3", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("23h")
                            .status(AgentThreadStatus::Running),
                    )
                    .into_any_element(),
            ),
            single_example(
                "In Worktree",
                container()
                    .child(
                        ThreadItem::new("ti-4", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("2w")
                            .worktree(ThreadItemWorktreeInfo {
                                name: "link-agent-panel".into(),
                                full_path: "link-agent-panel".into(),
                                highlight_positions: Vec::new(),
                            }),
                    )
                    .into_any_element(),
            ),
            single_example(
                "With Changes (months)",
                container()
                    .child(
                        ThreadItem::new("ti-5", "Managing user and project settings interactions")
                            .icon(IconName::AiClaude)
                            .timestamp("1mo")
                            .added(10)
                            .removed(3),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5b", "Full metadata example")
                            .icon(IconName::AiClaude)
                            .worktree(ThreadItemWorktreeInfo {
                                name: "my-project".into(),
                                full_path: "my-project".into(),
                                highlight_positions: Vec::new(),
                            })
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Branch + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5c", "Full metadata with branch")
                            .icon(IconName::AiClaude)
                            .worktree(ThreadItemWorktreeInfo {
                                name: "my-project".into(),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                            })
                            .branch_name("feature-branch")
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Branch + Changes (truncation)",
                container()
                    .child(
                        ThreadItem::new("ti-5d", "Metadata overflow with long branch name")
                            .icon(IconName::AiClaude)
                            .worktree(ThreadItemWorktreeInfo {
                                name: "my-project".into(),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                            })
                            .branch_name("fix-very-long-branch-name-here")
                            .added(108)
                            .removed(53)
                            .timestamp("2d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Branch Only + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5e", "Branch with diff stats, no worktree")
                            .icon(IconName::ZedAgent)
                            .branch_name("sidebar-show-branch-name")
                            .added(23)
                            .removed(8)
                            .timestamp("5m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Name + Worktree + Branch",
                container()
                    .child(
                        ThreadItem::new("ti-5f", "Multi-workspace thread with project")
                            .icon(IconName::AiClaude)
                            .project_name("my-remote-server")
                            .worktree(ThreadItemWorktreeInfo {
                                name: "jade-glen".into(),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                            })
                            .branch_name("feature-branch")
                            .timestamp("1d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Name (standalone)",
                container()
                    .child(
                        ThreadItem::new("ti-5g", "Archived thread with project context")
                            .icon(IconName::AiClaude)
                            .project_name("zed, docs")
                            .timestamp("2mo"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Name + Worktree + Branch + Changes",
                container()
                    .child(
                        ThreadItem::new("ti-5h", "Full metadata with all fields")
                            .icon(IconName::ZedAgent)
                            .project_name("remote-dev")
                            .worktree(ThreadItemWorktreeInfo {
                                name: "my-worktree".into(),
                                full_path: "/worktrees/my-worktree/zed".into(),
                                highlight_positions: Vec::new(),
                            })
                            .branch_name("main")
                            .added(15)
                            .removed(4)
                            .timestamp("8h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Selected Item",
                container()
                    .child(
                        ThreadItem::new("ti-6", "Refine textarea interaction behavior")
                            .icon(IconName::AiGemini)
                            .timestamp("45m")
                            .selected(true),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Focused Item (Keyboard Selection)",
                container()
                    .child(
                        ThreadItem::new("ti-7", "Implement keyboard navigation")
                            .icon(IconName::AiClaude)
                            .timestamp("12h")
                            .focused(true),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Hovered with Action Slot",
                container()
                    .child(
                        ThreadItem::new("ti-9", "Hover to see action button")
                            .icon(IconName::AiClaude)
                            .timestamp("6h")
                            .hovered(true)
                            .action_slot(
                                IconButton::new("delete", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted),
                            ),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Search Highlight",
                container()
                    .child(
                        ThreadItem::new("ti-10", "Implement keyboard navigation")
                            .icon(IconName::AiClaude)
                            .timestamp("4w")
                            .highlight_positions(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
                    )
                    .into_any_element(),
            ),
        ];

        Some(
            example_group(thread_item_examples)
                .vertical()
                .into_any_element(),
        )
    }
}

use crate::{AgentTool, GrokPlanItem, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema as acp;
use gpui::{App, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum PlanEntryStatus {
    /// The task has not started yet.
    Pending,
    /// The task is currently being worked on.
    InProgress,
    /// The task has been successfully completed.
    Completed,
}

impl From<PlanEntryStatus> for acp::PlanEntryStatus {
    fn from(value: PlanEntryStatus) -> Self {
        match value {
            PlanEntryStatus::Pending => acp::PlanEntryStatus::Pending,
            PlanEntryStatus::InProgress => acp::PlanEntryStatus::InProgress,
            PlanEntryStatus::Completed => acp::PlanEntryStatus::Completed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PlanItem {
    /// Human-readable description of what this task aims to accomplish.
    #[serde(alias = "content")]
    pub step: String,
    /// The current status of this task.
    pub status: PlanEntryStatus,
    /// Optional active form (Grok P4-0 observed shape for todo_write/enter_plan_mode fidelity; ignored for acp PlanEntry).
    #[serde(default, alias = "active_form", skip_serializing)]
    pub active_form: Option<String>,
}

impl From<PlanItem> for acp::PlanEntry {
    fn from(value: PlanItem) -> Self {
        acp::PlanEntry::new(
            value.step,
            acp::PlanEntryPriority::Medium,
            value.status.into(),
        )
    }
}

impl From<GrokPlanItem> for PlanItem {
    fn from(g: GrokPlanItem) -> Self {
        PlanItem {
            step: g.content,
            status: g.status,
            active_form: g.active_form,
        }
    }
}

/// Updates the task plan.
///
/// Provide a list of plan entries, each with a step and status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct UpdatePlanToolInput {
    /// The list of plan entries and their current statuses.
    pub plan: Vec<PlanItem>,
}

pub struct UpdatePlanTool;

impl UpdatePlanTool {
    pub(crate) fn to_plan(input: UpdatePlanToolInput) -> acp::Plan {
        acp::Plan::new(input.plan.into_iter().map(Into::into).collect())
    }

    /// Helper for enter_plan_mode shim (Grok fidelity).
    /// Produces a "proposed" plan (all pending) so ZT-1 can detect it via `Plan::is_proposed`.
    pub(crate) fn enter_plan_proposed(items: Vec<PlanItem>) -> acp::Plan {
        // Current implementation simply wraps the items. The "proposed" detection
        // happens via the heuristic in `Plan::is_proposed` (no completed / in_progress entries).
        // See AGENTS.md G-18 and the plan approval work for future refinements.
        acp::Plan::new(items.into_iter().map(Into::into).collect())
    }
}

impl AgentTool for UpdatePlanTool {
    type Input = UpdatePlanToolInput;
    type Output = String;

    const NAME: &'static str = "update_plan";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) if input.plan.is_empty() => "Clear plan".into(),
            Ok(_) | Err(_) => "Update plan".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |_cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;

            event_stream.update_plan(Self::to_plan(input));

            Ok("Plan updated".to_string())
        })
    }

    fn replay(
        &self,
        input: Self::Input,
        _output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> anyhow::Result<()> {
        event_stream.update_plan(Self::to_plan(input));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCallEventStream;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    fn sample_input() -> UpdatePlanToolInput {
        UpdatePlanToolInput {
            plan: vec![
                PlanItem {
                    step: "Inspect the existing tool wiring".to_string(),
                    status: PlanEntryStatus::Completed,
                },
                PlanItem {
                    step: "Implement the update_plan tool".to_string(),
                    status: PlanEntryStatus::InProgress,
                },
                PlanItem {
                    step: "Add tests".to_string(),
                    status: PlanEntryStatus::Pending,
                },
            ],
        }
    }

    #[gpui::test]
    async fn test_run_emits_plan_event(cx: &mut TestAppContext) {
        let tool = Arc::new(UpdatePlanTool);
        let (event_stream, mut event_rx) = ToolCallEventStream::test();

        let input = sample_input();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input.clone()), event_stream, cx))
            .await
            .expect("tool should succeed");

        assert_eq!(result, "Plan updated".to_string());

        let plan = event_rx.expect_plan().await;
        assert_eq!(
            plan,
            acp::Plan::new(vec![
                acp::PlanEntry::new(
                    "Inspect the existing tool wiring",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::Completed,
                ),
                acp::PlanEntry::new(
                    "Implement the update_plan tool",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::InProgress,
                ),
                acp::PlanEntry::new(
                    "Add tests",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::Pending,
                ),
            ])
        );
    }

    #[gpui::test]
    async fn test_replay_emits_plan_event(cx: &mut TestAppContext) {
        let tool = UpdatePlanTool;
        let (event_stream, mut event_rx) = ToolCallEventStream::test();

        let input = sample_input();

        cx.update(|cx| {
            tool.replay(input.clone(), "Plan updated".to_string(), event_stream, cx)
                .expect("replay should succeed");
        });

        let plan = event_rx.expect_plan().await;
        assert_eq!(
            plan,
            acp::Plan::new(vec![
                acp::PlanEntry::new(
                    "Inspect the existing tool wiring",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::Completed,
                ),
                acp::PlanEntry::new(
                    "Implement the update_plan tool",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::InProgress,
                ),
                acp::PlanEntry::new(
                    "Add tests",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::Pending,
                ),
            ])
        );
    }

    #[gpui::test]
    async fn test_initial_title(cx: &mut TestAppContext) {
        let tool = UpdatePlanTool;

        let title = cx.update(|cx| tool.initial_title(Ok(sample_input()), cx));
        assert_eq!(title, SharedString::from("Update plan"));

        let title =
            cx.update(|cx| tool.initial_title(Ok(UpdatePlanToolInput { plan: Vec::new() }), cx));
        assert_eq!(title, SharedString::from("Clear plan"));
    }
}

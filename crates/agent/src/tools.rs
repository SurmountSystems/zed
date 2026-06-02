mod apply_code_action_tool;
mod context_server_registry;
mod copy_path_tool;
mod create_directory_tool;
mod create_thread_tool;
mod delete_path_tool;
mod diagnostics_tool;
mod edit_file_tool;
mod edit_session;
#[cfg(test)]
mod evals;
mod fetch_tool;
mod find_path_tool;
mod find_references_tool;
mod get_code_actions_tool;
mod go_to_definition_tool;
mod grep_tool;
mod list_agents_and_models_tool;
mod list_directory_tool;
mod move_path_tool;
mod read_file_tool;
mod rename_tool;
mod skill_tool;
mod spawn_agent_tool;
mod symbol_locator;
mod terminal_tool;
mod tool_permissions;
mod update_plan_tool;
mod update_title_tool;
mod web_search_tool;
mod write_file_tool;

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema as acp;
use gpui::{App, Entity, SharedString, Task};
use language_model::{LanguageModelRequestTool, LanguageModelToolSchemaFormat};
use project::Project;
use schemars::JsonSchema;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, Error as _},
};
use std::rc::Rc;
use std::sync::Arc;

/// Deserialize a value that may have been provided as a JSON-encoded string
/// instead of the structured value. Some models occasionally stringify nested
/// arguments, so we accept either form.
pub(crate) fn deserialize_maybe_stringified<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: DeserializeOwned,
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ValueOrJsonString<T> {
        Value(T),
        String(String),
    }

    match ValueOrJsonString::<T>::deserialize(deserializer)? {
        ValueOrJsonString::Value(value) => Ok(value),
        ValueOrJsonString::String(string) => serde_json::from_str::<T>(&string).map_err(|error| {
            D::Error::custom(format!("failed to parse stringified value: {error}"))
        }),
    }
}

pub use apply_code_action_tool::*;
pub use context_server_registry::*;
pub use copy_path_tool::*;
pub use create_directory_tool::*;
pub use create_thread_tool::*;
pub use delete_path_tool::*;
pub use diagnostics_tool::*;
pub use edit_file_tool::*;
pub use fetch_tool::*;
pub use find_path_tool::*;
pub use find_references_tool::*;
pub use get_code_actions_tool::*;
pub use go_to_definition_tool::*;
pub use grep_tool::*;
pub use list_agents_and_models_tool::*;
pub use list_directory_tool::*;
pub use move_path_tool::*;
pub use read_file_tool::*;
pub use rename_tool::*;
pub use skill_tool::*;
pub use spawn_agent_tool::*;
pub use symbol_locator::*;
pub use terminal_tool::*;
pub use tool_permissions::*;
pub use update_plan_tool::*;
pub use update_title_tool::*;
pub use web_search_tool::*;
pub use write_file_tool::*;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct GrokPlanItem {
    /// Human-readable description of the task (P4-0 observed shape for todo_write and enter_plan_mode inputs uses "content").
    pub content: String,
    /// Stable short slug for cross-turn task references (TurnId-based task-ids from prior work, e.g. "task-17-..." style) in Grok Build sessions.
    pub id: String,
    /// Current status.
    pub status: PlanEntryStatus,
    /// Optional active form for display during progress (observed in some plan/todo samples).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

impl From<GrokPlanItem> for acp::PlanEntry {
    fn from(value: GrokPlanItem) -> Self {
        acp::PlanEntry::new(
            value.content,
            acp::PlanEntryPriority::Medium,
            value.status.into(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TodoWriteInput {
    /// List of todos (matches P4-0 captured todo_write input shape with "todos" containing content/status/active_form items using TurnId task-ids in the "id" fields per prior work).
    pub todos: Vec<GrokPlanItem>,
}

pub struct TodoWriteTool;

impl AgentTool for TodoWriteTool {
    type Input = TodoWriteInput;
    type Output = String;

    const NAME: &'static str = "todo_write";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(parsed_input) if parsed_input.todos.is_empty() => "Clear todos".into(),
            _ => "Write todos".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |_cx| {
            let parsed = input.recv().await.map_err(|e| e.to_string())?;
            let plan = acp::Plan::new(parsed.todos.into_iter().map(Into::into).collect());
            event_stream.update_plan(plan);
            Ok("Todos written".to_string())
        })
    }
}

pub struct MonitorTool {
    project: Entity<Project>,
    environment: Rc<dyn ThreadEnvironment>,
}

impl MonitorTool {
    pub fn new(project: Entity<Project>, environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self {
            project,
            environment,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct MonitorInput {
    /// The command to execute for background monitoring (matches P4-0 observed monitor tool input shape for native dispatch fidelity).
    pub command: String,
    /// Working directory to use for the command. Primary key "cd" (to match terminal tool); "cwd" alias supports current working directory label variants observed in P4-0 tool calls for exact deserialization and schema roundtrip fidelity.
    #[serde(alias = "cwd")]
    pub cd: String,
    /// Optional timeout in milliseconds for the monitor command.
    pub timeout_ms: Option<u64>,
    /// Optional human readable description of the monitor task.
    #[serde(default)]
    pub description: Option<String>,
}

impl AgentTool for MonitorTool {
    type Input = MonitorInput;
    type Output = String;

    const NAME: &'static str = "monitor";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(parsed_input) = input {
            parsed_input.command.into()
        } else {
            "monitor".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let environment = self.environment.clone();
        cx.spawn(async move |cx| {
            let input_val = input.recv().await.map_err(|e| e.to_string())?;

            let (working_dir, authorize) = cx.update(|cx| {
                let working_dir = terminal_tool::working_dir(
                    &TerminalToolInput {
                        command: input_val.command.clone(),
                        cd: input_val.cd.clone(),
                        timeout_ms: input_val.timeout_ms,
                        head_lines: None,
                        tail_lines: None,
                        allow_network: None,
                        allow_fs_write: None,
                        unsandboxed: None,
                    },
                    &project,
                    cx,
                )
                .map_err(|err| err.to_string())?;

                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input_val.command.clone()]);

                let title: SharedString = input_val
                    .description
                    .clone()
                    .unwrap_or_else(|| input_val.command.clone())
                    .into();

                let authorize = event_stream.authorize(title, context, cx);

                Ok::<_, String>((working_dir, authorize))
            })?;

            authorize.await.map_err(|e| e.to_string())?;

            let terminal = environment
                .create_terminal(
                    input_val.command.clone(),
                    vec![],
                    working_dir,
                    Some(16 * 1024),
                    None,
                    cx,
                )
                .await
                .map_err(|e| e.to_string())?;

            let terminal_id = terminal.id(cx).map_err(|e| e.to_string())?;
            event_stream.update_fields(acp::ToolCallUpdateFields::new().content(vec![
                acp::ToolCallContent::Terminal(acp::Terminal::new(terminal_id)),
            ]));

            let retained = terminal.clone();
            cx.spawn(async move |_cx| {
                let _keep_alive = retained;
                futures::future::pending::<()>().await;
            })
            .detach();

            Ok("Background monitor started".to_string())
        })
    }
}

pub struct EnterPlanModeTool;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct EnterPlanModeInput {
    /// Plan entries for enter_plan_mode (uses content/status shape for P4-0 fidelity match on Grok's plan tool inputs).
    pub plan: Vec<GrokPlanItem>,
    /// Optional explanation for entering plan mode (supports P4-0 observed optional field shape in enter_plan_mode inputs).
    #[serde(default)]
    pub explanation: Option<String>,
}

impl AgentTool for EnterPlanModeTool {
    type Input = EnterPlanModeInput;
    type Output = String;

    const NAME: &'static str = "enter_plan_mode";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Enter plan mode".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |_cx| {
            let parsed = input.recv().await.map_err(|e| e.to_string())?;
            let plan = UpdatePlanTool::enter_plan_proposed(
                parsed.plan.into_iter().map(Into::into).collect(),
            );
            event_stream.update_plan(plan);
            Ok("Plan mode entered".to_string())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetCommandOrSubagentOutputInput {
    pub task_id: String,
    #[serde(default)]
    pub block: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct GetCommandOrSubagentOutputTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl GetCommandOrSubagentOutputTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for GetCommandOrSubagentOutputTool {
    type Input = GetCommandOrSubagentOutputInput;
    type Output = String;

    const NAME: &'static str = "get_command_or_subagent_output";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(parsed) = input {
            format!("Get output {}", parsed.task_id).into()
        } else {
            "Get command or subagent output".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let environment = self.environment.clone();
        cx.spawn(async move |cx| {
            let parsed = input.recv().await.map_err(|e| e.to_string())?;
            let title: SharedString = format!("Retrieve output for {}", parsed.task_id).into();
            let context =
                crate::ToolPermissionContext::new(Self::NAME, vec![parsed.task_id.clone()]);
            let authorize = cx.update(|cx| {
                let authorize = event_stream.authorize(title, context, cx);
                Ok::<_, String>(authorize)
            })?;
            authorize.await.map_err(|e| e.to_string())?;
            let output = environment
                .get_command_or_subagent_output(parsed.task_id, parsed.block, parsed.timeout_ms, cx)
                .await
                .map_err(|e| e.to_string())?;
            Ok(output)
        })
    }
}

macro_rules! tools {
    ($($tool:ty),* $(,)?) => {
        /// Every built-in tool name, determined at compile time.
        pub const ALL_TOOL_NAMES: &[&str] = &[
            $(<$tool>::NAME,)*
        ];

        const _: () = {
            const fn str_eq(a: &str, b: &str) -> bool {
                let a = a.as_bytes();
                let b = b.as_bytes();
                if a.len() != b.len() {
                    return false;
                }
                let mut i = 0;
                while i < a.len() {
                    if a[i] != b[i] {
                        return false;
                    }
                    i += 1;
                }
                true
            }

            const NAMES: &[&str] = ALL_TOOL_NAMES;
            let mut i = 0;
            while i < NAMES.len() {
                let mut j = i + 1;
                while j < NAMES.len() {
                    if str_eq(NAMES[i], NAMES[j]) {
                        panic!("Duplicate tool name in tools! macro");
                    }
                    j += 1;
                }
                i += 1;
            }
        };

        /// Returns whether the tool with the given name supports the given provider.
        pub fn tool_supports_provider(name: &str, provider: &language_model::LanguageModelProviderId) -> bool {
            $(
                if name == <$tool>::NAME {
                    return <$tool>::supports_provider(provider);
                }
            )*
            false
        }

        /// A list of all built-in tools
        pub fn built_in_tools() -> impl Iterator<Item = LanguageModelRequestTool> {
            fn language_model_tool<T: AgentTool>() -> LanguageModelRequestTool {
                LanguageModelRequestTool {
                    name: T::NAME.to_string(),
                    description: T::description().to_string(),
                    input_schema: T::input_schema(LanguageModelToolSchemaFormat::JsonSchema).to_value(),
                    use_input_streaming: T::supports_input_streaming(),
                }
            }
            [
                $(
                    language_model_tool::<$tool>(),
                )*
            ]
            .into_iter()
        }
    };
}

// Adding a tool here (and constructing it in `Thread::add_default_tools`) is
// not enough to make the model actually receive it. Two further gates will
// silently drop the tool rather than fail to compile:
//
// 1. `assets/settings/default.json`: the `write` and `ask` agent profiles each
//    carry an explicit `tools` allowlist. `Thread::enabled_tools` filters out
//    any tool not present there with value `true`, so it never reaches the
//    model.
// 2. `test_all_tools_are_in_tool_info_or_excluded` in
//    `crates/settings_ui/src/pages/tool_permissions_setup.rs`: every tool must
//    be in the permission-UI `TOOLS` list (if it calls
//    `decide_permission_from_settings`) or in `EXCLUDED_TOOLS`.
tools! {
    ApplyCodeActionTool,
    CopyPathTool,
    CreateDirectoryTool,
    CreateThreadTool,
    DeletePathTool,
    DiagnosticsTool,
    EditFileTool,
    EnterPlanModeTool,
    FetchTool,
    FindPathTool,
    FindReferencesTool,
    GetCodeActionsTool,
    GoToDefinitionTool,
    GrepTool,
    ListAgentsAndModelsTool,
    ListDirectoryTool,
    MonitorTool,
    GetCommandOrSubagentOutputTool,
    MovePathTool,
    ReadFileTool,
    RenameTool,
    SkillTool,
    SpawnAgentTool,
    TerminalTool,
    TodoWriteTool,
    UpdatePlanTool,
    UpdateTitleTool,
    WebSearchTool,
    WriteFileTool,
}

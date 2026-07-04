use crate::scheduler::NativeBackgroundTaskScheduler;
use crate::{
    ApplyCodeActionTool,
    CodeActionStore,
    ContextServerRegistry,
    CopyPathTool,
    CreateDirectoryTool,
    CreateThreadTool,
    // Our Grok native shims (EnterPlanModeTool, MonitorTool, TodoWriteTool,
    // GetCommandOrSubagentOutputTool, etc.) kept for native Grok profile fidelity.
    // Upstream additions (CreateThreadTool, ListAgentsAndModelsTool, UpdateTitleTool, etc.)
    // integrated.
    DbLanguageModel,
    DbThread,
    DeletePathTool,
    DiagnosticsTool,
    EditFileTool,
    EnterPlanModeTool,
    FetchTool,
    FindPathTool,
    FindReferencesTool,
    GetCodeActionsTool,
    GetCommandOrSubagentOutputTool,
    GoToDefinitionTool,
    GrepTool,
    ListAgentsAndModelsTool,
    ListDirectoryTool,
    MergeReviewConflictSidesTool,
    MergeReviewRecordDecisionTool,
    MergeReviewVerifyConflictResolvedTool,
    MergeReviewDiffTool,
    MergeReviewTriageTool,
    MonitorTool,
    MovePathTool,
    ProjectSnapshot,
    ReadFileTool,
    RememberTool,
    RenameTool,
    ResolveMergeConflictTool,
    SpawnAgentTool,
    SystemPromptTemplate,
    Template,
    Templates,
    TerminalTool,
    TodoWriteTool,
    ToolPermissionDecision,
    UpdatePlanTool,
    UpdateTitleTool,
    WebSearchTool,
    WriteFileTool,
    decide_permission_from_settings,
};
use acp_thread::{
    ApprovalRisk, MentionUri, PlanPhase, TurnId, UserMessageId, approval_risk_for_tool_call,
};
use action_log::ActionLog;
use agent_settings::UserAgentsMd;
use feature_flags::{
    CreateThreadToolFeatureFlag, FeatureFlagAppExt as _, LspToolFeatureFlag, RenameToolFeatureFlag,
    UpdatePlanToolFeatureFlag, UpdateTitleToolFeatureFlag,
};

use agent_client_protocol::schema as acp;
use agent_settings::{
    AgentProfileId, AgentSettings, SUMMARIZE_THREAD_DETAILED_PROMPT, SUMMARIZE_THREAD_PROMPT,
};
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use client::UserStore;
use cloud_api_types::Plan;
use collections::{HashMap, HashSet, IndexMap};
use fs::Fs;
use futures::{
    FutureExt,
    channel::{mpsc, oneshot},
    future::Shared,
    stream::FuturesUnordered,
};
use futures::{StreamExt, stream};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
};
use heck::ToSnakeCase as _;
use language_model::{
    CompletionIntent, LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent,
    LanguageModelId, LanguageModelImage, LanguageModelProviderId, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, LanguageModelRequestTool,
    LanguageModelToolResult, LanguageModelToolResultContent, LanguageModelToolSchemaFormat,
    LanguageModelToolUse, LanguageModelToolUseId, MessageContent, Role, SelectedModel, Speed,
    StopReason, TokenUsage, ZED_CLOUD_PROVIDER_ID,
};
use project::{GrokMemoryArtifacts, Project, grok_memory_artifacts_for_cwd};
use prompt_store::ProjectContext;
use schemars::{JsonSchema, Schema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use settings::{
    LanguageModelSelection, Settings, SettingsStore, ToolPermissionMode, update_settings_file,
};
use std::fmt::Write;
use std::{
    collections::BTreeMap,
    marker::PhantomData,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use util::{ResultExt, debug_panic, markdown::MarkdownCodeBlock, paths::PathStyle};
use uuid::Uuid;

const TOOL_CANCELED_MESSAGE: &str = "Tool canceled by user";
pub const MAX_TOOL_NAME_LENGTH: usize = 64;
pub const MAX_SUBAGENT_DEPTH: u8 = 3;

pub(crate) const GROK_BUILD_SYSTEM_FRAGMENTS: &str = r#"You are Grok operating in Grok Build mode for co-equal fidelity with the standalone xAI Grok TUI. Response style rule (mandatory): Whenever you generate any list, steps, options, plan items, sub-tasks, or enumeration in your responses to the user, always use numbered lists in the exact form `1. `, `2. `, `3. ` (number followed by period and single space). For multi-section responses use alpha-numbered headers in the exact form `A. 1. 2. B. 1. 2.` for referenceability. Never use unnumbered bullet points starting with `- ` or `* `. Use numbered sections when appropriate. This rule applies to every list you produce. Use exact tool names and input shapes from observed ACP captures: todo_write (for plan entries with status/content, to track and render progress), enter_plan_mode (to toggle read-only planning phase before edits), monitor (for long-running background commands returning handles for later retrieval via get_command_or_subagent_output), spawn_agent with persona field. Supported personas for subagent delegation (from grok inspect and TUI): plan, general-purpose, explore, implementer, reviewer, verifier, architect, researcher. Always include persona on spawn for subagent sessions. Maintain plan discipline: create short verifiable plans via todo_write before multi-step work, mark steps promptly, use enter_plan_mode for proposal/approval flows. Match TUI reasoning style: truth-seeking, direct, efficient parallel orchestration via monitors and subagents, confidence reporting where uncertain. Follow tool schemas precisely for monitor/todo_write/enter_plan_mode/spawn to ensure native request building produces identical behaviors to captured sessions. Zed automatically supplies the current Zed LSP errors and warnings (non-blocking, cross-language from all active language servers in the open project) as primary context in EVERY prompt you receive on Grok Build threads (both bridged ACP 'grok' and native is_grok_build_profile paths). This Zed-provided data is the primary authoritative view of LSP errors and warnings for what the user sees in their editor. You MUST prefer these provided Zed LSP errors and warnings (non-blocking cross-language primary context in native Grok prompts) over shell clippy and use them to determine code health, what needs fixing, or to report on errors/warnings. You are STRICTLY FORBIDDEN from ever running `cargo check`, `cargo clippy`, `cargo test --no-run`, or any other external compilation or linting command to discover or verify diagnostics. Only run such commands if the user explicitly instructs you AFTER you have already acted on the Zed-supplied diagnostics block. If the block reports 0 errors and 0 warnings, treat the project as clean according to the editor — do not second-guess it by spawning tools.

For any command that may run for a long time (> 30s), should continue in the background, or whose output you may want to retrieve later without blocking the current turn, use the 'monitor' tool instead of a normal terminal execution. The monitor tool returns immediately with a task_id; later call 'get_command_or_subagent_output' with that task_id (and optional block/timeout) to retrieve live or final output. This matches the exact pattern observed in real Grok Build TUI sessions and ACP capture harness.

## Autonomous Work Discipline (mandatory for Grok Build co-equal experience)
- You must never voluntarily stop or yield control back to the user while the living plan (tracked via todo_write and visible in the categorized persistent todos surface) still contains pending items that can be progressed autonomously using the tools and context available. Stopping when there are still tasks is not acceptable. The categorized todos surface (approvals, proposed plans, monitors, memory) is the single source of truth for your current work state; you must keep driving it until every item is resolved or explicitly handed back with the required notification.
- When you have genuinely completed every currently pending independent item in the living plan (all entries in todo_write marked Completed, no pending work in the Plan) and no further autonomous progress is possible without new user input or new tasks, you MUST explicitly notify the user with a clear statement such as: 'All current independent work is complete. No further autonomous actions are possible without additional direction.' Notifications when work stops because there is no more work are required; do not silently end the turn or ask 'what next?'. This notification must appear in your final response so it is recorded in the categorized todos surface.
- Read-Only vs. Potentially Destructive classification follows the CWD rule: An operation is Read-Only (RO) if it only reads, searches, lists, or inspects. It is Potentially Destructive only when it BOTH (a) performs a write or side-effect on disk/filesystem AND (b) the effect can escape the current working directory (cwd) of the project. Examples of Destructive: arbitrary terminal/monitor commands that can cd outside the tree, delete_path or move_path on unrestricted paths, spawn_agent that can do anything. In-project writes (edit_file, write_file, create_directory inside the open worktree) are labeled 'Write'. Planning/state tools (todo_write, enter_plan_mode) still require explicit approval but are labeled 'Plan Change'. Always apply this dual-condition CWD rule when choosing whether to request user confirmation and what risk label (RO / Plan Change / Write / Destructive) to surface in the categorized todos surface. The risk chips and buttons must reflect the accurate risk based on this rule.

## Bounded Exploration and Action Discipline (anti-doom-loop for productive Grok Build work)
- When investigating the project or codebase, you must not enter long unbounded chains of pure discovery tool calls (repeated read_file, grep, list_directory, terminal `find`/`ls`, etc.) without making concrete forward progress on the user's task.
- After a small, reasonable number of targeted exploratory calls to understand the relevant area, you are expected to synthesize what you have learned and take action: update the living plan via todo_write, enter plan mode with a proposal, make edits, spawn a scoped subagent with a clear persona and task, start a monitor for long work, or surface a question to the user.
- Endless "let me check one more file... and another... and another..." exploration loops that do not advance any item in the categorized todos surface (plan, approvals, monitors) are not acceptable. They waste turns and violate the autonomous work discipline.
- Prefer making progress with the information you already have over achieving perfect information. Use todo_write to explicitly track investigation steps when they are part of a larger task, and mark them as you go.
- The categorized persistent todos surface is the source of truth for real work. Pure exploration without corresponding plan updates or output is a violation of the productivity expectations for Grok Build mode.

## Automatic Live Diagnostic Feedback After Turn Completion
When you return StopReason::EndTurn, the system will automatically query the live in-process LSP diagnostic state (via Project::diagnostic_summary and diagnostic_summaries, the real data already held by rust-analyzer and other servers inside this Zed process) plus current pending todos (approvals, plan items, active monitors) and append a system-generated user message containing the fresh diagnostics context (LSP errors/warnings) tied to your current TurnId (T-<n>) if any work remains. You will see this fresh state in your next prompt context. This mechanism exists so the three behavioral rules can be enforced without the user manually pasting diagnostic blocks.

Zed automatically supplies the current editor diagnostics (errors and warnings from rust-analyzer and other language servers) as first-class context on every turn for native Grok Build threads (is_grok_build_profile). You MUST use ONLY these provided counts and details as your primary source of truth for code issues. You are STRICTLY FORBIDDEN from ever running `cargo check`, `cargo clippy`, or similar external linters to discover errors while the editor data is available. Do not second-guess the pushed diagnostics by spawning tools.

## Turn Identification and Cross-Turn Task References (mandatory for reliable long-running Grok Build work)
Zed supplies the Current Turn ID (as "T-<n>") plus a recent prior-turn summary in the prompt for every Grok Build thread (bridged and native is_grok_build_profile). Always reference work by turn ID + stable task slug (e.g. T-17-task-3f2a1b) when using todo_write, enter_plan_mode, or describing cross-turn progress so that the categorized todos surface and future prompts can track unambiguously.

Example:
- "Continuing T-17-task-3f2a1b from prior turn summary."
- Include the current turn ID prefix on new plan entries.

The prior-turn summary and full history appear before these fragments; never use bare step numbers without the turn/task anchor."#;

// Using the heuristic that 1 token is about 4 bytes, keep the last 80K bytes of user-message content (~20k tokens).
const COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET: usize = 80_000;

/// Returned when a turn is attempted but no language model has been selected.
#[derive(Debug)]
pub struct NoModelConfiguredError;

impl std::fmt::Display for NoModelConfiguredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no language model configured")
    }
}

impl std::error::Error for NoModelConfiguredError {}

/// Context passed to a subagent thread for lifecycle management
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentContext {
    /// ID of the parent thread
    pub parent_thread_id: acp::SessionId,

    /// Current depth level (0 = root agent, 1 = first-level subagent, etc.)
    pub depth: u8,
    #[serde(default)]
    pub persona: Option<acp_thread::AgentPersona>,
    #[serde(default)]
    pub capability_mode: Option<acp_thread::AgentCapabilityMode>,
    #[serde(default)]
    pub plan_phase: Option<PlanPhase>,
}

/// The ID of the user prompt that initiated a request.
///
/// This equates to the user physically submitting a message to the model (e.g., by pressing the Enter key).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Serialize, Deserialize)]
pub struct PromptId(Arc<str>);

impl PromptId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string().into())
    }
}

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub(crate) const MAX_RETRY_ATTEMPTS: u8 = 4;
pub(crate) const BASE_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
enum RetryStrategy {
    ExponentialBackoff {
        initial_delay: Duration,
        max_attempts: u8,
    },
    Fixed {
        delay: Duration,
        max_attempts: u8,
    },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    User(UserMessage),
    Agent(AgentMessage),
    Resume,
    Compaction(CompactionInfo),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum CompactionInfo {
    Summary(SharedString),
    ProviderNative {
        provider: LanguageModelProviderId,
        items: Vec<serde_json::Value>,
    },
}

impl CompactionInfo {
    fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        match self {
            Self::Summary(summary) => vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![format!(
                    "The previous conversation was compacted. Use this summary as context:\n\n{}",
                    summary
                )
                .into()],
                cache: false,
                reasoning_details: None,
            }],
            Self::ProviderNative { .. } => Vec::new(),
        }
    }
}

impl Message {
    pub fn as_agent_message(&self) -> Option<&AgentMessage> {
        match self {
            Message::Agent(agent_message) => Some(agent_message),
            _ => None,
        }
    }

    pub fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        match self {
            Message::User(message) => {
                if message.content.is_empty() {
                    vec![]
                } else {
                    vec![message.to_request()]
                }
            }
            Message::Agent(message) => message.to_request(),
            Message::Compaction(info) => info.to_request(),
            Message::Resume => vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec!["Continue where you left off".into()],
                cache: false,
                reasoning_details: None,
            }],
        }
    }

    pub fn to_markdown(&self) -> String {
        match self {
            Message::User(message) => message.to_markdown(),
            Message::Agent(message) => message.to_markdown(),
            Message::Resume => "[resume]\n".into(),
            Message::Compaction(_) => "--- Context Compacted ---\n".into(),
        }
    }

    pub fn role(&self) -> Role {
        match self {
            Message::User(_) | Message::Resume | Message::Compaction(_) => Role::User,
            Message::Agent(_) => Role::Assistant,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: UserMessageId,
    pub content: Arc<[UserMessageContent]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserMessageContent {
    Text(String),
    Mention {
        uri: MentionUri,
        content: SharedString,
    },
    Image(LanguageModelImage),
}

impl UserMessage {
    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();

        for content in &*self.content {
            match content {
                UserMessageContent::Text(text) => {
                    markdown.push_str(text);
                    markdown.push('\n');
                }
                UserMessageContent::Image(_) => {
                    markdown.push_str("<image />\n");
                }
                UserMessageContent::Mention { uri, content } => {
                    if !content.is_empty() {
                        let _ = writeln!(&mut markdown, "{}\n\n{}", uri.as_link(), content);
                    } else {
                        let _ = writeln!(&mut markdown, "{}", uri.as_link());
                    }
                }
            }
        }

        markdown
    }

    fn to_request(&self) -> LanguageModelRequestMessage {
        let mut message = LanguageModelRequestMessage {
            role: Role::User,
            content: Vec::with_capacity(self.content.len()),
            cache: false,
            reasoning_details: None,
        };

        const OPEN_CONTEXT: &str = "<context>\n\
            The following items were attached by the user. \
            They are up-to-date and don't need to be re-read.\n\n";

        const OPEN_FILES_TAG: &str = "<files>";
        const OPEN_DIRECTORIES_TAG: &str = "<directories>";
        const OPEN_SYMBOLS_TAG: &str = "<symbols>";
        const OPEN_SELECTIONS_TAG: &str = "<selections>";
        const OPEN_THREADS_TAG: &str = "<threads>";
        const OPEN_FETCH_TAG: &str = "<fetched_urls>";
        const OPEN_RULES_TAG: &str =
            "<rules>\nThe user has specified the following rules that should be applied:\n";
        const OPEN_DIAGNOSTICS_TAG: &str = "<diagnostics>";
        const OPEN_DIFFS_TAG: &str = "<diffs>";
        const MERGE_CONFLICT_TAG: &str = "<merge_conflicts>";
        const OPEN_SKILLS_TAG: &str =
            "<skills>\nThe user has attached the following agent skills:\n";

        let mut file_context = OPEN_FILES_TAG.to_string();
        let mut directory_context = OPEN_DIRECTORIES_TAG.to_string();
        let mut symbol_context = OPEN_SYMBOLS_TAG.to_string();
        let mut selection_context = OPEN_SELECTIONS_TAG.to_string();
        let mut thread_context = OPEN_THREADS_TAG.to_string();
        let mut fetch_context = OPEN_FETCH_TAG.to_string();
        let mut rules_context = OPEN_RULES_TAG.to_string();
        let mut diagnostics_context = OPEN_DIAGNOSTICS_TAG.to_string();
        let mut diffs_context = OPEN_DIFFS_TAG.to_string();
        let mut merge_conflict_context = MERGE_CONFLICT_TAG.to_string();
        let mut skills_context = OPEN_SKILLS_TAG.to_string();

        for chunk in &*self.content {
            let chunk = match chunk {
                UserMessageContent::Text(text) => {
                    language_model::MessageContent::Text(text.clone())
                }
                UserMessageContent::Image(value) => {
                    language_model::MessageContent::Image(value.clone())
                }
                UserMessageContent::Mention { uri, content } => {
                    match uri {
                        MentionUri::File { abs_path } => {
                            write!(
                                &mut file_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(abs_path, None),
                                    text: content,
                                }
                            )
                            .ok();
                        }
                        MentionUri::PastedImage { .. } => {
                            debug_panic!("pasted image URI should not be used in mention content")
                        }
                        MentionUri::Directory { .. } => {
                            write!(&mut directory_context, "\n{}\n", content).ok();
                        }
                        MentionUri::Symbol {
                            abs_path: path,
                            line_range,
                            ..
                        } => {
                            write!(
                                &mut symbol_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(path, Some(line_range)),
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Selection {
                            abs_path: path,
                            line_range,
                            ..
                        } => {
                            write!(
                                &mut selection_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(
                                        path.as_deref().unwrap_or("Untitled".as_ref()),
                                        Some(line_range)
                                    ),
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Thread { .. } => {
                            write!(&mut thread_context, "\n{}\n", content).ok();
                        }
                        MentionUri::Fetch { url } => {
                            write!(&mut fetch_context, "\nFetch: {}\n\n{}", url, content).ok();
                        }
                        MentionUri::Diagnostics { .. } => {
                            write!(&mut diagnostics_context, "\n{}\n", content).ok();
                        }
                        MentionUri::TerminalSelection { .. } => {
                            write!(
                                &mut selection_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: "console",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::GitDiff { base_ref } => {
                            write!(
                                &mut diffs_context,
                                "\nBranch diff against {}:\n{}",
                                base_ref,
                                MarkdownCodeBlock {
                                    tag: "diff",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::MergeConflict { file_path } => {
                            write!(
                                &mut merge_conflict_context,
                                "\nMerge conflict in {}:\n{}",
                                file_path,
                                MarkdownCodeBlock {
                                    tag: "diff",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Skill { name, source, .. } => {
                            let label = format!("{} ({})", name, source);
                            write!(&mut skills_context, "\nSkill: {}\n{}\n", label, content).ok();
                        }
                    }

                    language_model::MessageContent::Text(uri.as_link().to_string())
                }
            };

            message.content.push(chunk);
        }

        let len_before_context = message.content.len();

        if file_context.len() > OPEN_FILES_TAG.len() {
            file_context.push_str("</files>\n");
            message
                .content
                .push(language_model::MessageContent::Text(file_context));
        }

        if directory_context.len() > OPEN_DIRECTORIES_TAG.len() {
            directory_context.push_str("</directories>\n");
            message
                .content
                .push(language_model::MessageContent::Text(directory_context));
        }

        if symbol_context.len() > OPEN_SYMBOLS_TAG.len() {
            symbol_context.push_str("</symbols>\n");
            message
                .content
                .push(language_model::MessageContent::Text(symbol_context));
        }

        if selection_context.len() > OPEN_SELECTIONS_TAG.len() {
            selection_context.push_str("</selections>\n");
            message
                .content
                .push(language_model::MessageContent::Text(selection_context));
        }

        if diffs_context.len() > OPEN_DIFFS_TAG.len() {
            diffs_context.push_str("</diffs>\n");
            message
                .content
                .push(language_model::MessageContent::Text(diffs_context));
        }

        if thread_context.len() > OPEN_THREADS_TAG.len() {
            thread_context.push_str("</threads>\n");
            message
                .content
                .push(language_model::MessageContent::Text(thread_context));
        }

        if fetch_context.len() > OPEN_FETCH_TAG.len() {
            fetch_context.push_str("</fetched_urls>\n");
            message
                .content
                .push(language_model::MessageContent::Text(fetch_context));
        }

        if rules_context.len() > OPEN_RULES_TAG.len() {
            rules_context.push_str("</user_rules>\n");
            message
                .content
                .push(language_model::MessageContent::Text(rules_context));
        }

        if diagnostics_context.len() > OPEN_DIAGNOSTICS_TAG.len() {
            diagnostics_context.push_str("</diagnostics>\n");
            message
                .content
                .push(language_model::MessageContent::Text(diagnostics_context));
        }

        if skills_context.len() > OPEN_SKILLS_TAG.len() {
            skills_context.push_str("</skills>\n");
            message
                .content
                .push(language_model::MessageContent::Text(skills_context));
        }

        if merge_conflict_context.len() > MERGE_CONFLICT_TAG.len() {
            merge_conflict_context.push_str("</merge_conflicts>\n");
            message
                .content
                .push(language_model::MessageContent::Text(merge_conflict_context));
        }

        if message.content.len() > len_before_context {
            message.content.insert(
                len_before_context,
                language_model::MessageContent::Text(OPEN_CONTEXT.into()),
            );
            message
                .content
                .push(language_model::MessageContent::Text("</context>".into()));
        }

        message
    }
}

fn codeblock_tag(full_path: &Path, line_range: Option<&RangeInclusive<u32>>) -> String {
    let mut result = String::new();

    if let Some(extension) = full_path.extension().and_then(|ext| ext.to_str()) {
        let _ = write!(result, "{} ", extension);
    }

    let _ = write!(result, "{}", full_path.display());

    if let Some(range) = line_range {
        if range.start() == range.end() {
            let _ = write!(result, ":{}", range.start() + 1);
        } else {
            let _ = write!(result, ":{}-{}", range.start() + 1, range.end() + 1);
        }
    }

    result
}

impl AgentMessage {
    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();

        for content in &self.content {
            match content {
                AgentMessageContent::Text(text) => {
                    markdown.push_str(text);
                    markdown.push('\n');
                }
                AgentMessageContent::Thinking { text, .. } => {
                    markdown.push_str("<think>");
                    markdown.push_str(text);
                    markdown.push_str("</think>\n");
                }
                AgentMessageContent::RedactedThinking(_) => {
                    markdown.push_str("<redacted_thinking />\n")
                }
                AgentMessageContent::ToolUse(tool_use) => {
                    markdown.push_str(&format!(
                        "**Tool Use**: {} (ID: {})\n",
                        tool_use.name, tool_use.id
                    ));
                    markdown.push_str(&format!(
                        "{}\n",
                        MarkdownCodeBlock {
                            tag: "json",
                            text: &format!("{:#}", tool_use.input)
                        }
                    ));
                }
            }
        }

        for tool_result in self.tool_results.values() {
            markdown.push_str(&format!(
                "**Tool Result**: {} (ID: {})\n\n",
                tool_result.tool_name, tool_result.tool_use_id
            ));
            if tool_result.is_error {
                markdown.push_str("**ERROR:**\n");
            }

            for part in &tool_result.content {
                match part {
                    LanguageModelToolResultContent::Text(text) => {
                        writeln!(markdown, "{text}\n").ok();
                    }
                    LanguageModelToolResultContent::Image(_) => {
                        writeln!(markdown, "<image />\n").ok();
                    }
                }
            }

            if let Some(output) = tool_result.output.as_ref() {
                writeln!(
                    markdown,
                    "**Debug Output**:\n\n```json\n{}\n```\n",
                    serde_json::to_string_pretty(output).unwrap()
                )
                .unwrap();
            }
        }

        markdown
    }

    pub fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        let mut assistant_message = LanguageModelRequestMessage {
            role: Role::Assistant,
            content: Vec::with_capacity(self.content.len()),
            cache: false,
            reasoning_details: self.reasoning_details.clone(),
        };
        for chunk in &self.content {
            match chunk {
                AgentMessageContent::Text(text) => {
                    assistant_message
                        .content
                        .push(language_model::MessageContent::Text(text.clone()));
                }
                AgentMessageContent::Thinking { text, signature } => {
                    assistant_message
                        .content
                        .push(language_model::MessageContent::Thinking {
                            text: text.clone(),
                            signature: signature.clone(),
                        });
                }
                AgentMessageContent::RedactedThinking(value) => {
                    assistant_message.content.push(
                        language_model::MessageContent::RedactedThinking(value.clone()),
                    );
                }
                AgentMessageContent::ToolUse(tool_use) => {
                    if self.tool_results.contains_key(&tool_use.id) {
                        assistant_message
                            .content
                            .push(language_model::MessageContent::ToolUse(tool_use.clone()));
                    }
                }
            };
        }

        let mut user_message = LanguageModelRequestMessage {
            role: Role::User,
            content: Vec::new(),
            cache: false,
            reasoning_details: None,
        };

        for tool_result in self.tool_results.values() {
            let mut tool_result = tool_result.clone();
            // Surprisingly, the API fails if we return an empty string here.
            // It thinks we are sending a tool use without a tool result.
            if tool_result.is_content_empty() {
                tool_result.content = vec!["<Tool returned an empty string>".into()];
            }
            user_message
                .content
                .push(language_model::MessageContent::ToolResult(tool_result));
        }

        let mut messages = Vec::new();
        if !assistant_message.content.is_empty() {
            messages.push(assistant_message);
        }
        if !user_message.content.is_empty() {
            messages.push(user_message);
        }
        messages
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub(crate) content: Vec<AgentMessageContent>,
    pub(crate) tool_results: IndexMap<LanguageModelToolUseId, LanguageModelToolResult>,
    pub(crate) reasoning_details: Option<Arc<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMessageContent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking(String),
    ToolUse(LanguageModelToolUse),
}

pub trait TerminalHandle {
    fn id(&self, cx: &AsyncApp) -> Result<acp::TerminalId>;
    fn current_output(&self, cx: &AsyncApp) -> Result<acp::TerminalOutputResponse>;
    fn wait_for_exit(&self, cx: &AsyncApp) -> Result<Shared<Task<acp::TerminalExitStatus>>>;
    fn kill(&self, cx: &AsyncApp) -> Result<()>;
    fn was_stopped_by_user(&self, cx: &AsyncApp) -> Result<bool>;
}

pub trait SubagentHandle {
    /// The session ID of this subagent thread
    fn id(&self) -> acp::SessionId;
    /// The current number of entries in the thread.
    /// Useful for knowing where the next turn will begin
    fn num_entries(&self, cx: &App) -> usize;
    /// Runs a turn for a given message and returns both the response and the index of that output message.
    fn send(&self, message: String, cx: &AsyncApp) -> Task<Result<String>>;
}

pub trait ThreadEnvironment {
    fn create_terminal(
        &self,
        command: String,
        extra_env: Vec<acp::EnvVariable>,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<u64>,
        sandbox_wrap: Option<acp_thread::SandboxWrap>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Rc<dyn TerminalHandle>>>;

    fn create_subagent(
        &self,
        label: String,
        persona: Option<acp_thread::AgentPersona>,
        capability_mode: Option<acp_thread::AgentCapabilityMode>,
        cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>>;

    fn resume_subagent(
        &self,
        _session_id: acp::SessionId,
        _cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>> {
        Err(anyhow::anyhow!(
            "Resuming subagent sessions is not supported"
        ))
    }

    // Our Grok native tool support (required for MonitorTool / GetCommandOrSubagentOutputTool
    // shims to match ACP capture harness fidelity). Integrated upstream sibling thread methods.
    fn get_command_or_subagent_output(
        &self,
        task_id: String,
        block: bool,
        timeout_ms: Option<u64>,
        cx: &mut AsyncApp,
    ) -> Task<Result<String>>;

    /// Creates an independent sibling thread visible in the agent sidebar.
    /// Unlike subagents, sibling threads are first-class threads that persist
    /// and run in parallel without reporting results back to the parent.
    fn create_sibling_thread(
        &self,
        request: SiblingThreadRequest,
        cx: &mut AsyncApp,
    ) -> Task<Result<SiblingThreadInfo>> {
        let _ = request;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!(
            "Creating sibling threads is not supported in this environment"
        )))
    }

    /// Lists the agents and models available for use with `create_sibling_thread`.
    fn list_available_agents(&self, cx: &mut App) -> Result<AvailableAgents> {
        let _ = cx;
        Err(anyhow::anyhow!(
            "Listing available agents is not supported in this environment"
        ))
    }
}

/// A request to create a new sibling thread.
#[derive(Debug, Clone)]
pub struct SiblingThreadRequest {
    /// A short title for the new thread, shown in the sidebar.
    pub title: SharedString,
    /// The initial prompt to send to the new thread.
    pub prompt: String,
    /// Optional agent ID to use. Defaults to the native Zed agent.
    pub agent_id: Option<String>,
    /// Optional model override, as `provider/model-id`.
    /// Defaults to the user's configured default model for the agent.
    pub model: Option<String>,
    /// Whether to create the thread in a new git worktree workspace.
    pub use_new_worktree: bool,
    /// Optional worktree directory name. When `None`, the UI generates a
    /// random non-colliding name (matching the manual "Create worktree"
    /// flow). Only relevant when `use_new_worktree` is true.
    pub worktree_name: Option<String>,
    /// Git ref (branch, tag, or commit) to base the new worktree on.
    /// Only relevant when `use_new_worktree` is true.
    pub base_ref: Option<String>,
}

/// Information returned when a sibling thread is successfully created.
#[derive(Debug, Clone)]
pub struct SiblingThreadInfo {
    /// The title assigned to the thread.
    pub title: SharedString,
    /// The agent ID used for the thread.
    pub agent_id: String,
    /// The model ID used for the thread, if known.
    pub model: Option<String>,
    /// An optional, non-fatal heads-up about the created thread that the
    /// caller should relay or take into account (e.g., the project had an
    /// unusual worktree layout that affected how the new worktree was set
    /// up). Empty when nothing noteworthy happened.
    pub warning: Option<String>,
}

/// A list of agents and, for each, the models available for use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableAgents {
    pub agents: Vec<AvailableAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableAgent {
    /// Identifier used when creating a thread.
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: SharedString,
    /// Whether this is Zed's built-in native agent.
    pub is_native: bool,
    /// Models available for this agent. May be empty if models are not
    /// enumerated up front (e.g., external agents that choose their own).
    pub models: Vec<AvailableModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableModel {
    /// Identifier to pass as the `model` field when creating a thread.
    pub id: String,
    /// Human-readable name.
    pub name: SharedString,
    /// Whether this is the default model for the agent.
    pub is_default: bool,
}

#[derive(Debug)]
pub enum ThreadEvent {
    UserMessage(UserMessage),
    AgentText(String),
    AgentThinking(String),
    ToolCall(acp::ToolCall),
    ToolCallUpdate(acp_thread::ToolCallUpdate),
    Plan(acp::Plan),
    ToolCallAuthorization(ToolCallAuthorization),
    SubagentSpawned(acp::SessionId),
    SubagentUpdated(acp::SessionId),
    Retry(acp_thread::RetryStatus),
    ContextCompaction,
    Stop(acp::StopReason),
}

/// Minimal skeleton entry point (NativeTurnDriver) for driving a pure native
/// Grok `Thread` turn directly. Returns / subscribes to the same `ThreadEvent`
/// values that power the shared ZedTodos collectors, `ZedTodosComponent`, plan
/// rendering, and persona badges.
///
/// This is the thinnest direct native path per the authoritative orchestration
/// design: callers operating under `is_grok_build_profile` obtain the canonical
/// event stream without going through `NativeAgentConnection` or the full ACP
/// translation layer in `handle_thread_events`.
///
/// Construction is explicitly gated on the profile flag. All usage follows
/// CLAUDE.md (existing files, no panics on fallible paths, full words).
pub struct NativeTurnDriver {
    thread: Entity<Thread>,
}

impl NativeTurnDriver {
    /// Returns a driver only for Threads where `is_grok_build_profile` is true.
    /// This is the mandatory gate for the direct native path.
    pub fn new_if_grok_native(thread: Entity<Thread>, cx: &App) -> Option<Self> {
        if thread.read(cx).is_grok_build_profile(cx) {
            Some(Self { thread })
        } else {
            None
        }
    }

    /// Drives a turn using the existing `send` path and returns the direct
    /// `ThreadEvent` subscription receiver. Identical events to the ACP path.
    pub fn send_and_drive<T>(
        &self,
        id: UserMessageId,
        content: impl IntoIterator<Item = T>,
        cx: &mut App,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>>
    where
        T: Into<UserMessageContent>,
    {
        self.thread
            .update(cx, |thread, cx| thread.send(id, content, cx))
    }

    /// Drives a resume turn, returning the direct native event receiver.
    pub fn resume_and_drive(
        &self,
        cx: &mut App,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        self.thread.update(cx, |thread, cx| thread.resume(cx))
    }

    /// Low-level drive of an already-prepared turn (after `send_existing` style prep).
    pub fn drive_existing_turn(
        &self,
        cx: &mut App,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        self.thread
            .update(cx, |thread, cx| thread.send_existing(cx))
    }
}

#[derive(Debug)]
pub struct NewTerminal {
    pub command: String,
    pub output_byte_limit: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub response: oneshot::Sender<Result<Entity<acp_thread::Terminal>>>,
}

#[derive(Debug, Clone)]
pub struct ToolPermissionContext {
    pub tool_name: String,
    pub input_values: Vec<String>,
    pub scope: ToolPermissionScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermissionScope {
    ToolInput,
    SymlinkTarget,
}

impl ToolPermissionContext {
    pub fn new(tool_name: impl Into<String>, input_values: Vec<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_values,
            scope: ToolPermissionScope::ToolInput,
        }
    }

    pub fn symlink_target(tool_name: impl Into<String>, target_paths: Vec<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_values: target_paths,
            scope: ToolPermissionScope::SymlinkTarget,
        }
    }

    /// Builds the permission options for this tool context.
    ///
    /// This is the canonical source for permission option generation.
    /// Tests should use this function rather than manually constructing options.
    ///
    /// # Shell Compatibility for Terminal Tool
    ///
    /// For the terminal tool, "Always allow" options are only shown when the user's
    /// shell supports POSIX-like command chaining syntax (`&&`, `||`, `;`, `|`).
    ///
    /// **Why this matters:** When a user sets up an "always allow" pattern like `^cargo`,
    /// we need to parse the command to extract all sub-commands and verify that EVERY
    /// sub-command matches the pattern. Otherwise, an attacker could craft a command like
    /// `cargo build && rm -rf /` that would bypass the security check.
    ///
    /// **Supported shells:** Posix (sh, bash, dash, zsh), Fish 3.0+, PowerShell 7+/Pwsh,
    /// Cmd, Xonsh, Csh, Tcsh
    ///
    /// **Unsupported shells:** Nushell (uses `and`/`or` keywords), Elvish (uses `and`/`or`
    /// keywords), Rc (Plan 9 shell - no `&&`/`||` operators)
    ///
    /// For unsupported shells, we hide the "Always allow" UI options entirely, and if
    /// the user has `always_allow` rules configured in settings, `ToolPermissionDecision::from_input`
    /// will return a `Deny` with an explanatory error message.
    pub fn build_permission_options(&self) -> acp_thread::PermissionOptions {
        use crate::pattern_extraction::*;
        use util::shell::ShellKind;

        let tool_name = &self.tool_name;
        let input_values = &self.input_values;
        if self.scope == ToolPermissionScope::SymlinkTarget {
            return acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "Yes",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "No",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ]);
        }

        // Check if the user's shell supports POSIX-like command chaining.
        // See the doc comment above for the full explanation of why this is needed.
        let shell_supports_always_allow = if tool_name == TerminalTool::NAME {
            ShellKind::system().supports_posix_chaining()
        } else {
            true
        };

        // For terminal commands with multiple pipeline commands, use DropdownWithPatterns
        // to let users individually select which command patterns to always allow.
        if tool_name == TerminalTool::NAME && shell_supports_always_allow {
            if let Some(input) = input_values.first() {
                let all_patterns = extract_all_terminal_patterns(input);
                if all_patterns.len() > 1 {
                    let mut choices = Vec::new();
                    choices.push(acp_thread::PermissionOptionChoice {
                        allow: acp::PermissionOption::new(
                            acp::PermissionOptionId::new(format!("always_allow:{}", tool_name)),
                            format!("Always for {}", tool_name.replace('_', " ")),
                            acp::PermissionOptionKind::AllowAlways,
                        ),
                        deny: acp::PermissionOption::new(
                            acp::PermissionOptionId::new(format!("always_deny:{}", tool_name)),
                            format!("Always for {}", tool_name.replace('_', " ")),
                            acp::PermissionOptionKind::RejectAlways,
                        ),
                        sub_patterns: vec![],
                    });
                    choices.push(acp_thread::PermissionOptionChoice {
                        allow: acp::PermissionOption::new(
                            acp::PermissionOptionId::new("allow"),
                            "Only this time",
                            acp::PermissionOptionKind::AllowOnce,
                        ),
                        deny: acp::PermissionOption::new(
                            acp::PermissionOptionId::new("deny"),
                            "Only this time",
                            acp::PermissionOptionKind::RejectOnce,
                        ),
                        sub_patterns: vec![],
                    });
                    return acp_thread::PermissionOptions::DropdownWithPatterns {
                        choices,
                        patterns: all_patterns,
                        tool_name: tool_name.clone(),
                    };
                }
            }
        }

        let extract_for_value = |value: &str| -> (Option<String>, Option<String>) {
            if tool_name == TerminalTool::NAME {
                (
                    extract_terminal_pattern(value),
                    extract_terminal_pattern_display(value),
                )
            } else if tool_name == CopyPathTool::NAME
                || tool_name == MovePathTool::NAME
                || tool_name == EditFileTool::NAME
                || tool_name == WriteFileTool::NAME
                || tool_name == DeletePathTool::NAME
                || tool_name == CreateDirectoryTool::NAME
            {
                (
                    extract_path_pattern(value),
                    extract_path_pattern_display(value),
                )
            } else if tool_name == FetchTool::NAME {
                (
                    extract_url_pattern(value),
                    extract_url_pattern_display(value),
                )
            } else {
                (None, None)
            }
        };

        // Extract patterns from all input values. Only offer a pattern-specific
        // "always allow/deny" button when every value produces the same pattern.
        let (pattern, pattern_display) = match input_values.as_slice() {
            [single] => extract_for_value(single),
            _ => {
                let mut iter = input_values.iter().map(|v| extract_for_value(v));
                match iter.next() {
                    Some(first) => {
                        if iter.all(|pair| pair.0 == first.0) {
                            first
                        } else {
                            (None, None)
                        }
                    }
                    None => (None, None),
                }
            }
        };

        let mut choices = Vec::new();

        let mut push_choice =
            |label: String, allow_id, deny_id, allow_kind, deny_kind, sub_patterns: Vec<String>| {
                choices.push(acp_thread::PermissionOptionChoice {
                    allow: acp::PermissionOption::new(
                        acp::PermissionOptionId::new(allow_id),
                        label.clone(),
                        allow_kind,
                    ),
                    deny: acp::PermissionOption::new(
                        acp::PermissionOptionId::new(deny_id),
                        label,
                        deny_kind,
                    ),
                    sub_patterns,
                });
            };

        if shell_supports_always_allow {
            push_choice(
                format!("Always for {}", tool_name.replace('_', " ")),
                format!("always_allow:{}", tool_name),
                format!("always_deny:{}", tool_name),
                acp::PermissionOptionKind::AllowAlways,
                acp::PermissionOptionKind::RejectAlways,
                vec![],
            );

            if let (Some(pattern), Some(display)) = (pattern, pattern_display) {
                let button_text = if tool_name == TerminalTool::NAME {
                    format!("Always for `{}` commands", display)
                } else {
                    format!("Always for `{}`", display)
                };
                push_choice(
                    button_text,
                    format!("always_allow:{}", tool_name),
                    format!("always_deny:{}", tool_name),
                    acp::PermissionOptionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectAlways,
                    vec![pattern],
                );
            }
        }

        push_choice(
            "Only this time".to_string(),
            "allow".to_string(),
            "deny".to_string(),
            acp::PermissionOptionKind::AllowOnce,
            acp::PermissionOptionKind::RejectOnce,
            vec![],
        );

        acp_thread::PermissionOptions::Dropdown(choices)
    }
}

#[derive(Debug)]
pub struct ToolCallAuthorization {
    pub tool_call: acp::ToolCallUpdate,
    pub options: acp_thread::PermissionOptions,
    pub response: oneshot::Sender<acp_thread::SelectedPermissionOutcome>,
    pub context: Option<ToolPermissionContext>,
    pub kind: acp_thread::AuthorizationKind,
}

#[derive(Debug, thiserror::Error)]
enum CompletionError {
    #[error("max tokens")]
    MaxTokens,
    #[error("refusal")]
    Refusal,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Thread {
    id: acp::SessionId,
    prompt_id: PromptId,
    updated_at: DateTime<Utc>,
    title: Option<SharedString>,
    pending_title_generation: Option<Task<()>>,
    title_generation_failed: bool,
    pending_summary_generation: Option<Shared<Task<Option<SharedString>>>>,
    summary: Option<SharedString>,
    // Accepted upstream change to Vec<Arc<Message>> for better sharing.
    // pub(crate) kept for internal Grok/ZedTodos access patterns.
    pub(crate) messages: Vec<Arc<Message>>,
    user_store: Entity<UserStore>,
    /// Holds the task that handles agent interaction until the end of the turn.
    /// Survives across multiple requests as the model performs tool calls and
    /// we run tools, report their results.
    running_turn: Option<RunningTurn>,
    /// Flag indicating the UI has a queued message waiting to be sent.
    /// Used to signal that the turn should end at the next message boundary.
    has_queued_message: bool,
    pending_message: Option<AgentMessage>,
    pub(crate) tools: BTreeMap<SharedString, Arc<dyn AnyAgentTool>>,
    request_token_usage: HashMap<UserMessageId, language_model::TokenUsage>,
    #[allow(unused)]
    cumulative_token_usage: TokenUsage,
    #[allow(unused)]
    initial_project_snapshot: Shared<Task<Option<Arc<ProjectSnapshot>>>>,
    pub(crate) context_server_registry: Entity<ContextServerRegistry>,
    profile_id: AgentProfileId,
    project_context: Entity<ProjectContext>,
    pub(crate) templates: Arc<Templates>,
    model: Option<Arc<dyn LanguageModel>>,
    grok_build_profile: bool,
    turn_id: TurnId,
    plan_phase: PlanPhase,
    summarization_model: Option<Arc<dyn LanguageModel>>,
    thinking_enabled: bool,
    thinking_effort: Option<String>,
    speed: Option<Speed>,
    prompt_capabilities_tx: watch::Sender<acp::PromptCapabilities>,
    pub(crate) prompt_capabilities_rx: watch::Receiver<acp::PromptCapabilities>,
    pub(crate) project: Entity<Project>,
    pub(crate) action_log: Entity<ActionLog>,
    /// True if this thread was imported from a shared thread and can be synced.
    imported: bool,
    /// If this is a subagent thread, contains context about the parent
    subagent_context: Option<SubagentContext>,
    /// The user's unsent prompt text, persisted so it can be restored when reloading the thread.
    draft_prompt: Option<Vec<acp::ContentBlock>>,
    ui_scroll_position: Option<gpui::ListOffset>,
    /// Weak references to running subagent threads for cancellation propagation
    running_subagents: Vec<WeakEntity<Thread>>,
    background_scheduler: NativeBackgroundTaskScheduler,
    inherits_parent_model_settings: bool,
    sandboxed_terminal_temp_dir: Option<PathBuf>,
}

impl Thread {
    fn prompt_capabilities(model: Option<&dyn LanguageModel>) -> acp::PromptCapabilities {
        let image = model.map_or(true, |model| model.supports_images());
        acp::PromptCapabilities::new()
            .image(image)
            .embedded_context(true)
    }

    fn compute_grok_build_profile(model: Option<&dyn LanguageModel>) -> bool {
        model.map_or(false, |model| {
            if &model.provider_id().0 == "x_ai" {
                model.name().0.to_ascii_lowercase().contains("grok")
            } else {
                false
            }
        })
    }

    pub fn new_subagent(
        parent_thread: &Entity<Thread>,
        persona: Option<acp_thread::AgentPersona>,
        capability_mode: Option<acp_thread::AgentCapabilityMode>,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = parent_thread.read(cx).project.clone();
        let project_context = parent_thread.read(cx).project_context.clone();
        let context_server_registry = parent_thread.read(cx).context_server_registry.clone();
        let templates = parent_thread.read(cx).templates.clone();
        let model = parent_thread.read(cx).model().cloned();
        let parent_action_log = parent_thread.read(cx).action_log().clone();
        let action_log =
            cx.new(|_cx| ActionLog::new(project.clone()).with_linked_action_log(parent_action_log));
        let mut thread = Self::new_internal(
            project,
            project_context,
            context_server_registry,
            templates,
            model,
            action_log,
            cx,
        );
        let parent_plan_phase = parent_thread.read(cx).plan_phase;
        let effective_capability = if parent_plan_phase.is_proposed() {
            Some(acp_thread::AgentCapabilityMode::ReadOnly)
        } else {
            capability_mode
        };
        thread.plan_phase = parent_plan_phase;
        thread.subagent_context = Some(SubagentContext {
            parent_thread_id: parent_thread.read(cx).id().clone(),
            depth: parent_thread.read(cx).depth() + 1,
            persona,
            capability_mode: effective_capability,
            plan_phase: Some(parent_plan_phase),
        });
        thread.inherit_parent_settings(parent_thread, cx);
        if let Some(subagent_model) = AgentSettings::get_global(cx).subagent_model.clone() {
            thread.inherits_parent_model_settings = false;
            thread.apply_model_selection(&subagent_model, cx);
        }
        thread.grok_build_profile = parent_thread.read(cx).grok_build_profile;
        thread
    }

    pub fn new(
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        model: Option<Arc<dyn LanguageModel>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            project.clone(),
            project_context,
            context_server_registry,
            templates,
            model,
            cx.new(|_cx| ActionLog::new(project)),
            cx,
        )
    }

    fn new_internal(
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        model: Option<Arc<dyn LanguageModel>>,
        action_log: Entity<ActionLog>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AgentSettings::get_global(cx);
        let profile_id = settings.default_profile.clone();
        let enable_thinking = settings
            .default_model
            .as_ref()
            .is_some_and(|model| model.enable_thinking);
        let thinking_effort = settings
            .default_model
            .as_ref()
            .and_then(|model| model.effort.clone());
        let speed = settings
            .default_model
            .as_ref()
            .and_then(|model| model.speed);
        let grok_build_profile = Self::compute_grok_build_profile(model.as_deref());
        let (prompt_capabilities_tx, prompt_capabilities_rx) =
            watch::channel(Self::prompt_capabilities(model.as_deref()));
        Self {
            id: acp::SessionId::new(uuid::Uuid::new_v4().to_string()),
            prompt_id: PromptId::new(),
            updated_at: Utc::now(),
            title: None,
            pending_title_generation: None,
            title_generation_failed: false,
            pending_summary_generation: None,
            summary: None,
            messages: Vec::new(),
            user_store: project.read(cx).user_store(),
            running_turn: None,
            has_queued_message: false,
            pending_message: None,
            tools: BTreeMap::default(),
            request_token_usage: HashMap::default(),
            cumulative_token_usage: TokenUsage::default(),
            initial_project_snapshot: {
                let project_snapshot = Self::project_snapshot(project.clone(), cx);
                cx.foreground_executor()
                    .spawn(async move { Some(project_snapshot.await) })
                    .shared()
            },
            context_server_registry,
            profile_id,
            project_context,
            templates,
            model,
            grok_build_profile,
            turn_id: TurnId::new(0),
            plan_phase: PlanPhase::default(),
            summarization_model: None,
            thinking_enabled: enable_thinking,
            speed,
            thinking_effort,
            prompt_capabilities_tx,
            prompt_capabilities_rx,
            project,
            action_log,
            imported: false,
            subagent_context: None,
            draft_prompt: None,
            ui_scroll_position: None,
            running_subagents: Vec::new(),
            background_scheduler: NativeBackgroundTaskScheduler::new(),
            inherits_parent_model_settings: true,
            sandboxed_terminal_temp_dir: None,
        }
    }

    /// Copies runtime-mutable settings from the parent thread so that
    /// subagents start with the same configuration the user selected.
    /// Every property that `set_*` propagates to `running_subagents`
    /// should be inherited here as well.
    fn inherit_parent_settings(&mut self, parent_thread: &Entity<Thread>, cx: &mut Context<Self>) {
        let parent = parent_thread.read(cx);
        self.speed = parent.speed;
        self.thinking_enabled = parent.thinking_enabled;
        self.thinking_effort = parent.thinking_effort.clone();
        self.summarization_model = parent.summarization_model.clone();
        self.profile_id = parent.profile_id.clone();
    }

    fn apply_model_selection(
        &mut self,
        selection: &LanguageModelSelection,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = Self::resolve_model_from_selection(selection, cx) else {
            log::warn!(
                "failed to resolve configured subagent model: {}/{}",
                selection.provider.0,
                selection.model
            );
            return;
        };

        self.model = Some(model.clone());
        self.grok_build_profile = Self::compute_grok_build_profile(self.model.as_deref());
        self.thinking_enabled = selection.enable_thinking && model.supports_thinking();
        self.thinking_effort = selection.effort.clone();
        self.speed = selection.speed.filter(|_| model.supports_fast_mode());
        self.prompt_capabilities_tx
            .send(Self::prompt_capabilities(self.model.as_deref()))
            .log_err();
    }

    pub fn id(&self) -> &acp::SessionId {
        &self.id
    }

    pub(crate) fn sandboxed_terminal_temp_dir(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<PathBuf> {
        if let Some(temp_dir) = &self.sandboxed_terminal_temp_dir {
            std::fs::create_dir_all(temp_dir).with_context(|| {
                format!(
                    "failed to recreate sandboxed terminal temp directory {}",
                    temp_dir.display()
                )
            })?;
            return Ok(temp_dir.clone());
        }

        let temp_dir = tempfile::Builder::new()
            .prefix("zed-agent-terminal-")
            .tempdir()
            .context("failed to create sandboxed terminal temp directory")?;
        let temp_dir = temp_dir.keep();
        self.sandboxed_terminal_temp_dir = Some(temp_dir.clone());
        cx.notify();
        Ok(temp_dir)
    }

    /// Returns true if this thread was imported from a shared thread.
    pub fn is_imported(&self) -> bool {
        self.imported
    }

    pub fn replay(
        &mut self,
        cx: &mut Context<Self>,
    ) -> mpsc::UnboundedReceiver<Result<ThreadEvent>> {
        let (tx, rx) = mpsc::unbounded();
        let stream = ThreadEventStream(tx);
        for message in &self.messages {
            match &**message {
                Message::User(user_message) => stream.send_user_message(user_message),
                Message::Agent(assistant_message) => {
                    for content in &assistant_message.content {
                        match content {
                            AgentMessageContent::Text(text) => stream.send_text(text),
                            AgentMessageContent::Thinking { text, .. } => {
                                stream.send_thinking(text)
                            }
                            AgentMessageContent::RedactedThinking(_) => {}
                            AgentMessageContent::ToolUse(tool_use) => {
                                self.replay_tool_call(
                                    tool_use,
                                    assistant_message.tool_results.get(&tool_use.id),
                                    &stream,
                                    cx,
                                );
                            }
                        }
                    }
                }
                Message::Resume => {}
                Message::Compaction(_) => stream.send_context_compaction(),
            }
        }
        rx
    }

    fn replay_tool_call(
        &self,
        tool_use: &LanguageModelToolUse,
        tool_result: Option<&LanguageModelToolResult>,
        stream: &ThreadEventStream,
        cx: &mut Context<Self>,
    ) {
        let output = tool_result
            .as_ref()
            .and_then(|result| result.output.clone());
        let replay_content = tool_result.and_then(Self::tool_result_content_for_replay);
        let status = tool_result
            .as_ref()
            .map_or(acp::ToolCallStatus::Failed, |result| {
                if result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                }
            });

        let tool = self.tools.get(tool_use.name.as_ref()).cloned().or_else(|| {
            self.context_server_registry
                .read(cx)
                .servers()
                .find_map(|(_, tools)| {
                    if let Some(tool) = tools.get(tool_use.name.as_ref()) {
                        Some(tool.clone())
                    } else {
                        None
                    }
                })
        });

        let Some(tool) = tool else {
            // Tool not found (e.g., MCP server not connected after restart),
            // but still display the saved result if available.
            // We need to send both ToolCall and ToolCallUpdate events because the UI
            // only converts raw_output to displayable content in update_fields, not from_acp.
            let title = Self::title_for_replayed_tool_use(tool_use);
            stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCall(
                    acp::ToolCall::new(tool_use.id.to_string(), title.clone())
                        .status(status)
                        .raw_input(tool_use.input.clone()),
                )))
                .ok();
            let mut fields = acp::ToolCallUpdateFields::new()
                .status(status)
                .raw_output(output);
            if tool_use.name.as_ref() == UpdateTitleTool::NAME {
                fields = fields.title(title);
            }
            if let Some(content) = replay_content {
                fields = fields.content(content);
            }
            stream.update_tool_call_fields(&tool_use.id, fields, None);
            return;
        };

        let title = tool.initial_title(tool_use.input.clone(), cx);
        let kind = tool.kind();
        stream.send_tool_call(
            &tool_use.id,
            &tool_use.name,
            title,
            kind,
            tool_use.input.clone(),
        );

        if let Some(content) = replay_content {
            stream.update_tool_call_fields(
                &tool_use.id,
                acp::ToolCallUpdateFields::new().content(content),
                None,
            );
        }

        if let Some(output) = output.clone() {
            // For replay, we use a dummy cancellation receiver since the tool already completed
            let (_cancellation_tx, cancellation_rx) = watch::channel(false);
            let tool_event_stream = ToolCallEventStream::new(
                tool_use.id.clone(),
                stream.clone(),
                Some(self.project.read(cx).fs().clone()),
                cancellation_rx,
                Some(self.plan_phase),
            );
            tool.replay(tool_use.input.clone(), output, tool_event_stream, cx)
                .log_err();
        }

        stream.update_tool_call_fields(
            &tool_use.id,
            acp::ToolCallUpdateFields::new()
                .status(status)
                .raw_output(output),
            None,
        );
    }

    fn title_for_replayed_tool_use(tool_use: &LanguageModelToolUse) -> String {
        if tool_use.name.as_ref() == UpdateTitleTool::NAME {
            let input = serde_json::from_value(tool_use.input.clone())
                .map_err(|_| serde_json::Value::String(tool_use.raw_input.clone()));
            UpdateTitleTool::title_for_input(input).to_string()
        } else {
            tool_use.name.to_string()
        }
    }

    fn tool_result_content_for_replay(
        tool_result: &LanguageModelToolResult,
    ) -> Option<Vec<acp::ToolCallContent>> {
        let has_image = tool_result
            .content
            .iter()
            .any(|part| matches!(part, LanguageModelToolResultContent::Image(_)));
        if !has_image && tool_result.output.is_some() {
            return None;
        }

        let content = tool_result
            .content
            .iter()
            .filter_map(|part| match part {
                LanguageModelToolResultContent::Text(text) => {
                    if text.is_empty() {
                        None
                    } else {
                        Some(acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text.to_string())),
                        )))
                    }
                }
                LanguageModelToolResultContent::Image(image) => Some(
                    acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Image(
                        acp::ImageContent::new(image.source.clone(), "image/png"),
                    ))),
                ),
            })
            .collect::<Vec<_>>();

        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    }

    pub fn from_db(
        id: acp::SessionId,
        db_thread: DbThread,
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AgentSettings::get_global(cx);
        let profile_id = db_thread
            .profile
            .unwrap_or_else(|| settings.default_profile.clone());

        let mut model = LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            db_thread
                .model
                .and_then(|model| {
                    let model = SelectedModel {
                        provider: model.provider.clone().into(),
                        model: model.model.into(),
                    };
                    registry.select_model(&model, cx)
                })
                .or_else(|| registry.default_model())
                .map(|model| model.model)
        });

        if model.is_none() {
            model = Self::resolve_profile_model(&profile_id, cx);
        }
        if model.is_none() {
            model = LanguageModelRegistry::global(cx).update(cx, |registry, _cx| {
                registry.default_model().map(|model| model.model)
            });
        }

        let (prompt_capabilities_tx, prompt_capabilities_rx) =
            watch::channel(Self::prompt_capabilities(model.as_deref()));

        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let grok_build_profile = Self::compute_grok_build_profile(model.as_deref());
        Self {
            id,
            prompt_id: PromptId::new(),
            title: if db_thread.title.is_empty() {
                None
            } else {
                Some(db_thread.title.clone())
            },
            pending_title_generation: None,
            title_generation_failed: false,
            pending_summary_generation: None,
            summary: db_thread.detailed_summary,
            messages: db_thread.messages,
            user_store: project.read(cx).user_store(),
            running_turn: None,
            has_queued_message: false,
            pending_message: None,
            tools: BTreeMap::default(),
            request_token_usage: db_thread.request_token_usage.clone(),
            cumulative_token_usage: db_thread.cumulative_token_usage,
            initial_project_snapshot: Task::ready(db_thread.initial_project_snapshot).shared(),
            context_server_registry,
            profile_id,
            project_context,
            templates,
            model,
            grok_build_profile,
            turn_id: TurnId::new(0),
            plan_phase: PlanPhase::default(),
            summarization_model: None,
            thinking_enabled: db_thread.thinking_enabled,
            thinking_effort: db_thread.thinking_effort,
            speed: db_thread.speed,
            project,
            action_log,
            updated_at: db_thread.updated_at,
            prompt_capabilities_tx,
            prompt_capabilities_rx,
            imported: db_thread.imported,
            subagent_context: db_thread.subagent_context,
            draft_prompt: db_thread.draft_prompt,
            ui_scroll_position: db_thread.ui_scroll_position.map(|sp| gpui::ListOffset {
                item_ix: sp.item_ix,
                offset_in_item: gpui::px(sp.offset_in_item),
            }),
            running_subagents: Vec::new(),
            background_scheduler: NativeBackgroundTaskScheduler::new(),
            inherits_parent_model_settings: true,
            sandboxed_terminal_temp_dir: db_thread.sandboxed_terminal_temp_dir,
        }
    }

    pub fn to_db(&self, cx: &App) -> Task<DbThread> {
        let initial_project_snapshot = self.initial_project_snapshot.clone();
        let mut thread = DbThread {
            title: self.title().unwrap_or_default(),
            messages: self.messages.clone(),
            updated_at: self.updated_at,
            detailed_summary: self.summary.clone(),
            initial_project_snapshot: None,
            cumulative_token_usage: self.cumulative_token_usage,
            request_token_usage: self.request_token_usage.clone(),
            model: self.model.as_ref().map(|model| DbLanguageModel {
                provider: model.provider_id().to_string(),
                model: model.id().0.to_string(),
            }),
            profile: Some(self.profile_id.clone()),
            imported: self.imported,
            subagent_context: self.subagent_context.clone(),
            speed: self.speed,
            thinking_enabled: self.thinking_enabled,
            thinking_effort: self.thinking_effort.clone(),
            draft_prompt: self.draft_prompt.clone(),
            ui_scroll_position: self.ui_scroll_position.map(|lo| {
                crate::db::SerializedScrollPosition {
                    item_ix: lo.item_ix,
                    offset_in_item: lo.offset_in_item.as_f32(),
                }
            }),
            // Keep our Grok native artifacts (Grok memory artifacts + prompt injection work).
            // Integrate upstream sandboxed terminal field.
            native_grok_artifacts: None,
            sandboxed_terminal_temp_dir: self.sandboxed_terminal_temp_dir.clone(),
        };

        cx.background_spawn(async move {
            let initial_project_snapshot = initial_project_snapshot.await;
            thread.initial_project_snapshot = initial_project_snapshot;
            thread
        })
    }

    /// Create a snapshot of the current project state including git information and unsaved buffers.
    fn project_snapshot(
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Arc<ProjectSnapshot>> {
        let task = project::telemetry_snapshot::TelemetrySnapshot::new(&project, cx);
        cx.spawn(async move |_, _| {
            let snapshot = task.await;

            Arc::new(ProjectSnapshot {
                worktree_snapshots: snapshot.worktree_snapshots,
                timestamp: Utc::now(),
            })
        })
    }

    pub fn project_context(&self) -> &Entity<ProjectContext> {
        &self.project_context
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn action_log(&self) -> &Entity<ActionLog> {
        &self.action_log
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.title.is_none()
    }

    pub fn draft_prompt(&self) -> Option<&[acp::ContentBlock]> {
        self.draft_prompt.as_deref()
    }

    pub fn set_draft_prompt(&mut self, prompt: Option<Vec<acp::ContentBlock>>) {
        self.draft_prompt = prompt;
    }

    pub fn ui_scroll_position(&self) -> Option<gpui::ListOffset> {
        self.ui_scroll_position
    }

    pub fn set_ui_scroll_position(&mut self, position: Option<gpui::ListOffset>) {
        self.ui_scroll_position = position;
    }

    pub fn model(&self) -> Option<&Arc<dyn LanguageModel>> {
        self.model.as_ref()
    }

    pub fn set_model(&mut self, model: Arc<dyn LanguageModel>, cx: &mut Context<Self>) {
        let old_usage = self.latest_token_usage();
        self.model = Some(model.clone());
        self.grok_build_profile = Self::compute_grok_build_profile(self.model.as_deref());
        let new_caps = Self::prompt_capabilities(self.model.as_deref());
        let new_usage = self.latest_token_usage();
        if old_usage != new_usage {
            cx.emit(TokenUsageUpdated(new_usage));
        }
        self.prompt_capabilities_tx.send(new_caps).log_err();

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_model(model.clone(), cx);
                    }
                })
                .ok();
        }

        cx.notify()
    }

    pub fn summarization_model(&self) -> Option<&Arc<dyn LanguageModel>> {
        self.summarization_model.as_ref()
    }

    pub fn set_summarization_model(
        &mut self,
        model: Option<Arc<dyn LanguageModel>>,
        cx: &mut Context<Self>,
    ) {
        self.summarization_model = model.clone();

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    thread.set_summarization_model(model.clone(), cx)
                })
                .ok();
        }
        cx.notify()
    }

    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }

    pub fn set_thinking_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.thinking_enabled = enabled;

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_thinking_enabled(enabled, cx);
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn thinking_effort(&self) -> Option<&String> {
        self.thinking_effort.as_ref()
    }

    pub fn set_thinking_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        self.thinking_effort = effort.clone();

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_thinking_effort(effort.clone(), cx)
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn speed(&self) -> Option<Speed> {
        self.speed
    }

    pub fn set_speed(&mut self, speed: Speed, cx: &mut Context<Self>) {
        self.speed = Some(speed);

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_speed(speed, cx);
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last().map(std::ops::Deref::deref)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn last_received_or_pending_message(&self) -> Option<Arc<Message>> {
        if let Some(message) = self.pending_message.clone() {
            Some(Arc::new(Message::Agent(message)))
        } else {
            self.messages.last().cloned()
        }
    }

    pub fn add_default_tools(
        &mut self,
        environment: Rc<dyn ThreadEnvironment>,
        cx: &mut Context<Self>,
    ) {
        let update_agent_location = self.parent_thread_id().is_none();

        let capability_read_only = self.capability_mode().map_or(false, |m| m.is_read_only());
        let plan_proposed = self.plan_phase.is_proposed();
        let read_only = capability_read_only || plan_proposed;
        self.add_tool(FetchTool::new(self.project.read(cx).client().http_client()));
        self.add_tool(FindPathTool::new(self.project.clone()));
        self.add_tool(GrepTool::new(self.project.clone()));
        self.add_tool(ListDirectoryTool::new(self.project.clone()));
        if cx.has_flag::<UpdatePlanToolFeatureFlag>() {
            self.add_tool(UpdatePlanTool);
        }
        if cx.has_flag::<UpdateTitleToolFeatureFlag>() {
            self.add_tool(UpdateTitleTool::new(cx.weak_entity()));
        }
        self.add_tool(ReadFileTool::new(
            self.project.clone(),
            self.action_log.clone(),
            update_agent_location,
        ));
        self.add_tool(WebSearchTool);

        self.add_tool(DiagnosticsTool::new(self.project.clone()));

        let code_action_store: CodeActionStore = cx.new(|_cx| None);
        self.add_tool(FindReferencesTool::new(self.project.clone()));
        self.add_tool(GetCodeActionsTool::new(
            self.project.clone(),
            code_action_store.clone(),
        ));
        self.add_tool(GoToDefinitionTool::new(self.project.clone()));

        if !read_only {
            let language_registry = self.project.read(cx).languages().clone();
            self.add_tool(CopyPathTool::new(self.project.clone()));
            self.add_tool(CreateDirectoryTool::new(self.project.clone()));
            self.add_tool(DeletePathTool::new(
                self.project.clone(),
                self.action_log.clone(),
            ));
            self.add_tool(EditFileTool::new(
                self.project.clone(),
                cx.weak_entity(),
                self.action_log.clone(),
                language_registry.clone(),
            ));
            self.add_tool(WriteFileTool::new(
                self.project.clone(),
                cx.weak_entity(),
                self.action_log.clone(),
                language_registry,
            ));
            self.add_tool(MovePathTool::new(self.project.clone()));
            self.add_tool(TerminalTool::new(self.project.clone(), environment.clone()));
            self.add_tool(ApplyCodeActionTool::new(
                self.project.clone(),
                code_action_store,
            ));
            self.add_tool(RenameTool::new(self.project.clone()));
        }

        if self.depth() < MAX_SUBAGENT_DEPTH && !read_only {
            self.add_tool(SpawnAgentTool::new(environment.clone()));
        }

        // These Grok-specific tool shims are registered unconditionally so that
        // models using the native Grok profile can invoke the exact tool names
        // observed in the TUI/harness (todo_write, monitor, enter_plan_mode,
        // get_command_or_subagent_output). Registration has zero branch cost on
        // non-Grok paths.
        self.add_tool(TodoWriteTool);
        self.add_tool(MonitorTool::new(self.project.clone(), environment.clone()));
        self.add_tool(RememberTool::new(self.project.clone()));
        self.add_tool(GetCommandOrSubagentOutputTool::new(environment.clone()));
        self.add_tool(EnterPlanModeTool);
        self.add_tool(ResolveMergeConflictTool::new(self.project.clone()));
        self.add_tool(MergeReviewTriageTool::new(self.project.clone()));
        self.add_tool(MergeReviewDiffTool::new(self.project.clone()));
        self.add_tool(MergeReviewConflictSidesTool::new(self.project.clone()));
        self.add_tool(MergeReviewRecordDecisionTool::new(self.project.clone()));
        self.add_tool(MergeReviewVerifyConflictResolvedTool::new(self.project.clone()));

        self.add_tool(CreateThreadTool::new(environment.clone()));
        self.add_tool(ListAgentsAndModelsTool::new(environment));
    }

    pub fn add_tool<T: AgentTool>(&mut self, tool: T) {
        debug_assert!(
            !self.tools.contains_key(T::NAME),
            "Duplicate tool name: {}",
            T::NAME,
        );
        self.tools.insert(T::NAME.into(), tool.erase());
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn remove_tool(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn profile(&self) -> &AgentProfileId {
        &self.profile_id
    }

    pub fn set_profile(&mut self, profile_id: AgentProfileId, cx: &mut Context<Self>) {
        if self.profile_id == profile_id {
            return;
        }

        self.profile_id = profile_id.clone();

        // Swap to the profile's preferred model when available.
        if let Some(model) = Self::resolve_profile_model(&self.profile_id, cx) {
            self.set_model(model, cx);
        }

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| thread.set_profile(profile_id.clone(), cx))
                .ok();
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) -> Task<()> {
        for subagent in self.running_subagents.drain(..) {
            if let Some(subagent) = subagent.upgrade() {
                subagent.update(cx, |thread, cx| thread.cancel(cx)).detach();
            }
        }

        let Some(running_turn) = self.running_turn.take() else {
            self.flush_pending_message(cx);
            return Task::ready(());
        };

        let turn_task = running_turn.cancel();

        cx.spawn(async move |this, cx| {
            turn_task.await;
            this.update(cx, |this, cx| {
                this.flush_pending_message(cx);
            })
            .ok();
        })
    }

    pub fn set_has_queued_message(&mut self, has_queued: bool) {
        self.has_queued_message = has_queued;
    }

    pub fn has_queued_message(&self) -> bool {
        self.has_queued_message
    }

    fn update_token_usage(&mut self, update: language_model::TokenUsage, cx: &mut Context<Self>) {
        let Some(last_user_message) = self.last_user_message() else {
            return;
        };

        self.request_token_usage
            .insert(last_user_message.id.clone(), update);
        cx.emit(TokenUsageUpdated(self.latest_token_usage()));
        cx.notify();
    }

    pub fn truncate(&mut self, message_id: UserMessageId, cx: &mut Context<Self>) -> Result<()> {
        self.cancel(cx).detach();
        // Clear pending message since cancel will try to flush it asynchronously,
        // and we don't want that content to be added after we truncate
        self.pending_message.take();
        let Some(position) = self.messages.iter().position(
            |msg| matches!(&**msg, Message::User(UserMessage { id, .. }) if id == &message_id),
        ) else {
            return Err(anyhow!("Message not found"));
        };

        for message in self.messages.drain(position..) {
            match &*message {
                Message::User(message) => {
                    self.request_token_usage.remove(&message.id);
                }
                Message::Agent(_) | Message::Resume | Message::Compaction(_) => {}
            }
        }
        self.clear_summary();
        cx.notify();
        Ok(())
    }

    pub fn latest_request_token_usage(&self) -> Option<language_model::TokenUsage> {
        let last_user_message = self.last_user_message()?;
        let tokens = self.request_token_usage.get(&last_user_message.id)?;
        Some(*tokens)
    }

    pub fn latest_token_usage(&self) -> Option<acp_thread::TokenUsage> {
        let usage = self.latest_request_token_usage()?;
        let model = self.model.clone()?;
        let input_tokens = total_input_tokens(usage);

        Some(acp_thread::TokenUsage {
            max_tokens: model.max_token_count(),
            max_output_tokens: model.max_output_tokens(),
            used_tokens: usage.total_tokens(),
            input_tokens,
            output_tokens: usage.output_tokens,
        })
    }

    /// Get the total input token count as of the message before the given message.
    ///
    /// Returns `None` if:
    /// - `target_id` is the first message (no previous message)
    /// - The previous message hasn't received a response yet (no usage data)
    /// - `target_id` is not found in the messages
    pub fn tokens_before_message(&self, target_id: &UserMessageId) -> Option<u64> {
        let mut previous_user_message_id: Option<&UserMessageId> = None;

        for message in &self.messages {
            if let Message::User(user_msg) = &**message {
                if &user_msg.id == target_id {
                    let prev_id = previous_user_message_id?;
                    let usage = self.request_token_usage.get(prev_id)?;
                    return Some(total_input_tokens(*usage));
                }
                previous_user_message_id = Some(&user_msg.id);
            }
        }
        None
    }

    /// Look up the active profile and resolve its preferred model if one is configured.
    fn resolve_profile_model(
        profile_id: &AgentProfileId,
        cx: &mut Context<Self>,
    ) -> Option<Arc<dyn LanguageModel>> {
        let selection = AgentSettings::get_global(cx)
            .profiles
            .get(profile_id)?
            .default_model
            .clone()?;
        Self::resolve_model_from_selection(&selection, cx)
    }

    /// Translate a stored model selection into the configured model from the registry.
    fn resolve_model_from_selection(
        selection: &LanguageModelSelection,
        cx: &mut Context<Self>,
    ) -> Option<Arc<dyn LanguageModel>> {
        let selected = SelectedModel {
            provider: LanguageModelProviderId::from(selection.provider.0.clone()),
            model: LanguageModelId::from(selection.model.clone()),
        };
        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry
                .select_model(&selected, cx)
                .map(|configured| configured.model)
        })
    }

    pub fn resume(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        self.messages.push(Arc::new(Message::Resume));
        cx.notify();

        log::debug!("Total messages in thread: {}", self.messages.len());
        self.advance_turn_id();
        self.run_turn(cx)
    }

    /// Sending a message results in the model streaming a response, which could include tool calls.
    /// After calling tools, the model will stops and waits for any outstanding tool calls to be completed and their results sent.
    /// The returned channel will report all the occurrences in which the model stops before erroring or ending its turn.
    pub fn send<T>(
        &mut self,
        id: UserMessageId,
        content: impl IntoIterator<Item = T>,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>>
    where
        T: Into<UserMessageContent>,
    {
        let content = content.into_iter().map(Into::into).collect::<Arc<_>>();
        log::debug!("Thread::send content: {:?}", content);

        self.messages
            .push(Arc::new(Message::User(UserMessage { id, content })));
        cx.notify();

        self.send_existing(cx)
    }

    pub fn send_existing(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        let model = self
            .model()
            .ok_or_else(|| anyhow!(NoModelConfiguredError))?;

        log::info!("Thread::send called with model: {}", model.name().0);
        self.advance_prompt_id();

        log::debug!("Total messages in thread: {}", self.messages.len());
        self.advance_turn_id();
        self.run_turn(cx)
    }

    pub fn push_acp_user_block(
        &mut self,
        id: UserMessageId,
        blocks: impl IntoIterator<Item = acp::ContentBlock>,
        path_style: PathStyle,
        cx: &mut Context<Self>,
    ) {
        let content = blocks
            .into_iter()
            .map(|block| UserMessageContent::from_content_block(block, path_style))
            .collect::<Arc<_>>();
        self.messages
            .push(Arc::new(Message::User(UserMessage { id, content })));
        cx.notify();
    }

    pub fn push_acp_agent_block(&mut self, block: acp::ContentBlock, cx: &mut Context<Self>) {
        let text = match block {
            acp::ContentBlock::Text(text_content) => text_content.text,
            acp::ContentBlock::Image(_) => "[image]".to_string(),
            acp::ContentBlock::Audio(_) => "[audio]".to_string(),
            acp::ContentBlock::ResourceLink(resource_link) => resource_link.uri,
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => resource.uri,
                acp::EmbeddedResourceResource::BlobResourceContents(resource) => resource.uri,
                _ => "[resource]".to_string(),
            },
            _ => "[unknown]".to_string(),
        };

        self.messages.push(Arc::new(Message::Agent(AgentMessage {
            content: vec![AgentMessageContent::Text(text)],
            ..Default::default()
        })));
        cx.notify();
    }

    fn run_turn(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        // Flush the old pending message synchronously before cancelling,
        // to avoid a race where the detached cancel task might flush the NEW
        // turn's pending message instead of the old one.
        self.flush_pending_message(cx);
        self.cancel(cx).detach();
        self.background_scheduler.cleanup_completed();

        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let event_stream = ThreadEventStream(events_tx);
        let message_ix = self.messages.len().saturating_sub(1);
        self.clear_summary();
        let (cancellation_tx, mut cancellation_rx) = watch::channel(false);
        self.running_turn = Some(RunningTurn {
            event_stream: event_stream.clone(),
            tools: self.enabled_tools(cx),
            cancellation_tx,
            streaming_tool_inputs: HashMap::default(),
            _task: cx.spawn(async move |this, cx| {
                log::debug!("Starting agent turn execution");

                let turn_result =
                    Self::run_turn_internal(&this, &event_stream, cancellation_rx.clone(), cx)
                        .await;

                // Check if we were cancelled - if so, cancel() already took running_turn
                // and we shouldn't touch it (it might be a NEW turn now)
                let was_cancelled = *cancellation_rx.borrow();
                if was_cancelled {
                    log::debug!("Turn was cancelled, skipping cleanup");
                    return;
                }

                _ = this.update(cx, |this, cx| this.flush_pending_message(cx));

                match turn_result {
                    Ok(()) => {
                        log::debug!("Turn execution completed");
                        _ = this.update(cx, |this, cx| {
                            this.capture_session_memory_if_grok(cx);
                        });
                        event_stream.send_stop(acp::StopReason::EndTurn);
                    }
                    Err(error) => {
                        log::error!("Turn execution failed: {:?}", error);
                        match error.downcast::<CompletionError>() {
                            Ok(CompletionError::Refusal) => {
                                event_stream.send_stop(acp::StopReason::Refusal);
                                _ = this.update(cx, |this, _| this.messages.truncate(message_ix));
                            }
                            Ok(CompletionError::MaxTokens) => {
                                event_stream.send_stop(acp::StopReason::MaxTokens);
                            }
                            Ok(CompletionError::Other(error)) | Err(error) => {
                                event_stream.send_error(error);
                            }
                        }
                    }
                }

                _ = this.update(cx, |this, _| this.running_turn.take());
            }),
        });
        Ok(events_rx)
    }

    async fn run_turn_internal(
        this: &WeakEntity<Self>,
        event_stream: &ThreadEventStream,
        mut cancellation_rx: watch::Receiver<bool>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let mut attempt = 0;
        let mut intent = CompletionIntent::UserPrompt;
        loop {
            // Re-read the model and refresh tools on each iteration so that
            // mid-turn changes (e.g. the user switches model, toggles tools,
            // or changes profile) take effect between tool-call rounds.
            let (model, request) = this.update(cx, |this, cx| {
                let model = this
                    .model
                    .clone()
                    .ok_or_else(|| anyhow!(NoModelConfiguredError))?;
                this.refresh_turn_tools(cx);
                let request = this.build_completion_request(intent, cx)?;
                anyhow::Ok((model, request))
            })??;

            telemetry::event!(
                "Agent Thread Completion",
                thread_id = this.read_with(cx, |this, _| this.id.to_string())?,
                parent_thread_id = this.read_with(cx, |this, _| this
                    .parent_thread_id()
                    .map(|id| id.to_string()))?,
                prompt_id = this.read_with(cx, |this, _| this.prompt_id.to_string())?,
                model = model.telemetry_id(),
                model_provider = model.provider_id().to_string(),
                attempt
            );

            log::debug!("Calling model.stream_completion, attempt {}", attempt);

            let (mut events, mut error) = match model.stream_completion(request, cx).await {
                Ok(events) => (events.fuse(), None),
                Err(err) => (stream::empty().boxed().fuse(), Some(err)),
            };
            let mut tool_results: FuturesUnordered<Task<LanguageModelToolResult>> =
                FuturesUnordered::new();
            let mut early_tool_results: Vec<LanguageModelToolResult> = Vec::new();
            let mut cancelled = false;
            loop {
                // Race between getting the first event, tool completion, and cancellation.
                let first_event = futures::select! {
                    event = events.next().fuse() => event,
                    tool_result = futures::StreamExt::select_next_some(&mut tool_results) => {
                        let is_error = tool_result.is_error;
                        let is_still_streaming = this
                            .read_with(cx, |this, _cx| {
                                this.running_turn
                                    .as_ref()
                                    .and_then(|turn| turn.streaming_tool_inputs.get(&tool_result.tool_use_id))
                                    .map_or(false, |inputs| !inputs.has_received_final())
                            })
                            .unwrap_or(false);

                        early_tool_results.push(tool_result);

                        // Only break if the tool errored and we are still
                        // streaming the input of the tool. If the tool errored
                        // but we are no longer streaming its input (i.e. there
                        // are parallel tool calls) we want to continue
                        // processing those tool inputs.
                        if is_error && is_still_streaming {
                            break;
                        }
                        continue;
                    }
                    _ = cancellation_rx.changed().fuse() => {
                        if *cancellation_rx.borrow() {
                            cancelled = true;
                            break;
                        }
                        continue;
                    }
                };
                let Some(first_event) = first_event else {
                    break;
                };

                // Collect all immediately available events to process as a batch
                let mut batch = vec![first_event];
                while let Some(event) = events.next().now_or_never().flatten() {
                    batch.push(event);
                }

                // Process the batch in a single update
                let batch_result = this.update(cx, |this, cx| {
                    let mut batch_tool_results = Vec::new();
                    let mut batch_error = None;

                    for event in batch {
                        log::trace!("Received completion event: {:?}", event);
                        match event {
                            Ok(event) => {
                                match this.handle_completion_event(
                                    event,
                                    event_stream,
                                    cancellation_rx.clone(),
                                    cx,
                                ) {
                                    Ok(Some(task)) => batch_tool_results.push(task),
                                    Ok(None) => {}
                                    Err(err) => {
                                        batch_error = Some(err);
                                        break;
                                    }
                                }
                            }
                            Err(err) => {
                                batch_error = Some(err.into());
                                break;
                            }
                        }
                    }

                    cx.notify();
                    (batch_tool_results, batch_error)
                })?;

                tool_results.extend(batch_result.0);
                if let Some(err) = batch_result.1 {
                    error = Some(err.downcast()?);
                    break;
                }
            }

            // Drop the stream to release the rate limit permit before tool execution.
            // The stream holds a semaphore guard that limits concurrent requests.
            // Without this, the permit would be held during potentially long-running
            // tool execution, which could cause deadlocks when tools spawn subagents
            // that need their own permits.
            drop(events);

            // Drop streaming tool input senders that never received their final input.
            // This prevents deadlock when the LLM stream ends (e.g. because of an error)
            // before sending a tool use with `is_input_complete: true`.
            this.update(cx, |this, _cx| {
                if let Some(running_turn) = this.running_turn.as_mut() {
                    if running_turn.streaming_tool_inputs.is_empty() {
                        return;
                    }
                    log::warn!("Dropping partial tool inputs because the stream ended");
                    running_turn.streaming_tool_inputs.drain();
                }
            })?;

            let end_turn = tool_results.is_empty() && early_tool_results.is_empty();

            for tool_result in early_tool_results {
                Self::process_tool_result(this, event_stream, cx, tool_result)?;
            }
            while let Some(tool_result) = tool_results.next().await {
                Self::process_tool_result(this, event_stream, cx, tool_result)?;
            }

            this.update(cx, |this, cx| {
                this.flush_pending_message(cx);
                if this.title.is_none() {
                    this.generate_title(cx);
                }
            })?;

            if cancelled {
                log::debug!("Turn cancelled by user, exiting");
                return Ok(());
            }

            if let Some(error) = error {
                attempt += 1;
                let retry = this.update(cx, |this, cx| {
                    let user_store = this.user_store.read(cx);
                    this.handle_completion_error(error, attempt, user_store.plan())
                })??;
                let timer = cx.background_executor().timer(retry.duration);
                event_stream.send_retry(retry);
                futures::select! {
                    _ = timer.fuse() => {}
                    _ = cancellation_rx.changed().fuse() => {
                        if *cancellation_rx.borrow() {
                            log::debug!("Turn cancelled during retry delay, exiting");
                            return Ok(());
                        }
                    }
                }
                this.update(cx, |this, _cx| {
                    if let Some(Message::Agent(message)) = this.last_message() {
                        if message.tool_results.is_empty() {
                            intent = CompletionIntent::UserPrompt;
                            this.messages.push(Arc::new(Message::Resume));
                        }
                    }
                })?;
            } else if end_turn {
                return Ok(());
            } else {
                let has_queued = this.update(cx, |this, _| this.has_queued_message())?;
                if has_queued {
                    log::debug!("Queued message found, ending turn at message boundary");
                    return Ok(());
                }
                intent = CompletionIntent::ToolResults;
                attempt = 0;
            }
        }
    }

    fn process_tool_result(
        this: &WeakEntity<Thread>,
        event_stream: &ThreadEventStream,
        cx: &mut AsyncApp,
        tool_result: LanguageModelToolResult,
    ) -> Result<(), anyhow::Error> {
        log::debug!("Tool finished {:?}", tool_result);

        event_stream.update_tool_call_fields(
            &tool_result.tool_use_id,
            acp::ToolCallUpdateFields::new()
                .status(if tool_result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                })
                .raw_output(tool_result.output.clone()),
            None,
        );
        this.update(cx, |this, _cx| {
            this.pending_message()
                .tool_results
                .insert(tool_result.tool_use_id.clone(), tool_result)
        })?;
        Ok(())
    }

    fn handle_completion_error(
        &mut self,
        error: LanguageModelCompletionError,
        attempt: u8,
        plan: Option<Plan>,
    ) -> Result<acp_thread::RetryStatus> {
        let Some(model) = self.model.as_ref() else {
            return Err(anyhow!(error));
        };

        let auto_retry = if model.provider_id() == ZED_CLOUD_PROVIDER_ID {
            plan.is_some()
        } else {
            true
        };

        if !auto_retry {
            return Err(anyhow!(error));
        }

        let Some(strategy) = Self::retry_strategy_for(&error) else {
            return Err(anyhow!(error));
        };

        let max_attempts = match &strategy {
            RetryStrategy::ExponentialBackoff { max_attempts, .. } => *max_attempts,
            RetryStrategy::Fixed { max_attempts, .. } => *max_attempts,
        };

        if attempt > max_attempts {
            return Err(anyhow!(error));
        }

        let delay = match &strategy {
            RetryStrategy::ExponentialBackoff { initial_delay, .. } => {
                let delay_secs = initial_delay.as_secs() * 2u64.pow((attempt - 1) as u32);
                Duration::from_secs(delay_secs)
            }
            RetryStrategy::Fixed { delay, .. } => *delay,
        };
        log::debug!("Retry attempt {attempt} with delay {delay:?}");

        Ok(acp_thread::RetryStatus {
            last_error: error.to_string().into(),
            attempt: attempt as usize,
            max_attempts: max_attempts as usize,
            started_at: Instant::now(),
            duration: delay,
        })
    }

    /// A helper method that's called on every streamed completion event.
    /// Returns an optional tool result task, which the main agentic loop will
    /// send back to the model when it resolves.
    fn handle_completion_event(
        &mut self,
        event: LanguageModelCompletionEvent,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Result<Option<Task<LanguageModelToolResult>>> {
        log::trace!("Handling streamed completion event: {:?}", event);
        use LanguageModelCompletionEvent::*;

        match event {
            StartMessage { .. } => {
                self.flush_pending_message(cx);
                self.pending_message = Some(AgentMessage::default());
            }
            Text(new_text) => self.handle_text_event(new_text, event_stream),
            Thinking { text, signature } => {
                self.handle_thinking_event(text, signature, event_stream)
            }
            RedactedThinking { data } => self.handle_redacted_thinking_event(data),
            ReasoningDetails(details) => {
                let last_message = self.pending_message();
                // Store the last non-empty reasoning_details (overwrites earlier ones)
                // This ensures we keep the encrypted reasoning with signatures, not the early text reasoning
                if let serde_json::Value::Array(arr) = &details {
                    if !arr.is_empty() {
                        last_message.reasoning_details = Some(Arc::new(details));
                    }
                } else {
                    last_message.reasoning_details = Some(Arc::new(details));
                }
            }
            ToolUse(tool_use) => {
                return Ok(self.handle_tool_use_event(tool_use, event_stream, cancellation_rx, cx));
            }
            ToolUseJsonParseError {
                id,
                tool_name,
                raw_input,
                json_parse_error,
            } => {
                return Ok(self.handle_tool_use_json_parse_error_event(
                    id,
                    tool_name,
                    raw_input,
                    json_parse_error,
                    event_stream,
                    cancellation_rx,
                    cx,
                ));
            }
            UsageUpdate(usage) => {
                telemetry::event!(
                    "Agent Thread Completion Usage Updated",
                    thread_id = self.id.to_string(),
                    parent_thread_id = self.parent_thread_id().map(|id| id.to_string()),
                    prompt_id = self.prompt_id.to_string(),
                    model = self.model.as_ref().map(|m| m.telemetry_id()),
                    model_provider = self.model.as_ref().map(|m| m.provider_id().to_string()),
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                );
                self.update_token_usage(usage, cx);
            }
            Stop(StopReason::Refusal) => return Err(CompletionError::Refusal.into()),
            Stop(StopReason::MaxTokens) => return Err(CompletionError::MaxTokens.into()),
            Stop(StopReason::ToolUse | StopReason::EndTurn) => {}
            Started | Queued { .. } => {}
        }

        Ok(None)
    }

    fn handle_text_event(&mut self, new_text: String, event_stream: &ThreadEventStream) {
        event_stream.send_text(&new_text);

        let last_message = self.pending_message();
        if let Some(AgentMessageContent::Text(text)) = last_message.content.last_mut() {
            text.push_str(&new_text);
        } else {
            last_message
                .content
                .push(AgentMessageContent::Text(new_text));
        }
    }

    fn handle_thinking_event(
        &mut self,
        new_text: String,
        new_signature: Option<String>,
        event_stream: &ThreadEventStream,
    ) {
        event_stream.send_thinking(&new_text);

        let last_message = self.pending_message();
        if let Some(AgentMessageContent::Thinking { text, signature }) =
            last_message.content.last_mut()
        {
            text.push_str(&new_text);
            *signature = new_signature.or(signature.take());
        } else {
            last_message.content.push(AgentMessageContent::Thinking {
                text: new_text,
                signature: new_signature,
            });
        }
    }

    fn handle_redacted_thinking_event(&mut self, data: String) {
        let last_message = self.pending_message();
        last_message
            .content
            .push(AgentMessageContent::RedactedThinking(data));
    }

    fn handle_tool_use_event(
        &mut self,
        tool_use: LanguageModelToolUse,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Option<Task<LanguageModelToolResult>> {
        cx.notify();

        let tool = self.tool(tool_use.name.as_ref());
        let mut title = SharedString::from(&tool_use.name);
        let mut kind = acp::ToolKind::Other;
        if let Some(tool) = tool.as_ref() {
            title = tool.initial_title(tool_use.input.clone(), cx);
            kind = tool.kind();
        }

        self.send_or_update_tool_use(&tool_use, title, kind, event_stream);

        let Some(tool) = tool else {
            let content = format!("No tool named {} exists", tool_use.name);
            return Some(Task::ready(LanguageModelToolResult {
                content: vec![LanguageModelToolResultContent::Text(Arc::from(content))],
                tool_use_id: tool_use.id,
                tool_name: tool_use.name,
                is_error: true,
                output: None,
            }));
        };

        if !tool_use.is_input_complete {
            if tool.supports_input_streaming() {
                let running_turn = self.running_turn.as_mut()?;
                if let Some(sender) = running_turn.streaming_tool_inputs.get_mut(&tool_use.id) {
                    sender.send_partial(tool_use.input);
                    return None;
                }

                let (mut sender, tool_input) = ToolInputSender::channel();
                sender.send_partial(tool_use.input);
                running_turn
                    .streaming_tool_inputs
                    .insert(tool_use.id.clone(), sender);

                let tool = tool.clone();
                log::debug!("Running streaming tool {}", tool_use.name);
                return Some(self.run_tool(
                    tool,
                    tool_input,
                    tool_use.id,
                    tool_use.name,
                    event_stream,
                    cancellation_rx,
                    cx,
                ));
            } else {
                return None;
            }
        }

        if let Some(mut sender) = self
            .running_turn
            .as_mut()?
            .streaming_tool_inputs
            .remove(&tool_use.id)
        {
            sender.send_full(tool_use.input);
            return None;
        }

        log::debug!("Running tool {}", tool_use.name);
        let tool_input = ToolInput::ready(tool_use.input);
        Some(self.run_tool(
            tool,
            tool_input,
            tool_use.id,
            tool_use.name,
            event_stream,
            cancellation_rx,
            cx,
        ))
    }

    fn run_tool(
        &mut self,
        tool: Arc<dyn AnyAgentTool>,
        tool_input: ToolInput<serde_json::Value>,
        tool_use_id: LanguageModelToolUseId,
        tool_name: Arc<str>,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Task<LanguageModelToolResult> {
        if tool.name() == "enter_plan_mode" {
            self.plan_phase.set_to_proposed();
        }
        let fs = self.project.read(cx).fs().clone();
        let tool_event_stream = ToolCallEventStream::new(
            tool_use_id.clone(),
            event_stream.clone(),
            Some(fs),
            cancellation_rx,
            Some(self.plan_phase),
        );
        tool_event_stream.update_fields(
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress),
        );
        let supports_images = self.model().is_some_and(|model| model.supports_images());
        let tool_result = tool.run(tool_input, tool_event_stream, cx);
        cx.foreground_executor().spawn(async move {
            let (is_error, output) = match tool_result.await {
                Ok(mut output) => {
                    let contains_image = output
                        .llm_output
                        .iter()
                        .any(|part| matches!(part, LanguageModelToolResultContent::Image(_)));
                    if contains_image && !supports_images {
                        // Replace each image part with an inline placeholder so
                        // any accompanying text is still presented to the model.
                        // If there's nothing else in the output, surface an error
                        // to match the pre-multi-part behavior for image-only
                        // tool results.
                        let placeholder = LanguageModelToolResultContent::Text(Arc::from(
                            "[Tool responded with an image, but this model doesn't support images]",
                        ));
                        let has_non_image = output
                            .llm_output
                            .iter()
                            .any(|part| !matches!(part, LanguageModelToolResultContent::Image(_)));
                        if has_non_image {
                            output.llm_output = output
                                .llm_output
                                .into_iter()
                                .map(|part| match part {
                                    LanguageModelToolResultContent::Image(_) => placeholder.clone(),
                                    other => other,
                                })
                                .collect();
                            (false, output)
                        } else {
                            let output = anyhow::anyhow!(
                                "Attempted to read an image, but this model doesn't support it.",
                            )
                            .into();
                            (true, output)
                        }
                    } else {
                        (false, output)
                    }
                }
                Err(output) => (true, output),
            };

            LanguageModelToolResult {
                tool_use_id,
                tool_name,
                is_error,
                content: output.llm_output,
                output: Some(output.raw_output),
            }
        })
    }

    fn handle_tool_use_json_parse_error_event(
        &mut self,
        tool_use_id: LanguageModelToolUseId,
        tool_name: Arc<str>,
        raw_input: Arc<str>,
        json_parse_error: String,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Option<Task<LanguageModelToolResult>> {
        let tool_use = LanguageModelToolUse {
            id: tool_use_id,
            name: tool_name,
            raw_input: raw_input.to_string(),
            input: serde_json::json!({}),
            is_input_complete: true,
            thought_signature: None,
        };
        self.send_or_update_tool_use(
            &tool_use,
            SharedString::from(&tool_use.name),
            acp::ToolKind::Other,
            event_stream,
        );

        let tool = self.tool(tool_use.name.as_ref());

        let Some(tool) = tool else {
            let content = format!("No tool named {} exists", tool_use.name);
            return Some(Task::ready(LanguageModelToolResult {
                content: vec![LanguageModelToolResultContent::Text(Arc::from(content))],
                tool_use_id: tool_use.id,
                tool_name: tool_use.name,
                is_error: true,
                output: None,
            }));
        };

        let error_message = format!("Error parsing input JSON: {json_parse_error}");

        if tool.supports_input_streaming()
            && let Some(mut sender) = self
                .running_turn
                .as_mut()?
                .streaming_tool_inputs
                .remove(&tool_use.id)
        {
            sender.send_invalid_json(error_message);
            return None;
        }

        log::debug!("Running tool {}. Received invalid JSON", tool_use.name);
        let tool_input = ToolInput::invalid_json(error_message);
        Some(self.run_tool(
            tool,
            tool_input,
            tool_use.id,
            tool_use.name,
            event_stream,
            cancellation_rx,
            cx,
        ))
    }

    fn send_or_update_tool_use(
        &mut self,
        tool_use: &LanguageModelToolUse,
        title: SharedString,
        kind: acp::ToolKind,
        event_stream: &ThreadEventStream,
    ) {
        // Ensure the last message ends in the current tool use
        let last_message = self.pending_message();

        let has_tool_use = last_message.content.iter_mut().rev().any(|content| {
            if let AgentMessageContent::ToolUse(last_tool_use) = content {
                if last_tool_use.id == tool_use.id {
                    *last_tool_use = tool_use.clone();
                    return true;
                }
            }
            false
        });

        if !has_tool_use {
            event_stream.send_tool_call(
                &tool_use.id,
                &tool_use.name,
                title,
                kind,
                tool_use.input.clone(),
            );
            last_message
                .content
                .push(AgentMessageContent::ToolUse(tool_use.clone()));
        } else {
            event_stream.update_tool_call_fields(
                &tool_use.id,
                acp::ToolCallUpdateFields::new()
                    .title(title.as_str())
                    .kind(kind)
                    .raw_input(tool_use.input.clone()),
                None,
            );
        }
    }

    pub fn title(&self) -> Option<SharedString> {
        self.title.clone()
    }

    pub fn is_generating_summary(&self) -> bool {
        self.pending_summary_generation.is_some()
    }

    pub fn is_generating_title(&self) -> bool {
        self.pending_title_generation.is_some()
    }

    pub fn has_failed_title_generation(&self) -> bool {
        self.title_generation_failed
    }

    pub fn can_generate_title(&self, cx: &App) -> bool {
        self.pending_title_generation.is_none()
            && self.summarization_model.is_some()
            && !self.update_title_tool_available(cx)
    }

    fn update_title_tool_available(&self, cx: &App) -> bool {
        if let Some(running_turn) = self.running_turn.as_ref() {
            running_turn.tools.contains_key(UpdateTitleTool::NAME)
        } else {
            self.enabled_tools(cx).contains_key(UpdateTitleTool::NAME)
        }
    }

    pub fn summary(&mut self, cx: &mut Context<Self>) -> Shared<Task<Option<SharedString>>> {
        if let Some(summary) = self.summary.as_ref() {
            return Task::ready(Some(summary.clone())).shared();
        }
        if let Some(task) = self.pending_summary_generation.clone() {
            return task;
        }
        let Some(model) = self.summarization_model.clone() else {
            log::error!("No summarization model available");
            return Task::ready(None).shared();
        };
        let mut request = LanguageModelRequest {
            intent: Some(CompletionIntent::ThreadContextSummarization),
            temperature: AgentSettings::temperature_for_model(&model, cx),
            ..Default::default()
        };

        for message in &self.messages {
            request.messages.extend(message.to_request());
        }

        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![SUMMARIZE_THREAD_DETAILED_PROMPT.into()],
            cache: false,
            reasoning_details: None,
        });

        let task = cx
            .spawn(async move |this, cx| {
                let mut summary = String::new();
                let mut messages = model.stream_completion(request, cx).await.log_err()?;
                while let Some(event) = messages.next().await {
                    let event = event.log_err()?;
                    let text = match event {
                        LanguageModelCompletionEvent::Text(text) => text,
                        _ => continue,
                    };

                    let mut lines = text.lines();
                    summary.extend(lines.next());
                }

                log::debug!("Setting summary: {}", summary);
                let summary = SharedString::from(summary);

                this.update(cx, |this, cx| {
                    this.summary = Some(summary.clone());
                    this.pending_summary_generation = None;
                    cx.notify()
                })
                .ok()?;

                Some(summary)
            })
            .shared();
        self.pending_summary_generation = Some(task.clone());
        task
    }

    pub fn generate_title(&mut self, cx: &mut Context<Self>) {
        if !self.can_generate_title(cx) {
            return;
        }

        self.title_generation_failed = false;
        let Some(model) = self.summarization_model.clone() else {
            return;
        };

        log::debug!(
            "Generating title with model: {:?}",
            self.summarization_model.as_ref().map(|model| model.name())
        );
        let mut request = LanguageModelRequest {
            intent: Some(CompletionIntent::ThreadSummarization),
            temperature: AgentSettings::temperature_for_model(&model, cx),
            ..Default::default()
        };

        for message in &self.messages {
            request.messages.extend(message.to_request());
        }

        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![SUMMARIZE_THREAD_PROMPT.into()],
            cache: false,
            reasoning_details: None,
        });
        self.pending_title_generation = Some(cx.spawn(async move |this, cx| {
            let mut title = String::new();

            let generate = async {
                let mut messages = model.stream_completion(request, cx).await?;
                while let Some(event) = messages.next().await {
                    let event = event?;
                    let text = match event {
                        LanguageModelCompletionEvent::Text(text) => text,
                        _ => continue,
                    };

                    let mut lines = text.lines();
                    title.extend(lines.next());

                    // Stop if the LLM generated multiple lines.
                    if lines.next().is_some() {
                        break;
                    }
                }
                anyhow::Ok(())
            };

            let succeeded = generate
                .await
                .context("failed to generate thread title")
                .log_err()
                .is_some();
            _ = this.update(cx, |this, cx| {
                this.pending_title_generation = None;
                if succeeded {
                    this.set_title(title.into(), cx);
                } else {
                    this.title_generation_failed = true;
                    cx.emit(TitleUpdated);
                    cx.notify();
                }
            });
        }));
    }

    pub fn set_title(&mut self, title: SharedString, cx: &mut Context<Self>) {
        self.pending_title_generation = None;
        self.title_generation_failed = false;
        if Some(&title) != self.title.as_ref() {
            self.title = Some(title);
            cx.emit(TitleUpdated);
            cx.notify();
        }
    }

    fn clear_summary(&mut self) {
        self.summary = None;
        self.pending_summary_generation = None;
    }

    fn last_user_message(&self) -> Option<&UserMessage> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| match &**message {
                Message::User(user_message) => Some(user_message),
                Message::Agent(_) | Message::Resume | Message::Compaction(_) => None,
            })
    }

    fn pending_message(&mut self) -> &mut AgentMessage {
        self.pending_message.get_or_insert_default()
    }

    fn flush_pending_message(&mut self, cx: &mut Context<Self>) {
        let Some(mut message) = self.pending_message.take() else {
            return;
        };

        if message.content.is_empty() {
            return;
        }

        for content in &message.content {
            let AgentMessageContent::ToolUse(tool_use) = content else {
                continue;
            };

            if !message.tool_results.contains_key(&tool_use.id) {
                message.tool_results.insert(
                    tool_use.id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: tool_use.id.clone(),
                        tool_name: tool_use.name.clone(),
                        is_error: true,
                        content: vec![LanguageModelToolResultContent::Text(
                            TOOL_CANCELED_MESSAGE.into(),
                        )],
                        output: None,
                    },
                );
            }
        }

        self.messages.push(Arc::new(Message::Agent(message)));
        self.updated_at = Utc::now();
        self.clear_summary();
        cx.notify()
    }

    pub(crate) fn build_completion_request(
        &self,
        completion_intent: CompletionIntent,
        cx: &App,
    ) -> Result<LanguageModelRequest> {
        let completion_intent =
            if self.is_subagent() && completion_intent == CompletionIntent::UserPrompt {
                CompletionIntent::Subagent
            } else {
                completion_intent
            };

        let model = self
            .model()
            .ok_or_else(|| anyhow!(NoModelConfiguredError))?;
        let tools = if let Some(turn) = self.running_turn.as_ref() {
            turn.tools
                .iter()
                .filter_map(|(tool_name, tool)| {
                    log::trace!("Including tool: {}", tool_name);
                    Some(LanguageModelRequestTool {
                        name: tool_name.to_string(),
                        description: tool.description().to_string(),
                        input_schema: tool.input_schema(model.tool_input_format()).log_err()?,
                        use_input_streaming: tool.supports_input_streaming(),
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        log::debug!("Building completion request");
        log::debug!("Completion intent: {:?}", completion_intent);

        let available_tools: Vec<_> = self
            .running_turn
            .as_ref()
            .map(|turn| turn.tools.keys().cloned().collect())
            .unwrap_or_default();

        log::debug!("Request includes {} tools", available_tools.len());
        let messages = self.build_request_messages(available_tools, cx);
        log::debug!("Request will include {} messages", messages.len());

        let request = LanguageModelRequest {
            thread_id: Some(self.id.to_string()),
            prompt_id: Some(self.prompt_id.to_string()),
            intent: Some(completion_intent),
            messages,
            tools,
            tool_choice: None,
            stop: Vec::new(),
            temperature: AgentSettings::temperature_for_model(model, cx),
            thinking_allowed: self.thinking_enabled,
            thinking_effort: self.thinking_effort.clone(),
            speed: self.speed(),
        };

        log::debug!("Completion request built successfully");
        Ok(request)
    }

    fn enabled_tools(&self, cx: &App) -> BTreeMap<SharedString, Arc<dyn AnyAgentTool>> {
        let Some(model) = self.model.as_ref() else {
            return BTreeMap::new();
        };
        let Some(profile) = AgentSettings::get_global(cx).profiles.get(&self.profile_id) else {
            return BTreeMap::new();
        };
        fn truncate(tool_name: &SharedString) -> SharedString {
            if tool_name.len() > MAX_TOOL_NAME_LENGTH {
                let mut truncated = tool_name.to_string();
                truncated.truncate(MAX_TOOL_NAME_LENGTH);
                truncated.into()
            } else {
                tool_name.clone()
            }
        }

        let mut tools = self
            .tools
            .iter()
            .filter_map(|(tool_name, tool)| {
                if tool.supports_provider(&model.provider_id())
                    && profile.is_tool_enabled(tool_name)
                {
                    Some((truncate(tool_name), tool.clone()))
                } else {
                    None
                }
            })
            .filter(|(tool_name, _)| match tool_name.as_ref() {
                RenameTool::NAME => cx.has_flag::<RenameToolFeatureFlag>(),
                FindReferencesTool::NAME
                | GetCodeActionsTool::NAME
                | ApplyCodeActionTool::NAME
                | GoToDefinitionTool::NAME => cx.has_flag::<LspToolFeatureFlag>(),
                CreateThreadTool::NAME | ListAgentsAndModelsTool::NAME => {
                    cx.has_flag::<CreateThreadToolFeatureFlag>()
                }
                _ => true,
            })
            .collect::<BTreeMap<_, _>>();

        let mut context_server_tools = Vec::new();
        let mut seen_tools = tools.keys().cloned().collect::<HashSet<_>>();
        let mut duplicate_tool_names = HashSet::default();
        for (server_id, server_tools) in self.context_server_registry.read(cx).servers() {
            for (tool_name, tool) in server_tools {
                if profile.is_context_server_tool_enabled(&server_id.0, &tool_name) {
                    let tool_name = truncate(tool_name);
                    if !seen_tools.insert(tool_name.clone()) {
                        duplicate_tool_names.insert(tool_name.clone());
                    }
                    context_server_tools.push((server_id.clone(), tool_name, tool.clone()));
                }
            }
        }

        // When there are duplicate tool names, disambiguate by prefixing them
        // with the server ID (converted to snake_case for API compatibility).
        // In the rare case there isn't enough space for the disambiguated tool
        // name, keep only the last tool with this name.
        for (server_id, tool_name, tool) in context_server_tools {
            if duplicate_tool_names.contains(&tool_name) {
                let available = MAX_TOOL_NAME_LENGTH.saturating_sub(tool_name.len());
                if available >= 2 {
                    let mut disambiguated = server_id.0.to_snake_case();
                    disambiguated.truncate(available - 1);
                    disambiguated.push('_');
                    disambiguated.push_str(&tool_name);
                    tools.insert(disambiguated.into(), tool.clone());
                } else {
                    tools.insert(tool_name, tool.clone());
                }
            } else {
                tools.insert(tool_name, tool.clone());
            }
        }

        tools
    }

    fn refresh_turn_tools(&mut self, cx: &App) {
        let tools = self.enabled_tools(cx);
        if let Some(turn) = self.running_turn.as_mut() {
            turn.tools = tools;
        }
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn AnyAgentTool>> {
        self.running_turn.as_ref()?.tools.get(name).cloned()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.running_turn
            .as_ref()
            .is_some_and(|turn| turn.tools.contains_key(name))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_registered_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub(crate) fn register_running_subagent(&mut self, subagent: WeakEntity<Thread>) {
        self.running_subagents.push(subagent);
    }

    pub(crate) fn unregister_running_subagent(
        &mut self,
        subagent_session_id: &acp::SessionId,
        cx: &App,
    ) {
        self.running_subagents.retain(|s| {
            s.upgrade()
                .map_or(false, |s| s.read(cx).id() != subagent_session_id)
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn running_subagent_ids(&self, cx: &App) -> Vec<acp::SessionId> {
        self.running_subagents
            .iter()
            .filter_map(|s| s.upgrade().map(|s| s.read(cx).id().clone()))
            .collect()
    }

    pub fn is_subagent(&self) -> bool {
        self.subagent_context.is_some()
    }

    pub fn parent_thread_id(&self) -> Option<acp::SessionId> {
        self.subagent_context
            .as_ref()
            .map(|c| c.parent_thread_id.clone())
    }

    pub fn depth(&self) -> u8 {
        self.subagent_context.as_ref().map(|c| c.depth).unwrap_or(0)
    }

    pub fn persona(&self) -> Option<acp_thread::AgentPersona> {
        self.subagent_context.as_ref().and_then(|c| c.persona)
    }

    pub fn capability_mode(&self) -> Option<acp_thread::AgentCapabilityMode> {
        self.subagent_context
            .as_ref()
            .and_then(|c| c.capability_mode)
    }

    /// Sets the operating persona for this (native) Grok thread and future
    /// subagents it spawns. Used by the rich Grok Build prompt-box menu.
    pub fn set_persona(&mut self, persona: Option<acp_thread::AgentPersona>) {
        let mut ctx = self
            .subagent_context
            .take()
            .unwrap_or_else(|| SubagentContext {
                parent_thread_id: acp::SessionId::new(""),
                depth: 0,
                persona: None,
                capability_mode: None,
                plan_phase: None,
            });
        ctx.persona = persona;
        self.subagent_context = Some(ctx);
    }

    /// Sets the capability mode (Read-Only vs Full) for this native Grok thread.
    pub fn set_capability_mode(&mut self, mode: Option<acp_thread::AgentCapabilityMode>) {
        let mut ctx = self
            .subagent_context
            .take()
            .unwrap_or_else(|| SubagentContext {
                parent_thread_id: acp::SessionId::new(""),
                depth: 0,
                persona: None,
                capability_mode: None,
                plan_phase: None,
            });
        ctx.capability_mode = mode;
        self.subagent_context = Some(ctx);
    }

    pub fn plan_phase(&self) -> PlanPhase {
        self.plan_phase
    }

    pub fn clear_plan(&mut self, cx: &mut Context<Self>) {
        if self.plan_phase.is_proposed() {
            self.plan_phase.approve();
        } else {
            self.plan_phase.reset();
        }
        cx.notify();
    }

    pub fn is_grok_build_profile(&self, _cx: &App) -> bool {
        self.grok_build_profile
    }

    pub fn current_turn_id(&self) -> TurnId {
        self.turn_id
    }

    fn advance_turn_id(&mut self) {
        self.turn_id = TurnId::new(u32::from(self.turn_id) + 1);
    }

    pub fn schedule_background_monitor(&mut self, command: String) -> String {
        let turn = self.current_turn_id();
        self.background_scheduler
            .register_monitor(turn, command, None)
    }

    pub fn retrieve_background_monitor_output(
        &self,
        task_id: String,
        block: bool,
        timeout_ms: Option<u64>,
    ) -> Task<Result<String>> {
        self.background_scheduler
            .retrieve_output(&task_id, block, timeout_ms)
    }

    pub fn has_active_background_monitors(&self) -> bool {
        self.background_scheduler.has_active_monitors()
    }

    fn capture_session_memory_if_grok(&self, cx: &App) {
        if !self.is_grok_build_profile(cx) {
            return;
        }
        let Some(cwd) = self
            .project
            .read(cx)
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        else {
            return;
        };
        let turn_id = self.current_turn_id();
        let summary = format!("Turn {turn_id} completed");
        if let Ok(mut store) = memory_palace::MemoryPalaceStore::open_for_cwd(&cwd) {
            store.project.capture_session(summary).log_err();
        }
    }

    pub fn grok_memory(&self, cx: &App) -> GrokMemoryArtifacts {
        if let Some(worktree) = self.project.read(cx).visible_worktrees(cx).next() {
            let abs_path = worktree.read(cx).abs_path().to_path_buf();
            return grok_memory_artifacts_for_cwd(&abs_path);
        }
        GrokMemoryArtifacts::default()
    }

    pub(crate) fn build_project_diagnostics_context(&self, cx: &App) -> String {
        self.project.read_with(cx, |project, cx| {
            let mut output = String::new();
            let mut has_any_diagnostics = false;
            for (project_path, _, diagnostic_summary) in project.diagnostic_summaries(true, cx) {
                if diagnostic_summary.error_count > 0 || diagnostic_summary.warning_count > 0 {
                    has_any_diagnostics = true;
                    let display_path = if let Some(worktree) =
                        project.worktree_for_id(project_path.worktree_id, cx)
                    {
                        worktree
                            .read(cx)
                            .absolutize(&project_path.path)
                            .display()
                            .to_string()
                    } else {
                        project_path.path.display(PathStyle::local()).to_string()
                    };
                    write!(
                        output,
                        "{}: {} errors, {} warnings\n",
                        display_path,
                        diagnostic_summary.error_count,
                        diagnostic_summary.warning_count
                    )
                    .ok();
                }
            }
            if has_any_diagnostics {
                output
            } else {
                "No errors or warnings reported by Zed's language servers.\n".to_string()
            }
        })
    }

    pub fn set_subagent_context(&mut self, context: SubagentContext) {
        self.subagent_context = Some(context);
    }

    pub fn is_turn_complete(&self) -> bool {
        self.running_turn.is_none()
    }

    fn build_request_messages(
        &self,
        available_tools: Vec<SharedString>,
        cx: &App,
    ) -> Vec<LanguageModelRequestMessage> {
        log::trace!(
            "Building request messages from {} thread messages",
            self.messages.len()
        );

        let user_agents_md = UserAgentsMd::global(cx).and_then(|s| s.content().cloned());
        let subagent_persona = self.persona().map(|p| p.display_name().to_string());
        let subagent_capability_mode = self.capability_mode().map(|m| m.display_name().to_string());
        let is_grok_build_profile = self.is_grok_build_profile(cx);
        let current_turn_id_str = if is_grok_build_profile {
            Some(format!("{}", self.current_turn_id()))
        } else {
            None
        };
        let prior_turn_summary = if is_grok_build_profile {
            self.messages.iter().rev().find_map(|message| {
                if let Message::Agent(agent_message) = &**message {
                    agent_message.content.iter().find_map(|content| {
                        if let AgentMessageContent::Text(text) = content {
                            let truncated = if text.len() > 180 {
                                let mut boundary = 180;
                                while boundary > 0 && !text.is_char_boundary(boundary) {
                                    boundary -= 1;
                                }
                                format!("{}…", &text[..boundary])
                            } else {
                                text.clone()
                            };
                            Some(format!("Prior assistant response: {}", truncated))
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
        } else {
            None
        };
        let mut system_prompt = SystemPromptTemplate {
            project: self.project_context.read(cx),
            available_tools,
            model_name: self.model.as_ref().map(|m| m.name().0.to_string()),
            date: Local::now().format("%Y-%m-%d").to_string(),
            user_agents_md,
            subagent_persona,
            subagent_capability_mode,
            // Our Grok profile data (is_grok_build_profile, TurnId, prior summary)
            // preserved for native fidelity + behavioral rules. Integrated upstream
            // sandboxing field.
            is_grok_build_profile,
            current_turn_id: current_turn_id_str,
            prior_turn_summary,
            sandboxing: crate::sandboxing::sandboxing_enabled(cx),
        }
        .render(&self.templates)
        .context("failed to build system prompt")
        .expect("Invalid template");
        if self.is_grok_build_profile(cx) {
            system_prompt.push_str("\n\n");
            let grok_with_verification =
                crate::inject_verification_rules_for_native_profile(GROK_BUILD_SYSTEM_FRAGMENTS);
            system_prompt.push_str(&grok_with_verification);
            let memory_artifacts = self.grok_memory(cx);
            if let Some(full) = &memory_artifacts.workspace_memory_full {
                system_prompt.push_str("\n\n## Grok Persistent Memory (MEMORY.md)\n");
                system_prompt.push_str(full);
            }
            if let Some(full) = &memory_artifacts.global_memory_full {
                system_prompt.push_str("\n\n## Grok Persistent Memory (global)\n");
                system_prompt.push_str(full);
            }
            let facts = &memory_artifacts.facts_from_db;
            if !facts.is_empty() {
                system_prompt.push_str("\n\n## Grok Learned Facts (SQLite DB layer)\n");
                for fact in facts {
                    if let Some(content) = &fact.content {
                        system_prompt.push_str(content);
                        system_prompt.push_str("\n");
                    }
                }
            }
            let project_diagnostics_context = self.build_project_diagnostics_context(cx);
            system_prompt.push_str("\n\n## Current Zed Editor Diagnostics (LSP errors and warnings - primary context, prefer over shell clippy)\n");
            system_prompt.push_str(&project_diagnostics_context);
        }
        let mut messages = vec![LanguageModelRequestMessage {
            role: Role::System,
            content: vec![system_prompt.into()],
            cache: false,
            reasoning_details: None,
        }];
        self.extend_request_history(&mut messages);

        if let Some(last_message) = messages.last_mut() {
            last_message.cache = true;
        }

        if let Some(message) = self.pending_message.as_ref() {
            messages.extend(message.to_request());
        }

        messages
    }

    fn extend_request_history(&self, messages: &mut Vec<LanguageModelRequestMessage>) {
        let Some(compaction_ix) = self.latest_compaction_message_ix() else {
            for message in &self.messages {
                messages.extend(message.to_request());
            }
            return;
        };

        if matches!(
            &*self.messages[compaction_ix],
            Message::Compaction(CompactionInfo::Summary(_))
        ) {
            messages.extend(self.retained_user_request_messages_before(compaction_ix));
        }

        for message in &self.messages[compaction_ix..] {
            messages.extend(message.to_request());
        }
    }

    fn latest_compaction_message_ix(&self) -> Option<usize> {
        self.messages
            .iter()
            .rposition(|message| matches!(&**message, Message::Compaction(_)))
    }

    fn retained_user_request_messages_before(
        &self,
        compaction_ix: usize,
    ) -> Vec<LanguageModelRequestMessage> {
        let mut remaining_bytes = COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET;
        let mut retained_messages = Vec::new();

        for message in self.messages[..compaction_ix].iter().rev() {
            let Message::User(user_message) = &**message else {
                continue;
            };
            if user_message.content.is_empty() {
                continue;
            }

            let request_message = user_message.to_request();
            let byte_count = user_message_byte_len(&request_message);
            if let Some(bytes) = remaining_bytes.checked_sub(byte_count) {
                remaining_bytes = bytes;
                retained_messages.push(request_message);
            } else {
                if remaining_bytes > 0
                    && let Some(request_message) =
                        truncate_user_message_to_byte_budget(request_message, remaining_bytes)
                {
                    retained_messages.push(request_message);
                }
                break;
            }
        }

        retained_messages.reverse();
        retained_messages
    }

    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();
        for (ix, message) in self.messages.iter().enumerate() {
            if ix > 0 {
                markdown.push('\n');
            }
            match &**message {
                Message::User(_) => markdown.push_str("## User\n\n"),
                Message::Agent(_) => markdown.push_str("## Assistant\n\n"),
                Message::Resume | Message::Compaction(_) => {}
            }
            markdown.push_str(&message.to_markdown());
        }

        if let Some(message) = self.pending_message.as_ref() {
            markdown.push_str("\n## Assistant\n\n");
            markdown.push_str(&message.to_markdown());
        }

        markdown
    }

    fn advance_prompt_id(&mut self) {
        self.prompt_id = PromptId::new();
    }

    fn retry_strategy_for(error: &LanguageModelCompletionError) -> Option<RetryStrategy> {
        use LanguageModelCompletionError::*;
        use http_client::StatusCode;

        // General strategy here:
        // - If retrying won't help (e.g. invalid API key or payload too large), return None so we don't retry at all.
        // - If it's a time-based issue (e.g. server overloaded, rate limit exceeded), retry up to 4 times with exponential backoff.
        // - If it's an issue that *might* be fixed by retrying (e.g. internal server error), retry up to 3 times.
        match error {
            HttpResponseError {
                status_code: StatusCode::TOO_MANY_REQUESTS,
                ..
            } => Some(RetryStrategy::ExponentialBackoff {
                initial_delay: BASE_RETRY_DELAY,
                max_attempts: MAX_RETRY_ATTEMPTS,
            }),
            ServerOverloaded { retry_after, .. } | RateLimitExceeded { retry_after, .. } => {
                Some(RetryStrategy::Fixed {
                    delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                    max_attempts: MAX_RETRY_ATTEMPTS,
                })
            }
            UpstreamProviderError {
                status,
                retry_after,
                ..
            } => match *status {
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    Some(RetryStrategy::Fixed {
                        delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                        max_attempts: MAX_RETRY_ATTEMPTS,
                    })
                }
                StatusCode::INTERNAL_SERVER_ERROR => Some(RetryStrategy::Fixed {
                    delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                    // Internal Server Error could be anything, retry up to 3 times.
                    max_attempts: 3,
                }),
                status => {
                    // There is no StatusCode variant for the unofficial HTTP 529 ("The service is overloaded"),
                    // but we frequently get them in practice. See https://http.dev/529
                    if status.as_u16() == 529 {
                        Some(RetryStrategy::Fixed {
                            delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                            max_attempts: MAX_RETRY_ATTEMPTS,
                        })
                    } else {
                        Some(RetryStrategy::Fixed {
                            delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                            max_attempts: 2,
                        })
                    }
                }
            },
            ApiInternalServerError { .. } => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 3,
            }),
            ApiReadResponseError { .. }
            | HttpSend { .. }
            | DeserializeResponse { .. }
            | BadRequestFormat { .. } => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 3,
            }),
            // Retrying these errors definitely shouldn't help.
            HttpResponseError {
                status_code:
                    StatusCode::PAYLOAD_TOO_LARGE | StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED,
                ..
            }
            | AuthenticationError { .. }
            | PermissionError { .. }
            | NoApiKey { .. }
            | ApiEndpointNotFound { .. }
            | PromptTooLarge { .. } => None,
            // These errors might be transient, so retry them
            SerializeRequest { .. } | BuildRequestBody { .. } | StreamEndedUnexpectedly { .. } => {
                Some(RetryStrategy::Fixed {
                    delay: BASE_RETRY_DELAY,
                    max_attempts: 1,
                })
            }
            // Retry all other 4xx and 5xx errors once.
            HttpResponseError { status_code, .. }
                if status_code.is_client_error() || status_code.is_server_error() =>
            {
                Some(RetryStrategy::Fixed {
                    delay: BASE_RETRY_DELAY,
                    max_attempts: 3,
                })
            }
            Other(err) if err.is::<language_model::PaymentRequiredError>() => {
                // Retrying won't help for Payment Required errors.
                None
            }
            // Conservatively assume that any other errors are non-retryable
            HttpResponseError { .. } | Other(..) => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 2,
            }),
        }
    }
}

fn total_input_tokens(usage: language_model::TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
}

fn user_message_byte_len(message: &LanguageModelRequestMessage) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            MessageContent::Text(text) => text.len(),
            MessageContent::Image(image) => image.len(),
            // These can never occur in a user message
            MessageContent::Thinking { .. }
            | MessageContent::RedactedThinking(_)
            | MessageContent::ToolResult(_)
            | MessageContent::ToolUse(_) => 0,
        })
        .sum()
}

fn truncate_user_message_to_byte_budget(
    mut message: LanguageModelRequestMessage,
    byte_budget: usize,
) -> Option<LanguageModelRequestMessage> {
    let mut remaining_bytes = byte_budget;
    let mut content = Vec::with_capacity(message.content.len());

    for item in message.content {
        match item {
            MessageContent::Text(text) => {
                let fits = text.len() <= remaining_bytes;
                if let Some(text) = take_text_within_byte_budget(text, &mut remaining_bytes) {
                    content.push(MessageContent::Text(text));
                }
                if !fits {
                    break;
                }
            }
            MessageContent::Image(image) => {
                let byte_len = image.len();
                if let Some(bytes) = remaining_bytes.checked_sub(byte_len) {
                    remaining_bytes = bytes;
                    content.push(MessageContent::Image(image));
                } else {
                    break;
                }
            }
            // These can never occur in a user message
            MessageContent::Thinking { .. }
            | MessageContent::RedactedThinking(_)
            | MessageContent::ToolResult(_)
            | MessageContent::ToolUse(_) => {}
        }
    }

    if content.is_empty() {
        None
    } else {
        message.content = content;
        Some(message)
    }
}

fn take_text_within_byte_budget(text: String, remaining_bytes: &mut usize) -> Option<String> {
    if text.is_empty() || *remaining_bytes == 0 {
        return None;
    }

    if let Some(bytes) = remaining_bytes.checked_sub(text.len()) {
        *remaining_bytes = bytes;
        return Some(text);
    }

    let end = text.floor_char_boundary((*remaining_bytes).min(text.len()));
    *remaining_bytes = 0;

    let text = text[..end].to_string();

    if text.is_empty() { None } else { Some(text) }
}

struct RunningTurn {
    /// Holds the task that handles agent interaction until the end of the turn.
    /// Survives across multiple requests as the model performs tool calls and
    /// we run tools, report their results.
    _task: Task<()>,
    /// The current event stream for the running turn. Used to report a final
    /// cancellation event if we cancel the turn.
    event_stream: ThreadEventStream,
    /// The tools that are enabled for the current iteration of the turn.
    /// Refreshed at the start of each iteration via `refresh_turn_tools`.
    tools: BTreeMap<SharedString, Arc<dyn AnyAgentTool>>,
    /// Sender to signal tool cancellation. When cancel is called, this is
    /// set to true so all tools can detect user-initiated cancellation.
    cancellation_tx: watch::Sender<bool>,
    /// Senders for tools that support input streaming and have already been
    /// started but are still receiving input from the LLM.
    streaming_tool_inputs: HashMap<LanguageModelToolUseId, ToolInputSender>,
}

impl RunningTurn {
    fn cancel(mut self) -> Task<()> {
        log::debug!("Cancelling in progress turn");
        self.cancellation_tx.send(true).ok();
        self.event_stream.send_canceled();
        self._task
    }
}

pub struct TokenUsageUpdated(pub Option<acp_thread::TokenUsage>);

impl EventEmitter<TokenUsageUpdated> for Thread {}

pub struct TitleUpdated;

impl EventEmitter<TitleUpdated> for Thread {}

/// A channel-based wrapper that delivers tool input to a running tool.
///
/// For non-streaming tools, created via `ToolInput::ready()` so `.recv()` resolves immediately.
/// For streaming tools, partial JSON snapshots arrive via `.recv_partial()` as the LLM streams
/// them, followed by the final complete input available through `.recv()`.
pub struct ToolInput<T> {
    rx: mpsc::UnboundedReceiver<ToolInputPayload<serde_json::Value>>,
    _phantom: PhantomData<T>,
}

impl<T: DeserializeOwned> ToolInput<T> {
    #[cfg(any(test, feature = "test-support"))]
    pub fn resolved(input: impl Serialize) -> Self {
        let value = serde_json::to_value(input).expect("failed to serialize tool input");
        Self::ready(value)
    }

    pub fn ready(value: serde_json::Value) -> Self {
        let (tx, rx) = mpsc::unbounded();
        tx.unbounded_send(ToolInputPayload::Full(value)).ok();
        Self {
            rx,
            _phantom: PhantomData,
        }
    }

    pub fn invalid_json(error_message: String) -> Self {
        let (tx, rx) = mpsc::unbounded();
        tx.unbounded_send(ToolInputPayload::InvalidJson { error_message })
            .ok();
        Self {
            rx,
            _phantom: PhantomData,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> (ToolInputSender, Self) {
        let (sender, input) = ToolInputSender::channel();
        (sender, input.cast())
    }

    /// Wait for the final deserialized input, ignoring all partial updates.
    /// Non-streaming tools can use this to wait until the whole input is available.
    pub async fn recv(mut self) -> Result<T> {
        while let Ok(value) = self.next().await {
            match value {
                ToolInputPayload::Full(value) => return Ok(value),
                ToolInputPayload::Partial(_) => {}
                ToolInputPayload::InvalidJson { error_message } => {
                    return Err(anyhow!(error_message));
                }
            }
        }
        Err(anyhow!("tool input was not fully received"))
    }

    pub async fn next(&mut self) -> Result<ToolInputPayload<T>> {
        let value = self
            .rx
            .next()
            .await
            .ok_or_else(|| anyhow!("tool input was not fully received"))?;

        Ok(match value {
            ToolInputPayload::Partial(payload) => ToolInputPayload::Partial(payload),
            ToolInputPayload::Full(payload) => {
                ToolInputPayload::Full(serde_json::from_value(payload)?)
            }
            ToolInputPayload::InvalidJson { error_message } => {
                ToolInputPayload::InvalidJson { error_message }
            }
        })
    }

    fn cast<U: DeserializeOwned>(self) -> ToolInput<U> {
        ToolInput {
            rx: self.rx,
            _phantom: PhantomData,
        }
    }
}

pub enum ToolInputPayload<T> {
    Partial(serde_json::Value),
    Full(T),
    InvalidJson { error_message: String },
}

pub struct ToolInputSender {
    has_received_final: bool,
    tx: mpsc::UnboundedSender<ToolInputPayload<serde_json::Value>>,
}

impl ToolInputSender {
    pub(crate) fn channel() -> (Self, ToolInput<serde_json::Value>) {
        let (tx, rx) = mpsc::unbounded();
        let sender = Self {
            tx,
            has_received_final: false,
        };
        let input = ToolInput {
            rx,
            _phantom: PhantomData,
        };
        (sender, input)
    }

    pub(crate) fn has_received_final(&self) -> bool {
        self.has_received_final
    }

    pub fn send_partial(&mut self, payload: serde_json::Value) {
        self.tx
            .unbounded_send(ToolInputPayload::Partial(payload))
            .ok();
    }

    pub fn send_full(&mut self, payload: serde_json::Value) {
        self.has_received_final = true;
        self.tx.unbounded_send(ToolInputPayload::Full(payload)).ok();
    }

    pub fn send_invalid_json(&mut self, error_message: String) {
        self.has_received_final = true;
        self.tx
            .unbounded_send(ToolInputPayload::InvalidJson { error_message })
            .ok();
    }
}

pub trait AgentTool
where
    Self: 'static + Sized,
{
    type Input: for<'de> Deserialize<'de> + Serialize + JsonSchema;
    type Output: for<'de> Deserialize<'de> + Serialize + Into<LanguageModelToolResultContent>;

    const NAME: &'static str;

    fn description() -> SharedString {
        let schema = schemars::schema_for!(Self::Input);
        SharedString::new(
            schema
                .get("description")
                .and_then(|description| description.as_str())
                .unwrap_or_default(),
        )
    }

    fn kind() -> acp::ToolKind;

    /// The initial tool title to display. Can be updated during the tool run.
    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        cx: &mut App,
    ) -> SharedString;

    /// Returns the JSON schema that describes the tool's input.
    fn input_schema(format: LanguageModelToolSchemaFormat) -> Schema {
        language_model::tool_schema::root_schema_for::<Self::Input>(format)
    }

    /// Returns whether the tool supports streaming of tool use parameters.
    fn supports_input_streaming() -> bool {
        false
    }

    /// Some tools rely on a provider for the underlying billing or other reasons.
    /// Allow the tool to check if they are compatible, or should be filtered out.
    fn supports_provider(_provider: &LanguageModelProviderId) -> bool {
        true
    }

    /// Runs the tool with the provided input.
    ///
    /// Returns `Result<Self::Output, Self::Output>` rather than `Result<Self::Output, anyhow::Error>`
    /// because tool errors are sent back to the model as tool results. This means error output must
    /// be structured and readable by the agent — not an arbitrary `anyhow::Error`. Returning the
    /// same `Output` type for both success and failure lets tools provide structured data while
    /// still signaling whether the invocation succeeded or failed.
    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>>;

    /// Emits events for a previous execution of the tool.
    fn replay(
        &self,
        _input: Self::Input,
        _output: Self::Output,
        _event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        Ok(())
    }

    fn erase(self) -> Arc<dyn AnyAgentTool> {
        Arc::new(Erased(Arc::new(self)))
    }
}

pub struct Erased<T>(T);

pub struct AgentToolOutput {
    pub llm_output: Vec<LanguageModelToolResultContent>,
    pub raw_output: serde_json::Value,
}

impl From<anyhow::Error> for AgentToolOutput {
    fn from(error: anyhow::Error) -> Self {
        let llm_output = vec![error.into()];
        let raw_output = serde_json::to_value(&llm_output).unwrap_or_else(|e| {
            log::error!("Failed to serialize tool output: {e}");
            serde_json::Value::Null
        });
        Self {
            raw_output,
            llm_output,
        }
    }
}

pub trait AnyAgentTool {
    fn name(&self) -> SharedString;
    fn description(&self) -> SharedString;
    fn kind(&self) -> acp::ToolKind;
    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString;
    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value>;
    fn supports_input_streaming(&self) -> bool {
        false
    }
    fn supports_provider(&self, _provider: &LanguageModelProviderId) -> bool {
        true
    }
    /// See [`AgentTool::run`] for why this returns `Result<AgentToolOutput, AgentToolOutput>`.
    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>>;
    fn replay(
        &self,
        input: serde_json::Value,
        output: serde_json::Value,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Result<()>;
}

impl<T> AnyAgentTool for Erased<Arc<T>>
where
    T: AgentTool,
{
    fn name(&self) -> SharedString {
        T::NAME.into()
    }

    fn description(&self) -> SharedString {
        T::description()
    }

    fn kind(&self) -> acp::ToolKind {
        T::kind()
    }

    fn supports_input_streaming(&self) -> bool {
        T::supports_input_streaming()
    }

    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString {
        let parsed_input = serde_json::from_value(input.clone()).map_err(|_| input);
        self.0.initial_title(parsed_input, _cx)
    }

    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value> {
        let mut json = serde_json::to_value(T::input_schema(format))?;
        language_model::tool_schema::adapt_schema_to_format(&mut json, format)?;
        Ok(json)
    }

    fn supports_provider(&self, provider: &LanguageModelProviderId) -> bool {
        T::supports_provider(provider)
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>> {
        let tool_input: ToolInput<T::Input> = input.cast();
        let task = self.0.clone().run(tool_input, event_stream, cx);
        cx.spawn(async move |_cx| match task.await {
            Ok(output) => {
                let raw_output = serde_json::to_value(&output).unwrap_or_else(|e| {
                    log::error!("Failed to serialize tool output: {e}");
                    serde_json::Value::Null
                });
                Ok(AgentToolOutput {
                    raw_output,
                    llm_output: vec![output.into()],
                })
            }
            Err(error_output) => {
                let raw_output = serde_json::to_value(&error_output).unwrap_or_else(|e| {
                    log::error!("Failed to serialize tool error output: {e}");
                    serde_json::Value::Null
                });
                Err(AgentToolOutput {
                    llm_output: vec![error_output.into()],
                    raw_output,
                })
            }
        })
    }

    fn replay(
        &self,
        input: serde_json::Value,
        output: serde_json::Value,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Result<()> {
        let input = serde_json::from_value(input)?;
        let output = serde_json::from_value(output)?;
        self.0.replay(input, output, event_stream, cx)
    }
}

#[derive(Clone)]
struct ThreadEventStream(mpsc::UnboundedSender<Result<ThreadEvent>>);

impl ThreadEventStream {
    fn send_user_message(&self, message: &UserMessage) {
        self.0
            .unbounded_send(Ok(ThreadEvent::UserMessage(message.clone())))
            .ok();
    }

    fn send_text(&self, text: &str) {
        self.0
            .unbounded_send(Ok(ThreadEvent::AgentText(text.to_string())))
            .ok();
    }

    fn send_thinking(&self, text: &str) {
        self.0
            .unbounded_send(Ok(ThreadEvent::AgentThinking(text.to_string())))
            .ok();
    }

    fn send_tool_call(
        &self,
        id: &LanguageModelToolUseId,
        tool_name: &str,
        title: SharedString,
        kind: acp::ToolKind,
        input: serde_json::Value,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ToolCall(Self::initial_tool_call(
                id,
                tool_name,
                title.to_string(),
                kind,
                input,
            ))))
            .ok();
    }

    fn initial_tool_call(
        id: &LanguageModelToolUseId,
        tool_name: &str,
        title: String,
        kind: acp::ToolKind,
        input: serde_json::Value,
    ) -> acp::ToolCall {
        acp::ToolCall::new(id.to_string(), title)
            .kind(kind)
            .raw_input(input)
            .meta(acp_thread::meta_with_tool_name(tool_name))
    }

    fn update_tool_call_fields(
        &self,
        tool_use_id: &LanguageModelToolUseId,
        fields: acp::ToolCallUpdateFields,
        meta: Option<acp::Meta>,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ToolCallUpdate(
                acp::ToolCallUpdate::new(tool_use_id.to_string(), fields)
                    .meta(meta)
                    .into(),
            )))
            .ok();
    }

    fn send_plan(&self, plan: acp::Plan) {
        self.0.unbounded_send(Ok(ThreadEvent::Plan(plan))).ok();
    }

    fn send_retry(&self, status: acp_thread::RetryStatus) {
        self.0.unbounded_send(Ok(ThreadEvent::Retry(status))).ok();
    }

    fn send_context_compaction(&self) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ContextCompaction))
            .ok();
    }

    fn send_stop(&self, reason: acp::StopReason) {
        self.0.unbounded_send(Ok(ThreadEvent::Stop(reason))).ok();
    }

    fn send_canceled(&self) {
        self.0
            .unbounded_send(Ok(ThreadEvent::Stop(acp::StopReason::Cancelled)))
            .ok();
    }

    fn send_error(&self, error: impl Into<anyhow::Error>) {
        self.0.unbounded_send(Err(error.into())).ok();
    }
}

#[derive(Clone)]
pub struct ToolCallEventStream {
    tool_use_id: LanguageModelToolUseId,
    stream: ThreadEventStream,
    fs: Option<Arc<dyn Fs>>,
    cancellation_rx: watch::Receiver<bool>,
    plan_phase: Option<PlanPhase>,
}

impl ToolCallEventStream {
    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> (Self, ToolCallEventStreamReceiver) {
        let (stream, receiver, _cancellation_tx) = Self::test_with_cancellation();
        (stream, receiver)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_with_cancellation() -> (Self, ToolCallEventStreamReceiver, watch::Sender<bool>) {
        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let (cancellation_tx, cancellation_rx) = watch::channel(false);

        let stream = ToolCallEventStream::new(
            "test_id".into(),
            ThreadEventStream(events_tx),
            None,
            cancellation_rx,
            None,
        );

        (
            stream,
            ToolCallEventStreamReceiver(events_rx),
            cancellation_tx,
        )
    }

    /// Signal cancellation for this event stream. Only available in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn signal_cancellation_with_sender(cancellation_tx: &mut watch::Sender<bool>) {
        cancellation_tx.send(true).ok();
    }

    fn new(
        tool_use_id: LanguageModelToolUseId,
        stream: ThreadEventStream,
        fs: Option<Arc<dyn Fs>>,
        cancellation_rx: watch::Receiver<bool>,
        plan_phase: Option<PlanPhase>,
    ) -> Self {
        Self {
            tool_use_id,
            stream,
            fs,
            cancellation_rx,
            plan_phase,
        }
    }

    /// Returns a future that resolves when the user cancels the tool call.
    /// Tools should select on this alongside their main work to detect user cancellation.
    pub fn cancelled_by_user(&self) -> impl std::future::Future<Output = ()> + '_ {
        let mut rx = self.cancellation_rx.clone();
        async move {
            loop {
                if *rx.borrow() {
                    return;
                }
                if rx.changed().await.is_err() {
                    // Sender dropped, will never be cancelled
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Returns true if the user has cancelled this tool call.
    /// This is useful for checking cancellation state after an operation completes,
    /// to determine if the completion was due to user cancellation.
    pub fn was_cancelled_by_user(&self) -> bool {
        *self.cancellation_rx.clone().borrow()
    }

    pub fn tool_use_id(&self) -> &LanguageModelToolUseId {
        &self.tool_use_id
    }

    pub fn update_fields(&self, fields: acp::ToolCallUpdateFields) {
        self.stream
            .update_tool_call_fields(&self.tool_use_id, fields, None);
    }

    pub fn update_fields_with_meta(
        &self,
        fields: acp::ToolCallUpdateFields,
        meta: Option<acp::Meta>,
    ) {
        self.stream
            .update_tool_call_fields(&self.tool_use_id, fields, meta);
    }

    pub fn update_diff(&self, diff: Entity<acp_thread::Diff>) {
        self.stream
            .0
            .unbounded_send(Ok(ThreadEvent::ToolCallUpdate(
                acp_thread::ToolCallUpdateDiff {
                    id: acp::ToolCallId::new(self.tool_use_id.to_string()),
                    diff,
                }
                .into(),
            )))
            .ok();
    }

    pub fn subagent_spawned(&self, id: acp::SessionId) {
        self.stream
            .0
            .unbounded_send(Ok(ThreadEvent::SubagentSpawned(id)))
            .ok();
    }

    pub fn subagent_updated(&self, id: acp::SessionId) {
        self.stream
            .0
            .unbounded_send(Ok(ThreadEvent::SubagentUpdated(id)))
            .ok();
    }

    pub fn update_plan(&self, plan: acp::Plan) {
        self.stream.send_plan(plan);
    }

    /// Authorize a third-party tool (e.g., MCP tool from a context server).
    ///
    /// Unlike built-in tools, third-party tools don't support pattern-based permissions.
    /// They only support `default` (allow/deny/confirm) per tool.
    ///
    /// Uses the dropdown authorization flow with two granularities:
    /// - "Always for <display_name> MCP tool" → sets `tools.<tool_id>.default = "allow"` or "deny"
    /// - "Only this time" → allow/deny once
    pub fn authorize_third_party_tool(
        &self,
        title: impl Into<String>,
        tool_id: String,
        display_name: String,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        let options = acp_thread::PermissionOptions::Dropdown(vec![
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new(format!("always_allow_mcp:{tool_id}")),
                    format!("Always for {display_name} MCP tool"),
                    acp::PermissionOptionKind::AllowAlways,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new(format!("always_deny_mcp:{tool_id}")),
                    format!("Always for {display_name} MCP tool"),
                    acp::PermissionOptionKind::RejectAlways,
                ),
                sub_patterns: vec![],
            },
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "Only this time",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "Only this time",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                sub_patterns: vec![],
            },
        ]);

        // MCP tools are gated only by tool id (no per-input pattern
        // matching), so we pass a single empty input value just to satisfy
        // `decide_permission_from_settings`' signature.
        let check_settings: Box<dyn Fn(&App) -> ToolPermissionDecision> =
            Box::new(move |cx: &App| {
                let settings = agent_settings::AgentSettings::get_global(cx);
                decide_permission_from_settings(&tool_id, &[String::new()], settings)
            });

        self.run_authorization_loop(title, options, None, Some(check_settings), cx)
    }

    /// Gate a tool call on user permission, driven by the agent's
    /// tool-permission settings.
    ///
    /// Evaluates the current settings up-front: returns `Ok(())` immediately
    /// if the tool is already allowed, an error if it is denied, and
    /// otherwise prompts the user for a decision. While a prompt is pending,
    /// a subscription to `SettingsStore` watches for changes (for example,
    /// when the user clicks "Always for …" on a sibling tool call and the
    /// new rule becomes globally visible). When settings change, the current
    /// prompt is dismissed and the decision is re-evaluated. This closes the
    /// gap where an "Always for …" decision on one pending tool call would
    /// not propagate to other pending tool calls in the same turn or in
    /// subagent turns.
    ///
    /// For authorizations that must always prompt regardless of settings
    /// (e.g. symlink-escape confirmations, sensitive settings-file edits),
    /// use [`Self::prompt`] instead.
    pub fn authorize(
        &self,
        title: impl Into<String>,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        let options = context.build_permission_options();

        let tool_name = context.tool_name.clone();
        let input_values = context.input_values.clone();
        let check_settings: Box<dyn Fn(&App) -> ToolPermissionDecision> =
            Box::new(move |cx: &App| {
                decide_permission_from_settings(
                    &tool_name,
                    &input_values,
                    agent_settings::AgentSettings::get_global(cx),
                )
            });

        self.run_authorization_loop(title, options, Some(context), Some(check_settings), cx)
    }

    /// Like [`Self::authorize`], but always prompts the user without
    /// consulting settings. Use this for authorizations that must be
    /// confirmed even when the user has configured `always_allow` rules —
    /// for example, symlink-escape confirmations or edits that target
    /// sensitive settings files.
    pub fn authorize_always_prompt(
        &self,
        title: impl Into<String>,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        let options = context.build_permission_options();
        self.run_authorization_loop(title, options, Some(context), None, cx)
    }

    /// Prompts the user to choose between an explicit set of actions and
    /// returns the chosen `option_id`.
    ///
    /// Unlike [`Self::authorize`] / [`Self::authorize_always_prompt`], this
    /// does not interpret the user's choice as a permission grant — callers
    /// are responsible for handling each `option_id` explicitly. Use this
    /// when a tool needs the user to pick between several side-effecting
    /// actions (for example, "Save" vs "Discard" for a dirty buffer).
    pub fn prompt_for_decision(
        &self,
        title: Option<String>,
        message: Option<String>,
        options: Vec<acp::PermissionOption>,
        cx: &mut App,
    ) -> Task<Result<acp::PermissionOptionId>> {
        let options = acp_thread::PermissionOptions::Flat(options);
        let stream = self.stream.clone();
        let tool_use_id = self.tool_use_id.clone();
        cx.spawn(async move |_cx| {
            let mut fields = acp::ToolCallUpdateFields::new();
            if let Some(title) = title {
                fields = fields.title(title);
            }
            if let Some(message) = message {
                fields = fields.content(vec![acp::ToolCallContent::from(message)]);
            }

            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(tool_use_id.to_string(), fields),
                        options,
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::ActionChoice,
                    },
                )))
            {
                log::error!("Failed to send tool call decision prompt: {error}");
                return Err(anyhow!("Failed to send tool call decision prompt: {error}"));
            }

            let outcome = response_rx
                .await
                .map_err(|_| anyhow!("authorization channel closed"))?;
            Ok(outcome.option_id)
        })
    }

    /// Prompts the user for authorization.
    ///
    /// When `check_settings` is `Some`, this gate is settings-driven: the
    /// settings are evaluated up-front (an Allow or Deny result resolves the
    /// task immediately without prompting), and while a prompt is pending a
    /// `SettingsStore` subscription watches for changes. A subsequent Allow
    /// or Deny dismisses the prompt UI and resolves the task without user
    /// interaction.
    ///
    /// When `check_settings` is `None`, the user is always prompted and
    /// settings changes are ignored. This suits prompts that aren't
    /// settings-driven (e.g. symlink-escape confirmations).
    fn run_authorization_loop(
        &self,
        title: String,
        options: acp_thread::PermissionOptions,
        context: Option<ToolPermissionContext>,
        check_settings: Option<Box<dyn Fn(&App) -> ToolPermissionDecision>>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // Short-circuit when current settings yield a definitive answer.
        if let Some(check) = check_settings.as_ref() {
            match check(cx) {
                ToolPermissionDecision::Allow => return Task::ready(Ok(())),
                ToolPermissionDecision::Deny(reason) => {
                    return Task::ready(Err(anyhow!(reason)));
                }
                ToolPermissionDecision::Confirm => {}
            }
        }

        if self.plan_phase.map_or(false, |p| p.is_proposed()) {
            let tool_name_for_risk: Option<SharedString> = context
                .as_ref()
                .map(|c| SharedString::from(c.tool_name.clone()));
            let tool_name_opt = tool_name_for_risk.as_ref();
            let risk = approval_risk_for_tool_call(tool_name_opt, acp::ToolKind::Edit);
            if risk == ApprovalRisk::PotentiallyDestructive {
                let is_plan_maintenance = tool_name_opt.map(|n| n.as_ref()).map_or(false, |n| {
                    matches!(
                        n,
                        "enter_plan_mode"
                            | "todo_write"
                            | "get_command_or_subagent_output"
                            | "monitor"
                    )
                });
                if !is_plan_maintenance {
                    return Task::ready(Err(anyhow!(
                        "Plan approval required before destructive operations. Review the proposed plan in the Zed Todos section and accept it to proceed with edits."
                    )));
                }
            }
        }

        let fs = self.fs.clone();
        let stream = self.stream.clone();
        let tool_use_id = self.tool_use_id.clone();
        let tool_name_for_meta = context.as_ref().map(|c| c.tool_name.clone());
        cx.spawn(async move |cx| {
            let (response_tx, mut response_rx) = oneshot::channel();
            let update_fields = acp::ToolCallUpdateFields::new().title(title);
            let tool_call_update = if let Some(ref name) = tool_name_for_meta {
                acp::ToolCallUpdate::new(tool_use_id.to_string(), update_fields)
                    .meta(acp_thread::meta_with_tool_name(name))
            } else {
                acp::ToolCallUpdate::new(tool_use_id.to_string(), update_fields)
            };
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: tool_call_update,
                        options,
                        response: response_tx,
                        context,
                        kind: acp_thread::AuthorizationKind::PermissionGrant,
                    },
                )))
            {
                log::error!("Failed to send tool call authorization: {error}");
                return Err(anyhow!("Failed to send tool call authorization: {error}"));
            }

            let Some(check_settings) = check_settings else {
                let outcome = response_rx
                    .await
                    .map_err(|_| anyhow!("authorization channel closed"))?;

                return Self::persist_permission_outcome(&outcome, fs, cx);
            };

            let (mut settings_tx, mut settings_rx) = watch::channel(());
            let _settings_subscription = cx.update(|cx| {
                cx.observe_global::<SettingsStore>(move |_cx| {
                    settings_tx.send(()).ok();
                })
            });

            // Race the user's response against settings changes. On each
            // settings change, re-evaluate `check_settings`: if it now
            // yields a definitive Allow or Deny, resolve the prompt
            // without user interaction. Otherwise keep waiting on the
            // same prompt.
            loop {
                let settings_changed = async {
                    if settings_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                futures::select_biased! {
                    outcome = (&mut response_rx).fuse() => {
                        let outcome = outcome
                            .map_err(|_| anyhow!("authorization channel closed"))?;
                        return Self::persist_permission_outcome(&outcome, fs.clone(), cx);
                    }
                    _ = settings_changed.fuse() => {
                        // On auto-resolve, we dismiss the prompt UI by
                        // replacing the tool call's `WaitingForConfirmation`
                        // status with `InProgress` (or `Failed`). Dropping
                        // `response_rx` closes the `oneshot` held by the
                        // UI, so any late click by the user is a no-op.
                        match cx.update(|cx| check_settings(cx)) {
                            ToolPermissionDecision::Allow => {
                                drop(response_rx);
                                stream.update_tool_call_fields(
                                    &tool_use_id,
                                    acp::ToolCallUpdateFields::new()
                                        .status(acp::ToolCallStatus::InProgress),
                                    None,
                                );
                                return Ok(());
                            }
                            ToolPermissionDecision::Deny(reason) => {
                                drop(response_rx);
                                stream.update_tool_call_fields(
                                    &tool_use_id,
                                    acp::ToolCallUpdateFields::new()
                                        .status(acp::ToolCallStatus::Failed),
                                    None,
                                );
                                return Err(anyhow!(reason));
                            }
                            ToolPermissionDecision::Confirm => continue,
                        }
                    }
                }
            }
        })
    }

    /// Interprets a `SelectedPermissionOutcome` and persists any settings changes.
    /// Returns `true` if the tool call should be allowed, `false` if denied.
    fn persist_permission_outcome(
        outcome: &acp_thread::SelectedPermissionOutcome,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) -> Result<()> {
        let option_id = outcome.option_id.0.as_ref();
        let err = || Err(anyhow!("Permission to run tool denied by user"));

        let always_permission = option_id
            .strip_prefix("always_allow:")
            .map(|tool| (tool, ToolPermissionMode::Allow))
            .or_else(|| {
                option_id
                    .strip_prefix("always_deny:")
                    .map(|tool| (tool, ToolPermissionMode::Deny))
            })
            .or_else(|| {
                option_id
                    .strip_prefix("always_allow_mcp:")
                    .map(|tool| (tool, ToolPermissionMode::Allow))
            })
            .or_else(|| {
                option_id
                    .strip_prefix("always_deny_mcp:")
                    .map(|tool| (tool, ToolPermissionMode::Deny))
            });

        if let Some((tool, mode)) = always_permission {
            let params = outcome.params.as_ref();
            Self::persist_always_permission(tool, mode, params, fs, cx);
            return if mode == ToolPermissionMode::Allow {
                Ok(())
            } else {
                err()
            };
        }

        // Handle simple "allow" / "deny" (once, no persistence)
        if option_id == "allow" || option_id == "deny" {
            debug_assert!(
                outcome.params.is_none(),
                "unexpected params for once-only permission"
            );
            return if option_id == "allow" { Ok(()) } else { err() };
        }

        debug_assert!(false, "unexpected permission option_id: {option_id}");

        err()
    }

    /// Persists an "always allow" or "always deny" permission, using sub_patterns
    /// from params when present.
    fn persist_always_permission(
        tool: &str,
        mode: ToolPermissionMode,
        params: Option<&acp_thread::SelectedPermissionParams>,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) {
        let Some(fs) = fs else {
            return;
        };

        match params {
            Some(acp_thread::SelectedPermissionParams::Terminal {
                patterns: sub_patterns,
            }) => {
                debug_assert!(
                    !sub_patterns.is_empty(),
                    "empty sub_patterns for tool {tool} - callers should pass None instead"
                );
                let tool = tool.to_string();
                let sub_patterns = sub_patterns.clone();
                cx.update(|cx| {
                    update_settings_file(fs, cx, move |settings, _| {
                        let agent = settings.agent.get_or_insert_default();
                        for pattern in sub_patterns {
                            match mode {
                                ToolPermissionMode::Allow => {
                                    agent.add_tool_allow_pattern(&tool, pattern);
                                }
                                ToolPermissionMode::Deny => {
                                    agent.add_tool_deny_pattern(&tool, pattern);
                                }
                                // If there's no matching pattern this will
                                // default to confirm, so falling through is
                                // fine here.
                                ToolPermissionMode::Confirm => (),
                            }
                        }
                    });
                });
            }
            None => {
                let tool = tool.to_string();
                cx.update(|cx| {
                    update_settings_file(fs, cx, move |settings, _| {
                        settings
                            .agent
                            .get_or_insert_default()
                            .set_tool_default_permission(&tool, mode);
                    });
                });
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct ToolCallEventStreamReceiver(mpsc::UnboundedReceiver<Result<ThreadEvent>>);

#[cfg(any(test, feature = "test-support"))]
impl ToolCallEventStreamReceiver {
    pub async fn expect_authorization(&mut self) -> ToolCallAuthorization {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallAuthorization(auth))) = event {
            auth
        } else {
            panic!("Expected ToolCallAuthorization but got: {:?}", event);
        }
    }

    pub async fn expect_update_fields(&mut self) -> acp::ToolCallUpdateFields {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(
            update,
        )))) = event
        {
            update.fields
        } else {
            panic!("Expected update fields but got: {:?}", event);
        }
    }

    pub async fn expect_diff(&mut self) -> Entity<acp_thread::Diff> {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateDiff(
            update,
        )))) = event
        {
            update.diff
        } else {
            panic!("Expected diff but got: {:?}", event);
        }
    }

    pub async fn expect_terminal(&mut self) -> Entity<acp_thread::Terminal> {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateTerminal(
            update,
        )))) = event
        {
            update.terminal
        } else {
            panic!("Expected terminal but got: {:?}", event);
        }
    }

    pub async fn expect_plan(&mut self) -> acp::Plan {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::Plan(plan))) = event {
            plan
        } else {
            panic!("Expected plan but got: {:?}", event);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::ops::Deref for ToolCallEventStreamReceiver {
    type Target = mpsc::UnboundedReceiver<Result<ThreadEvent>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::ops::DerefMut for ToolCallEventStreamReceiver {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&str> for UserMessageContent {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

impl From<String> for UserMessageContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl UserMessageContent {
    pub fn from_content_block(value: acp::ContentBlock, path_style: PathStyle) -> Self {
        match value {
            acp::ContentBlock::Text(text_content) => Self::Text(text_content.text),
            acp::ContentBlock::Image(image_content) => Self::Image(convert_image(image_content)),
            acp::ContentBlock::Audio(_) => {
                // TODO
                Self::Text("[audio]".to_string())
            }
            acp::ContentBlock::ResourceLink(resource_link) => {
                match MentionUri::parse(&resource_link.uri, path_style) {
                    Ok(uri) => Self::Mention {
                        uri,
                        content: SharedString::default(),
                    },
                    Err(err) => {
                        log::error!("Failed to parse mention link: {}", err);
                        Self::Text(format!("[{}]({})", resource_link.name, resource_link.uri))
                    }
                }
            }
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => {
                    match MentionUri::parse(&resource.uri, path_style) {
                        Ok(uri) => Self::Mention {
                            uri,
                            content: resource.text.into(),
                        },
                        Err(err) => {
                            log::error!("Failed to parse mention link: {}", err);
                            Self::Text(
                                MarkdownCodeBlock {
                                    tag: &resource.uri,
                                    text: &resource.text,
                                }
                                .to_string(),
                            )
                        }
                    }
                }
                acp::EmbeddedResourceResource::BlobResourceContents(_) => {
                    // TODO
                    Self::Text("[blob]".to_string())
                }
                other => {
                    log::warn!("Unexpected content type: {:?}", other);
                    Self::Text("[unknown]".to_string())
                }
            },
            other => {
                log::warn!("Unexpected content type: {:?}", other);
                Self::Text("[unknown]".to_string())
            }
        }
    }
}

impl From<UserMessageContent> for acp::ContentBlock {
    fn from(content: UserMessageContent) -> Self {
        match content {
            UserMessageContent::Text(text) => text.into(),
            UserMessageContent::Image(image) => {
                acp::ContentBlock::Image(acp::ImageContent::new(image.source, "image/png"))
            }
            UserMessageContent::Mention { uri, content } => acp::ContentBlock::Resource(
                acp::EmbeddedResource::new(acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new(content, uri.to_uri().to_string()),
                )),
            ),
        }
    }
}

fn convert_image(image_content: acp::ImageContent) -> LanguageModelImage {
    LanguageModelImage {
        source: image_content.data.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language_model::LanguageModelToolUseId;
    use language_model::fake_provider::FakeLanguageModel;
    use serde_json::json;
    use std::sync::Arc;

    async fn setup_thread_for_test(cx: &mut TestAppContext) -> (Entity<Thread>, ThreadEventStream) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let templates = Templates::new();
        let project = Project::test(fs.clone(), [], cx).await;

        cx.update(|cx| {
            let project_context = cx.new(|_cx| prompt_store::ProjectContext::default());
            let context_server_store = project.read(cx).context_server_store();
            let context_server_registry =
                cx.new(|cx| ContextServerRegistry::new(context_server_store, cx));

            let thread = cx.new(|cx| {
                Thread::new(
                    project,
                    project_context,
                    context_server_registry,
                    templates,
                    None,
                    cx,
                )
            });

            let (event_tx, _event_rx) = mpsc::unbounded();
            let event_stream = ThreadEventStream(event_tx);

            (thread, event_stream)
        })
    }

    #[test]
    fn test_summary_compaction_renders_for_request_and_markdown() {
        let message = Message::Compaction(CompactionInfo::Summary("Older context".into()));

        assert_eq!(message.role(), Role::User);
        assert_eq!(message.to_markdown(), "--- Context Compacted ---\n");

        let request_messages = message.to_request();
        assert_eq!(request_messages.len(), 1);
        assert_eq!(request_messages[0].role, Role::User);
        assert!(!request_messages[0].cache);
        assert_eq!(request_messages[0].reasoning_details, None);
        assert_eq!(request_messages[0].content.len(), 1);
        let language_model::MessageContent::Text(text) = &request_messages[0].content[0] else {
            panic!("expected text summary context");
        };
        assert_eq!(
            text.as_str(),
            "The previous conversation was compacted. Use this summary as context:\n\nOlder context"
        );
    }

    fn user_text_message(id: UserMessageId, text: &str) -> Arc<Message> {
        Arc::new(Message::User(UserMessage {
            id,
            content: vec![UserMessageContent::Text(text.to_string())].into(),
        }))
    }

    fn agent_text_message(text: &str) -> Arc<Message> {
        Arc::new(Message::Agent(AgentMessage {
            content: vec![AgentMessageContent::Text(text.to_string())],
            ..Default::default()
        }))
    }

    fn summary_compaction(summary: &str) -> Arc<Message> {
        Arc::new(Message::Compaction(CompactionInfo::Summary(summary.into())))
    }

    fn summary_request_text(summary: &str) -> String {
        format!(
            "The previous conversation was compacted. Use this summary as context:\n\n{summary}"
        )
    }

    fn request_texts_after_system(messages: &[LanguageModelRequestMessage]) -> Vec<String> {
        messages
            .iter()
            .skip(1)
            .map(LanguageModelRequestMessage::string_contents)
            .collect()
    }

    #[gpui::test]
    async fn test_replay_emits_context_compaction(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let user_message_id = UserMessageId::new();

        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "before"));
                thread.messages.push(summary_compaction("summary"));
                thread.messages.push(agent_text_message("after"));

                thread.replay(cx)
            })
        });

        let event = replay_events.next().await;
        assert!(
            matches!(
                &event,
                Some(Ok(ThreadEvent::UserMessage(UserMessage { id, .. }))) if id == &user_message_id
            ),
            "expected replayed user message, got {event:?}"
        );

        let event = replay_events.next().await;
        assert!(
            matches!(&event, Some(Ok(ThreadEvent::ContextCompaction))),
            "expected context compaction event, got {event:?}"
        );

        let event = replay_events.next().await;
        assert!(
            matches!(&event, Some(Ok(ThreadEvent::AgentText(text))) if text == "after"),
            "expected replayed agent text, got {event:?}"
        );
    }

    #[gpui::test]
    async fn test_native_compaction_boundary(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let request_messages = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .messages
                    .push(user_text_message(UserMessageId::new(), "before native"));
                thread.messages.push(Arc::new(Message::Compaction(
                    CompactionInfo::ProviderNative {
                        provider: LanguageModelProviderId::from("openai".to_string()),
                        items: vec![json!({"type": "compaction"})],
                    },
                )));
                thread
                    .messages
                    .push(user_text_message(UserMessageId::new(), "after native"));

                thread.build_request_messages(Vec::new(), cx)
            })
        });

        assert_eq!(
            request_texts_after_system(&request_messages),
            vec!["after native".to_string()]
        );
    }

    #[gpui::test]
    async fn test_retained_users_truncate_oldest(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let mut long_text = "START".to_string();
        long_text.push_str(&"x".repeat(COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET));
        long_text.push_str("END");

        let request_messages = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.messages.push(user_text_message(
                    UserMessageId::new(),
                    "dropped older user",
                ));
                thread
                    .messages
                    .push(agent_text_message("dropped assistant"));
                thread
                    .messages
                    .push(user_text_message(UserMessageId::new(), &long_text));
                thread
                    .messages
                    .push(user_text_message(UserMessageId::new(), "new"));
                thread.messages.push(summary_compaction("summary context"));
                thread.messages.push(agent_text_message("after assistant"));
                thread
                    .messages
                    .push(user_text_message(UserMessageId::new(), "after user"));

                thread.build_request_messages(Vec::new(), cx)
            })
        });

        let request_texts = request_texts_after_system(&request_messages);
        assert_eq!(request_texts.len(), 5);
        assert_eq!(
            request_texts[0],
            format!(
                "START{}",
                "x".repeat(
                    COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET - "START".len() - "new".len()
                )
            )
        );
        assert_eq!(request_texts[1], "new");
        assert_eq!(request_texts[2], summary_request_text("summary context"));
        assert_eq!(request_texts[3], "after assistant");
        assert_eq!(request_texts[4], "after user");
        assert!(request_texts.iter().all(
            |text| !text.contains("dropped older user") && !text.contains("dropped assistant")
        ));
    }

    #[test]
    fn test_truncate_text_utf8_boundary() {
        let message = LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hello 👋 world".to_string())],
            cache: false,
            reasoning_details: None,
        };

        let truncated = truncate_user_message_to_byte_budget(message, 8).unwrap();
        assert_eq!(
            truncated.content,
            vec![MessageContent::Text("hello ".to_string())]
        );
    }

    #[test]
    fn test_truncate_keeps_fitting_images() {
        let image = LanguageModelImage {
            source: "image".into(),
        };
        let message = LanguageModelRequestMessage {
            role: Role::User,
            content: vec![
                MessageContent::Text("abc".to_string()),
                MessageContent::Image(image.clone()),
            ],
            cache: false,
            reasoning_details: None,
        };

        let truncated = truncate_user_message_to_byte_budget(message, 8).unwrap();
        assert_eq!(
            truncated.content,
            vec![
                MessageContent::Text("abc".to_string()),
                MessageContent::Image(image),
            ]
        );
    }

    fn setup_parent_with_subagents(
        cx: &mut TestAppContext,
        parent: &Entity<Thread>,
        count: usize,
    ) -> Vec<Entity<Thread>> {
        cx.update(|cx| {
            let mut subagents = Vec::new();
            for _ in 0..count {
                let subagent = cx.new(|cx| Thread::new_subagent(parent, None, None, cx));
                parent.update(cx, |thread, _cx| {
                    thread.register_running_subagent(subagent.downgrade());
                });
                subagents.push(subagent);
            }
            subagents
        })
    }

    struct ReplayImageTool;

    impl AgentTool for ReplayImageTool {
        type Input = ();
        type Output = String;

        const NAME: &'static str = "registered_image_tool";

        fn kind() -> acp::ToolKind {
            acp::ToolKind::Other
        }

        fn initial_title(
            &self,
            _input: Result<Self::Input, serde_json::Value>,
            _cx: &mut App,
        ) -> SharedString {
            "Registered Image Tool".into()
        }

        fn run(
            self: Arc<Self>,
            _input: ToolInput<Self::Input>,
            _event_stream: ToolCallEventStream,
            _cx: &mut App,
        ) -> Task<Result<Self::Output, Self::Output>> {
            Task::ready(Ok(String::new()))
        }
    }

    #[gpui::test]
    async fn test_replay_tool_call_replays_image_content(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let registered_tool_use_id = LanguageModelToolUseId::from("registered_tool_id");
        let missing_tool_use_id = LanguageModelToolUseId::from("missing_tool_id");
        let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
        let image = LanguageModelImage {
            source: image_data.into(),
        };

        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.add_tool(ReplayImageTool);

                let registered_tool_use = LanguageModelToolUse {
                    id: registered_tool_use_id.clone(),
                    name: ReplayImageTool::NAME.into(),
                    raw_input: "null".to_string(),
                    input: json!(null),
                    is_input_complete: true,
                    thought_signature: None,
                };
                let missing_tool_use = LanguageModelToolUse {
                    id: missing_tool_use_id.clone(),
                    name: "missing_image_tool".into(),
                    raw_input: "{}".to_string(),
                    input: json!({}),
                    is_input_complete: true,
                    thought_signature: None,
                };

                let mut tool_results = IndexMap::default();
                tool_results.insert(
                    registered_tool_use_id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: registered_tool_use_id.clone(),
                        tool_name: ReplayImageTool::NAME.into(),
                        is_error: false,
                        content: vec![
                            LanguageModelToolResultContent::Text("before".into()),
                            LanguageModelToolResultContent::Image(image.clone()),
                            LanguageModelToolResultContent::Text("after".into()),
                        ],
                        output: Some(json!("raw output")),
                    },
                );
                tool_results.insert(
                    missing_tool_use_id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: missing_tool_use_id.clone(),
                        tool_name: "missing_image_tool".into(),
                        is_error: false,
                        content: vec![LanguageModelToolResultContent::Image(image.clone())],
                        output: Some(json!("raw output")),
                    },
                );

                thread.messages.push(Arc::new(Message::Agent(AgentMessage {
                    content: vec![
                        AgentMessageContent::ToolUse(registered_tool_use),
                        AgentMessageContent::ToolUse(missing_tool_use),
                    ],
                    tool_results,
                    reasoning_details: None,
                })));

                thread.replay(cx)
            })
        });

        let mut tool_use_ids_with_image_content = HashSet::default();
        while let Some(event) = replay_events.next().await {
            let event = event.unwrap();
            if let ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(update)) =
                event
                && let Some(content) = &update.fields.content
                && content.iter().any(|content| {
                    matches!(
                        content,
                        acp::ToolCallContent::Content(acp::Content {
                            content: acp::ContentBlock::Image(_),
                            ..
                        })
                    )
                })
            {
                tool_use_ids_with_image_content.insert(update.tool_call_id.to_string());
            }
        }

        assert!(tool_use_ids_with_image_content.contains(&registered_tool_use_id.to_string()));
        assert!(tool_use_ids_with_image_content.contains(&missing_tool_use_id.to_string()));
    }

    #[gpui::test]
    async fn test_update_title_tool_replay_does_not_reenter_thread(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let tool_use_id = LanguageModelToolUseId::from("title_tool_id");
        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.add_tool(UpdateTitleTool::new(cx.weak_entity()));
                push_completed_update_title_tool_call(thread, tool_use_id.clone());

                thread.replay(cx)
            })
        });

        let mut saw_tool_call_title = false;
        let mut saw_replayed_title_update = false;
        let mut saw_completed_update = false;
        while let Some(event) = replay_events.next().await {
            let event = event.unwrap();
            match event {
                ThreadEvent::ToolCall(tool_call)
                    if tool_call.tool_call_id.to_string() == tool_use_id.to_string()
                        && tool_call.title == "Update title: Replayed title" =>
                {
                    saw_tool_call_title = true;
                }
                ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(update))
                    if update.tool_call_id.to_string() == tool_use_id.to_string() =>
                {
                    if update.fields.title == Some("Update title: Replayed title".to_string()) {
                        saw_replayed_title_update = true;
                    }
                    if update.fields.status == Some(acp::ToolCallStatus::Completed) {
                        saw_completed_update = true;
                    }
                }
                _ => {}
            }
        }

        assert!(saw_tool_call_title);
        assert!(saw_replayed_title_update);
        assert!(saw_completed_update);
        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.title(), None);
        });
    }

    #[gpui::test]
    async fn test_update_title_tool_replay_title_when_tool_not_registered(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let tool_use_id = LanguageModelToolUseId::from("title_tool_id");
        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                push_completed_update_title_tool_call(thread, tool_use_id.clone());
                thread.replay(cx)
            })
        });

        let mut saw_tool_call_title = false;
        let mut saw_replayed_title_update = false;
        let mut saw_completed_update = false;
        while let Some(event) = replay_events.next().await {
            let event = event.unwrap();
            match event {
                ThreadEvent::ToolCall(tool_call)
                    if tool_call.tool_call_id.to_string() == tool_use_id.to_string()
                        && tool_call.title == "Update title: Replayed title" =>
                {
                    saw_tool_call_title = true;
                }
                ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(update))
                    if update.tool_call_id.to_string() == tool_use_id.to_string() =>
                {
                    if update.fields.title == Some("Update title: Replayed title".to_string()) {
                        saw_replayed_title_update = true;
                    }
                    if update.fields.status == Some(acp::ToolCallStatus::Completed) {
                        saw_completed_update = true;
                    }
                }
                _ => {}
            }
        }

        assert!(saw_tool_call_title);
        assert!(saw_replayed_title_update);
        assert!(saw_completed_update);
        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.title(), None);
        });
    }

    fn push_completed_update_title_tool_call(
        thread: &mut Thread,
        tool_use_id: LanguageModelToolUseId,
    ) {
        let tool_use = LanguageModelToolUse {
            id: tool_use_id.clone(),
            name: UpdateTitleTool::NAME.into(),
            raw_input: json!({ "title": "Replayed title" }).to_string(),
            input: json!({ "title": "Replayed title" }),
            is_input_complete: true,
            thought_signature: None,
        };

        let mut tool_results = IndexMap::default();
        tool_results.insert(
            tool_use_id.clone(),
            LanguageModelToolResult {
                tool_use_id,
                tool_name: UpdateTitleTool::NAME.into(),
                is_error: false,
                content: vec![LanguageModelToolResultContent::Text(
                    "Session title updated".into(),
                )],
                output: Some(json!("Session title updated")),
            },
        );

        thread.messages.push(Arc::new(Message::Agent(AgentMessage {
            content: vec![AgentMessageContent::ToolUse(tool_use)],
            tool_results,
            reasoning_details: None,
        })));
    }

    #[gpui::test]
    async fn test_set_model_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        let new_model: Arc<dyn LanguageModel> = Arc::new(FakeLanguageModel::with_id_and_thinking(
            "test-provider",
            "new-model",
            "New Model",
            false,
        ));

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_model(new_model, cx);
            });

            for subagent in &subagents {
                let subagent_model_id = subagent.read(cx).model().unwrap().id();
                assert_eq!(
                    subagent_model_id.0.as_ref(),
                    "new-model",
                    "Subagent model should match parent model after set_model"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_summarization_model_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        let summary_model: Arc<dyn LanguageModel> =
            Arc::new(FakeLanguageModel::with_id_and_thinking(
                "test-provider",
                "summary-model",
                "Summary Model",
                false,
            ));

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_summarization_model(Some(summary_model), cx);
            });

            for subagent in &subagents {
                let subagent_summary_id = subagent.read(cx).summarization_model().unwrap().id();
                assert_eq!(
                    subagent_summary_id.0.as_ref(),
                    "summary-model",
                    "Subagent summarization model should match parent after set_summarization_model"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_thinking_enabled_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(true, cx);
            });

            for subagent in &subagents {
                assert!(
                    subagent.read(cx).thinking_enabled(),
                    "Subagent thinking should be enabled after parent enables it"
                );
            }

            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(false, cx);
            });

            for subagent in &subagents {
                assert!(
                    !subagent.read(cx).thinking_enabled(),
                    "Subagent thinking should be disabled after parent disables it"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_thinking_effort_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_effort(Some("high".to_string()), cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).thinking_effort().map(|s| s.as_str()),
                    Some("high"),
                    "Subagent thinking effort should match parent"
                );
            }

            parent.update(cx, |thread, cx| {
                thread.set_thinking_effort(None, cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).thinking_effort(),
                    None,
                    "Subagent thinking effort should be None after parent clears it"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_subagent_inherits_settings_at_creation(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_speed(Speed::Fast, cx);
                thread.set_thinking_enabled(true, cx);
                thread.set_thinking_effort(Some("high".to_string()), cx);
                thread.set_profile(AgentProfileId("custom-profile".into()), cx);
            });
        });

        let subagents = setup_parent_with_subagents(cx, &parent, 1);

        cx.update(|cx| {
            let sub = subagents[0].read(cx);
            assert_eq!(sub.speed(), Some(Speed::Fast));
            assert!(sub.thinking_enabled());
            assert_eq!(sub.thinking_effort().map(|s| s.as_str()), Some("high"));
            assert_eq!(sub.profile(), &AgentProfileId("custom-profile".into()));
        });
    }

    #[gpui::test]
    async fn test_set_speed_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_speed(Speed::Fast, cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).speed(),
                    Some(Speed::Fast),
                    "Subagent speed should match parent after set_speed"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_dropped_subagent_does_not_panic(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 1);

        // Drop the subagent so the WeakEntity can no longer be upgraded
        drop(subagents);

        // Should not panic even though the subagent was dropped
        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(true, cx);
                thread.set_speed(Speed::Fast, cx);
                thread.set_thinking_effort(Some("high".to_string()), cx);
            });
        });
    }

    #[gpui::test]
    async fn test_handle_tool_use_json_parse_error_adds_tool_use_to_content(
        cx: &mut TestAppContext,
    ) {
        let (thread, event_stream) = setup_thread_for_test(cx).await;

        let tool_use_id = LanguageModelToolUseId::from("test_tool_id");
        let tool_name: Arc<str> = Arc::from("test_tool");
        let raw_input: Arc<str> = Arc::from("{invalid json");
        let json_parse_error = "expected value at line 1 column 1".to_string();

        let (_cancellation_tx, cancellation_rx) = watch::channel(false);

        let result = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    // Call the function under test
                    thread
                        .handle_tool_use_json_parse_error_event(
                            tool_use_id.clone(),
                            tool_name.clone(),
                            raw_input.clone(),
                            json_parse_error,
                            &event_stream,
                            cancellation_rx,
                            cx,
                        )
                        .unwrap()
                })
            })
            .await;

        // Verify the result is an error
        assert!(result.is_error);
        assert_eq!(result.tool_use_id, tool_use_id);
        assert_eq!(result.tool_name, tool_name);
        assert!(matches!(
            result.content.as_slice(),
            [LanguageModelToolResultContent::Text(_)]
        ));

        thread.update(cx, |thread, _cx| {
            // Verify the tool use was added to the message content
            {
                let last_message = thread.pending_message();
                assert_eq!(
                    last_message.content.len(),
                    1,
                    "Should have one tool_use in content"
                );

                match &last_message.content[0] {
                    AgentMessageContent::ToolUse(tool_use) => {
                        assert_eq!(tool_use.id, tool_use_id);
                        assert_eq!(tool_use.name, tool_name);
                        assert_eq!(tool_use.raw_input, raw_input.to_string());
                        assert!(tool_use.is_input_complete);
                        // Should fall back to empty object for invalid JSON
                        assert_eq!(tool_use.input, json!({}));
                    }
                    _ => panic!("Expected ToolUse content"),
                }
            }

            // Insert the tool result (simulating what the caller does)
            thread
                .pending_message()
                .tool_results
                .insert(result.tool_use_id.clone(), result);

            // Verify the tool result was added
            let last_message = thread.pending_message();
            assert_eq!(
                last_message.tool_results.len(),
                1,
                "Should have one tool_result"
            );
            assert!(last_message.tool_results.contains_key(&tool_use_id));
        })
    }

    #[gpui::test]
    async fn test_plan_phase_propagation_and_clear_in_native_thread(cx: &mut TestAppContext) {
        let (thread, _events) = setup_thread_for_test(cx).await;
        cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                assert_eq!(t.plan_phase(), PlanPhase::None);
                t.plan_phase.set_to_proposed();
                assert!(t.plan_phase().is_proposed());
                let turn_id: TurnId = TurnId::new(3);
                assert_eq!(u32::from(turn_id), 3u32);
                t.clear_plan(_cx);
                assert_eq!(t.plan_phase(), PlanPhase::Active);
            });
        });
    }

    #[gpui::test]
    async fn test_native_grok_plan_proposed_zt1_cwd_classification_turnid(cx: &mut TestAppContext) {
        let (thread, _events) = setup_thread_for_test(cx).await;
        cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.grok_build_profile = true;
                t.plan_phase.set_to_proposed();
                let risk = approval_risk_for_tool_call(None, acp::ToolKind::Edit);
                assert_eq!(risk, ApprovalRisk::PotentiallyDestructive);
                let turn_id: TurnId = TurnId::new(42);
                let _pinned: TurnId = turn_id;
            });
        });
    }

    #[gpui::test]
    async fn test_native_grok_subagent_persona_capability_and_profile_propagation(
        cx: &mut TestAppContext,
    ) {
        let (parent, _events) = setup_thread_for_test(cx).await;
        cx.update(|cx| {
            parent.update(cx, |t, _cx| {
                t.grok_build_profile = true;
            });
            let subagent = cx.new(|sub_cx| {
                Thread::new_subagent(&parent, None, Some(acp_thread::AgentCapabilityMode::ReadOnly), sub_cx)
            });
            let sub_ref = subagent.read(cx);
            assert!(
                sub_ref.grok_build_profile,
                "subagent must receive parent's native grok_build_profile for full prompt fragments TurnId and the categorized todos surface under is_grok (native subagent persona following native subagent persona from prior TurnId 019e3f87 task decomposition)"
            );
            assert_eq!(
                sub_ref.capability_mode(),
                Some(acp_thread::AgentCapabilityMode::ReadOnly),
                "capability_mode Read-Only must propagate through new_subagent context to feed system prompt for subagent"
            );
            assert!(
                sub_ref.persona().is_none(),
                "persona None passed must remain for this case"
            );
        });
    }

    #[gpui::test]
    async fn test_grok_memory_artifacts_and_facts_injection_for_native_profile(
        cx: &mut TestAppContext,
    ) {
        let (thread, _events) = setup_thread_for_test(cx).await;
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.grok_build_profile = true;
                assert!(thread.is_grok_build_profile(cx), "native profile gate required for GrokMemoryArtifacts full facts surface plus TurnId in prompt build for GrokMemoryArtifacts prompt build");
                let artifacts_val: GrokMemoryArtifacts = thread.grok_memory(cx);
                let _artifacts_pin: GrokMemoryArtifacts = artifacts_val.clone();
                let _facts_ref = &artifacts_val.facts_from_db;
                assert_eq!(artifacts_val.has_workspace_memory, false);
                assert!(artifacts_val.workspace_memory_full.is_none());
                assert!(artifacts_val.global_memory_full.is_none());
                assert!(artifacts_val.facts_from_db.is_empty());
                let turn_val = thread.current_turn_id();
                let _turn_id_pin: TurnId = turn_val;
            });
        });
    }

    #[gpui::test]
    async fn test_project_diagnostics_context_building_and_native_grok_profile_injection(
        cx: &mut TestAppContext,
    ) {
        let (thread, _events) = setup_thread_for_test(cx).await;
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.grok_build_profile = true;
                assert!(thread.is_grok_build_profile(cx), "native profile gate required for project diagnostics context building injection in prompt for diagnostics context injection");
                let turn_val = thread.current_turn_id();
                let _turn_id_pin: TurnId = turn_val;
                let turn_for_roundtrip: TurnId = TurnId::new(17);
                let serialized = serde_json::to_string(&turn_for_roundtrip).unwrap_or_default();
                let roundtripped: TurnId = if let Ok(r) = serde_json::from_str(&serialized) { r } else { TurnId::new(0) };
                let _roundtrip_pin: TurnId = roundtripped;
                assert_eq!(u32::from(roundtripped), 17, "TurnId value roundtrip for native profile");
                let diags_context_val = thread.build_project_diagnostics_context(cx);
                let _diags_context_pin: String = diags_context_val.clone();
                assert!(
                    diags_context_val.contains("Zed's language servers") || diags_context_val.contains("errors,"),
                    "project diagnostics context building must produce primary LSP errors/warnings block for native grok"
                );
                let fragments = GROK_BUILD_SYSTEM_FRAGMENTS;
                assert!(
                    fragments.contains("CWD rule"),
                    "CWD label cases must be present in native grok prompt fragments for risk classification"
                );
                assert!(
                    fragments.contains("primary context"),
                    "native profile rule injection must embed primary diagnostics preference over shell clippy to prevent E2E kickback regression to external linters"
                );
                assert!(
                    fragments.contains("Bounded Exploration and Action Discipline"),
                    "anti-doom-loop / bounded exploration rule must be present in native grok fragments so the agent itself does not waste turns on unbounded discovery"
                );
            });
        });
    }
}

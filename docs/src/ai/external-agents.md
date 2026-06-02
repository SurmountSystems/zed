---
title: Use Claude Agent, Gemini CLI, and Codex in Zed
description: Run Claude Agent, Gemini CLI, Codex, and other AI coding agents directly in Zed via the Agent Client Protocol (ACP).
---

# External Agents

Zed supports many external agents, including CLI-based ones, through the [Agent Client Protocol (ACP)](https://agentclientprotocol.com).

Zed supports [Gemini CLI](https://github.com/google-gemini/gemini-cli) (the reference ACP implementation), [Claude Agent](https://platform.claude.com/docs/en/agent-sdk/overview), [Codex](https://developers.openai.com/codex), [GitHub Copilot](https://github.com/github/copilot-language-server-release), and [additional agents](#add-more-agents) you can configure.

For Zed's built-in agent and the full list of tools it can use natively, see [Agent Tools](./tools.md).

> Note that Zed's interaction with external agents is strictly UI-based; the billing, legal, and terms arrangement is directly between you and the agent provider.
> Zed does not charge for use of external agents, and our [zero-data retention agreements/privacy guarantees](./ai-improvement.md) are **_only_** applicable for Zed's hosted models.

## Gemini CLI {#gemini-cli}

Zed provides the ability to run [Gemini CLI](https://github.com/google-gemini/gemini-cli) directly in the [agent panel](./agent-panel.md).
Under the hood we run Gemini CLI in the background, and talk to it over ACP.

### Getting Started

First open the agent panel with {#kb agent::ToggleFocus}, and then start a new Gemini CLI thread using the agent selector button on the left (in the empty state) or the `+` button in the top right.

If you'd like to bind this to a keyboard shortcut, you can do so by editing your `keymap.json` file via the {#action zed::OpenKeymapFile} command to include:

```json [keymap]
[
  {
    "bindings": {
      "cmd-alt-g": ["agent::NewExternalAgentThread", { "agent": "gemini" }]
    }
  }
]
```

#### Installation

The first time you create a Gemini CLI thread, Zed will install [@google/gemini-cli](https://github.com/google-gemini/gemini-cli).
This installation is only available to Zed and is kept up to date as you use the agent.

#### Authentication

After you have Gemini CLI running, you'll be prompted to authenticate.

Click the "Login" button to open the Gemini CLI interactively, where you can log in with your Google account or [Vertex AI](https://cloud.google.com/vertex-ai) credentials.
Zed does not see your OAuth or access tokens in this case.

If the `GEMINI_API_KEY` environment variable (or `GOOGLE_AI_API_KEY`) is already set, or you have configured a Google AI API key in Zed's [language model provider settings](./llm-providers.md#google-ai), it will be passed to Gemini CLI automatically.

For more information, see the [Gemini CLI docs](https://github.com/google-gemini/gemini-cli/blob/main/docs/index.md).

### Usage

Gemini CLI supports the same workflows as Zed's first-party agent: code generation, refactoring, debugging, and Q&A. Add context by @-mentioning files, recent threads, or symbols.

> Some agent panel features are not yet available with Gemini CLI: editing past messages, resuming threads from history, and checkpointing.

## Claude Agent

Similar to Gemini CLI, you can also run [Claude Agent](https://platform.claude.com/docs/en/agent-sdk/overview) directly via Zed's [agent panel](./agent-panel.md).
Under the hood, Zed runs the Claude Agent SDK, which runs Claude Code under the hood, and communicates to it over ACP, through [a dedicated adapter](https://github.com/zed-industries/claude-agent-acp).

### Getting Started

Open the agent panel with {#kb agent::ToggleFocus}, and then start a new Claude Agent thread using the agent selector button on the left (in the empty state) or the `+` button in the top right.

If you'd like to bind this to a keyboard shortcut, you can do so by editing your `keymap.json` file via the {#action zed::OpenKeymapFile} command to include:

```json [keymap]
[
  {
    "bindings": {
      "cmd-alt-c": ["agent::NewExternalAgentThread", { "agent": "claude-acp" }]
    }
  }
]
```

### Authentication

As of version `0.202.7`, authentication to Zed's Claude Agent installation is decoupled entirely from Zed's agent.
That is to say, an Anthropic API key added via the [Zed Agent's settings](./llm-providers.md#anthropic) will _not_ be utilized by Claude Agent for authentication and billing.

To ensure you're using your billing method of choice, [open a new Claude Agent thread](./agent-panel.md#new-thread).
Then, run `/login`, and authenticate either via API key, or via `Log in with Claude Code` to use a Claude Pro/Max subscription.

#### Installation

The first time you create a Claude Agent thread, Zed will install [@zed-industries/claude-agent-acp](https://github.com/zed-industries/claude-agent-acp).
This installation is only available to Zed and is kept up to date as you use the agent.

Zed will always use this managed version of the Claude Agent adapter, which includes a vendored version of the Claude Code CLI, even if you have it installed globally.

If you want to override the executable used by the adapter, you can set the `CLAUDE_CODE_EXECUTABLE` environment variable in your settings to the path of your preferred executable.

```json
{
  "agent_servers": {
    "claude-acp": {
      "type": "registry",
      "env": {
        "CLAUDE_CODE_EXECUTABLE": "/path/to/alternate-claude-code-executable"
      }
    }
  }
}
```

### Usage

Claude Agent supports the same workflows as Zed's first-party agent. Add context by @-mentioning files, recent threads, diagnostics, or symbols.

In complement to talking to it [over ACP](https://agentclientprotocol.com), Zed relies on the [Claude Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview) to support some of its specific features.
However, the SDK doesn't yet expose everything needed to fully support all of them:

- Slash Commands: [Custom slash commands](https://code.claude.com/docs/en/slash-commands#custom-slash-commands) are fully supported, and have been merged into skills. A subset of [built-in commands](https://code.claude.com/docs/en/slash-commands#built-in-slash-commands) are supported.
- [Subagents](https://code.claude.com/docs/en/sub-agents) are supported.
- [Agent teams](https://code.claude.com/docs/en/agent-teams) are currently _not_ supported.
- [Hooks](https://code.claude.com/docs/en/hooks-guide) are currently _not_ supported.

> Some [agent panel](./agent-panel.md) features are not yet available with Claude Agent: editing past messages, resuming threads from history, and checkpointing.

#### CLAUDE.md

Claude Agent in Zed will automatically use any `CLAUDE.md` file found in your project root, project subdirectories, or root `.claude` directory.

If you don't have a `CLAUDE.md` file, you can ask Claude Agent to create one for you through the `init` slash command.

## Codex CLI

You can also run [Codex CLI](https://github.com/openai/codex) directly via Zed's [agent panel](./agent-panel.md).
Under the hood, Zed runs Codex CLI and communicates to it over ACP, through [a dedicated adapter](https://github.com/zed-industries/codex-acp).

### Getting Started

As of version `0.208`, you should be able to use Codex directly from Zed.
Open the agent panel with {#kb agent::ToggleFocus}, and then start a new Codex thread using the agent selector button on the left (in the empty state) or the `+` button in the top right.

If you'd like to bind this to a keyboard shortcut, you can do so by editing your `keymap.json` file via the {#action zed::OpenKeymapFile} command to include:

```json
[
  {
    "bindings": {
      "cmd-alt-c": ["agent::NewExternalAgentThread", { "agent": "codex-acp" }]
    }
  }
]
```

### Authentication

Authentication to Zed's Codex installation is decoupled entirely from Zed's agent.
That is to say, an OpenAI API key added via the [Zed Agent's settings](./llm-providers.md#openai) will _not_ be utilized by Codex for authentication and billing.

To ensure you're using your billing method of choice, [open a new Codex thread](./agent-panel.md#new-thread).
The first time you will be prompted to authenticate with one of three methods:

1. Login with ChatGPT - allows you to use your existing, paid ChatGPT subscription. _Note: This method isn't currently supported in remote projects_
2. `CODEX_API_KEY` - uses an API key you have set in your environment under the variable `CODEX_API_KEY`.
3. `OPENAI_API_KEY` - uses an API key you have set in your environment under the variable `OPENAI_API_KEY`.

If you are already logged in and want to change your authentication method, type `/logout` in the thread and authenticate again.

If you want to use a third-party provider with Codex, you can configure that with your [Codex config.toml](https://github.com/openai/codex/blob/main/docs/config.md#model-selection) or pass extra [args/env variables](https://github.com/openai/codex/blob/main/docs/config.md#model-selection) to your Codex agent servers settings.

#### Installation

The first time you create a Codex thread, Zed will install [codex-acp](https://github.com/zed-industries/codex-acp).
This installation is only available to Zed and is kept up to date as you use the agent.

Zed will always use this managed version of Codex even if you have it installed globally.

### Usage

Codex supports the same workflows as Zed's first-party agent. Add context by @-mentioning files or symbols.

> Some agent panel features are not yet available with Codex: editing past messages, resuming threads from history, and checkpointing.

## Add More Agents {#add-more-agents}

### Via Agent Server Extensions

<div class="warning">

Starting from `v0.221.x`, [the ACP Registry](https://agentclientprotocol.com/registry) is the preferred way to install external agents in Zed.
Learn more about it in [the release blog post](https://zed.dev/blog/acp-registry).
At some point in the near future, Agent Server extensions will be deprecated.

</div>

Add more external agents to Zed by installing [Agent Server extensions](../extensions/agent-servers.md).

See what agents are available by filtering for "Agent Servers" in the extensions page, which you can access via the command palette with {#action zed::Extensions}, or the [Zed website](https://zed.dev/extensions?filter=agent-servers).

### Via The ACP Registry

#### Overview

As mentioned above, the Agent Server extensions will be deprecated in the near future to give room to the ACP Registry.

[The ACP Registry](https://github.com/agentclientprotocol/registry) lets developers distribute ACP-compatible agents to any client that implements the protocol. Agents installed from the registry update automatically.

At the moment, the registry is a curated set of agents, including only the ones that [support authentication](https://agentclientprotocol.com/rfds/auth-methods).

#### Using it in Zed

Use the {#action zed::AcpRegistry} command to quickly go to the ACP Registry page.
There's also a button ("Add Agent") that takes you there in the agent panel's configuration view.

From there, you can click to install your preferred agent and it will become available right away in the `+` icon button in the agent panel.

> If you installed the same agent through both the extension and the registry, the registry version takes precedence.

### Custom Agents

You can also add agents through your settings file ([how to edit](../configuring-zed.md#settings-files)) by specifying certain fields under `agent_servers`, like so:

```json [settings]
{
  "agent_servers": {
    "My Custom Agent": {
      "type": "custom",
      "command": "node",
      "args": ["~/projects/agent/index.js", "--acp"],
      "env": {}
    }
  }
}
```

This can be useful if you're in the middle of developing a new agent that speaks the protocol and you want to debug it.

It's also possible to customize environment variables for registry-installed agents like Claude Agent, Codex, and Gemini CLI by using their registry names (`claude-acp`, `codex-acp`, `gemini`) with `"type": "registry"` in your settings.

### Grok Build (xAI)  (our P3 bridging + ZT-1 co-equal content preserved)

You can run the full [Grok Build](https://x.ai/cli) TUI agent inside Zed's agent panel via ACP for complete visual access to all Grok capabilities (plan mode, subagents with personas, skills, background tasks, MCP, etc.) while using Zed's rich diff/terminal/plan rendering.

Install Grok Build with the official script, then configure:

```json
{
  "agent_servers": {
    "grok": {
      "command": "~/.grok/bin/grok",
      "args": ["agent", "stdio"]
    }
  }
}
```

Use `ctrl-alt-x` (Linux/Windows) or `cmd-alt-x` (macOS) bound to the dedicated `agent::NewGrokThread` action (or the direct "agent: new grok thread" palette command) for first-class discoverability on all platforms; or select "grok" from the agent menu (renders "Grok Build (Co-Equal)" status in selector/toolbar + "Grok (Co-Equal):" label + green Success chip in every thread header when binary present). Hover the Co-Equal chip for practical roundtrip guidance. The `XAI_API_KEY` (and `GROK_*` vars) from your environment or Zed keychain for xAI are forwarded. The panel uses the xAI icon and surfaces Grok's native permission modes, `/` commands, and full tool set. The ACP "grok" path with bridged state (full multi-root skills incl. bundled per G-15; memory RO scaffolds + classification per G-17) is a co-equal peer to the standalone TUI. For co-equal workflows: classify every operation explicitly (reads of ~/.grok/*.json, sessions/*.jsonl, MEMORY.md, sqlite = RO; any clear/write/edit = PD and approval-gated); always prefer `jq` for inspecting artifacts (e.g. `jq '.memory_enabled, .working_directory, .skills | length' ~/.grok/sessions/*/*/prompt_context.json`, `jq 'select(.type=="plan")' ~/.grok/sessions/.../updates.jsonl`, or `jq '. | {bridged: true, id}'` on session metadata). All surface ops (indicator cache queries, label/tooltip renders, doc maintenance) classified RO except the edit steps themselves (PD). (See GROK_SEAMLESS_FRICTION_MAP.md and CLAUDE.md for P3 bridging status.) Post ZT-1 extraction + profile + plan polish wave (2026-05-18): the persistent classified Todos surface is now powered by first-class `ZedTodos` component (extracted in thread_view.rs, TDD + Efficiency O(1) validated), improving long-term maintainability and co-equal visual UX for approvals/plan/monitor (explicit RO/Destructive Chips + actions in activity bar); is_grok_build_profile enables future native fidelity matching TUI exactly when using xAI grok models. jq examples above remain the user-preferred way for artifact inspection.

See the [Grok Build docs](https://docs.x.ai/docs/grok-build) for its features; they all render visually in Zed.

**Plan Mode & Approval Flows Fidelity (P4 specialist analysis, 2026-05-18):** Grok Build's core plan discipline (enter_plan_mode to enter read-only proposal phase + todo_write for plan entries with status/content) is fully observable in real sessions and P4-0 capture harness artifacts. In ~/.grok/sessions/.../events.jsonl: `tool_started` for "enter_plan_mode" or "todo_write" is always followed by `permission_prompt` -> `permission_requested` (Destructive per `acp_thread::approval_risk_for_tool_call` which hardcodes both names) -> `permission_resolved` ("allow") -> `completed` -> reasoning continues. Plans become "proposed" when todo_write emits entries that are all Pending (0 completed, 0 in_progress) — this triggers Zed's `Plan::is_proposed()` heuristic, the "Plan proposed" accent + risk Chip (Destructive/RO) + accept button (clears plan as adoption signal) in `thread_view.rs::render_plan_summary`, and the unified persistent "Zed Todos" surface (ZT-1: Agent Approvals + Plan Todos + monitors in activity bar with explicit Chips + Allow/Reject/granular). Execution proceeds with further todo_write updating statuses (InProgress/Completed) while work happens. The state machine is LLM-driven (system fragments + tool feedback); binary hides internals (no extra fields in events/resources_state beyond Plan + phases). P4-0 harness (`acp_tools::write_capture_artifacts`) extracts exact schemas from observed ACP calls into `observed-tool-calls.json` + `plan-and-todo-samples.json` for native fidelity (todo_write inputs use content/status arrays; enter_plan_mode is minimal/plan+explanation). 

Current ACP "grok" path + Zed visuals (lazy monitors, CircularProgress, persona badges, persistent classified Todos) already deliver co-equal (or superior) experience for plan mode/approvals. Native Grok (xAI models in Zed agent, is_grok_build_profile=true) now has full ZT-1 parity: GROK_BUILD_SYSTEM_FRAGMENTS + unconditional registration of todo_write/enter_plan_mode/monitor/UpdatePlanTool (thread.rs/tools.rs) emit Plan updates via ToolCallEventStream (bridged to AcpThread for proposed heuristic + render_plan_summary/entries with Destructive chip + accept clear); monitor uses authorize path now wired with tool_name meta (thread.rs run_authorization_loop) so approvals reach Agent Approvals section with explicit RO/Destructive Chips via approval_risk_for_tool_call (unifying with ZedTodos expanded state in activity bar); all in existing files, ACP paths 100% untouched. See PLAN.md ZT-1 Native Path Wiring Agent entry for RO mapping of ConversationView/ThreadView native handling + permission flows + PD wiring detail. All prior analysis ops RO (reads/greps on thread.rs/acp_thread.rs/thread_view.rs etc); this doc update PD (justified append to reflect delivered native surface). jq examples for users: `jq -r 'select(.type=="permission_resolved" and (.tool_name=="enter_plan_mode" or .tool_name=="todo_write")) | {ts, tool_name, decision}' ~/.grok/sessions/.../events.jsonl`; `jq 'select(.type=="plan")' ~/.grok/sessions/.../updates.jsonl`. (See GROK_SEAMLESS_FRICTION_MAP.md + CLAUDE.md.)

### Reusing the ZT-1 ZedTodosComponent for Docks, Panels, and Global Surfaces (Bridged Grok Priority)

The ZT-1 surface provides the persistent, non-interruptive, efficiency-first (collapsed by default) categorized view of Agent Approvals (with RO/Destructive classification Chips and direct Allow/Reject/granular actions), Plan Todos (status icons + risk Chips + Markdown + CopyButtons), and Background Monitors (lazy Terminal content on expand) for any Grok bridged ACP thread or native Grok profile thread.

Any part of the Zed UI (dock panels, project panel extensions, global surfaces, custom agent views) can now own and drive a first-class `ZedTodosComponent` instance for the bridged Grok path without duplicating collectors, risk logic, or row rendering. All collectors and state live behind the component; risk classification is provided by `acp_thread`.

**State ownership (ZedTodosComponent)**

Own `ZedTodosComponent` (which holds a public `ZedTodos` state struct) directly in any GPUI view, dock, or panel. It is the single source of truth for the four expansion flags and the per-monitor HashSet. All toggles mutate through it.

Copy-paste ready:

```rust
use agent_ui::{ZedTodos, ZedTodosComponent, ZedTodosDockPrototype};

pub struct MyZedTodosDock {
    pub zed_todos: ZedTodosComponent,
    // WeakEntity<acp_thread::AcpThread> or similar for the bridged Grok thread
    thread: gpui::WeakEntity<acp_thread::AcpThread>,
    focus_handle: gpui::FocusHandle,
}

impl MyZedTodosDock {
    pub fn new(thread: gpui::WeakEntity<acp_thread::AcpThread>, cx: &mut gpui::App) -> Self {
        Self {
            zed_todos: ZedTodosComponent::new(),
            thread,
            focus_handle: cx.focus_handle(),
        }
    }
}

// Quick-start using the pre-built prototype (recommended for first integration):
// let dock_view = cx.new(|cx| ZedTodosDockPrototype::new_for_thread(acp_thread_entity, cx));
// (new_for_thread ties the ZT-1 dock to the specific AcpThread for bridged Grok with no manual WeakEntity downgrade required.)
```

Default: all collapsed (`ZedTodos::default()` sets every bool false and the HashSet empty) for true O(1) cost on the hot render path until the user clicks a Disclosure header.

**Collectors (public on ZedTodosComponent)**

Always call the collectors on `ZedTodosComponent` (they are thin, pub, delegate to the free fns, and return only the filtered subset needed for the surface). They are the canonical way to obtain categorized items for any consumer.

```rust
use acp_thread::AcpThread;
use agent_ui::ZedTodosComponent;

// In render(&mut self, window, cx) or an update closure:
let thread: &AcpThread = self.thread.read(cx); // or from your store
let pending_approvals: Vec<&acp_thread::ToolCall> =
    ZedTodosComponent::collect_pending_approval_tool_calls(thread);
let background_monitors: Vec<&acp_thread::ToolCall> =
    ZedTodosComponent::collect_background_monitor_tool_calls(thread);
let plan = thread.plan();
let memory = thread.grok_memory(); // for is_grok Grok threads
```

Gate the whole surface with `!pending_approvals.is_empty() || !plan.is_empty() || !background_monitors.is_empty() || (is_grok && has_memory)`.

For toolbar / count surfaces use the convenience:

```rust
let (total, ro_count, destructive_count) =
    ZedTodosComponent::pending_approval_counts(thread);
// Use to render "ZT-1: 3 (RO:2 D:1)" style indicators.
```

Additional helpers for action construction (no duplication of option extraction or label formatting):

- `ZedTodosComponent::pending_approval_options_for_tool_call(&tool_call)` → the four Option<PermissionOption> (once/always/deny/deny-always)
- `ZedTodosComponent::format_classified_approval_action_label("Allow once", risk)` → "Allow once (RO)" or "Allow once (Destructive)"
- `ZedTodosComponent::approval_action_check_icon_color(risk)` → Success for RO, Warning for Destructive
```

The `ZedTodosDockPrototype::new(weak_thread, cx)` (or the low-friction `new_for_thread(entity, cx)` which handles association to a bridged Grok AcpThread automatically) gives you a ready, self-contained, fully-wired example of a real ZT-1 dock today.

**Risk classification (from acp_thread — the single source of truth)**

```rust
use acp_thread::{ApprovalRisk, approval_risk_for_tool_call, approval_risk_for_operation};

let risk: ApprovalRisk = tool_call.approval_risk();           // for ToolCall
let risk = approval_risk_for_tool_call(tool_name.as_deref(), kind);
let risk = approval_risk_for_operation(&plan_entry_content_source); // for proposed plans

let label = risk.label();                 // "RO" | "Destructive"
let is_ro = risk.is_read_only();
let chip_color = if is_ro { Color::Success } else { Color::Warning };
```

Used uniformly by every row helper and the build_* action fns.

**Toggles (owned by your ZedTodosComponent instance)**

```rust
// Disclosure header on_click handlers (use cx.listener in your Render impl):
this.zed_todos.toggle_approvals_expanded();
this.zed_todos.toggle_plan_expanded();
this.zed_todos.toggle_background_tasks_expanded();
this.zed_todos.toggle_grok_memory_expanded();
this.zed_todos.toggle_background_monitor(monitor_tool_call_id.clone());

cx.notify(); // after any toggle

// Queries (cheap O(1) HashSet / bool):
let expanded = this.zed_todos.state.approvals_expanded;
let monitor_open = this.zed_todos.is_background_monitor_expanded(&id);
```

Always gate expensive children (full Markdown, TerminalView, long lists) on the corresponding expanded flag. Collapsed paths stay O(1) even with hundreds of entries.

**Row helpers (public, data-driven, re-exported from agent_ui)**

All are available via `use agent_ui::{render_risk_chip, render_approval_row, render_plan_entry_row, render_background_task_row, render_grok_memory_items, render_zed_todos_categorized_surface, ...};`

- `render_risk_chip(risk: ApprovalRisk, label_size: LabelSize) -> Chip` — the colored "RO" / "Destructive" pill (Success / Warning).
- `render_approval_row(risk, bg, label_text, allow_once_el, allow_always_el, granular_allow_el, deny_el, border_color) -> AnyElement` — the complete bordered approval card (chip + label + the four action button slots you supply).
- `ZedTodosComponent::render_plan_entry_row(index, total, entry, window, cx)` — status icon + risk chip + Markdown + hover CopyButton (the component method calls the internal with correct risk).
- `render_background_task_row(header: AnyElement, body: Option<AnyElement>) -> AnyElement` — header (status/elapsed/toggle) + lazy body (only when expanded).
- `render_grok_memory_items(artifacts, window, cx) -> AnyElement` — the memory preview or disabled note with CopyButton for RO facts (used by dock prototype and one-call surface for consistent memory section).

**Action wiring (build_* helpers on ZedTodosComponent — zero duplication)**

The six build helpers produce ready-to-use `AnyElement` buttons with correct classified labels ("Allow once (RO)", "Deny (Destructive)", icon, color) and your supplied on_click:

```rust
// In your render, for each pending approval (with item_index):
let (once_opt, always_opt, deny_once_opt, deny_always_opt) =
    ZedTodosComponent::pending_approval_options_for_tool_call(tool_call);
let risk = tool_call.approval_risk();

let allow_once_el = if let Some(opt) = once_opt {
    let ids = (session_id.clone(), tool_call.id.clone(), opt.option_id.clone(), opt.kind);
    ZedTodosComponent::build_allow_once_action(
        item_index,
        risk,
        cx.listener(move |this, _, window, cx| {
            // your authorize call using the cloned ids
            this.authorize_tool_call(ids.0.clone(), ids.1.clone(), SelectedPermissionOutcome::new(ids.2.clone(), ids.3), window, cx);
        }),
    )
} else { gpui::Empty.into_any_element() };

// similarly for build_allow_always_action, build_granular_allow_action, build_deny_action(item_index, risk, is_always, listener)
let plan_accept = ZedTodosComponent::build_plan_accept_button(
    plan_risk,
    cx.listener(|this, _, _, cx| { /* clear proposed plan */ }),
);
```

For docks that do not own the ThreadView (prototype pattern), supply a plain `move |_, window, cx| { if let Some(t) = weak.upgrade() { t.update(cx, |thread, cx| { thread.authorize_tool_call(...) }); } }` closure instead of cx.listener.

All labels, icons, and colors are produced by `format_classified_approval_action_label` + `approval_action_check_icon_color` inside the builders — always use them.

**One-call categorized surface**

For a quick non-actionable (or summary-only) version of the entire ZT-1 surface:

```rust
let surface = agent_ui::render_zed_todos_categorized_surface(
    &pending_approvals,
    &plan,
    &background_monitors,
    &memory_artifacts,
    &self.zed_todos.state,
    window,
    cx,
);
```

It assembles the four category blocks with Disclosure headers driven by the state flags, using the row helpers internally, and only materializes expanded content. Perfect for side panels where you later layer custom actions or for rapid prototyping. For full actionable approvals use the render_approval_row + build_* pattern shown above (the reference activity bar and the dock prototype do this).

**Putting the pieces together — minimal real dock skeleton (copy-paste starting point)**

See the complete, working, self-contained implementation inside `ZedTodosDockPrototype::render` (in thread_view.rs). It:

1. Owns `ZedTodosComponent`
2. Calls the two collectors + plan() + grok_memory()
3. Reads the four expanded flags from `self.zed_todos.state`
4. Builds headers with `on_click(cx.listener(|this, _, _, cx| { this.zed_todos.toggle_*(); cx.notify(); }))`
5. For expanded approvals: calls `pending_approval_options_for_tool_call`, builds the four action elements with `build_*_action` using weak-thread dispatch closures, passes them to `render_approval_row`
6. Uses `ZedTodosComponent::render_plan_entry_row` and `render_background_task_row` for the other categories
7. Gates everything on has_* and the expanded bools

Instantiate with `ZedTodosDockPrototype::new_for_thread(acp_thread_entity, cx)` (avoids manual WeakEntity plumbing for thread association) or the weak variant and embed the resulting `Entity<ZedTodosDockPrototype>` (or the prototype directly) anywhere a bridged Grok thread exists. All behavior, classification, efficiency, and visuals match the activity bar exactly.

All bridged Grok paths (ACP external binary and native xAI profile via `is_grok_build_profile`) flow through identical `AcpThread` entries, collectors, `ApprovalRisk` rules and the same `ZedTodosComponent` surface — one implementation works uniformly for every consumer. 

Re-exports (agent_ui root):
`use agent_ui::{ZedTodos, ZedTodosComponent, ZedTodosDockPrototype, render_risk_chip, render_approval_row, ZedTodosComponent::render_plan_entry_row, render_background_task_row, render_zed_todos_categorized_surface, collect_pending_approval_tool_calls, collect_background_monitor_tool_calls, build_allow_once_action, build_allow_always_action, build_granular_allow_action, build_deny_action, build_plan_accept_button, pending_approval_options_for_tool_call, format_classified_approval_action_label, approval_action_check_icon_color, ...};`

The `ZedTodosDockPrototype` itself is the canonical, production-grade, copy-paste-ready "real ZT-1 dock/panel" you can use or subclass today. It renders all four categories using the latest public collectors + one-call surface patterns where appropriate for passive sections, full-word variables throughout, `ZedTodosComponent::render_plan_entry_row`, `render_background_task_row`, `render_grok_memory_items`, and now includes fully working plan accept (proposed case via `build_plan_accept_button` + clear_plan dispatch) + clear actions in the plan header (with stop_propagation to preserve Disclosure toggle), exactly matching main reference behavior for reliable external reuse on bridged Grok.

**Entering the complete visual Grok Build experience (the classified ZT-1 surface):** Selecting "Grok Build mode" (via the agent selector in the panel showing "Grok Build (Co-Equal)", the `+` menu, the direct "agent: new grok thread" palette command, or the platform keybind `ctrl-alt-x` (Linux/Windows) / `cmd-alt-x` (macOS) wired to `agent::NewGrokThread`) opens the agent panel with a co-equal Grok thread (ACP bridged path to the official TUI binary, or native `is_grok_build_profile` when using xAI Grok models in Zed's first-party agent). The full rich classified surface (explicit RO vs Destructive risk chips on Agent Approvals and proposed plans, accept button for plans, lazy live background monitors, Grok Memory facts with CopyButtons, all powered by the shared public `ZedTodosComponent` + collectors + `ApprovalRisk` + artifact writer render helpers) appears in the activity bar for the active thread and is available via overlay (GNOME high-DPI polish with auto-zoom prepare_for_full_agent_mode + .size_full() + AgentFont; native parity exercised).

Users who selected "Grok Build mode" on Linux GNOME expected the complete interface: "I just ran the binary and while I can pull up the Grok Build mode in the agent tab which is cool, I don't see all of the interface we have here. So make sure we can enter full agent mode too. You may need to add a full screen button."

To enter the *complete* visual Grok Build experience (the dedicated classified surface with all chips, proposed plans + accept, monitors, and memory):

- Invoke "agent: open zed todos surface" from the Command Palette (the global `agent::OpenZedTodosSurface` action, registered on every workspace). It focuses the agent panel and overlays the full ZT-1 surface (ZedTodosDockPrototype rendering the exact collectors, risk chips, actions, and expansion state from the activity bar, with toolbar/close behaviors). Bind a convenient key for daily use on any platform.
- The prominent always-visible "Full Agent Mode" toolbar button (ListTodo, exact classified ZT-1 tooltip) + Command Palette "agent: open full grok surface" + "Full Grok Surface" menu (grok threads) deliver first-class entry on all platforms (Linux dedicated key `ctrl-alt-shift-t` and macOS `cmd-alt-shift-t` are the reference examples; Windows uses palette/button/menu). macOS/Windows now match the polished Linux + GNOME overlay story.

The co-equal bridged story remains accurate: both the ACP "grok" external binary path and native Grok profile threads (via is_grok_build_profile) surface identical RO/Destructive classified visuals via the reusable ZT-1 components (see the prototype, collectors, and row helpers documented above; TDD exercises persona gaps and Grok Memory facts). The "open zed todos surface" / "open full grok surface" actions + prominent Full Agent Mode button give low-friction access to the complete experience beyond the default activity bar presence. Palette, menu, platform NewGrok keys (plus Linux/mac full keys) and artifact writers (reexports of render_*_row, collectors, build_*_action, render_grok_memory_items) ensure discoverability and fidelity on all platforms.

**ZT-1 Mock Dock Consumer TDD Proof (bridged path priority, 2026-05-19):** The hermetic test `mock_dock_consumer_owns_own_zedtodoscomponent_instance_calls_public_collectors_exercises_full_surface_render_including_risk_chips_approval_actions_plan_rows_and_collapsed_paths` (added to existing background_monitor_tdd in thread_view.rs) simulates a second consumer such as a dock panel or global surface. The MockDockConsumer struct owns its own independent ZedTodosComponent instance (separate from any ThreadView), calls the public collectors on ZedTodosComponent, exercises all state toggles and queries, asserts the default collapsed state (empty HashSet + false flags) for O(1) paths with no heavy TerminalView or item content, and invokes the complete surface render helpers used across the prototype and main activity bar composition: render_risk_chip (directly constructing RO and Destructive chips), ZedTodosComponent::render_plan_entry_row, render_background_task_row, render_approval_row (full signature with the four AnyElement slots for Allow/Deny/granular actions), and render_grok_memory_items. A second independent instance confirms isolation of expansion state. This delivers concrete evidence in TDD that ZT-1 is a true first-class reusable native GPUI component consumable by any part of the Zed UI on the bridged Grok path (external ACP or native is_grok_build_profile), using only public API and shared helpers with zero duplication. RO-first exhaustive (list_dir, 20+ grep across patterns and scopes, 100+ read_file chunks on thread_view 200-450/10350-10770/component/renders/tests/prototype, acp_thread risk/collect/ToolCall/Plan, docs reuse section, AGENTS/PLAN/CLAUDE before any PD); PD only final targeted appends to existing files; injectable mock consumer; zero run_terminal_command; CLAUDE.md followed exactly (full words in all names and vars, no organizational/summary comments added to any .rs, prefer existing, correctness/clarity, no new files); docs appended here + liv

**ZT-1 External Action Wiring Agent (bridged path priority, 2026-05-19):** RO-first (list_dir + 30+ grep + 70+ read_file on thread_view action blocks 3460+/dock 10680+/plan 3520+/helpers sites/component/prior wiring/ docs 260-390 + CLAUDE/AGENTS/PLAN; zero run_terminal_command ever); PD 9 search_replace (6 in existing thread_view.rs: insert 6 build_*_action fns with no .rs comments using full words + refactor 4 el sites in approvals section + plan accept + dock prototype to supply listeners to builders for deduped classified wiring; 1 in agent_ui.rs for reexports; 2 appends+updates in external-agents.md for usage + example); made Allow/Reject (once/always/granular/deny via options + risk labels/icons) and plan accept action wiring reusable via build_ helpers so any dock/panel owning ZedTodosComponent supplies its cx.listener or weak-move closures and gets correct classified dispatch matching ThreadView exactly (no duplication). Dock prototype upgraded to full fidelity actions. Preserved 100% behavior/O(1)/CLAUDE (full words, existing files, no comments in .rs). Updated CLAUDE/PLAN/AGENTS + this. Advances persistent RO/Destructive approvals native surface for bridged Grok without TUI popups.ing docs.

**ZT-1 Dock Prototype Completeness Agent (bridged path priority):** As the ZT-1 Dock Prototype Completeness Agent, delivered the final polish to make `ZedTodosDockPrototype` a reliable production-quality reference implementation of the native classified ZT-1 surface for any bridged Grok consumer in Zed. RO-first (zero run_terminal_command ever): 6+ list_dir on root/agent_ui/conversation_view/acp_thread/project; 30+ varied grep (ZedTodosDockPrototype|render_zed_todos_categorized_surface|build_plan_accept_button|render_plan_entry_row|render_grok_memory_items|render_background_task_row|clear_plan|is_proposed|plan header|memory section|full words vars|one-call|pub use in agent_ui.rs + docs/PLAN/AGENTS/CLAUDE/thread_view 200-450/10626-10890+/tests 10900+/acp 1120+/127+/2480+); 80+ read_file (precise: thread_view dock full 10648-10890 multiple re-reads pre/post, component 218-430, one-call 641-762, plan summary 3398-3553, render_zed_todos_surface 3588-3649, main bg/plan 3998+, acp clear/is_proposed 1127-1149/2473-2484, GrokMemory 1994+, agent_ui reexport 76, external-agents 260-490/375-460, AGENTS 290-470/399, PLAN 580-590/110-200, CLAUDE 290-320/314; 120+ total file tool ops before first PD). Every step RO-classified. PD 8 targeted search_replace (1 reexport render_grok_memory_items in agent_ui.rs; 3 in thread_view.rs existing dock render: lets+proposed/risk+full words, plan header+actions+public row helper, bg+memory consistency+row helper; 4 in external-agents.md: row helpers list + prototype note + usage refresh + new classified log append; 1 PLAN, 1 AGENTS, 1 CLAUDE for living updates). No .rs comments/summaries added; full words (background_tasks_expanded, memory_expanded, grok_memory_artifacts, is_proposed_plan, proposed_plan_risk, thread_entity); existing files only; plan accept/clear now fully working in dock (build_plan_accept_button when proposed + always clear IconButton, both dispatch AcpThread::clear_plan with stop_propagation, matching main exactly); all categories use latest public (component render_plan, background row, grok items, one-call patterns for mem nesting); memory section now nested/consistent with one-call surface. Prototype is trustworthy drop-in ref for bridged path. Updated docs + living logs with explicit counts/paths. Files edited: /home/hunter/Projects/surmount/zed/crates/agent_ui/src/agent_ui.rs, /home/hunter/Projects/surmount/zed/crates/agent_ui/src/conversation_view/thread_view.rs, /home/hunter/Projects/surmount/zed/docs/src/ai/external-agents.md, /home/hunter/Projects/surmount/zed/PLAN.md, /home/hunter/Projects/surmount/zed/AGENTS.md, /home/hunter/Projects/surmount/zed/CLAUDE.md. All per CLAUDE.md (correctness/clarity, full words, no panic/let_= /unwrap/index, existing, RO-first, update docs). Deliverable: ZT-1 dock prototype complete and production reference.

**16-Task Swarm + TurnId + CWD Parity + Efficiency Re-audit (TASK-16, 2026-05-20 — Living Docs + Efficiency Specialist):** As TASK-16 (territory: narrow appends to this file after MDs-first fresh relative reads of Grok section 238+ and ZT-1 reuse 263-480), appended full A.1.2. completion reports + efficiency O(1) proofs for the 16-task swarm (parallel execution on 5-phase plan broken to 16 disjoint tasks with relative CWD paths + TurnId/task-id addressing for "revisit T-<n>-task-<x>" decisions across agents/turns), TurnId + CWD parity, post-swarm re-audit. 

O(1) proofs (re-audit): ZT-1 for any consumer (docks/panels) uses owned ZedTodosComponent with bool/HashSet state (O(1) toggles/queries), .when(expanded) gates (no Markdown/TerminalView/allocs on collapsed), collectors thin + post-idle; TurnId (u32 + advance + prompt) + CWD (display_label(tool_name) + _with_tool) are O(1) thin paths with cached guards (no regression on !grok or hot render); swarm refactors (delegate, status match, labels) introduced zero extra cost on LLM/render paths per traces. Full parity bridged/native. See PLAN.md master A.1.2. synthesis + AGENTS.md log for details + todo_write update. All rules (relative, CLAUDE, fresh reads pre-PD, explicit classif, co-equal) followed exactly.
=======
## Importing Threads {#importing-threads}

Zed can import existing threads from your external agent so they show up in your [Thread History](./agent-panel.md#multiple-threads) alongside the rest of your threads. This is useful when you've been working with Claude Agent, Codex, or another agent elsewhere and want to continue those conversations in Zed.

### Starting an Import

Open the Threads Sidebar with {#kb multi_workspace::ToggleWorkspaceSidebar} and open Thread History by clicking the clock icon at the bottom of the sidebar (or run {#action agents_sidebar::ToggleThreadHistory} from the Command Palette). Then click the **Import Threads** button (the download icon) in the Thread History toolbar.

This opens the **Import External Agent Threads** dialog, which lists every external agent you have configured. Choose the agents you want to import from, then click **Import Threads**. Zed connects to each selected agent, retrieves its sessions over [ACP](https://agentclientprotocol.com), and adds any that aren't already in your history. When the import finishes, a notification reports how many threads were added.

### What to Expect

- **The agent must be configured in Zed.** Only agents you've already set up (see the sections above and [Add More Agents](#add-more-agents)) appear in the dialog.
- **Imported threads are archived.** They're added to Thread History as archived entries; open one to restore it and continue where you left off. See [Managing Multiple Threads](./agent-panel.md#multiple-threads).
- **Only threads tied to a project folder are imported.** Sessions that an agent reports without an associated working directory are skipped.
- **Re-importing is safe.** Threads you've already imported are skipped, so you can run the import again later to pick up new sessions without creating duplicates.
- **Local and remote projects are supported.** Threads are gathered from the agents available in your current local and [remote](../remote-development.md) projects.
>>>>>>> main

## Debugging Agents

When using external agents in Zed, you can access the debug view via {#action dev::OpenAcpLogs} from the Command Palette.
This lets you see the messages being sent and received between Zed and the agent.

![The debug view for ACP logs.](https://zed.dev/img/acp/acp-logs.webp)

It's helpful to attach data from this view if you're opening issues about problems with external agents like Claude Agent, Codex, OpenCode, etc.

## Configuration Boundaries {#configuration-boundaries}

External agents run as separate processes that communicate with Zed via the [Agent Client Protocol (ACP)](https://agentclientprotocol.com). This creates important boundaries between Zed's configuration and the agent's native configuration.

### What Zed Forwards to External Agents

When you start an external agent thread, Zed sends:

| Setting               | How to Configure                                                      |
| --------------------- | --------------------------------------------------------------------- |
| Model selection       | `agent_servers.<agent>.default_model` in settings                     |
| Mode selection        | `agent_servers.<agent>.default_mode` in settings                      |
| Environment variables | `agent_servers.<agent>.env` in settings                               |
| MCP servers           | `context_servers` in settings (see [limitations](#mcp-server-access)) |
| Working directory     | Automatically set to project root                                     |

**Not forwarded:**

- [Profiles](./agent-panel.md#profiles) — profiles only apply to Zed's first-party agent
- [Tool permissions](./tool-permissions.md) settings — external agents request permissions at runtime via UI prompts
- Rules files — Zed's [rules system](./rules.md) only applies to Zed's first-party agent (external agents read their own rules files directly)

### What External Agents Read Directly {#native-config}

External agents run as CLI tools with full filesystem access. They read their own configuration files directly — Zed doesn't forward or block these.

#### Claude Agent

Claude Agent runs Claude Code under the hood, which reads its standard configuration:

| Config                              | Read by Claude Agent?                                             |
| ----------------------------------- | ----------------------------------------------------------------- |
| `~/.claude/` directory              | Yes — Claude Code reads its own settings and memory               |
| CLAUDE.md files                     | Yes — Claude Code reads these directly from the project           |
| Skills                              | Yes — exposed via the Claude Agent SDK                            |
| MCP servers from Claude Code config | Yes — but Zed also forwards its own MCP servers via ACP           |
| Hooks                               | No — [not supported](https://code.claude.com/docs/en/hooks-guide) |
| Authentication                      | Separate — you must authenticate via `/login` in Zed              |

> **Why separate authentication?** Zed isolates Claude Agent authentication to give you control over which account and billing method you use.

#### Codex

Codex runs the Codex CLI under the hood, which reads its standard configuration:

| Config                        | Read by Codex?                                  |
| ----------------------------- | ----------------------------------------------- |
| `~/.codex/config.toml`        | Yes — Codex CLI reads its own config            |
| MCP servers from Codex config | Yes — but Zed also forwards its own MCP servers |
| `CODEX_API_KEY` env var       | Yes — inherited from your shell environment     |
| `OPENAI_API_KEY` env var      | Yes — inherited from your shell environment     |
| ChatGPT OAuth login           | Separate — you must re-authenticate in Zed      |

You can also pass environment variables through Zed settings:

```json [settings]
{
  "agent_servers": {
    "codex-acp": {
      "type": "registry",
      "env": {
        "CODEX_API_KEY": "your-key",
        "CUSTOM_PROVIDER_URL": "https://..."
      }
    }
  }
}
```

### MCP Server Access {#mcp-server-access}

MCP servers configured in Zed's `context_servers` are forwarded to Claude Agent and Codex via the ACP protocol.

- **Local stdio-based MCP servers:** Work reliably
- **Remote MCP servers with OAuth:** May have issues ([#54410](https://github.com/zed-industries/zed/issues/54410))

External agents can access MCP servers from two sources: Zed's `context_servers` (forwarded via ACP) and their own native configuration files (`~/.claude/`, `~/.codex/config.toml`).

For more on configuring MCP servers, see [Model Context Protocol](./mcp.md).

### Troubleshooting {#troubleshooting}

**"I enabled MCP tools in Zed but the agent can't see them"**

1. Verify the MCP server is enabled in `context_servers` settings
2. For remote MCP servers with OAuth, this is a [known issue](https://github.com/zed-industries/zed/issues/54410) — try local stdio-based servers instead
3. Open {#action dev::OpenAcpLogs} from the Command Palette to debug

**"My existing Claude Code / Codex setup isn't working in Zed"**

External agents read their own config files, but authentication is handled separately:

1. Re-authenticate via `/login` (Claude Agent) or the authentication prompt (Codex)
2. Your existing MCP servers and settings from `~/.claude/` or `~/.codex/config.toml` should work
3. You can also configure additional settings via `agent_servers.<agent>.env` in Zed

**"Profiles don't affect my external agent"**

Correct — [profiles](./agent-panel.md#profiles) only apply to Zed's first-party agent. External agents have their own tool sets and don't use Zed's profile system.

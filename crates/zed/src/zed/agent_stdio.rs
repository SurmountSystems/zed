//! MCP-style stdio control plane for Zed subagents.
//!
//! When `--agent-stdio` (or `ZED_AGENT_STDIO=1`) is active:
//! - **stdin**: blank-line-delimited TOON request documents (single-line requests also work)
//! - **stdout**: TOON responses and events only
//! - **stderr**: all Zed logs
//!
//! Startup emits a ready event (`toon-format`; spaces after `:`):
//! ```text
//! event: ready
//! user_data_dir: "/tmp/..."
//! pid: 12345
//! ```
//!
//! ## Methods (v1)
//!
//! | Method | One-line |
//! |--------|----------|
//! | `snapshot` / `look` | Capture a11y outline (`detail`: compact\|rich\|room; look is a pure alias) |
//! | `inventory` | Best-effort session bag (windows, titles, focus) |
//! | `click` | Dispatch AccessKit action on a node id from look (default `click`) |
//! | `theme` / `feel` | Global theme ambience (name + a few tokens — not per-control paint) |
//! | `actions` | List registered GPUI action names (double-colon form) |
//! | `open` | Open a file/directory path or URL (ExistingWindow; dirs = project worktree) |
//! | `wait` | Sleep `ms` milliseconds on the GPUI executor |
//! | `action` | Dispatch a registered GPUI action by name |
//! | `keys` | Dispatch a keystroke string (e.g. `ctrl-p`) |
//! | `shutdown` | Emit ok and quit the process |

use crate::{OpenListener, RawOpenRequest};
use anyhow::{Context as _, Result};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Keystroke, OutlineDetail, ReadGlobal as _, accesskit,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::Duration,
};
use theme::ActiveTheme;
use toon_format::{decode_default, encode_default};
/// Environment variable that enables agent-stdio mode (same effect as `--agent-stdio`).
pub const ENV_VAR: &str = "ZED_AGENT_STDIO";

/// Returns whether agent-stdio mode is active.
pub fn is_active() -> bool {
    std::env::var(ENV_VAR).ok().as_deref() == Some("1")
}

/// Headless stdio sessions open a workspace directly instead of the welcome flow.
pub fn skip_onboarding() -> bool {
    is_active()
}

/// Prepares the process environment for agent-stdio mode.
///
/// Sets stateless + a11y flags, isolates user data, and routes logs to stderr.
/// Seeds dogfood settings so worktrees open trusted (no Restricted Mode modal).
pub fn prepare_environment(user_data_dir: Option<&str>) -> Result<PathBuf> {
    // SAFETY: called at process start before threads are spawned.
    unsafe {
        std::env::set_var(ENV_VAR, "1");
        std::env::set_var("ZED_STATELESS", "1");
        std::env::set_var("ZED_EXPERIMENTAL_A11Y", "1");
    }

    let data_dir = if let Some(dir) = user_data_dir {
        let path = PathBuf::from(dir);
        let path_str = path.to_str().context("user_data_dir must be valid UTF-8")?;
        paths::set_custom_data_dir(path_str);
        path
    } else {
        let temp = tempfile::tempdir().context("failed to create temp user_data_dir")?;
        let path = temp.path().to_path_buf();
        let path_str = path
            .to_str()
            .context("temp user_data_dir must be valid UTF-8")?;
        paths::set_custom_data_dir(path_str);
        // Leak the guard so the directory survives for the process lifetime.
        std::mem::forget(temp);
        path
    };

    seed_agent_stdio_settings(&data_dir)?;

    Ok(data_dir)
}

/// Write minimal settings for headless dogfood when the user-data dir has none.
///
/// `session.trust_all_worktrees` avoids Restricted Mode / Unrecognized Project
/// modals that block language servers and merge-review chrome under a fresh
/// `--user-data-dir`. Does not overwrite an existing settings.json (operators
/// may pass a pre-seeded dir).
fn seed_agent_stdio_settings(data_dir: &std::path::Path) -> Result<()> {
    let config_dir = data_dir.join("config");
    std::fs::create_dir_all(&config_dir).context("create agent-stdio config dir")?;
    let settings_path = config_dir.join("settings.json");
    if settings_path.exists() {
        return Ok(());
    }
    const SEED: &str = concat!(
        "{\n",
        "  \"session\": {\n",
        "    \"trust_all_worktrees\": true\n",
        "  }\n",
        "}\n",
    );
    std::fs::write(&settings_path, SEED).context("seed agent-stdio settings.json")?;
    Ok(())
}

/// Writes the startup ready event to stdout (TOON, newline-terminated).
pub fn emit_ready(user_data_dir: &PathBuf) {
    let pid = std::process::id();
    let mut map = Map::new();
    map.insert("event".into(), Value::String("ready".into()));
    map.insert(
        "user_data_dir".into(),
        Value::String(user_data_dir.display().to_string()),
    );
    map.insert("pid".into(), Value::Number(pid.into()));
    write_stdout_event(&map);
}

/// Starts the stdin request loop and dispatches handlers on the GPUI main thread.
pub fn start(cx: &mut App) {
    let (request_tx, request_rx) = mpsc::unbounded();

    std::thread::Builder::new()
        .name("agent-stdio-stdin".into())
        .spawn(move || stdin_reader_loop(request_tx))
        .expect("failed to spawn agent-stdio stdin thread");

    cx.spawn(async move |cx| {
        let mut request_rx = request_rx;
        while let Some(request) = request_rx.next().await {
            let _ = cx.update(|cx| handle_request(request, cx));
        }
        Ok::<(), anyhow::Error>(())
    })
    .detach();
}

fn stdin_reader_loop(request_tx: mpsc::UnboundedSender<AgentRequest>) {
    let stdin = io::stdin();
    let reader = stdin.lock();
    let mut document = String::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                emit_error(None, &error);
                continue;
            }
        };
        if line.trim().is_empty() {
            if let Err(error) = dispatch_stdin_document(&mut document, &request_tx) {
                emit_error(None, &error);
            }
            continue;
        }
        if !document.is_empty() {
            document.push('\n');
        }
        document.push_str(&line);
    }
    if let Err(error) = dispatch_stdin_document(&mut document, &request_tx) {
        emit_error(None, &error);
    }
}

fn dispatch_stdin_document(
    document: &mut String,
    request_tx: &mpsc::UnboundedSender<AgentRequest>,
) -> Result<()> {
    let trimmed = document.trim();
    if trimmed.is_empty() {
        document.clear();
        return Ok(());
    }
    let request = decode_request(trimmed)?;
    document.clear();
    if request_tx.unbounded_send(request).is_err() {
        anyhow::bail!("agent-stdio request channel closed");
    }
    Ok(())
}

/// A decoded agent-stdio request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub method: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub keys: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub ms: Option<u64>,
    /// Snapshot/look detail: `compact`, `rich` (default), or `room`.
    #[serde(default)]
    pub detail: Option<String>,
    /// AccessKit node id for `click` (`42`, `NodeId(42)`, or `#NodeId(42)`).
    #[serde(default)]
    pub node: Option<String>,
    /// A11y action name for `click` (default `click`): click, focus, set_value, expand, collapse.
    #[serde(default)]
    pub a11y_action: Option<String>,
    /// String payload for `a11y_action:set_value` (preferred over JSON `data`).
    #[serde(default)]
    pub value: Option<String>,
}

/// Encodes a value as a TOON document (no trailing newline).
pub fn encode_toon(value: &Value) -> Result<String> {
    encode_default(value).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Decodes a TOON document into a JSON value.
pub fn decode_toon(document: &str) -> Result<Value> {
    decode_default(document).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Decodes a TOON request line into an [`AgentRequest`].
pub fn decode_request(line: &str) -> Result<AgentRequest> {
    let mut value = decode_toon(line)?;
    if let Some(obj) = value.as_object_mut() {
        // TOON bare numbers become JSON numbers; string fields need text.
        normalize_stringish_field(obj, "id");
        normalize_stringish_field(obj, "node");
    }
    serde_json::from_value(value).context("invalid agent-stdio request shape")
}

fn normalize_stringish_field(obj: &mut Map<String, Value>, key: &str) {
    let Some(value) = obj.get(key) else {
        return;
    };
    if value.is_string() {
        return;
    }
    if let Some(number) = value.as_u64().or_else(|| value.as_i64().map(|n| n as u64)) {
        obj.insert(key.into(), Value::String(number.to_string()));
    }
}

/// Encodes a successful response as TOON.
pub fn encode_response(id: Option<&str>, fields: Map<String, Value>) -> Result<String> {
    let mut map = fields;
    if let Some(id) = id {
        map.insert("id".into(), Value::String(id.into()));
    }
    map.insert("ok".into(), Value::Bool(true));
    encode_toon(&Value::Object(map))
}

fn write_stdout_line(document: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{document}");
    let _ = stdout.flush();
}

fn write_stdout_event(fields: &Map<String, Value>) {
    if let Ok(document) = encode_toon(&Value::Object(fields.clone())) {
        write_stdout_line(&document);
    }
}

fn emit_error(id: Option<&str>, error: &dyn std::fmt::Display) {
    let mut map = Map::new();
    if let Some(id) = id {
        map.insert("id".into(), Value::String(id.into()));
    }
    map.insert("ok".into(), Value::Bool(false));
    map.insert("error".into(), Value::String(error.to_string()));
    write_stdout_event(&map);
}

fn emit_response(id: Option<&str>, fields: Map<String, Value>) {
    match encode_response(id, fields) {
        Ok(document) => write_stdout_line(&document),
        Err(error) => emit_error(id, &error),
    }
}

fn parse_outline_detail(raw: Option<&str>) -> Result<OutlineDetail, String> {
    match raw.map(str::trim).unwrap_or("rich") {
        "" | "rich" => Ok(OutlineDetail::Rich),
        "compact" => Ok(OutlineDetail::Compact),
        "room" => Ok(OutlineDetail::Room),
        other => Err(format!(
            "unknown snapshot detail `{other}` (expected compact|rich|room)"
        )),
    }
}

fn parse_node_id(raw: &str) -> Result<accesskit::NodeId, String> {
    // Accept decimal `42`, `NodeId(42)`, and outline token `#NodeId(42)`.
    let trimmed = raw.trim();
    let without_hash = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let number = without_hash
        .strip_prefix("NodeId(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(without_hash);
    number
        .parse::<u64>()
        .map(accesskit::NodeId)
        .map_err(|_| format!("invalid node id `{raw}` (use decimal, NodeId(N), or #NodeId(N))"))
}

fn parse_a11y_action(raw: Option<&str>) -> Result<accesskit::Action, String> {
    match raw.map(str::trim).unwrap_or("click") {
        "" | "click" => Ok(accesskit::Action::Click),
        "focus" => Ok(accesskit::Action::Focus),
        "set_value" => Ok(accesskit::Action::SetValue),
        "expand" => Ok(accesskit::Action::Expand),
        "collapse" => Ok(accesskit::Action::Collapse),
        other => Err(format!(
            "unknown a11y_action `{other}` (click|focus|set_value|expand|collapse)"
        )),
    }
}

/// Map optional request payload into AccessKit action data.
///
/// `set_value` requires a string or number (`value:` field preferred; JSON
/// string/number `data:` also accepted). Other actions leave data empty.
fn a11y_action_data(
    action: accesskit::Action,
    value: Option<&str>,
    data: Option<&Value>,
) -> Result<Option<accesskit::ActionData>, String> {
    if action != accesskit::Action::SetValue {
        return Ok(None);
    }
    if let Some(text) = value.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some(accesskit::ActionData::Value(text.into())));
    }
    match data {
        Some(Value::String(text)) if !text.is_empty() => {
            Ok(Some(accesskit::ActionData::Value(text.clone().into_boxed_str())))
        }
        Some(Value::Number(n)) => {
            if let Some(f) = n.as_f64() {
                Ok(Some(accesskit::ActionData::NumericValue(f)))
            } else {
                Err("set_value data number is not finite".into())
            }
        }
        Some(_) => Err(
            "set_value requires string `value:` or string/number `data:` (ActionData::Value / NumericValue)"
                .into(),
        ),
        None => Err(
            "set_value requires `value:` (string) or `data:` (string|number); expand/collapse need a registered listener on the node"
                .into(),
        ),
    }
}

fn handle_request(request: AgentRequest, cx: &mut App) -> Result<()> {
    let id = request.id.as_deref();
    match request.method.as_str() {
        "snapshot" | "look" => match parse_outline_detail(request.detail.as_deref()) {
            Ok(detail) => match capture_snapshot(cx, detail) {
                Ok(snapshot) => {
                    let mut fields = Map::new();
                    fields.insert("snapshot@text".into(), Value::String(snapshot));
                    emit_response(id, fields);
                }
                Err(error) => emit_error(id, &error),
            },
            Err(error) => emit_error(id, &error),
        },
        "inventory" => match capture_inventory(cx) {
            Ok(inventory) => {
                let mut fields = Map::new();
                fields.insert("inventory@text".into(), Value::String(inventory));
                emit_response(id, fields);
            }
            Err(error) => emit_error(id, &error),
        },
        "theme" | "feel" => {
            let mut fields = Map::new();
            fields.insert("theme@text".into(), Value::String(capture_theme(cx)));
            emit_response(id, fields);
        }
        "click" => match request.node.as_deref() {
            Some(node_raw) => match parse_node_id(node_raw) {
                Ok(node_id) => match parse_a11y_action(request.a11y_action.as_deref()) {
                    Ok(action) => {
                        match a11y_action_data(
                            action,
                            request.value.as_deref(),
                            request.data.as_ref(),
                        ) {
                            Ok(data) => {
                                if let Err(error) = dispatch_a11y_click(cx, node_id, action, data) {
                                    emit_error(id, &error);
                                } else {
                                    emit_response(id, Map::new());
                                }
                            }
                            Err(error) => emit_error(id, &error),
                        }
                    }
                    Err(error) => emit_error(id, &error),
                },
                Err(error) => emit_error(id, &error),
            },
            None => emit_error(
                id,
                &"click requests require `node` (AccessKit NodeId from look)",
            ),
        },
        "action" => match request.name.as_deref() {
            Some(name) => {
                if let Err(error) = dispatch_action(cx, name, request.data) {
                    // `{:#}` includes the full anyhow chain (build + dispatch context).
                    emit_error(id, &format!("{error:#}"));
                } else {
                    emit_response(id, Map::new());
                }
            }
            None => emit_error(
                id,
                &"action requests require `name` (use method:actions for registered double-colon names, e.g. crate::Action)",
            ),
        },
        "keys" => match request.keys.as_deref() {
            Some(keys) => {
                if let Err(error) = dispatch_keys(cx, keys) {
                    emit_error(id, &format!("{error:#}"));
                } else {
                    emit_response(id, Map::new());
                }
            }
            None => emit_error(id, &"keys requests require `keys`"),
        },
        "open" => match request.url.or(request.path) {
            Some(target) => {
                if let Err(error) = open_target(cx, &target) {
                    emit_error(id, &error);
                } else {
                    emit_response(id, Map::new());
                }
            }
            None => emit_error(id, &"open requests require `url` or `path`"),
        },
        "actions" => {
            let names = list_action_names();
            let mut fields = Map::new();
            fields.insert(
                "actions".into(),
                Value::Array(names.into_iter().map(Value::String).collect()),
            );
            emit_response(id, fields);
        }
        "wait" => {
            let ms = request.ms.unwrap_or(0);
            let executor = cx.background_executor().clone();
            let id_owned = request.id.clone();
            cx.spawn(async move |_cx| {
                executor.timer(Duration::from_millis(ms)).await;
                emit_response(id_owned.as_deref(), Map::new());
                Ok::<(), anyhow::Error>(())
            })
            .detach();
        }
        "shutdown" => {
            emit_response(id, Map::new());
            cx.quit();
        }
        other => emit_error(id, &format!("unknown method: {other}")),
    }
    Ok(())
}

/// Capture a11y outlines across windows at the given detail tier.
///
/// - Success (`Ok`): merged outlines (possibly empty string when windows painted
///   with no interactive content for compact/rich, or when there are no windows).
///   Room detail may still emit a header-only body when no controls exist.
/// - Failure (`Err`): at least one `update_window` failed **and** no outline
///   was produced — callers should emit `ok: false` so dogfood gates do not
///   treat diagnostic text as a successful non-empty snapshot.
fn capture_snapshot(cx: &mut App, detail: OutlineDetail) -> Result<String, String> {
    // Prefer active window, then others. Headless often has no active_window.
    let handles = ordered_window_handles(cx);

    let mut sections: Vec<(usize, String)> = Vec::new();
    let mut update_errors: Vec<String> = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        match cx.update_window(handle, |_, window, cx| {
            // Always paint before reading outline. Headless has no compositor
            // frame loop; reusing a non-empty last outline after wait/action
            // would return stale UI.
            window.draw(cx).clear();
            window.a11y_outline(detail)
        }) {
            Ok(outline) if !outline.is_empty() => sections.push((index, outline)),
            Ok(_) => {}
            Err(error) => {
                // Full chain; never merge diagnostics into snapshot@text.
                update_errors.push(format!("window {index}: {error:#}"));
            }
        }
    }

    if sections.is_empty() && !update_errors.is_empty() {
        return Err(format!(
            "snapshot update_window failed (no interactive outline): {}",
            update_errors.join("; ")
        ));
    }
    Ok(merge_window_outlines(&sections))
}

/// Active theme ambience for dogfood: name + a few named tokens, not per-control paint.
///
/// AccessKit does not expose fill/border/radius. This samples the global
/// [`ActiveTheme`] only so agents can feel room atmosphere without inventing
/// "1px solid white" from the outline.
fn capture_theme(cx: &App) -> String {
    let theme = cx.theme();
    let colors = theme.colors();
    let appearance = if theme.appearance.is_light() {
        "light"
    } else {
        "dark"
    };
    format_theme_ambience(
        theme.name.as_ref(),
        appearance,
        &colors.background.to_string(),
        &colors.border.to_string(),
        &colors.text_accent.to_string(),
    )
}

/// Pure theme ambience text (unit-testable; no App required).
fn format_theme_ambience(
    name: &str,
    appearance: &str,
    background: &str,
    border: &str,
    text_accent: &str,
) -> String {
    format!(
        "# theme: {name}\n# appearance: {appearance}\n# background: {background}\n# border: {border}\n# text_accent: {text_accent}\n"
    )
}

/// Best-effort retained-session inventory for dogfood (agent_stdio only).
fn capture_inventory(cx: &mut App) -> Result<String, String> {
    let mut lines = Vec::new();
    let window_count = cx.windows().len();
    lines.push(format!("windows: {window_count}"));
    // Headless often has no cx.active_window() while windows still exist; report honestly.
    if cx.active_window().is_some() {
        lines.push("active_window: yes".into());
    } else if window_count > 0 {
        lines.push("active_window: yes (headless-fallback)".into());
    } else {
        lines.push("active_window: (none)".into());
    }

    // Open paths when workspace APIs are available on the app; keep best-effort.
    // Title from force-drawn outline focus line is enough for tactile inventory.
    for (index, handle) in cx.windows().into_iter().enumerate() {
        match cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear();
            let room = window.a11y_outline(OutlineDetail::Room);
            let title = room
                .lines()
                .find_map(|line| line.strip_prefix("# window: "))
                .unwrap_or("(unknown)")
                .to_string();
            let focus = room
                .lines()
                .find_map(|line| line.strip_prefix("# focus: "))
                .unwrap_or("(unknown)")
                .to_string();
            (title, focus)
        }) {
            Ok((title, focus)) => {
                lines.push(format!("window[{index}].title: {title}"));
                lines.push(format!("window[{index}].focus: {focus}"));
            }
            Err(error) => lines.push(format!("window[{index}].error: {error:#}")),
        }
    }

    Ok(lines.join("\n"))
}

/// Ordered window handles: active first, then remaining (same as snapshot/look).
fn ordered_window_handles(cx: &App) -> Vec<AnyWindowHandle> {
    let mut handles = Vec::new();
    if let Some(active) = cx.active_window() {
        handles.push(active);
    }
    for window in cx.windows() {
        if !handles.contains(&window) {
            handles.push(window);
        }
    }
    handles
}

fn dispatch_a11y_click(
    cx: &mut App,
    node_id: accesskit::NodeId,
    action: accesskit::Action,
    data: Option<accesskit::ActionData>,
) -> Result<(), String> {
    let handles = ordered_window_handles(cx);
    if handles.is_empty() {
        return Err("no windows available for click".to_string());
    }

    let mut saw_missing = false;
    let mut last_update_error: Option<String> = None;
    for (index, handle) in handles.into_iter().enumerate() {
        // Clone data for each window attempt (SetValue payload is cheap).
        let data = data.clone();
        match cx.update_window(handle, |_, window, cx| {
            // Paint so node ids match the tree the agent saw in look/snapshot.
            window.draw(cx).clear();
            window.dispatch_a11y_action(node_id, action, data, cx)
        }) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) if error.starts_with("no a11y node ") => {
                saw_missing = true;
            }
            // Node present in this window but action did not apply — definitive.
            Ok(Err(error)) => return Err(error),
            Err(error) => {
                last_update_error = Some(format!("window {index}: {error:#}"));
            }
        }
    }

    if saw_missing {
        return Err(format!(
            "no a11y node #NodeId({}) in any window (look first, then click that id)",
            u64::from(node_id)
        ));
    }
    Err(last_update_error.unwrap_or_else(|| {
        format!(
            "click failed for #NodeId({}) (no window accepted the action)",
            u64::from(node_id)
        )
    }))
}

/// Merge per-window interactive outlines. Single non-empty window is returned as-is;
/// multiple get `--- window N ---` separators (`N` is 0-based walk order: active first).
fn merge_window_outlines(sections: &[(usize, String)]) -> String {
    match sections {
        [] => String::new(),
        [(_, only)] => only.clone(),
        many => many
            .iter()
            .map(|(index, outline)| format!("--- window {index} ---\n{outline}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn dispatch_action(cx: &mut App, name: &str, data: Option<Value>) -> Result<()> {
    // Outer context omits `{name}` — GPUI `ActionBuildError` Display already includes it.
    let action = cx.build_action(name, data).context(
        "build action failed; names come from method:actions (double-colon form, e.g. crate::Action)",
    )?;
    if let Some(handle) = active_window_handle(cx) {
        handle
            .update(cx, |_, window, cx| {
                window.dispatch_action(action, cx);
            })
            .with_context(|| format!("failed to dispatch action `{name}` on active window"))?;
    } else {
        cx.dispatch_action(action.as_ref());
    }
    Ok(())
}

fn dispatch_keys(cx: &mut App, keys: &str) -> Result<()> {
    let keystroke = Keystroke::parse(keys).context("invalid keystroke")?;
    let handle = active_window_handle(cx).context("no active window for keystroke dispatch")?;
    handle
        .update(cx, |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx);
        })
        .context("failed to dispatch keystroke")?;
    Ok(())
}

fn open_target(cx: &mut App, target: &str) -> Result<()> {
    let path = if target.starts_with("file://") {
        target.to_string()
    } else if target.starts_with("zed://")
        || target.starts_with("http://")
        || target.starts_with("https://")
    {
        target.to_string()
    } else {
        format!("file://{target}")
    };

    // Prefer the existing (or first) window so dogfood does not leave an empty
    // shell window beside the real project after method:open.
    OpenListener::global(cx).open(RawOpenRequest {
        urls: vec![path],
        open_behavior: Some(cli::OpenBehavior::ExistingWindow),
        ..Default::default()
    });
    Ok(())
}

fn active_window_handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.active_window().or_else(|| cx.windows().first().copied())
}

fn list_action_names() -> Vec<String> {
    let mut names = gpui::generate_list_of_all_registered_actions()
        .map(|action| action.name.to_string())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_toon_roundtrip_request() {
        let original = AgentRequest {
            method: "action".into(),
            id: Some("42".into()),
            name: Some("workspace:ToggleLeftDock".into()),
            data: Some(json!({"visible": true})),
            keys: None,
            url: None,
            path: None,
            ms: None,
            detail: None,
            node: None,
            a11y_action: None,
            value: None,
        };
        let encoded = encode_toon(&serde_json::to_value(&original).unwrap()).unwrap();
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_toon_roundtrip_response() {
        let mut fields = Map::new();
        fields.insert(
            "snapshot@text".into(),
            Value::String("[Button] \"OK\"".into()),
        );
        let encoded = encode_response(Some("1"), fields.clone()).unwrap();
        let value = decode_toon(&encoded).unwrap();
        assert_eq!(value.get("ok"), Some(&Value::Bool(true)));
        assert_eq!(value.get("id"), Some(&Value::String("1".into())));
        assert_eq!(
            value.get("snapshot@text"),
            Some(&Value::String("[Button] \"OK\"".into()))
        );
    }

    #[test]
    fn test_decode_snapshot_request() {
        let request = decode_request("method:snapshot\nid:7").unwrap();
        assert_eq!(request.method, "snapshot");
        assert_eq!(request.id.as_deref(), Some("7"));
        assert_eq!(request.detail, None);
        assert_eq!(
            parse_outline_detail(request.detail.as_deref()).unwrap(),
            OutlineDetail::Rich,
            "snapshot without detail defaults to rich"
        );
    }

    #[test]
    fn test_parse_outline_detail_table() {
        let cases: &[(&str, Result<OutlineDetail, ()>)] = &[
            ("", Ok(OutlineDetail::Rich)),
            ("rich", Ok(OutlineDetail::Rich)),
            ("  rich  ", Ok(OutlineDetail::Rich)),
            ("compact", Ok(OutlineDetail::Compact)),
            ("room", Ok(OutlineDetail::Room)),
            ("ROOM", Err(())),
            ("full", Err(())),
            ("richroom", Err(())),
        ];
        for (raw, expected) in cases {
            let got = parse_outline_detail(Some(raw));
            match expected {
                Ok(detail) => assert_eq!(got.as_ref().ok(), Some(detail), "input {raw:?}"),
                Err(()) => {
                    assert!(got.is_err(), "expected error for {raw:?}, got {got:?}");
                    let err = got.unwrap_err();
                    assert!(
                        err.contains("expected compact|rich|room"),
                        "error shape: {err}"
                    );
                }
            }
        }
        assert_eq!(
            parse_outline_detail(None).unwrap(),
            OutlineDetail::Rich,
            "missing detail defaults to rich"
        );
    }

    #[test]
    fn test_decode_look_room_and_click() {
        let look = decode_request("method:look\nid:1\ndetail:room").unwrap();
        assert_eq!(look.method, "look");
        assert_eq!(look.detail.as_deref(), Some("room"));
        assert_eq!(
            parse_outline_detail(look.detail.as_deref()).unwrap(),
            OutlineDetail::Room
        );
        // look without detail still decodes; default is rich (same as snapshot).
        let look_default = decode_request("method:look\nid:1b").unwrap();
        assert_eq!(look_default.method, "look");
        assert_eq!(look_default.detail, None);
        assert_eq!(
            parse_outline_detail(look_default.detail.as_deref()).unwrap(),
            OutlineDetail::Rich
        );

        let inventory = decode_request("method:inventory\nid:inv1").unwrap();
        assert_eq!(inventory.method, "inventory");
        assert_eq!(inventory.id.as_deref(), Some("inv1"));

        let theme = decode_request("method:theme\nid:t1").unwrap();
        assert_eq!(theme.method, "theme");
        assert_eq!(theme.id.as_deref(), Some("t1"));
        let feel = decode_request("method:feel\nid:t2").unwrap();
        assert_eq!(feel.method, "feel");

        let click = decode_request("method:click\nid:2\nnode:42\na11y_action:focus").unwrap();
        assert_eq!(click.method, "click");
        assert_eq!(click.node.as_deref(), Some("42"));
        assert_eq!(parse_node_id("42").unwrap(), accesskit::NodeId(42));
        assert_eq!(parse_node_id("NodeId(7)").unwrap(), accesskit::NodeId(7));
        assert_eq!(parse_node_id("#NodeId(42)").unwrap(), accesskit::NodeId(42));
        assert_eq!(parse_node_id("#99").unwrap(), accesskit::NodeId(99));
        assert_eq!(
            parse_a11y_action(click.a11y_action.as_deref()).unwrap(),
            accesskit::Action::Focus
        );
        // Bare decimal preferred; TOON number for node normalizes to string.
        let click_num = decode_request("method:click\nid:3\nnode:99").unwrap();
        assert_eq!(click_num.node.as_deref(), Some("99"));
        assert_eq!(
            parse_a11y_action(None).unwrap(),
            accesskit::Action::Click,
            "missing a11y_action defaults to click"
        );
        assert_eq!(
            parse_a11y_action(Some("set_value")).unwrap(),
            accesskit::Action::SetValue
        );
        assert_eq!(
            parse_a11y_action(Some("expand")).unwrap(),
            accesskit::Action::Expand
        );
        assert_eq!(
            parse_a11y_action(Some("collapse")).unwrap(),
            accesskit::Action::Collapse
        );
        let bad_action = parse_a11y_action(Some("hover"));
        assert!(bad_action.is_err(), "{bad_action:?}");
        assert!(
            bad_action.unwrap_err().contains("click|focus|set_value"),
            "error should list allowed verbs"
        );
        assert!(parse_node_id("").is_err());
        assert!(parse_node_id("NodeId").is_err());
        assert!(parse_node_id("#NodeId()").is_err());
        assert!(parse_node_id("not-a-node").is_err());
        let missing_node = decode_request("method:click\nid:4").unwrap();
        assert_eq!(missing_node.method, "click");
        assert_eq!(missing_node.node, None);

        // set_value requires value/data; other actions do not.
        assert!(a11y_action_data(accesskit::Action::SetValue, None, None).is_err());
        assert!(matches!(
            a11y_action_data(accesskit::Action::SetValue, Some("hi"), None).unwrap(),
            Some(accesskit::ActionData::Value(text)) if text.as_ref() == "hi"
        ));
        assert!(matches!(
            a11y_action_data(
                accesskit::Action::SetValue,
                None,
                Some(&Value::String("via-data".into()))
            )
            .unwrap(),
            Some(accesskit::ActionData::Value(text)) if text.as_ref() == "via-data"
        ));
        assert!(matches!(
            a11y_action_data(
                accesskit::Action::SetValue,
                None,
                Some(&json!(1.5))
            )
            .unwrap(),
            Some(accesskit::ActionData::NumericValue(n)) if (n - 1.5).abs() < f64::EPSILON
        ));
        assert_eq!(
            a11y_action_data(accesskit::Action::Click, None, None).unwrap(),
            None
        );
        assert_eq!(
            a11y_action_data(accesskit::Action::Expand, None, None).unwrap(),
            None
        );
        let set_value_req =
            decode_request("method:click\nid:5\nnode:9\na11y_action:set_value\nvalue:hello")
                .unwrap();
        assert_eq!(set_value_req.value.as_deref(), Some("hello"));
        assert_eq!(
            a11y_action_data(
                accesskit::Action::SetValue,
                set_value_req.value.as_deref(),
                set_value_req.data.as_ref()
            )
            .unwrap(),
            Some(accesskit::ActionData::Value("hello".into()))
        );
    }

    #[test]
    fn test_merge_window_outlines_empty_single_and_multi() {
        assert_eq!(merge_window_outlines(&[]), "");
        assert_eq!(
            merge_window_outlines(&[(0, "[Button] \"A\"".into())]),
            "[Button] \"A\""
        );
        let merged = merge_window_outlines(&[
            (0, "[Button] \"A\"".into()),
            (2, "[TextInput] value=\"x\"".into()),
        ]);
        assert_eq!(
            merged,
            "--- window 0 ---\n[Button] \"A\"\n--- window 2 ---\n[TextInput] value=\"x\""
        );
    }

    #[test]
    fn test_blank_line_delimited_document_dispatch() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut document = "method:wait\nid:3\nms:250".to_string();
        dispatch_stdin_document(&mut document, &tx).unwrap();
        assert!(document.is_empty());
        let request = rx.try_recv().unwrap();
        assert_eq!(request.method, "wait");
        assert_eq!(request.ms, Some(250));
    }

    #[test]
    fn test_decode_wait_request() {
        let request = decode_request("method:wait\nid:3\nms:250").unwrap();
        assert_eq!(request.method, "wait");
        assert_eq!(request.ms, Some(250));
    }

    #[test]
    fn test_list_action_names_is_sorted_unique() {
        let names = list_action_names();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn test_format_theme_ambience_named_tokens_only() {
        let text = format_theme_ambience(
            "One Dark",
            "dark",
            "hsla(210.00, 13.00%, 13.00%, 1.00)",
            "hsla(210.00, 10.00%, 40.00%, 1.00)",
            "hsla(207.00, 82.00%, 66.00%, 1.00)",
        );
        assert_eq!(
            text,
            "# theme: One Dark\n\
             # appearance: dark\n\
             # background: hsla(210.00, 13.00%, 13.00%, 1.00)\n\
             # border: hsla(210.00, 10.00%, 40.00%, 1.00)\n\
             # text_accent: hsla(207.00, 82.00%, 66.00%, 1.00)\n"
        );
        // Honest ambience only — no per-control CSS dump fields.
        assert!(!text.contains("border-radius"), "{text}");
        assert!(!text.contains("1px solid"), "{text}");
    }

    #[test]
    fn test_seed_agent_stdio_settings_writes_trust_once() {
        let dir = tempfile::tempdir().unwrap();
        seed_agent_stdio_settings(dir.path()).unwrap();
        let settings = dir.path().join("config/settings.json");
        let body = std::fs::read_to_string(&settings).unwrap();
        assert!(body.contains("trust_all_worktrees"));
        assert!(body.contains("true"));
        // Second call must not overwrite custom settings.
        std::fs::write(&settings, "{\"custom\":true}\n").unwrap();
        seed_agent_stdio_settings(dir.path()).unwrap();
        let body = std::fs::read_to_string(&settings).unwrap();
        assert!(body.contains("custom"));
        assert!(!body.contains("trust_all_worktrees"));
    }

    #[test]
    fn test_decode_open_keys_actions_shutdown_and_compact() {
        let open = decode_request("method:open\nid:o1\npath:/tmp/readme.md").unwrap();
        assert_eq!(open.method, "open");
        assert_eq!(open.path.as_deref(), Some("/tmp/readme.md"));
        assert_eq!(open.url, None);

        let open_url = decode_request("method:open\nid:o2\nurl:https://example.com").unwrap();
        assert_eq!(open_url.url.as_deref(), Some("https://example.com"));
        assert_eq!(open_url.path, None);

        let keys = decode_request("method:keys\nid:k1\nkeys:ctrl-p").unwrap();
        assert_eq!(keys.method, "keys");
        assert_eq!(keys.keys.as_deref(), Some("ctrl-p"));

        let actions = decode_request("method:actions\nid:a1").unwrap();
        assert_eq!(actions.method, "actions");
        assert_eq!(actions.id.as_deref(), Some("a1"));

        let shutdown = decode_request("method:shutdown\nid:s1").unwrap();
        assert_eq!(shutdown.method, "shutdown");

        let action = decode_request("method:action\nid:act1\nname:agent::ToggleFocus").unwrap();
        assert_eq!(action.method, "action");
        assert_eq!(action.name.as_deref(), Some("agent::ToggleFocus"));

        let compact = decode_request("method:snapshot\nid:c1\ndetail:compact").unwrap();
        assert_eq!(compact.detail.as_deref(), Some("compact"));
        assert_eq!(
            parse_outline_detail(compact.detail.as_deref()).unwrap(),
            OutlineDetail::Compact
        );
        let look_compact = decode_request("method:look\nid:c2\ndetail:compact").unwrap();
        assert_eq!(look_compact.method, "look");
        assert_eq!(
            parse_outline_detail(look_compact.detail.as_deref()).unwrap(),
            OutlineDetail::Compact
        );
    }
}

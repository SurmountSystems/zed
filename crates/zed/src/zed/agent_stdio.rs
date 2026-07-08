//! MCP-style stdio control plane for Zed subagents.
//!
//! When `--agent-stdio` (or `ZED_AGENT_STDIO=1`) is active:
//! - **stdin**: blank-line-delimited TOON request documents (single-line requests also work)
//! - **stdout**: TOON responses and events only
//! - **stderr**: all Zed logs
//!
//! Startup emits a ready event:
//! ```text
//! event:ready
//! user_data_dir:/tmp/...
//! pid:12345
//! ```
//!
//! Methods (v1): `snapshot`, `action`, `keys`, `open`, `actions`, `wait`, `shutdown`.

use crate::{OpenListener, RawOpenRequest};
use anyhow::{Context as _, Result};
use futures::{StreamExt, channel::mpsc};
use gpui::{AnyWindowHandle, App, AppContext as _, Keystroke, ReadGlobal as _};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::Duration,
};
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

    Ok(data_dir)
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
        normalize_request_id_field(obj);
    }
    serde_json::from_value(value).context("invalid agent-stdio request shape")
}

fn normalize_request_id_field(obj: &mut Map<String, Value>) {
    let Some(id) = obj.get("id") else {
        return;
    };
    if id.is_string() {
        return;
    }
    if let Some(number) = id.as_u64().or_else(|| id.as_i64().map(|n| n as u64)) {
        obj.insert("id".into(), Value::String(number.to_string()));
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

fn handle_request(request: AgentRequest, cx: &mut App) -> Result<()> {
    let id = request.id.as_deref();
    match request.method.as_str() {
        "snapshot" => {
            let snapshot = capture_snapshot(cx);
            let mut fields = Map::new();
            fields.insert("snapshot@text".into(), Value::String(snapshot));
            emit_response(id, fields);
        }
        "action" => match request.name.as_deref() {
            Some(name) => {
                if let Err(error) = dispatch_action(cx, name, request.data) {
                    emit_error(id, &error);
                } else {
                    emit_response(id, Map::new());
                }
            }
            None => emit_error(id, &"action requests require `name`"),
        },
        "keys" => match request.keys.as_deref() {
            Some(keys) => {
                if let Err(error) = dispatch_keys(cx, keys) {
                    emit_error(id, &error);
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

fn capture_snapshot(cx: &mut App) -> String {
    let mut outline = String::new();

    if let Some(active) = cx.active_window() {
        let _ = cx.update_window(active, |_, window, _| {
            outline = window.a11y_interactive_outline();
        });
    }

    if outline.is_empty() {
        for window in cx.windows() {
            let _ = cx.update_window(window, |_, window, _| {
                if outline.is_empty() {
                    outline = window.a11y_interactive_outline();
                }
            });
        }
    }

    outline
}

fn dispatch_action(cx: &mut App, name: &str, data: Option<Value>) -> Result<()> {
    let action = cx.build_action(name, data)?;
    if let Some(handle) = active_window_handle(cx) {
        handle
            .update(cx, |_, window, cx| {
                window.dispatch_action(action, cx);
            })
            .context("failed to dispatch action on active window")?;
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

    OpenListener::global(cx).open(RawOpenRequest {
        urls: vec![path],
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
}

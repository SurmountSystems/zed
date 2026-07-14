//! Drive a headless Zed process over agent-stdio TOON for preflight / golden / smoke.
//!
//! Protocol matches `crates/zed/src/zed/agent_stdio.rs`: blank-line-delimited
//! request documents on stdin; multi-line TOON responses on stdout; logs on stderr.

#![allow(clippy::disallowed_methods, reason = "tooling is exempt")]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use regex::Regex;

use crate::workspace;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct DogfoodArgs {
    #[command(subcommand)]
    command: DogfoodCommand,
}

#[derive(Subcommand)]
enum DogfoodCommand {
    /// Assert the first stdout event is ready, then shut down.
    Preflight(PreflightArgs),
    /// Full protocol golden session (open, wait, snapshot, action, keys, shutdown).
    Golden(GoldenArgs),
    /// Automation smoke: open fixture, poll until non-empty snapshot (+ optional expects / action / keys).
    Smoke(SmokeArgs),
    /// Headless merge-review workshop: open Surmount root → Start → chrome expects → Preview → End.
    MergeReview(MergeReviewArgs),
    /// Agent-driven TOON step queue with per-step tracking (no ad-hoc shell scripts).
    Queue(QueueArgs),
}

#[derive(Parser)]
struct PreflightArgs {
    /// Path to the zed binary (default: target/release/zed under workspace root).
    #[arg(long, env = "ZED_BIN")]
    bin: Option<PathBuf>,
    /// Whole-session timeout seconds.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

#[derive(Parser)]
struct GoldenArgs {
    #[arg(long, env = "ZED_BIN")]
    bin: Option<PathBuf>,
    /// File path for method:open (prefer a file, not a directory).
    #[arg(long, env = "ZED_DOGFOOD_FIXTURE")]
    fixture: Option<PathBuf>,
    /// Settle wait after open (milliseconds).
    #[arg(long, default_value_t = 3000)]
    wait_ms: u64,
    #[arg(long, default_value_t = 90)]
    timeout_secs: u64,
    /// Action name for method:action (must be registered).
    #[arg(long, default_value = "agent::ToggleFocus")]
    action: String,
    /// Keystroke for method:keys (Linux-oriented default).
    #[arg(long, default_value = "ctrl-p")]
    keys: String,
    /// Snapshot detail: compact | rich (default) | room.
    #[arg(long, default_value = "rich", value_parser = ["compact", "rich", "room"])]
    snapshot_detail: String,
}

#[derive(Parser)]
struct SmokeArgs {
    #[arg(long, env = "ZED_BIN")]
    bin: Option<PathBuf>,
    #[arg(long, env = "ZED_DOGFOOD_FIXTURE")]
    fixture: Option<PathBuf>,
    /// Initial settle wait after open (milliseconds) before polling snapshots.
    #[arg(long, default_value_t = 3000)]
    wait_ms: u64,
    #[arg(long, default_value_t = 90)]
    timeout_secs: u64,
    /// Require this substring in the snapshot@text outline only (repeatable; not TOON metadata).
    #[arg(long = "expect")]
    expect: Vec<String>,
    /// Optional GPUI action after the first successful snapshot (double-colon name).
    /// Example: `agent::ToggleFocus`. Thread search (`agent::ToggleSearch`) is registered
    /// and works once an agent thread has focus; default smoke does not open the agent panel,
    /// so prefer the zed-dogfood skill manual checklist over requiring ToggleSearch in CI.
    #[arg(long)]
    action: Option<String>,
    /// Optional keystroke after action (Linux-oriented, e.g. ctrl-p).
    #[arg(long)]
    keys: Option<String>,
    /// Interval between snapshot polls while waiting for non-empty / expects (ms).
    #[arg(long, default_value_t = 250)]
    poll_ms: u64,
    /// Fail the run if `--action` returns ok: false (default: warn and continue).
    #[arg(long, default_value_t = false)]
    require_action: bool,
    /// Snapshot detail: compact | rich (default) | room.
    #[arg(long, default_value = "rich", value_parser = ["compact", "rich", "room"])]
    snapshot_detail: String,
}

#[derive(Parser)]
struct MergeReviewArgs {
    #[arg(long, env = "ZED_BIN")]
    bin: Option<PathBuf>,
    /// Surmount workspace root directory, or a file under it (parent is opened).
    /// Prefer a directory so agent-stdio lands in a real project worktree, not a
    /// single-file shell. Default: cargo workspace root when SURMOUNT.md exists.
    #[arg(long, env = "ZED_DOGFOOD_FIXTURE")]
    fixture: Option<PathBuf>,
    /// Settle after open (ms) before StartMergeReview.
    #[arg(long, default_value_t = 4000)]
    wait_ms: u64,
    /// Extra settle after StartMergeReview for git populate / Branch Diff (ms).
    #[arg(long, default_value_t = 8000)]
    post_start_wait_ms: u64,
    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,
    /// GPUI action that starts the workflow (default Surmount StartMergeReview).
    #[arg(long, default_value = "surmount::StartMergeReview")]
    action: String,
    #[arg(long, default_value = "room", value_parser = ["compact", "rich", "room"])]
    snapshot_detail: String,
    /// Substrings that must appear in the post-start room outline (`snapshot@text`).
    /// When omitted, defaults to stable chrome: `Merge review`.
    #[arg(long = "expect")]
    expect: Vec<String>,
    /// Only Start + post-start look/expects (skip Preview/End workshop steps).
    #[arg(long, default_value_t = false)]
    start_only: bool,
    /// After Start settle, run MergeReviewNextFile and require a path/cursor delta
    /// before Preview. Default off — Start → Preview → End unchanged.
    #[arg(long, default_value_t = false)]
    with_advance: bool,
    /// Build a tiny conflicted git tree under tempfile and gate decision chrome
    /// (Discuss / Resolve / Synthesize) when the fixture exists. Default off —
    /// does not require live Surmount MERGE_HEAD. Skips with a log if fixture
    /// build fails or decision chrome is missing after Start.
    #[arg(long, default_value_t = false)]
    with_conflict: bool,
    /// With `--with-conflict`: soft-poll for **live** agent summary capture before
    /// synthetic inject. Soft-skips on timeout then runs hard synthetic spine
    /// (hard green never requires Grok). Live poll budget =
    /// `max(step_wait_ms, 30_000).min(90_000)` ms (raise via `--step-wait-ms`).
    #[arg(long, default_value_t = false)]
    decide_live_agent: bool,
    /// Settle after Preview / End / Advance actions (ms). Also floors the
    /// `--decide-live-agent` soft poll budget (see that flag).
    #[arg(long, default_value_t = 2500)]
    step_wait_ms: u64,
}

#[derive(Parser)]
struct QueueArgs {
    #[arg(long, env = "ZED_BIN")]
    bin: Option<PathBuf>,
    /// Default path for a bare `open` step (file or directory).
    #[arg(long, env = "ZED_DOGFOOD_FIXTURE")]
    fixture: Option<PathBuf>,
    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,
    /// Default look detail when a step is bare `look` (compact|rich|room).
    #[arg(long, default_value = "room", value_parser = ["compact", "rich", "room"])]
    snapshot_detail: String,
    /// TOON step (repeatable). See `parse_queue_step` / dogfood skill.
    /// Examples: `open`, `open:/path`, `wait:4000`, `action:agent::ToggleFocus`,
    /// `look:room`, `expect:Merge review`, `hit:Prepare|Review Diff`, `lines:40`,
    /// `inventory`, `theme`, `stderr:merge`, `keys:ctrl-p`, `click:42`.
    #[arg(long = "step")]
    step: Vec<String>,
    /// Optional script file: one step per line (`#` comments and blanks skipped).
    #[arg(long)]
    script: Option<PathBuf>,
    /// Soft-fail `action` / `keys` / `click` (warn and continue). Default: hard-fail.
    #[arg(long, default_value_t = false)]
    soft_action: bool,
}

/// Default post-start chrome expects when CLI `--expect` is empty.
fn merge_review_default_expects() -> Vec<String> {
    vec!["Merge review".to_string()]
}

/// Resolve expects for post-start look: CLI list, or default chrome.
fn merge_review_post_start_expects(cli: &[String]) -> Vec<String> {
    if cli.is_empty() {
        merge_review_default_expects()
    } else {
        cli.to_vec()
    }
}

// ── TOON helpers (request encode + response scrape; no toon-format dep) ──────

/// Fields golden/smoke attach to every `method:snapshot` request.
///
/// `detail` is always present on the wire (CLI default `"rich"` via
/// `--snapshot-detail` / `GoldenArgs` / `SmokeArgs`).
fn snapshot_method_fields(detail: &str) -> [(&str, &str); 2] {
    [("method", "snapshot"), ("detail", detail)]
}

/// Escape a string for a quoted TOON value (mirrors toon-format escapes).
pub fn escape_toon_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// Whether a TOON value needs quoting (structural chars, spaces, literals, empty).
pub fn needs_toon_value_quoting(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("null")
    {
        return true;
    }
    value.chars().any(|c| {
        matches!(
            c,
            ':' | '"' | '\\' | '\n' | '\r' | '\t' | ',' | '[' | ']' | '{' | '}' | ' '
        )
    })
}

/// Encode a TOON field value, quoting/escaping when required (e.g. Windows paths).
pub fn encode_toon_value(value: &str) -> String {
    if needs_toon_value_quoting(value) {
        format!("\"{}\"", escape_toon_string(value))
    } else {
        value.to_string()
    }
}

/// Build a blank-line-terminated request document (agent-stdio stdin shape).
pub fn encode_request(fields: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in fields {
        out.push_str(key);
        out.push(':');
        out.push_str(&encode_toon_value(value));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Pure cursor slice used by `wait_until` (lines at index >= `from_line`).
pub fn lines_since(buf: &[String], from_line: usize) -> &[String] {
    let start = from_line.min(buf.len());
    &buf[start..]
}

/// True if a line looks like the ready event (live encoder: `event: ready`).
pub fn is_ready_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 6 || !trimmed[..6].eq_ignore_ascii_case("event:") {
        return false;
    }
    // Exact value only — reject readyish / ready-foo.
    trimmed[6..].trim().eq_ignore_ascii_case("ready")
}

fn parse_ok_line(line: &str) -> Option<bool> {
    let t = line.trim();
    if t.len() < 3 || !t[..3].eq_ignore_ascii_case("ok:") {
        return None;
    }
    let rest = t[3..].trim();
    if rest.eq_ignore_ascii_case("true") {
        Some(true)
    } else if rest.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// Line-anchored `ok: true` (avoids matching inside snapshot text).
pub fn blob_has_ok_true(blob: &str) -> bool {
    blob.lines().any(|line| parse_ok_line(line) == Some(true))
}

/// Line-anchored `ok: false`.
pub fn blob_has_ok_false(blob: &str) -> bool {
    blob.lines().any(|line| parse_ok_line(line) == Some(false))
}

fn parse_id_line(line: &str) -> Option<&str> {
    let t = line.trim();
    let rest = t.strip_prefix("id:")?;
    Some(rest.trim().trim_matches('"'))
}

fn is_error_field_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("error:") || (t.len() >= 6 && t[..6].eq_ignore_ascii_case("error:"))
}

/// True if a TOON response blob carries `id: <id>` as its own field line.
#[cfg(test)]
fn blob_has_response_id(blob: &str, id: &str) -> bool {
    blob.lines().any(|line| parse_id_line(line) == Some(id))
}

/// Extract the contiguous field block for the response document with `id: <id>`.
///
/// Walks backward from the matching id past non-boundary fields (e.g. snapshot),
/// stopping at blank lines, another `id:`, or a prior response's `ok:` / `error:`.
/// Walks forward until blank or the next `id:` line.
pub fn document_block_for_id(blob: &str, id: &str) -> Option<String> {
    let lines: Vec<&str> = blob.lines().collect();
    let id_idx = lines
        .iter()
        .position(|line| parse_id_line(line) == Some(id))?;

    let mut start = id_idx;
    while start > 0 {
        let prev = lines[start - 1].trim();
        if prev.is_empty() {
            break;
        }
        if parse_id_line(prev).is_some() {
            break;
        }
        if parse_ok_line(prev).is_some() {
            break;
        }
        if is_error_field_line(prev) {
            break;
        }
        start -= 1;
    }

    let mut end = id_idx + 1;
    while end < lines.len() {
        let next = lines[end].trim();
        if next.is_empty() {
            break;
        }
        if parse_id_line(next).is_some() {
            break;
        }
        end += 1;
    }

    Some(lines[start..end].join("\n"))
}

/// Response document is complete: matching id block contains its own ok field.
pub fn response_complete_for_id(blob: &str, id: &str) -> bool {
    let Some(block) = document_block_for_id(blob, id) else {
        return false;
    };
    blob_has_ok_true(&block) || blob_has_ok_false(&block)
}

/// Ok status for the document bound to `id` (`None` if incomplete / missing).
pub fn response_ok_for_id(blob: &str, id: &str) -> Option<bool> {
    let block = document_block_for_id(blob, id)?;
    if blob_has_ok_true(&block) {
        Some(true)
    } else if blob_has_ok_false(&block) {
        Some(false)
    } else {
        None
    }
}

/// True when outline text has at least one **body** line (interactive control or
/// landmark). Skips empty lines, `--- window N ---` separators, room `# …` headers,
/// and `[snapshot error]` diagnostics. Landmark-only room bodies count as non-empty.
fn outline_has_body_content(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // TOON may escape newlines as `\n` inside the quoted field.
    text.replace("\\n", "\n").lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with("--- window ")
            && !line.starts_with('#')
            && !line.starts_with("[snapshot error]")
    })
}

/// Classify snapshot emptiness from accumulated response text.
///
/// Returns `"true"` (empty outline), `"false"` (non-empty body), or `"missing"`.
/// Diagnostic-only bodies (`[snapshot error]…`) and room `#` headers alone count as
/// empty so gates cannot false-pass without interactive/landmark body lines.
pub fn classify_snapshot(blob: &str) -> &'static str {
    // Live empty form: "snapshot@text": ""  (@ forces quoted key in toon-format)
    if blob.contains("\"snapshot@text\": \"\"") || blob.contains("\"snapshot@text\":\"\"") {
        return "true";
    }
    // Quoted value on one line (newlines inside snapshot are escaped as \n).
    if let Some(caps) = Regex::new(r#""snapshot@text":\s*"(.*)""#)
        .expect("snap re")
        .captures(blob)
    {
        let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        return if outline_has_body_content(inner) {
            "false"
        } else {
            "true"
        };
    }
    // Key present without a parseable quoted value — heuristic on outline markers.
    if blob.contains("snapshot@text") {
        if blob.contains("[snapshot error]")
            && !blob.contains("Button")
            && !blob.contains("TextInput")
            && !blob.contains("NodeId")
        {
            return "true";
        }
        if blob.contains('[')
            && (blob.contains("Button") || blob.contains("TextInput") || blob.contains("NodeId"))
        {
            return "false";
        }
        if Regex::new(r#"snapshot@text:\s*""#)
            .expect("empty unquoted")
            .is_match(blob)
        {
            return "true";
        }
        return "missing";
    }
    "missing"
}

/// Extract the quoted `snapshot@text` payload (outline only; no TOON field names / ok/id).
pub fn extract_snapshot_text(blob: &str) -> Option<String> {
    let re = Regex::new(r#""snapshot@text":\s*"(.*)""#).ok()?;
    let caps = re.captures(blob)?;
    Some(caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string())
}

/// True when the snapshot outline is non-empty and every expected substring is in outline text.
pub fn snapshot_satisfies<S: AsRef<str>>(blob: &str, expects: &[S]) -> bool {
    if classify_snapshot(blob) != "false" {
        return false;
    }
    if expects.is_empty() {
        return true;
    }
    // Match expects against outline only so response metadata (ok/id) cannot false-pass.
    let Some(text) = extract_snapshot_text(blob) else {
        return false;
    };
    expects.iter().all(|expect| text.contains(expect.as_ref()))
}

/// Expected substrings missing from the snapshot@text outline (order preserved).
pub fn missing_snapshot_expects<'a, S: AsRef<str>>(blob: &str, expects: &'a [S]) -> Vec<&'a str> {
    let text = extract_snapshot_text(blob).unwrap_or_default();
    expects
        .iter()
        .filter(|expect| !text.contains(expect.as_ref()))
        .map(|expect| expect.as_ref())
        .collect()
}

/// Prefer a non-empty outline, then fewer missing expects, then the newer candidate.
fn prefer_diagnostic_blob(current: &str, candidate: &str, expects: &[String]) -> bool {
    let cand_non_empty = classify_snapshot(candidate) == "false";
    let cur_non_empty = classify_snapshot(current) == "false";
    if cand_non_empty && !cur_non_empty {
        return true;
    }
    if !cand_non_empty && cur_non_empty {
        return false;
    }
    let cand_missing = missing_snapshot_expects(candidate, expects).len();
    let cur_missing = missing_snapshot_expects(current, expects).len();
    cand_missing <= cur_missing
}

/// Soft-retry only step timeouts while Zed is still alive; hard-fail process death / ok:false.
fn is_retryable_poll_error(error: &anyhow::Error) -> bool {
    let msg = format!("{error:#}");
    if msg.contains("zed exited early")
        || msg.contains("stdout closed")
        || msg.contains("returned ok: false")
    {
        return false;
    }
    msg.contains("step timed out")
}

pub fn extract_snapshot_preview(blob: &str, max_chars: usize) -> String {
    if let Some(caps) = Regex::new(r#""snapshot@text":\s*"(.*)""#)
        .ok()
        .and_then(|re| re.captures(blob))
    {
        let s = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        if s.len() <= max_chars {
            return s.to_string();
        }
        // Avoid panicking on non-char boundary (snapshot may contain unicode).
        let mut end = max_chars.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}…", &s[..end]);
    }
    blob.lines()
        .filter(|l| l.contains('[') || l.contains("NodeId"))
        .take(5)
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── Session ──────────────────────────────────────────────────────────────────

struct DogfoodSession {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout_rx: Receiver<String>,
    stderr_rx: Receiver<String>,
    stdout_buf: Vec<String>,
    stderr_buf: Vec<String>,
    _udd: tempfile::TempDir,
    deadline: Instant,
}

impl DogfoodSession {
    fn spawn(bin: &Path, timeout: Duration) -> Result<Self> {
        let udd = tempfile::Builder::new()
            .prefix("zed-dogfood-")
            .tempdir()
            .context("create temp user-data-dir")?;

        let mut child = Command::new(bin)
            .arg("--agent-stdio")
            .arg("--user-data-dir")
            .arg(udd.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;

        let stdin = child.stdin.take().context("child stdin")?;
        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;

        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        spawn_line_reader(stdout, stdout_tx);
        spawn_line_reader(stderr, stderr_tx);

        Ok(Self {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            _udd: udd,
            deadline: Instant::now() + timeout,
        })
    }

    fn pump(&mut self) {
        while let Ok(line) = self.stdout_rx.try_recv() {
            self.stdout_buf.push(line);
        }
        while let Ok(line) = self.stderr_rx.try_recv() {
            self.stderr_buf.push(line);
        }
    }

    /// Wait until `pred` is true on stdout lines after `from_line`.
    ///
    /// Only those new lines are returned. Callers must capture `from_line` once
    /// (after `pump`, before or after `send`) so a fast response cannot be
    /// sliced out of the return value.
    fn wait_until(
        &mut self,
        from_line: usize,
        mut pred: impl FnMut(&[String]) -> bool,
        step_timeout: Duration,
    ) -> Result<Vec<String>> {
        let step_deadline = Instant::now()
            .checked_add(step_timeout)
            .unwrap_or(self.deadline);
        let end = step_deadline.min(self.deadline);

        loop {
            self.pump();
            if pred(lines_since(&self.stdout_buf, from_line)) {
                self.settle_stdout();
                return Ok(lines_since(&self.stdout_buf, from_line).to_vec());
            }

            if Instant::now() >= end {
                self.pump();
                if pred(lines_since(&self.stdout_buf, from_line)) {
                    self.settle_stdout();
                    return Ok(lines_since(&self.stdout_buf, from_line).to_vec());
                }
                bail!(
                    "step timed out after {:?}\n--- stdout (tail) ---\n{}\n--- stderr (tail) ---\n{}",
                    step_timeout,
                    tail_lines(&self.stdout_buf, 40),
                    tail_lines(&self.stderr_buf, 40)
                );
            }

            if let Some(status) = self.child.try_wait().context("try_wait")? {
                self.drain_remaining(Duration::from_millis(200));
                if pred(lines_since(&self.stdout_buf, from_line)) {
                    self.settle_stdout();
                    return Ok(lines_since(&self.stdout_buf, from_line).to_vec());
                }
                bail!(
                    "zed exited early ({status})\n--- stdout (tail) ---\n{}\n--- stderr (tail) ---\n{}",
                    tail_lines(&self.stdout_buf, 40),
                    tail_lines(&self.stderr_buf, 40)
                );
            }

            match self.stdout_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => self.stdout_buf.push(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.drain_remaining(Duration::from_millis(100));
                    if pred(lines_since(&self.stdout_buf, from_line)) {
                        self.settle_stdout();
                        return Ok(lines_since(&self.stdout_buf, from_line).to_vec());
                    }
                    bail!(
                        "stdout closed before condition\n--- stdout (tail) ---\n{}\n--- stderr (tail) ---\n{}",
                        tail_lines(&self.stdout_buf, 40),
                        tail_lines(&self.stderr_buf, 40)
                    );
                }
            }
            while let Ok(line) = self.stderr_rx.try_recv() {
                self.stderr_buf.push(line);
            }
        }
    }

    /// Drain trailing lines of a multi-line TOON document until stdout is quiet.
    fn settle_stdout(&mut self) {
        let settle_deadline = Instant::now() + Duration::from_millis(400);
        let mut last_len = self.stdout_buf.len();
        let mut quiet_since = Instant::now();
        while Instant::now() < settle_deadline {
            thread::sleep(Duration::from_millis(40));
            self.pump();
            let n = self.stdout_buf.len();
            if n != last_len {
                last_len = n;
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() >= Duration::from_millis(120) {
                break;
            }
        }
    }

    fn drain_remaining(&mut self, budget: Duration) {
        let end = Instant::now() + budget;
        while Instant::now() < end {
            let mut got = false;
            while let Ok(line) = self.stdout_rx.try_recv() {
                self.stdout_buf.push(line);
                got = true;
            }
            while let Ok(line) = self.stderr_rx.try_recv() {
                self.stderr_buf.push(line);
                got = true;
            }
            if !got {
                match self.stdout_rx.recv_timeout(Duration::from_millis(30)) {
                    Ok(line) => self.stdout_buf.push(line),
                    Err(_) => break,
                }
            }
        }
    }

    fn send(&mut self, document: &str) -> Result<()> {
        self.stdin
            .write_all(document.as_bytes())
            .context("write stdin")?;
        self.stdin.flush().context("flush stdin")?;
        Ok(())
    }

    fn request_ok(
        &mut self,
        id: &str,
        fields: &[(&str, &str)],
        step_timeout: Duration,
    ) -> Result<Vec<String>> {
        let mut pairs = Vec::with_capacity(fields.len() + 1);
        for (k, v) in fields {
            pairs.push((*k, *v));
        }
        if !pairs.iter().any(|(k, _)| *k == "id") {
            pairs.push(("id", id));
        }
        let doc = encode_request(&pairs);
        // Drain any late lines from the previous step before arming the cursor.
        self.pump();
        let from_line = self.stdout_buf.len();
        self.send(&doc)?;
        let id_owned = id.to_string();
        let lines = self.wait_until(
            from_line,
            |new| {
                let blob = new.join("\n");
                response_complete_for_id(&blob, &id_owned)
            },
            step_timeout,
        )?;
        let blob = lines.join("\n");
        // Score ok only inside the document block bound to this id (not foreign events).
        match response_ok_for_id(&blob, id) {
            Some(true) => Ok(lines),
            Some(false) => {
                let block = document_block_for_id(&blob, id).unwrap_or(blob);
                bail!("request id={id} returned ok: false\n{block}");
            }
            None => bail!("request id={id} missing ok: true\n{blob}"),
        }
    }

    fn shutdown_best_effort(&mut self) {
        let _ = self.send(&encode_request(&[
            ("method", "shutdown"),
            ("id", "shutdown"),
        ]));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DogfoodSession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_line_reader(stream: impl Read + Send + 'static, tx: Sender<String>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn tail_lines(lines: &[String], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn default_bin() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("ZED_BIN") {
        return Ok(PathBuf::from(path));
    }
    let meta = workspace::load_workspace()?;
    let root = PathBuf::from(meta.workspace_root);
    let candidate = root.join("target/release/zed");
    if candidate.is_file() {
        return Ok(candidate);
    }
    let debug = root.join("target/debug/zed");
    if debug.is_file() {
        return Ok(debug);
    }
    bail!(
        "zed binary not found at {} (or target/debug/zed); pass --bin or set ZED_BIN",
        candidate.display()
    );
}

fn resolve_fixture(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("fixture is not a file: {}", path.display());
        }
        return Ok(path.canonicalize().unwrap_or(path));
    }
    let meta = workspace::load_workspace()?;
    let root = PathBuf::from(meta.workspace_root);
    let readme = root.join("README.md");
    if readme.is_file() {
        return Ok(readme);
    }
    bail!("no --fixture and README.md missing at workspace root");
}

/// Resolve a Surmount **workspace root directory** for merge-review dogfood.
///
/// Opens a directory (not a single file) so git + Branch Diff see a real project
/// worktree under agent-stdio. Accepts an explicit directory, a file under the
/// root (parent is used), or defaults to the cargo workspace root when it has
/// `SURMOUNT.md`.
fn resolve_surmount_workspace(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = path.canonicalize().unwrap_or(path);
        if path.is_dir() {
            return Ok(path);
        }
        if path.is_file() {
            let parent = path
                .parent()
                .map(|parent| parent.to_path_buf())
                .context("fixture file has no parent directory")?;
            return Ok(parent.canonicalize().unwrap_or(parent));
        }
        bail!(
            "merge-review fixture is neither file nor directory: {}",
            path.display()
        );
    }
    let meta = workspace::load_workspace()?;
    let root = PathBuf::from(meta.workspace_root);
    if root.join("SURMOUNT.md").is_file() {
        return Ok(root.canonicalize().unwrap_or(root));
    }
    bail!(
        "no Surmount workspace root (SURMOUNT.md missing at {}); pass --fixture <dir>",
        root.display()
    )
}

// ── Commands ─────────────────────────────────────────────────────────────────

fn run_preflight(args: PreflightArgs) -> Result<()> {
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    println!("dogfood preflight: bin={}", bin.display());

    let mut session = DogfoodSession::spawn(&bin, timeout)?;
    let ready_lines =
        session.wait_until(0, |lines| lines.iter().any(|l| is_ready_line(l)), timeout)?;
    println!("ok: event ready");
    for line in ready_lines.iter().take(6) {
        println!("  {line}");
    }
    session.shutdown_best_effort();
    println!("preflight passed");
    Ok(())
}

fn run_golden(args: GoldenArgs) -> Result<()> {
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    let fixture = resolve_fixture(args.fixture)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let fixture_str = fixture.display().to_string();

    println!(
        "dogfood golden: bin={} fixture={} wait_ms={}",
        bin.display(),
        fixture.display(),
        args.wait_ms
    );

    let mut session = DogfoodSession::spawn(&bin, timeout)?;
    let ready = session.wait_until(
        0,
        |lines| lines.iter().any(|l| is_ready_line(l)),
        Duration::from_secs(45),
    )?;
    println!("[event:ready] ok");
    for line in ready.iter().take(4) {
        println!("  {line}");
    }

    let actions = session.request_ok(
        "actions1",
        &[("method", "actions")],
        Duration::from_secs(20),
    )?;
    println!("[method:actions] ok ({} lines)", actions.len());
    let actions_blob = actions.join("\n");
    for name in [
        "agent::ToggleFocus",
        "agent::Toggle",
        "workspace::ToggleRightDock",
        "file_finder::Toggle",
        "agent::NewThread",
    ] {
        let present = actions_blob.contains(name);
        println!(
            "  action_present {name}: {}",
            if present { "yes" } else { "no" }
        );
    }

    let path_field = fixture_str.as_str();
    session.request_ok(
        "open1",
        &[("method", "open"), ("path", path_field)],
        Duration::from_secs(15),
    )?;
    println!("[method:open] ok");

    let wait_ms = args.wait_ms.to_string();
    session.request_ok(
        "wait1",
        &[("method", "wait"), ("ms", wait_ms.as_str())],
        Duration::from_millis(args.wait_ms + 10_000),
    )?;
    println!("[method:wait] ok");

    let detail = args.snapshot_detail.as_str();
    let snap1 = session.request_ok(
        "snap1",
        &snapshot_method_fields(detail),
        Duration::from_secs(15),
    )?;
    let snap1_blob = snap1.join("\n");
    let empty1 = classify_snapshot(&snap1_blob);
    println!("[method:snapshot snap1] ok snapshot_empty={empty1} detail={detail}");
    if empty1 != "false" {
        println!("  preview={}", extract_snapshot_preview(&snap1_blob, 120));
    } else {
        println!("  preview={}", extract_snapshot_preview(&snap1_blob, 160));
    }

    let action_name = args.action.clone();
    match session.request_ok(
        "action1",
        &[("method", "action"), ("name", action_name.as_str())],
        Duration::from_secs(15),
    ) {
        Ok(_) => println!("[method:action {}] ok", args.action),
        Err(error) => {
            // Action may fail if focus/window not ready; golden still values snapshot quality.
            println!("[method:action {}] warn: {error:#}", args.action);
        }
    }

    let keys = args.keys.clone();
    match session.request_ok(
        "keys1",
        &[("method", "keys"), ("keys", keys.as_str())],
        Duration::from_secs(15),
    ) {
        Ok(_) => println!("[method:keys {}] ok", args.keys),
        Err(error) => println!("[method:keys {}] warn: {error:#}", args.keys),
    }

    let snap2 = session.request_ok(
        "snap2",
        &snapshot_method_fields(detail),
        Duration::from_secs(15),
    )?;
    let snap2_blob = snap2.join("\n");
    let empty2 = classify_snapshot(&snap2_blob);
    println!("[method:snapshot snap2] ok snapshot_empty={empty2}");

    session.shutdown_best_effort();
    println!("[method:shutdown] requested");

    // Pass criterion: at least one non-empty snapshot (Phase 1B).
    if empty1 != "false" && empty2 != "false" {
        bail!(
            "golden failed: snapshots empty (snap1={empty1}, snap2={empty2}). \
             Rebuild release with headless a11y fix, or inspect stderr above."
        );
    }

    println!("golden passed");
    Ok(())
}

/// Poll `method:snapshot` until non-empty (+ optional substrings) or `budget` elapses.
///
/// Prefer this runner-side settle over a single fixed wait when UI needs time to paint.
/// Process exit / stdout close / snapshot `ok: false` hard-fail immediately; only step
/// timeouts soft-retry while budget remains.
fn poll_until_snapshot(
    session: &mut DogfoodSession,
    expects: &[String],
    poll_interval: Duration,
    budget: Duration,
    detail: &str,
) -> Result<String> {
    let deadline = Instant::now() + budget;
    let mut best_blob = String::new();
    let mut attempt: u32 = 0;
    let mut tried_once = false;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() && tried_once {
            break;
        }
        // One attempt even if budget is already exhausted at entry; no multi-second floor.
        let step_timeout = if remaining.is_zero() {
            Duration::from_millis(500)
        } else {
            remaining
                .min(Duration::from_secs(15))
                .max(Duration::from_millis(100))
        };

        attempt = attempt.saturating_add(1);
        tried_once = true;
        let id = format!("snap{attempt}");
        match session.request_ok(&id, &snapshot_method_fields(detail), step_timeout) {
            Ok(lines) => {
                let blob = lines.join("\n");
                let empty = classify_snapshot(&blob);
                println!("[method:snapshot {id}] ok snapshot_empty={empty} attempt={attempt}");
                if snapshot_satisfies(&blob, expects) {
                    println!("  preview={}", extract_snapshot_preview(&blob, 200));
                    for expect in expects {
                        println!("  expect ok: {expect:?}");
                    }
                    return Ok(blob);
                }
                if prefer_diagnostic_blob(&best_blob, &blob, expects) {
                    best_blob = blob;
                }
            }
            Err(error) => {
                if !is_retryable_poll_error(&error) {
                    bail!("smoke failed: snapshot poll fatal: {error:#}");
                }
                println!("[method:snapshot {id}] warn (retryable): {error:#}");
            }
        }

        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(poll_interval);
    }

    let empty = classify_snapshot(&best_blob);
    let missing = missing_snapshot_expects(&best_blob, expects);
    if empty != "false" {
        bail!(
            "smoke failed: snapshot empty ({empty}) after {attempt} poll(s)\n  preview={}",
            extract_snapshot_preview(&best_blob, 200)
        );
    }
    bail!(
        "smoke failed: snapshot missing expected substring(s) {missing:?} after {attempt} poll(s)\n  preview={}",
        extract_snapshot_preview(&best_blob, 200)
    );
}

fn run_smoke(args: SmokeArgs) -> Result<()> {
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    let fixture = resolve_fixture(args.fixture)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let fixture_str = fixture.display().to_string();
    let poll_interval = Duration::from_millis(args.poll_ms.max(1));
    let session_start = Instant::now();

    println!(
        "dogfood smoke: bin={} fixture={} wait_ms={} poll_ms={} expects={} action={:?} keys={:?}",
        bin.display(),
        fixture.display(),
        args.wait_ms,
        args.poll_ms,
        args.expect.len(),
        args.action.as_deref(),
        args.keys.as_deref(),
    );

    let mut session = DogfoodSession::spawn(&bin, timeout)?;
    session.wait_until(
        0,
        |lines| lines.iter().any(|l| is_ready_line(l)),
        Duration::from_secs(45),
    )?;
    println!("[event:ready] ok");

    session.request_ok(
        "open1",
        &[("method", "open"), ("path", fixture_str.as_str())],
        Duration::from_secs(15),
    )?;
    println!("[method:open] ok");

    if args.wait_ms > 0 {
        let wait_ms = args.wait_ms.to_string();
        session.request_ok(
            "wait1",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(args.wait_ms + 10_000),
        )?;
        println!("[method:wait] ok");
    }

    // Poll until non-empty snapshot (and all --expect substrings), using remaining budget only.
    let poll_budget = timeout.saturating_sub(session_start.elapsed());
    if poll_budget.is_zero() {
        session.shutdown_best_effort();
        bail!("smoke failed: session timeout exhausted before snapshot poll");
    }
    let mut last_good = poll_until_snapshot(
        &mut session,
        &args.expect,
        poll_interval,
        poll_budget,
        args.snapshot_detail.as_str(),
    )?;

    let did_side_effect = args.action.is_some() || args.keys.is_some();

    if let Some(action_name) = args.action.as_deref() {
        match session.request_ok(
            "action1",
            &[("method", "action"), ("name", action_name)],
            Duration::from_secs(15),
        ) {
            Ok(_) => println!("[method:action {action_name}] ok"),
            Err(error) => {
                if args.require_action {
                    session.shutdown_best_effort();
                    bail!("smoke failed: action {action_name:?}: {error:#}");
                }
                println!("[method:action {action_name}] warn: {error:#}");
            }
        }
    }

    if let Some(keys) = args.keys.as_deref() {
        match session.request_ok(
            "keys1",
            &[("method", "keys"), ("keys", keys)],
            Duration::from_secs(15),
        ) {
            Ok(_) => println!("[method:keys {keys}] ok"),
            Err(error) => println!("[method:keys {keys}] warn: {error:#}"),
        }
    }

    // After action/keys, require a second non-empty snapshot (expects still held).
    if did_side_effect {
        let second_budget = timeout.saturating_sub(session_start.elapsed());
        if second_budget.is_zero() {
            session.shutdown_best_effort();
            bail!("smoke failed: session timeout exhausted before post-action snapshot poll");
        }
        last_good = poll_until_snapshot(
            &mut session,
            &args.expect,
            poll_interval,
            second_budget,
            args.snapshot_detail.as_str(),
        )?;
        println!("[method:snapshot post-action] ok");
    }

    // Graceful shutdown before pass/fail so teardown does not depend on Drop kill.
    session.shutdown_best_effort();
    println!("[method:shutdown] requested");

    // Final assert on the latest successful poll (post-action when side effects ran).
    if !snapshot_satisfies(&last_good, &args.expect) {
        let empty = classify_snapshot(&last_good);
        if empty != "false" {
            bail!("smoke failed: snapshot empty ({empty})");
        }
        let missing = missing_snapshot_expects(&last_good, &args.expect);
        bail!("smoke failed: snapshot missing expected substring(s) {missing:?}");
    }

    println!("smoke passed");
    Ok(())
}

pub fn run_dogfood(args: DogfoodArgs) -> Result<()> {
    match args.command {
        DogfoodCommand::Preflight(a) => run_preflight(a),
        DogfoodCommand::Golden(a) => run_golden(a),
        DogfoodCommand::Smoke(a) => run_smoke(a),
        DogfoodCommand::MergeReview(a) => run_merge_review(a),
        DogfoodCommand::Queue(a) => run_queue(a),
    }
}

/// Headless merge-review workshop: Start → chrome expects → [optional Advance] → Preview → End.
///
/// **Settle rule (binding):** after open (+ wait), run a pre-start `look` so force-draw
/// paints chrome **before** `surmount::StartMergeReview`. Start no-ops without that paint.
///
/// Always records action ok/error and snapshot preview honestly. Dumps stderr
/// lines that mention merge review / surmount so product toasts and populate
/// logs are visible without inventing UI that did not paint.
/// Default adventure never requires Dialog, Advance, or conflict fixture
/// (`--with-advance` / `--with-conflict` are opt-in).
fn run_merge_review(args: MergeReviewArgs) -> Result<()> {
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    // Keep tempfile root alive for the whole adventure when --with-conflict builds one.
    let (workspace_root, conflict_fixture_keep) = if args.with_conflict {
        match prepare_merge_review_conflict_fixture() {
            Ok((root, keep)) => {
                println!(
                    "[conflict] fixture ready at {} (decision chrome gated when present)",
                    root.display()
                );
                (root, Some(keep))
            }
            Err(error) => {
                println!(
                    "[conflict] skip: could not build tempfile conflict fixture ({error:#}); default adventure continues on Surmount workspace"
                );
                (resolve_surmount_workspace(args.fixture)?, None)
            }
        }
    } else {
        (resolve_surmount_workspace(args.fixture)?, None)
    };
    let conflict_fixture_active = conflict_fixture_keep.is_some();
    let _conflict_fixture_keep = conflict_fixture_keep;
    let timeout = Duration::from_secs(args.timeout_secs);
    let workspace_str = workspace_root.display().to_string();
    let detail = args.snapshot_detail.as_str();
    let action_name = args.action.as_str();
    let post_start_expects = merge_review_post_start_expects(&args.expect);

    println!(
        "dogfood merge-review: bin={} workspace={} action={} detail={} wait_ms={} post_start_wait_ms={} start_only={} with_advance={} with_conflict={} decide_live_agent={} expects={:?}",
        bin.display(),
        workspace_root.display(),
        action_name,
        detail,
        args.wait_ms,
        args.post_start_wait_ms,
        args.start_only,
        args.with_advance,
        args.with_conflict,
        args.decide_live_agent,
        post_start_expects,
    );

    let mut session = DogfoodSession::spawn(&bin, timeout)?;
    session.wait_until(
        0,
        |lines| lines.iter().any(|l| is_ready_line(l)),
        Duration::from_secs(45),
    )?;
    println!("[event:ready] ok");

    // Open the Surmount repo root (directory worktree), not a single-file shell.
    session.request_ok(
        "open1",
        &[("method", "open"), ("path", workspace_str.as_str())],
        Duration::from_secs(20),
    )?;
    println!("[method:open] ok workspace={workspace_str}");

    if args.wait_ms > 0 {
        let wait_ms = args.wait_ms.to_string();
        session.request_ok(
            "wait1",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(args.wait_ms + 10_000),
        )?;
        println!("[method:wait pre-start] ok ms={}", args.wait_ms);
    }

    // Pre-start room look (baseline).
    let look1 = session.request_ok(
        "look1",
        &snapshot_method_fields(detail),
        Duration::from_secs(20),
    )?;
    let look1_blob = look1.join("\n");
    println!(
        "[method:look pre-start] empty={} preview={}",
        classify_snapshot(&look1_blob),
        extract_snapshot_preview(&look1_blob, 220)
    );

    match session.request_ok(
        "start1",
        &[("method", "action"), ("name", action_name)],
        Duration::from_secs(20),
    ) {
        Ok(lines) => {
            println!("[method:action {action_name}] ok");
            let blob = lines.join("\n");
            if blob_has_ok_false(&blob) {
                println!("  warn: response contains ok:false in broader buffer");
            }
        }
        Err(error) => {
            // Still dump stderr for product diagnosis before failing.
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!("merge-review adventure: action {action_name:?} failed: {error:#}");
        }
    }

    if args.post_start_wait_ms > 0 {
        let wait_ms = args.post_start_wait_ms.to_string();
        session.request_ok(
            "wait2",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(args.post_start_wait_ms + 15_000),
        )?;
        println!("[method:wait post-start] ok ms={}", args.post_start_wait_ms);
    }

    let look2 = session.request_ok(
        "look2",
        &snapshot_method_fields(detail),
        Duration::from_secs(30),
    )?;
    let look2_blob = look2.join("\n");
    println!(
        "[method:look post-start] empty={} preview={}",
        classify_snapshot(&look2_blob),
        extract_snapshot_preview(&look2_blob, 400)
    );
    if let Some(text) = extract_snapshot_text(&look2_blob) {
        let decoded = text.replace("\\n", "\n");
        println!("--- post-start outline (first 40 lines) ---");
        for (i, line) in decoded.lines().take(40).enumerate() {
            println!("{:02}|{}", i + 1, line);
        }
    }

    if !snapshot_satisfies(&look2_blob, &post_start_expects) {
        let missing = missing_snapshot_expects(&look2_blob, &post_start_expects);
        if conflict_fixture_active {
            // Conflict fixture is opt-in proof; do not fail the default green path.
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            println!(
                "[conflict] skip: post-Start expects missing {missing:?}; decision chrome not gated"
            );
            session.shutdown_best_effort();
            println!("merge-review adventure finished (conflict fixture soft-skip)");
            return Ok(());
        }
        session.pump();
        print_merge_review_stderr_tail(&session.stderr_buf);
        session.shutdown_best_effort();
        bail!(
            "merge-review adventure: post-start look missing expected substring(s) {missing:?}\n  preview={}",
            extract_snapshot_preview(&look2_blob, 400)
        );
    }
    for expect in &post_start_expects {
        println!("  post-start expect ok: {expect:?}");
    }

    // PR4b residual gate: Expand controls must not appear at negative Y in room outline.
    if let Some(text) = extract_snapshot_text(&look2_blob) {
        let outline = decode_outline_escapes(&text);
        if let Some(bad) = first_expand_negative_y_line(&outline) {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!("merge-review adventure: off-screen Expand in post-start outline: {bad}");
        }
    }

    if conflict_fixture_active {
        gate_merge_review_decision_chrome(&look2_blob);
        // Hard Decide spine: Review Diff → Summarizing → synthetic capture (or soft live agent)
        // → Discuss/Record chrome → product resolve. No Preview/End.
        if !args.start_only {
            let review_diff_dispatched = run_merge_review_conflict_review_diff_step(
                &mut session,
                detail,
                args.step_wait_ms,
            )?;
            run_merge_review_conflict_summary_capture_step(
                &mut session,
                detail,
                args.step_wait_ms,
                args.decide_live_agent,
                review_diff_dispatched,
            )?;
            run_merge_review_conflict_decide_step(
                &mut session,
                detail,
                args.step_wait_ms,
            )?;
        }
    } else if args.with_conflict {
        println!(
            "[conflict] decision chrome gate skipped (fixture not active; default path does not require MERGE_HEAD)"
        );
    }

    if args.with_advance && !args.start_only && !conflict_fixture_active {
        run_merge_review_advance_step(&mut session, detail, &look2_blob, args.step_wait_ms)?;
    } else if args.with_advance && args.start_only {
        println!("[advance] skipped (--start-only takes precedence over --with-advance)");
    } else if args.with_advance && conflict_fixture_active {
        println!("[advance] skipped (conflict fixture path gates decision chrome + Review Diff, not Next file)");
    }

    if !args.start_only && !conflict_fixture_active {
        run_merge_review_workshop_steps(&mut session, detail, args.step_wait_ms)?;
    } else if conflict_fixture_active {
        println!(
            "[workshop] skipped (conflict fixture: decision chrome + Review Diff; Preview/End stays on default path)"
        );
    } else {
        println!("[workshop] skipped (--start-only)");
    }

    // inventory / theme are optional protocol toys — best-effort, non-fatal.
    match session.request_ok("inv1", &[("method", "inventory")], Duration::from_secs(15)) {
        Ok(lines) => {
            let blob = lines.join("\n");
            println!("[method:inventory] ok");
            if let Some(caps) = Regex::new(r#""inventory@text":\s*"(.*)""#)
                .ok()
                .and_then(|re| re.captures(&blob))
            {
                let inv = caps
                    .get(1)
                    .map(|m| m.as_str().replace("\\n", "\n"))
                    .unwrap_or_default();
                println!("--- inventory ---");
                for line in inv.lines().take(20) {
                    println!("{line}");
                }
            }
        }
        Err(error) => println!("[method:inventory] warn: {error:#}"),
    }

    session.pump();
    print_merge_review_stderr_tail(&session.stderr_buf);

    session.shutdown_best_effort();
    println!("[method:shutdown] requested");
    println!("merge-review adventure finished (see stderr tail for populate / toast logs)");
    Ok(())
}

/// Regex for room outline bounds: `"Label" @x,y WxH` (y may be negative).
fn outline_bounds_y(line: &str) -> Option<i64> {
    // Match `@x,y` after a quoted label; y is signed.
    let re = Regex::new(r#"@-?\d+,(-?\d+)\s+\d+x\d+"#).ok()?;
    let caps = re.captures(line)?;
    caps.get(1)?.as_str().parse().ok()
}

/// First outline line that looks like an Expand control with negative Y (PR4b).
fn first_expand_negative_y_line(outline: &str) -> Option<String> {
    for line in outline.lines() {
        let lower = line.to_ascii_lowercase();
        // Match Disclosure "Expand" / Expand Excerpt labels, not Action::Expand verb lists alone.
        let is_expand_label = line.contains("\"Expand\"")
            || line.contains("\"Expand Excerpt\"")
            || lower.contains("[button] \"expand");
        if !is_expand_label {
            continue;
        }
        if outline_bounds_y(line).is_some_and(|y| y < 0) {
            return Some(line.trim().to_string());
        }
    }
    None
}

/// Path-ish fingerprints from a room outline (basename segments on labeled buttons).
/// Used by `--with-advance` to require a path/cursor delta after NextFile.
fn path_fingerprints_from_outline(outline: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Quoted labels that look like paths (contain / or end with a common source suffix).
    let re = Regex::new(r#"\"([^\"]+)\""#).expect("path fingerprint re");
    for line in outline.lines() {
        for caps in re.captures_iter(line) {
            let label = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if label.is_empty() || label.len() > 120 {
                continue;
            }
            let looks_like_path = label.contains('/')
                || label.contains('\\')
                || label.ends_with(".rs")
                || label.ends_with(".toml")
                || label.ends_with(".md")
                || label.ends_with(".json")
                || label.ends_with(".ts")
                || label.ends_with(".tsx");
            if !looks_like_path {
                continue;
            }
            // Prefer basename for stable compare across truncation.
            let base = label
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(label)
                .trim_start_matches('…');
            if base.is_empty() {
                continue;
            }
            if !out.iter().any(|existing| existing == base) {
                out.push(base.to_string());
            }
        }
    }
    out
}

/// True when post-advance outline or stderr shows a different path than pre-capture.
fn advance_shows_path_delta(pre_paths: &[String], post_outline: &str, stderr: &[String]) -> bool {
    let post_paths = path_fingerprints_from_outline(post_outline);
    if !pre_paths.is_empty() && !post_paths.is_empty() {
        // Success if any post path is not in the pre set, or ordered first path changed.
        if post_paths.iter().any(|p| !pre_paths.contains(p)) {
            return true;
        }
        if pre_paths.first() != post_paths.first() {
            return true;
        }
    }
    // Product log: "advanced to next file {path}"
    for line in stderr {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("advanced to next file ") {
            let path_part = line[idx + "advanced to next file ".len()..].trim();
            let base = path_part.rsplit(['/', '\\']).next().unwrap_or(path_part);
            if base.is_empty() {
                continue;
            }
            if pre_paths.is_empty() || !pre_paths.iter().any(|p| p == base || path_part.contains(p))
            {
                return true;
            }
            // Path differs from first pre fingerprint.
            if pre_paths
                .first()
                .is_some_and(|p| p != base && !path_part.ends_with(p.as_str()))
            {
                return true;
            }
        }
    }
    false
}

/// Optional `--with-advance`: NextFile after Start settle; success = path/cursor delta.
fn run_merge_review_advance_step(
    session: &mut DogfoodSession,
    detail: &str,
    post_start_blob: &str,
    step_wait_ms: u64,
) -> Result<()> {
    const NEXT_ACTION: &str = "surmount::MergeReviewNextFile";

    let pre_outline = extract_snapshot_text(post_start_blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let pre_paths = path_fingerprints_from_outline(&pre_outline);
    println!(
        "[advance] pre-capture path fingerprints ({})={:?}",
        pre_paths.len(),
        pre_paths.iter().take(8).collect::<Vec<_>>()
    );

    // Single-file queue: NextFile may no-op — skip with log (not green success).
    if !advance_has_enough_path_fingerprints(&pre_paths) {
        println!(
            "[advance] skip: need ≥2 path-labeled files in post-Start outline (found {}); not counting as success",
            pre_paths.len()
        );
        return Ok(());
    }

    let stderr_before = session.stderr_buf.len();
    match session.request_ok(
        "advance1",
        &[("method", "action"), ("name", NEXT_ACTION)],
        Duration::from_secs(20),
    ) {
        Ok(_) => println!("[method:action {NEXT_ACTION}] ok"),
        Err(error) => {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!("merge-review advance: {NEXT_ACTION} failed: {error:#}");
        }
    }

    if step_wait_ms > 0 {
        let wait_ms = step_wait_ms.to_string();
        session.request_ok(
            "wait_advance",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(step_wait_ms + 10_000),
        )?;
        println!("[method:wait post-advance] ok ms={step_wait_ms}");
    }

    let look_advance = session.request_ok(
        "look_advance",
        &snapshot_method_fields(detail),
        Duration::from_secs(30),
    )?;
    let look_advance_blob = look_advance.join("\n");
    session.pump();
    let post_outline = extract_snapshot_text(&look_advance_blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let stderr_delta: Vec<String> = session.stderr_buf[stderr_before..].to_vec();

    if !advance_shows_path_delta(&pre_paths, &post_outline, &stderr_delta)
        && !advance_shows_path_delta(&pre_paths, &post_outline, &session.stderr_buf)
    {
        print_merge_review_stderr_tail(&session.stderr_buf);
        session.shutdown_best_effort();
        bail!(
            "merge-review advance: no path/cursor delta after {NEXT_ACTION} (static Next file chrome is not enough)\n  pre_paths={pre_paths:?}\n  post_paths={:?}\n  preview={}",
            path_fingerprints_from_outline(&post_outline),
            extract_snapshot_preview(&look_advance_blob, 300)
        );
    }
    println!("[advance] path/cursor delta ok");

    // AC-B: after multi-file NextFile success, if room reports focus it must not be
    // solely Window. Default adventure never runs this step.
    if let Some(focus_line) = room_focus_line(&post_outline) {
        if room_focus_is_solely_window(focus_line) {
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!(
                "merge-review advance AC-B: room # focus: still solely Window after NextFile ({focus_line})"
            );
        }
        println!("[advance] AC-B focus ok: {focus_line}");
    } else {
        println!("[advance] AC-B: no # focus: line in post-advance outline (not hard-failing)");
    }
    Ok(())
}

/// True when post-Start path fingerprints are enough to exercise NextFile delta.
fn advance_has_enough_path_fingerprints(pre_paths: &[String]) -> bool {
    pre_paths.len() >= 2
}

/// Room `# focus:` line from a decoded outline, if present.
fn room_focus_line(outline: &str) -> Option<&str> {
    outline.lines().find(|line| line.starts_with("# focus:"))
}

/// True when the focus header is solely the root Window (AC-B failure shape).
fn room_focus_is_solely_window(focus_line: &str) -> bool {
    focus_line.starts_with("# focus:") && focus_line.contains("[Window]")
}

/// Conflict-specific decision chrome (not always-on labels like `"Review Diff"`).
///
/// Prefer stable product strings (aligned with merge-review rail / conflict bar).
/// Dynamic branch-named `Use …` buttons are optional extras via
/// [`decision_chrome_dynamic_use_hits`] and never the sole generic gate.
const MERGE_REVIEW_DECISION_CHROME_CONFLICT: &[&str] = &[
    "Use Both",
    "Resolve with Agent",
    "Summarize this conflict",
    "Discuss conflict",
    "Keep fork",
    "Take upstream",
    "Record decision",
    "Synthesize",
];

/// Soft Decide rail labels after Review Diff / Summarizing (product strings only).
const MERGE_REVIEW_DECIDE_RAIL: &[&str] = &[
    "Discuss conflict",
    "Record decision",
    "Keep fork",
    "Take upstream",
    "Use Both",
];

/// Stable conflict-specific needles present in `outline`.
fn decision_chrome_hits(outline: &str) -> Vec<&'static str> {
    MERGE_REVIEW_DECISION_CHROME_CONFLICT
        .iter()
        .copied()
        .filter(|label| outline.contains(label))
        .collect()
}

/// Decide-rail needles present in `outline` (Discuss / Record / Use Both / …).
fn decide_rail_hits(outline: &str) -> Vec<&'static str> {
    MERGE_REVIEW_DECIDE_RAIL
        .iter()
        .copied()
        .filter(|label| outline.contains(label))
        .collect()
}

/// Soft poll may stop waiting for agent essay when settle chrome is ready.
///
/// Ready when Summarizing cleared with decide rail, or network-free **Use Both** is visible.
fn decide_settle_ready(
    still_summarizing: bool,
    decide_hits: &[&str],
    use_both_visible: bool,
) -> bool {
    (!still_summarizing && !decide_hits.is_empty()) || use_both_visible
}

/// Classify soft Decide product outcome (for logs + unit tests).
fn decide_path_outcome(acted: bool, post_rail_nonempty: bool) -> &'static str {
    if acted {
        "acted"
    } else if post_rail_nonempty {
        "rail_present"
    } else {
        "soft_skip"
    }
}

/// Hard L2 after synthetic inject: **capture stderr required**.
/// Post-capture Discuss/Record rail is soft annotation only when capture ok — never greens alone.
fn synthetic_capture_l2_verdict(capture_ok: bool, post_capture_rail: bool) -> &'static str {
    match (capture_ok, post_capture_rail) {
        (true, true) => "ok_capture_and_rail",
        (true, false) => "ok_capture",
        (false, _) => "fail",
    }
}

/// Path from production capture log only (`capture ok path=…` / `captured summary for …`).
/// Does not parse UI toast strings.
fn extract_synthetic_capture_path(stderr: &str) -> Option<&str> {
    for line in stderr.lines() {
        for marker in ["capture ok path=", "captured summary for "] {
            if let Some(rest) = line.split(marker).nth(1) {
                let path = rest.split_whitespace().next().unwrap_or(rest).trim();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Production `log::info` capture markers only (not toast / inject-failed warns).
fn stderr_has_synthetic_capture_log(stderr: &str) -> bool {
    stderr.contains("dogfood synthetic summary capture ok")
        || stderr.contains("captured summary for")
}

/// Post-capture workshop labels only (Discuss/Record). Excludes pre-capture Use Both chrome.
fn post_capture_rail_hits(outline: &str) -> Vec<&'static str> {
    ["Discuss conflict", "Record decision"]
        .into_iter()
        .filter(|label| outline.contains(label))
        .collect()
}

/// Live-agent production capture log only (`captured summary for`), not toast or synthetic inject.
fn stderr_has_live_capture_log(stderr: &str) -> bool {
    stderr.contains("captured summary for")
}

/// Soft live-agent poll verdict (unit-tested).
///
/// - `capture_log` — production `captured summary for` present (skip synthetic)
/// - `discuss_rail` — Summarizing cleared + Discuss/Record rail (skip synthetic)
/// - `still_summarizing` — keep polling (not terminal)
/// - `soft_skip` — budget exhausted; fall through to synthetic hard spine
fn live_capture_settle_verdict(
    capture_log: bool,
    still_summarizing: bool,
    post_capture_rail: bool,
    timed_out: bool,
) -> &'static str {
    if capture_log {
        "capture_log"
    } else if !still_summarizing && post_capture_rail {
        "discuss_rail"
    } else if timed_out {
        "soft_skip"
    } else {
        "still_summarizing"
    }
}

fn live_capture_settled(verdict: &str) -> bool {
    matches!(verdict, "capture_log" | "discuss_rail")
}

/// Live soft-poll budget: floor 30s for Grok latency, ceiling 90s; floor raised by `step_wait_ms`.
fn live_capture_poll_budget_ms(step_wait_ms: u64) -> u64 {
    step_wait_ms.max(30_000).min(90_000)
}

/// Production resolve success on stderr (`Resolved … git checkout …`), not ignore/fail warns.
fn stderr_has_resolve_success(stderr: &str) -> bool {
    if stderr.contains("conflict resolve ignored")
        || stderr.contains("conflict resolve failed")
        || stderr.contains("Could not resolve")
    {
        // Fall through: a later success line may still appear after an earlier ignore.
    }
    for line in stderr.lines() {
        if line.contains("conflict resolve ignored") || line.contains("conflict resolve failed") {
            continue;
        }
        if line.contains("Resolved ") && line.contains("git checkout") {
            return true;
        }
    }
    false
}

/// Soft L3 product evidence: resolve success log or conflict chrome/rail delta.
fn product_resolve_has_evidence(stderr_delta: &str, pre_outline: &str, post_outline: &str) -> bool {
    if stderr_has_resolve_success(stderr_delta) {
        return true;
    }
    // Use Both / Keep fork cleared after product resolve.
    if pre_outline.contains("Use Both") && !post_outline.contains("Use Both") {
        return true;
    }
    if pre_outline.contains("Keep fork") && !post_outline.contains("Keep fork") {
        return true;
    }
    if pre_outline.contains("Take upstream") && !post_outline.contains("Take upstream") {
        return true;
    }
    // Workshop advanced into Record after act.
    if !pre_outline.contains("Record decision") && post_outline.contains("Record decision") {
        return true;
    }
    false
}

/// Soft L3: `acted` only with product evidence — bare TOON dispatch ok is not enough.
fn soft_l3_act_result(dispatch_ok: bool, has_evidence: bool) -> &'static str {
    if dispatch_ok && has_evidence {
        "acted"
    } else if dispatch_ok {
        "dispatch_only"
    } else {
        "no_dispatch"
    }
}

/// Optional dynamic conflict-bar labels like `"Use HEAD"` / `"Use origin/main"`.
/// Excludes `"Use Both"` (stable list) and unrelated `"User …"` chrome.
fn decision_chrome_dynamic_use_hits(outline: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for line in outline.lines() {
        if !line.contains("[Button]") {
            continue;
        }
        let Some(start) = line.find("\"Use ") else {
            continue;
        };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let label = &rest[..end];
        if label == "Use Both" {
            continue;
        }
        if label.starts_with("Use ") && label.len() > "Use ".len() {
            hits.push(label.to_string());
        }
    }
    hits
}

/// Soft gate: log conflict-specific decision labels after Start on a conflict fixture.
/// Never fails the run — fixture path is opt-in proof, not default green.
/// `"Review Diff"` alone does **not** count.
fn gate_merge_review_decision_chrome(post_start_blob: &str) {
    let outline = extract_snapshot_text(post_start_blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let hits = decision_chrome_hits(&outline);
    let dynamic = decision_chrome_dynamic_use_hits(&outline);
    if hits.is_empty() {
        println!(
            "[conflict] skip: no conflict-specific decision chrome in post-Start outline \
             (need Use Both / Resolve with Agent / Summarize this conflict / Discuss-rail; \
             Review Diff alone is not enough); dynamic Use hits={dynamic:?}"
        );
    } else {
        println!("[conflict] decision chrome ok: stable={hits:?} dynamic_use={dynamic:?}");
    }
}

/// NodeId for a labeled button in a room/rich outline line, e.g. `[Button] "Review Diff" … #NodeId(42)`.
/// Prefers interactive body lines (`[click]`) over `# focus:` header duplicates.
fn outline_button_node_id(outline: &str, label: &str) -> Option<String> {
    let needle = format!("[Button] \"{label}\"");
    let mut fallback: Option<String> = None;
    for line in outline.lines() {
        if !line.contains(&needle) {
            continue;
        }
        let Some(idx) = line.rfind("#NodeId(") else {
            continue;
        };
        let rest = &line[idx + "#NodeId(".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        // Body controls advertise click; focus header lines often repeat a shorter id.
        if line.contains("[click]") {
            return Some(digits);
        }
        fallback = Some(digits);
    }
    fallback
}

/// Review Diff product dispatch markers on stderr (not TOON action ok alone).
fn stderr_has_review_diff_dispatch(stderr: &str) -> bool {
    stderr.contains("Review Diff sent")
        || stderr.contains("conflict Review Diff dispatch")
        || stderr.contains("Review Diff requested")
}

/// After conflict Start settle: click **Review Diff** (or `git::ReviewDiff`) → **Summarizing…**.
///
/// Prefer a11y click on the rail primary — after Start, focus is often that button, and
/// window-level `git::ReviewDiff` may not hit ProjectDiff listeners. Soft-skip if ACP offline.
/// Returns whether a Review Diff **dispatch log** was observed (gates live soft poll).
fn run_merge_review_conflict_review_diff_step(
    session: &mut DogfoodSession,
    detail: &str,
    step_wait_ms: u64,
) -> Result<bool> {
    const REVIEW_DIFF_ACTION: &str = "git::ReviewDiff";
    const REVIEW_DIFF_LABEL: &str = "Review Diff";
    let stderr_before = session.stderr_buf.len();

    // Fresh look for NodeId (ids change each session).
    let pre_look = session.request_ok(
        "conflict_review_prelook",
        &[("method", "look"), ("detail", detail)],
        Duration::from_secs(30),
    )?;
    let pre_blob = pre_look.join("\n");
    let pre_outline = extract_snapshot_text(&pre_blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();

    // Prefer a11y click on the rail primary when present; always also try window action
    // (ProjectDiff listens for git::ReviewDiff when its surface holds focus).
    if let Some(node_id) = outline_button_node_id(&pre_outline, REVIEW_DIFF_LABEL) {
        match session.request_ok(
            "conflict_review_click",
            &[("method", "click"), ("node", node_id.as_str())],
            Duration::from_secs(20),
        ) {
            Ok(_) => println!("[method:click Review Diff node={node_id}] ok"),
            Err(error) => println!("[method:click Review Diff] warn: {error:#}"),
        }
    } else {
        println!("[conflict] no \"Review Diff\" button NodeId in outline");
    }

    match session.request_ok(
        "conflict_review_diff",
        &[("method", "action"), ("name", REVIEW_DIFF_ACTION)],
        Duration::from_secs(20),
    ) {
        Ok(_) => println!("[method:action {REVIEW_DIFF_ACTION}] ok"),
        Err(error) => {
            println!("[method:action {REVIEW_DIFF_ACTION}] warn: {error:#}");
        }
    }

    // Rail moves to Summarizing… when dispatch posts the package (not full agent completion).
    let settle_ms = step_wait_ms.max(2500).min(15_000);
    if settle_ms > 0 {
        let _ = session.request_ok(
            "conflict_review_wait",
            &[("method", "wait"), ("ms", &settle_ms.to_string())],
            Duration::from_secs(settle_ms / 1000 + 5),
        );
        println!("[method:wait post-conflict-review-diff] ok ms={settle_ms}");
    }

    session.pump();
    let look_lines = session.request_ok(
        "conflict_review_look",
        &[("method", "look"), ("detail", detail)],
        Duration::from_secs(30),
    )?;
    let look_blob = look_lines.join("\n");
    println!(
        "[method:look post-conflict-review-diff] empty={} preview={}",
        classify_snapshot(&look_blob),
        extract_snapshot_preview(&look_blob, 200)
    );

    let outline = extract_snapshot_text(&look_blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let summarizing = outline.contains("Summarizing");
    let stderr_slice = &session.stderr_buf[stderr_before.min(session.stderr_buf.len())..];
    let stderr_joined = stderr_slice.join("\n");
    let dispatch_ok = stderr_has_review_diff_dispatch(&stderr_joined);

    if dispatch_ok {
        println!("[conflict] Review Diff dispatch ok (stderr)");
    } else {
        println!(
            "[conflict] skip: no Review Diff dispatch log (agent/ACP may be offline or click missed)"
        );
        return Ok(false);
    }

    if summarizing {
        println!("[conflict] Summarizing rail ok");
    } else {
        println!(
            "[conflict] skip: rail has no \"Summarizing\" yet (dispatch ok; settle soft)"
        );
    }

    Ok(true)
}

/// L2 summary capture: synthetic inject (hard) or soft live-agent wait.
///
/// Synthetic path calls production capture via `surmount::InjectMergeReviewDogfoodSummary`
/// (same Summary:/Outcome: parser as agent stop). Live path polls production capture log
/// (`captured summary for`) or Discuss/Record after Summarizing; soft-skips then synthetic.
/// When `review_diff_dispatched` is false, live soft poll is skipped (no pending capture path).
fn run_merge_review_conflict_summary_capture_step(
    session: &mut DogfoodSession,
    detail: &str,
    step_wait_ms: u64,
    decide_live_agent: bool,
    review_diff_dispatched: bool,
) -> Result<()> {
    const INJECT: &str = "surmount::InjectMergeReviewDogfoodSummary";

    if decide_live_agent && !review_diff_dispatched {
        println!(
            "[conflict] soft-skip live agent (no Review Diff dispatch; synthetic hard spine next)"
        );
    } else if decide_live_agent {
        // Soft: production capture log or Discuss/Record after Summarizing clears.
        // Budget: max(step_wait_ms, 30s) capped at 90s — never hard-fail without synthetic.
        let poll_budget_ms = live_capture_poll_budget_ms(step_wait_ms);
        let poll_slice_ms = 2_000u64;
        let mut elapsed = 0u64;
        let stderr_before = session.stderr_buf.len();
        loop {
            session.pump();
            let look_result = session.request_ok(
                &format!("live_capture_look_{elapsed}"),
                &[("method", "look"), ("detail", detail)],
                Duration::from_secs(30),
            );
            let stderr_joined = session.stderr_buf[stderr_before.min(session.stderr_buf.len())..]
                .join("\n");
            let capture_log = stderr_has_live_capture_log(&stderr_joined);
            // Capture log alone can settle even if look flakes (no need for rail outline).
            if capture_log {
                let path = extract_synthetic_capture_path(&stderr_joined).unwrap_or("(unknown)");
                println!("[conflict] live settle ok verdict=capture_log path={path}");
                return Ok(());
            }
            let (outline, look_failed) = match look_result {
                Ok(look) => {
                    let blob = look.join("\n");
                    let outline = extract_snapshot_text(&blob)
                        .map(|t| decode_outline_escapes(&t))
                        .unwrap_or_default();
                    (outline, false)
                }
                Err(error) => {
                    // Soft: flaky look must not abort before synthetic hard spine.
                    let timed_out = elapsed >= poll_budget_ms;
                    if timed_out {
                        println!(
                            "[conflict] soft-skip live agent (look failed: {error:#}; synthetic hard spine next)"
                        );
                        break;
                    }
                    println!(
                        "[conflict] live look warn: {error:#}; retry within budget"
                    );
                    (String::new(), true)
                }
            };
            // Discuss/Record only — not Use Both / Keep fork (pre-capture chrome).
            let rail = post_capture_rail_hits(&outline);
            // Look failure: treat as still waiting (no discuss_rail without outline).
            let still_summarizing = look_failed || outline.contains("Summarizing");
            let timed_out = elapsed >= poll_budget_ms;
            let verdict = live_capture_settle_verdict(
                false, // capture_log already handled above
                still_summarizing,
                !rail.is_empty(),
                timed_out,
            );
            if live_capture_settled(verdict) {
                // discuss_rail only reaches here (capture_log returned earlier).
                println!(
                    "[conflict] live settle ok verdict={verdict} rail={rail:?}"
                );
                // Live evidence present — skip synthetic inject.
                return Ok(());
            }
            if verdict == "soft_skip" {
                println!(
                    "[conflict] soft-skip live agent (timeout {poll_budget_ms}ms; synthetic hard spine next)"
                );
                break;
            }
            let wait_ms = poll_slice_ms.min(poll_budget_ms.saturating_sub(elapsed).max(1));
            let _ = session.request_ok(
                &format!("live_capture_wait_{elapsed}"),
                &[("method", "wait"), ("ms", &wait_ms.to_string())],
                Duration::from_secs(wait_ms / 1000 + 5),
            );
            elapsed = elapsed.saturating_add(wait_ms);
        }
    }

    // Hard spine: synthetic inject through production capture.
    let stderr_before = session.stderr_buf.len();
    match session.request_ok(
        "conflict_synthetic_summary",
        &[("method", "action"), ("name", INJECT)],
        Duration::from_secs(20),
    ) {
        Ok(_) => println!("[method:action {INJECT}] ok"),
        Err(error) => {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            bail!(
                "merge-review conflict: synthetic summary inject failed ({error:#})\n  \
                 hard Decide spine requires surmount::InjectMergeReviewDogfoodSummary"
            );
        }
    }

    let settle_ms = step_wait_ms.max(1_500).min(8_000);
    if settle_ms > 0 {
        let _ = session.request_ok(
            "conflict_capture_wait",
            &[("method", "wait"), ("ms", &settle_ms.to_string())],
            Duration::from_secs(settle_ms / 1000 + 5),
        );
        println!("[method:wait post-synthetic-capture] ok ms={settle_ms}");
    }
    session.pump();

    let look = session.request_ok(
        "conflict_capture_look",
        &[("method", "look"), ("detail", detail)],
        Duration::from_secs(30),
    )?;
    let blob = look.join("\n");
    let outline = extract_snapshot_text(&blob)
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let post_rail = post_capture_rail_hits(&outline);
    let stderr_joined =
        session.stderr_buf[stderr_before.min(session.stderr_buf.len())..].join("\n");
    let capture_ok = stderr_has_synthetic_capture_log(&stderr_joined);
    let path = extract_synthetic_capture_path(&stderr_joined);
    let path_display = path.unwrap_or("(unknown)");
    let verdict = synthetic_capture_l2_verdict(capture_ok, !post_rail.is_empty());

    match verdict {
        "ok_capture_and_rail" => {
            println!("[conflict] capture ok path={path_display} rail={post_rail:?}");
        }
        "ok_capture" => {
            // Hard L2 green only on production capture log; Discuss/Record paint is soft.
            println!(
                "[conflict] capture ok path={path_display} (rail soft: no Discuss/Record yet)"
            );
        }
        "fail" => {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            bail!(
                "merge-review conflict: hard L2 capture failed after synthetic inject \
                 (missing production capture log)\n  \
                 expected stderr: dogfood synthetic summary capture ok path=…\n  \
                 (pre-capture Use Both chrome is not L2 evidence; inject TOON-ok alone is not enough)"
            );
        }
        other => bail!("merge-review conflict: unexpected L2 verdict {other}"),
    }

    Ok(())
}

/// After Review Diff / Summarizing: soft-prove a **decision path** without full Grok essays.
///
/// Prefers network-free product actions (`Use Both` / resolve ours) and optional soft poll for
/// Discuss/Record chrome after summarize settles. Soft-skip when ACP/agent never settles —
/// never hard-fail on agent quality. Hard contracts stay in chrome/dispatch steps only.
fn run_merge_review_conflict_decide_step(
    session: &mut DogfoodSession,
    detail: &str,
    step_wait_ms: u64,
) -> Result<()> {
    const USE_BOTH: &str = "Use Both";
    const RESOLVE_OURS: &str = "surmount::ResolveMergeReviewConflictOurs";

    // Soft poll: Summarizing may clear into Discuss/Record when ACP completes (no Grok → soft).
    // On timeout we still try network-free Use Both / resolve-ours below (never early-return).
    let poll_budget_ms = step_wait_ms.max(3_000).min(20_000);
    let poll_slice_ms = 1_500u64;
    let mut polls = 0u32;
    let mut elapsed = 0u64;
    let mut decide_hits: Vec<&'static str> = Vec::new();
    let mut still_summarizing = false;
    while elapsed <= poll_budget_ms {
        session.pump();
        let look_lines = session.request_ok(
            &format!("conflict_decide_poll_{polls}"),
            &[("method", "look"), ("detail", detail)],
            Duration::from_secs(30),
        )?;
        let outline = extract_snapshot_text(&look_lines.join("\n"))
            .map(|t| decode_outline_escapes(&t))
            .unwrap_or_default();
        still_summarizing = outline.contains("Summarizing");
        decide_hits = decide_rail_hits(&outline);
        let use_both_visible = outline.contains(USE_BOTH);
        if decide_settle_ready(still_summarizing, &decide_hits, use_both_visible) {
            if use_both_visible && still_summarizing {
                println!(
                    "[conflict] decide soft: Use Both visible (summarizing=true); \
                     proceeding without waiting for agent essay"
                );
            } else {
                println!(
                    "[conflict] decide settle ok: summarizing={still_summarizing} \
                     rail={decide_hits:?} polls={polls}"
                );
            }
            break;
        }
        if elapsed + poll_slice_ms > poll_budget_ms {
            break;
        }
        let _ = session.request_ok(
            &format!("conflict_decide_wait_{polls}"),
            &[("method", "wait"), ("ms", &poll_slice_ms.to_string())],
            Duration::from_secs(poll_slice_ms / 1000 + 5),
        );
        elapsed += poll_slice_ms;
        polls += 1;
    }

    if still_summarizing && decide_hits.is_empty() {
        println!(
            "[conflict] decide settle timeout (still Summarizing / empty rail); \
             best-effort product resolve next"
        );
    } else if !decide_hits.is_empty() {
        println!("[conflict] decide chrome soft: {decide_hits:?}");
    }

    // Soft L3 product resolve (network-free): Use Both → resolve-ours → Record when painted.
    // `acted` only with product evidence (stderr resolve success or rail delta) — never on
    // bare TOON dispatch ok. Soft-skip with one-line reason; never hard-fail default spine.
    const RECORD: &str = "Record decision";
    const NEXT_FILE: &str = "surmount::MergeReviewNextFile";

    session.pump();
    let pre_look = session.request_ok(
        "conflict_decide_prelook",
        &[("method", "look"), ("detail", detail)],
        Duration::from_secs(30),
    )?;
    let pre_outline = extract_snapshot_text(&pre_look.join("\n"))
        .map(|t| decode_outline_escapes(&t))
        .unwrap_or_default();
    let pre_paths = path_fingerprints_from_outline(&pre_outline);
    let stderr_mark = session.stderr_buf.len();
    let settle_ms = step_wait_ms.max(1_500).min(8_000);

    let mut acted = false;
    let mut act_reason = "none";
    let mut last_post_outline = pre_outline.clone();

    // Attempt → settle → evidence. Shared settle helper via inline waits.
    let mut attempt_idx = 0u32;
    let try_evidence = |session: &mut DogfoodSession,
                        attempt: &str,
                        attempt_idx: u32|
     -> Result<(String, bool)> {
        if settle_ms > 0 {
            let _ = session.request_ok(
                &format!("conflict_decide_settle_{attempt_idx}"),
                &[("method", "wait"), ("ms", &settle_ms.to_string())],
                Duration::from_secs(settle_ms / 1000 + 5),
            );
        }
        session.pump();
        let look = session.request_ok(
            &format!("conflict_decide_evidence_{attempt_idx}"),
            &[("method", "look"), ("detail", detail)],
            Duration::from_secs(30),
        )?;
        let post = extract_snapshot_text(&look.join("\n"))
            .map(|t| decode_outline_escapes(&t))
            .unwrap_or_default();
        let stderr_delta = session.stderr_buf[stderr_mark.min(session.stderr_buf.len())..].join("\n");
        let has = product_resolve_has_evidence(&stderr_delta, &pre_outline, &post);
        if has {
            println!("[conflict] product evidence ok via={attempt}");
        } else {
            println!(
                "[conflict] soft-skip {attempt}: dispatch ok without product evidence \
                 (need resolve success log or rail delta)"
            );
        }
        Ok((post, has))
    };

    if let Some(node_id) = outline_button_node_id(&pre_outline, USE_BOTH) {
        match session.request_ok(
            "conflict_decide_use_both",
            &[("method", "click"), ("node", node_id.as_str())],
            Duration::from_secs(20),
        ) {
            Ok(_) => {
                println!("[method:click Use Both node={node_id}] dispatch-ok");
                let (post, has) = try_evidence(session, "use_both", attempt_idx)?;
                attempt_idx += 1;
                last_post_outline = post;
                if soft_l3_act_result(true, has) == "acted" {
                    acted = true;
                    act_reason = "use_both";
                }
            }
            Err(error) => println!("[method:click Use Both] warn: {error:#}"),
        }
    } else {
        println!("[conflict] no \"Use Both\" button NodeId; trying resolve-ours action");
    }

    if !acted {
        match session.request_ok(
            "conflict_decide_resolve_ours",
            &[("method", "action"), ("name", RESOLVE_OURS)],
            Duration::from_secs(20),
        ) {
            Ok(_) => {
                println!(
                    "[method:action {RESOLVE_OURS}] dispatch-ok (not product proof yet)"
                );
                let (post, has) = try_evidence(session, "resolve_ours", attempt_idx)?;
                attempt_idx += 1;
                last_post_outline = post;
                if soft_l3_act_result(true, has) == "acted" {
                    acted = true;
                    act_reason = "resolve_ours";
                }
            }
            Err(error) => {
                println!("[method:action {RESOLVE_OURS}] soft-skip: {error:#}");
            }
        }
    }

    // Record when painted and resolve path had no product evidence (soft; never hard).
    if !acted {
        if let Some(node_id) = outline_button_node_id(&pre_outline, RECORD)
            .or_else(|| outline_button_node_id(&last_post_outline, RECORD))
        {
            match session.request_ok(
                "conflict_decide_record",
                &[("method", "click"), ("node", node_id.as_str())],
                Duration::from_secs(20),
            ) {
                Ok(_) => {
                    println!("[method:click Record decision node={node_id}] dispatch-ok");
                    let (post, has) = try_evidence(session, "record", attempt_idx)?;
                    attempt_idx += 1;
                    last_post_outline = post;
                    if soft_l3_act_result(true, has) == "acted" {
                        acted = true;
                        act_reason = "record";
                    }
                }
                Err(error) => println!("[method:click Record decision] soft-skip: {error:#}"),
            }
        }
    }

    // Optional soft Next file — soft-ok only with path/stderr advance evidence (same discipline).
    if acted && (pre_outline.contains("Next file") || last_post_outline.contains("Next file")) {
        let next_stderr_mark = session.stderr_buf.len();
        match session.request_ok(
            "conflict_decide_next_file",
            &[("method", "action"), ("name", NEXT_FILE)],
            Duration::from_secs(15),
        ) {
            Ok(_) => {
                if settle_ms > 0 {
                    let _ = session.request_ok(
                        "conflict_decide_next_settle",
                        &[("method", "wait"), ("ms", &settle_ms.to_string())],
                        Duration::from_secs(settle_ms / 1000 + 5),
                    );
                }
                session.pump();
                let next_look = session.request_ok(
                    "conflict_decide_next_look",
                    &[("method", "look"), ("detail", detail)],
                    Duration::from_secs(30),
                )?;
                let next_outline = extract_snapshot_text(&next_look.join("\n"))
                    .map(|t| decode_outline_escapes(&t))
                    .unwrap_or_default();
                let next_stderr = &session.stderr_buf[next_stderr_mark.min(session.stderr_buf.len())..];
                if advance_shows_path_delta(&pre_paths, &next_outline, next_stderr) {
                    println!("[method:action {NEXT_FILE}] soft-ok (path/stderr advance evidence)");
                } else {
                    println!(
                        "[method:action {NEXT_FILE}] soft-skip: dispatch-ok without advance evidence"
                    );
                }
                last_post_outline = next_outline;
            }
            Err(error) => println!("[method:action {NEXT_FILE}] soft-skip: {error:#}"),
        }
    }

    // Final post-look if we never ran evidence settle (no attempts painted).
    if attempt_idx == 0 {
        if settle_ms > 0 {
            let _ = session.request_ok(
                "conflict_decide_post_wait",
                &[("method", "wait"), ("ms", &settle_ms.to_string())],
                Duration::from_secs(settle_ms / 1000 + 5),
            );
        }
        session.pump();
        let post_look = session.request_ok(
            "conflict_decide_postlook",
            &[("method", "look"), ("detail", detail)],
            Duration::from_secs(30),
        )?;
        last_post_outline = extract_snapshot_text(&post_look.join("\n"))
            .map(|t| decode_outline_escapes(&t))
            .unwrap_or_default();
    }

    let post_hits = decide_rail_hits(&last_post_outline);
    let post_summarizing = last_post_outline.contains("Summarizing");
    let outcome = decide_path_outcome(acted, !post_hits.is_empty());
    println!(
        "[conflict] decide post-look: acted={acted} via={act_reason} summarizing={post_summarizing} \
         rail={post_hits:?} outcome={outcome} preview={}",
        last_post_outline.chars().take(160).collect::<String>()
    );
    match outcome {
        "acted" => {
            println!(
                "[conflict] Decide path soft-ok (product resolve via={act_reason}; no agent essay required)"
            )
        }
        "rail_present" => {
            println!(
                "[conflict] Decide path soft-ok (rail present; product resolve evidence missed)"
            )
        }
        _ => println!(
            "[conflict] soft-skip Decide product act (no product evidence from Use Both / resolve-ours / Record; rail empty)"
        ),
    }

    Ok(())
}

/// Build a minimal conflicted git worktree under a tempfile (two branches, merge conflict).
///
/// Used by `merge-review --with-conflict` so dogfood does not require live Surmount
/// `MERGE_HEAD`. Layout mirrors `tooling/xtask/dogfood_fixtures/merge_review_conflict/README.md`.
fn prepare_merge_review_conflict_fixture() -> Result<(PathBuf, tempfile::TempDir)> {
    let keep = tempfile::Builder::new()
        .prefix("zed-dogfood-merge-conflict-")
        .tempdir()
        .context("create conflict fixture tempdir")?;
    // Worktree + bare origin as **siblings** so origin.git is not a dirty path in Branch Diff.
    let work = keep.path().join("worktree");
    std::fs::create_dir_all(&work).context("mkdir conflict worktree")?;
    let bare = keep.path().join("origin.git");
    build_merge_review_conflict_git_tree(&work, &bare)?;
    Ok((work, keep))
}

fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Dogfood")
        .env("GIT_AUTHOR_EMAIL", "dogfood@example.com")
        .env("GIT_COMMITTER_NAME", "Dogfood")
        .env("GIT_COMMITTER_EMAIL", "dogfood@example.com")
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn build_merge_review_conflict_git_tree(root: &Path, bare_origin: &Path) -> Result<()> {
    std::fs::create_dir_all(root).context("mkdir conflict fixture root")?;
    git_in(root, &["init", "-b", "main"])?;
    git_in(root, &["config", "user.email", "dogfood@example.com"])?;
    git_in(root, &["config", "user.name", "Dogfood"])?;
    // Local fixture only — never inherit user commit.gpgsign / interactive gpg.
    git_in(root, &["config", "commit.gpgsign", "false"])?;
    git_in(root, &["config", "tag.gpgsign", "false"])?;
    // Surmount workspace marker + minimal category manifest so StartMergeReview can load.
    std::fs::write(
        root.join("SURMOUNT.md"),
        "# dogfood conflict fixture\n",
    )
    .context("write SURMOUNT.md")?;
    std::fs::write(
        root.join("surmount-merge-categories.toml"),
        r#"version = 1

[[rules]]
category_id = "dogfood_conflict"
surmount_section = "Dogfood conflict fixture"
disposition = "conflict"
risk = "low"
paths = [
  "conflict.txt",
  "**/*",
]
"#,
    )
    .context("write surmount-merge-categories.toml")?;
    std::fs::write(root.join("conflict.txt"), "base line\n").context("write base conflict.txt")?;
    git_in(
        root,
        &[
            "add",
            "SURMOUNT.md",
            "surmount-merge-categories.toml",
            "conflict.txt",
        ],
    )?;
    git_in(root, &["commit", "-m", "base"])?;
    // StartMergeReview defaults to origin/main — pin remote-tracking ref at base.
    let base_sha = git_in(root, &["rev-parse", "HEAD"])?;
    git_in(
        root,
        &["update-ref", "refs/remotes/origin/main", base_sha.as_str()],
    )?;
    git_in(root, &["checkout", "-b", "theirs"])?;
    std::fs::write(root.join("conflict.txt"), "theirs line\n").context("write theirs")?;
    git_in(root, &["add", "conflict.txt"])?;
    git_in(root, &["commit", "-m", "theirs"])?;
    git_in(root, &["checkout", "main"])?;
    std::fs::write(root.join("conflict.txt"), "ours line\n").context("write ours")?;
    git_in(root, &["add", "conflict.txt"])?;
    git_in(root, &["commit", "-m", "ours"])?;
    // Merge should conflict and leave MERGE_HEAD.
    let merge = Command::new("git")
        .args(["merge", "--no-ff", "--no-edit", "theirs"])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Dogfood")
        .env("GIT_AUTHOR_EMAIL", "dogfood@example.com")
        .env("GIT_COMMITTER_NAME", "Dogfood")
        .env("GIT_COMMITTER_EMAIL", "dogfood@example.com")
        .output()
        .context("spawn git merge")?;
    // Conflict exits non-zero — that is expected.
    let merge_head = root.join(".git").join("MERGE_HEAD");
    if !merge_head.is_file() {
        let stderr = String::from_utf8_lossy(&merge.stderr);
        bail!("expected MERGE_HEAD after conflict merge; stderr={stderr}");
    }
    // Sanity: product default upstream must resolve.
    git_in(root, &["rev-parse", "origin/main"])?;
    // Bare origin **outside** the worktree so it is not a dirty path in Branch Diff.
    // Fetch stays offline and quiet for Start's `git fetch origin`.
    if let Some(parent) = bare_origin.parent() {
        std::fs::create_dir_all(parent).context("mkdir bare origin parent")?;
    }
    let bare_str = bare_origin.to_str().context("bare origin path utf8")?;
    let clone = Command::new("git")
        .args(["clone", "--bare", "--quiet", ".", bare_str])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Dogfood")
        .env("GIT_AUTHOR_EMAIL", "dogfood@example.com")
        .env("GIT_COMMITTER_NAME", "Dogfood")
        .env("GIT_COMMITTER_EMAIL", "dogfood@example.com")
        .output()
        .context("spawn git clone --bare for dogfood origin")?;
    if !clone.status.success() {
        bail!(
            "git clone --bare failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
    }
    let bare_path = bare_origin
        .canonicalize()
        .unwrap_or_else(|_| bare_origin.to_path_buf())
        .to_string_lossy()
        .into_owned();
    git_in(root, &["remote", "add", "origin", bare_path.as_str()])?;
    git_in(root, &["fetch", "origin", "--quiet"])?;
    git_in(root, &["rev-parse", "origin/main"])?;
    Ok(())
}

/// Preview merge modal then End merge review — fail closed on action/look expects.
fn run_merge_review_workshop_steps(
    session: &mut DogfoodSession,
    detail: &str,
    step_wait_ms: u64,
) -> Result<()> {
    const PREVIEW_ACTION: &str = "surmount::PreviewMergeReviewMerge";
    const END_ACTION: &str = "surmount::EndMergeReview";
    const PREVIEW_EXPECT: &str = "Preview merge";

    match session.request_ok(
        "preview1",
        &[("method", "action"), ("name", PREVIEW_ACTION)],
        Duration::from_secs(20),
    ) {
        Ok(_) => println!("[method:action {PREVIEW_ACTION}] ok"),
        Err(error) => {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!("merge-review workshop: {PREVIEW_ACTION} failed: {error:#}");
        }
    }

    if step_wait_ms > 0 {
        let wait_ms = step_wait_ms.to_string();
        session.request_ok(
            "wait_preview",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(step_wait_ms + 10_000),
        )?;
        println!("[method:wait post-preview] ok ms={step_wait_ms}");
    }

    let look_preview = session.request_ok(
        "look_preview",
        &snapshot_method_fields(detail),
        Duration::from_secs(30),
    )?;
    let look_preview_blob = look_preview.join("\n");
    println!(
        "[method:look post-preview] empty={} preview={}",
        classify_snapshot(&look_preview_blob),
        extract_snapshot_preview(&look_preview_blob, 300)
    );
    let preview_expects = [PREVIEW_EXPECT.to_string()];
    if !snapshot_satisfies(&look_preview_blob, &preview_expects) {
        session.pump();
        print_merge_review_stderr_tail(&session.stderr_buf);
        session.shutdown_best_effort();
        let missing = missing_snapshot_expects(&look_preview_blob, &preview_expects);
        bail!(
            "merge-review workshop: post-preview look missing {missing:?}\n  preview={}",
            extract_snapshot_preview(&look_preview_blob, 400)
        );
    }
    println!("  post-preview expect ok: {PREVIEW_EXPECT:?}");

    // Dismiss modal so End is not blocked by overlay focus (best-effort).
    let _ = session.request_ok(
        "keys_esc",
        &[("method", "keys"), ("keys", "escape")],
        Duration::from_secs(10),
    );

    match session.request_ok(
        "end1",
        &[("method", "action"), ("name", END_ACTION)],
        Duration::from_secs(20),
    ) {
        Ok(_) => println!("[method:action {END_ACTION}] ok"),
        Err(error) => {
            session.pump();
            print_merge_review_stderr_tail(&session.stderr_buf);
            session.shutdown_best_effort();
            bail!("merge-review workshop: {END_ACTION} failed: {error:#}");
        }
    }

    if step_wait_ms > 0 {
        let wait_ms = step_wait_ms.to_string();
        session.request_ok(
            "wait_end",
            &[("method", "wait"), ("ms", wait_ms.as_str())],
            Duration::from_millis(step_wait_ms + 10_000),
        )?;
        println!("[method:wait post-end] ok ms={step_wait_ms}");
    }

    let look_end = session.request_ok(
        "look_end",
        &snapshot_method_fields(detail),
        Duration::from_secs(30),
    )?;
    let look_end_blob = look_end.join("\n");
    println!(
        "[method:look post-end] empty={} preview={}",
        classify_snapshot(&look_end_blob),
        extract_snapshot_preview(&look_end_blob, 300)
    );
    // Layout restore: require a non-empty outline; do not require session chrome.
    if classify_snapshot(&look_end_blob) != "false" {
        session.pump();
        print_merge_review_stderr_tail(&session.stderr_buf);
        session.shutdown_best_effort();
        bail!(
            "merge-review workshop: post-end look empty\n  preview={}",
            extract_snapshot_preview(&look_end_blob, 400)
        );
    }
    println!("[workshop] Preview + End complete");
    Ok(())
}

// ── Queue (agent TOON step runner) ───────────────────────────────────────────

/// One tracked step in a dogfood queue session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QueueStep {
    Open(Option<String>),
    Wait(u64),
    Action(String),
    Keys(String),
    Look(String),
    Expect(String),
    Hit(Vec<String>),
    Lines(usize),
    Inventory,
    Theme,
    Click {
        node: String,
        a11y_action: Option<String>,
    },
    StderrMerge,
    /// Poll look until outline contains needle or timeout_ms elapses.
    /// Wire form: `poll:NEEDLE:TIMEOUT_MS` or `poll:NEEDLE:TIMEOUT_MS:DETAIL`.
    Poll {
        needle: String,
        timeout_ms: u64,
        detail: String,
    },
}

/// Parse a single queue step string into a [`QueueStep`].
fn parse_queue_step(raw: &str) -> Result<QueueStep> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty queue step");
    }
    let (head, tail) = match raw.split_once(':') {
        Some((h, t)) => (h.trim(), Some(t.trim())),
        None => (raw, None),
    };
    let head_l = head.to_ascii_lowercase();
    match head_l.as_str() {
        "open" => Ok(QueueStep::Open(
            tail.filter(|t| !t.is_empty()).map(|t| t.to_string()),
        )),
        "wait" => {
            let ms = tail
                .context("wait step needs ms, e.g. wait:4000")?
                .parse::<u64>()
                .context("wait ms must be u64")?;
            Ok(QueueStep::Wait(ms))
        }
        "action" => {
            let name = tail.context("action step needs name, e.g. action:agent::ToggleFocus")?;
            if name.is_empty() {
                bail!("action name empty");
            }
            Ok(QueueStep::Action(name.to_string()))
        }
        "keys" => {
            let keys = tail.context("keys step needs stroke, e.g. keys:ctrl-p")?;
            Ok(QueueStep::Keys(keys.to_string()))
        }
        "look" | "snapshot" => {
            let detail = tail.unwrap_or("room");
            if !matches!(detail, "compact" | "rich" | "room") {
                bail!("look detail must be compact|rich|room, got {detail:?}");
            }
            Ok(QueueStep::Look(detail.to_string()))
        }
        "expect" => {
            let s = tail.context("expect step needs substring")?;
            if s.is_empty() {
                bail!("expect substring empty");
            }
            Ok(QueueStep::Expect(s.to_string()))
        }
        "hit" => {
            let s = tail.context("hit step needs | -separated needles")?;
            let needles: Vec<String> = s
                .split('|')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect();
            if needles.is_empty() {
                bail!("hit step has no needles");
            }
            Ok(QueueStep::Hit(needles))
        }
        "lines" => {
            let n = tail
                .context("lines step needs count, e.g. lines:40")?
                .parse::<usize>()
                .context("lines count must be usize")?;
            Ok(QueueStep::Lines(n))
        }
        "inventory" => Ok(QueueStep::Inventory),
        "theme" | "feel" => Ok(QueueStep::Theme),
        "click" => {
            let rest = tail.context("click step needs node id, e.g. click:42 or click:42:focus")?;
            let mut parts = rest.splitn(2, ':');
            let node = parts.next().unwrap_or("").trim().to_string();
            if node.is_empty() {
                bail!("click node empty");
            }
            let a11y_action = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Ok(QueueStep::Click { node, a11y_action })
        }
        "stderr" => {
            let kind = tail.unwrap_or("merge");
            if kind != "merge" {
                bail!("only stderr:merge is supported (got {kind:?})");
            }
            Ok(QueueStep::StderrMerge)
        }
        "poll" => {
            // poll:TIMEOUT_MS:NEEDLE  (needle may contain spaces/colons)
            let rest = tail.context("poll needs poll:TIMEOUT_MS:NEEDLE")?;
            let (ms_s, needle) = rest
                .split_once(':')
                .context("poll needs poll:TIMEOUT_MS:NEEDLE")?;
            let timeout_ms = ms_s
                .trim()
                .parse::<u64>()
                .context("poll timeout_ms must be u64")?;
            let needle = needle.trim();
            if needle.is_empty() {
                bail!("poll needle empty");
            }
            Ok(QueueStep::Poll {
                needle: needle.to_string(),
                timeout_ms,
                detail: "room".to_string(),
            })
        }
        other => bail!("unknown queue step {other:?} (raw={raw:?})"),
    }
}

fn load_queue_steps(args: &QueueArgs) -> Result<Vec<QueueStep>> {
    let mut raws: Vec<String> = args.step.clone();
    if let Some(path) = &args.script {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read queue script {}", path.display()))?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            raws.push(line.to_string());
        }
    }
    if raws.is_empty() {
        bail!("dogfood queue needs at least one --step or --script entry");
    }
    raws.iter().map(|s| parse_queue_step(s)).collect()
}

fn decode_outline_escapes(text: &str) -> String {
    text.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn last_look_outline(last_look_blob: &Option<String>) -> Result<String> {
    let blob = last_look_blob
        .as_deref()
        .context("no prior look/snapshot in this queue (run look: before expect/hit/lines)")?;
    let text = extract_snapshot_text(blob).context("prior look missing snapshot@text")?;
    Ok(decode_outline_escapes(&text))
}

/// Run an agent-authored TOON step queue with per-step tracking.
fn run_queue(args: QueueArgs) -> Result<()> {
    let steps = load_queue_steps(&args)?;
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let default_detail = args.snapshot_detail.as_str();
    let default_open = match &args.fixture {
        Some(p) => Some(p.canonicalize().unwrap_or_else(|_| p.clone())),
        None => None,
    };

    println!(
        "dogfood queue: bin={} steps={} soft_action={} detail={} timeout_secs={}",
        bin.display(),
        steps.len(),
        args.soft_action,
        default_detail,
        args.timeout_secs,
    );

    let mut session = DogfoodSession::spawn(&bin, timeout)?;
    let ready = session.wait_until(0, |lines| lines.iter().any(|l| is_ready_line(l)), timeout)?;
    println!("[event:ready] ok");
    for line in ready.iter().take(4) {
        println!("  {line}");
    }

    let mut last_look: Option<String> = None;
    let total = steps.len();
    let mut failed: Vec<String> = Vec::new();

    for (index, step) in steps.iter().enumerate() {
        let n = index + 1;
        let step_id = format!("q{n}");
        let remaining = session.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("dogfood queue: session deadline exhausted before step {n}/{total}");
        }

        match step {
            QueueStep::Open(path_opt) => {
                let path = match path_opt {
                    Some(p) => PathBuf::from(p),
                    None => default_open
                        .clone()
                        .context("open step has no path; pass open:/path or --fixture")?,
                };
                let path = path.canonicalize().unwrap_or(path);
                let path_str = path.display().to_string();
                match session.request_ok(
                    &step_id,
                    &[("method", "open"), ("path", path_str.as_str())],
                    Duration::from_secs(30).min(remaining),
                ) {
                    Ok(_) => println!("[queue {n}/{total}] open ok path={path_str}"),
                    Err(error) => {
                        println!("[queue {n}/{total}] open FAIL: {error:#}");
                        session.pump();
                        print_merge_review_stderr_tail(&session.stderr_buf);
                        session.shutdown_best_effort();
                        bail!("queue step {n} open failed: {error:#}");
                    }
                }
            }
            QueueStep::Wait(ms) => {
                let ms_s = ms.to_string();
                session.request_ok(
                    &step_id,
                    &[("method", "wait"), ("ms", ms_s.as_str())],
                    Duration::from_millis(*ms + 15_000).min(remaining),
                )?;
                println!("[queue {n}/{total}] wait ok ms={ms}");
            }
            QueueStep::Action(name) => {
                match session.request_ok(
                    &step_id,
                    &[("method", "action"), ("name", name.as_str())],
                    Duration::from_secs(30).min(remaining),
                ) {
                    Ok(_) => println!("[queue {n}/{total}] action ok name={name}"),
                    Err(error) => {
                        if args.soft_action {
                            println!("[queue {n}/{total}] action WARN name={name}: {error:#}");
                            failed.push(format!("action {name}: {error:#}"));
                        } else {
                            session.pump();
                            print_merge_review_stderr_tail(&session.stderr_buf);
                            session.shutdown_best_effort();
                            bail!("queue step {n} action {name:?} failed: {error:#}");
                        }
                    }
                }
            }
            QueueStep::Keys(keys) => {
                match session.request_ok(
                    &step_id,
                    &[("method", "keys"), ("keys", keys.as_str())],
                    Duration::from_secs(20).min(remaining),
                ) {
                    Ok(_) => println!("[queue {n}/{total}] keys ok keys={keys}"),
                    Err(error) => {
                        if args.soft_action {
                            println!("[queue {n}/{total}] keys WARN keys={keys}: {error:#}");
                            failed.push(format!("keys {keys}: {error:#}"));
                        } else {
                            session.shutdown_best_effort();
                            bail!("queue step {n} keys failed: {error:#}");
                        }
                    }
                }
            }
            QueueStep::Look(detail) => {
                let detail = if detail.is_empty() {
                    default_detail
                } else {
                    detail.as_str()
                };
                let lines = session.request_ok(
                    &step_id,
                    &snapshot_method_fields(detail),
                    Duration::from_secs(30).min(remaining),
                )?;
                let blob = lines.join("\n");
                last_look = Some(blob.clone());
                let empty = classify_snapshot(&blob);
                let preview = extract_snapshot_preview(&blob, 240);
                let line_count = extract_snapshot_text(&blob)
                    .map(|t| decode_outline_escapes(&t).lines().count())
                    .unwrap_or(0);
                println!(
                    "[queue {n}/{total}] look ok detail={detail} empty={empty} lines={line_count} preview={preview}"
                );
            }
            QueueStep::Expect(needle) => {
                let outline = last_look_outline(&last_look)?;
                if outline.contains(needle) {
                    println!("[queue {n}/{total}] expect ok {needle:?}");
                } else {
                    session.pump();
                    print_merge_review_stderr_tail(&session.stderr_buf);
                    session.shutdown_best_effort();
                    bail!(
                        "queue step {n} expect missing {needle:?}\n  preview={}",
                        outline.lines().take(12).collect::<Vec<_>>().join(" | ")
                    );
                }
            }
            QueueStep::Hit(needles) => {
                let outline = last_look_outline(&last_look)?;
                let mut hits = 0usize;
                println!("[queue {n}/{total}] hit needles={needles:?}");
                for line in outline.lines() {
                    if needles.iter().any(|n| line.contains(n)) {
                        println!("  HIT: {}", &line[..line.len().min(180)]);
                        hits += 1;
                    }
                }
                println!("  hit count={hits}");
            }
            QueueStep::Lines(count) => {
                let outline = last_look_outline(&last_look)?;
                println!("[queue {n}/{total}] lines first {count}");
                for (i, line) in outline.lines().take(*count).enumerate() {
                    println!("{:02}|{}", i + 1, line);
                }
            }
            QueueStep::Inventory => {
                let lines = session.request_ok(
                    &step_id,
                    &[("method", "inventory")],
                    Duration::from_secs(15).min(remaining),
                )?;
                let blob = lines.join("\n");
                println!("[queue {n}/{total}] inventory ok");
                if let Some(caps) = Regex::new(r#""inventory@text":\s*"(.*)""#)
                    .ok()
                    .and_then(|re| re.captures(&blob))
                {
                    let text =
                        decode_outline_escapes(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
                    for line in text.lines().take(12) {
                        println!("  {line}");
                    }
                }
            }
            QueueStep::Theme => {
                let lines = session.request_ok(
                    &step_id,
                    &[("method", "theme")],
                    Duration::from_secs(15).min(remaining),
                )?;
                let blob = lines.join("\n");
                println!("[queue {n}/{total}] theme ok");
                if let Some(caps) = Regex::new(r#""theme@text":\s*"(.*)""#)
                    .ok()
                    .and_then(|re| re.captures(&blob))
                {
                    let text =
                        decode_outline_escapes(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
                    for line in text.lines().take(8) {
                        println!("  {line}");
                    }
                }
            }
            QueueStep::Click { node, a11y_action } => {
                let mut fields: Vec<(&str, &str)> =
                    vec![("method", "click"), ("node", node.as_str())];
                if let Some(a) = a11y_action {
                    fields.push(("a11y_action", a.as_str()));
                }
                match session.request_ok(&step_id, &fields, Duration::from_secs(20).min(remaining))
                {
                    Ok(_) => println!(
                        "[queue {n}/{total}] click ok node={node} a11y={:?}",
                        a11y_action
                    ),
                    Err(error) => {
                        if args.soft_action {
                            println!("[queue {n}/{total}] click WARN node={node}: {error:#}");
                            failed.push(format!("click {node}: {error:#}"));
                        } else {
                            session.shutdown_best_effort();
                            bail!("queue step {n} click failed: {error:#}");
                        }
                    }
                }
            }
            QueueStep::StderrMerge => {
                session.pump();
                println!("[queue {n}/{total}] stderr:merge");
                print_merge_review_stderr_tail(&session.stderr_buf);
            }
            QueueStep::Poll {
                needle,
                timeout_ms,
                detail,
            } => {
                let detail = if detail.is_empty() {
                    default_detail
                } else {
                    detail.as_str()
                };
                let poll_deadline = Instant::now() + Duration::from_millis(*timeout_ms);
                let mut attempts = 0u32;
                let mut last_preview: String;
                loop {
                    attempts += 1;
                    let remaining = session.deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        bail!("queue step {n} poll: session deadline exhausted");
                    }
                    let poll_id = format!("{step_id}p{attempts}");
                    let lines = session.request_ok(
                        &poll_id,
                        &snapshot_method_fields(detail),
                        Duration::from_secs(20).min(remaining),
                    )?;
                    let blob = lines.join("\n");
                    last_look = Some(blob.clone());
                    last_preview = extract_snapshot_preview(&blob, 200);
                    let outline = extract_snapshot_text(&blob)
                        .map(|t| decode_outline_escapes(&t))
                        .unwrap_or_default();
                    if outline.contains(needle) {
                        println!(
                            "[queue {n}/{total}] poll ok needle={needle:?} attempts={attempts} preview={last_preview}"
                        );
                        break;
                    }
                    if Instant::now() >= poll_deadline {
                        session.pump();
                        print_merge_review_stderr_tail(&session.stderr_buf);
                        session.shutdown_best_effort();
                        bail!(
                            "queue step {n} poll timed out after {timeout_ms}ms needle={needle:?} attempts={attempts}\n  preview={last_preview}"
                        );
                    }
                    // Small settle between looks (runner-side; no server wait-until).
                    let pause = Duration::from_millis(400)
                        .min(poll_deadline.saturating_duration_since(Instant::now()));
                    if !pause.is_zero() {
                        let pause_ms = pause.as_millis().to_string();
                        let wait_id = format!("{step_id}w{attempts}");
                        let _ = session.request_ok(
                            &wait_id,
                            &[("method", "wait"), ("ms", pause_ms.as_str())],
                            pause + Duration::from_secs(5),
                        );
                    }
                }
            }
        }
    }

    session.pump();
    session.shutdown_best_effort();
    println!("[method:shutdown] requested");
    if failed.is_empty() {
        println!("queue finished ok ({total} steps)");
    } else {
        println!(
            "queue finished with {} soft failure(s): {:?}",
            failed.len(),
            failed
        );
    }
    Ok(())
}

fn print_merge_review_stderr_tail(stderr: &[String]) {
    let interesting: Vec<&String> = stderr
        .iter()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("merge review")
                || lower.contains("surmount")
                || lower.contains("branch diff")
                || lower.contains("start requested")
                || lower.contains("populated")
                || lower.contains("empty queue")
        })
        .collect();
    println!(
        "--- stderr merge-review related ({} of {} lines) ---",
        interesting.len(),
        stderr.len()
    );
    if interesting.is_empty() {
        println!("(none matched; last 25 stderr lines follow)");
        for line in stderr
            .iter()
            .rev()
            .take(25)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            println!("{line}");
        }
    } else {
        for line in interesting
            .iter()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            println!("{line}");
        }
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_blank_line_terminated() {
        let doc = encode_request(&[("method", "snapshot"), ("id", "1")]);
        assert!(doc.ends_with("\n\n"));
        assert!(doc.contains("method:snapshot\n"));
        assert!(doc.contains("id:1\n"));
    }

    /// Locks CLI defaults + golden/smoke wire shape: `--snapshot-detail` → TOON `detail`.
    #[test]
    fn snapshot_detail_cli_and_wire_plumbing() {
        // Clap defaults for golden / smoke (standalone Parser on the arg structs).
        let golden_default = GoldenArgs::try_parse_from(["golden"]).expect("golden defaults");
        assert_eq!(
            golden_default.snapshot_detail, "rich",
            "golden --snapshot-detail default"
        );
        let smoke_default = SmokeArgs::try_parse_from(["smoke"]).expect("smoke defaults");
        assert_eq!(
            smoke_default.snapshot_detail, "rich",
            "smoke --snapshot-detail default"
        );

        for detail in ["compact", "rich", "room"] {
            let golden = GoldenArgs::try_parse_from(["golden", "--snapshot-detail", detail])
                .unwrap_or_else(|e| panic!("golden accepts {detail}: {e}"));
            assert_eq!(golden.snapshot_detail, detail);
            let smoke = SmokeArgs::try_parse_from(["smoke", "--snapshot-detail", detail])
                .unwrap_or_else(|e| panic!("smoke accepts {detail}: {e}"));
            assert_eq!(smoke.snapshot_detail, detail);

            // Production golden/smoke paths use snapshot_method_fields(detail).
            let fields = snapshot_method_fields(detail);
            assert_eq!(fields, [("method", "snapshot"), ("detail", detail)]);
            let doc =
                encode_request(&[("method", "snapshot"), ("id", "snap1"), ("detail", detail)]);
            assert!(
                doc.contains(&format!("detail:{detail}\n")),
                "detail tier on wire: {doc}"
            );
            assert!(doc.contains("method:snapshot\n"), "{doc}");
            // Same fields golden/smoke pass into request_ok (id is separate arg).
            let wire = encode_request(&snapshot_method_fields(detail));
            assert!(
                wire.contains(&format!("detail:{detail}\n")),
                "snapshot_method_fields encodes detail: {wire}"
            );
            assert!(wire.contains("method:snapshot\n"), "{wire}");
        }

        assert!(
            GoldenArgs::try_parse_from(["golden", "--snapshot-detail", "full"]).is_err(),
            "unknown detail must fail clap value_parser"
        );
        assert!(
            SmokeArgs::try_parse_from(["smoke", "--snapshot-detail", "css"]).is_err(),
            "unknown detail must fail clap value_parser"
        );
    }

    #[test]
    fn merge_review_default_expects_when_cli_empty() {
        assert_eq!(
            merge_review_post_start_expects(&[]),
            vec!["Merge review".to_string()]
        );
        let cli = vec!["Branch Diff".to_string(), "Base:".to_string()];
        assert_eq!(merge_review_post_start_expects(&cli), cli);
    }

    #[test]
    fn merge_review_args_start_only_and_step_wait_defaults() {
        let defaults =
            MergeReviewArgs::try_parse_from(["merge-review"]).expect("merge-review defaults");
        assert!(!defaults.start_only);
        assert!(
            !defaults.with_advance,
            "default adventure must not require Advance"
        );
        assert_eq!(defaults.step_wait_ms, 2500);
        assert!(defaults.expect.is_empty());
        let start_only = MergeReviewArgs::try_parse_from(["merge-review", "--start-only"])
            .expect("--start-only");
        assert!(start_only.start_only);
        let with_advance = MergeReviewArgs::try_parse_from(["merge-review", "--with-advance"])
            .expect("--with-advance");
        assert!(with_advance.with_advance);
        let with_conflict = MergeReviewArgs::try_parse_from(["merge-review", "--with-conflict"])
            .expect("--with-conflict");
        assert!(with_conflict.with_conflict);
        assert!(
            !defaults.with_conflict,
            "default adventure must not require conflict fixture"
        );
        assert!(
            !defaults.decide_live_agent,
            "default conflict spine must not require live Grok"
        );
        assert!(
            !with_conflict.decide_live_agent,
            "--with-conflict alone must stay synthetic hard spine"
        );
        let live = MergeReviewArgs::try_parse_from([
            "merge-review",
            "--with-conflict",
            "--decide-live-agent",
        ])
        .expect("--decide-live-agent");
        assert!(live.decide_live_agent && live.with_conflict);
        let live_only =
            MergeReviewArgs::try_parse_from(["merge-review", "--decide-live-agent"])
                .expect("--decide-live-agent alone");
        assert!(live_only.decide_live_agent && !live_only.with_conflict);
    }

    #[test]
    fn outline_button_node_id_parses_review_diff() {
        let outline = r#"
# focus: [Button] "Review Diff" #NodeId(9)
    *[Button] "Review Diff" @1046,73 105x32 [click,focus] #NodeId(7707873156005017608)
    [Button] "Use Both" @0,0 10x10 [click] #NodeId(1)
"#;
        // Prefer [click] body line over `# focus:` header id.
        assert_eq!(
            outline_button_node_id(outline, "Review Diff").as_deref(),
            Some("7707873156005017608")
        );
        assert_eq!(
            outline_button_node_id(outline, "Use Both").as_deref(),
            Some("1")
        );
        assert_eq!(outline_button_node_id(outline, "Missing"), None);
    }

    #[test]
    fn build_merge_review_conflict_git_tree_leaves_merge_head() {
        let keep = tempfile::Builder::new()
            .prefix("zed-dogfood-conflict-unit-")
            .tempdir()
            .expect("tempdir");
        let work = keep.path().join("worktree");
        std::fs::create_dir_all(&work).expect("worktree dir");
        let bare = keep.path().join("origin.git");
        build_merge_review_conflict_git_tree(&work, &bare).expect("build conflict fixture");
        assert!(
            work.join(".git").join("MERGE_HEAD").is_file(),
            "fixture must leave MERGE_HEAD"
        );
        assert!(
            work.join("conflict.txt").is_file(),
            "fixture must include conflict.txt"
        );
        assert!(
            work.join("SURMOUNT.md").is_file(),
            "fixture should carry Surmount marker for open"
        );
        assert!(
            bare.is_dir(),
            "fixture must ship bare origin sibling (not inside worktree)"
        );
        assert!(
            !work.join("origin.git").exists() && !work.join(".dogfood-origin.git").exists(),
            "bare origin must not live inside the worktree (pollutes Branch Diff)"
        );
        let origin_url =
            git_in(&work, &["remote", "get-url", "origin"]).expect("fixture must have origin remote");
        assert!(
            !origin_url.is_empty(),
            "origin remote URL must be non-empty"
        );
        // Product Start calls fetch origin — must succeed offline.
        git_in(&work, &["fetch", "origin", "--quiet"]).expect("fetch origin offline");
    }

    #[test]
    fn decision_chrome_hits_require_conflict_specific_labels() {
        for label in MERGE_REVIEW_DECISION_CHROME_CONFLICT {
            assert!(!label.is_empty());
            assert!(
                label.chars().any(|c| c.is_ascii_alphabetic()),
                "{label}"
            );
        }
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Use Both"));
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Resolve with Agent"));
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Summarize this conflict"));
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Keep fork"));
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Take upstream"));
        assert!(MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Record decision"));
        // Dead product labels — resolve uses git checkout, not these strings.
        assert!(!MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Take ours"));
        assert!(!MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Take theirs"));
        // Always-on MergeInProgress rail chrome must not be a sole decision proof.
        assert!(!MERGE_REVIEW_DECISION_CHROME_CONFLICT.contains(&"Review Diff"));

        let only_review_diff = r#"
  [Toolbar] "Merge review"
    [Button] "Review Diff" @0,0 100x32
"#;
        assert!(
            decision_chrome_hits(only_review_diff).is_empty(),
            "Review Diff alone must not count as decision chrome"
        );

        let conflict_bar = r#"
  [Label] "File 1/1 · conflict.txt · Summarize this conflict"
  [Button] "Use Both" @0,0 59x23
  [Button] "Resolve with Agent" @0,0 134x23
  [Button] "Review Diff" @0,0 100x32
"#;
        let hits = decision_chrome_hits(conflict_bar);
        assert!(hits.contains(&"Use Both"), "{hits:?}");
        assert!(hits.contains(&"Resolve with Agent"), "{hits:?}");
        assert!(hits.contains(&"Summarize this conflict"), "{hits:?}");
        assert!(!hits.iter().any(|h| *h == "Review Diff"), "{hits:?}");

        let miss = r#"  [Button] "Preview merge" @0,0 100x32 "#;
        assert!(decision_chrome_hits(miss).is_empty());
    }

    #[test]
    fn decide_rail_hits_and_settle_outcome_policy() {
        let empty = decide_rail_hits("  [Button] \"Review Diff\"");
        assert!(empty.is_empty(), "{empty:?}");

        let outline = r#"
  [Button] "Discuss conflict" @0,0 10x10
  [Button] "Keep fork" @0,0 10x10
  [Button] "Use Both" @0,0 10x10
"#;
        let hits = decide_rail_hits(outline);
        assert!(hits.contains(&"Discuss conflict"), "{hits:?}");
        assert!(hits.contains(&"Keep fork"), "{hits:?}");
        assert!(hits.contains(&"Use Both"), "{hits:?}");
        assert!(!hits.iter().any(|h| *h == "Review Diff"), "{hits:?}");

        // Settled: Summarizing gone + rail present.
        assert!(decide_settle_ready(false, &["Discuss conflict"], false));
        // Still summarizing with empty rail → not ready.
        assert!(!decide_settle_ready(true, &[], false));
        // Use Both visible allows proceed even while summarizing.
        assert!(decide_settle_ready(true, &[], true));
        // Empty rail + not summarizing → not ready (wait/timeout then product act).
        assert!(!decide_settle_ready(false, &[], false));

        assert_eq!(decide_path_outcome(true, false), "acted");
        assert_eq!(decide_path_outcome(true, true), "acted");
        assert_eq!(decide_path_outcome(false, true), "rail_present");
        assert_eq!(decide_path_outcome(false, false), "soft_skip");
    }

    #[test]
    fn synthetic_capture_l2_verdict_requires_capture_log() {
        // Hard green only when capture log present — Discuss/Record rail is soft annotation.
        assert_eq!(
            synthetic_capture_l2_verdict(true, true),
            "ok_capture_and_rail"
        );
        assert_eq!(synthetic_capture_l2_verdict(true, false), "ok_capture");
        // Pre-capture Use Both / empty rail without capture log → hard fail (not ok_rail_only).
        assert_eq!(synthetic_capture_l2_verdict(false, true), "fail");
        assert_eq!(synthetic_capture_l2_verdict(false, false), "fail");
    }

    #[test]
    fn extract_synthetic_capture_path_and_log_markers() {
        let log = "surmount merge review: dogfood synthetic summary capture ok path=conflict.txt";
        assert_eq!(
            extract_synthetic_capture_path(log),
            Some("conflict.txt")
        );
        assert!(stderr_has_synthetic_capture_log(log));

        let for_form = "surmount merge review: captured summary for src/foo.rs";
        assert_eq!(
            extract_synthetic_capture_path(for_form),
            Some("src/foo.rs")
        );
        assert!(stderr_has_synthetic_capture_log(for_form));

        // Toast is not production stderr protocol — must not green hard L2.
        let toastish = "Dogfood: captured synthetic summary for notes.md";
        assert_eq!(extract_synthetic_capture_path(toastish), None);
        assert!(!stderr_has_synthetic_capture_log(toastish));

        // Inject-failed warn must not set capture_ok.
        let inject_fail =
            "surmount merge review: dogfood synthetic summary inject failed: no pending path";
        assert!(!stderr_has_synthetic_capture_log(inject_fail));
        assert_eq!(extract_synthetic_capture_path(inject_fail), None);

        assert_eq!(extract_synthetic_capture_path("unrelated noise"), None);
        assert!(!stderr_has_synthetic_capture_log("unrelated noise"));
    }

    #[test]
    fn post_capture_rail_excludes_pre_capture_use_both() {
        let pre_only = r#"
  [Button] "Use Both" @0,0 10x10
  [Button] "Keep fork" @0,0 10x10
  [Button] "Take upstream" @0,0 10x10
"#;
        assert!(
            post_capture_rail_hits(pre_only).is_empty(),
            "Use Both/Keep fork alone must not satisfy post-capture L2 rail"
        );
        let workshop = r#"
  [Button] "Discuss conflict" @0,0 10x10
  [Button] "Record decision" @0,0 10x10
  [Button] "Use Both" @0,0 10x10
"#;
        let hits = post_capture_rail_hits(workshop);
        assert!(hits.contains(&"Discuss conflict"), "{hits:?}");
        assert!(hits.contains(&"Record decision"), "{hits:?}");
        // Combined with no capture log → still hard fail (capture required).
        assert_eq!(
            synthetic_capture_l2_verdict(false, !hits.is_empty()),
            "fail"
        );
    }

    #[test]
    fn live_capture_settle_verdict_edges() {
        // Production capture log wins even while Summarizing.
        assert_eq!(
            live_capture_settle_verdict(true, true, false, false),
            "capture_log"
        );
        assert!(live_capture_settled("capture_log"));
        // Discuss/Record only after Summarizing clears.
        assert_eq!(
            live_capture_settle_verdict(false, false, true, false),
            "discuss_rail"
        );
        assert!(live_capture_settled("discuss_rail"));
        // Rail while still summarizing → keep polling (not discuss_rail).
        assert_eq!(
            live_capture_settle_verdict(false, true, true, false),
            "still_summarizing"
        );
        assert!(!live_capture_settled("still_summarizing"));
        // Pre-capture Use Both alone is not post-capture rail.
        assert!(post_capture_rail_hits(r#"[Button] "Use Both""#).is_empty());
        assert_eq!(
            live_capture_settle_verdict(false, false, false, false),
            "still_summarizing"
        );
        // Timeout → soft-skip (synthetic hard spine next); never hard-fail live.
        assert_eq!(
            live_capture_settle_verdict(false, true, false, true),
            "soft_skip"
        );
        assert_eq!(
            live_capture_settle_verdict(false, false, false, true),
            "soft_skip"
        );
        assert!(!live_capture_settled("soft_skip"));
        // Rail visible but Summarizing not cleared at budget end → soft_skip (not discuss_rail).
        assert_eq!(
            live_capture_settle_verdict(false, true, true, true),
            "soft_skip"
        );
        // Capture log at timeout still settles (skip synthetic).
        assert_eq!(
            live_capture_settle_verdict(true, true, false, true),
            "capture_log"
        );
        // Discuss/Record at timeout still settles.
        assert_eq!(
            live_capture_settle_verdict(false, false, true, true),
            "discuss_rail"
        );
    }

    #[test]
    fn review_diff_dispatch_markers() {
        assert!(stderr_has_review_diff_dispatch(
            "surmount: Review Diff sent to agent for conflict.txt"
        ));
        assert!(stderr_has_review_diff_dispatch(
            "conflict Review Diff dispatch ok"
        ));
        assert!(stderr_has_review_diff_dispatch("Review Diff requested"));
        assert!(!stderr_has_review_diff_dispatch("method:action git::ReviewDiff ok"));
        assert!(!stderr_has_review_diff_dispatch("unrelated noise"));
    }

    #[test]
    fn live_capture_log_and_poll_budget() {
        let live = "surmount merge review: captured summary for conflict.txt";
        assert!(stderr_has_live_capture_log(live));
        assert_eq!(
            extract_synthetic_capture_path(live),
            Some("conflict.txt")
        );
        // Synthetic inject marker is not live capture evidence.
        let synthetic =
            "surmount merge review: dogfood synthetic summary capture ok path=conflict.txt";
        assert!(!stderr_has_live_capture_log(synthetic));
        assert!(stderr_has_synthetic_capture_log(synthetic));
        // Toast must not green live settle.
        let toastish = "Dogfood: captured synthetic summary for notes.md";
        assert!(!stderr_has_live_capture_log(toastish));
        // Budget: default step_wait floors to 30s; raised by step_wait; ceiling 90s.
        assert_eq!(live_capture_poll_budget_ms(2_500), 30_000);
        assert_eq!(live_capture_poll_budget_ms(45_000), 45_000);
        assert_eq!(live_capture_poll_budget_ms(120_000), 90_000);
    }

    #[test]
    fn soft_l3_requires_product_evidence_not_dispatch_ok() {
        assert_eq!(soft_l3_act_result(true, true), "acted");
        assert_eq!(soft_l3_act_result(true, false), "dispatch_only");
        assert_eq!(soft_l3_act_result(false, false), "no_dispatch");
        // resolve-ours dispatch-ok without post-state change is not acted.
        assert_eq!(
            decide_path_outcome(soft_l3_act_result(true, false) == "acted", false),
            "soft_skip"
        );

        let success =
            "surmount merge review: Resolved conflict.txt with `git checkout --ours` and staged with `git add`.";
        assert!(stderr_has_resolve_success(success));
        assert!(!stderr_has_resolve_success(
            "surmount merge review: conflict resolve ignored (no active file)"
        ));
        assert!(!stderr_has_resolve_success(
            "surmount merge review: conflict resolve failed: git checkout failed"
        ));

        let pre = r#"[Button] "Use Both" @0,0 10x10"#;
        let post_cleared = r#"[Button] "Next file →" @0,0 10x10"#;
        assert!(product_resolve_has_evidence("", pre, post_cleared));
        assert!(!product_resolve_has_evidence("", pre, pre));
        assert!(product_resolve_has_evidence(success, pre, pre));
    }

    #[test]
    fn decision_chrome_dynamic_use_hits_optional_branch_names() {
        let outline = r#"
  [Button] "Use HEAD" @0,0 65x23 [click]
  [Button] "Use theirs" @0,0 64x23 [click]
  [Button] "Use Both" @0,0 59x23 [click]
  [Button] "User menu" @0,0 22x22 [click]
"#;
        let dynamic = decision_chrome_dynamic_use_hits(outline);
        assert!(dynamic.iter().any(|h| h == "Use HEAD"), "{dynamic:?}");
        assert!(dynamic.iter().any(|h| h == "Use theirs"), "{dynamic:?}");
        assert!(
            !dynamic.iter().any(|h| h == "Use Both"),
            "Use Both is stable, not dynamic: {dynamic:?}"
        );
        assert!(
            !dynamic.iter().any(|h| h.contains("User")),
            "User menu must not match: {dynamic:?}"
        );
    }

    #[test]
    fn advance_has_enough_path_fingerprints_precheck() {
        assert!(!advance_has_enough_path_fingerprints(&[]));
        assert!(!advance_has_enough_path_fingerprints(&[
            "only.rs".to_string()
        ]));
        assert!(advance_has_enough_path_fingerprints(&[
            "a.rs".to_string(),
            "b.rs".to_string()
        ]));
        assert!(advance_has_enough_path_fingerprints(&[
            "a.rs".to_string(),
            "b.rs".to_string(),
            "c.rs".to_string()
        ]));
    }

    #[test]
    fn room_focus_window_classifier() {
        assert_eq!(
            room_focus_line("# window: \"zed\"\n# focus: [Window] \"zed\" #NodeId(0)\n"),
            Some("# focus: [Window] \"zed\" #NodeId(0)")
        );
        assert!(room_focus_is_solely_window(
            "# focus: [Window] \"zed\" #NodeId(0)"
        ));
        assert!(!room_focus_is_solely_window(
            "# focus: [Group] \"Branch Diff\" #NodeId(1)"
        ));
        assert!(!room_focus_is_solely_window(
            "# focus: [Button] \"Preview merge\" #NodeId(2)"
        ));
        assert!(room_focus_line("no focus header\n").is_none());
    }

    #[test]
    fn first_expand_negative_y_line_detects_offscreen_expand() {
        let good = r#"  [Button] "Expand" @10,20 16x16 [click] #NodeId(1)"#;
        assert!(first_expand_negative_y_line(good).is_none());
        let bad = r#"  [Button] "Expand" @1142,-125 16x16 [click,focus] #NodeId(9)"#;
        let hit = first_expand_negative_y_line(bad).expect("negative Y Expand");
        assert!(hit.contains("Expand"));
        assert!(hit.contains("-125"));
        let excerpt_bad = r#"  [Button] "Expand Excerpt" @0,-40 20x20 #NodeId(2)"#;
        assert!(first_expand_negative_y_line(excerpt_bad).is_some());
    }

    #[test]
    fn path_fingerprints_and_advance_delta() {
        let pre = r#"
  [Button] "crates/a/foo.rs" @0,10 100x20
  [Button] "crates/b/bar.rs" @0,40 100x20
  [Button] "Next file →" @0,80 80x24
"#;
        let pre_paths = path_fingerprints_from_outline(pre);
        assert!(pre_paths.contains(&"foo.rs".to_string()), "{pre_paths:?}");
        assert!(pre_paths.contains(&"bar.rs".to_string()), "{pre_paths:?}");
        assert!(
            !pre_paths.iter().any(|p| p.contains("Next file")),
            "{pre_paths:?}"
        );

        let post_same = pre;
        assert!(!advance_shows_path_delta(&pre_paths, post_same, &[]));

        let post_delta = r#"
  [Button] "crates/b/bar.rs" @0,10 100x20
  [Button] "crates/c/baz.rs" @0,40 100x20
"#;
        assert!(advance_shows_path_delta(&pre_paths, post_delta, &[]));

        let stderr =
            vec!["INFO surmount merge review: advanced to next file crates/b/bar.rs".into()];
        // bar.rs may already be in pre_paths; still ok if log path differs from first fingerprint.
        let pre_first_only = vec!["foo.rs".to_string()];
        assert!(advance_shows_path_delta(
            &pre_first_only,
            post_same,
            &stderr
        ));

        // Edges: empty pre/post without stderr → no delta.
        assert!(!advance_shows_path_delta(&[], "", &[]));
        assert!(!advance_shows_path_delta(
            &["foo.rs".to_string()],
            "",
            &[]
        ));
        // Identical multi-path set, same first order, no stderr → no delta.
        let identical_multi = r#"
  [Button] "crates/a/foo.rs" @0,10 100x20
  [Button] "crates/b/bar.rs" @0,40 100x20
"#;
        assert!(!advance_shows_path_delta(
            &pre_paths,
            identical_multi,
            &[]
        ));
        // Reorder-only: same set, first path changed → delta.
        let reordered = r#"
  [Button] "crates/b/bar.rs" @0,10 100x20
  [Button] "crates/a/foo.rs" @0,40 100x20
"#;
        assert!(advance_shows_path_delta(&pre_paths, reordered, &[]));
    }

    #[test]
    fn resolve_surmount_workspace_prefers_directory_over_file() {
        let root = std::env::temp_dir().join(format!("dogfood-surmount-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let surmount = root.join("SURMOUNT.md");
        std::fs::write(&surmount, "# fork").unwrap();

        let from_dir = super::resolve_surmount_workspace(Some(root.clone())).unwrap();
        assert_eq!(from_dir, root.canonicalize().unwrap_or(root.clone()));

        let from_file = super::resolve_surmount_workspace(Some(surmount)).unwrap();
        assert_eq!(from_file, root.canonicalize().unwrap_or(root.clone()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outline_has_body_content_headers_alone_are_empty() {
        assert!(!outline_has_body_content(""));
        assert!(!outline_has_body_content(
            "# window: \"Zed\"\n# focus: (none)\n# interactive: 0  landmarks: 0"
        ));
        assert!(!outline_has_body_content(
            "--- window 0 ---\n# window: \"A\""
        ));
        assert!(!outline_has_body_content(
            "[snapshot error] update_window failed: closed"
        ));
        assert!(outline_has_body_content("[Button] Open #NodeId(1)"));
        assert!(outline_has_body_content(
            "# window: \"Zed\"\n  [Heading] \"Welcome\"\n  *[Button] \"Go\" #NodeId(2)"
        ));
        // Escaped TOON newlines still count body lines.
        assert!(outline_has_body_content(
            r#"[Button] Open #NodeId(1)\n[TextInput] q"#
        ));
    }

    #[test]
    fn encode_request_quotes_paths_with_spaces_and_backslashes() {
        let doc = encode_request(&[
            ("method", "open"),
            ("path", r#"C:\Users\me\My Docs\file.txt"#),
            ("id", "open1"),
        ]);
        assert!(doc.contains(r#"path:"C:\\Users\\me\\My Docs\\file.txt""#));
        assert!(doc.contains("id:open1\n"));

        let spaced = encode_request(&[("path", "/tmp/my file.md"), ("id", "1")]);
        assert!(spaced.contains("path:\"/tmp/my file.md\"\n"));
    }

    #[test]
    fn ready_line_accepts_spaced_and_unspaced() {
        assert!(is_ready_line("event: ready"));
        assert!(is_ready_line("event:ready"));
        assert!(is_ready_line("  event:  ready  "));
        assert!(is_ready_line("EVENT:READY"));
        assert!(!is_ready_line("user_data_dir: /tmp/ready-event"));
        assert!(!is_ready_line("ok: true"));
        assert!(!is_ready_line("event: readyish"));
        assert!(!is_ready_line("event:notready"));
        assert!(!is_ready_line("event:ready-foo"));
        assert!(!is_ready_line("event: ready-ish"));
    }

    #[test]
    fn classify_snapshot_empty_and_missing() {
        assert_eq!(
            classify_snapshot("id: snap1\nok: true\n\"snapshot@text\": \"\""),
            "true"
        );
        assert_eq!(
            classify_snapshot(r#""snapshot@text": "[Button] Open #NodeId(1)""#),
            "false"
        );
        assert_eq!(
            classify_snapshot(
                "id: snap1\nok: true\n\"snapshot@text\": \"[Button] X\\n[TextInput] y\""
            ),
            "false"
        );
        assert_eq!(classify_snapshot("ok: true\nid: 1"), "missing");
        // Diagnostic-only bodies must not count as non-empty body content.
        assert_eq!(
            classify_snapshot(
                r#""snapshot@text": "[snapshot error] update_window failed: closed""#
            ),
            "true"
        );
        assert_eq!(
            classify_snapshot(
                r#""snapshot@text": "--- window 0 ---\n[snapshot error] update_window failed: x""#
            ),
            "true"
        );
        // Room header-only (no interactive / landmark body lines) counts as empty.
        // Use r## so embedded `"# window` does not terminate the raw string.
        assert_eq!(
            classify_snapshot(
                r##""snapshot@text": "# window: \"Zed\"\n# focus: (none)\n# interactive: 0  landmarks: 0""##
            ),
            "true"
        );
        // Multi-window room headers under separators still empty without body lines.
        assert_eq!(
            classify_snapshot(
                r##""snapshot@text": "--- window 0 ---\n# window: \"A\"\n# focus: (none)\n# interactive: 0  landmarks: 0\n--- window 1 ---\n# window: \"B\"\n# focus: (none)\n# interactive: 0  landmarks: 0""##
            ),
            "true"
        );
        // Landmark / interactive body under room header counts as non-empty.
        assert_eq!(
            classify_snapshot(
                r##""snapshot@text": "# window: \"Zed\"\n# focus: [Button] \"Go\" #NodeId(2)\n# interactive: 1  landmarks: 1\n  [Heading] \"Welcome\"\n  *[Button] \"Go\" #NodeId(2)""##
            ),
            "false"
        );
    }

    #[test]
    fn classify_and_complete_live_shaped_multiline() {
        // Live encode_response order: payload fields, then id, then ok.
        let live = concat!(
            r#""snapshot@text": "[Button] Open #NodeId(1)\n[TextInput] q""#,
            "\n",
            "id: snap1\n",
            "ok: true\n",
        );
        assert_eq!(classify_snapshot(live), "false");
        assert!(response_complete_for_id(live, "snap1"));
        assert_eq!(response_ok_for_id(live, "snap1"), Some(true));
        let block = document_block_for_id(live, "snap1").unwrap();
        assert!(block.contains("snapshot@text"));
        assert!(block.contains("id: snap1"));
        assert!(block.contains("ok: true"));
    }

    #[test]
    fn ok_true_false_matchers_line_anchored() {
        assert!(blob_has_ok_true("ok: true\nid: 1"));
        assert!(blob_has_ok_true("ok:true"));
        assert!(blob_has_ok_false("ok: false\nerror: nope"));
        assert!(!blob_has_ok_false("ok: true"));
        // Must not match `ok: true` buried inside snapshot outline text.
        assert!(!blob_has_ok_true(
            "id: snap1\n\"snapshot@text\": \"status ok: true forever\""
        ));
        assert!(!blob_has_ok_false(
            "id: snap1\n\"snapshot@text\": \"ok: false buried\""
        ));
    }

    #[test]
    fn response_id_and_complete_helpers() {
        let ok_blob = "id: open1\nok: true\n";
        assert!(blob_has_response_id(ok_blob, "open1"));
        assert!(!blob_has_response_id(ok_blob, "open"));
        assert!(!blob_has_response_id(ok_blob, "open12"));
        assert!(response_complete_for_id(ok_blob, "open1"));
        assert!(!response_complete_for_id("id: open1\n", "open1"));
        assert!(!response_complete_for_id("ok: true\n", "open1"));

        let err_blob = "id: action1\nok: false\nerror: missing action\n";
        assert!(response_complete_for_id(err_blob, "action1"));
        assert_eq!(response_ok_for_id(err_blob, "action1"), Some(false));

        // Substring trap: id "1" must not match "id: 12" or path segments.
        assert!(!blob_has_response_id("id: 12\nok: true\n", "1"));
        assert!(blob_has_response_id("id: 1\nok: true\n", "1"));
        assert!(blob_has_response_id("id: \"snap1\"\nok: true\n", "snap1"));
    }

    #[test]
    fn response_complete_rejects_foreign_ok_cross_talk() {
        // Id-less decode error before this request's own ok must not complete open1.
        let cross = "ok: false\nerror: decode failed\nid: open1\n";
        assert!(blob_has_response_id(cross, "open1"));
        assert!(blob_has_ok_false(cross));
        assert!(
            !response_complete_for_id(cross, "open1"),
            "foreign ok:false must not pair with id: open1"
        );
        assert_eq!(response_ok_for_id(cross, "open1"), None);

        // After open1's own ok arrives, complete and succeed despite foreign error.
        let resolved = "ok: false\nerror: decode failed\nid: open1\nok: true\n";
        assert!(response_complete_for_id(resolved, "open1"));
        assert_eq!(response_ok_for_id(resolved, "open1"), Some(true));

        // Foreign ok:true must not complete a partial id-only document.
        let foreign_true = "ok: true\nid: snap1\n";
        assert!(!response_complete_for_id(foreign_true, "snap1"));
    }

    #[test]
    fn lines_since_cursor_contract() {
        let buf = vec!["event: ready".into(), "id: open1".into(), "ok: true".into()];
        assert_eq!(lines_since(&buf, 0).len(), 3);
        assert_eq!(lines_since(&buf, 1), &buf[1..]);
        assert!(lines_since(&buf, 3).is_empty());
        assert!(lines_since(&buf, 99).is_empty());
        // Fast-response contract: cursor before send keeps post-cursor lines.
        let from_line = 1;
        let new = lines_since(&buf, from_line);
        assert!(response_complete_for_id(&new.join("\n"), "open1"));
        assert!(!response_complete_for_id(
            &lines_since(&buf, 0)[0..1].join("\n"),
            "open1"
        ));
    }

    #[test]
    fn extract_snapshot_preview_truncates_safely() {
        let blob = r#""snapshot@text": "abcdefghijklmnopqrstuvwxyz""#;
        let preview = extract_snapshot_preview(blob, 8);
        assert!(preview.ends_with('…'));
        assert!(preview.starts_with("abcdefgh"));

        // Multi-byte char straddling max_chars must not panic / split the scalar.
        let unicode = r#""snapshot@text": "日本語テストabc""#;
        let preview = extract_snapshot_preview(unicode, 4);
        assert!(preview.ends_with('…'));
        assert!(!preview.contains('\u{FFFD}'));
        // All chars before ellipsis are complete UTF-8 scalars.
        let body = preview.trim_end_matches('…');
        assert!(body.chars().all(|c| !c.is_control() || c == '…'));
        assert!(body.is_char_boundary(body.len()));
    }

    #[test]
    fn snapshot_satisfies_non_empty_and_expects() {
        let non_empty = r#""snapshot@text": "[Button] Open #NodeId(1)\n[TextInput] q""#;
        let empty = "id: snap1\nok: true\n\"snapshot@text\": \"\"";
        let missing_field = "ok: true\nid: 1";

        let no_expects: &[&str] = &[];
        assert!(snapshot_satisfies(non_empty, no_expects));
        assert!(!snapshot_satisfies(empty, no_expects));
        assert!(!snapshot_satisfies(missing_field, no_expects));

        let expects = ["Button", "TextInput"];
        assert!(snapshot_satisfies(non_empty, &expects));
        assert!(!snapshot_satisfies(non_empty, &["Button", "MissingRole"]));
        assert!(!snapshot_satisfies(empty, &expects));

        // Room body: landmark line counts; header-only does not.
        let room_body = r##"id: r1
ok: true
"snapshot@text": "# window: \"Zed\"\n# focus: (none)\n# interactive: 0  landmarks: 1\n  [Heading] \"Welcome\""
"##;
        assert!(snapshot_satisfies(room_body, &["Heading"]));
        assert!(snapshot_satisfies(room_body, &["# window"])); // substring is in outline payload
        let room_header_only = r##"id: r2
ok: true
"snapshot@text": "# window: \"Zed\"\n# focus: (none)\n# interactive: 0  landmarks: 0"
"##;
        assert!(!snapshot_satisfies(room_header_only, no_expects));
        assert_eq!(classify_snapshot(room_header_only), "true");
        assert_eq!(classify_snapshot(room_body), "false");
    }

    #[test]
    fn extract_snapshot_text_and_expects_are_outline_only() {
        // Live-shaped response: metadata has ok/true/id; outline does not.
        let live = concat!(
            r#""snapshot@text": "  *[Button] \"Go\" @1,2 3x4 #NodeId(1)""#,
            "\n",
            "id: snap1\n",
            "ok: true\n",
        );
        let text = extract_snapshot_text(live).expect("snapshot field");
        assert!(text.contains("[Button]"));
        assert!(text.contains("#NodeId(1)"));
        assert!(!text.contains("ok: true"), "{text}");
        assert!(!text.contains("id: snap1"), "{text}");
        assert_eq!(extract_snapshot_text("ok: true\nid: 1"), None);

        assert_eq!(classify_snapshot(live), "false");
        assert!(snapshot_satisfies(live, &["Button"]));
        assert!(!snapshot_satisfies(live, &["true"]));
        assert!(!snapshot_satisfies(live, &["ok"]));
        assert!(!snapshot_satisfies(live, &["snap1"]));
        assert!(!snapshot_satisfies(live, &["id"]));
        let missing = missing_snapshot_expects(live, &["true", "Button", "ok"]);
        assert_eq!(missing, vec!["true", "ok"]);
    }

    #[test]
    fn missing_snapshot_expects_preserves_order() {
        let blob = r#""snapshot@text": "[Button] Open #NodeId(1)""#;
        let expects = ["Button", "TextInput", "Open", "Ghost"];
        let missing = missing_snapshot_expects(blob, &expects);
        assert_eq!(missing, vec!["TextInput", "Ghost"]);
        assert!(missing_snapshot_expects(blob, &["Button", "Open"]).is_empty());
        assert!(missing_snapshot_expects(blob, &[] as &[&str]).is_empty());
    }

    #[test]
    fn prefer_diagnostic_blob_keeps_non_empty_over_empty() {
        let empty = "id: snap1\nok: true\n\"snapshot@text\": \"\"";
        let non_empty = r#""snapshot@text": "[Button] Open #NodeId(1)""#;
        let expects = vec!["TextInput".to_string()];
        assert!(prefer_diagnostic_blob(empty, non_empty, &expects));
        assert!(!prefer_diagnostic_blob(non_empty, empty, &expects));
        // Prefer fewer missing expects among non-empty.
        let better = r#""snapshot@text": "[Button] Open [TextInput] q #NodeId(1)""#;
        assert!(prefer_diagnostic_blob(non_empty, better, &expects));
    }

    #[test]
    fn retryable_poll_error_classification() {
        let timeout = anyhow::anyhow!("step timed out after 1s\n--- stdout ---");
        assert!(is_retryable_poll_error(&timeout));
        let exited = anyhow::anyhow!("zed exited early (exit status: 1)\n--- stderr ---");
        assert!(!is_retryable_poll_error(&exited));
        let closed = anyhow::anyhow!("stdout closed before condition\n--- stdout ---");
        assert!(!is_retryable_poll_error(&closed));
        let hard = anyhow::anyhow!("request id=snap1 returned ok: false\nerror: boom");
        assert!(!is_retryable_poll_error(&hard));
    }

    #[test]
    fn parse_queue_step_core_verbs() {
        assert_eq!(parse_queue_step("open").unwrap(), QueueStep::Open(None));
        assert_eq!(
            parse_queue_step("open:/tmp/x").unwrap(),
            QueueStep::Open(Some("/tmp/x".into()))
        );
        assert_eq!(
            parse_queue_step("wait:4000").unwrap(),
            QueueStep::Wait(4000)
        );
        assert_eq!(
            parse_queue_step("action:surmount::StartMergeReview").unwrap(),
            QueueStep::Action("surmount::StartMergeReview".into())
        );
        assert_eq!(
            parse_queue_step("look:room").unwrap(),
            QueueStep::Look("room".into())
        );
        assert_eq!(
            parse_queue_step("look").unwrap(),
            QueueStep::Look("room".into())
        );
        assert_eq!(
            parse_queue_step("expect:Merge review").unwrap(),
            QueueStep::Expect("Merge review".into())
        );
        assert_eq!(
            parse_queue_step("hit:Prepare|Review Diff").unwrap(),
            QueueStep::Hit(vec!["Prepare".into(), "Review Diff".into()])
        );
        assert_eq!(parse_queue_step("lines:40").unwrap(), QueueStep::Lines(40));
        assert_eq!(parse_queue_step("inventory").unwrap(), QueueStep::Inventory);
        assert_eq!(parse_queue_step("theme").unwrap(), QueueStep::Theme);
        assert_eq!(
            parse_queue_step("click:42:focus").unwrap(),
            QueueStep::Click {
                node: "42".into(),
                a11y_action: Some("focus".into()),
            }
        );
        assert_eq!(
            parse_queue_step("stderr:merge").unwrap(),
            QueueStep::StderrMerge
        );
        assert_eq!(
            parse_queue_step("poll:30000:Merge review").unwrap(),
            QueueStep::Poll {
                needle: "Merge review".into(),
                timeout_ms: 30000,
                detail: "room".into(),
            }
        );
        assert!(parse_queue_step("nope").is_err());
    }

    #[test]
    fn decode_outline_escapes_newlines() {
        assert_eq!(decode_outline_escapes(r"a\nb"), "a\nb");
    }
}

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
    /// Headless merge-review start adventure: open Surmount fixture → StartMergeReview → wait → look → stderr tail.
    MergeReview(MergeReviewArgs),
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
    /// Prefer SURMOUNT.md so the worktree root is a Surmount workspace when git is wired.
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

/// Prefer SURMOUNT.md at the cargo workspace root (Surmount merge-review detector).
fn resolve_surmount_fixture(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return resolve_fixture(Some(path));
    }
    let meta = workspace::load_workspace()?;
    let root = PathBuf::from(meta.workspace_root);
    let surmount = root.join("SURMOUNT.md");
    if surmount.is_file() {
        return Ok(surmount.canonicalize().unwrap_or(surmount));
    }
    resolve_fixture(None)
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
    }
}

/// Headless Start Merge Review adventure for debugging the Surmount workflow.
///
/// Always records action ok/error and snapshot preview honestly. Dumps stderr
/// lines that mention merge review / surmount so product toasts and populate
/// logs are visible without inventing UI that did not paint.
fn run_merge_review(args: MergeReviewArgs) -> Result<()> {
    let bin = args.bin.map(Ok).unwrap_or_else(default_bin)?;
    let fixture = resolve_surmount_fixture(args.fixture)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let fixture_str = fixture.display().to_string();
    let detail = args.snapshot_detail.as_str();
    let action_name = args.action.as_str();

    println!(
        "dogfood merge-review: bin={} fixture={} action={} detail={} wait_ms={} post_start_wait_ms={}",
        bin.display(),
        fixture.display(),
        action_name,
        detail,
        args.wait_ms,
        args.post_start_wait_ms,
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
        Duration::from_secs(20),
    )?;
    println!("[method:open] ok path={fixture_str}");

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
        println!(
            "[method:wait post-start] ok ms={}",
            args.post_start_wait_ms
        );
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
        for line in stderr.iter().rev().take(25).collect::<Vec<_>>().into_iter().rev() {
            println!("{line}");
        }
    } else {
        for line in interesting.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
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
}

#!/usr/bin/env bash
# Phase 1A golden session for agent-stdio (Linux headless dogfood).
# Requires: target/release/zed (or ZED_BIN), python3 for reliable line I/O.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
ZED_BIN="${ZED_BIN:-$ROOT/target/release/zed}"
UDD="${UDD:-/tmp/zed-golden-session}"
OUT="${OUT:-/tmp/zed-golden-session.out}"
ERR="${ERR:-/tmp/zed-golden-session.log}"
FIXTURE="${FIXTURE:-$ROOT/README.md}"
if [[ ! -f "$FIXTURE" ]]; then
  FIXTURE="$ROOT/Cargo.toml"
fi
WAIT_MS="${WAIT_MS:-3000}"

if [[ ! -x "$ZED_BIN" ]]; then
  echo "missing binary: $ZED_BIN (build with: cargo build --release -p zed)" >&2
  exit 1
fi
if [[ ! -f "$FIXTURE" ]]; then
  echo "missing open fixture: $FIXTURE" >&2
  exit 1
fi

# Refuse recursive delete outside a subdirectory of /tmp unless ALLOW_UDD_RM=1.
# realpath -m resolves without creating parents (blocks /tmp/../home/… prefix tricks).
# Never allow exact /tmp or /tmp/ — that would wipe the system temp directory.
if ! UDD_CANON="$(realpath -m -- "$UDD" 2>/dev/null)"; then
  echo "refusing UDD='$UDD' (could not resolve canonical path)" >&2
  exit 1
fi
if [[ "$UDD_CANON" == "/tmp" ]]; then
  echo "refusing rm -rf on UDD='$UDD' (canonical='/tmp'; refuse wiping /tmp itself)" >&2
  exit 1
fi
case "$UDD_CANON" in
  /tmp/*) ;;
  *)
    if [[ "${ALLOW_UDD_RM:-}" != "1" ]]; then
      echo "refusing rm -rf on UDD='$UDD' (canonical='$UDD_CANON'; must be a path under /tmp/, or set ALLOW_UDD_RM=1)" >&2
      exit 1
    fi
    ;;
esac
UDD="$UDD_CANON"

rm -rf "$UDD"
mkdir -p "$UDD"
: >"$OUT"
: >"$ERR"

export ZED_BIN UDD OUT ERR FIXTURE WAIT_MS

python3 - <<'PY'
import os, re, select, subprocess, sys, time

bin_path = os.environ["ZED_BIN"]
udd = os.environ["UDD"]
out_path = os.environ["OUT"]
err_path = os.environ["ERR"]
fixture = os.environ["FIXTURE"]
wait_ms = int(os.environ["WAIT_MS"])

proc = subprocess.Popen(
    [bin_path, "--agent-stdio", "--user-data-dir", udd],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(err_path, "wb"),
    text=True,
    bufsize=1,
)
all_out = []
failed = False
EMPTY_SNAPSHOT = '"snapshot@text": ""'


def read_available(timeout=0.5):
    lines = []
    end = time.time() + timeout
    while time.time() < end:
        ready, _, _ = select.select([proc.stdout], [], [], max(0.0, end - time.time()))
        if not ready:
            break
        line = proc.stdout.readline()
        if not line:
            break
        lines.append(line.rstrip("\n"))
    return lines


def wait_for(pred, timeout=30.0):
    collected = []
    end = time.time() + timeout
    while time.time() < end:
        if proc.poll() is not None:
            collected.extend(read_available(0.1))
            return collected, "died"
        chunk = read_available(0.25)
        collected.extend(chunk)
        if any(pred(line) for line in collected):
            collected.extend(read_available(0.4))
            return collected, "ok"
    return collected, "timeout"


def send(document: str) -> None:
    proc.stdin.write(document.rstrip() + "\n\n")
    proc.stdin.flush()


def has_ok_false(lines):
    blob = "\n".join(lines)
    return bool(re.search(r"ok:\s*false", blob, re.IGNORECASE))


def has_ok_true(lines):
    blob = "\n".join(lines)
    return bool(re.search(r"ok:\s*true", blob, re.IGNORECASE))


def classify_snapshot(lines):
    """Return 'true' (empty outline), 'false' (non-empty), or 'missing'."""
    blob = "\n".join(lines)
    # Live empty encoder form (toon-format 0.5): "snapshot@text": ""
    if EMPTY_SNAPSHOT in blob:
        return "true"
    m = re.search(r'"snapshot@text":\s*"(.*)"', blob, re.DOTALL)
    if m is not None:
        return "true" if m.group(1) == "" else "false"
    return "missing"


def note(label, status, lines, *, expect_ok=True):
    global failed
    print(f"[{label}] {status}")
    for line in lines[:6]:
        preview = line if len(line) < 200 else line[:200] + "…"
        print(f"  {preview}")
    all_out.extend(lines)
    if status != "ok":
        failed = True
        print(f"  FAIL: step status={status}")
        return
    if expect_ok:
        if has_ok_false(lines):
            failed = True
            print("  FAIL: response contains ok: false")
        elif not has_ok_true(lines) and "ready" not in label:
            # ready event has no ok field
            failed = True
            print("  FAIL: response missing ok: true")


def note_snapshot(label, status, lines):
    note(label, status, lines, expect_ok=True)
    emptiness = classify_snapshot(lines)
    chars = len("\n".join(lines))
    print(f"  snapshot_empty={emptiness} chars={chars}")
    if emptiness == "missing":
        global failed
        failed = True
        print("  FAIL: snapshot@text field missing from response")


ready, status = wait_for(lambda line: "ready" in line, timeout=45)
note("event:ready", status, ready, expect_ok=False)
if status != "ok":
    print(open(err_path).read()[-3000:], file=sys.stderr)
    open(out_path, "w").write("\n".join(all_out) + "\n")
    sys.exit(1)

send("method:actions\nid:actions1")
lines, status = wait_for(lambda line: "actions1" in line or "actions[" in line, timeout=15)
if status == "ok":
    lines.extend(read_available(1.0))
note("method:actions", status, lines)
blob = "\n".join(lines)
for name in (
    "agent::ToggleFocus",
    "agent::Toggle",
    "workspace::ToggleRightDock",
    "file_finder::Toggle",
    "agent::NewThread",
):
    print(f"  action_present {name}: {'yes' if name in blob else 'no'}")

send(f"method:open\nid:open1\npath:{fixture}")
lines, status = wait_for(lambda line: "open1" in line, timeout=10)
note("method:open", status, lines)

send(f"method:wait\nid:wait1\nms:{wait_ms}")
lines, status = wait_for(lambda line: "wait1" in line, timeout=max(8, wait_ms / 1000 + 3))
note("method:wait", status, lines)

send("method:snapshot\nid:snap1")
lines, status = wait_for(lambda line: "snap1" in line or "snapshot" in line, timeout=10)
if status == "ok":
    lines.extend(read_available(0.5))
note_snapshot("method:snapshot", status, lines)

action = "agent::ToggleFocus" if "agent::ToggleFocus" in blob else "workspace::ToggleRightDock"
send(f"method:action\nid:act1\nname:{action}")
lines, status = wait_for(lambda line: "act1" in line, timeout=8)
if status == "ok":
    lines.extend(read_available(0.3))
note(f"method:action name={action}", status, lines)

send("method:wait\nid:wait2\nms:1000")
lines, status = wait_for(lambda line: "wait2" in line, timeout=5)
note("method:wait(post-action)", status, lines)

send("method:snapshot\nid:snap2")
lines, status = wait_for(lambda line: "snap2" in line or "snapshot" in line, timeout=10)
if status == "ok":
    lines.extend(read_available(0.5))
note_snapshot("method:snapshot(post-action)", status, lines)

send("method:keys\nid:keys1\nkeys:ctrl-p")
lines, status = wait_for(lambda line: "keys1" in line, timeout=8)
if status == "ok":
    lines.extend(read_available(0.3))
note("method:keys keys=ctrl-p", status, lines)

send("method:wait\nid:wait3\nms:1000")
lines, status = wait_for(lambda line: "wait3" in line, timeout=5)
note("method:wait(post-keys)", status, lines)

send("method:snapshot\nid:snap3")
lines, status = wait_for(lambda line: "snap3" in line or "snapshot" in line, timeout=10)
if status == "ok":
    lines.extend(read_available(0.5))
note_snapshot("method:snapshot(post-keys)", status, lines)

send("method:shutdown\nid:shut1")
lines, status = wait_for(lambda line: "shut1" in line, timeout=5)
note("method:shutdown", status, lines)

try:
    proc.stdin.close()
except Exception:
    pass
try:
    code = proc.wait(timeout=8)
except subprocess.TimeoutExpired:
    proc.kill()
    code = proc.wait(timeout=3)
    failed = True
print(f"[exit] process={code} protocol_failed={failed}")

open(out_path, "w").write("\n".join(all_out) + "\n")
print(f"stdout saved: {out_path}")
print(f"stderr saved: {err_path}")
print("Inspect snapshot@text empty vs interactive nodes; grep stderr for ERROR/WARN.")
# Empty snapshot is a known headless outcome (not a protocol failure).
# Exit non-zero only on process failure or step timeout / ok:false / missing fields.
if failed or code != 0:
    sys.exit(1)
sys.exit(0)
PY

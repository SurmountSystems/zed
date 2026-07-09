#!/usr/bin/env bash
# Mandatory preflight gate for agent-stdio dogfood / release dogfood.
# Spawns headless Zed, asserts first stdout event is ready, exits non-zero on
# panic, timeout, or missing ready. Safe UDD under /tmp/* only (same rules as
# golden-session.sh).
#
# Usage (repo root; needs release binary):
#   .agents/skills/zed-dogfood/scripts/preflight.sh
#
# Overrides:
#   ZED_BIN=target/release/zed
#   UDD=/tmp/zed-preflight
#   OUT=/tmp/zed-preflight.out
#   ERR=/tmp/zed-preflight.log
#   TIMEOUT_SECS=15
#   ALLOW_UDD_RM=1   # only if UDD is outside /tmp/*
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
ZED_BIN="${ZED_BIN:-$ROOT/target/release/zed}"
UDD="${UDD:-/tmp/zed-preflight}"
OUT="${OUT:-/tmp/zed-preflight.out}"
ERR="${ERR:-/tmp/zed-preflight.log}"
TIMEOUT_SECS="${TIMEOUT_SECS:-15}"

if [[ ! -x "$ZED_BIN" ]]; then
  echo "preflight FAIL: missing binary: $ZED_BIN (build with: cargo build --release -p zed)" >&2
  exit 1
fi

if ! [[ "$TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]]; then
  echo "preflight FAIL: TIMEOUT_SECS must be a positive integer (got '$TIMEOUT_SECS')" >&2
  exit 1
fi

# Refuse recursive delete outside a subdirectory of /tmp unless ALLOW_UDD_RM=1.
# realpath -m resolves without creating parents (blocks /tmp/../home/… prefix tricks).
# Never allow exact /tmp or /tmp/ — that would wipe the system temp directory.
if ! UDD_CANON="$(realpath -m -- "$UDD" 2>/dev/null)"; then
  echo "preflight FAIL: refusing UDD='$UDD' (could not resolve canonical path)" >&2
  exit 1
fi
if [[ "$UDD_CANON" == "/tmp" ]]; then
  echo "preflight FAIL: refusing rm -rf on UDD='$UDD' (canonical='/tmp'; refuse wiping /tmp itself)" >&2
  exit 1
fi
case "$UDD_CANON" in
  /tmp/*) ;;
  *)
    if [[ "${ALLOW_UDD_RM:-}" != "1" ]]; then
      echo "preflight FAIL: refusing rm -rf on UDD='$UDD' (canonical='$UDD_CANON'; must be a path under /tmp/, or set ALLOW_UDD_RM=1)" >&2
      exit 1
    fi
    ;;
esac
UDD="$UDD_CANON"

rm -rf "$UDD"
mkdir -p "$UDD"
: >"$OUT"
: >"$ERR"

export ZED_BIN UDD OUT ERR TIMEOUT_SECS

python3 - <<'PY'
import os, re, select, subprocess, sys, time

bin_path = os.environ["ZED_BIN"]
udd = os.environ["UDD"]
out_path = os.environ["OUT"]
err_path = os.environ["ERR"]
timeout_secs = float(os.environ["TIMEOUT_SECS"])

proc = subprocess.Popen(
    [bin_path, "--agent-stdio", "--user-data-dir", udd],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(err_path, "wb"),
    text=True,
    bufsize=1,
)
collected = []
exit_code = 1
# Ready gate only: success means first stdout event was ready. Panic needles below
# diagnose already-failed runs; they are not an independent fail criterion.


def read_available(timeout=0.25):
    lines = []
    end = time.time() + timeout
    while time.time() < end:
        readable, _, _ = select.select(
            [proc.stdout], [], [], max(0.0, end - time.time())
        )
        if not readable:
            break
        line = proc.stdout.readline()
        if not line:
            break
        lines.append(line.rstrip("\n"))
    return lines


def first_nonempty(lines):
    for line in lines:
        if line.strip():
            return line
    return None


def is_ready_event(line: str) -> bool:
    # Spaced TOON from emit_ready: "event: ready"; also accept unspaced "event:ready".
    return re.match(r"^event:\s*ready\b", line.strip()) is not None


def mark_ready_ok(*, died_after_ready=False, process_returncode=None):
    global exit_code
    first = first_nonempty(collected)
    print(f"preflight OK: {first}")
    for line in collected[:6]:
        if line.strip():
            print(f"  {line}")
    if died_after_ready:
        print(
            f"preflight WARN: process exited after ready (exit_code={process_returncode})",
            file=sys.stderr,
        )
    exit_code = 0


def shutdown_or_kill():
    """Best-effort graceful shutdown; kill fallback. Returns process returncode."""
    if proc.poll() is not None:
        return proc.returncode
    try:
        proc.stdin.write("method:shutdown\nid:preflight-shut\n\n")
        proc.stdin.flush()
    except Exception:
        pass
    try:
        return proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            proc.kill()
        except Exception:
            pass
        try:
            return proc.wait(timeout=3)
        except Exception:
            return proc.returncode


try:
    end = time.time() + timeout_secs
    while time.time() < end:
        if proc.poll() is not None:
            # Drain remaining stdout — ready may already be in the pipe.
            collected.extend(read_available(0.2))
            first = first_nonempty(collected)
            if first is not None and is_ready_event(first):
                mark_ready_ok(
                    died_after_ready=True,
                    process_returncode=proc.returncode,
                )
            else:
                print("preflight FAIL: process exited before ready", file=sys.stderr)
                print(f"  exit_code={proc.returncode}", file=sys.stderr)
                if first is not None:
                    print(f"  first_line={first!r}", file=sys.stderr)
            break

        chunk = read_available(0.25)
        collected.extend(chunk)
        first = first_nonempty(collected)
        if first is None:
            continue
        # Drain a bit more of the ready document (user_data_dir, pid lines).
        collected.extend(read_available(0.3))
        first = first_nonempty(collected)
        if is_ready_event(first):
            rc = shutdown_or_kill()
            mark_ready_ok()
            # Ready gate succeeded; non-zero exit after ready is a warning only.
            if rc not in (0, None):
                print(
                    f"preflight WARN: process exit after ready/shutdown was {rc}",
                    file=sys.stderr,
                )
            break
        print("preflight FAIL: first stdout event is not ready", file=sys.stderr)
        print(f"  first_line={first!r}", file=sys.stderr)
        break
    else:
        print(
            f"preflight FAIL: no ready event within {timeout_secs:g}s",
            file=sys.stderr,
        )
except Exception as exc:
    print(f"preflight FAIL: {exc}", file=sys.stderr)
    exit_code = 1
finally:
    if proc.poll() is None:
        try:
            proc.kill()
        except Exception:
            pass
        try:
            proc.wait(timeout=3)
        except Exception:
            pass
    open(out_path, "w").write("\n".join(collected) + ("\n" if collected else ""))
    if exit_code != 0:
        try:
            err_tail = open(err_path, "r", errors="replace").read()[-4000:]
        except Exception:
            err_tail = ""
        if err_tail.strip():
            print("--- stderr (tail) ---", file=sys.stderr)
            print(err_tail, file=sys.stderr)
        if collected:
            print("--- stdout captured ---", file=sys.stderr)
            print("\n".join(collected[:20]), file=sys.stderr)
        # Failure diagnostics only (not an independent pass/fail criterion).
        panic_hit = False
        try:
            err_blob = open(err_path, "r", errors="replace").read()
        except Exception:
            err_blob = ""
        for needle in (
            "panicked at",
            "panic:",
            "Action is already registered",
            "duplicate action",
        ):
            if needle.lower() in err_blob.lower():
                panic_hit = True
                print(
                    f"preflight diagnostic: panic/registration signal in stderr: {needle!r}",
                    file=sys.stderr,
                )
                break
        if not panic_hit and not collected:
            print(
                "preflight FAIL: no stdout and no panic fingerprint; inspect ERR log",
                file=sys.stderr,
            )
        print(f"stdout saved: {out_path}", file=sys.stderr)
        print(f"stderr saved: {err_path}", file=sys.stderr)

sys.exit(exit_code)
PY

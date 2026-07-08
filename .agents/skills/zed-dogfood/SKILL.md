# Zed Dogfood — Agent Stdio Subagent Protocol

Spawn a headless/offscreen Zed instance controlled over stdio using the TOON protocol (MCP-style).

## Spawn

```bash
# Build once
cargo build --release -p zed --features agent-stdio

# Run with isolated temp user data (recommended)
target/release/zed --agent-stdio

# Or via environment variable
ZED_AGENT_STDIO=1 target/release/zed

# Custom user data directory
target/release/zed --agent-stdio --user-data-dir /tmp/my-zed-agent
```

## I/O model

| Stream | Purpose |
|--------|---------|
| **stdin** | Blank-line-delimited TOON request documents |
| **stdout** | TOON responses and events **only** (never logs) |
| **stderr** | All Zed logs when agent-stdio is active |

## Startup

On ready, Zed writes a single TOON document to stdout:

```text
event:ready
user_data_dir:/tmp/...
pid:12345
```

## Request / response format

Each request is a TOON document; terminate it with a blank line (multi-line documents are fine). Each response is one line of TOON followed by a newline.

Successful responses include `ok:true` and echo the request `id` when provided.

Errors include `ok:false` and an `error` field.

## Methods (v1)

### `snapshot`

Capture the UI accessibility tree as compact text (interactive nodes only).

**Request:**
```text
method:snapshot
id:1
```

**Response:**
```text
ok:true
id:1
snapshot@text:[Button] "Open" #NodeId(42)
  [TextInput] value="fn main" #NodeId(99)
```

### `action`

Dispatch a registered GPUI action by name (same names as `zed --dump-all-actions`).

**Request:**
```text
method:action
id:2
name:workspace:ToggleLeftDock
```

With JSON payload:
```text
method:action
id:3
name:some:Action
data:{"key":"value"}
```

### `keys`

Simulate a keystroke on the active window (`Keystroke::parse` syntax).

**Request:**
```text
method:keys
id:4
keys:cmd-shift-p
```

### `open`

Open a file path or URL.

**Request:**
```text
method:open
id:5
path:/home/user/project/src/main.rs
```

```text
method:open
id:6
url:file:///home/user/project
```

### `actions`

List all registered action names (sorted, unique).

**Request:**
```text
method:actions
id:7
```

**Response:**
```text
ok:true
id:7
actions[3]:workspace:ToggleLeftDock,file_finder:Toggle,file_finder:Deploy
```

### `wait`

Pause for the given milliseconds (async; response arrives after the delay).

**Request:**
```text
method:wait
id:8
ms:500
```

### `shutdown`

Gracefully quit Zed.

**Request:**
```text
method:shutdown
id:9
```

## Example session

```bash
target/release/zed --agent-stdio 2>zed.log &
PID=$!

# Read ready event from stdout
read -r READY

# Open a project
echo 'method:open
id:1
path:/home/user/my-project' >&0

# Wait for window
echo 'method:wait
id:2
ms:2000' >&0

# Snapshot UI
echo 'method:snapshot
id:3' >&0

# Quit
echo 'method:shutdown
id:4' >&0
```

## Notes

- Uses `ZED_STATELESS=1` and skips the single-instance socket to avoid colliding with a running Zed.
- On Linux, boots with the headless GPUI platform (wgpu layout path) and enables `ZED_EXPERIMENTAL_A11Y=1` for snapshots.
- Encode/decode uses the `toon-format` 0.5 crate.
# lint-shell-gate — omp extension

> Bridges oh-my-pi's `bash` tool to the [serena-rust](https://github.com/oraios/serena-rust) `lint-shell` static linter, surfacing the agent's own command-construction mistakes back into its conversation.

## What it does

For every successful `bash` tool_result, this extension:

1. Reads the command string from the preceding `tool_call` event.
2. Spawns `serena-cli lint-shell --cmd <the command> --json` (5s timeout by default).
3. If the linter reports at least one `error`-level finding, appends a `[lint-shell] N error(s) found ...` reminder to the tool_result content array.
4. Otherwise stays completely quiet — `warning`/`info` findings do not inject.

The reminder rides in the tool_result content, so the model sees it in conversation history on its next turn, instead of as a transient TUI status line. **The extension never blocks execution — warn-only.**

It coexists with `cbm-bridge` and `time-window-router`; each extension owns a different event surface (`bash` tool_result for this one, code-read tool_result for cbm-bridge, `session_start` for time-window-router).

## Installation

> The OMP plugin loader does not auto-discover extensions dropped into `~/.omp/agent/extensions/`. You must manually copy this directory and restart your OMP session.

```bash
# Linux / macOS / Git Bash
cp -r /path/to/serena-rust/extensions/lint-shell-gate ~/.omp/agent/extensions/

# Restart OMP — the next session loads the extension on session_start.
# Verify by running a bash command that the linter flags as an error:
omp-cli
> run: cli unknown_tool foo
# Expect: tool_result content ends with `[lint-shell] 1 error(s) found ...`
```

```powershell
# Windows PowerShell
Copy-Item -Recurse `
    "D:\Project\serena-rust\extensions\lint-shell-gate" `
    "$env:USERPROFILE\.omp\agent\extensions\lint-shell-gate"

# Restart OMP session.
```

`installed_plugins.json` and `omp-plugins.lock.json` are not edited by hand. OMP discovers the extension by directory presence at session start. If you upgrade by editing files in place, a session restart is required.

### Uninstall

```bash
rm -rf ~/.omp/agent/extensions/lint-shell-gate
# Restart OMP session.
```

## Behaviour

| Bash command scenario | What happens |
|---|---|
| `cli unknown_tool foo` (error finding: `UNKNOWN_TOOL`) | Reminder appended; model sees it next turn |
| `python <<EOF ... rc, out = subprocess.run(...) EOF` (error finding: `PY_UNPACK` on line 3) | Reminder appended |
| `echo hello` (no error findings) | No reminder; tool_result passes through unchanged |
| `serena-cli ...` with only `warning`/`info` findings | No reminder (threshold default: `error`) |
| Bash command returns non-zero (`isError: true`) | No reminder (failure already in content) |
| `serena-cli` not on PATH | Fail-open: silent, log line in stderr (`~/.omp/logs/omp.*.log`) |
| Spawn hangs > 5s | Fail-open: silent, killed process |

## Configuration

All tunables live in `~/.omp/hooks/configs.yml` under `lint_shell_gate:`. Missing keys fall back to the defaults below.

```yaml
# ~/.omp/hooks/configs.yml
lint_shell_gate:
  # Tools whose tool_result should be gated. Default: ["bash"].
  # Anything else (read/grep/lsp/...) is unaffected — cbm-bridge owns that surface.
  gated_tools:
    - bash

  # CLI binary name (PATH lookup) or absolute path.
  # Default: "serena-cli" — install via `cargo install --path crates/cli`.
  cli: "serena-cli"

  # Per-spawn timeout (ms). Default: 5000.
  cli_timeout_ms: 5000

  # Minimum severity that triggers an injection.
  # Default: "error" — warnings and infos stay silent to avoid noise.
  # Set to "warning" to inject for warnings too, "info" for everything.
  severity_threshold: "error"
```

Edit at will; reload on next session_start. Parse failure (invalid YAML) makes the extension silently fall back to defaults — see `index.ts:loadConfig()`.

## Why a single-slot cache?

OMP fires `tool_call` then `tool_result` for a given tool call, but the wire format does not surface a stable call id on `tool_result` events (see `cbm-bridge/index.ts:170-180` for the same observation). Concurrent bash calls would race anyway, and OMP runs tool calls sequentially per turn, so a single-slot last-write-wins cache is the simplest correct design. The cache is cleared after each `tool_result` read so it cannot leak across turns.

## Files in this directory

```
lint-shell-gate/
├── index.ts        # Extension factory — main entry point
├── index.test.ts   # Pure-function tests (parseFindings, severity filter, renderReminder)
├── package.json    # Manifest (name / main / engines.omp)
└── README.md       # This file
```

## Running the tests

```bash
cd ~/.omp/agent/extensions/lint-shell-gate
bun test index.test.ts
```

The test suite covers JSON parsing, severity filtering, and reminder rendering. End-to-end verification (real OMP session → bash command → reminder appears in tool_result) is documented in `local/report-sm0.md` of the serena-rust repo.

## Diagnostic

The extension writes `[lint-shell-gate] loaded (pid=...)` to stderr on every OMP startup so a `~/.omp/logs/omp.*.log` search can confirm the extension is active.

If the CLI binary is missing or a spawn fails, a single line is written to stderr (visible in the session log) but never blocks the tool result. This is intentional — the extension is fail-open by design.
---
name: debug
description: "MUST read before debugging crashes, segfaults, wrong runtime state, or stepping through code (instead of running raw `gdb`, `lldb`, `dlv`, or `pdb`). Uses the `debug` command to drive structured Debug Adapter Protocol (DAP) sessions across Rust, C/C++, Go, Python, and container or loopback stubs."
requires-command: debug
---

# Structured DAP debugging (`debug`)

`debug` drives a live Debug Adapter Protocol session through May's host broker. `debug --help` prints the exact command and flag syntax.

## Choosing how to start

- **Native binaries (Rust, C, C++)**:
  - Launch a binary inside the session sandbox while the host broker runs `lldb-dap` (or `gdb --interpreter=dap`):
    `debug launch ./target/debug/app --stop-on-entry -- arg1 arg2`
  - Attach to an already-running background job by its real PID from `terminal`:
    `debug attach <pid>`
  - On macOS, `lldb-dap` is resolved from `PATH` or `xcrun -f lldb-dap` (Xcode Command Line Tools). On Linux, `lldb-dap` or `gdb --interpreter=dap` is used.
- **Go (`dlv`)**:
  - Launch or attach with `--adapter dlv` (runs `dlv dap` on the host while the target runs in the sandbox), or start `dlv dap --listen=127.0.0.1:<port>` and attach with `debug connect 127.0.0.1:<port>`.
- **Python (`debugpy`)**:
  - Launch under `debugpy` inside the sandbox (requires `debugpy` in the active Python environment — install with `uv pip install debugpy`):
    `debug launch path/to/script.py --stop-on-entry -- arg1`
  - Or start a Python target with an explicit loopback listener and connect:
    `python3 -m debugpy --listen 127.0.0.1:5678 --wait-for-client script.py` (in a background `terminal` job), then `debug connect 127.0.0.1:5678`.
- **Container sessions and loopback stubs**:
  - Start any DAP server listening on loopback (`127.0.0.1:<port>`) inside the host or container and connect with `debug connect 127.0.0.1:<port>`.
  - Native `ptrace` attach inside containers is disabled by default; opt in via `[debug] container_ptrace = true` in `~/.may/config.toml` when attaching to in-container native PIDs.
- **Unsupported protocols (Node CDP, JVM JDWP)**:
  - Node/Bun/Deno speak CDP and JVMs speak JDWP rather than DAP. Use a DAP bridge listening on loopback with `debug connect <host:port>`, or runtime-native tooling instead.

## Breakpoints, stepping, and inspection

1. Set breakpoints with `debug break <file> <line>` (optional `--condition`, `--hit-condition`, `--log-message`) or `debug break-function <name>`. Each breakpoint receives a monotonic client ID (`#1`, `#2`, ...); list them with `debug breaks` and remove one with `debug unbreak <id>`.
2. Resume with `debug continue`, or step with `debug next`, `debug step-in`, `debug step-out`, and `debug pause`.
3. **Never poll `debug status` or `debug wait` in a loop.** When `debug continue` returns while the target is still running, the host broker wakes the session automatically with a one-shot notification as soon as the target stops at a breakpoint, hits an exception, or exits.
4. When stopped, inspect call stacks with `debug stack` (0-based frame indices `#0`, `#1`, ...), variables with `debug vars --frame <n>`, lazy one-level child expansion with `debug vars --expand <handle>`, expressions with `debug eval "<expr>" --frame <n>`, threads with `debug threads`, and captured target streams with `debug output --category stdout|stderr|console|all`.
5. Frame indices and variable handles belong to the current stop and are invalidated automatically as soon as execution resumes.

## Cleaning up

- Use `debug detach` to disconnect while leaving an attached background job running.
- Use `debug terminate` to stop both the debug session and its owning background job cleanly before finishing a turn.

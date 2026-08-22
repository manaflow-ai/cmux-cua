# cmux CUA

Background computer-use driver for any agents. Speaks MCP over stdio; drives native macOS apps without stealing focus.

**[Documentation](https://cua.ai/docs/cmux-cua)** - Installation, guides, and API reference.

## Repository Layout

| Path | Purpose |
| --- | --- |
| `rust/` | Cargo workspace for the driver daemon, platform crates, and Rust tests |
| `python/` | Python package wrapper and package tests |
| `tests/fixtures/` | Source-built GUI harness apps and shared fixtures |
| `rust/crates/cmux-cua/tests/` | Rust integration tests for the driver and GUI harnesses |
| `scripts/` | Install, uninstall, local build, and VM sync helpers |
| `docs/` | Small repo-local specs that are not part of the hosted docs site |

Start with `rust/README.md`, `rust/crates/cmux-cua/tests/README.md`, and
`tests/fixtures/README.md` when changing driver behavior or tests.

Contributor documentation:

- `docs/test-matrix.md` maps unit and canonical harness E2E suites.
- `docs/action-support.md` is the empirical platform behavior ledger.
- `docs/test-harnesses-guide.md` explains fixture and runner ownership.
- `docs/linux-desktop-validation.md` covers representative Linux sessions.
- `docs/linux-support-completion-plan.md` preserves the historical Linux plan.

## Claude Code computer-use compatibility

Standard Claude Code MCP registration:

```bash
claude mcp add --transport stdio cmux-cua -- cmux-cua mcp
```

If you want Claude Code's vision/computer-use-style flow to ground on CmuxCua window screenshots, register the compatibility mode:

```bash
claude mcp add --transport stdio cua-computer-use -- cmux-cua mcp --claude-code-computer-use-compat
```

This keeps CmuxCua's normal MCP tools and changes only `screenshot`, which requires `pid` and `window_id` and captures that window only.

Use MCP for this Claude Code vision/computer-use-style path. CLI screenshots still work as CmuxCua calls, but they do not expose the `mcp__cua-computer-use__screenshot` tool name that Claude Code appears to use as the image-grounding cue.

## Codex Computer Use compatibility on macOS

Run the driver with the app-oriented Codex Computer Use contract:

```bash
cmux-cua mcp --codex-computer-use-compat
```

This opt-in mode exposes the ten Codex v829 tools, requires a fresh
`get_app_state` snapshot before actions, and returns text plus a logical-point
JPEG after state reads and successful actions. It uses the Sky cursor by
default, while explicit `--cursor-shape` or `--cursor-icon` values still win.
The native tool catalog is unchanged when the flag is absent.

The driver blocks terminal-class apps, System Settings, authentication UI,
and its embedding host. Before `get_app_state` can launch, inspect, or capture
another app, the daemon requires an authenticated MCP session and asks the
client for approval with `elicitation/create`. A plain accept lasts for the MCP
session. Choosing permanent approval stores the app's bundle ID plus canonical
app path in a private local file. The daemon accepts compatibility sessions only
from the matching live CmuxCua code launched by signed OpenAI Codex, and every
tool call must prove ownership of that broker session. Raw and in-process calls
cannot bypass this check.

Manage permanent approvals without adding MCP tools:

```bash
cmux-cua approvals list
cmux-cua approvals revoke com.example.Editor
cmux-cua approvals clear
```

Add `--json` to any approvals command for machine-readable output.

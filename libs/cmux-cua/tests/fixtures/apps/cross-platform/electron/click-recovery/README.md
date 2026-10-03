# Electron click recovery fixture

Run on a disposable macOS GUI session with Accessibility and Screen Recording
permission. This test deliberately changes the frontmost app. It uses a real
Electron BrowserWindow with 20 buttons. By default a button ignores clicks
while the document is unfocused, reproducing an acknowledged AX press with no
application effect. It is a controlled recovery case, not a claim that every
Electron app rejects background clicks.

Install the fixture dependencies:

```sh
npm install
```

From this directory, run a bounded probe against an unbundled development driver.
It starts the fixture, measures 20 clicks, stops its fixture and driver process
groups, and exits within five minutes. Take the host lock around each invocation
and release it between invocations. Never hold a capture host while coding,
surveying, or waiting between probes.
`in-process` uses one persistent MCP process, retaining its element cache and
the permission attribution of the launching terminal:

```sh
python3 bounded_probe.py /path/to/cmux-cua background
python3 bounded_probe.py /path/to/cmux-cua foreground
python3 bounded_probe.py /path/to/cmux-cua fallback
python3 bounded_probe.py /path/to/cmux-cua cache
```

`probe.py` is the lower-level measurement helper. An isolated approved daemon
socket may be passed to it instead of `in-process`.
Do not point the probe at a user session. Each run switches to Finder before each click, performs 20 pixel clicks, and
checks an independent IPC counter written by the Electron main process. It
saves the driver response for every attempt, a grounding screenshot, and a
success total. A missing or duplicate counter increment is a failed attempt.
Use one Electron fixture process at a time. Run the unchanged base first, then
the changed driver with an identical fresh fixture. The foreground run is the positive
control. `CUA_CLICK_REQUIRE_FOCUS=0 python3 bounded_probe.py /path/to/cmux-cua background` provides an ordinary-control
comparison that accepts background clicks.

The expected focus-dependent results are 0/20 background and 20/20 foreground;
with foreground fallback enabled, the changed driver should reach 20/20 and
report the retry. Driver results alone do not determine the success count.

The `cache` probe changes a text value, adds/removes a child, and moves its
container through the fixture's file-driven IPC hook. It checks notification
refreshes, stable IDs and explicit diffs, role/label/region filters, a clean
snapshot with zero AX reads, and screenshot-only invalidation of action indices.
It uses the same bounded fixture lifecycle as the click probes.

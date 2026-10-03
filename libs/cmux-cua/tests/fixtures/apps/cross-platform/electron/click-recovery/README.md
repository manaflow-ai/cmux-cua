# Electron click recovery fixture

Run on a disposable macOS GUI session with Accessibility and Screen Recording
permission. This test deliberately changes the frontmost app. It uses a real
Electron BrowserWindow with 20 buttons. By default a button ignores clicks
while the document is unfocused, reproducing an acknowledged AX press with no
application effect. It is a controlled recovery case, not a claim that every
Electron app rejects background clicks.

Install and start the fixture:

```sh
npm install
npm start
```

From this directory, run the probe against an unbundled development driver.
`in-process` uses one persistent MCP process, retaining its element cache and
the permission attribution of the launching terminal:

```sh
python3 probe.py /path/to/cmux-cua in-process background
python3 probe.py /path/to/cmux-cua in-process foreground
python3 probe.py /path/to/cmux-cua in-process fallback
```

An isolated approved daemon socket may be passed instead of `in-process`.
Do not point the probe at a user session. Each run switches to Finder before each click, performs 20 pixel clicks, and
checks an independent IPC counter written by the Electron main process. It
saves the driver response for every attempt, a grounding screenshot, and a
success total. A missing or duplicate counter increment is a failed attempt.
Use one Electron fixture process at a time. Run the unchanged base first, then
the changed driver with the same fixture. The foreground run is the positive
control. `CUA_CLICK_REQUIRE_FOCUS=0 npm start` provides an ordinary-control
comparison that accepts background clicks.

The expected focus-dependent results are 0/20 background and 20/20 foreground;
with foreground fallback enabled, the changed driver should reach 20/20 and
report the retry. Driver results alone do not determine the success count.

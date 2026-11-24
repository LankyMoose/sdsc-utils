# Session tracing

`app.log` is the human-facing warning/error stream. Session tracing is the
structured record used to explain and replay user-visible behavior.

## Layout

One directory per process run:

```text
%APPDATA%\sdsc-utils\trace\session-<session-id>\
  manifest.json       # version, commit, profile, OS, env
  manifest.jsonl      # prefs/games/known-population + replay lifecycle
  actions.jsonl       # semantic user actions and their outcomes
  input.jsonl         # pad edges, actions, gate, owner routing
  app.jsonl           # top-level message dispatch
  home.jsonl
  launcher.jsonl
  browser.jsonl
  engine.jsonl        # CEF commands, address/load/history capability
  device.jsonl
  games.jsonl
  persist.jsonl
  platform.jsonl
```

Every record is one JSON object per line. Files rotate at 8 MB to
`<lane>.1.jsonl`, `<lane>.2.jsonl`, ....

## Levels

| `SDSC_TRACE` | Behavior |
|---|---|
| `off` | No session directory. |
| `actions` | `actions.jsonl` + manifest only. |
| `full` | Actions plus every lane's detail events. |

Debug builds default to `full`; release defaults to `actions`.

## Records

Common fields:

- `v` — schema version (currently 1)
- `ts` — wall-clock epoch ms
- `up_ms` — milliseconds since trace init
- `seq` — global monotonic sequence (sort key when merging lanes)
- `lane` — owning subsystem
- `flow` — causal chain id (one user intent, all consequences)
- `parent` — optional parent flow for a child flow
- `app`, `instance`, `component` — optional app/scope identity

Semantic action:

```json
{"v":1,"kind":"action","action_id":17,"flow":42,"lane":"browser",
 "action":"browser.history.pick","app":"browser","source":"pad",
 "params":{"requested_index":0,"requested_url":"https://example.com/"},
 "pre":{"ui":{"nav":{...}},"engine":{"history":{...}}},
 "requires":[{"path":"ui.nav.current","op":"eq","value":2}],
 "repro":{"app":"browser","action":"history.pick",
          "params":{"index":0,"url":"https://example.com/"}}}
```

Outcome:

```json
{"v":1,"kind":"outcome","action_id":17,"flow":42,"lane":"browser",
 "action":"browser.history.pick","outcome":"url_changed","duration_ms":34,
 "post":{"ui":{...},"engine":{...}},
 "checks":[{"name":"requested_url","pass":true},
           {"name":"ui_engine_agree","pass":false}],
 "resolution":{"matched_by":"url","actual_index":0}}
```

`pre`/`post` are bounded forensics snapshots. `requires` is the stable subset
replay validates. `repro` is the semantic command a replay driver consumes.

## Flows

`trace::action` allocates or inherits a flow. Nested actions share the flow;
`.parent(flow)` creates a child flow. Async work carries the flow explicitly
(engine commands use `EngineHandle::send_traced`, CEF stores it on the tab and
joins address/load log lines).

A flow that never settles is closed as `unfinished` when its guard drops.
`--trace-audit` flags action/outcome mismatches.

## CLI

```bash
sdsc-utils --trace-list
sdsc-utils --trace-last
sdsc-utils --trace-actions [--app browser] [--flow 42]
sdsc-utils --trace-follow 42
sdsc-utils --trace-find "example.com"
sdsc-utils --trace-failures
sdsc-utils --trace-audit
sdsc-utils --trace-repro 42
sdsc-utils --trace-path
sdsc-utils --trace-session <id> ...
```

`--trace-repro` prints the replay plan for a flow and validates the recorded
`requires` against the recorded `pre`.

Replay a flow in an isolated sandbox:

```bash
sdsc-utils --trace-replay <session-id-or-dir> --flow 42
```

The replay driver currently supports browser `navigate`, `history.back`,
`history.forward`, `history.open`, `history.pick`, and `history.cancel`.
Targets resolve by URL first, then index (`pass_resolved` semantics). State
mismatches are logged as `replay.mismatch` and replay continues best-effort;
the trace records both the mismatch and the final landing.

## Adding instrumentation

Semantic action:

```rust
let trace = trace::action(Lane::Browser, "browser.history.pick")
    .app("browser")
    .source("pad")
    .field("requested_index", index)
    .field("requested_url", url.as_deref())
    .pre(trace_browser_state(&self.browser.panel))
    .require("ui.nav.current", current)
    .repro(json!({
        "app": "browser",
        "action": "history.pick",
        "params": {"index": index, "url": url},
    }))
    .begin();
// ...perform the action, then:
trace.finish("sent").post(trace_browser_state(&self.browser.panel)).emit();
```

For async outcomes, call `.detach()` and settle when the real result lands.
Do not label a queued command as the outcome.

Detail event:

```rust
trace::event(Lane::Browser, "history.entry")
    .field("index", index)
    .field("url", url)
    .emit();
```

or, for hot paths:

```rust
trace_event!(Lane::Input, "pad.gate", owner = owner, edge_seq = edge.seq);
```

Rules:

- `actions.jsonl` is only for user-visible intents and their outcomes.
- Lane detail goes in the owning lane; never dump whole app state.
- Snapshots are bounded and semantic (no DOM, frames, or raw HID reports).
- New user-facing `Message` variants must be classified in
  `Message::trace_action`; the exhaustive match is intentional.
- Do not add new `diag_info` calls. Migrate high-value diagnostics to a lane;
  `app.log` stays warnings/errors/panics.
- Prefer stable identities in `repro` (`app id`, URL, title) over row/tab
  indices.

## Current gaps

- CEF does not emit canonical `HistoryChanged` yet; the trace deliberately
  records that absence (`engine.history: null`) instead of hiding it.
- Home/Launcher/app actions are top-level `app.input` records until their
  surface-specific semantic actions are instrumented.
- Replay currently covers browser navigation/history. Add adapters per app;
  do not grow the trace core per app.

# Debugging the desktop UI

The desktop app is immediate-mode egui rendered by wgpu. There is no webview and
no inspector, but the whole widget tree is exposed through **AccessKit**, and
`egui_kittest` can drive it: find a widget by its label, click it, type into it,
press keys, step frames, and render a PNG — with no display server.

That is the way to reproduce an interaction bug. Instead of "the play button got
stuck" you get a scripted test that fails at the frame where it happens.

## Running the scenarios

```bash
cargo test -p reel-desktop --test ui_scenarios          # the scripted flows
cargo test -p reel-desktop --test ui                    # the rendering snapshots
UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test ui_scenarios   # rewrite PNGs
```

A failing query prints the entire AccessKit tree, so a wrong selector shows you
what is actually on screen.

## An interactive control channel

For poking at the UI without writing a test first, there is a small socket
server around the same harness: `examples/ui_debug.rs`.

```bash
# start it (background); it keeps running
cargo run -p reel-desktop --example ui_debug -- serve /tmp/opencode/reel-ui.sock /tmp/opencode/reel-ui

# then, from another shell
cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/reel-ui.sock "dump"
cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/reel-ui.sock "click Play"
cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/reel-ui.sock "state"
cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/reel-ui.sock "snapshot home"
```

Commands: `state` (a text summary of the library and every torrent's selection,
pause and download flags), `dump` (every labelled node on screen),
`navigate <library|add|search|settings|detail <id>>`, `click <label>`,
`click_contains <substr>`, `click_role <role> <n>`,
`type_role <role> <n> <text>`, `key <name>`, `step [n]`, `snapshot <name>`,
`reset <sample|season|paused|duplicates|two_seasons>`, `quit`.

`click` prefers an actionable node when several share a label, so a page title
"Search" and the "Search" button do not confuse it. `snapshot` renders the frame
to `<dir>/<name>.png`, which can be opened directly.

This is how the season-pack bugs were found: `reset paused` then `navigate detail
9` shows every episode in the fetch plan but `state` reports `paused=true`, and
`click Play` narrows the selection to one file.

## Writing a scenario

`crates/reel-desktop/tests/ui_scenarios.rs` is the place. The shape is:

```rust
let mut harness = harness(items);        // App over a FakeBackend
harness.run_steps(3);                    // settle the layout
harness.state_mut().navigate(Screen::Detail(9));

harness.get_by_label("Play").click();    // real input
harness.run_steps(4);

assert!(!harness.state().item(9).unwrap().torrent.stats.is_paused());
```

Useful queries (from `kittest::Queryable`):

| Call | Finds |
| --- | --- |
| `get_by_label("Play")` | one node with exactly that AccessKit label |
| `get_all_by_label_contains("Download")` | every node whose label contains it |
| `get_all_by_role(Role::Button)` | by role, in tree order |
| `query_all_by(|node| …)` | anything, by predicate |
| `get_by_role(Role::MultilineTextInput)` | a multiline text field |

Useful input:

| Call | Does |
| --- | --- |
| `node.click()` | a real pointer click |
| `node.focus()` + `node.type_text("…")` | type into a field |
| `harness.key_press(egui::Key::Enter)` | a key press |
| `harness.event(egui::Event::…)` | any raw event |
| `harness.input_mut().…` | raw input for a frame |

Rendering: `harness.snapshot("name")` writes
`tests/snapshots/name.png`, which you can open to see the frame.

## Keeping the app testable

* **AccessKit sees labelled widgets.** A bare `Checkbox::without_text` has no
  label and cannot be found by the query helpers. Prefer a labelled control, or
  drive the flow through the app's own methods (`App::select_files`) and assert
  the resulting state.
* **Expose a little state.** `App::item`, `App::work`, `App::works`,
  `App::screen` and `App::library_len` are public so a test can assert the
  engine view the UI produced.
* **Fixtures live in `src/testing.rs`.** Add one that looks the way the engine
  leaves a torrent — `sample_paused_season_pack` exists precisely because the
  paused-on-add state was misread as "everything is downloading".

## Which layer to test where

| What | Where |
| --- | --- |
| Layout, labels, clicks, pauses, toggles | `tests/ui_scenarios.rs` (fake backend) |
| Byte-exact rendering | `tests/ui.rs` snapshots |
| Real engine: add, stream, keep, release, re-add | `tests/backend.rs` (`EngineBackend`) |
| Decode to a texture, ranges, magnet, packaging | `scripts/e2e.sh` |

The fake backend is deliberate: it never opens a video device, so UI tests stay
fast and headless. Anything that needs the engine's real state machine belongs in
`tests/backend.rs`, which starts the real session and streaming server on a
localhost port.

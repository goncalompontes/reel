# UI/UX rework plan

Status: **proposed** (Phase 1 starting on branch `feature/ui-rework`).

This document is the contract for the desktop UI rework: what the current UI
does, why it feels unplanned, and the structure, views, components and phases
that replace it. The backend seams (`Backend`, the crates) do not change — this
is a `reel-desktop`/`ui.rs` rework.

## 1. Where we are today

- `Screen` is a flat enum: `Library | Detail(id) | Player | Add | Search |
  Settings` (`ui.rs:27`). No history, no notion of "where you are".
- The shell is one top bar (`ui.rs:1139`) with four peer tabs (Library / Search
  / Add / Settings), the wordmark, and a rate/peers readout. Feedback is a
  single bottom toast plus a centered remove `Window`.
- **Library**: a "LATEST" hero (`ui.rs:1755`) then shelves (Continue watching /
  Recently added / Not started / Watched) of poster cards (`ui.rs:1439`); a
  filter switches to a flat grid.
- **Detail**: one long scroll — backdrop + overlapping poster (placed with a
  negative-space hack, `ui.rs:1933`), actions, overview, stats, then a film
  `Versions` list (`ui.rs:2167`) or a series episode list (`ui.rs:2320`) with
  season sections, then `Sources` (`ui.rs:2585`), extras and a storage line.
- **Add**: a whole tab for one action (`ui.rs:2956`), duplicating the library
  in "Already in your library".
- **Search**: a filter row plus result cards that expand: work Show/Hide,
  category Show/Hide, season tabs, Complete/Episodes tabs, a Quality filter, and
  a grouped/flat toggle (`ui.rs:3414-3815`).
- **Settings**: one ~450-line scroll with Save at the bottom (`ui.rs:4215`).
- **Player**: bare canvas + control panel (`ui.rs:4705`, `player.rs:442`).

### Why it feels unplanned

1. **Peer tabs across abstraction levels.** Library/Search are places, Add is a
   command, Settings is configuration. No Home, no hierarchy.
2. **No navigation stack.** Detail hardcodes "← Library" (`ui.rs:1898`), so a
   title opened from Search loses its context.
3. **Disclosure overload.** Show/Hide + categories + season tabs + type tabs +
   quality filter + grouped/flat: six independent bits of state per result set.
4. **Detail is an undifferentiated wall** with no sub-navigation for large
   series.
5. **Two competing search concepts** (top-bar library filter vs Search tab).
6. **Add is a destination**, not an action.
7. **Settings mixes routine toggles with destructive actions** and hides Save.
8. **Terminology drift**: copies / versions / sources / torrents / releases.
9. **No component layer**; `ui.rs` is ~5,100 lines of one-off egui calls with
   layout hacks and emoji icons.
10. **Feedback is toast-only**; transfers surface only as text in the top bar.

### What is worth keeping

Dark palette and poster-first cards; stream-while-downloading; compact release
rows with quality chips; the grouped-by-title *concept*; the `Backend` seam.

## 2. Principles

- **One shell, nested views.** A persistent left rail for destinations; the
  content area is a navigation *stack* with breadcrumbs and back/forward.
  Depth is expressed by navigation, not by disclosure.
- **Push, don't expand.** Selecting a title opens its view; it does not unfold
  inside a list card.
- **One primary action per view**; destructive actions are demoted.
- **Reuse components, not bespoke widgets.** One vocabulary, one widget set.

## 3. Decisions

Confirmed by the owner:

- **Left rail** (not top tabs).
- **Add is a global modal**, reachable from the rail and from empty states.

Designer's calls (revisable):

- **Home** is Continue-watching-first; the static "LATEST" hero is dropped.
- **Discover** results are a scannable work list that **pushes a Work view**;
  no in-result expansion and no grouped/flat toggle.
- **Series** seasons/type/quality are **segmented controls inside the Work
  view**, not nested Season views; episode details open in a side panel.
- **Settings** is two-pane with **per-section autosave** and a dirty indicator;
  destructive actions live under Metadata/Advanced only.
- **Phase 1** is the shell first (rail + stack + breadcrumbs + global search +
  Add modal), mapping existing screens with no visual redesign yet.

## 4. Information architecture

### The shell

```
┌──────────┬────────────────────────────────────────────────────────────┐
│ reel     │  ‹ Back   Discover / The Bear (2022)        [ ⌕ search… ]  │
│          ├────────────────────────────────────────────────────────────┤
│ ▸ Home   │                                                            │
│   Discover                                                            │
│   Activity                                                            │
│          │                                                            │
│ ──────── │                        view content                        │
│   Settings                                                            │
│          │                                                            │
│ 12 MB/s ↓│                                                            │
└──────────┴────────────────────────────────────────────────────────────┘
```

- `egui::SidePanel` rail: **Home, Discover, Activity** (content) with
  **Settings** pinned at the bottom and a live transfers summary. Collapses to
  icons below ~1040px.
- A header band carries the breadcrumb + Back and the contextual search field.

### Navigation stack

- `Root` = `Home | Discover | Activity | Settings`.
- `Nav { root: Root, stack: Vec<View> }`; `View` starts as `Work(usize)` and
  later grows (`Collection`, `Season`, `Episode`).
- Back pops the stack; at an empty stack it stays on the root. Esc, mouse
  back-button and Alt+← all map to Back. Forward is a later addition.
- Switching roots from the rail resets that root's stack (per-root history is a
  later enhancement).

### Global search

- A single header field, focusable with ⌘/Ctrl-K.
- Scope follows the root: on **Home** it filters the library; on **Discover** it
  submits a sources query. On any other view, ⌘K switches focus to Discover's
  search. A future step merges both into one command palette.

### Add

- A global `+` (rail/header) opens a modal that wraps the existing add form
  (magnet / URL / infohash, media-only toggle, submit). `Screen::Add` and the
  "Already in your library" duplicate list are removed; failures surface in the
  modal and the toast.

## 5. View specs

### Home

```
Continue watching
[ poster ] [ poster ] [ poster ] …
Recently added
[ poster ] [ poster ] …
```

- Continue watching first, then Recently added / Not started / Watched.
- Shelf headers carry a count and (later) a "see all" that pushes a Collection
  view; a `⋯` opens sort/filter.
- The library search switches the shelves to an in-place results grid with a
  visible "filtering library" chip.
- Empty state offers Add (opens the modal).

### Discover

```
[ search sources…            ] [Search]
[ ext.to ]   Sort [Best ▾]   Filters ▾
────────────────────────────────────────────────────────────
[▭] The Bear (2022)   Series   ★ 8.6   Comedy, Drama
     42 releases · best 4K · 12 seeded             [▶ Play best]
[▭] The Bear (2015)   Series   ★ 7.2   …
```

- Work rows are scannable; selecting one pushes the Work view.
- Filters live in a popover; there is no grouped/flat toggle and no in-card
  expansion.

### Work (film or series)

```
[poster]  The Bear (2022)                         [ ▶ Play ]  [ ⤓ ]  [⋯]
Series · Comedy/Drama · ★8.6 · 3 seasons · 42 releases
Synopsis…

Seasons (1)(2)(3)    Show (All)(Complete)(Episodes)    Quality (All)(4K)(1080p)
──────────────────────────────────────────────────────────────────────────
S02E01 Tomorrow   1.2 GB · 1080p · 240↑ 6↓   [▶] [⤓] [⋯]
S02E02 Pizza      1.1 GB · 1080p · 198↑ 4↓   [▶] [⤓] [⋯]
```

- Two columns: identity/actions left, content right. Collapses to one column
  below ~900px.
- Segmented selectors replace accordions; selection persists per work.
- Films show one `Releases` list (best first); a `Sources` section exposes the
  raw per-torrent controls. Duplicate copies use a small popover picker.
- Absorbs today's Detail, the grouped-search expansion, and the sources list.

### Activity

- Active transfers with progress / rate / ETA and pause/resume, a
  completed/kept section, and global rate limits. Replaces the top-bar readout
  as the place to manage transfers.

### Settings

```
Settings
[ General ]        (form for the selected section; autosave + dirty dot)
[ Appearance ]
[ Metadata ]
[ Playback ]
[ Downloads ]
[ Network ]
[ Search sources ]
[ Advanced ]
[ About ]
```

- Two panes; destructive actions only under Metadata/Advanced.

### Player

- Keep the bare canvas; add an auto-hiding OSD (click-to-pause, hover scrub with
  time tooltip), an "Up next" card for series, keyboard help (`?`), and clearer
  buffering/error states. Keep the existing shortcut set.

## 6. Design system

- **Tokens**: the existing palette plus a type scale (display / title / section
  / body / caption) and a 4px spacing scale, radii and elevation.
- **Components**: `PrimaryButton`, `GhostButton`, `Chip`, `Card`, `Shelf`,
  `SectionHeader`, `Stat`, `Badge`, `ProgressBar`, `EmptyState`, `Skeleton`,
  `Toast`, `Modal`, `SegmentedControl`, `Popover`, `Breadcrumb`, `RailItem`.
- **Iconography**: one inline vector set (or bundled icon font) replacing emoji.
- **Vocabulary**: *Title* (film/series), *Release* (one torrent), *Season*,
  *Episode*; retire copy/version/source in user-facing text.
- **Motion**: short cross-fades on push/pop and hover lift; disabled under
  reduced motion.

## 7. States, accessibility, responsiveness

- Per-view loading / empty / error / offline / no-metadata states, with
  skeletons for shelves and results, instead of toast-only feedback.
- AccessKit labels on every interactive node; logical focus order; full
  keyboard navigation (arrows move through cards, Enter opens, Esc backs);
  visible focus rings; adequate hit targets.
- Rail collapses < ~1040px; Work collapses to one column < ~900px; the app min
  size is 940×620.

## 8. Technical plan

- UI-only refactor; `Backend` and the crates stay put.
- Replace `Screen` with `Nav` (root + stack) and update the public helpers,
  tests, `examples/ui_debug.rs`, and `lib.rs` exports. Keep small accessors
  (`root()`, `view()`) so the headless tests can assert location.
- Route the player as an overlay that returns to its origin (generalize the
  existing `player_origin`).
- Split `ui.rs` into `views/{home,discover,work,activity,settings,player}.rs`,
  `components/`, `nav.rs`, `design.rs`. This structural split is the main
  source of "feels planned".
- Remove the negative-space header hack; allocate the Work header properly.
- Keep the `egui_kittest` harness; update snapshots per phase and add navigation
  tests.

## 9. Phases

1. **Shell** (started): rail + nav stack + breadcrumbs + Back + global search +
   Add modal; existing screens hosted unchanged; `Screen` removed.
2. **Discover → Work**: replace expanding result cards with the work list and a
   pushed Work view; delete the disclosure state.
3. **Work view**: two-column layout, segmented season/type/quality selectors,
   unified `Releases`, popover copy picker.
4. **Settings two-pane** with autosave.
5. **Activity** view.
6. **Design-system pass**: components, icons, type/spacing, motion, a11y.
7. **Player OSD** polish.

Each phase is independently shippable and leaves the app working.

## 10. Testing and verification

- `cargo test -p reel-desktop` (lib + `ui` snapshots + `ui_scenarios` + `e2e` +
  `backend`), `cargo clippy --all-targets`, `cargo fmt`.
- Update snapshot tests when the shell changes; add navigation tests: push/pop,
  breadcrumb contents, opening a Work from Home and from Discover, and the Add
  modal open/close.
- The `examples/ui_debug.rs` control channel keeps working with the new
  `navigate` mapping so GUI debugging stays available.

## 11. Risks and open questions

- **Public API churn**: `Screen` is exported and used by tests/tools; replacing
  it touches ~25 call sites. Mitigation: do it in one commit and keep
  label-level helpers.
- **Snapshot churn**: the shell change rewrites every snapshot. Expected.
- **Scope**: phases 1–3 are the bulk of the perceived improvement; keep them
  moving and defer polish (6–7).
- Revisit later: forward navigation, per-root history, global command palette,
  Collection/Season views, "Up next".

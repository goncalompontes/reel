# Search view: grouped by title

Status: **implemented.** The decisions below were confirmed (resolution tiers
for films, season → one row per release for series, posters on the cards, and a
grouped/flat toggle, grouped by default).

## Goal

Today a search returns a flat list of torrents. Searching "the lighthouse" gives
one row per release, interleaved with unrelated fuzzy matches, and you cannot see
at a glance that twelve of them are the same film.

Instead, a search should answer *"what can I watch?"*: **one card per film or
series**, with the artwork and details, and under it **every torrent for that
title, grouped into the categories that make sense for its kind** — quality tiers
for a film, seasons (and episodes) for a series. ext.to is the model: its detail
page shows the title, its IMDb data, and a "Related Torrents" table of the other
releases of the same thing.

## What we already have

Per search hit (from the backend): `title`, `year`, `size_bytes`, `seeders`,
`leechers`, `popularity`, `source`, `magnet`, `torrent_url`, `published_at`,
`detail`, `detail_url`.

Per hit, parsed and cached (`release::analyse_name`): kind (film / series),
season, episode, and attributes — resolution, source, video codec, audio codec,
edition, release group, **languages**.

Per unique title, fetched in the background (`enrich_search`): canonical title,
year, kind, rating, votes, runtime, genres, overview, **poster and backdrop**.

That is everything the design below needs; no new backend data.

## Information architecture

Three levels:

1. **Work** — a film or a series. Hits are grouped by identity, exactly as the
   library already groups torrents into works: the metadata provider's id when a
   title matched, otherwise `normalized title | year | kind` with compatible
   years allowed. Reusing that rule keeps search and library consistent.
2. **Category** — depends on the work's kind:
   - **Film:** quality tier — `2160p`, `1440p`, `1080p`, `720p`, `SD`, `Unknown`.
   - **Series:** season — `Season 1`, `Season 2`, …, `Specials`, `Unknown`.
3. **Torrent** — one release: a row with its quality, size, swarm and actions.

## Layout

```
┌───────────────────────────────────────────────────────────────────────────┐
│  Search   [ the lighthouse                    ]  [Search]                  │
│  Filters:  year [____]  season [__]  episode [__]  min seeders [____]      │
│  Sort: [Best match v]      Group: [By title | Flat]                        │
│  Sources: ext.to · 50 results · ext.to failed: HTTP 502                    │
├───────────────────────────────────────────────────────────────────────────┤
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │ [poster]  The Lighthouse (2019)                        ★ 7.5/10      │  │
│  │ 1h 49m · Drama, Fantasy, Thriller · 6 torrents          [1080p] …    │  │
│  │ Two lighthouse keepers try to maintain their sanity while living…    │  │
│  │                                                          [ ▾ hide ]  │  │
│  │                                                                      │  │
│  │   2160p ───────────────────────────────────────────────────────────  │  │
│  │     Ultra HD Blu-ray · H.265 · 15.3 GiB · 128↑ 8↓ · 6y   [Add][Open] │  │
│  │                                                                      │  │
│  │   1080p ───────────────────────────────────────────────────────────  │  │
│  │     Blu-ray · H.265 · AAC · 5.0 GiB · 170↑ 55↓ · 6y     [Add][Open]  │  │
│  │     WEB-DL · H.264 · Dolby Digital · 3.8 GiB · 139↑ 30↓  [Add][Open] │  │
│  │     Russian · English · HDRip · 15.3 GiB · 128↑ 8↓      [Add][Open]  │  │
│  │                                                                      │  │
│  │   720p ────────────────────────────────────────────────────────────  │  │
│  │     …                                                                │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│                                                                            │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │ [poster]  SpongeBob SquarePants                        ★ 7.7/10      │  │
│  │ Series · Animation, Comedy, Family · 17 seasons · 4 torrents         │  │
│  │ Deep down in the Pacific Ocean in the subterranean city of Bikini…   │  │
│  │                                                          [ ▸ show ]  │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────────────────────────────┘
```

Expanded **series** card:

```
│  [poster]  SpongeBob SquarePants                        ★ 7.7/10        │
│  Series · 17 seasons · 4 torrents                        [ ▾ hide ]     │
│                                                                            │
│   Season 17 ─────────────────────────────────────────────────────────    │
│     S17E05 · 1080p · HDTV · H.264 · Dolby Digital · 384 MiB · 328↑ 22↓    │
│     S17E06 · 1080p · HDTV · H.264 · Dolby Digital · 410 MiB · 301↑ 19↓    │
│                                                                            │
│   Season 16 ─────────────────────────────────────────────────────────    │
│     Complete · 1080p · WEB-DL · H.265 · 6.2 GiB · 210↑ 40↓                │
```

## Behaviour

- **Grouping** is on by default; a `By title | Flat` switch keeps the current
  flat list for anyone who wants it (and for the snapshot tests).
- **Work card header**: poster, `Title (year)`, kind badge, rating, runtime,
  genres, a torrent count (and, for a series, a season count), and a one-line
  overview. Clicking the header (or the chevron) expands/collapses.
- **Category sections** are collapsible too. Only the first category of the first
  work is open initially, so a 50-result search does not become a wall.
- **Filters** (year / season / episode / min seeders) still apply to the hits
  *before* grouping — a season filter keeps only that season's rows, a year filter
  keeps matching works.
- **Sort** applies *within* each category: by seeders (default), size, or newest.
- **Row actions**: `Add` (resolves the magnet on demand, as now), `Open` (the
  source's page), `Copy` (magnet). Row content mirrors ext.to's columns — quality
  chips, size, `seed↑ leech↓`, age, backend name — so a row reads like one line
  of ext.to's table.
- **A work with no matched metadata** still gets a card: the parsed title and
  year, a placeholder poster, and the categories from the release names.
- **Empty / failure states** are unchanged ("Nothing matched", "every source
  failed", the per-source note).

## Ordering

- **Works** are ranked by a relevance score: how well the title matches the
  query (exact > contains the whole query > contains every query word), then
  whether metadata was fetched, then how many torrents the work has. So an exact
  hit stays on top, and among near-matches the real title with many releases
  wins over a one-release oddity. Backend order breaks ties.
- **Categories**: films by resolution descending (2160p first); series by season
  ascending.
- **Rows**: by the chosen sort (seeders / size / newest), applied within each
  category; changing the sort redraws immediately.

## Why this is cheap to build

- The grouping key reuses the library's identity rule, so there is one definition
  of "same film".
- Parsing per hit and metadata per title are already done and cached; grouping is
  a pure function over that, computed once per search (not per frame), like the
  existing `search_releases` / `search_metadata` caches.
- Rendering is the same egui widgets already in use (frames, collapsible headers,
  chips, buttons).

## Decisions to confirm

1. **Film categories**: resolution tiers (recommended), or by release source
   (BluRay / WEB-DL / HDTV …), or resolution then source?
2. **Series nesting**: season → rows (recommended, one row per release), or
   season → episode → releases?
3. **Posters** on the cards (recommended), or keep it compact and text-only?
4. Keep the **Flat** toggle (recommended), or group always?
5. Default expansion: **first work, first category** open (recommended), or all
   collapsed?

---

# Round 2: play, metadata, series groupings

Status: **implemented.** All recommended options were confirmed (season tabs,
Complete/Episodes tabs, a Quality filter row plus resolution headers, and a Play
on the work header).

## 1. Play straight from a search result

Today a search row can only Add, Open or Copy; playing means adding, waiting for
the library to show it, then opening the detail. Instead every result gets a
**Play** button that does the whole thing.

**Behaviour:** resolve the magnet if the row only has a page (ext.to), add the
torrent as stream-only, and open the player on its best file the moment it is
live. It reuses the machinery the library already has: the add path, the
`Ready` event, and the player's `pending_play`/resume handling — so the resume
position, sidecar subtitles and "kept in the session cache" behaviour all come
along for free.

**Where the buttons live:**
- a **Play** on each torrent row, next to Add / Open / Copy;
- a prominent **Play** in the work header, meaning "the best release of this
  film/series" (highest quality, then seeders) — one click from a search to
  watching.

For a series pack, "best file" is the engine's primary video; the episode list
in the detail view is where you pick a specific episode.

If the magnet cannot be resolved, a toast says so and the row stays.

## 2. Metadata that matches more titles

Many torrents still show no rating/overview. The lookup commits to **one**
catalogue — film or series — from the release-name parse, so a wrong parse means
a miss; and the acceptance threshold is deliberately strict.

**Plan:**
1. **Try both catalogues.** Search the film catalogue and the TV catalogue for
   the title and take the best-scoring candidate, instead of trusting the
   parsed kind. The parse still decides which is preferred (and, for a series,
   which season to fetch), but a film that parsed as a series — or the reverse —
   now matches.
2. **Retry with the raw name.** If the cleaned title yields nothing, ask again
   with the torrent's own name (`LookupQuery::from_release_name`), which keeps
   words the parser dropped.
3. **Tune the acceptance.** Accept a candidate whose normalised title matches
   exactly with a compatible year even when the generic score is just under
   `MIN_SCORE`; keep "no match rather than a wrong match" for everything else.
4. Same path for search results and library entries.

**How we'll know it worked:** count matched vs total for a handful of searches
(`the lighthouse`, `friends`, a few obscure ones) before and after, and watch
that the extra matches are right.

## 3. Series groupings: season, then type, then resolution

A series today is: season → a flat list mixing complete-season packs and single
episodes. The ask is to separate them and slice by resolution.

```
┌────────────────────────────────────────────────────────────────────────┐
│ [poster] Friends (1994)   Series   ★ 8.4/10                   [ Hide ] │
│ Comedy · 135 torrents · 10 seasons                                     │
│ Six young people from New York City, on their own and struggling to…   │
│                                                                        │
│ Seasons   [ 1 ]  2   3   4   5   6   7   8   9   10     ← season tabs   │
│ Show      ( Complete season )  ( Episodes )             ← type tabs     │
│ Quality   [ All ]  2160p   1080p   720p                 ← only present  │
│ ────────────────────────────────────────────────────────────────────── │
│  Complete season · 8 releases                                          │
│    2160p ───────────────────────────────────────────────────────────── │
│      Web · H.265 · Dolby Digital Plus · 84.6 GiB · 1↑ 0↓ · 1y · ext.to │
│                                                  [Play] [Add] [Open]   │
│    1080p ───────────────────────────────────────────────────────────── │
│      Blu-ray · H.265 · AAC · 6.1 GiB · 487↑ 466↓ · 7y · ext.to         │
│                                                  [Play] [Add] [Open]   │
│  Episodes · 15 releases                                                │
│    1080p ───────────────────────────────────────────────────────────── │
│      S01E01 · Blu-ray · H.265 · AAC · 1.2 GiB · 40↑ 3↓  [Play][Add]…   │
│      S01E02 · Blu-ray · H.265 · AAC · 1.2 GiB · 38↑ 2↓  [Play][Add]…   │
└────────────────────────────────────────────────────────────────────────┘
```

- **Seasons** are tabs (a dropdown instead once there are many).
- **Show** splits *Complete season* from *Episodes* — the two are never mixed,
  and a section only appears when it has releases.
- **Quality** narrows to one resolution (a tab row, `All` by default).
- Inside a section, releases are grouped under a **resolution header** and rows
  keep the compact ext.to-style line with `Play` / `Add` / `Open`.

Films are unchanged: still grouped by resolution tier.

### Decisions to confirm

1. **Seasons:** tabs (recommended), or collapsible sections as today?
2. **Type split:** `Complete season` / `Episodes` tabs (recommended), or two
   stacked sections with the resolution headers inside each?
3. **Resolution:** a filter row (`All | 2160p | 1080p | …`, recommended) plus
   resolution headers, or headers only?
4. **Work-header Play:** add it (recommended), or keep Play on rows only?

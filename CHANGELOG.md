# Changelog

All notable changes to BRAIN are tracked here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2] — 2026-07-23

### Fixed

- **Finished LLM sessions no longer leave `brain mcp` processes
  running.** Every Claude (or other MCP client) session starts its own
  BRAIN server process, which is supposed to exit when the session
  closes. On Windows that exit signal can get lost, and dead sessions
  accumulated dozens of background `brain.exe` processes over a day.
  Each server now watches its parent process directly and shuts down
  within seconds of the session ending. One-time cleanup of existing
  orphans: close your LLM clients and end the leftover `brain.exe`
  processes in Task Manager (or reboot).

## [0.3.1] — 2026-07-23

### Fixed

- **Only one BRAIN instance runs at a time.** Launching BRAIN from the
  Programs menu while it was already running in the tray started a
  second copy — two file watchers double-committing, two auto-sync
  schedulers, the index database opened twice, and a duplicate tray
  icon. A second launch now simply brings the existing window to the
  front.

## [0.3.0] — 2026-07-23

### Added — S11 vault sync (encrypted Git remote)

- **Sync the vault through a private Git remote (e.g. GitHub) with
  client-side encryption.** Committed blobs are XChaCha20-Poly1305
  ciphertext, filenames are keyed-HMAC tokens; the hosting service
  never sees page names or content. Settings → Git sync: enable
  encryption (one-time recovery key), save remote + GitHub PAT (both
  live only in the OS keychain), Sync now / opt-in auto-sync.
- **Everything non-regenerable travels:** wiki pages + full history,
  `01_raw` attachments (encrypted mirror `raw/<HMAC>` + encrypted
  token→path manifest), and the customisable `00_meta/AGENTS.md` /
  `CLAUDE.md` (encrypted mirror `meta/<HMAC>`). Deliberately local:
  vault marker, `.mcp.json` bearer token, the SQLite index (rebuilt),
  models (re-downloaded), caches/logs, graph layout.
- **Second-machine join:** enable encryption with an existing recovery
  key, connect the same remote, and the unrelated histories merge
  (union of pages; genuine same-page conflicts get plaintext conflict
  markers to resolve). "Show recovery key" re-displays the key from
  the OS keychain for the hand-over. A clone-from-remote path exists
  in onboarding for brand-new machines.
- **Auto-sync** pushes local changes seconds after the auto-commit
  (watcher nudge) and pulls remote changes on a 2-minute interval.
  Background conflicts and permanent errors surface as toasts.

### Security model (read before first push)

- **The convert re-roots git history.** Enabling encryption commits
  the encrypted snapshot as a fresh root; the plaintext past stays
  local on the `pre-encryption-backup` branch and can never be pushed.
- **No Git without encryption, both directions.** A network remote
  cannot be attached to a plaintext vault, and disabling encryption
  automatically disconnects the remote and deletes the stored PAT.
- **Wrong keys are refused, early.** Every merge validates the local
  master key against the remote's committed canary; the UI runs the
  same check when the remote/token is saved and blocks sync controls
  on mismatch. Two differently-keyed vaults can no longer be unioned
  into a mixed, half-unreadable repo.
- **Known metadata that an encrypted remote still reveals** (accepted,
  git-crypt-class model): commit timestamps (activity timing),
  per-page ciphertext sizes ≈ plaintext sizes, page/attachment counts,
  and the four type directories (`entities/…` — categories, by
  design). Commit messages are path-free on encrypted vaults.
- **What BRAIN's encryption does NOT cover:** the working tree on the
  local disk stays plaintext (that is the editing model). Protection
  of the physical medium (lost stick/laptop) is the user's volume
  encryption (BitLocker/VeraCrypt). Key custody: the recovery key
  exists only in the user's password manager and the OS keychain —
  losing both makes every pushed copy permanently unreadable.
- Attachment paths from the remote manifest are sanitised against
  directory traversal; files > 95 MB stay local (GitHub blob limit).

### Fixed

- The dev/test client "forgot" the vault location after every test
  run: mount lifecycle tests persisted TempDir paths into the real
  `settings.json`. Tests now run against isolated config files.
- Converting a vault whose plaintext-era history mirrored `.md`
  attachments could fail mid-convert; mirror paths are now skipped by
  the page-encryption pass and restaged from their sources.
- The auto-commit watcher no longer leaks the changed page's name into
  commit messages of encrypted vaults ("wiki: N change(s)" instead).

### Migration notes

- Existing plaintext vaults: Settings → Git sync → *Enable content
  encryption* → save the recovery key → create a **private, empty**
  GitHub repo → save remote + token → the first sync publishes a
  single encrypted root commit (no plaintext history).
- Second PC with a diverged copy of the same vault: *Enable
  encryption* → "Joining a vault that already exists?" → paste the
  SAME recovery key → save the same remote + a token → the automatic
  first sync merges both histories; resolve any conflict markers and
  sync again.

## [0.2.20] — 2026-06-20

### Fixed

- **MCP server survives a vault-disk stale handle (standby/resume,
  long uptime, USB unplug) without hanging or needing a Claude
  restart.** Bug report: after a resume the `brain mcp` subprocess's
  SQLite handle on drive `E:` went stale; all index tools
  (`brain_search`, `brain_query`, `brain_list_pages`,
  `brain_list_tags`) returned `SQLITE_IOERR`, and eventually even
  `brain_ping` timed out for 4 minutes. Two root causes, both fixed:
  - **`brain_ping` regression (introduced in 0.2.19).** The
    0.2.19 self-heal ran a pre-flight `is_vault` stat + `SELECT 1`
    liveness probe before *every* request, so ping paid a
    filesystem stat and a DB query — and a stale-handle hang on
    either wedged the single-threaded loop. The pre-flight probe is
    gone. `brain_ping`, `initialize`, `tools/list` and notifications
    are now answered before any vault/DB access, so ping always
    responds instantly even while the index DB is dead.
  - **No bound on a wedged syscall.** DB operations now run on a
    worker thread bounded by an 8 s timeout (`DbHandle::with_timeout`).
    A hung kernel read on a stale handle can no longer block the
    dispatch loop: the op returns `BRAIN_INDEX_TIMEOUT`, the handle
    is abandoned, and the next call reopens a fresh connection. On a
    fatal connection error (`SQLITE_IOERR` / `NOTADB` / `CANTOPEN`)
    the handle is dropped, reopened, and the op retried once —
    transparent self-heal across an unplug/replug. `busy_timeout`
    (5 s) is also set so GUI-writer lock contention no longer
    fast-fails with `SQLITE_BUSY`.
- **`brain_embedding_status.chunk_count_indexed` is never a silent
  `null`.** It now reports a number, or an explicit
  `{ "error": "…" }` object when the count can't be read — the bug
  report flagged `null` as indistinguishable from "index genuinely
  empty".
- **`model_dir` in `brain_embedding_status` no longer shows mixed
  path separators** (`E:/04_models\bge-m3`). Display paths are
  normalised to forward slashes. Cosmetic; real filesystem access
  always used native separators and was never affected.

### Changed

- **MCP DB access is now reactive, not pre-flight.** Every
  index-backed tool runs through a single `db_op` choke-point that
  owns the open / timeout / reopen-on-IOERR policy. `brain_search`
  runs the hybrid (FTS5 + vector) path through it and falls back
  explicitly to the filesystem brute-force walk on any DB failure;
  `brain_list_pages` falls back to the filesystem listing. The
  v0.2.19 `maybe_reopen_db` pre-flight + `DbHandle::is_alive` probe
  are removed.

### Migration notes

- No breaking changes, no schema/DB migration. Drop-in update.
- The fix lives in the `brain mcp` subprocess, so it only takes
  effect once Claude (or the MCP host) launches the new binary —
  **restart Claude / the MCP host once after upgrading.**

### Residual notes

- The `is_vault` stat, the `has_full_model` model-file stats, and the
  brute-force search filesystem walk still touch the disk
  synchronously. On a fast-failing drive (USB) that's sub-ms; on a
  truly hung network mount they could delay the *individual*
  vault-touching request (ping stays unaffected). Bounding those is a
  noted follow-up.
- One worker thread leaks per timeout event (it's blocked in the
  kernel and clears itself once the OS errors the syscall). Rare and
  self-clearing for a personal-scale local server.

## [0.2.19] — 2026-05-12

### Fixed

- **Browse view text is selectable again.** `ResizableSplit` had
  `select-none` on its outer container so the panel-splitter drag
  wouldn't snag mid-row text. That class was always-on, and it
  cascaded down into the Tier1 markdown body — the user couldn't
  highlight a found term to look it up elsewhere. `select-none` is
  now applied only while the splitter is actually being dragged.
- **MCP server recovers from a vault-disk unplug/replug without a
  Claude restart.** The `brain mcp` subprocess opened its SQLite
  handle once at startup and cached it for the whole process
  lifetime. When the vault USB stick was pulled, the cached
  `rusqlite::Connection`'s file descriptor died; replugging
  restored the path but not the fd, so every tool call returned an
  IO error until the user fully restarted Claude. The subprocess
  now self-heals: before each request it probes the cached
  connection (`SELECT 1`) and the vault marker, drops a stale
  handle when the disk is gone, and reopens cleanly once the disk
  is back. No signal from the GUI is required — the subprocess
  can't receive one, so it heals autonomously. (New
  `DbHandle::is_alive()` liveness probe + `maybe_reopen_db` in the
  MCP dispatch loop, six new tests covering open / drop / keep /
  unplug-then-replug.)

### Added

- **Right-click "Search in browser" + "Copy" on selected text.**
  When the user right-clicks on a non-empty selection inside the
  markdown body, an in-app menu appears with two items: "Search in
  browser" (opens the OS default browser to a Google query for the
  selection via the Tauri shell plugin) and "Copy" (writes the
  selection to the clipboard). Esc / outside-click dismiss. No
  menu is shown when the selection is empty, so the user still
  gets whatever native context-menu behaviour the webview offers
  for the unselected case.

### Changed

- **Graph label staffelung — hubs are always labeled.** Pascal's
  "zoom-out feels empty" feedback on the 0.2.18 policy: at deep
  zoom-out even the structural-anchor hubs were hidden, leaving
  the overview as a soup of colored dots without text references.
  The revised policy keeps hub labels (top-15 % by degree)
  visible at every zoom level and only staffel-ts the long-tail
  non-hub labels: they appear when the user zooms past 0.7 (was
  1.0 in 0.2.18). Small vaults (< 80 nodes) continue to show
  every label at every zoom, unchanged.

### Migration notes

- No breaking changes, no schema or DB migration. Drop-in update.
- The MCP self-heal is transparent — existing registrations need
  no change. The fix only takes effect once this build's
  `brain mcp` binary is the one Claude launches, so restart Claude
  (or its MCP host) once after upgrading.

## [0.2.18] — 2026-05-12

### Performance

- **Graph stays responsive on dense vaults.** Pascal's vault grew to
  362 nodes / 2105 edges and the Force-layout view became
  unbedienbar — labels stacked into illegible soup and pan/zoom
  dropped to single-digit FPS. Two interlocking changes mitigate
  both effects without touching the small-vault experience:
  - **Cytoscape on-interaction render flags**: `hideEdgesOnViewport`,
    `hideLabelsOnViewport`, and `textureOnViewport` are now all
    `true`. Edges and labels are dropped during pan / zoom and
    snapped back when the gesture stops; the camera moves a texture
    snapshot instead of triggering a vector redraw. Visual identity
    at rest is unchanged.
  - **Zoom-aware label staffelung** (new `src/lib/graphLabelVisibility.ts`):
    small vaults (< 80 nodes) keep every label visible at every zoom
    level. Larger vaults staffel by zoom + node degree — overview
    zoom shows colored dots only, mid-zoom keeps the top-15 % of
    nodes by degree labeled (the structural anchors), and zooming in
    past 1.0 brings every label back inside the viewport. Helper is
    a pure function with seven unit tests pinning the boundary
    semantics so a future refactor can't silently drop the
    small-vault bypass.

## [0.2.17] — 2026-05-12

### Added

- **Page-scoped lint in `brain_write_page` response.** The lint
  output returned by a write is now filtered to findings whose path
  matches the page that was just written. Pre-0.2.17 every write
  surfaced the entire vault's lint state, which during bulk-ingest
  drowned the agent in stale errors from earlier writes and burnt
  context budget. Global state stays accessible via
  `brain_lint_report`.
- **Structured success response for `brain_write_page`.** Replaces
  the plain `wrote {id}` string with `{wrote, previous_size_bytes,
  new_size_bytes, warnings}` so the agent can detect an accidental
  shrink (rich page overwritten with sparse content) in the same
  round-trip.
- **New MCP tool `brain_ping`.** Vault-independent liveness probe.
  Returns `{status, server, version, uptime_seconds}`. Use between
  bulk-ingest batches to detect a stuck server within seconds
  instead of waiting for the IPC timeout.
- **New MCP tool `brain_get_pages([ids])`.** Bulk-read variant of
  `brain_get_page`. One call returns `{pages: [{id, found, page?}]}`
  in request order; missing ids surface as `found: false` rather
  than aborting the batch. Cuts round-trips for refactor sweeps
  inspecting 5+ related pages.
- **New MCP tool `brain_write_batch([{id, content}])`.** Atomic
  multi-page write. Phase 1 validates + buffers all pages in
  memory; if any single page fails to parse, nothing is written.
  Phase 2 writes the batch to disk. Phase 3 lints once over the
  whole vault and returns the union-scoped findings. Eliminates
  the cascade of broken-link errors that the single-page form
  produced when multiple new pages referenced each other.
- **New MCP tool `brain_list_tags`.** Returns each distinct tag in
  the vault with its page count, sorted by count desc (alphabetic
  on ties). Closes the tag-discoverability gap: agents no longer
  have to guess tag names before writing a `brain_query tag:<foo>`
  filter.
- **New MCP tool `brain_embedding_status`.** Reports the active
  embedder (`bge-m3` vs the deterministic `hashed-fh-1024`
  fallback), model directory, vector dimension, and indexed chunk
  count. Lets an agent distinguish "search isn't finding it
  because the page genuinely doesn't match" from "search isn't
  finding it because the semantic pass is on the hashed fallback".
- **New MCP tool `brain_get_page_history(id, limit?)`.** Returns
  the Git commits that touched a single page, newest first, each
  `{sha, ts, message, files_changed}`. Pair with
  `brain_restore_page` for accident-recovery.
- **New MCP tool `brain_restore_page(id, sha)`.** Replaces the
  current page content with the version at the given Git sha and
  records a `revert: restored …` commit on top. Never destructive
  to history.
- **"Update vault templates" button in Settings → Danger.** Pulls
  the bundled `AGENTS.md` / `CLAUDE.md` into the vault's
  `00_meta/` from the current binary, with per-file delta in the
  success toast (e.g. `AGENTS.md: overwritten (10411 → 10750 B);
  CLAUDE.md: unchanged`). `.mcp.json` is deliberately not touched
  so the user's bearer token / external MCP servers / routing
  rules survive. Confirmation flow uses a new in-app
  `ConfirmDialog` modal (replaces `window.confirm()`, which some
  Tauri builds silently suppress).

### Changed

- **`unregistered-type` lint promoted from Warning to Error.** A
  page whose frontmatter type is not one of `entity`, `concept`,
  `source`, `topic` now blocks the auto-commit until corrected.
  The error message lists the four valid singular forms verbatim
  so an LLM agent can fix the page in one round-trip without an
  extra lookup. Schema integrity over commit availability — drift
  no longer accumulates silently.
- **`brain_write_page` no longer self-commits.** The watcher
  (5 s idle debounce) is now the single source of commits for
  MCP-driven writes. Pre-0.2.17 every `brain_write_page` emitted
  its own immediate commit plus the watcher's follow-up commit,
  producing duplicate entries in wiki history.
- **`brain_memory_system_prompt` rewritten.** Removed the obsolete
  `notes/<slug>` bucket suggestion that pre-0.2.17 trained agents
  to write outside the four registered subdirs (those writes then
  escaped the lint walker and accumulated as orphan files). The
  prompt now steers single-fact memos into a `## Notes` section
  on the relevant `entities/<person>` page. Three regression
  tests pin the four-bucket contract.
- **Hybrid-search snippet markers deduplicated and capped.** FTS5
  used to wrap every occurrence of every query token in `«…»`; on
  pages where the user's name appeared dozens of times this
  produced very noisy snippets. The post-processor now keeps
  markers only for the first occurrence of each distinct token
  and caps at 3 distinct tokens per snippet, preserving the "why
  did this page rank" signal without the repetition.
- **`brain_search` description rewritten** to spell out the hybrid
  pattern (FTS5 BM25 + sqlite-vec KNN over bge-m3, fused via
  RRF) and to point at `brain_embedding_status` as the
  diagnostic when semantic matching seems off.
- **Updated `00_meta/AGENTS.md` template** with the new tools, the
  rollback workflow, the table-cell pipe-collision warning, and
  explicit Error-severity guidance for plural-type drift.

### Fixed

- **Graph labels and edges shrink along with nodes when zooming
  in.** The v0.2.14 zoom-aware sizing only scaled node width/
  height; labels (`font-size`), label haloes
  (`text-outline-width`), edge widths and arrow heads stayed at
  base size and grew linearly with `cy.zoom()`. The handler now
  applies the same `1 / √zoom` factor to all four properties.
- **New `wikilink-pipe-in-table-cell` lint warning.** Catches the
  GFM table-cell collision where `[[id|alias]]` shares its `|`
  separator with the table syntax and breaks rendering. Pre-empts
  the issue at write time so the agent can switch to the
  un-aliased `[[id]]` form inside table cells.
- **Buttons no longer wrap their label.** Multi-word button labels
  ("Update vault templates", "Rebuild index", "Re-register MCP")
  now stay on a single line via `whitespace-nowrap` + `shrink-0`
  in the base Button styles. Visible on the screenshots Pascal
  shared from the Danger zone.
- **Empty orphan `02_wiki/notes/` directory cleaned up.** The
  combination of the obsolete `notes/<slug>` system-prompt
  suggestion and BRAIN's path-creating write logic produced an
  empty directory that nothing in the codebase removed. Fixed by
  patching the prompt; existing vaults can remove the dir
  manually (lint does not walk it, so nothing references it).

### Migration notes

- **`unregistered-type` is now an Error.** Vaults with pre-existing
  plural-form `type:` values (`entities`, `concepts`, …) will see
  their auto-commits blocked on first run until the drift is
  fixed. The error message tells the agent exactly what to do;
  alternatively run a brief MCP session: *"call
  `brain_lint_report`, for every `unregistered-type` error
  rewrite the page via `brain_write_page` with the singular form,
  repeat until clean"*.
- **Update vault templates** to pull the new `AGENTS.md` into
  existing vaults: Settings → Danger zone → "Update vault
  templates". Optional — the old `AGENTS.md` is still functional,
  just lacks the new tool documentation and rollback workflow.
- **No schema or DB migration.** Drop-in update.

## [0.2.16] — 2026-05-11

### Added

- **Lint warns about unregistered frontmatter `type:` values.** The
  set of canonical singular page-types is now hardcoded as
  `KNOWN_TYPES = ["entity", "concept", "source", "topic"]` in
  `wiki/lint.rs`. Pages whose `type:` falls outside that set produce
  a Warning of kind `unregistered-type` — never an Error — so reads,
  auto-commits, the indexer and the graph continue to see them
  unchanged. The warning carries the offending value so an agent
  can correct it in one round-trip (typical case: the directory
  name plural `type: entities` slipped in via an MCP write and
  needs to become `type: entity`). Introducing a new page-type
  stays a deliberate design decision via code change; for one-off
  artifacts that don't fit any category, the warning explicitly
  points at `01_raw/`.
- **New MCP tool `brain_lint_report`.** Returns the current
  `{errors, warnings}` from the same lint pass that drives the
  auto-commit watcher and the Tauri toast bridge. Closes the loop
  where external MCP clients (Claude Code, Ollama-driven hosts, …)
  could see the user's wiki *change* via `brain_get_page` /
  `brain_search` but had no path to inspect *its lint state*. The
  intended workflow when the user says "fix all lint issues" is:
  call `brain_lint_report` → iterate, fix each entry via
  `brain_write_page` → call again until both arrays are empty.

### Fixed

- **Graph labels and edges now shrink along with nodes when zooming
  in.** The v0.2.14 zoom-aware sizing only scaled `width`/`height`
  on nodes — labels (`font-size`), label haloes
  (`text-outline-width`), edge widths and arrow heads stayed at
  their base values and grew linearly with `cy.zoom()` (Cytoscape
  renders all of those in world coordinates by default). On a
  ~166-node / 966-edge vault inspected at moderate zoom this produced
  ~55 px labels that fully covered the canvas, fat ribbon-like
  edges, and dwarfed node circles. The handler now applies the
  same `1 / √zoom` to font-size, text-outline-width, edge width and
  arrow-scale, so the visual hierarchy stays balanced across the
  full zoom range.

### Migration notes (for existing 0.2.x users)

- **No breaking changes.** Existing vaults open and work unchanged
  on first launch. The new lint rule is a Warning, not an Error;
  pages with a non-canonical `type:` value (typical drift: the
  directory plural `entities` instead of the singular `entity`)
  stay fully readable, indexed and graph-visible, they just get
  flagged in the lint toast and in `brain_lint_report`. Auto-
  commit continues to run.
- **No schema or DB migration required.** Drop-in update.
- **Optional cleanup.** Vaults that accumulated drift can be
  cleaned by asking an MCP-connected agent: *"Call brain_lint_
  report. For every `unregistered-type` warning, read the page,
  change the frontmatter type to the singular form (`entities`
  → `entity` etc.), write it back via brain_write_page. Repeat
  until warnings is empty."* Same flow handles `non-canonical-
  wiki-link` warnings (brain_write_page auto-normalises markdown
  links on every save).

## [0.2.15] — 2026-05-11

### Fixed

- **Wiki history no longer overflows under the StatusBar.** The
  History route is a flex-column with a `<header>` plus a
  `<ResizableSplit>`. `ResizableSplit`'s root is `flex h-full
  w-full`, but `h-full` means "100 % of parent" — so it claimed
  the *entire* column height while the header sat on top of it,
  pushing the last 40-odd pixels of the commit list (and the
  Pick-a-commit panel) behind the StatusBar. The split is now
  marked `flex-1 min-h-0` at its call site, so it claims only the
  leftover row inside the flex column and the inner
  `overflow-y-auto` panes scroll cleanly inside their bounds.
- **Mini-map no longer paints the broken-image glyph on first
  open.** The cytoscape-navigator plugin injects an `<img
  alt="Graph navigator">` into the panel and registers a throttled
  thumbnail handler against `cy.onRender(...)`. Because nothing on
  the cy side renders between toggle-on and the user's first
  pan/zoom, the handler never fired and the `<img>` stayed
  `src`-less — the browser then drew its broken-image icon plus
  the alt text in the corner of the panel for several seconds
  until the user interacted. `attachNavigatorIfWanted` now
  schedules a one-frame-deferred `cy.resize()` + `cy.forceRender()`
  immediately after the plugin attaches, so the first thumbnail is
  generated synchronously with the panel becoming visible. A CSS
  fallback (`[&_img:not([src])]:opacity-0` on the panel container)
  hides the empty `<img>` during the single rAF frame, so even on
  a slow first paint the user never sees the broken glyph.

## [0.2.14] — 2026-05-06

### Changed

- Graph layout breathes properly on dense vaults. Three tunings
  reinforce each other:
  - **fcose spread bumped**: `nodeRepulsion` 12000 → 28000,
    `idealEdgeLength` 70 → 120, `nodeSeparation` 80 → 140. On
    vaults of 50–500 nodes this stops hub-of-hubs from
    overlapping its neighbours and lets labels read at default
    zoom.
  - **Smaller base node sizes**: floor 22 px → 14 px, slope
    ×2 → ×1.5 per connection, cap 56 px → 38 px. Hubs stay
    visually distinguishable without dominating the canvas.
  - **Zoom-aware shrink**: a new `cy.on("zoom", …)` listener
    rescales node widths by `1 / √zoom`. Zooming in to inspect
    a neighbourhood now shrinks nodes (at 2× zoom they're ~71 %
    of base size, at 4× ~50 %) instead of bloating them into
    each other. Throttled to one `requestAnimationFrame` tick
    per zoom burst so it doesn't fight the zoom gesture.

### Fixed

- Graph no longer "randomises" when the user navigates from
  another tab back into Graph. Three interlocking root causes
  were dismantled:
  - **Flush-on-unmount**: Tier3 used to drop any save still
    inside the 400 ms debounce window when React unmounted it
    on route change — `setTimeout`'s callback was garbage-
    collected with the closure and the positions never reached
    SQLite. A cleanup useEffect now cancels the timer *and*
    fires a final `saveGraphPositions(...)` immediately so the
    IPC is in flight before unmount completes.
  - **fcose `fixedNodeConstraint`**: when only some nodes had
    persisted positions (new pages joined an existing layout)
    fcose's partial-seed path used to nudge the saved nodes
    along with the rest. Pre-existing saved positions are now
    passed in `fixedNodeConstraint`, freezing them as anchors
    so the force iterations move only the new arrivals.
  - **Visible save-failure feedback**: a silent IPC failure
    used to leave the user thinking their hand-tuned layout
    was being persisted while in fact every reopen fell back
    to fresh fcose. Saves now surface as a warning toast
    ("Graph layout couldn't be saved") on first failure of
    a Tier3 mount, with a one-shot guard so a sustained
    backend issue doesn't carpet-bomb the toast layer.
- History tab opens instantly on first visit. `wikiHistory(100)`
  is now warmed in the background by `AppShell` on shell mount
  via a new `useWikiHistoryStore` (zustand) cache. The History
  view consumes from the store instead of firing its own IPC,
  and a `wiki-changed` subscription on the shell invalidates
  the cache after every auto-commit so the timeline stays
  fresh. Restore/hard-reset actions also nudge the store so
  the new "revert: …" / "reset: …" commits show without
  waiting for the event roundtrip.

## [0.2.13] — 2026-05-06

### Fixed

- Mini-map toggle truly does not relayout the graph anymore. The
  v0.2.10 / v0.2.11 / v0.2.12 attempts kept whittling away at the
  symptom but never removed the root cause: `showMinimap` was in
  the create-effect's dependency list, so every toggle destroyed
  cy and built a fresh one — even with `positionMapRef`'s
  perfectly preserved coordinates, a freshly-built cy still has
  to *run* a layout, and any state edge-case in that single run
  (NaN render coordinates → pink-screen recovery, partial-seed
  detection, etc.) could degrade the result. The only structurally
  correct fix is to never rebuild cy on toggle. Mini-map lifecycle
  is now in a separate `useEffect` keyed on `showMinimap` only;
  it attaches / detaches `cytoscape-navigator` against the live
  cy via two helpers (`attachNavigatorIfWanted` /
  `detachNavigator`) and never touches the cy graph. The main
  create-effect's deps list lost `showMinimap` and now reads
  only `[nodes, edges, renderKey, layoutMode, layoutResetSignal]`.
  When the main effect *does* rebuild cy (e.g. graph data
  changed), it detaches the navigator first and reattaches via
  the same helper at the end so a stale instance can never point
  at a destroyed cy.

## [0.2.12] — 2026-05-06

### Changed

- Graph zoom now adapts to the input device. Cytoscape's built-in
  wheel zoom uses a single `wheelSensitivity` constant which can't
  feel right on both a discrete mouse-wheel (deltaY ≈ 100 per
  click) and a continuous trackpad scroll (deltaY ≈ 5 per event,
  dozens of events per gesture). Pre-0.2.12 we'd pinned it at 0.2
  to keep macOS trackpad zoom from jumping by 50 % per tick — at
  the cost of the mouse-wheel feeling glacial. Replaced with a
  custom wheel handler that picks a per-event zoom step from the
  event's |deltaY|:
  - **Mouse wheel** (|deltaY| ≥ 50): 15 % per tick — meaningful
    step without overshooting.
  - **Trackpad scroll** (|deltaY| < 50): 2 % per event — feels
    smooth across the gesture.
  - **Trackpad pinch** (browsers report this as a wheel event
    with `ctrlKey: true` and pre-scaled deltaY): 5 % per event so
    the pinch feels natural without amplifying the browser's
    pre-scaling.
  Zoom is also now centred on the cursor instead of the canvas
  centre — standard Map-/Graph-UI expectation, much nicer when
  hovering over a specific node before zooming in.

## [0.2.11] — 2026-05-06

### Fixed

- Mini-map toggle truly no longer rotates the graph. The v0.2.10
  attempt fixed one half of the race (synchronous
  `setSavedPositions`) but missed two more sources of remount-with-
  stale-positions:
  - **`positionMap` was a `useMemo` keyed off `savedPositions`**
    and listed in the create-effect's dependency array. Even with
    a synchronous state update, React's re-render is asynchronous;
    a user click that landed before the next commit caused the
    effect to re-run with the prior positionMap (still empty),
    and fcose ran again with `randomize: true`.
  - **`onNodeClick` was an inline arrow function in Tier3** with no
    `useCallback`, so every Tier3 state change (including
    `setShowMinimap`) created a new function identity, the
    create-effect saw a "changed" dependency, and the cy instance
    was destroyed and rebuilt — even though nothing meaningful
    about the graph had changed.
  Fix: GraphCanvas now keeps a `positionMapRef` that the cy
  `layoutstop` and `dragfree` callbacks update synchronously (so
  the freshest layout is always available even before React
  commits a render), `onNodeClick` is captured into a ref like the
  other callbacks, and the create-effect's dependency list drops
  both. Re-layouts go through a new `layoutResetSignal` prop —
  Tier3 bumps it on the "Re-layout" button so the explicit-reset
  path still works. Net effect: toggling the mini-map (or any
  other Tier3 state) preserves the on-screen layout exactly as
  it was, the way the user wanted.

## [0.2.10] — 2026-05-06

### Fixed

- Graph layout no longer rotates on every reopen / mini-map toggle.
  Two underlying causes:
  - **Race between fcose and the position-save round-trip.** Tier3
    only mirrored the just-computed coordinates into local state
    AFTER `saveGraphPositions(...).then(...)` resolved — i.e. after
    the 400 ms debounce + the Tauri IPC roundtrip, ~500 ms total.
    Any remount in that window (mini-map toggle, wiki-changed event,
    filter change) found `savedPositions` still empty, ran fcose
    again with `randomize: true`, and produced a fresh rotation.
    `handlePositionsChange` now mirrors positions into local state
    *immediately*, before the IPC. Subsequent re-mounts within the
    save window see the fcose result and use `preset`, not `fcose`.
  - **fcose-with-partial-seeds reshuffled the entire graph.** When
    a single new wiki page appeared between sessions (one node
    without a saved position, all others with), the layout
    dispatcher dropped to fcose with `randomize: true`, ignoring
    every saved coordinate. Old pages got randomly re-placed.
    Layout now sets `randomize` based on whether ANY saved seeds
    exist: cold-start (no saves) keeps `randomize: true` to avoid
    the (0,0)-collapse degenerate row; partial saves use
    `randomize: false` so existing pages stay roughly where the
    user put them and only the new ones get force-placed.

## [0.2.9] — 2026-05-06

### Fixed

- Toggling the mini-map off no longer crashes the renderer with
  `Cannot read properties of null (reading 'removeChild')`. The
  cytoscape-navigator plugin's `destroy()` with the v0.2.8
  setting `removeCustomContainer: true` calls
  `this.$panel.parentElement.removeChild(this.$panel)`, but
  React's commit phase removes the conditionally-rendered
  `<div id="cy-navigator-container">` from the DOM *before*
  the effect cleanup fires — so `parentElement` is null when
  the navigator tries to detach itself. Flipping the option to
  `removeCustomContainer: false` makes the destroy path take
  `this.$panel.innerHTML = ''` instead, which is safe on a
  detached element; React then garbage-collects the empty
  container during its own unmount. Single-line config change
  in `GraphCanvas.tsx`.

## [0.2.8] — 2026-05-06

### Fixed

- Mini-map overlay no longer hijacks the viewport. Previous
  attempt (the v0.2.7 release) tried to override the package's
  `.cytoscape-navigator` CSS with `!important`, which masked the
  symptom but missed the actual cause: `cytoscape-navigator`'s
  `container` option only accepts a string CSS selector
  (verified in the package source at `cytoscape-navigator.js`
  line 378). Passing an HTMLElement is silently rejected and
  the plugin builds its own `<div class="cytoscape-navigator">`
  attached to `document.body` with the package's hard-coded
  white 400×400 styling — that was the giant white block the
  user reported. Fix: give our container element an `id`
  attribute and pass the matching `#…` string to the navigator
  plugin so it actually re-uses our element. The container is
  then styled directly with Tailwind (small dark thumbnail in
  the GraphCanvas's bottom-LEFT, away from the StatusBar's
  version label) and the only remaining CSS rule applies to
  the inner `.cytoscape-navigatorView` viewport rectangle —
  recoloured from the package's pale baby-blue to BRAIN's
  accent green so it reads on the dark thumbnail.
- Bootstrap now auto-mounts an attached BRAIN drive even when
  there's no saved `last_active_vault_path`. Pre-0.2.8 the
  startup flow only tried the persisted path; if that was None
  (fresh install, post-temp-self-heal, post-reset_brain) the
  user landed in the welcome wizard even with the BRAIN disk
  literally plugged in. The new fallback runs `list_disks()`
  after the persisted-path branch, finds the first attached
  mount whose root contains the BRAIN marker file, mounts it,
  and persists the path so the next startup skips the disk-scan.
  Cross-platform via the existing `disks` module (Disk-arbitration
  on macOS, udev on Linux, WMI on Windows). Stale-but-saved paths
  still surface the "BRAIN is offline" screen instead of silently
  switching to a different attached vault.

### Changed

- `bootstrap_app` internals refactored: extracted the
  mount-and-finalize sequence into `complete_auto_mount` so the
  persisted-path branch and the new auto-detect-fallback branch
  use the same post-mount machinery (DB open, watcher spawn,
  state event emit, background indexing). Behaviour for the
  saved-path happy path is byte-identical to v0.2.7.

## [0.2.7] — 2026-05-06

### Fixed

- Force-directed graph layout no longer collapses into a "comic
  row" of nodes. v0.2.6's pink-screen mitigations had set
  `randomize: false` and `quality: "draft"` on the fcose layout,
  which combined to leave nodes starting at the same coordinates
  and not getting enough force-iterations to spread apart. With
  the v0.2.6 position-persistence already in place, fcose only
  runs once per vault before the cached preset takes over, so
  paying the full GPU cost once is acceptable. Both knobs are
  back at the library defaults (`randomize: true`,
  `quality: "default"`); the still-active mitigations
  (200 ms ResizeObserver debounce + auto-recover plan-B remount
  on NaN node positions) handle the GPU stress without degrading
  layout quality.

### Changed

- **Mini-map is now opt-in.** v0.2.6 showed it permanently in the
  bottom-right corner where it crowded the StatusBar's version
  label and added GPU cost even when the user wasn't panning.
  New "Mini-map" toggle in the graph toolbar — default off,
  shown when the user wants pan-by-thumbnail. When off, the
  navigator extension is not instantiated at all (zero
  background cost).
- Mini-map repositioned to bottom-LEFT and shrunk from 128×176 px
  to 96×128 px so even when shown it reads as a thumbnail rather
  than a competing pane, and never overlaps the version label.

## [0.2.6] — 2026-05-06

### Fixed

- Post-update offline loop. Pre-0.2.6 a stale temp-directory path in
  `last_active_vault_path` (most likely written by an earlier setup
  that picked the wrong default folder) survived every update,
  pointed BRAIN at a path the OS had already wiped, and forced the
  user back through the wizard on each launch — where the same
  broken default location was about to get re-saved. Two layers
  prevent that now: (1) `finish_onboarding` rejects any path inside
  the OS temp dir with a clear error, (2) `bootstrap_app` self-heals
  by treating an existing temp-dir-based path as None and sending
  the user to onboarding instead of the "BRAIN is offline" screen.
  Detection works across stale 8.3-shortname paths
  (`PASCAL~1.POL\AppData\Local\Temp\…`) that no longer exist on disk.
- Pink-screen Cytoscape rendering on macOS (Apple Silicon WKWebView).
  The graph view used to occasionally fill its entire area with
  magenta — Metal's debug-fill backstop when a backing-store
  allocation fails. Two reinforcing fixes:
  - **Auto-recover** (Plan B): on `layoutstop`, if every node has
    non-finite render coordinates, the GraphCanvas remounts itself
    once. Guarded by a one-shot ref so a graph that stays broken
    after the recovery doesn't loop.
  - **Hardening** (Plan C): ResizeObserver now debounces window-drag
    bursts to 200 ms (was RAF, too tight); fcose runs with
    `randomize: false` for deterministic layouts; `quality` drops
    to `"draft"` past 100 nodes so the GPU pipeline doesn't get
    pinned long enough to trip Metal's failure path.

### Added

- **Persistent graph layout** with one-click reset. New
  `node_positions` table (DB migration v4) stores the user's last
  layout per page id. The graph viewer reads it on mount and skips
  fcose entirely (preset layout) when every visible node has a
  saved position — re-opens are visually instant. Drag-and-drop is
  saved automatically (debounced 400 ms). New "Re-layout" toolbar
  button wipes the table so the next mount falls back to fcose,
  and the resulting positions get persisted afresh.
- **Hierarchical layout** as an alternative to force-directed.
  Toolbar toggle "Force / Hierarchical" — Hierarchical uses
  `cytoscape-dagre` for a top-down rank flow, useful when looking
  for parent/child structure across topics → concepts → entities.
- **Cluster-focus chip strip**. Connected components are computed
  on every layout settle; if the graph has more than one cluster a
  chip strip appears below the filter bar showing each cluster's
  size. Clicking a chip fits the viewport to that cluster only;
  clicking "All" releases the focus.
- **Mini-map overlay** in the graph view's bottom-right corner via
  `cytoscape-navigator` — drag the viewport rectangle to pan
  large graphs without losing context. Static framerate
  (`viewLiveFramerate: 0`) so the navigator doesn't add to GPU
  pressure.
- **Hover tooltip** on graph nodes: title + backlink count + the
  first prose line of the page body. Body excerpt is lazy-fetched
  via `read_page` and cached in-memory per id, so re-hovering the
  same node doesn't re-fire the IPC.
- **Degree-based node sizing**. Nodes with more wiki-link
  connections render larger (linear in degree, capped at 56 px),
  giving the graph an immediate visual hierarchy without any
  configuration.

## [0.2.5] — 2026-05-06

### Fixed

- `brain_write_page`'s auto-normalisation no longer corrupts markdown
  tables. Pre-0.2.5, a markdown-style link inside a table cell —
  `| [Dan](entities/dan-shapiro) | CEO |` — was rewritten to pipe-form
  wiki-link `[[entities/dan-shapiro|Dan]]`, and the resulting `|Dan]]`
  collided with the table's column separator, silently breaking the
  rendered row in the viewer (and in every standard GFM renderer).
  `normalize_internal_links` now detects markdown table rows
  (line-by-line, leading-and-trailing `|` heuristic) and leaves their
  content verbatim. The original markdown-style link still renders
  correctly and `extract_wiki_links` still picks it up as a graph
  edge — only the unsafe rewrite-to-pipe-form is suppressed inside
  table rows. Idempotent, prose before/after tables stays unaffected.

## [0.2.4] — 2026-05-06

### Fixed

- `brain_list_pages` no longer times out on large vaults backed by slow
  storage. When the SQLite index is available the dispatch hits a
  `SELECT id FROM pages` fastpath (sub-millisecond regardless of disk
  speed) instead of walking the filesystem; without the index the
  walker is still used as a fallback and never reads file contents.
  This is the fix for the user-reported 4-minute timeout on
  `BRAIN:brain_list_pages`.
- `brain_write_page`'s lint-failure response now embeds the full
  `LintError[]` array as JSON next to the human summary (`"page
  written but lint failed: 13 errors\n[…]"`). Pre-0.2.4 only the
  count was returned, so the LLM had to probe iteratively to discover
  *which* links were broken — multiple round-trips for data that's
  already known server-side. Calling LLMs can now create the missing
  pages or fix typos in one shot.

### Added

- **`brain_list_pages` filter parameters** (all optional, no-arg
  behaviour unchanged for backward compat):
  - `type`: `entities | concepts | sources | topics` — restrict to
    one bucket
  - `prefix`: id-prefix substring filter (e.g. `entities/dextra`)
  - `limit` / `offset`: per-bucket pagination
  Server-side filtering saves both bandwidth and tokens for the LLM
  on large vaults.
- **`brain_page_exists`** — new lightweight existence-check tool.
  Returns `{id, exists}` for the given page id via a single
  `Path::is_file()` call. Use this when the LLM only needs yes/no
  (e.g. before deciding create-vs-update); much cheaper than
  `brain_get_page`, which loads and parses the entire markdown body.
  Hardened against empty ids and `..` path-traversal attempts.

## [0.2.3] — 2026-05-06

### Changed

- Auto-update install is now **silent** on Windows
  (`plugins.updater.windows.installMode = "quiet"`). The NSIS installer
  no longer pops a wizard window during update; the app closes for a
  couple of seconds, the binary is replaced in-place, the app reopens
  on the new version. Matches the Claude Desktop / Slack
  background-install experience. Note: this only takes effect for
  updates *originating from* a 0.2.3+ install — the legacy 0.2.0 →
  0.2.3 path still shows the visible installer because the 0.2.0
  binary doesn't know about the new flag.

### Fixed

- MCP subprocess no longer dies silently when a tool handler panics —
  the dispatch loop catches unwinds and converts them into a JSON-RPC
  `-32603` "Internal error" response. Claude Desktop / Claude Code stay
  connected, the LLM gets a structured error it can react to, and the
  user no longer has to restart the client to recover the BRAIN
  connection. Cause of the recurring "Server transport closed
  unexpectedly" entries in `mcp-server-brain.log`.
- **Codex registration**: file path moved from `~/.codex/config.json`
  (legacy) to `~/.codex/config.toml`, and the format is now valid TOML
  with `[mcp_servers.BRAIN]` + `[mcp_servers.BRAIN.env]` sub-tables.
  Codex CLI's `/mcp` command now actually finds BRAIN; pre-0.2.2
  installs were writing to a path Codex never reads. Cross-platform:
  same path on macOS, Linux and Windows; `CODEX_HOME` env override is
  respected. Existing entries in `~/.codex/config.toml` are preserved
  via `toml_edit` (round-trip-safe) so the user's other Codex config
  isn't disturbed.

### Added

- Stderr-only logging for the `brain mcp` subprocess
  (`logging::init_for_mcp`). Tracing output goes to stderr exclusively
  so the stdout JSON-RPC channel stays uncorrupted; Claude Desktop
  captures stderr into `mcp-server-<name>.log`, so panics, warnings and
  vault-disconnect notices are now visible during diagnosis instead of
  being dropped on the floor. Cross-platform: relies only on
  `std::io::stderr`, which behaves identically on macOS, Linux and
  Windows.

### Removed

- **ChatGPT Desktop auto-registration**. ChatGPT does support MCP
  servers (Settings → Apps & Connectors → Advanced → Developer Mode),
  but the connect originates from OpenAI's backend, so the URL must
  be public HTTPS — localhost is unreachable. Registration is UI-only
  with no config file we could write. Pre-0.2.2 BRAIN was writing
  `mcp.json` files at paths ChatGPT never reads, so the "Registered"
  status was misleading. Settings and onboarding now spell out the
  manual tunnel-based workaround (Cloudflare Tunnel / ngrok) plus its
  C-04 trade-off, instead of pretending ChatGPT is auto-supported.

### Documentation

- README now explains the macOS Sequoia 15+ Gatekeeper situation: BRAIN
  is not yet Apple-Developer-ID-signed, so first launch needs a one-off
  `xattr -cr /Applications/BRAIN.app` to clear the quarantine flag. The
  Code-signing section spells out the proper Developer ID + notarytool
  roadmap (the GitHub Actions secrets are already wired up, just
  commented out until the cert is acquired).

### Cleanup

- On first post-upgrade mount, BRAIN deletes the orphan files older
  versions wrote to paths the target clients don't actually read:
  `~/.codex/config.json` and the per-OS ChatGPT `mcp.json`
  (`%APPDATA%\ChatGPT\mcp.json` on Windows,
  `~/Library/Application Support/ChatGPT/mcp.json` on macOS,
  `~/.config/chatgpt/mcp.json` on Linux). Conservative: only files
  whose root contains exactly one `mcpServers` map with no entries
  other than BRAIN's canonical and legacy keys are removed. Anything
  with foreign entries, hand-edits, or non-JSON content is left alone.

## [0.2.1] — 2026-05-05

### Fixed

- Status-bar version label is no longer the hardcoded literal `BRAIN
  v0.1.0`. It now reads the running binary's version via Tauri's
  `getVersion()` API, so a successful auto-update reflects in the UI
  immediately instead of pretending nothing happened.

## [0.2.0] — 2026-05-05

### Fixed

- Onboarding flow no longer crashes with a React Router 404 right after
  the model-download step on "Open existing vault" runs — stale
  `navigate("/onboarding/connectors")` pointed at a route that was
  removed during the connectors-tab cleanup

### Changed

- Bundle identifier moved from `com.ppoletto.brain` to `eu.poletto.brain`
  to reflect the actual domain. Existing 0.1.0 Tauri data at
  `%LOCALAPPDATA%\com.ppoletto.brain\` becomes orphaned after upgrade and
  is safe to delete manually
- `bundle.targets` back to `"all"` so MSI is built alongside NSIS;
  `updaterJsonPreferNsis: true` keeps `latest.json` pointing at the NSIS
  variant for seamless per-user auto-updates

## [0.1.0] — 2026-05-05

First public release on GitHub.

### Added

- BRAIN-themed app + tray icons in all four state colours
- "Click the version in the status bar" → on-demand update check
- Launch-at-login toggle in Settings → General (Windows registry, macOS
  LaunchAgent, Linux `~/.config/autostart/`)
- Wiki-link clicks in Search results open the linked page in the same
  reader (no more 404)
- Graph node clicks deep-link the Browse tab to the picked page
- Wiki history detail panel: per-file diff, +/− stats, per-page restore
  buttons, hard-reset action with confirmation
- Search has a real loading spinner — no more "did the click land?"
  silence
- BRAIN logo replaces the generic CSS spinner on Bootstrap and
  Onboarding splash screens
- Release playbook: `docs/RELEASE.md` with the minisign + GitHub Actions
  flow
- Tauri 2 updater plugin wired to GitHub Releases with `latest.json` +
  minisign signatures (`createUpdaterArtifacts: true`)

### Changed

- Display name is uppercase **BRAIN** everywhere it's user-visible
  (tray, window title, MCP server key, manual snippets). Code-level
  identifiers and the binary stay lowercase.
- Status bar at the bottom now shows the vault path instead of repeating
  the tray pill's status — pill stays the source of truth for liveness
- Hybrid search KNN sub-query restructured so sqlite-vec's required
  `LIMIT` constraint sits on the inner query (fixes "A LIMIT or 'k = ?'
  constraint is required" on graph queries)
- bge-m3 loader now reads `pytorch_model.bin` (matches BAAI's actual
  artefact) with a fallback to `model.safetensors` for community
  mirrors
- hf-hub upgraded to 0.4 — the 0.3 redirect bug
  (`RelativeUrlWithoutBase`) on HuggingFace's CDN no longer breaks the
  bge-m3 download

### Fixed

- Onboarding hangs on download failure → now offers Retry / Skip / Back
- "Disconnected" stuck after format/reset → AppShell pulls the actual
  tray status on mount instead of waiting for the next event
- CMD/PowerShell windows flashing during disk operations and MCP
  registration on Windows → all subprocesses now spawn with
  `CREATE_NO_WINDOW`
- Elevated PowerShell during disk format runs hidden after the UAC
  prompt
- Hybrid search returning empty after vault reset because vec-extension
  load happened *after* the first SQLite connection — moved before
- Keyboard shortcuts swallowed on the Graph tab — `useGlobalShortcuts`
  now runs in capture phase and stops propagation
- Mod+1/2/3/H/K work even with the global search bar focused

### Security

- MCP server key migrated from `brain` to `BRAIN` — both unregister
  flows purge the legacy entry to avoid duplicates after upgrade

---

## [0.0.1] — 2026-04-30

Initial private build. The bones of the system: Tauri 2 tray app,
Cryptomator-format-ready vault layout, MCP auto-registration in five
LLM clients, FTS5 + sqlite-vec hybrid search with bge-m3 embeddings,
auto-commit wiki versioning, three-tier viewer, settings UI.

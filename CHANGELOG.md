# Changelog

All notable changes to BRAIN are tracked here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

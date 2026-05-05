<div align="center">

<img src="src-tauri/icons/128x128.png" alt="BRAIN" width="96" height="96"/>

# BRAIN

**A personal knowledge system that lives on a portable SSD or local folder, with first-class MCP integration for every LLM you already use.**

[![Built with Tauri 2](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=white)](https://v2.tauri.app/)
[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust&logoColor=white)](https://rust-lang.org)
[![TypeScript](https://img.shields.io/badge/TypeScript-React%2018-3178C6?logo=typescript&logoColor=white)](https://www.typescriptlang.org/)
[![Status](https://img.shields.io/badge/status-pre--1.0-blue)](#roadmap)

</div>

---

BRAIN is a single-user knowledge management application built for a specific
workflow: an external SSD (or local folder) holds an opinionated layout of
Markdown files that you author, read, and link freely; an embedded SQLite
index makes them searchable in milliseconds; a built-in MCP server exposes
the whole vault to Claude Code, Claude Desktop, Codex, Continue.dev and any
other MCP-compatible LLM client running on the same machine.

The tray icon stays out of your way until you need it. When you plug in
your BRAIN SSD on a different computer, the wizard remounts the vault and
re-registers MCP automatically — your context follows you.

> [!NOTE]
> This is a personal-tool-as-a-product. It is intentionally *not* a
> multi-user, sync-everything, cloud-first system. If you want
> Notion or Obsidian Sync, you want a different application. If you
> want a private, portable, LLM-aware second brain that you fully
> own, read on.

---

## Table of contents

- [Why](#why)
- [What you get](#what-you-get)
- [Screenshots](#screenshots)
- [Architecture](#architecture)
- [Getting started](#getting-started)
- [Usage](#usage)
- [MCP integration](#mcp-integration)
- [Configuration](#configuration)
- [Building from source](#building-from-source)
- [Release process](#release-process)
- [Roadmap](#roadmap)
- [FAQ](#faq)
- [License](#license)

---

## Why

I write a lot. I read a lot. I refer to a lot of things later. The state of
the art for "remember this" in 2026 is still:

- **Notion-style apps** — tied to a vendor, sluggish search, opaque LLM access
- **Obsidian** — beautiful, but no native LLM integration, plugin-driven sync
- **Browser tabs** — the dishwasher pile of digital life
- **`~/notes/`** — fine until you have 4,000 of them

BRAIN sits in a different corner: **portable, private, LLM-native**.

- The vault is a directory tree of Markdown files. Open it in any editor.
  No proprietary format, no lock-in. Cryptomator-format encryption is on
  the roadmap.
- Search is hybrid: SQLite FTS5 (lexical) + sqlite-vec (semantic via the
  bge-m3 multilingual embedding model), fused with reciprocal-rank-fusion.
  Sub-100 ms over a 10k-page wiki.
- Every change is auto-committed to a per-vault Git repo. You can browse
  the history, restore individual pages, or roll the whole wiki back —
  always as a *new* commit, never destructive.
- The MCP server runs locally. Your Claude/Codex/Continue.dev session
  reads, writes, and queries the vault directly through stdio — no cloud
  hop, no third-party auth.

---

## What you get

### Tray app

A small icon in your menu bar. Four colours, four meanings:

|       | State              | Meaning |
|-------|--------------------|---------|
| 🟢    | `mounted-idle`     | Vault is open, nothing is in flight, safe to remove. |
| 🟡    | `mounted-busy`     | Indexing, embedding or auto-committing — wait before unplugging. |
| 🔴    | `error`            | A subsystem is unhappy. Hover for the message. |
| ⚪    | `disconnected`     | No vault is mounted. |

Right-click the tray for: open window, jump to Browse / History / Settings,
re-register MCP, cleanly eject the SSD, quit.

### Window: three tiers

| Tier | Tab | What it does |
|------|-----|--------------|
| 1    | **Browse**  | Tree of Markdown pages on the left, rendered Markdown on the right, frontmatter toggle, "Open in editor" jumps to your default editor. |
| 2    | **Search**  | Hybrid full-text + semantic search, plus a structured query DSL (`type:source AND tag:nis2 AND updated:>2026-04-01`). Results show snippets; clicking opens the page with backlinks. |
| 3    | **Graph**   | Interactive graph of pages and `[[wiki-links]]`. Tag, type, and recency filters. Click a node to jump to it in Browse. |

Plus **Wiki history** (commit timeline with diff/files panel and per-page
restore), **Settings** (autostart, MCP-registration status, manual
re-register, reset, danger-zone), and an **Integrity check** screen that
auto-runs after an unclean shutdown.

### Vault layout

Decrypted view of an open vault:

```
<vault>/
├── 00_meta/                       internal metadata
│   ├── .mcp.json                  MCP server config + bearer token
│   ├── AGENTS.md                  conventions for LLM agents writing pages
│   └── brain-marker.json          format identifier
├── 01_raw/                        raw source material (emails, exports, …)
├── 02_wiki/                       Markdown pages — git-versioned
│   ├── .git/
│   ├── entities/
│   ├── concepts/
│   ├── sources/
│   ├── topics/
│   ├── index.md                   auto-generated catalogue
│   └── log.md                     auto-generated change log
├── 03_db/                         SQLite + WAL + sqlite-vec
│   └── brain.db
├── 04_models/                     embedding model files (~2.3 GB)
│   └── bge-m3/
├── 05_cache/                      transient state
└── 06_logs/                       structured logs
```

Pages have YAML frontmatter:

```markdown
---
id: entities/dan-shapiro
type: entity
title: Dan Shapiro
tags: [customer, sustainability]
created: 2026-04-29
updated: 2026-04-29
---

# Dan Shapiro

CEO of [[entities/glowforge]], speaker on consumer-3D-printing.
Linked customer of [[concepts/circular-economy]].
```

Wiki-style `[[id]]` links resolve at index time and feed the graph view.

---

## Screenshots

> Drop screenshots into `docs/screenshots/` and reference them here, e.g.
> `![Browse view](docs/screenshots/browse.png)`. The README leaves slots
> for: tray menu, onboarding wizard, browse + search + graph tabs, history
> detail panel, MCP setup screen.

---

## Architecture

```
                   ┌──────────────────────────┐
                   │     Tauri main (Rust)    │
                   └──┬─────────────────┬─────┘
                      │                 │
              ┌───────▼──────┐   ┌──────▼──────┐
              │  Tray + UI   │   │ MCP server  │
              │ (React/TS)   │   │ (stdio +    │
              └───────┬──────┘   │  HTTP)      │
                      │          └──────┬──────┘
                      ▼                 ▼
       ┌──────────────────────┐  ┌─────────────┐
       │  Mount + Wiki + Git  │  │ Tool calls  │
       │  + Index + Embed     │  │ from LLM    │
       └──────────┬───────────┘  └──────┬──────┘
                  │                     │
                  └──────┬──────────────┘
                         ▼
                ┌────────────────┐
                │   <vault>/     │
                │   on disk      │
                └────────────────┘
```

| Layer        | Stack |
|--------------|-------|
| Desktop shell | [Tauri 2](https://v2.tauri.app/) |
| Backend       | Rust (Edition 2024), single binary |
| Frontend      | TypeScript + React 18 + Vite + Tailwind 4 |
| Storage       | SQLite (WAL, FTS5, [sqlite-vec](https://github.com/asg017/sqlite-vec)) |
| Embedding     | [bge-m3](https://huggingface.co/BAAI/bge-m3) (1024-d, multilingual) via [candle](https://github.com/huggingface/candle) |
| Versioning    | libgit2 via [git2-rs](https://crates.io/crates/git2) |
| MCP           | [Model Context Protocol](https://modelcontextprotocol.io) JSON-RPC over stdio + HTTP |
| Update        | Tauri Updater + minisign signature verification |
| Graph         | [Cytoscape.js](https://js.cytoscape.org/) + [fcose](https://github.com/iVis-at-Bilkent/cytoscape.js-fcose) layout |

The architecture document lives at [`docs/architecture.md`](docs/architecture.md).
Specs (NLSpec methodology) live at [`requirements/spec/`](requirements/spec).

---

## Getting started

### Prerequisites

- **OS:** Windows 10/11, macOS 12+, or any Linux with WebKitGTK 2.40+
- **Disk:** Either a portable SSD (formatted as exFAT, label `BRAIN`) or
  any local folder you can write to
- **RAM:** ≥ 4 GB free during indexing; bge-m3 takes ~1 GB for inference
- **Network:** First-time onboarding downloads the bge-m3 weights
  (~2.3 GB) from HuggingFace. After that BRAIN works fully offline.

### Install (when published)

> [!NOTE]
> Pre-built bundles are not yet on GitHub Releases. For now, build from
> source — see [Building from source](#building-from-source). The release
> workflow is documented in [`docs/RELEASE.md`](docs/RELEASE.md).

Download the bundle for your platform from
[Releases](https://github.com/PPoletto/BRAIN/releases) and run the
installer:

- **Windows:** `BRAIN_<version>_x64-setup.exe`
- **macOS:**   `BRAIN_<version>_universal.dmg`
- **Linux:**   `brain_<version>_amd64.AppImage` or `.deb`

### First run (onboarding)

The tray icon appears, the window opens to the wizard:

1. **Welcome** → Create a new BRAIN, or open an existing vault
2. **Medium** → Pick an external SSD or a local folder
3. **Format** *(only when creating on an SSD)* → exFAT format with `BRAIN`
   label, OS-native UAC/admin prompt
4. **Setup** → Initialize the directory layout, populate the canonical
   templates (`AGENTS.md`, `CLAUDE.md`, `.mcp.json`), download bge-m3.
   The download step has a **Skip** option — BRAIN will fall back to a
   deterministic feature-hash embedder until you re-run the download from
   Settings, so search still works.
5. **Connectors** *(optional)* → Quick-setup for MS365, Atlassian,
   HubSpot MCP connectors
6. **Done** → MCP is registered with every detected LLM client, the tray
   turns green

---

## Usage

### Reading and writing

Pages live in `<vault>/02_wiki/<type>/<slug>.md`. Open them in any editor
— BRAIN is your reader, your editor is your writer. Common combos:

- VSCode + Obsidian-flavoured-Markdown extension
- Obsidian itself, pointed at `02_wiki/` as a vault
- nvim + telescope.nvim
- Typora, Mark Text, … anything that handles plain Markdown with YAML frontmatter

The watcher debounces edits for 5 seconds, lints the changed pages, then
auto-commits with a message like `2 pages updated: entities/alice,
concepts/nlspec [trigger=edit]`. You see the commit appear immediately in
**Wiki history**.

### Searching

Two modes in the **Search** tab:

| Mode | What it does |
|------|--------------|
| **Full-text** | FTS5 BM25 + bge-m3 cosine, RRF-fused. Pass a free-form query like `nis2 audit obligations`. |
| **Query DSL** | Structured filter over frontmatter: `type:source AND tag:customer AND updated:>2026-04-01`. Fields: `id`, `type`, `title`, `tag`, `created`, `updated`. Operators: `:` (eq), `:>` (after), `:<` (before). Combine with `AND`, `OR`, `NOT`, parens. |

Click any result to read the page, follow `[[wiki-links]]`, and see
backlinks — all without leaving the Search tab. **Open in Browse** jumps
to the same page with the full sidebar tree.

### Graph

The **Graph** tab visualises pages as nodes and wiki-links as edges. Use
the type/tag/recency filters at the top to focus. Click a node to open
the page in Browse. Small graphs (≤ 12 nodes) use a deterministic
concentric layout; bigger ones get fcose's organic force-directed layout.

### Keyboard

Modifier-prefixed shortcuts work everywhere — including with the search
bar focused.

| Combo       | Action |
|-------------|--------|
| `Mod`+`,`   | Settings |
| `Mod`+`1`   | Browse (Tier 1) |
| `Mod`+`2`   | Search (Tier 2) |
| `Mod`+`3`   | Graph (Tier 3) |
| `Mod`+`H`   | Wiki history |
| `Mod`+`K`   | Focus the global search |

`Mod` = `Cmd` on macOS, `Ctrl` on Windows/Linux.

### Eject

Right-click tray → **Eject BRAIN**. BRAIN refuses to unmount while there
are active operations (indexing, embedding, an open MCP tool call). If
you're sure, it will offer a force-eject — that sets the
`unclean-shutdown` flag in `00_meta/`, and the next mount runs an
**Integrity check** to repair anything that was mid-write.

---

## MCP integration

BRAIN auto-registers itself in every MCP-compatible client it detects on
the host:

| Client | Where it gets written |
|--------|----------------------|
| Claude Code (CLI) | `~/.claude.json` (`mcpServers.BRAIN`) |
| Claude Desktop    | `%APPDATA%\Claude\claude_desktop_config.json` (Windows), `~/Library/Application Support/Claude/...` (macOS), `~/.config/Claude/...` (Linux) |
| Codex             | `~/.codex/config.toml` / `~/.codex/mcp.json` |
| Continue.dev      | `~/.continue/config.json` |
| ChatGPT Desktop   | The MS Store sandbox path on Windows, `~/Library/...` on macOS |

After registration, **restart your LLM client** (Claude Desktop reads
config only at process start). The MCP server name is `BRAIN`. Verify
with `claude mcp list` for Claude Code.

### Tools exposed

| Tool | Purpose |
|------|---------|
| `brain_search`        | Hybrid search across the wiki |
| `brain_query`         | Dataview-style structured query |
| `brain_get_page`      | Read a single page by id |
| `brain_get_context`   | Page body + outbound links + backlinks |
| `brain_list_pages`    | Tree listing |
| `brain_graph`         | `{nodes, edges}` for graph rendering |
| `brain_write_page`    | Create or update a page (lints before commit) |
| `brain_write_raw_file`| Drop a raw source file under `01_raw/<connector>/…` |

### Manual setup snippet

For clients without auto-registration (Open WebUI, custom integrations),
copy the snippet from **Settings → MCP & Clients → Manual setup snippet**.
It's a one-line CLI command for Claude Code, or a JSON object you can
paste into any `mcpServers` map.

### Replace Claude Desktop's built-in memory

Claude Desktop has its own internal memory by default. To make it write to
BRAIN instead, copy the system-prompt snippet from
**Settings → Memory mode** into a new Claude Desktop *Project*'s
instructions. Conversations under that project will store and recall via
BRAIN's `brain_write_page` and `brain_search` tools.

---

## Configuration

### Settings UI

- **General** — Launch at login toggle, tray menu reference,
  keyboard reference, status colour legend
- **Connectors** — list of optional MCP connectors (Outlook, Atlassian,
  HubSpot…)
- **MCP & Clients** — auto-registration status per client, re-register
  button, Claude Desktop verification checklist, manual setup snippets
- **Memory mode** — system-prompt snippet to redirect Claude Desktop's
  memory into BRAIN
- **Danger zone** — Reset BRAIN (eject + forget vault path + relaunch
  onboarding; vault data on disk is **not** deleted)

### File locations

- **OS config dir** (`~/.config/com.ppoletto.brain/` on Linux,
  `~/Library/Application Support/com.ppoletto.brain/` on macOS,
  `%APPDATA%\com.ppoletto.brain\` on Windows): persistent settings,
  pre-mount logs
- **Vault root** (your SSD or chosen folder): everything else (pages, db,
  models, cache, logs)
- **HuggingFace cache** (`~/.cache/huggingface/hub`): the bge-m3 download
  is cached here as well as in the vault, so re-mounts on the same host
  skip the network round-trip

### Privacy

BRAIN does **not** phone home. Three network calls exist and are all
opt-in or initiated by the user:

| Call | When | Trigger |
|------|------|---------|
| HuggingFace download | First onboarding (or "Re-download model" in Settings) | User-initiated |
| GitHub Releases (updater) | Every 6 h while the app is open, plus manual click on the version string in the status bar | Configurable; can be disabled by setting `plugins.updater.active` to `false` and rebuilding |
| MCP transport | When a registered LLM client invokes a BRAIN tool | LLM-initiated, stays on `127.0.0.1` |

No telemetry, no analytics, no error reporting beacon.

---

## Building from source

### Toolchain

- Rust stable (pinned in [`rust-toolchain.toml`](rust-toolchain.toml))
- Node ≥ 20.18 (pinned in [`.nvmrc`](.nvmrc))
- pnpm ≥ 9
- Tauri 2 platform prerequisites:
  - Windows: WebView2 Runtime (preinstalled on Windows 11)
  - macOS: Xcode CLI tools
  - Linux: `webkit2gtk-4.1`, `libssl-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`

### Commands

```bash
git clone https://github.com/PPoletto/BRAIN
cd brain
pnpm install

# Dev mode: hot-reload frontend + cargo-watch backend
pnpm tauri dev

# Production build (bundles for current platform)
pnpm tauri build

# Tests
cargo test --workspace
pnpm test

# Lint
cargo clippy --all-targets -- -D warnings
pnpm lint

# Regenerate icons from src-tauri/icons/brain-source.svg
pnpm icons
pnpm tauri icon src-tauri/icons/icon.png
```

### Project layout

```
brain/
├── src/                          # React/TypeScript frontend
│   ├── routes/                   # Bootstrap, onboarding, viewer, settings, history
│   ├── components/               # Shared UI components
│   ├── lib/                      # Tauri command wrappers, stores, events
│   └── styles/                   # Tailwind layer
├── src-tauri/                    # Rust backend
│   ├── src/
│   │   ├── mount/                # Disk detection + mount lifecycle (S01)
│   │   ├── crypto/               # (Roadmap: S02 Cryptomator-format encryption)
│   │   ├── wiki/                 # Git auto-commit, lint, history (S03)
│   │   ├── update/               # Tauri Updater + minisign verify (S04)
│   │   ├── onboarding/           # Wizard backend (S05)
│   │   ├── mcp/                  # MCP server + auto-registration (S06)
│   │   ├── tray/                 # Tray icon, menu, state machine (S07)
│   │   ├── viewer/               # Tree, search, graph backends (S08-S10)
│   │   ├── db/                   # SQLite migrations, FTS5, sqlite-vec
│   │   ├── embedding/            # bge-m3 + hashed fallback
│   │   └── proc.rs               # Subprocess helpers (CREATE_NO_WINDOW on Win)
│   ├── icons/                    # Tray + app icons (regen via pnpm icons)
│   └── tauri.conf.json
├── docs/                         # Architecture, ADRs, implementation plan
├── requirements/                 # NLSpec specs (canonical behavioural source)
└── scripts/                      # Build helpers (icon rendering)
```

---

## Release process

> [!IMPORTANT]
> Before the first GitHub release you need to do three one-time steps:
> generate a minisign keypair, paste the public key into
> `tauri.conf.json`, and (optionally) acquire OS code-signing
> certificates. See [`docs/RELEASE.md`](docs/RELEASE.md) for the
> step-by-step playbook.

### One-time: signing keys

```bash
# Generate a long-lived minisign keypair for the auto-updater
minisign -G -p brain.pub -s brain.key

# Paste the SECOND line of brain.pub (the base64 string after `untrusted comment:`)
# into src-tauri/tauri.conf.json -> plugins.updater.pubkey

# Keep brain.key OFF the repo — store it in your password manager and as a
# GitHub Actions secret named TAURI_SIGNING_PRIVATE_KEY (and the password
# as TAURI_SIGNING_PRIVATE_KEY_PASSWORD).
```

### Per release

1. Bump `version` in `src-tauri/tauri.conf.json` and `package.json`.
2. Update `CHANGELOG.md` with user-facing notes.
3. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
4. CI (see [`.github/workflows/release.yml`](.github/workflows/release.yml))
   builds bundles for Windows, macOS, Linux, signs each with minisign,
   and uploads them plus a `latest.json` to the GitHub Release.
5. Existing BRAIN installs see the update on the next 6-hour check (or
   when the user clicks the version string in the status bar). The
   updater downloads, verifies the minisign signature, installs, and
   restarts.

### Code-signing for OS-level trust

Beyond minisign (which protects the *updater*), Windows SmartScreen and
macOS Gatekeeper want bundles signed with an OS-trusted certificate:

- **Windows**: EV or standard code-signing certificate (DigiCert,
  Sectigo, …). Configure under `bundle.windows.certificateThumbprint`.
- **macOS**: Apple Developer ID certificate, set
  `APPLE_CERTIFICATE` + `APPLE_SIGNING_IDENTITY` env vars. Notarisation
  via `notarytool`.
- **Linux**: not strictly required, but signing the AppImage with `gpg`
  is good practice.

Without these, users see "Unknown publisher" warnings; functionally
nothing breaks.

---

## Roadmap

What's already shipped:

- ✅ Tauri 2 tray app for Windows, macOS, Linux
- ✅ Vault layout, marker file, idempotent re-init
- ✅ Auto-mount on disk detection / saved-folder bootstrap
- ✅ Cross-platform exFAT format with native UAC/admin elevation
- ✅ Wiki auto-commit pipeline with lint, history, restore, hard-reset
- ✅ Hybrid search: FTS5 + sqlite-vec KNN + RRF fusion
- ✅ Real bge-m3 embeddings via candle (XLM-RoBERTa, CLS-pooled, 1024-d)
- ✅ Cytoscape graph view with type/tag/recency filters
- ✅ Dataview-style structured query DSL
- ✅ MCP server (stdio + HTTP) with bearer-token auth and 8 tools
- ✅ Auto-registration in Claude Code, Claude Desktop, Codex,
  Continue.dev, ChatGPT Desktop
- ✅ Tray state machine with 2 s busy-stabilisation
- ✅ Auto-update with minisign verification
- ✅ Launch-at-login on all three OSes

What's next, in rough priority order:

- ⏳ **S02 Encryption** — Cryptomator-compatible vault encryption
  (Argon2id KEK, AES-SIV filenames, AES-GCM content). Multi-device
  support via `.brain/devices.json` outside the Cryptomator vault so a
  stock Cryptomator client can still open the data.
- ⏳ **S06 Cron** — periodic, scheduled MCP-driven refreshes (e.g.
  "every Monday at 8 AM, summarise last week's emails").
- ⏳ **OS code-signing** for installer bundles (currently shipped
  unsigned).
- ⏳ **Vault encryption export** — single-file `.brain` archive for
  off-host backup.
- 🔭 **Mobile MCP companion** — read-only iPhone/iPadOS app that talks
  to BRAIN over Tailscale or a paired USB-C connection.

---

## FAQ

### Is my data sent anywhere?

No. The vault, the index, and the embeddings all stay on your disk. The
only network calls are: HuggingFace model download (one-time, opt-out
available), GitHub release check (every 6 h, can be disabled in build
config), and MCP traffic between BRAIN and your locally-running LLM
client.

### Why "BRAIN" in capitals?

Branding consistency: the volume label is `BRAIN`, the MCP server name is
`BRAIN`, the visible UI strings say `BRAIN`. The binary itself is
`brain` (lowercase) by Unix convention, and the LLM tool names like
`brain_search` follow the snake_case standard.

### Why bge-m3 specifically?

Multilingual (100+ languages, including German), permissive license
(MIT), small enough to ship locally (~2.3 GB), 1024-d dense vectors that
play nicely with sqlite-vec, and backed by a research lab that keeps
publishing improvements. The CLS-pooled output is what FlagEmbedding's
"dense retrieval" mode uses, and we mirror that path.

### Can I use a different model?

The embedding layer is trait-based (`crate::embedding::Embedder`). Swap
`BgeM3Embedder` for any 1024-d model and re-run `pages_index::rebuild`.
If you want a different output dimension, edit `EMBED_DIM` and the
`chunk_vectors` migration in lockstep.

### Why no encryption yet?

Encryption (Spec S02) is on the immediate next-version list. The current
release ships *plaintext* on disk so we can iterate on the rest of the
system without juggling key-management at every checkpoint. If your
threat model needs encryption *today*, host the vault on a
full-disk-encrypted SSD (BitLocker / FileVault / LUKS) — that's a
reasonable interim.

### Where do I get help?

[Open an issue](https://github.com/PPoletto/BRAIN/issues) on GitHub.
This is a one-person project; expect best-effort, not 24/7 support.

---

## License

[MIT](LICENSE) © 2026 Pascal Poletto.

The bundled bge-m3 model weights are released by BAAI under the MIT
License (separate copyright). Tauri, Rust, React, and the rest of the
dependency tree are MIT or Apache-2.0 — both compatible with this
project's MIT.

---

<div align="center">

Built by **Pascal Poletto**.
A personal tool, released under the principle that the best knowledge
systems are the ones you fully own.

</div>

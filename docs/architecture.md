# Brain Client — Architecture (Phase 0, MVP-Scope)

> Phase-0-Output gemäß `CLAUDE.md`. Übersetzt die Specs S01, S03–S10 und
> Constraints in Module, Datenstrukturen, Datenflüsse und externe
> Abhängigkeiten. Implementations-Reihenfolge folgt in
> `docs/implementation-plan.md` (Phase 1).

## Context

Brain ist ein persönliches Wissens-System für eine Einzelperson, bestehend aus
einem portablen Vault auf einer exFAT-SSD (oder lokalem Folder) und einem
Tray-Client (Tauri 2 + Rust + React/TS).

**MVP-Scope-Entscheidungen (Phase 0, fixiert):**

- **S02 (Encryption) ist aufgeschoben.** Der MVP läuft ohne Verschlüsselung at
  rest. C-02 ist temporär ausgesetzt. Die Vault-Struktur enthält einen
  Platzhalter (`encryption.scheme = "none"` in `00_meta/brain-marker.json`),
  damit eine spätere Verschlüsselungs-Migration ohne harten Format-Bruch
  möglich ist.
- **Auto-/Cron-Sync (S06, optionaler Anteil) ist aufgeschoben.** Sync wird
  ausschließlich vom User aus seinem LLM-Frontend (Claude Code, Codex, …) via
  MCP angestoßen. Der Brain-Client startet keinen eigenen
  Headless-LLM-Subprozess.
- **Embedding-Runtime: `candle`** (gemäß CLAUDE.md, reine Rust-Lösung), hinter
  einer Trait-Abstraktion für späteren Wechsel.
- **MCP-Bearer-Token rotiert pro Mount** (256 bit zufällig, beim Unmount
  geleert).

Architektonische Treiber (verbleibend):

- **C-01** Cross-Plattform (macOS, Linux, Windows) verlustfrei austauschbare
  Vaults (exFAT-Disk).
- **C-04** Embedding lokal; Cloud-Routing pro Pfad-Klasse einstellbar (für
  interaktive LLM-Nutzung).
- **C-08/C-10** Keine eigene Chat-UI, MCP als einzige LLM-Schnittstelle.
- **C-15** User-Service-Rechte; keine Root-Operationen ohne OS-Mechanismus.
- **C-16/C-18** Holdouts liegen außerhalb dieses Repos und werden nie gelesen.

---

## 1. Module-Übersicht (Spec → Code-Topologie)

### 1.1 Backend (Rust, `src-tauri/src/`) — MVP

| Modul | Specs | Verantwortung |
|---|---|---|
| `main.rs` | alle | Tauri-Setup, Tray-Bootstrap, Command-/Event-Registrierung, App-State |
| `mount/` | S01, S07 | Volume-Watcher (DiskArbitration / udev / WMI), Vault-Erkennung via Marker, logischer Mount-State-Lifecycle (die exFAT-SSD ist selbst das Filesystem), Folder-Mode-Polling, plattform-spezifischer Mount-Pfad |
| `wiki/` | S03 | git2-Wrapper, Auto-Commit-Debouncer, Lint (Frontmatter / Link-Integrität / ID-Eindeutigkeit), History-API, Restore, Hard-Reset mit zwei Stufen |
| `update/` | S04 | Tauri-Updater-Integration, minisign-Verifikation, Channel-Selection, Skip-Liste |
| `onboarding/` | S05 | Disk-Listing (System-Disks ausblenden), exFAT-Format-Delegation, Vault-Initialisierung (Verzeichnis-Skelett + Marker), Template-Population, Embedding-Modell-Download |
| `mcp/` | S06 | Brain-MCP-Server (stdio + HTTP/Bearer), Tool-Implementations (Search/Read/Context/WritePage/UpsertChunks/…), Auto-Registrierung in Claude Code/Codex/Continue.dev/ChatGPT-Desktop, Open-WebUI-Snippet-Generator |
| `mcp/routing/` | S06 | Per-Pfad-Routing-Policy für interne LLM-Calls (lokal/Anthropic/OpenAI), genutzt z. B. von Lint-Korrektur-Helfern |
| `tray/` | S07 | Tray-State-Machine (disconnected/idle/busy/error), Tooltip-Strings, Active-Operations-Counter mit 2 s-Stabilisierung, Menüstruktur, Eject-Pre-Check |
| `viewer/` | S08–S10 | Backend-APIs für Frontend-Tier-1/2/3: Tree-Listing, Page-Read, FTS5+Vector-Hybrid-Suche, Backlink-Lookup, Graph-Daten |
| `db/` | S03/S06/S08–S10 | SQLite-Connection-Pool, Schema-Migrationen, FTS5-Setup, sqlite-vec-Loader |
| `embedding/` | C-04, S05/S06 | Lokales Embedding (bge-m3, 1024 Dim) via `candle`, hinter Trait `Embedder` |
| `vault/` | übergreifend | Vault-Layout-Konstanten, Marker-Lese/Schreib-Logik, Pfad-Hilfen, Idempotenz-Check |
| `state/` | übergreifend | App-State (Arc<RwLock<…>>), Command-Wiring, Event-Emitter |
| `config/` | S04, S06, S07 | Persistente Settings (Mount-Pfad, Channel, Routing), JSON in OS-Conf-Dir |
| `error.rs` | übergreifend | thiserror-basierte Modul-Fehler + Top-Level-`anyhow` |
| `logging.rs` | übergreifend | tracing-subscriber, Log-File in `06_logs/` und in OS-Conf-Dir bei Disconnected |

### 1.2 Aufgeschoben für spätere Phasen

| Modul | Specs | Status |
|---|---|---|
| `crypto/`, `crypto/keychain/` | S02 | aufgeschoben — Verschlüsselung kommt nach MVP |
| `mount/fs_layer/` | S02 | aufgeschoben — kein FUSE/WinFsp, da MVP-Vault unverschlüsselt direkt vom OS gemountet wird |
| `mcp/sync/` | S06 (Cron-Anteil) | aufgeschoben — Sync nur user-initiiert via interaktives LLM-Frontend |

### 1.3 Frontend (TypeScript + React, `src/`)

| Bereich | Specs | Verantwortung |
|---|---|---|
| `routes/onboarding/` | S05 | Welcome → Medium → Format (bei Disk) → Template-Population → Connectors → Completion (im MVP **kein Master-Passwort-Schritt**) |
| `routes/settings/` | S07 | Settings-Fenster: Mount-Pfad, Channel, MCP-Toggles, Routing, Logs (im MVP **kein „Authorized Devices"-Bereich**) |
| `routes/viewer/` | S08–S10 | Tree-Browser + Reader (Tier 1), Wiki-Links + Backlinks + Search (Tier 2), Graph-Modus (Tier 3) |
| `routes/wiki-history/` | S03 | Commit-Liste, Diff-Anzeige, Restore-Aktion, Hard-Reset-Bestätigungs-Dialog |
| `routes/integrity/` | S07 | Integritäts-Check-UI nach unsauberer Trennung |
| `components/` | übergreifend | Wiederverwendbare UI (DiskList, ProgressBar, MarkdownRenderer, GraphCanvas, …) |
| `lib/commands.ts` | übergreifend | Typsichere Wrapper über `invoke()` für jedes Tauri-Command |
| `lib/events.ts` | übergreifend | Subscriptions auf `mount-state`, `tray-state`, `wiki-changed`, … |
| `lib/state.ts` | übergreifend | Zustand-Store für UI-Zustand; Backend bleibt Source of Truth |
| `styles/` | übergreifend | Tailwind v4 Setup, Tray-Status-Farben (grün/gelb/rot/grau) |

Tray-Menü selbst wird über die Tauri-Tray-API nativ gerendert; das React-Frontend liefert nur die Settings/Viewer/Onboarding-Fenster.

---

## 2. Datenstrukturen

### 2.1 Vault-Layout (auf SSD oder Folder)

Da im MVP keine Verschlüsselung aktiv ist, ist die On-Disk-Sicht gleich der Mount-Sicht:

```
<source>/                          # exFAT-SSD (Volume-Label BRAIN) oder Folder
├── 00_meta/
│   ├── brain-marker.json          # Vault-Identifikation und Format-Versionen
│   ├── AGENTS.md                  # LLM-Konventionen (C-12) — Template aus Onboarding
│   ├── CLAUDE.md                  # idem
│   ├── .mcp.json                  # MCP-Server-Konfig + Bearer-Token-Slot
│   ├── settings-internal.json     # Vault-interne Settings (z. B. Embedding-Modell-Name)
│   └── unclean-shutdown.flag      # Marker für unsaubere Trennung (S07-Recovery-Trigger)
├── 01_raw/                        # Raw-Quellmaterial; NICHT git-versioniert
│   ├── email/
│   ├── confluence/
│   └── …
├── 02_wiki/                       # Markdown-Pages; git-versioniert (S03)
│   ├── .git/
│   ├── .gitignore                 # ignoriert nur Editor-Artefakte
│   ├── entities/
│   ├── concepts/
│   ├── sources/
│   └── topics/
├── 03_db/                         # SQLite + WAL + sqlite-vec; NICHT git-versioniert
│   └── brain.db
├── 04_models/                     # Embedding-Modell-Files; NICHT git-versioniert
│   └── bge-m3/
├── 05_cache/                      # NICHT git-versioniert
└── 06_logs/                       # NICHT git-versioniert
```

`02_wiki/.gitignore` exkludiert nur Editor-Artefakte. Die übrigen Verzeichnisse
liegen außerhalb des Wiki-Git-Repos, daher ist keine globale `.gitignore` nötig.

### 2.2 Vault-Marker — `00_meta/brain-marker.json`

```jsonc
{
  "format": "brain-v1",
  "vault_id": "01HW…ULID",            // generiert beim Init
  "created_at": "2026-04-29T10:00:00Z",
  "client_version": "0.1.0",
  "encryption": {                      // Platzhalter für spätere Verschlüsselungs-Phase
    "scheme": "none",                  // wird später z. B. zu "cryptomator-v8"
    "params": {}
  },
  "embedding_model": "bge-m3"          // C-11: einmal fixiert
}
```

Folder-Detection und Disk-Identifikation prüfen die Existenz dieser Datei und das
Feld `format`. Wenn Verschlüsselung später hinzukommt, wird `encryption.scheme`
gesetzt und das Layout migriert (z. B. zu `cryptomator-v8` mit Wechsel auf das
Cryptomator-Format und einer separaten `.brain/devices.json` für Authorized
Devices).

### 2.3 Datenbank-Schema (`03_db/brain.db`)

```sql
-- Kanonische Page-Metadaten (Sync-Quelle: Filesystem)
CREATE TABLE pages (
    id            TEXT PRIMARY KEY,    -- z.B. 'entities/dan-shapiro'
    type          TEXT NOT NULL,        -- entity|concept|source|topic
    path          TEXT NOT NULL,        -- relativ zu 02_wiki/
    title         TEXT,
    frontmatter   TEXT,                 -- JSON
    body          TEXT,
    updated_at    TEXT,
    file_mtime    INTEGER,
    file_hash     TEXT
);

CREATE INDEX idx_pages_type ON pages(type);
CREATE INDEX idx_pages_updated ON pages(updated_at);

-- Wiki-Link-Graph (S09 Backlinks, S10 Graph)
CREATE TABLE wiki_links (
    src_id  TEXT NOT NULL,
    dst_id  TEXT NOT NULL,
    broken  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (src_id, dst_id),
    FOREIGN KEY (src_id) REFERENCES pages(id) ON DELETE CASCADE
);

CREATE INDEX idx_wiki_links_dst ON wiki_links(dst_id);

-- Tags aus Frontmatter (S10 Filter)
CREATE TABLE page_tags (
    page_id TEXT NOT NULL,
    tag     TEXT NOT NULL,
    PRIMARY KEY (page_id, tag),
    FOREIGN KEY (page_id) REFERENCES pages(id) ON DELETE CASCADE
);

-- Volltext (FTS5)
CREATE VIRTUAL TABLE pages_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    content='pages', content_rowid='rowid'
);

-- Embedding-Chunks
CREATE TABLE chunks (
    id        INTEGER PRIMARY KEY,
    page_id   TEXT NOT NULL,
    chunk_idx INTEGER NOT NULL,
    text      TEXT NOT NULL,
    FOREIGN KEY (page_id) REFERENCES pages(id) ON DELETE CASCADE
);

-- Vektor-Index (sqlite-vec)
CREATE VIRTUAL TABLE chunk_vectors USING vec0(
    embedding float[1024]            -- bge-m3 (C-11 fixiert)
);

-- Connector-State + Raw-Source-Tracking
CREATE TABLE raw_sources (
    id           TEXT PRIMARY KEY,
    connector    TEXT,
    external_id  TEXT,
    raw_path     TEXT,                -- relativ zu 01_raw/
    fetched_at   TEXT,
    page_id      TEXT,
    payload      TEXT
);

CREATE TABLE connector_state (
    connector    TEXT PRIMARY KEY,
    cursor       TEXT,
    last_sync_at TEXT,
    enabled      INTEGER
);

-- Audit-/Event-Log
CREATE TABLE events (
    id      INTEGER PRIMARY KEY,
    ts      TEXT NOT NULL,
    kind    TEXT NOT NULL,           -- ingest|mount|unmount|commit|sync|error|…
    payload TEXT
);

-- Schema-Migration
CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
```

WAL-Modus aktiv; Checkpoint vor Unmount.

### 2.4 Tray-State-Maschine

```
┌──────────────┐  source detected  ┌──────────────┐
│ disconnected │ ────────────────► │ mounting     │
└──────────────┘                   └──────┬───────┘
       ▲                                  │ marker valid
       │ unmount complete                 ▼
       │                          ┌──────────────┐
       │                          │ mounted-busy │ ◄──┐ active op start
       │ unmount                  └──────┬───────┘    │
       │                                 │ idle 2s    │
       │                                 ▼            │
       │                          ┌──────────────┐    │
       └────────────────────────  │ mounted-idle │ ───┘
                                  └──────────────┘
              any failure       ┌──────────────┐
        ────────────────────────►│    error     │
                                 └──────────────┘
```

Active-Operations-Counter (atomarer u32) wird von Wiki-Mutationen,
DB-Schreibvorgängen, Embedding-Jobs, Git-Commits, MCP-Tool-Calls erhöht.
Idle-Übergang nach 2 s ohne Aktivität.

### 2.5 MCP-Server-Konfig — `00_meta/.mcp.json`

```jsonc
{
  "version": 1,
  "brain": {                                // Brain-eigener MCP-Server
    "transports": ["stdio", "http"],
    "http": {
      "host": "127.0.0.1",
      "port_strategy": "first-free-from-7137",
      "bearer_token": "<random 256 bit, beim Mount neu rotiert; nach Unmount geleert>"
    }
  },
  "external_servers": [                     // Connectors (MS365, Atlassian, HubSpot…)
    {
      "id": "outlook",
      "command": "…",
      "args": [],
      "env": {},
      "enabled": true,
      "scopes": ["mail.read"]
    }
  ],
  "internal_routing": {
    "default_provider": "local",            // local|anthropic|openai
    "rules": [
      { "path_prefix": "01_raw/email/personal/", "provider": "local" },
      { "path_prefix": "01_raw/email/work/",     "provider": "anthropic" }
    ]
  }
}
```

---

## 3. Datenflüsse

### 3.1 Disk-Mount (S01)

```
Disk-Subsystem (DiskArbitration | udev | WMI)
   │ "volume label=BRAIN appeared at /Volumes/BRAIN"
   ▼
mount/watcher.rs
   │ 00_meta/brain-marker.json existiert + format=brain-v1 + encryption.scheme=none?
   │ ─ nein → ignorieren (keine MVP-Brain-Source)
   ▼
db/ + embedding/                       (init lazy)
   │ SQLite open, FTS5 + sqlite-vec geladen
   │ candle-Modell aus 04_models/bge-m3/ laden
   ▼
mcp/                                   (Server start)
   │ Bearer-Token rotiert, .mcp.json geschrieben
   │ Auto-Registrierung in Claude Code / Codex / Continue.dev / ChatGPT-Desktop
   ▼
tray/  → mounted-idle  + emit("mount-state", …)

Latenz-Budget: Anschluss → mounted-idle ≤ 5 s.
```

Folder-Mode läuft analog, aber statt Volume-Detection per Polling auf das im
Settings registrierte Verzeichnis.

### 3.2 Wiki-Mutation → Auto-Commit (S03)

```
notify-Watcher auf 02_wiki/
   │ Debounce 5 s
   ▼
wiki/lint.rs
   │ Frontmatter (serde_yaml), Wiki-Links (parser), ID-Eindeutigkeit
   │ ─ Hartfehler? → tray-Notification, kein Commit, Lint-Resultat in events-Tabelle
   ▼
git2 add . && commit
   │ Message: "<source>: N pages, <top-paths> [trigger=<edit|mcp|ingest>]"
   ▼
db/ inkrementelle Sync (pages + wiki_links + page_tags + chunks)
   │ Embeddings nur für veränderte Chunks
   ▼
emit("wiki-changed", …)  → Frontend-Tree refresh
```

Restore und Hard-Reset laufen über dieselbe Pipeline; Commit-Typ in der Message.

### 3.3 Auto-Update (S04)

```
on_startup OR schedule(every N hours) OR user click
   ▼
update/check.rs  → GitHub Releases API (öffentlich)
   │ Channel-Filter (stable|beta), skip-list anwenden
   ▼
Tauri-Updater download Bundle + .minisig
   ▼
update/verify.rs  minisign-verify(Bundle, Bundle.minisig, EMBEDDED_PUBLIC_KEY)
   │ ─ Fehlschlag → tray-Status, log, kein Install
   ▼
UI-Prompt (jetzt | später | überspringen)
   ▼
Tauri-Updater install + restart  (Vault-Daten unangetastet)
```

### 3.4 Sync (user-initiiert via interaktives LLM, S06)

```
User in Claude Code / Codex / ChatGPT mit Brain-MCP verbunden
   │ "Zieh meine neuen Outlook-Mails und lege Source-Pages an"
   ▼
LLM-Frontend ruft via MCP:
   │ outlook.fetch_messages_since(cursor)        (User-aktivierter Connector-MCP)
   │ brain.write_raw_file(path, payload)         (Brain-MCP)
   │ brain.write_page(id, frontmatter, body)     (Brain-MCP)
   │ brain.upsert_chunks(page_id, [...])         (Brain-MCP)
   │ Active-Operation +1 pro Tool-Call
   ▼
Auto-Commit-Pipeline (3.2)
   ▼
tray bleibt mounted-busy bis Idle-Stabilisierung greift
```

Im MVP gibt es keinen Headless-LLM-Subprozess und keinen Cron-Sync-Job.
Periodische Sync-Funktion ist für eine spätere Phase vorgesehen.

### 3.5 Read-Browser (S08–S10)

- **Tier 1**: `viewer/tree.rs` listet Filesystem; Frontend rendert via unified+remark+rehype.
- **Tier 2**: `viewer/search.rs` Hybrid-Suche (FTS5-BM25 + sqlite-vec-Cosine, lineare Kombination); `viewer/backlinks.rs` Lookup via `wiki_links`.
- **Tier 3**: `viewer/graph.rs` liefert {nodes, edges} mit Filter-Parametern; Frontend rendert via Cytoscape.js + fcose-Layout. Sub-Graph-Modus auf 1- oder 2-Hop-Nachbarschaft.

### 3.6 Eject mit Pre-Check (S07)

```
User → Tray „Eject Brain"
   ▼
Active-Op-Counter > 0?
   │ ja → Dialog (warten | force | abbrechen)
   ▼
mount/unmount.rs:
   │ Subroutinen abbrechen
   │ DB-WAL-Checkpoint, SQLite close
   │ Git-Repo-Flush
   │ MCP-Server stoppen, MCP-Client-Configs aufräumen
   │ Bearer-Slot in .mcp.json leeren
   ▼
unclean-shutdown.flag setzen (bei „force") oder löschen (bei sauberer Trennung)
   ▼
emit("mount-state", "disconnected")
```

---

## 4. Externe Abhängigkeiten

### 4.1 Rust-Crates (MVP-Scope)

| Crate | Zweck | Begründung |
|---|---|---|
| `tauri` (2.x) | Desktop-Shell | Vorgabe CLAUDE.md |
| `tauri-plugin-fs`, `tauri-plugin-dialog`, `tauri-plugin-shell`, `tauri-plugin-notification`, `tauri-plugin-updater`, `tauri-plugin-store` | OS-Integration, Updater, Settings | Tauri-2-Standard |
| `tokio` | Async-Runtime | Standard für I/O in Rust |
| `serde`, `serde_json`, `serde_yaml` | Konfig, Frontmatter | Standard |
| `thiserror`, `anyhow` | Error-Typen | Modul-Fehler / Top-Level |
| `tracing`, `tracing-subscriber`, `tracing-appender` | Logs | Strukturiert, async-tauglich |
| `rusqlite` (mit `bundled` + `fts5`) | SQLite | Bundled SQLite garantiert FTS5 |
| `sqlite-vec` | Vektor-Index | Vorgabe CLAUDE.md |
| `git2` (libgit2) | Wiki-Versionierung | Vorgabe CLAUDE.md |
| `notify` | Filesystem-Watcher | Cross-Plattform |
| `minisign-verify` | Signaturprüfung | Vorgabe CLAUDE.md |
| `directories` | Plattform-Pfade | Standard |
| `sysinfo` | Disk-/System-Info | Onboarding-Disk-List |
| `pulldown-cmark` | Markdown-Parsing für Lint/Index | Schnell, etabliert |
| `regex`, `unicode-segmentation` | Wiki-Link-Parser, Tokenisierung | Hilfsmittel |
| `ulid` oder `uuid` | IDs | Standard |
| `candle-core`, `candle-transformers`, `tokenizers` | Embedding-Inferenz (bge-m3) | Vorgabe CLAUDE.md |
| Plattform-Crates: `core-foundation`, `disk-arbitration` (macOS), `udev` (Linux), `windows`-Crate (Windows) | Disk-Detection | OS-spezifisch |

**Aufgeschoben** (für die spätere Verschlüsselungs- und Sync-Phase):
`argon2`, `aes-gcm`, `aes-siv`, `hkdf`, `sha2`, `zeroize`, `keyring`, `fuser`,
`winfsp`/`winfsp-rs`.

### 4.2 Frontend-Pakete

| Paket | Zweck |
|---|---|
| `react`, `react-dom` | Frontend-Framework |
| `react-router-dom@6` | Routing (Vorgabe) |
| `@tauri-apps/api`, `@tauri-apps/plugin-*` | Backend-Bridge |
| `vite`, `@vitejs/plugin-react`, `typescript` | Build/Dev |
| `tailwindcss@4` | Styling (Vorgabe) |
| `unified`, `remark-parse`, `remark-rehype`, `remark-gfm`, `remark-wiki-link`, `rehype-stringify`, `rehype-highlight`, `rehype-katex` | Markdown-Pipeline (S08–S09) |
| `cytoscape`, `cytoscape-fcose` | Graph (S10) |
| `zustand` | Frontend-State |
| `vitest`, `@testing-library/react`, `@testing-library/jest-dom` | Tests |
| `eslint`, `@typescript-eslint/*`, `prettier` | Lint/Format |

### 4.3 Build-Werkzeuge & Toolchain

- `pnpm >= 9` (Vorgabe)
- Rust stable per `rust-toolchain.toml`
- Node `>=20.18` per `.nvmrc`
- Plattform-Voraussetzungen für Tauri 2 (WebView2 / WebKitGTK / Cocoa)
- **Keine FUSE/WinFsp-Prereqs im MVP.** Die exFAT-SSD wird vom OS direkt gemountet.

---

## 5. Architecture Decision Records (Stubs)

In `docs/adr/` zu erstellen, parallel zu den ersten Spec-Slices in Phase 1:

- **ADR-0001** — MVP ohne Verschlüsselung; Platzhalter im `brain-marker.json`;
  spätere Migration zu Cryptomator-kompatiblem Format als eigene Phase.
- **ADR-0002** — Embedding-Runtime: `candle` mit Trait-Abstraktion in
  `embedding/`, damit ein späterer Wechsel (z. B. zu `ort`) machbar bleibt.
- **ADR-0003** — IPC-Pattern: typisierte Tauri-Commands + namespacierte
  Tauri-Events; kein WebSocket nach innen.
- **ADR-0004** — Sync ist im MVP rein user-initiiert via interaktives
  LLM-Frontend über MCP. Cron-/Headless-Sync ist eine spätere Phase.
- **ADR-0005** — Tier-3-Graph mit Cytoscape.js + fcose-Layout statt D3-Force.
- **ADR-0006** — MCP-HTTP-Bearer-Token rotiert pro Mount; nach Unmount wird der
  Slot in `.mcp.json` geleert.

---

## 6. Test-Strategie (Übersicht; Detail in Phase 1)

- **Rust-Unit-Tests** pro Modul in `#[cfg(test)]`-Sektionen — z. B. Lint-Regeln,
  Tray-State-Übergänge, Marker-Parsing, Vault-Idempotenz, MCP-Tool-Roundtrips,
  Embedder-Ausgabe-Form.
- **Rust-Integration-Tests** in `src-tauri/tests/` mit temporären
  Vault-Verzeichnissen.
- **Frontend-Unit-Tests** mit `vitest` für Markdown-Renderer, Wiki-Link-Klick,
  State-Reducer.
- **Lint-Suite**: `cargo clippy --all-targets -- -D warnings`, `pnpm lint`.
- **Audit**: `cargo audit`, `pnpm audit`.
- **Holdouts** sind separat (C-16/C-18) und nie Bestandteil dieses Repos.

---

## 7. Modul-Abhängigkeitsgraph (MVP)

```
              ┌─────────────────────────┐
              │   tauri-main (main.rs)  │
              └────┬───────────┬────────┘
                   │           │
        ┌──────────┘           └──────────┐
        ▼                                 ▼
   ┌─────────┐                       ┌─────────┐
   │  tray   │                       │ onboard │
   └────┬────┘                       └────┬────┘
        │                                 │
        ▼                                 ▼
   ┌─────────┐                       ┌──────────┐
   │  mount  │ ──────────────────────►│  vault   │
   └────┬────┘                       └────┬─────┘
        │                                 │
        ▼                                 │
   ┌─────────┐    ┌─────────┐             │
   │   db    │ ◄──│ wiki    │ ◄───────────┘
   └────┬────┘    └────┬────┘
        │              │
        ▼              ▼
   ┌─────────┐    ┌─────────┐
   │embedding│    │   mcp   │
   └─────────┘    └────┬────┘
                       ▼
                  ┌─────────┐
                  │ viewer  │
                  └─────────┘
```

`update/`, `config/`, `state/`, `error.rs`, `logging.rs` sind quer-eingehängt
und nicht im Graph. `crypto/`, `mount/fs_layer/`, `mcp/sync/` sind aus dem
MVP-Graph entfernt (siehe Abschnitt 1.2).

---

## 8. Offene Fragen — Phase 0 (alle gelöst, Stand 29.04.2026)

| Q | Entscheidung |
|---|---|
| Q1 — Verschlüsselung | aufgeschoben; Platzhalter im Marker (`encryption.scheme="none"`) |
| Q2 — FS-Layer-Prereqs | entfällt durch Q1 |
| Q3 — Embedding-Runtime | `candle` (CLAUDE.md-Default), Trait-Abstraktion |
| Q4 — MCP-Bearer-Lebensdauer | Rotation pro Mount, geleert beim Unmount |
| Q5 — Sync-CLI / Cron | aufgeschoben; Sync nur user-initiiert via LLM-Frontend |

Eine kleine offene Frage für Phase 1 (**nicht Phase-0-blockierend**):

- Default für `internal_routing.default_provider`? Vorschlag: `local`, damit
  ohne API-Key alles auf dem Host bleibt. Anthropic/OpenAI sind opt-in, sobald
  der User in den Settings einen API-Key hinterlegt. Klärung beim Bau von S06.

---

## 9. Verifikation der Architektur

Diese Phase 0 hat keinen Code-Output. Die Verifikation läuft über Review:

1. Du liest dieses Dokument vollständig.
2. Bei „abgenommen" wird `docs/implementation-plan.md` (Phase 1) auf Basis
   dieser Architektur erstellt — mit Spec-für-Spec-Reihenfolge, Aufwand,
   Test-Strategie.
3. **MVP-Spec-Reihenfolge** (S02 raus, S06-Cron-Anteil reduziert):
   **S05 (Onboarding) → S01 (Mount) → S03 (Wiki/Git) → S07 (Tray) → S06 (MCP, ohne Cron) → S04 (Update) → S08 → S09 → S10**.
4. Erst danach beginnt eine Vertical Slice (S05 als erste Spec gemäß CLAUDE.md).

Bis dahin: keine Code-Änderungen.

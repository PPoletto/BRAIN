# Brain — Post-MVP Roadmap

> Aufbauend auf [docs/architecture.md](./architecture.md) und [docs/implementation-plan.md](./implementation-plan.md).
> Schließt die Lücken aus dem Karpathy-/pgvector-/Second-Brain-Vergleich (siehe Notizen vom 2026-04-30).

## Executive Summary

| # | Milestone | Liefert | Aufwand | Hängt ab von |
|---|---|---|---|---|
| **M1** | Frontend-UX-Refactor | Konsistenter App-Shell, responsive Layout, keine abgeschnittenen Boxen, volle Flächen-Nutzung | **L** (~5–7 Tage) | — |
| **M2** | Semantic Embeddings via `candle` + `bge-m3` | Echte semantische Suche statt Hash-Dummy | **L** (~5–8 Tage) | — |
| **M3** | `sqlite-vec` Extension + KNN | Sub-100 ms Vektor-Queries auf zehntausenden Chunks | **M** (~3 Tage) | M2 |
| **M4** | Auto-generiertes `index.md` + `log.md` | Mensch-lesbarer Katalog & Audit-Trail (Karpathy-Style) | **S** (~2 Tage) | — |
| **M5** | Dataview-style Frontmatter-Queries | „Alle Pages mit `tag: customer`, `updated > 30d`" als MCP-Tool + UI | **M** (~3 Tage) | M1 |

**Gesamt:** ~18–23 Arbeitstage. Nicht zwingend in einem Stück — die Reihenfolge ist so, dass nach jedem Milestone ein **release-fähiger Zwischenstand** existiert.

**Sequenzbegründung:** M1 zuerst, weil die App heute akut user-unfreundlich ist und alle nachfolgenden Features (besonders die Query-UI in M5) auf einem polierten Shell aufsetzen sollen. M2+M3 freischalten den größten Datenwert (semantische Suche). M4 ist niedrigaufwändig und kompatibel zu allem. M5 macht zuletzt die polierte UI sichtbar wertvoll.

> **Schema-Flexibilität bewusst aufgeschoben.** Das ursprüngliche M5 (User-definierbare Page-Typen mit UI-Wizard und Migrations-Tool) ist gestrichen. Begründung siehe [Anhang A](#anhang-a--warum-keine-schema-flexibilität).

---

## M1 — Frontend-UX-Refactor

### Was es bringt

- **App-Shell** mit konsistenter Top-Bar, Sidebar und Status-Bar, die in allen mounted-Routes gleich aussehen — der User muss nicht mehr raten, wie er navigiert
- **Responsive Layout** für 1366×768 bis 2560×1440 (Tauri-MinSize 800×600 als untere Grenze) — keine abgeschnittenen Boxen, keine sinnlos schmalen Center-Cards auf Ultrawides
- **Volle Flächen-Nutzung**: Wizard und Settings expandieren bis 5xl statt steifem `max-w-2xl`; Viewer-Splits nutzen den Resize-Bereich aus
- **Tab-Navigation** in Settings statt 1500 px Single-Page-Scroll
- **Status-Bar** unten mit Vault-Pfad / Active-Ops / MCP-Status — überall sichtbar, immer-aktuelle Wahrheit
- **Toast-System** für transiente Feedbacks (MCP re-registriert, Auto-Commit fertig, Lint-Fehler) statt versteckter Console-Logs
- **Konsistente Component-Library** (Button-Varianten, EmptyState, ErrorBanner, KeyboardShortcuts) — Boilerplate verschwindet aus den Routes

### Architektur-Entscheidungen

- **Layout-Hierarchie:**
  ```
  RouterProvider
   └─ <AppShell>                  (zeigt-sich-wenn-mounted)
       ├─ <TopBar>                Breadcrumb + Global-Search + Mount-Status-Pill
       ├─ <Sidebar>               Collapsible Nav (Browse / Search / Graph / History / Settings)
       ├─ <main><Outlet/></main>
       └─ <StatusBar>             Vault-Pfad · Active-Ops · MCP-Connected · Last-Commit
  ```
  Onboarding und Bootstrap haben **eigenes** minimales Layout (kein Shell), weil sie pre-mount laufen.
- **Tailwind-Theme** wird erweitert um Status-Farb-Tokens (`brain.idle`, `brain.busy`, `brain.error`, `brain.disconnected`), Spacing-Scale (`shell-padding`, `panel-gap`) und `data-[state=…]`-Variants für State-driven Styling.
- **Resizable Splits** in Tier-1 und Tier-2 (Sidebar-Width per `react-resizable-panels` oder eigener kleiner Drag-Handle) — die 280-px-fix-Spalte schneidet bei langen Page-IDs ab.
- **Tab-Layout in Settings** mit URL-Sync (`/settings/general`, `/settings/connectors`, `/settings/mcp`, `/settings/memory`, `/settings/danger`) — direkter Deep-Link aus Tray-Eintrag möglich.
- **Keyboard-Shortcuts** (Cmd/Ctrl-K für globale Search, Cmd/Ctrl-, für Settings, Cmd/Ctrl-H für History, Cmd/Ctrl-G für Graph, Cmd/Ctrl-1/2/3 für Tier-1/2/3) — über kleine `useGlobalShortcuts`-Hook.
- **Tray-Status-Banner**: wenn Mount = `error` oder `mounted-busy` länger als 30 s, erscheint ein gelber/roter Banner direkt unter der Top-Bar mit Erklärung + Action-Button.

### Konkrete Refactor-Punkte pro Route

| Route | Heute (Problem) | Nach M1 |
|---|---|---|
| **Bootstrap** | `max-w-md`-Card mittig, sehr klein auf großen Screens | Vollbild-Skeleton mit Logo + Spinner; bei `last_known_vault_missing` größere Card mit Action-Buttons |
| **Onboarding-Welcome** | Zwei Buttons in `max-w-2xl`-Card | `max-w-3xl` auf 1080p, `max-w-5xl` auf 4K; Buttons als horizontale 2-col-grid mit Icons + Untertext |
| **Onboarding-Medium** | Dropdown + Liste, schmale Card | Volle Shell-Breite; Disk-Liste in 2-col-grid auf >1200 px; größere Klick-Targets; Hint-Banner für "Nicht alle Disks sichtbar?" |
| **Onboarding-Format** | Bestätigungs-Dialog in schmaler Card | Größerer Dialog mit klar getrennten Disk-Info-/Warning-Bereichen, Loading-State während Format läuft |
| **Onboarding-Template-Population** | Vertikale Step-Liste mit Spinner | Stepper-Komponente horizontal; pro Step Detail-Hint („Embedding-Modell wird geladen…") |
| **Onboarding-Connectors** | Liste mit Toggle-Buttons | Card-Grid mit Icon, Beschreibung, Verbinden-Button — wie ein App-Store |
| **Onboarding-Completion** | Zwei Quick-Action-Buttons + Status-Box | Zentriertes Hero mit Vault-Path, Status-Karten für jeden Client (✓ Claude Code, ✓ Claude Desktop, …), Restart-Hinweis prominent |
| **Tier-1 Viewer** | 280 px fixe Sidebar; Page-IDs schneiden ab; kein Welcome-State | Resizable Sidebar 240–480 px, Truncate + Tooltip auf Page-IDs, Welcome-State („Pick a page to start") |
| **Tier-2 Viewer** | Search-Bar oben, Results-Liste, Backlinks rechts | Search prominent oben (mit Cmd-K-Hint), Results full-width mit Score-Badges, Backlinks in einklappbarer Card |
| **Tier-3 Viewer** | Filter-Sidebar links, Graph rechts | Filter zur Toolbar oben (Type + Tag + Date in einer Zeile), Graph nutzt volle Höhe |
| **Wiki-History** | Flache Liste | Vertikale Timeline mit Commit-Bubbles, Diff-Hint pro Commit, Filter-Bar oben |
| **Settings** | Eine Long-Page mit ~6 Sektionen | Tabs: General · Connectors · MCP · Memory Mode · Danger zone |
| **Integrity** | OK aber visuell uneinheitlich | An die neuen Card-Patterns angepasst |

### Module / Files

```
src/components/shell/
├── AppShell.tsx              # NEU: Layout-Wrapper für mounted-Routes
├── TopBar.tsx                # NEU
├── Sidebar.tsx               # NEU
├── StatusBar.tsx             # NEU
├── BrainBranding.tsx         # NEU: Logo + Tagline
└── MountStatusPill.tsx       # NEU: kleine farbige Anzeige in Top-Bar

src/components/ui/            # gemeinsame UI-Primitives
├── Button.tsx                # NEU: variant=primary|secondary|destructive|ghost
├── Card.tsx                  # NEU
├── Tabs.tsx                  # NEU: ariaroles + keyboard nav
├── EmptyState.tsx            # NEU
├── Toast.tsx                 # NEU + ToastProvider
├── KeyboardShortcuts.tsx     # NEU + useGlobalShortcuts hook
├── ResizableSplit.tsx        # NEU: Drag-Handle für Sidebar-Resize
└── ErrorBanner.tsx           # NEU

src/styles/
├── globals.css               # erweitert: Status-Farb-Tokens, Component-Layer
└── theme.ts                  # NEU: Tailwind v4 @theme-Block

src/routes/                   # alle bestehenden Routes refactored
├── Bootstrap.tsx
├── onboarding/*.tsx
├── viewer/*.tsx
├── settings/Settings.tsx
├── wiki-history/WikiHistory.tsx
└── integrity/Integrity.tsx
```

### Tests

- Vitest + @testing-library:
  - `AppShell renders TopBar, Sidebar, Outlet, StatusBar in correct slots`
  - `Settings tabs sync with URL on click and on direct navigation`
  - `useGlobalShortcuts fires the registered handler when shortcut pressed`
  - `Toast auto-dismisses after configured timeout`
  - `EmptyState renders custom action when provided`
- Playwright (neu hinzugefügt) oder manueller Checklist:
  - 1366×768: Sidebar collapsed by default, alle Routes lesbar ohne horizontalen Scroll
  - 1920×1080: Standard-Layout
  - 2560×1440: keine über-stretched Inputs, Maxwidth-Container halten Lesbarkeit
- Visual-Regression-Snapshots: `pnpm test:visual` schießt PNGs für jede Route in 3 Viewports und committet sie

### Risiken & Mitigations

| Risiko | Mitigation |
|---|---|
| Großer Refactor → Bug-Surface über alle Routes | Schritt-für-Schritt: Phase 1 = Shell-Components in Isolation testen; Phase 2 = eine Route nach der anderen migrieren, manuell testen, mergen |
| Resizable-Splits konflikten mit Browser-Drag-Events | `react-resizable-panels` (gepflegt, ~10 KB) statt Eigenbau; deckt Edge-Cases ab |
| Keyboard-Shortcuts überschreiben native (Cmd-W, Cmd-Q) | Nur ungenutzte Shortcuts (Cmd-K, Cmd-Comma, Cmd-1/2/3); Doku in Settings → Tastatur |
| Tailwind v4 ist Beta — Theme-API könnte sich ändern | Wir nutzen schon v4-Beta; bei Major-Update sauber migrieren |
| Visual-Regression-Tests sind brüchig | Optional, nicht Pflicht; Mindestanspruch ist Vitest-Unit-Tests + manuelle Screenshot-Review |

### Aufwand

**L — 5–7 Tage:**
- 1 Tag App-Shell + StatusBar + Sidebar
- 1 Tag UI-Primitives (Button, Card, Tabs, Toast, EmptyState)
- 1 Tag Settings auf Tabs + URL-Sync
- 1 Tag Onboarding-Refactor (alle 6 Steps + Bootstrap)
- 1 Tag Viewer Tier-1/2/3 mit Resizable + Welcome-State
- 1 Tag Wiki-History Timeline + Integrity Polish + Keyboard-Shortcuts
- 1 Tag Tests + Polish + Cross-Viewport-Review

---

## M2 — Semantic Embeddings via `candle` + `bge-m3`

### Was es bringt

- **Semantische Suche**: „Pages über Compliance-Audits" findet auch Pages, die das Wort selbst nicht enthalten, aber inhaltlich nah liegen
- Ersetzt den `HashedEmbedder`, der heute nur deterministisches Rauschen produziert (Stand-in für die Pipeline)
- Erlaubt das versprochene **Hybrid-Scoring** (BM25 + Cosine-Distance, linear kombiniert)
- Ist die einzige der fünf Erweiterungen, die echte „Karpathy-at-scale"-Tauglichkeit liefert (Karpathy: „at small scale (~100 sources), index files suffice without vector embeddings")

### Architektur-Entscheidungen

- **Modell:** `BAAI/bge-m3` — 1024 Dim, multilingual (DE/EN/etc.), Apache-2.0
- **Runtime:** `candle-core` + `candle-transformers` (XLM-RoBERTa-Backend); reine Rust-Lösung, keine Python-Dependency
- **Tokenizer:** `tokenizers`-Crate (HuggingFace), bge-m3 nutzt SentencePiece-Variante
- **Modell-Storage:** `04_models/bge-m3/` im Vault — die Safetensors liegen lokal, kein Cloud-Download zur Inferenz-Zeit
- **Modell-Download:** beim ersten Vault-Init über HuggingFace Hub API (`hf-hub`-Crate). ~2 GB single-shot, anschließend offline einsatzfähig
- **Inferenz-Strategie:**
  - **Cold start:** Modell-Load erfolgt asynchron beim Mount (im Hintergrund-Thread des `bootstrap_app`)
  - **Batching:** Beim Index-Rebuild werden Chunks in Batches à 16 verarbeitet
  - **CPU-only:** Erste Version, keine CUDA-Detection — bge-m3 auf CPU schafft ~50–100 ms pro Chunk auf modernem Hardware
- **Chunking:** bestehender `chunker::chunks` bleibt, target ~300 Tokens pro Chunk
- **Hybrid-Scoring:** `score = α · normalize(bm25) + (1-α) · cosine_similarity`, default α=0.5, konfigurierbar pro Query

### Module / Files

```
src-tauri/src/embedding/
├── mod.rs                    # Embedder-Trait (existiert), neu: BgeM3Embedder
├── bge_m3.rs                 # NEU: candle-Implementation, lazy-init, batch-inference
├── chunk.rs                  # bestehend, evtl. tokenizer-aware chunking
├── hashed.rs                 # bestehend, bleibt als Fallback für Tests
└── download.rs               # NEU: hf-hub-basiertes Modell-Fetching mit Progress-Events

src-tauri/src/viewer/search.rs
└── search_with_db()          # Erweitert um Hybrid-Scoring + Vector-Sub-Query
```

### Tests

- `bge_m3::tests::embeds_short_text_to_1024_dim_vector` (mit Mini-Fixture-Modell)
- `bge_m3::tests::two_paraphrases_have_higher_cosine_than_unrelated_pair`
- `download::tests::resumes_partial_download` (Mock HTTP)
- `search::tests::hybrid_outscores_pure_lexical_for_paraphrase_query`

### Risiken & Mitigations

| Risiko | Mitigation |
|---|---|
| Cold-Start 3–5 s Modell-Load blockt erste Suche | Pre-Warm im Hintergrund-Thread bei Mount, lazy-init mit `OnceCell` |
| 2 GB Modell-Footprint auf SSD | Modell als optional-Feature: User-Opt-in im Onboarding („Enable semantic search?") — Lexical-Only läuft ohne |
| CPU-Inferenz langsam bei Index-Rebuild über tausende Chunks | Inkrementell: nur Chunks mit veränderten Quell-Hash neu embedden (Hash bereits in `chunks.text`) |
| `candle` API unter Iteration → Build-Brüche bei Updates | Feste Version pinnen, regelmäßig dependabot-Update prüfen |

### Aufwand

**L — 5–8 Tage**: 1 Tag Modell-Download, 2 Tage candle-Integration + Inferenz, 1 Tag Pipeline-Wiring, 1 Tag Hybrid-Scoring, 1–3 Tage Tests + Performance-Tuning

---

## M3 — `sqlite-vec` Extension + KNN-Queries

### Was es bringt

- KNN-Queries auf den von M2 erzeugten Embeddings in **Sub-100 ms** statt linearer Vec-Scan
- Ersetzt das aktuell ungenutzte `chunks.embedding` BLOB durch eine vec0-Virtual-Table mit nativem Index
- Skaliert auf Hunderttausende Chunks ohne UX-Degradation

### Architektur-Entscheidungen

- **Extension-Loading:** beim `DbHandle::open` per `conn.load_extension("sqlite-vec", None)`
- **Distribution:** `sqlite-vec` als platform-spezifische Binärdatei in `src-tauri/resources/sqlite-vec/{win,mac,linux}` ausgeliefert; `tauri.conf.json` `bundle.resources` deklariert sie
- **Schema-Migration v3:**
  - `chunk_vectors` als Virtual-Table `vec0(embedding float[1024])`
  - `chunks.embedding`-Spalte bleibt als Source-of-Truth (für Re-Indexing nach Schema-Änderung), `chunk_vectors` wird daraus gespiegelt
  - `chunk_id` ist `rowid` in beiden Tabellen — JOIN ist trivial
- **Query-API:** `search_with_db` erweitert um `vector_top_k(query_embedding, k=20)` als Sub-Query, der KNN-Kandidaten liefert
- **Hybrid-Algorithmus:** Reziproker Rank-Fusion (RRF) statt linear-weighted, weil RRF robust ggü. Score-Skala-Unterschieden ist:
  ```
  rrf_score = Σ 1 / (60 + rank_in_source)
  ```

### Module / Files

```
src-tauri/src/db/
├── migrations.rs             # NEU: v3 = ALTER für chunk_vectors vec0
├── pages_index.rs            # erweitert: schreibt parallel in chunk_vectors
└── vec_loader.rs             # NEU: platform-spezifischer Pfad zur Extension

src-tauri/src/viewer/search.rs
└── search_with_db()          # Sub-Query gegen chunk_vectors via vec_search()

src-tauri/resources/sqlite-vec/
├── windows/sqlite-vec.dll    # geladen via include_bytes! ins Binary
├── macos/sqlite-vec.dylib    # ggf. universal-binary
└── linux/sqlite-vec.so
```

### Tests

- `migrations::tests::apply_v3_creates_chunk_vectors_virtual_table`
- `vec_loader::tests::sqlite_vec_extension_loads_successfully` (smoke)
- `search::tests::knn_returns_results_in_subsecond_for_10k_chunks` (perf-Bench)
- `search::tests::rrf_ranks_paraphrase_above_unrelated_lexical_match`

### Risiken & Mitigations

| Risiko | Mitigation |
|---|---|
| Extension-Loading per Plattform unterschiedlich, Pfad-Detection | Bundling in `src-tauri/resources/`, zur Laufzeit per `app.path().resource_dir()` lokalisiert |
| `sqlite-vec` ist jung (v0.1.x), Breaking-Changes möglich | Version pinnen + smoke-Test bei jedem Build |
| Binary-Größe wächst um ~5 MB pro Plattform | akzeptabel; Brain-Bundle bereits >100 MB durch Embedding-Modell |
| Cross-Plattform-CI muss alle drei Binaries bauen | Build-Step in CI klarmachen oder vendored sqlite-vec aus Source kompilieren |

### Aufwand

**M — 3 Tage**: 1 Tag Extension-Bundling, 1 Tag Migration + Pipeline, 1 Tag RRF + Tests

---

## M4 — Auto-generiertes `index.md` + `log.md`

### Was es bringt

- Karpathy-konformer **menschen-lesbarer Catalog** (`00_meta/index.md`) und **Append-only-Logbook** (`00_meta/log.md`)
- Browse-bar in Obsidian, VS Code, jedem Markdown-Editor — ohne SQL-Kenntnis
- Diff-bar in Git: jeder Ingest erzeugt einen lesbaren Log-Eintrag, der mit dem Auto-Commit gemeinsam committet wird
- Dient gleichzeitig als **Backup-Sicht** falls die SQLite-DB korrupt würde — der Vault rekonstruiert sich aus den Markdown-Files

### Architektur-Entscheidungen

- **Wo geschrieben:** `00_meta/index.md` und `00_meta/log.md` — **außerhalb von 02_wiki/**, daher **nicht git-versioniert** im Wiki-Repo. Stattdessen separates Side-Repo? Nein: einfacher als zwei Snapshot-Files unter `00_meta/`, und der Wiki-Watcher ignoriert dieses Verzeichnis (kein Auto-Commit-Loop)
- **Wann aktualisiert:**
  - `index.md`: nach jedem `pages_index::rebuild` (atomar, full rewrite)
  - `log.md`: append-only, ein neuer Eintrag pro Auto-Commit oder MCP-Write
- **Format `index.md`:**
  ```markdown
  # Brain Index — auto-generated, do not edit
  
  Last updated: 2026-05-XX 09:14 UTC · 31 pages · last commit a1b2c3d
  
  ## Entities (15)
  - [[entities/dan-shapiro]] — Dan Shapiro
  - [[entities/fabro]] — Fabro …
  
  ## Concepts (7)
  …
  ```
- **Format `log.md`:**
  ```markdown
  # Brain Log — append-only, do not edit
  
  ## 2026-05-15T14:23:11Z · commit a1b2c3d · MCP write
  Added: notes/customer-feedback-q2-summary
  
  ## 2026-05-15T14:18:02Z · commit 9f8e7d6 · ingest from outlook
  Touched: entities/acme-corp, sources/email-acme-2026-05-15
  …
  ```
- **Idempotenz:** `index.md` wird hash-verglichen; Re-Generation mit identischem Output schreibt nicht (vermeidet sinnlose mtime-Änderungen)

### Module / Files

```
src-tauri/src/wiki/
├── meta_files.rs             # NEU: render_index() + append_log()
└── watcher.rs                # erweitert: ruft meta_files nach jedem Commit
```

### Tests

- `meta_files::tests::index_md_lists_all_pages_grouped_by_type`
- `meta_files::tests::index_md_is_byte_identical_when_nothing_changed`
- `meta_files::tests::log_md_appends_one_entry_per_commit`
- `meta_files::tests::log_md_uses_iso8601_timestamps_for_diff_friendliness`

### Risiken & Mitigations

| Risiko | Mitigation |
|---|---|
| Watcher-Loop wenn beide Files in `02_wiki/` lägen | Files in `00_meta/`, das nicht vom Wiki-Watcher beobachtet wird |
| `log.md` wächst unbegrenzt | Rotation: bei >10 000 Zeilen wird der älteste 10 % in `log.archive-YYYY.md` ausgelagert |
| User editiert manuell → Brain überschreibt | Banner-Header `do not edit — auto-generated` und detaillierter Reset-Button in Settings |

### Aufwand

**S — 2 Tage**: 1 Tag Generierungs-Logik + Tests, 1 Tag Watcher-Wiring + Rotation

---

## M5 — Dataview-style Frontmatter-Queries

### Was es bringt

- Beantwortet Fragen wie: *„Welche Source-Pages habe ich seit dem 1. April hinzugefügt, die mit Tag `customer` markiert sind?"*
- Verfügbar **als MCP-Tool `brain_query`** (für LLM-Aufrufe) **und** als UI-Search-Bar im Viewer
- Karpathy verweist auf Obsidian-Dataview als optionalen Bonus — Brain integriert es nativ

### Architektur-Entscheidungen

- **Query-Sprache:** kompakter DSL, der zu SQL über `pages` + `page_tags` mapped:
  ```
  type:source AND tag:customer AND updated_after:2026-04-01
  type:entity AND title:~/Acme/i
  tag:customer OR tag:supplier
  ```
- **Parser:** handgeschrieben (~150 Zeilen), kein nom oder pest — Sprache ist klein und stabil
- **Operatoren:** `:` (eq), `:~` (regex), `:>`, `:<` (für Dates), `AND`, `OR`, `NOT`, Klammerung
- **Felder:** `id`, `type`, `title`, `tag`, `created`, `updated`, `body` (volltext via FTS5-Subquery)
- **Result-Shape:** identisch zu `brain_search` — `Vec<SearchHit>`, sortiert nach `updated DESC`

### Module / Files

```
src-tauri/src/viewer/
├── query/
│   ├── mod.rs                # NEU: parser + AST
│   ├── parser.rs             # tokenize + parse zu Query-AST
│   └── sql.rs                # AST → parametrisiertes SQL
└── commands.rs               # neuer Tauri-Command query_pages

src-tauri/src/mcp/server.rs
└── brain_query als neues Tool im catalogue + call_tool

src/routes/viewer/
└── QueryBar.tsx              # NEU: dedizierte Query-UI mit Auto-Complete für Felder
```

### Tests

- `query::parser::tests::single_field_eq_parses_correctly`
- `query::parser::tests::nested_and_or_with_parens`
- `query::parser::tests::regex_modifier_case_insensitive`
- `query::parser::tests::rejects_sql_injection_attempts` (Sicherheit!)
- `query::sql::tests::generates_parameterised_sql_no_string_interp`
- `viewer::commands::tests::query_pages_returns_filtered_results`
- MCP integration test: `brain_query` mit komplexer DSL-Query

### Risiken & Mitigations

| Risiko | Mitigation |
|---|---|
| SQL-Injection durch User-Input | Strikt parametrisierte Queries, AST → `?`-Binding, keine String-Interpolation |
| LLM nutzt DSL falsch und bekommt unhelpful errors | Clear Error-Messages in der MCP-Response („expected `tag:` got `#`"), Beispiele in Tool-Description |
| Performance bei komplexen ORs auf 10k+ Pages | SQLite-Indexes auf `pages.type`, `pages.updated`, `page_tags.tag` (existieren schon) reichen |

### Aufwand

**M — 3 Tage**: 1 Tag Parser + AST, 1 Tag SQL-Generierung + MCP-Tool, 1 Tag UI + Tests

---

## Abhängigkeitsgraph

```
M1 (UX-Refactor)  ──────────────►  M5 (Query-DSL nutzt App-Shell + Tabs)

M2 (bge-m3)  ──►  M3 (sqlite-vec KNN)

M4 (index/log)         — unabhängig von allen anderen
```

Keine zirkulären Abhängigkeiten. M2+M3 sowie M4 könnten technisch parallel zu M1 laufen, wenn mehrere Hände am Brain arbeiten würden — sie greifen Backend-Module nicht an, die M1 anfasst.

## Sequenz-Empfehlung

Wenn der gesamte Plan gemacht werden soll, in dieser Reihenfolge:

1. **M1 — UX-Refactor** (5–7 Tage, größter sofort-fühlbarer User-Win, Plattform für M5)
2. **M2 — Embeddings** (5–8 Tage, semantische Suche unter der Haube)
3. **M3 — Vektor-Index** (3 Tage, baut direkt auf M2 auf)
4. **M4 — index.md / log.md** (2 Tage, Karpathy-Konformität als Side-Effekt)
5. **M5 — Query-DSL** (3 Tage, nutzt M1's App-Shell + Tabs für die neue Query-UI)

**Total:** ~18–23 Arbeitstage.

## Was außer-Scope bleibt (bewusst nicht in diesem Plan)

- **Encryption (S02)** — separater Plan, nicht Teil der Karpathy-Lücken
- **Auto-Sync / Cron-Ingest (S06)** — bleibt user-initiiert via interaktive LLM-Sessions
- **Express-Layer (Forte CODE)** — Output-Formatierung wie Marp-Slides bleibt bei externen Tools
- **Multi-User / Sharing** — Brain ist explizit Single-User (C-03)
- **Cloud-Sync zwischen Hosts** — die portable SSD ist der Sync-Mechanismus
- **Schema-Flexibilität mit UI-Wizard** — siehe [Anhang A](#anhang-a--warum-keine-schema-flexibilität)

---

## Anhang A — Warum keine Schema-Flexibilität

Das ursprüngliche M5 hätte einen User-zugänglichen Schema-Editor mit Migrations-Wizard gebracht: User legt einen neuen Page-Typ an, alle Konsumenten (FS-Layout, DB-Constraint, Lint, Viewer, AGENTS.md) ziehen automatisch nach. Das ist gestrichen, aus zwei zusammenhängenden Gründen.

### 1. „Neuer Page-Typ hinzufügen" ist im aktuellen Code billig

Stand heute ist es ein **30-Minuten-Code-Edit**, einen neuen Typ einzuführen:

| Stelle | Änderung |
|---|---|
| [`vault/layout.rs`](../src-tauri/src/vault/layout.rs) → `WIKI_SUBDIRS` | ein neuer `&str` in der Konstante (`["entities", "concepts", "sources", "topics", "projects"]`) |
| [`vault/layout.rs`](../src-tauri/src/vault/layout.rs) → `RAW_SUBDIRS` | optional ein neuer Roh-Quelldaten-Ordner |
| [`onboarding/templates/AGENTS.md`](../src-tauri/src/onboarding/templates/AGENTS.md) | 2–3 Zeilen die den neuen Typ und sein Verwendungsmuster beschreiben (für die LLM-Agenten) |
| [`src/routes/viewer/Tier1.tsx`](../src/routes/viewer/Tier1.tsx) | eine `<Section title="Projects" ids={tree.projects} />`-Zeile |
| [`src/lib/commands.ts`](../src/lib/commands.ts) → `listWikiTree`-Type | ein neues Feld `projects: string[]` im Return-Type |
| [`viewer/tree.rs`](../src-tauri/src/viewer/tree.rs) → `WikiTree`-Struct | ein neues Feld `pub projects: Vec<String>` und ein `match`-Arm |

Total: ~10 Zeilen Code-Edit, keine Daten-Migration, keine Schema-Wandlung in der DB (`pages.type` ist eine freie `TEXT`-Spalte ohne `CHECK`-Constraint). Das ist billiger als jeder UI-Wizard, der dieselbe Operation auf User-Klick durchführt.

### 2. „Existierende Kategorien umbauen" ist genau der Fall, den niemand braucht

Splittings (`entities` → `people` + `organizations`) oder Mergings (`concepts` + `sources` zusammenführen) sind teuer:
- Jede `id` ändert sich (`entities/dan-shapiro` → `people/dan-shapiro`)
- **Jeder** Wiki-Link in **jeder** Page muss migriert werden
- FTS5-Index, Backlinks-Tabelle, Graph — alles wird invalidiert
- Lint-Pipeline meldet während der Migration alles als „broken-link"

So eine Operation passiert in der Praxis **einmal** im Leben eines Vaults — wenn überhaupt. Dafür ein Tooling zu bauen ist Goldplating. Wenn so eine Migration je nötig wird, kann sie als einmaliges Skript geschrieben werden, das den Vault offline transformiert.

### Konsequenz

- Brain bleibt bei der **fixen Taxonomie** entities / concepts / sources / topics
- Wenn ein User eine zusätzliche Kategorie braucht, dokumentieren wir den 30-min-Code-Edit als „Add a new page type" in der Contributor-Doku
- Sollte der User-Druck nach Schema-Flexibilität jemals real werden (mehrere Personen wollen eigene Taxonomien), ist das ein dann-zu-bauendes Feature mit konkreter Bedarfs-Validierung — nicht heute auf Verdacht

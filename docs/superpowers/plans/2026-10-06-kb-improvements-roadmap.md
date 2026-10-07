# Wissensbasis-Verbesserungen — Umsetzungsplan (Entwurf, Roadmap-Ebene)

> **Status:** Stand 06.10.2026, **zusammengeführt mit der Deep-Research** (Teil 2 des
> Recherche-Berichts). Nächster Schritt: pro Slice ein code-granularer Task-Plan (Format
> `superpowers:writing-plans`). Kein Release, bis Slice A und B drin sind.
>
> **Grundlage:** [docs/research/2026-10-stack-review.md](../../research/2026-10-stack-review.md)
> (Keep/Upgrade-Urteile, priorisierte Chancen B1–B9, Nicht-übernehmen-Liste) sowie die
> heutigen Befunde aus Code-Review und Praxis (Suche, Prozesse, Rename/Delete-Lücke).
>
> **Methodik-Hinweis (CLAUDE.md):** Specs in `requirements/spec/` sind kanonisch. Jeder
> Slice, der *Verhalten* ändert (nicht nur Performance), bekommt vor der Umsetzung einen
> Spec-Zusatz (S06 MCP, S09 Index/Suche, S03 Wiki/Lint, S08 Viewer). C-11 (Embedding-Modell
> fest für die Vault-Lebensdauer) bleibt unangetastet: **kein Modellwechsel**.

**Ziel:** BRAIN für den KI-Agenten als Hauptnutzer besser machen — zuerst die Pflege
(Hygiene), dann messbar bessere Suche, dann die Schnittstelle — ohne den Stack umzubauen.

**Architektur-Leitplanken:** Markdown + Frontmatter bleibt die Wahrheit, SQLite (FTS5 +
sqlite-vec) bleibt der Index, bge-m3 F32 bleibt das Modell, stdio-MCP bleibt der
Transport. Alles Neue sind Konventionen im Frontmatter, Lint-Regeln, Index-Erweiterungen
und MCP-Tools — nichts davon braucht einen neuen Dienst oder eine Cloud.

**Tech-Stack:** unverändert (Tauri 2, Rust 2024, React, SQLite/FTS5/sqlite-vec, candle,
git2). Einzige Dependency-Kandidatin: ein Reranker-Modell (Slice E, gleiche Modellfamilie,
kein neues Crate).

---

## Globale Constraints

- `cargo clippy --all-targets -- -D warnings`, `cargo test`, `pnpm lint`, `pnpm test`
  grün vor jedem Commit; Conventional Commits mit Spec-ID.
- Keine neue Crate ohne Begründung in der Commit-Message (Hard Rule 4).
- Verschlüsselte Vaults: jede Dateioperation geht über `wiki::encryption::page_path` /
  `page_relpath`; Commit-Messages bleiben pfadfrei.
- Index-Format-Änderungen bumpen `INDEX_FORMAT_VERSION` (einmaliger Re-Index, idempotent).
- MCP-Tool-Umbenennungen sind **Breaking** für laufende Agenten-Sessions → nur in einem
  eigenen Release mit AGENTS.md-Update (Slice D), nie beiläufig.
- Datenschutz (C-04): kein Feature ruft externe Dienste; alle Lints/Eval-Läufe lokal.
- Test-Isolation: Tests schreiben nie in die echte Config/den echten Keychain (gelernt am
  `try_auto_reconnect`-Vorfall).

## Review-Fokus (projektweit, über alle Slices)

1. **Falscher/ungültiger `id`-String** (`Entities/Foo`, `entities/foo.md`, Leerzeichen,
   Umlaute) in rename/merge/aliases → klare Ablehnung, kein halbfertiger Zustand im Vault.
2. **Verschlüsselter Vault** bei jeder neuen Datei-Operation → Datei landet am opaken
   Pfad, Commit-Message ohne Namen; Test mit MemStore pflicht.
3. **Index-Konsistenz nach Rename/Delete/Merge** → keine Geisterzeilen in `pages`/`chunks`
   /FTS; Suche findet die neue id, nicht die alte.
4. **Lange Rebuilds blockieren Suchen** (eine Transaktion hält den DB-Mutex) → bei
   Re-Index-Bumps Batch-Commits oder expliziter „Index wird neu aufgebaut"-Hinweis.
5. **Reranker-Latenz auf CPU** → Feature nur hinter Flag, Budget 2 s für Top-20, sonst
   degradieren statt blockieren.

---

## Slice 0 — MCP-Protokoll-Konformität prüfen (neu, aus der Deep-Research)

**Bezug:** Deep-Research Befund 9 (3-0 verifiziert): Die finale MCP-Spec **2026-07-28** ist
zustandslos — `initialize`-Handshake und `Mcp-Session-Id` entfallen, jede Anfrage trägt
Protokollversion/Client-Info/Capabilities, `initialize` **und `ping`** entfallen,
`server/discover` wird Pflicht, Listen-Ergebnisse bekommen `resultType`/`ttlMs`/`cacheScope`;
Roots/Sampling/Logging/HTTP+SSE sind deprecated (Frist ≥ 12 Monate, frühestens Juli 2027).
Unser Server spricht heute die alte Version (`initialize` → `tools/list` → `tools/call`), und
die Claude-Desktop-Logs zeigen, dass die Clients sie **aktuell noch senden**.

**Umfang**
- 0.1 Spec-Abgleich: Changelog 2026-07-28 gegen `mcp/server.rs` lesen; Liste der
  betroffenen Methoden/Felder (`initialize`, `ping`, `server/discover`, `resultType`,
  `ttlMs`, `cacheScope`, Protokollversion pro Anfrage).
- 0.2 Client-Verhalten feststellen: welche Protokollversion senden Claude Code, Claude
  Desktop (Store-Build) und Codex heute? (Aus den MCP-Logs ablesbar; ggf. die erste Anfrage
  im Server mit Version loggen.)
- 0.3 **Dual-Version** (Details und Korrekturen: `docs/research/2026-10-mcp-spec-2026-07-28-gap.md`):
  der Server beantwortet weiterhin `initialize`/`ping` (alt) **und** `server/discover` +
  versionierte Einzelanfragen (neu, Version/Capabilities in `params._meta`), Auswahl pro
  Anfrage. **Nutzerentscheidung 06.10.: Versionen gleich mit anheben** — der Legacy-Pfad
  antwortet nicht mehr starr mit `2024-11-05`, sondern verhandelt nach altem Schema: die vom
  Client gewünschte Version, wenn wir sie unterstützen (`2024-11-05`, `2025-03-26`,
  `2025-06-18`, `2025-11-25`), sonst unsere höchste alte (`2025-11-25`); die Features der
  neueren alten Versionen, die ein stdio-Tool-Server sinnvoll nutzt (`outputSchema` +
  `structuredContent` seit 2025-06-18, Tool-`title`/Annotations), kommen mit Slice D.
  Defaults für den neuen Pfad: `resultType: "complete"` auf jedem Ergebnis, `ttlMs` 1 h auf
  `tools/list`/`server/discover`, `serverInfo` in `_meta`, fehlende `clientCapabilities` →
  `-32602`; unbekanntes Tool → `-32602` (spec-konform statt `isError`), „kein Vault" raus
  aus dem `-32000`-Bereich. `brain_ping` als Tool bleibt. Kein SDK-Wechsel (rmcp), kein
  HTTP — stdio bleibt.
- 0.4 Tests: je ein Handshake-Test alt und neu; `tools/list` liefert die neuen Pflichtfelder,
  wenn die neue Version verhandelt wurde.

**Akzeptanzkriterien:** Ein Client der alten Version arbeitet unverändert; ein simulierter
Client der neuen Version bekommt `server/discover` und versionierte Antworten; keine
deprecated Features werden neu eingeführt.

**Aufwand:** S (Prüfung) + S–M (Dual-Version). **Risiko:** gering mit Dual-Version; hoch,
wenn wir warten, bis ein Client die alte Version abschaltet.

**Entscheidung offen:** Dual-Version sofort bauen oder erst, wenn ein Client umschaltet?
(Vorschlag: 0.1–0.2 sofort, 0.3 in 0.3.6.)

---

## Slice A — Wiki-Hygiene (Fehlerbilder des LLM-Wikis)

**Bezug:** Recherche B6 + heutiger Befund „kein Delete/Rename". Praxisbericht nach sechs
Monaten LLM-Wiki: Dubletten unter verschiedenen Namen, verwaiste Seiten, Doku-Drift.
Deep-Research Befund 2 (Preprint 2604.12034, Design-Hypothese): Hauptrisiko von LLM-Wikis
ist **Verfestigung und Drift**, nicht Retrieval; empfohlen sind **geplante** (nicht nur
reaktive) Konsolidierungs-/Audit-Läufe und **Archivieren statt Löschen** — deckt sich mit
„Delete verweigert bei Verweisern" und `superseded_by` (Slice C).

**Umfang**
- A1 Umbenennen, Löschen (verweigert bei Backlinks, `force` entschärft Links) und
  Zusammenführen von Seiten — heute `brain_refactor` (`action: rename | delete | merge`,
  Slice D) — **umgesetzt**, inkl. Linkumschreibung
  vault-weit, ein Commit pro Operation, AGENTS.md-Abschnitt „falsche Seite reparieren".
- A2 Frontmatter `aliases: [..]` (Liste alternativer Namen). `brain_lookup` (vormals
  Existenzprüfung) und
  `brain_write_page` prüfen Treffer auch gegen Aliases und gegen einen unscharfen Vergleich
  des Slugs (Kleinschreibung, Umlaute → ae/oe/ue, Bindestrich/Unterstrich, Levenshtein ≤ 2)
  und antworten mit „existiert vermutlich schon als `<id>`" statt stillschweigend eine
  Dublette anzulegen.
- A3 Lint-Regeln: (a) Dubletten-Kandidaten per Seiten-Embedding-Ähnlichkeit (Mittelwert
  der Chunk-Vektoren, Cosine ≥ 0,92 → Kandidat), (b) Waisen (0 eingehende Links **und**
  älter als 90 Tage ohne Änderung), (c) tote Links (`[[id]]` ohne Zielseite).
  `brain_lint_report` liefert sie strukturiert; die GUI-Integrity-Seite zeigt sie.
- A4 Index-Konsistenz: Rename/Delete/Merge entfernen alte `pages`/`chunks`/FTS-Zeilen im
  MCP-Prozess sofort (nicht erst beim nächsten Watcher-Rebuild).
- A5 Geplanter Audit-Lauf: der Watcher (oder ein täglicher Timer in der GUI) schreibt den
  Lint-/Hygiene-Report nach `00_meta/audit/<datum>.md`; AGENTS.md erhält den Auftrag, den
  jüngsten Report zu Beginn einer Pflege-Sitzung abzuarbeiten (Konsolidierung als geplante
  Operation, ohne eigenes LLM in BRAIN).

**Akzeptanzkriterien**
- Rename einer Seite mit 3 Verweisern: Datei am neuen (opaken) Pfad, 3 Seiten umgeschrieben,
  `[[alt-2]]` unverändert, genau 1 Commit, `brain_search` findet die neue id.
- `brain_write_page` mit id `entities/muller-gmbh` bei existierender `entities/mueller-gmbh`
  → Fehler mit Verweis auf die existierende id (kein zweiter Eintrag).
- Lint-Report nennt Dubletten-Paare mit Score, Waisen mit letztem Änderungsdatum, tote Links
  mit Quellseite.

**Aufwand:** A1 M (läuft), A2 S, A3 M, A4 S. **Risiko:** gering–mittel (A3 False
Positives → Schwellwert konfigurierbar halten).

**Entscheidungen offen:** Soll der Watcher Waisen/Dubletten als Toast melden oder nur im
Report? (Vorschlag: nur Report + Integrity-Seite, kein Toast-Spam.)

---

## Slice B — Messbarkeit + kostenloser Kontext

**Bezug:** Recherche B1 (Testsatz) und B4 (`summary` im Frontmatter). Ohne B1 ist jede
Retrieval-Änderung Glaubenssache.

**Umfang**
- B1 Retrieval-Eval: Datei `00_meta/eval/queries.yaml` im Vault (Frage → erwartete
  Seiten-ids, 50–100 Einträge, vom Nutzer/Agenten gepflegt, **getrennt von Holdouts**,
  C-16). CLI `brain eval <vault>` und MCP-Tool `brain_eval` rechnen Recall@10, MRR und
  nDCG@10 für FTS-only, Dense-only und Hybrid und drucken eine Tabelle. Ergebnisse landen
  als Markdown-Zeile in `00_meta/eval/history.md` (Datum, Index-Version, Kennzahlen).
- B2 Frontmatter `summary:` (1–2 Sätze), vom schreibenden Agenten geliefert
  (`brain_write_page` nimmt es optional an, AGENTS.md verlangt es für neue Seiten). Der
  Chunk-Header wird `<title> (<type>) › <heading path> — <summary>`; Header-Cap steigt auf
  280 Zeichen. **Zusätzlich** fließt `summary` in die FTS-Tabelle (eigene Spalte, Gewicht
  höher als Body) → kontextuelles BM25.
- B3 Lint: Seiten ohne `summary` als Hinweis (nicht Fehler).
- Index-Format-Version bump (einmaliger Re-Embed) — Rebuild in **Batches zu 50 Seiten
  committen**, damit Suchen währenddessen nicht minutenlang blockieren (Review-Fokus 4).

**Akzeptanzkriterien**
- `brain eval` auf dem Testvault liefert Kennzahlen für drei Modi; ein zweiter Lauf ohne
  Änderung liefert identische Zahlen (deterministisch).
- Nach B2 steigt Recall@10 Hybrid auf dem eigenen Testsatz gegenüber der Baseline (Zahl
  wird im Plan-Review festgehalten; Erwartung aus der Literatur: spürbar, nicht garantiert).
- Während eines Re-Embeds antwortet `brain_search` innerhalb von 5 s (ggf. mit noch alten
  Vektoren) statt zu blockieren.

**Aufwand:** B1 S–M, B2 S, B3 S, Batch-Rebuild S. **Risiko:** gering.

**Entscheidungen offen:** Pflege des Testsatzes — vom Nutzer per Hand, oder lässt der
Agent beim Anlegen einer Seite automatisch eine Beispielfrage dazu eintragen?

---

## Slice C — Zeitliche Gültigkeit und Provenienz (ohne Knowledge Graph)

**Bezug:** Recherche B5 (Kernidee von Zep/Graphiti bitemporal, mit Bordmitteln).

**Umfang**
- C1 Frontmatter-Konvention: `valid_from`, `valid_to` (ISO-Datum), `superseded_by: <id>`,
  `sources: [<id>, …]` (Quellenseiten vom Typ `sources`).
- C2 `brain_query` versteht `valid:now` (Standard) und `valid:all`; `brain_get_pages`
  kennzeichnet abgelöste Seiten im Ergebnis („superseded by …").
- C3 Lint: (a) Seite mit Fakten-Typ (`entities`, `concepts`) ohne `sources` → Hinweis,
  (b) `superseded_by` zeigt auf nicht existierende Seite → Fehler, (c) Seiten mit
  `valid_to` in der Vergangenheit, die noch eingehende Links haben → Hinweis.
- C4 AGENTS.md: Regel „Fakten überschreiben nie — Seite ablösen (`superseded_by`) und
  `valid_to` setzen".

**Akzeptanzkriterien**
- Zwei Seiten A (valid_to 2025-12-31, superseded_by B) und B: `brain_query type:entities`
  liefert nur B; mit `valid:all` beide; `get_context` auf A trägt den Hinweis.

**Aufwand:** S–M. **Risiko:** gering. **Abhängigkeit:** Spec-Zusatz zu S03/S06.

---

## Slice D — MCP-Schnittstelle straffen (Breaking, eigenes Release)

**Bezug:** Recherche B3 (Anthropic „Writing effective tools for agents", Context-Bloat).

**Umfang**
- D1 Tools zusammenlegen — **Nutzerentscheidung: genau 15 Tools** (`brain_ping`,
  `brain_search`, `brain_lookup`, `brain_get_pages`, `brain_query`, `brain_graph`,
  `brain_write_page`, `brain_write_batch`, `brain_patch_page`, `brain_refactor`,
  `brain_lint_report`, `brain_history`, `brain_write_raw_file`, `brain_eval`,
  `brain_dream`). Einzel- und Mehrfachlesen → `brain_get_pages(ids: [..])`, Kontext per
  `include_context: true`; Existenzprüfung → `brain_lookup` (Id oder Name, ohne Bodies);
  Seiten- und Tag-Listing → `brain_query` (`*`, `facet: "tags"`); Umbenennen/Zusammenführen/
  Löschen → `brain_refactor` (`action`); Historie/Wiederherstellen → `brain_history`
  (`action`); Eval und Traum je ein Tool mit `action`. Die vollständige Zuordnung alt → neu
  steht im CHANGELOG.
- D2 `response_format: "concise" | "detailed"` an `search`, `query`, `get_pages`,
  `lint_report`; Standard `concise` (ids, Titel, Snippet) — spart Kontext.
- D3 Strukturierte Ausgaben: `outputSchema` + `structuredContent` für alle Tools mit
  JSON-Ergebnis (prüfen, was die eingesetzte MCP-Protokollversion/der Client trägt;
  Fallback bleibt der heutige JSON-String im Textblock).
- D4 MCP Prompts: `ingest` (Rohdatei → Quellseite → Entitäten) und `lint-session`
  (Report abarbeiten) als wiederverwendbare Prompt-Templates; AGENTS.md zusätzlich als
  MCP-Resource.
- D5 `SKILL.md` im Vault (`00_meta/skills/brain-wiki/SKILL.md`) nach dem offenen
  Agent-Skills-Standard, gleicher Inhalt wie AGENTS.md, für Clients, die Skills laden.
- D6 Protokoll-Check: Unterstützt der eingesetzte Rust-MCP-Pfad die zustandslose Variante
  (2026-07-28)? Nur dokumentieren, nicht umbauen, solange stdio funktioniert.

**Akzeptanzkriterien**
- `tools/list` genau 15 Einträge; jede Beschreibung nennt den Anwendungsfall in einem Satz
  und das Geschwister-Tool für den Fall „nicht hierfür".
- `brain_search` concise für 10 realistische Treffer (35-Zeichen-Id, 30-Zeichen-Titel, 80-Zeichen-Snippet) < 2.000 Zeichen, Titel auf 60 Zeichen gekürzt; detailed enthält hervorgehobene Snippets.
- Alte Tool-Namen liefern einen Fehler mit dem neuen Namen (eine Übergangsversion lang).

**Aufwand:** S–M. **Risiko:** gering technisch. **Nutzerentscheidung (06.10.): keine
Übergangsfrist** — alte Tool-Namen antworten mit einem Fehler, der den neuen Namen nennt;
Agenten lesen `tools/list` ohnehin pro Sitzung. AGENTS.md und SKILL.md ziehen im selben
Release mit. CHANGELOG mit Mapping alt → neu.

---

## Slice E — Reranker (gated durch Slice B)

**Bezug:** Recherche B2. Deep-Research (3-0 verifiziert): −67 % gegenüber Baseline, aber
**nur ~34 % relativ zusätzlich** zu Contextual Embeddings + BM25 (2,9 % → 1,9 %); gemessen
mit Cohere-API-Reranker, nicht CPU-lokal. ONNX-Export von `bge-reranker-v2-m3` (fp32 +
quantisiert) existiert. **Unsicher:** CPU-Latenz; Erwartung bewusst gedämpft.

**Umfang**
- E1 Benchmark zuerst: `bge-reranker-v2-m3` (Apache-2.0, XLM-RoBERTa, Loader/Tokenizer von
  bge-m3 wiederverwendbar) auf dieser CPU mit 20 Paaren messen — Ladezeit, Latenz, RAM.
  Alternative `Qwen3-Reranker-0.6B`. Go/No-Go-Schwelle: ≤ 2 s für Top-20 nach warmem
  Modell, sonst Slice E zurückstellen.
- E2 Integration: nach RRF die Top-20 (konfigurierbar) rerank­en; `brain_search` bekommt
  `rerank: bool` (Standard an, wenn Modell vorhanden und Budget hält), GUI-Suche nutzt es
  nur, wenn Latenz < 1 s gemessen. Modell teilt sich den Embedder-Cache-Mechanismus
  (Idle-Eviction, Fehlschlag-Merker).
- E3 Eval: Recall@10/nDCG mit und ohne Reranker im `brain eval` ausweisen.

**Akzeptanzkriterien:** messbare nDCG@10-Verbesserung auf dem Testsatz bei eingehaltenem
Latenzbudget; ohne Modell im Vault verhält sich alles wie heute.

**Aufwand:** M. **Risiko:** mittel (Latenz, +1,1–2,2 GB RAM bei aktivem Reranker).
**Abhängigkeit:** B1 (Messung), Download-Pfad wie beim Embedding-Modell (C-11-analog: der
Reranker ist optional und austauschbar, da er keine Vektoren persistiert).

---

## Slice F — Viewer für den Menschen

**Bezug:** Recherche B8 (Obsidian-Stärken: Smart Connections, Bases).

**Umfang**
- F1 „Ähnliche Seiten" im Seiten-Viewer: Top-5 nach Cosine der Seiten-Mittelwert-Vektoren
  (gleiche Berechnung wie A3-Dubletten, geteilter Code).
- F2 Tabellenansicht über Frontmatter: `brain_query`-Ergebnis als sortier-/filterbare
  Tabelle (Typ, Tags, updated, valid_to, summary) in Tier 2.

**Aufwand:** M. **Risiko:** gering. **Abhängigkeit:** A3 (Seitenvektoren), C1 (Spalten).

---

## Slice H — Konsolidierung („Träumen"): geplantes Ordnen des Wikis

**Bezug:** Nutzerwunsch (06.10.): „wie ein Gehirn, das träumt und Gedanken ordnet";
Deep-Research Befund 2 (Preprint 2604.12034: TRIAGE/DECAY/CONTEXTUALIZE/CONSOLIDATE/AUDIT),
Lettas „Sleep-Time Compute". Leitidee: **BRAIN hat kein LLM** (C-04/C-08) — also
Arbeitsteilung: BRAIN erledigt nachts das mechanische Ordnen und bereitet eine Traumqueue
vor (Tiefschlaf); ein geplanter Agenten-Lauf über MCP erledigt das inhaltliche Ordnen
(REM) nach festen Regeln.

**Umfang**
- H1 **Traumqueue** (`00_meta/dream-queue.md`, täglich, erweitert A5): priorisierte
  Arbeitsliste aus Audit-Funden (Dubletten, Waisen, tote Links) plus neuen Signalen:
  (a) `summary` veraltet (Body-Hash geändert seit `summary` geschrieben → `summary_hash`
  im Frontmatter oder in der DB), (b) Hubs ohne `summary` (≥ 5 eingehende Links), (c)
  Decay-Kandidaten (nie gelesen laut H3, keine eingehenden Links, > 90 Tage) — immer als
  Vorschlag „ablösen/archivieren", nie als Löschung. Dazu DB-Hausarbeit: FTS `optimize`,
  Seiten-Mittelwertvektoren vorberechnen (`page_vectors`-Tabelle, dient Dubletten-Lint und
  später „ähnliche Seiten"), `VACUUM` bei Bedarf.
- H2 **Traum-Protokoll**: MCP-Prompt-Template `dream` (Slice D4) — „lies die Queue,
  arbeite die Top-N ab (merge/rename/patch/summary), schreibe `00_meta/dream-log.md`";
  AGENTS.md-Abschnitt mit harten Regeln: max N Änderungen pro Lauf (Standard 10), nie
  verlinkte Seiten löschen, Ablösen statt Überschreiben (`superseded_by`),
  Minderheitshypothesen behalten (Preprint: Verfestigung), jeder Lauf hinterlässt ein
  lesbares Log, alles per `brain_history` (`action: restore`) rückholbar. **Log pro Eintrag
  (0.3.5):** der eine `brain_dream`-`log`-Aufruf am Ende nennt jedes angesehene Queue-Item mit
  Ergebnis (`done` / `skipped` + Grund / `deferred`); die Queue zählt frühere Skips
  (`skipped_before`), ab 3 mit Hinweis im Grund und P3-Items ans Ende (nie ausgeblendet).
  **`keep: true`** im Frontmatter („bleibt, obwohl unverlinkt/ungelesen") unterdrückt
  `orphan`-Warnung und Decay-/Orphan-Items — sonst nichts. **Auslösung (Nutzerentscheidung
  06.10.): kein Zeitplan.** Der Nutzer triggert die REM-Phase bei Gelegenheit selbst — im
  Client per Prompt-Template `dream` oder schlicht „träum mal" an den Agenten, der dann
  `brain_dream` (`action: queue`) liest; wer automatisieren will, kann es (z. B. `/schedule`), BRAIN
  setzt es nicht voraus. Damit die Queue jederzeit frisch ist, erzeugt BRAIN sie nicht nur
  nachts, sondern auch **on demand**: Tool `brain_dream` mit `action: queue` (gibt die aktuelle Queue
  zurück, rechnet sie neu, wenn älter als 1 h) — so braucht ein spontaner Traum keinen
  vorherigen Tiefschlaf-Lauf.
- H3 **Salienz**: Tabelle `page_access(page_id, reads, last_read_at, search_hits)`;
  Hooks in `brain_get_pages` (reads, auch mit `include_context`) und `brain_search` (Treffer in
  Top-10). `brain_query` erhält `sort:salience`; Audit/Traumqueue nutzen sie. Keine
  Zeitstempel ins Frontmatter (würde Commits erzeugen) — nur DB, lokal, nicht gesynct.
- H4 (später, optional) lokaler LLM-Provider (Ollama) ausschließlich für
  `summary`-Erzeugung innerhalb von BRAIN, Flag-gesteuert; erst wenn H2 im Alltag läuft.

**Akzeptanzkriterien**
- Nach einem Mount + 24 h existieren `00_meta/audit/<datum>.md` und `dream-queue.md`; die
  Queue nennt pro Eintrag Typ, Seiten-ids, Grund und die empfohlene Operation.
- Eine Seite, die der Agent dreimal liest, hat `reads = 3` in `page_access`; `brain_query
  sort:salience` sortiert sie nach vorn.
- Ein simulierter Traum-Lauf (Test: Queue mit einem Dubletten-Paar → `brain_refactor` merge)
  reduziert die Queue beim nächsten Tiefschlaf um genau diesen Eintrag.

**Aufwand:** H1 S–M, H2 S, H3 S. **Risiko:** gering (alles additiv, nichts löscht).
**Release:** 0.3.5 (H1–H3), H4 offen.

---

## Slice G — Infrastruktur-Optionen (nur bei Bedarf)

- G1 **Graph-Nachbarn im Retrieval** (Recherche B7, HippoRAG-light): Treffer um direkte
  Linknachbarn erweitern, nur hinter Reranker (Slice E), nur wenn B1 einen Gewinn bei
  Multi-Hop-Fragen zeigt. Aufwand M, Nutzen unsicher.
- G2 **Geteilter MCP-Daemon** (ein Prozess, ein Modell für alle Clients): nur wenn RAM
  (N × 2,2 GB) oder die Prozessliste im Alltag stören. Aufwand L.
- G3 **Quantisierte Vektoren** (int8/binär+Rescore) erst ab ~100k Chunks. Aufwand S.
- G4 **Rebuild in Batches** — in Slice B eingeplant (Review-Fokus 4), hier als
  eigenständiger Punkt falls Slice B später kommt.

---

## Reihenfolge und Release-Schnitte (Vorschlag)

| Release | Inhalt | Begründung |
|---|---|---|
**Nutzerentscheidung 06.10.: eine große Version statt mehrerer kleiner.**

| Release | Inhalt | Begründung |
|---|---|---|
| **0.3.5** | Contextual Chunking + Rename/Delete/Merge (committed) · **Slice 0** (Spec-Abgleich, Versions-Logging, Dual-Version-Server) · **Slice A** (Aliases/Dublettenprüfung, Hygiene-Lint, Audit-Report, Index-Konsistenz) · **Slice B** (Eval + `summary`) · **Slice C** (Gültigkeit/Provenienz) · **Slice D** (MCP straffen, ohne Übergangsfrist) · **Slice H1–H3** (Traumqueue, Traum-Protokoll, Salienz) · **G4** Batch-Rebuild | Alles, was ohne Messdaten gebaut werden kann; Agenten lesen `tools/list` pro Sitzung, daher kein Alias-Zwischenschritt |
| **0.3.6** | Slice E (Reranker, gated durch Eval-Zahlen aus B1) + Slice F (Viewer: ähnliche Seiten, Tabellen) + H4 (lokaler LLM-Provider, optional) | Erst nach Messung bzw. Alltagserfahrung mit dem Traum-Protokoll |
| offen | Slice G (Graph-Nachbarn, geteilter Daemon, Quantisierung) | nur bei konkretem Bedarf |

**Umsetzungswellen für 0.3.5** (sequenziell, weil `mcp/server.rs` von fast allem berührt wird):
1. Welle 1 (läuft): A3 + A5 + G4 · Slice 0.1–0.2 + Design 0.3
2. Welle 2: A2 Aliases/Dublettenprüfung · Slice B (`brain eval`, `summary`) · Slice C · H3 Salienz · H1 Traumqueue
3. Welle 3: Slice D (Tools straffen, Prompts inkl. `dream`, Resources, SKILL.md) · Slice 0.3 Dual-Version · H2 Traum-Protokoll in AGENTS.md
4. Review je Welle, dann CHANGELOG konsolidieren, Version 0.3.5, Tag, Release.

## 0.3.6 — Backlog aus dem Betrieb von 0.3.5 (Stand 07.10.2026)

Ergänzt die Release-Tabelle oben. Quelle: erste Traumsitzungen im echten Vault am
07.10.2026 (zwei echte Dubletten gefunden, die die Queue nicht hatte) und die
Review-Runden vor dem Release.

| # | Item | Warum | Größe |
|---|---|---|---|
| 1 | **Titel-Gleichheit als Dubletten-Signal.** Hygiene und Traumqueue melden zwei Seiten gleichen Typs mit identischem (normalisiertem) Titel als `duplicate-candidate`, unabhängig von der Embedding-Ähnlichkeit; `distinct_from` unterdrückt wie bisher. | Beide echten Dubletten des ersten Traumlaufs (Firmenseite, COCKPIT) hatten denselben Titel, lagen aber unter der 0,92-Schwelle; `brain_lookup` greift nur über Slugs. Billig, treffsicher. | S |
| 2 | **YAML-Hinweis für Summaries.** Tool-Beschreibung von `brain_write_page`/`brain_write_batch`: Summary quoten, wenn sie `: ` enthält; optional serverseitig den Parse-Fehler mit genau diesem Hinweis anreichern. | Erster Schreibversuch im Traumlauf scheiterte an „mapping values are not allowed"; der Fehler war sauber, aber ohne Hinweis auf die Ursache. | XS |
| 3 | ✅ **erledigt (0.3.6)** — **Memory-Prompt und SKILL.md per Schalter installieren.** Settings → MCP: markierter Block in `~/.claude/CLAUDE.md` und `~/.codex/AGENTS.md`, SKILL.md nach `~/.claude/skills/brain-wiki/`; Versionsmarke, Block-Ersatz bei Update, Entfernen bei Abwahl. Claude Desktop bleibt Copy-Paste. | „Update vault templates" ändert am Client nichts; der kopierte Prompt veraltet still. **Entscheidung Pascal offen** (Prompt gilt dann in allen Claude-Code-Projekten). | M |
| 4 | ✅ **erledigt (0.3.6)** — **Traum-Log auswerten.** Kleine Auswertung je Item-Art: wie oft done/skipped/deferred; Anzeige in Integrity oder als Resource. | Das strukturierte Log aus 0.3.5 liefert die Daten, genutzt werden sie noch nicht. | S |
| 5 | ✅ **erledigt (0.3.6, nur Prompt/AGENTS.md)** — **`missing-sources` für Ingestion-Seiten** halbautomatisch: Lint-Session-Prompt schlägt den Master-Index der jeweiligen Mail-Ingestion als `sources`-Eintrag vor. | Fast jede aus Mails erzeugte Seite trägt die Warnung; von Hand ist das Fleißarbeit. | S |
| 6 | **Repo-Hygiene:** repo-weites `cargo fmt` als eigener Commit, ungenutztes `axum`-Crate entfernen, flaky Windows-Test `viewer::eval::tests::concurrent_adds_keep_every_entry` (Datei-Lock-Rennen) stabilisieren. | Technische Schulden aus dem 0.3.5-Zyklus. | S |
| 7 | **Spec-Addenda** S03 (Lint/Hygiene), S06 (MCP-Oberfläche/Protokoll), S09 (Index/Suche) als Entwürfe unter `docs/`, Übernahme nach `requirements/` durch Pascal. | CLAUDE.md verlangt Spec-Deckung für Verhaltensänderungen; `requirements/` ist für den Build-Agenten read-only. | M |
| — | Slice E Reranker, Slice F Viewer, H4 lokaler LLM-Provider | wie in der Release-Tabelle: gated durch `brain eval`-Zahlen bzw. Alltagserfahrung | L |

## Nicht in diesem Plan (bewusst)

GraphRAG/LightRAG mit LLM-Extraktion, Memory-Frameworks als Abhängigkeit, Wechsel der
Vektor-DB, Embedding-Modellwechsel (C-11), F16/INT8 für bge-m3, ColBERT, HyDE/Query-
Rewriting in BRAIN, Remote-HTTP-MCP, eingebaute Chat-UI (C-08). Begründungen im
Recherche-Bericht, Abschnitt C.

## Ergebnis der Zusammenführung mit `/deep-research`

- **Reranker-Priorität:** bestätigt als Upgrade-Kandidat, Erwartung gedämpft (~34 % relativ
  zusätzlich); bleibt hinter Eval (B1) und Slice D. **E nach D.**
- **CPU-Latenzzahlen:** keine verifizierten; nur die Existenz des ONNX-Exports. → Benchmark
  bleibt Pflicht (E1).
- **MCP 2026-07-28:** größte Änderung — **neuer Slice 0** (Spec-Abgleich, Client-Versionen,
  Dual-Version). Rust-SDK-Stand weiter ungeprüft (Teil von 0.1).
- **Hygiene-Fehlerbilder:** Preprint ergänzt „Verfestigung/Drift" und „geplante Läufe" →
  **A5 Audit-Lauf** und das Prinzip „Archivieren statt Löschen" (Slice C deckt es).
- **Nicht-übernehmen-Liste:** vollständig bestätigt (GraphRAG-Familie, Late Chunking,
  Cloud-Kontext/Cloud-Reranker, Vektor-DB-Wechsel, Remote-HTTP-MCP). Zu Memory-Frameworks,
  ColBERT/Sparse, Quantisierung und Storage-Alternativen fand die Deep-Research **keine
  Evidenz** — Teil-1-Urteile stehen unverifiziert.
- **Obsidian-Vergleich** abgeschwächt: Vorteil liegt bei Modellqualität, Betrieb ohne GUI,
  Git-Verschlüsselung, agentengepflegter Struktur — nicht pauschal bei „Schreiben können".

Nächster Schritt: pro Slice ein code-granularer Task-Plan (TDD, bite-sized, Commits) und
Umsetzung per Subagent-driven Development mit Review-Gate je Task — beginnend mit Slice A
(Rename/Delete/Merge im Review) und Slice 0.1–0.2.

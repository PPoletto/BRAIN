# BRAIN-Stack-Review, Stand Oktober 2026

> Recherche-Bericht (Opus-Subagent, 06.10.2026), nur gelesen: MCP-Toolliste,
> `docs/architecture.md`, `requirements/constraints.md`. Quellen am Ende.
> Unsicherheiten sind im Text markiert.

## Kernaussage

Der Stack ist für eine persönliche Wissensbasis, die vor allem KI-Agenten nutzen,
weiterhin solide und weitgehend zeitgemäß. Einen Umbau braucht es nicht. Sinnvoll
sind drei Ergänzungen:

1. **Reranking** hinter der Hybrid-Suche.
2. **Agentengerechtere MCP-Schnittstelle**: weniger Tools, strukturierte Ausgaben,
   mitgelieferte Workflows.
3. **Wiki-Hygiene-Konzepte aus den Agent-Memory-Frameworks**: zeitliche Gültigkeit,
   Provenienz, Alias-Auflösung, Konsolidierungs-Lint.

Graph-RAG mit LLM-Extraktion und fertige Memory-Frameworks lohnen sich bei Hunderten
bis wenigen Tausend Seiten nicht. Der kuratierte Wiki-Link-Graph ist dafür bereits das
günstigere Gegenstück.

Geprüft: 18 MCP-Tools, RRF mit k=60 in `viewer/search.rs`, Constraint C-11
(Embedding-Modell bleibt für die Lebensdauer des Vaults fest).

---

## A. Urteil je Stack-Komponente

| Komponente | Urteil | Begründung |
|---|---|---|
| Tauri 2 / Rust / React | **Beibehalten** | Kein Ablösedruck erkennbar, passt zu local-first. |
| Markdown + YAML + `[[id]]` | **Beibehalten** | Gleiches Format wie Karpathys „LLM wiki" und Obsidian — faktischer Standard für agentengepflegte Wikis. |
| Git (libgit2) + clientseitige Verschlüsselung + opake Dateinamen | **Beibehalten** | Wichtigstes Alleinstellungsmerkmal. Die gängigen Obsidian-MCP-Wege bieten Verschlüsselung auf Git-Ebene nicht (*Annahme, nicht im Detail geprüft*). |
| SQLite FTS5 (BM25) | **Beibehalten** | Agenten arbeiten nachweislich gut mit iterativer Stichwortsuche; Claude Code hat Vektor-RAG zugunsten von grep aufgegeben. |
| sqlite-vec (Brute Force, F32) | **Beibehalten, beobachten** | Projekt wird wieder aktiv gepflegt (0.1.7–0.1.9 stabil). DiskANN, IVF und Rescore sind erst Alpha (0.1.10-alpha, Mai 2026). Bei ≤100k Chunks reicht Brute Force. |
| bge-m3 dense (candle, CPU) | **Beibehalten** | Qwen3-Embedding-0.6B liegt auf MMTEB gesamt vorn (64,33 zu 59,56), bei Retrieval aber nahezu gleichauf (80,83 zu 80,76). Ein Wechsel würde außerdem C-11 verletzen. |
| RRF k=60 | **Beibehalten** | Robuster Standard. Der eigentliche Hebel sitzt danach, beim Reranker. |
| 220-Wort-Fenster + Contextual Chunk Headers (in Arbeit) | **Upgrade (in Arbeit)** | Laut Anthropic sinkt die Top-20-Fehlerrate mit kontextuellen Embeddings und kontextuellem BM25 um 49 %, mit zusätzlichem Reranking um 67 %. |
| Cytoscape-Graph + Backlinks | **Beibehalten** | Für Menschen ausreichend. Der Graph lässt sich zusätzlich fürs Retrieval nutzen (B7). |
| stdio-MCP mit 18 Tools | **Upgrade** | Toolanzahl reduzieren, `outputSchema`/`structuredContent`, Prompts und Resources nutzen (B3). stdio bleibt in MCP 2026-07-28 zulässig. |
| AGENTS.md im Vault | **Beibehalten und ergänzen** | AGENTS.md wird von der Agentic AI Foundation (Linux Foundation) betreut. Zusätzlich ein Agent Skill (`SKILL.md`, offener Standard seit Dezember 2025) mitliefern. |

---

## B. Chancen nach Priorität

Aufwand für einen Rust-Entwickler allein: S = Tage, M = 1–3 Wochen, L = > 1 Monat.

**1. Eigener Retrieval-Testsatz als Voraussetzung**
- 50–100 echte Agenten-Anfragen mit erwarteten Seiten, Kennzahl Recall@10. Ohne diesen
  Satz lässt sich keine der folgenden Maßnahmen bewerten.
- Abgrenzung: eigenes Werkzeug zur Feinabstimmung, getrennt von den Holdouts (C-16).
- Aufwand: S. Risiko: gering.

**2. Cross-Encoder-Reranker auf den Top 20–30 der RRF-Liste**
- Kandidaten: `bge-reranker-v2-m3` (Apache-2.0, XLM-RoBERTa, gleiche Modellfamilie wie
  bge-m3 — Tokenizer und Loader weitgehend wiederverwendbar) oder `Qwen3-Reranker-0.6B`
  (MMTEB-R 66,36). Nicht nehmen: `jina-reranker-v2` (CC-BY-NC-4.0).
- Belege: Anthropic meldet 67 % weniger Fehlabrufe; mehrere Vergleichsstudien 2025/26
  nennen Reranking als wirksamsten Einzelbaustein, auch gegenüber Graph-Retrieval.
- *Unsicher ist die CPU-Latenz:* bei 568M Parametern und 25 Paaren geschätzt 1–3 s ohne
  Quantisierung (nicht gemessen; 0,14 s/Anfrage-Angaben sind vermutlich GPU).
  Gegenmittel: int8, Kandidaten begrenzen, optionaler Parameter `rerank=true`.
- Aufwand: M. Risiko: mittel (Latenz, RAM).

**3. MCP-Schnittstelle straffen**
- Tools zusammenlegen: `page_exists` in `get_page`, `get_page`/`get_pages` zusammen,
  `list_tags` in `query`. `response_format: concise|detailed`, strukturierte Ausgaben.
  `ingest` und `lint` als MCP Prompts, AGENTS.md als Resource.
- Grundlage: Anthropics „Writing effective tools for agents", Context-Bloat-Debatte;
  Claude Code lädt Tool-Definitionen inzwischen deferred — knappe, gut beschriebene Tools
  werden zuverlässiger gefunden.
- Protokoll: MCP 2026-07-28 ist zustandslos (kein Initialize-Handshake mehr), Roots/
  Sampling/Logging veraltet. Vor SDK-Update prüfen, ob die Rust-SDK-Version das trägt.
- Aufwand: S–M. Risiko: gering, Clients müssen neue Tool-Namen lernen.

**4. Kontext ohne zusätzlichen LLM-Aufruf**
- Den schreibenden Agenten bei `write_page` ein `summary` im Frontmatter mitliefern
  lassen; `summary` + Titel + Typ + Abschnittspfad jedem Chunk voranstellen (FTS5 und
  Vektorindex). Contextual Retrieval ohne Cloud und ohne lokales LLM.
- Aufwand: S. Risiko: gering.

**5. Zeitliche Gültigkeit und Provenienz als Konvention plus Lint (kein KG)**
- Frontmatter: `valid_from`, `valid_to`, `superseded_by`, `sources:[id]`. Lint meldet
  Aussagen ohne Quelle und abgelöste Seiten, die noch verlinkt sind; `query` filtert
  standardmäßig auf aktuell Gültiges. Kernidee von Graphiti/Zep (bitemporal), mit
  Bordmitteln.
- Aufwand: S–M. Risiko: gering.

**6. Lint gegen die bekannten Fehlerbilder des LLM-Wikis**
- Praxisbericht nach sechs Monaten Betrieb (08/2026): Doku-Drift, stille
  Entity-Dubletten unter verschiedenen Namen, verwaiste Seiten.
- Gegenmaßnahmen: Frontmatter `aliases:` + unscharfer Namensabgleich in `page_exists`;
  Dubletten-Kandidaten über Embedding-Ähnlichkeit zwischen Seiten; verwaiste Seiten
  melden (wenig eingehende Links, lange nicht gelesen). Lint regelmäßig planen; der
  Bericht wird vom Agenten in der nächsten Sitzung abgearbeitet (Lettas „Sleep-Time
  Compute", ohne eigenes LLM in BRAIN).
- Aufwand: M. Risiko: gering.

**7. Leichtes Graph-Retrieval über den vorhandenen Linkgraphen**
- Treffer um direkte Nachbarn erweitern oder Personalized PageRank über `wiki_links` ab
  den Suchtreffern — HippoRAG 2 ohne LLM-Extraktion.
- Belege: HippoRAG 2 gewinnt vor allem bei Multi-Hop (MuSiQue 48,6 zu 45,7 F1), bei
  Faktenfragen kaum (NQ 63,3 zu 61,9). Naives Mischen verschlechtert ohne Reranker —
  deshalb erst nach Punkt 2.
- Aufwand: M. Nutzen: unsicher, mit Punkt 1 messen.

**8. „Ähnliche Seiten" und Tabellenansichten im Viewer (für den Menschen)**
- Smart Connections (verwandte Notizen via lokale Embeddings) und Bases (Tabellen über
  Frontmatter) machen Obsidian im Alltag stark. BRAIN hat die Embeddings bereits; für
  Seitenähnlichkeit fehlt nur ein Mittelwert der Chunk-Vektoren pro Seite.
- Aufwand: M. Risiko: gering.

**9. Quantisierung (int8 oder binär mit Rescore) erst ab ~100k Chunks**
- 25k Chunks × 1024 × 4 Byte ≈ 100 MB — Brute Force unkritisch. Binäre Quantisierung
  ohne Rescore kostet spürbar Qualität (sqlite-vec-Doku).
- Aufwand: S. Nutzen jetzt: gering.

### Einordnung gegenüber Obsidian

- **Obsidian mehr:** Editor, Mobile-Apps, Canvas, Bases, ~2.500 Plugins, Copilot-Chat mit
  Agentenmodus, offizielle CLI seit v1.12 (02/2026), mindestens sechs MCP-Server.
- **BRAIN mehr:** Verschlüsselung bis ins Git-Remote, nativer MCP-Server ohne laufende
  Editor-App (Obsidian-CLI spricht per IPC mit laufender Instanz), agentengepflegte
  Wiki-Struktur mit Historie und Restore.
- **Folgerung:** Chat nicht übernehmen (C-08). Ähnliche Seiten und Bases-artige Ansichten
  übernehmen (B8).

---

## C. Jetzt nicht übernehmen

| Kandidat | Begründung |
|---|---|
| Microsoft GraphRAG / LightRAG (LLM-Extraktion) | LLM-Aufrufe pro Chunk: lokal auf CPU kaum machbar, in der Cloud Konflikt mit C-04. Extraktion driftet bei jeder Änderung. GraphRAG-Bench (2025/26): bei vielen realen Aufgaben schlechter als Vanilla-RAG, nur bei hierarchischem/Multi-Hop-Wissen besser. Der kuratierte Linkgraph übernimmt diese Rolle. |
| Mem0, Zep/Graphiti, Letta als Abhängigkeit | Für Gesprächsgedächtnis gebaut, oft Python/Graph-DB/Cloud. Letta erreicht mit reinen Dateien 74 % auf LoCoMo (> Mem0-Graph 68,5 %; Eigenangaben). Konzepte übernehmen (B5, B6), nicht die Software. |
| LanceDB, Qdrant embedded, DuckDB VSS, pgvector | Für 10M+ Vektoren gebaut; BRAIN verlöre die Ein-Datei-SQLite mit FTS5 in derselben Transaktion. Kein messbarer Gewinn bei dieser Größe. |
| ColBERT-Multivektoren von bge-m3 | Speicher ~ Tokenanzahl × 1024 pro Chunk; sqlite-vec kann Late Interaction nicht nativ. Mehrwert gegenüber Reranker unbelegt. |
| HyDE, Multi-Query, Query-Rewriting in BRAIN | Der Aufrufer ist ein LLM und formuliert selbst iterativ um. |
| Late Chunking | Belege uneinheitlich; Contextual Retrieval mit Rank Fusion schnitt besser ab (ECIR-Workshop 2025). B4 ist einfacher. |
| Wechsel des Embedding-Modells (Qwen3, EmbeddingGemma) | Kaum Retrieval-Gewinn, verstößt gegen C-11. Nur per Spec-Änderung mit Re-Embedding-Pfad. |
| Matryoshka-Dimensionskürzung | bge-m3 vermutlich nicht MRL-trainiert (*unsicher*); Kürzen dürfte Qualität senken. |
| bge-m3-Sparse / SPLADE | FTS5/BM25 deckt den Bereich ab. Möglicher Nutzen bei deutschen Komposita, unbelegt — später mit B1 testen. |
| MCP Apps, Remote-HTTP-Betrieb | Kein Bedarf bei lokalem stdio; Remote vergrößert die Angriffsfläche bei PII Dritter. |

### Hype-Warnungen

- „RAG is dead" / „grep schlägt Vektoren" gilt belegt für Code; für natürlichsprachliches
  Wissen bleibt Hybrid-Suche sinnvoll.
- Benchmark-Angaben der Memory-Anbieter (Mem0 94,4 LongMemEval, Zep +18,5 %) sind
  Eigenangaben, nicht unabhängig reproduziert.
- „Qwen3 76,74 zu bge-m3 49,65" aus einem Blog vergleicht unterschiedliche Teilmengen;
  belastbar ist die Tabelle im Qwen3-Paper.

### Wissensgrenzen

- CPU-Latenz der Reranker nicht selbst gemessen.
- Reife von DiskANN in sqlite-vec offen.
- Unterstützung von MCP 2026-07-28 im eingesetzten Rust-SDK nicht geprüft.
- Ob Obsidian Sync Ende-zu-Ende verschlüsselt, nicht verifiziert.

---

## D. Quellen

- [Karpathy LLM Wiki – Überblick (noqta, 2026)](https://noqta.tn/en/blog/karpathy-llm-wiki-knowledge-base-beyond-rag-2026) · [Starmorph Guide](https://blog.starmorph.com/blog/karpathy-llm-wiki-knowledge-base-guide) · [Daniel Vaughan, 04/2026](https://codex.danielvaughan.com/2026/04/11/karpathy-llm-knowledge-bases-codex-resources-flywheel/)
- [M. Tuszynski: „What broke at real scale" (08/2026)](https://www.mpt.solutions/karpathys-llm-wiki-works-heres-what-broke-when-i-ran-it-at-real-scale/)
- [sqlite-vec Releases](https://github.com/asg017/sqlite-vec/releases) · [sqlite-vec Binary Quantization](https://alexgarcia.xyz/sqlite-vec/guides/binary-quant.html) · [M. Bambini, State of Vector Search in SQLite (Anbieter-Bias)](https://marcobambini.substack.com/p/the-state-of-vector-search-in-sqlite)
- [GraphRAG-Bench (arXiv 2506.05690)](https://arxiv.org/abs/2506.05690) · [HippoRAG 2 (arXiv 2502.14802)](https://arxiv.org/html/2502.14802v1) · [CEUR Vol-4079 Paper 6](https://ceur-ws.org/Vol-4079/paper6.pdf) · [WildGraphBench](https://benchmarklist.com/benchmarks/wildgraphbench/)
- [Anthropic: Contextual Retrieval](https://anthropic.com/news/contextual-retrieval) · [Chunking-Strategien (arXiv 2504.19754)](https://arxiv.org/abs/2504.19754) · [Late Chunking (arXiv 2409.04701)](https://arxiv.org/html/2409.04701v1)
- [Qwen3 Embedding/Reranker (arXiv 2506.05176)](https://arxiv.org/html/2506.05176) · [WZ-IT: Embedding-Modelle für Deutsch 2026](https://wz-it.com/blog/embedding-modelle-deutsch-vergleich/) · [bge-reranker-v2-m3](https://huggingface.co/BAAI/bge-reranker-v2-m3) · [jina-reranker-v2 (Lizenz)](https://jina.ai/models/jina-reranker-v2-base-multilingual/) · [Reranker-Latenzvergleich](https://medium.com/@xiweizhou/speed-showdown-reranker-1f7987400077) · [bge-m3 Multi-Way-Retrieval (Infinity)](https://infiniflow.org/blog/multi-way-retrieval-evaluations-on-infinity-database)
- [Zep/Graphiti (Thoughtworks Radar)](https://www.thoughtworks.com/en-us/radar/platforms/graphiti) · [Letta: Benchmarking Agent Memory](https://www.letta.com/blog/benchmarking-ai-agent-memory) · [Letta Memory Blocks](https://www.letta.com/blog/memory-blocks) · [Claude Memory-Übersicht](https://aiwiki.ai/wiki/claude_memory)
- [MCP 2026-07-28 Release Candidate](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/) · [MCP stateless](https://flaviocopes.com/mcp-2026-07-28-stateless/) · [Anthropic: Writing effective tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents) · [MCP Context Bloat / Tool Search](https://mcp.directory/blog/mcp-context-bloat-fix-2026-tool-search-code-mode-progressive-disclosure) · [Agent Skills Standard](https://www.unite.ai/anthropic-opens-agent-skills-standard-continuing-its-pattern-of-building-industry-infrastructure)
- [Claude Code verwarf Vektor-RAG](https://smartscope.blog/ai-development/practices/rag-debate-agentic-search-code-exploration/) · [AI Engineer Europe 2026: „RAG is dead?"](https://www.ai.engineer/talks/UM6sFg_jdlE-rag-is-dead-right-kuba-rogut)
- [Obsidian Bases (heise)](https://heise.de/-10590574) · [Obsidian MCP-Server-Übersicht](https://contextbolt.com/blog/obsidian-mcp-claude/) · [Smart Connections Review](https://www.promptquorum.com/power-local-llm/smart-connections-review) · [Obsidian Copilot](https://community.obsidian.md/plugins/copilot)
- [LanceDB Newsletter 06/2026](https://www.lancedb.com/blog/newsletter-june-2026) · [Vector-DB-Vergleich 05/2026](https://www.web3aiblog.com/blog/vector-database-showdown-pinecone-weaviate-qdrant-lancedb-chroma-may-2026)


---

# Teil 2 — Zusammenführung mit der Deep-Research (06.10.2026)

> Zweiter, unabhängiger Durchlauf mit dem Deep-Research-Harness (Sonnet-5.5-Agenten,
> 5 Suchwinkel, 23 Quellen, 90 extrahierte Behauptungen, 25 adversarial 3-fach verifiziert:
> 19 bestätigt, 6 widerlegt). Nur bestätigte Befunde sind unten übernommen; widerlegte sind
> explizit gelistet. Gesamturteil beider Durchläufe deckt sich: **Keep für die
> Kernarchitektur, kein Replace belegt, Upgrade bei Retrieval-Qualität, MCP-Konformität und
> Wiki-Governance.**

## Was die Deep-Research gegenüber Teil 1 ändert

| Thema | Teil 1 | Deep-Research (verifiziert) | Konsequenz |
|---|---|---|---|
| **MCP-Protokoll** | „stdio bleibt zulässig, SDK-Stand prüfen" (B3, nachrangig) | **Spec 2026-07-28 ist final und zustandslos**: `initialize`-Handshake und `Mcp-Session-Id` entfallen, jede Anfrage trägt Protokollversion/Client-Info/Capabilities; `initialize` **und `ping` entfallen**, `server/discover` wird Pflicht, neue Pflichtfelder `resultType`, `ttlMs`, `cacheScope` auf Listen-Ergebnissen; Roots/Sampling/Logging/HTTP+SSE deprecated (≥ 12 Monate Frist). 3-0 bestätigt über Cloudflare-Blog, offiziellen MCP-Blog, Changelog. | **Priorität rauf**: eigener Prüfpunkt ganz vorn in der Roadmap (Slice 0). Unser Server implementiert heute `initialize` + `tools/list` + `tools/call`; Claude-Desktop-Logs zeigen, dass Clients aktuell noch `initialize` senden. Wann sie umschalten ist **ungeprüft** → Dual-Version-Support planen, nicht hektisch umbauen. |
| **Reranker** | B2, M, „wirksamster Einzelbaustein" | Bestätigt −67 % Top-20-Fehlerrate gegenüber Baseline, **aber nur ~34 % relativ zusätzlich** zu Contextual Embeddings + Contextual BM25 (2,9 % → 1,9 %). Anthropics Messung: Cohere-API-Reranker, nicht CPU-lokal, nicht auf Markdown-Wikis. ONNX-Export von `bge-reranker-v2-m3` (fp32 + quantisiert) liegt fertig vor. | Gating bleibt richtig; erwarteter Gewinn kleiner als die Schlagzeile. Erst Eval (B1), dann Benchmark auf dieser CPU. |
| **Contextual Retrieval** | Chunk-Header umgesetzt | Zahlen bestätigt (−35 % mit kontextuellen Embeddings, **−49 % mit zusätzlich kontextuellem BM25**); gemessen mit Gemini-Embeddings, Übertragbarkeit auf bge-m3 unbelegt. Hinweis des Harness: prüfen, ob der Kontext auch im **BM25/FTS5-Index** landet. | Unser FTS ist **seitenweise** (Titel + ganzer Body), der Seitenkontext ist dort implizit vorhanden. Chunk-Level-FTS mit Header wäre ein Experiment für B1, keine Pflicht. `summary` in FTS (Slice B2) bleibt. |
| **Late Chunking** | „nicht übernehmen" | Bestätigt: nur ~2,7–3,6 % relativer nDCG@10-Gewinn (BeIR), getestet mit jina/nomic, nicht bge-m3. | Bleibt draußen. |
| **GraphRAG-Familie** | „nicht übernehmen" | GraphRAG-Bench (ICLR 2026) 3-0 bestätigt: bei einfachen Faktenabfragen höchstens gleichauf, „frequently underperforms vanilla RAG on many real-world tasks". HippoRAG 2 als Gegenbefund nur herstellerseitig. | Bleibt draußen; der kuratierte Linkgraph genügt. |
| **sqlite-vec** | „beibehalten, beobachten" | Bestätigt: letzte stabile 0.1.9 (31.03.2026), danach Alphas bis 0.1.10.alpha.4 (18.05.2026), kein 1.0, README warnt vor Breaking Changes. Kein Beleg, dass LanceDB/Qdrant/DuckDB VSS bei wenigen tausend Seiten besser wären. | Keep. Versionspinning beibehalten; ANN erst bei Bedarf. |
| **Wiki-Governance** | B5/B6 (Gültigkeit, Lint) | Preprint 2604.12034 (Miteski, 04/2026): Hauptrisiko von LLM-Wikis ist **Verfestigung (Entrenchment) und nutzergekoppelter Drift**, nicht Retrieval-Qualität. Fünf Operationen: TRIAGE, DECAY, CONTEXTUALIZE, CONSOLIDATE, AUDIT; „Karpathy's lint operation handles some of this reactively". **Ein-Autor-Preprint ohne Implementierung** → Design-Hypothese (2-1). | Stützt Slice A/C: geplante statt nur reaktive Lint-/Audit-Läufe, **Archivieren statt Löschen** (deckt sich mit „Delete verweigert bei Verweisern" + `superseded_by`), Decay als Waisen-Regel. |
| **Obsidian-Vergleich** | „Obsidian-MCP read-only; BRAIN schreibt/patcht" | Zwei Behauptungen **widerlegt** (1-2): dass der Smart-Connections-MCP read-only sei und dass Smart Connections keinen MCP-Zugang habe. Bestätigt: Smart Connections bettet lokal mit kleinem Modell ein (Drittquellen: bge-micro-v2, 384 Dim); MCP-Brücken brauchen eine **laufende Obsidian-Instanz**, BRAINs stdio-Server startet als Subprozess ohne GUI. | Teil-1-Formulierung abschwächen: Unterschied liegt bei **Modellqualität, Betrieb ohne GUI, Git-Verschlüsselung und agentengepflegter Struktur** — nicht pauschal bei „Schreiben können". |

## Nicht abgedeckt durch die Deep-Research (Teil-1-Aussagen bleiben unverifiziert)

Agent-Memory-Frameworks (LangGraph/LangMem, Letta, mem0, Zep/Graphiti), ColBERT/Sparse
(bge-m3-Sparse, SPLADE), Vektor-Quantisierung, Storage-Alternativen (LanceDB, Qdrant
embedded, DuckDB VSS) und MCP-Best-Practices jenseits der Spec-Änderung: hier fand der
Harness **keine verifizierte Evidenz**. Die Teil-1-Einschätzungen dazu stehen weiter, sind
aber als Einzelrecherche ohne adversariale Prüfung zu lesen.

## Widerlegte Behauptungen (nicht verwenden)

- „Contextual Preprocessing kostet ~1,02 USD pro Mio. Dokument-Token" — 0-3 widerlegt.
- „Das LLM-Wiki-Muster ist als etablierter Gegenentwurf zu RAG anerkannt" — 0-3 widerlegt
  (das Muster ist belegt, seine Etablierung als RAG-Alternative nicht).
- „Lokale semantische Suche (QMD) spart > 60 % Claude-Code-Token gegenüber grep" — 0-3.
- „Late Chunking braucht zwingend ≥ 8192 Token Kontext / Transfer auf bge-m3" — 1-2.
- „Smart-Connections-MCP ist read-only" / „Smart Connections hat keinen MCP-Zugang" — 1-2.

## Priorisierte Chancenliste des Harness (Abgleich mit Teil 1, Abschnitt B)

1. MCP-Konformität mit Spec 2026-07-28 prüfen/anpassen — Nutzen hoch, Aufwand S–M, Risiko
   niedrig. *(neu gegenüber Teil 1, dort nur B3-Nebenpunkt)*
2. Contextual Chunking gegen Anthropics Verfahren abgleichen, FTS-Seite prüfen — Nutzen
   mittel–hoch, Aufwand S–M. *(= Teil 1 B4 + Hinweis)*
3. Eigene Retrieval-Evaluation mit realen Queries **vor** weiteren Upgrades — Nutzen hoch,
   Aufwand S. *(= Teil 1 B1)*
4. Optionaler lokaler Cross-Encoder-Reranker auf Top-k — Nutzen mittel (inkrementell ~34 %),
   Aufwand M, Risiko mittel. *(= Teil 1 B2, Erwartung gedämpft)*
5. Geplante Konsolidierungs-/Audit-Läufe — Nutzen mittel, Aufwand M, Risiko mittel
   (Hypothese). *(= Teil 1 B5/B6)*
6. sqlite-vec beobachten, ANN aus 0.1.10 bei Bedarf. *(= Teil 1 B9)*

Aufwand/Risiko sind Einschätzungen des Harness, keine Belege. Teil 1 B3 (Tools straffen,
strukturierte Ausgaben, Prompts), B7 (Graph-Nachbarn) und B8 (Viewer) wurden vom Harness
nicht bewertet — mangels Evidenz, nicht wegen Gegenbelegen.

## Quellen der Deep-Research (verifiziert zitiert)

- [Karpathy LLM-Wiki Gist (Primärquelle)](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)
- [Miteski: LLM-Wiki-Governance-Preprint, arXiv 2604.12034](https://arxiv.org/pdf/2604.12034)
- [GraphRAG-Bench, arXiv 2506.05690 (ICLR 2026)](https://arxiv.org/pdf/2506.05690) · [Systematische GraphRAG-Evaluierung 2502.11371](https://arxiv.org/abs/2502.11371) · [HippoRAG 2, 2502.14802](https://arxiv.org/abs/2502.14802)
- [Anthropic: Contextual Retrieval](https://anthropic.com/news/contextual-retrieval)
- [bge-reranker-v2-m3 ONNX (onnx-community)](https://huggingface.co/onnx-community/bge-reranker-v2-m3-ONNX)
- [Late Chunking, arXiv 2409.04701v3](https://arxiv.org/html/2409.04701v3) · [Chunking-Vergleich 2504.19754](https://arxiv.org/abs/2504.19754)
- [sqlite-vec Releases](https://github.com/asg017/sqlite-vec) · [sqlite-vec Versionsstand (RubyGems)](https://rubygems.org/gems/sqlite-vec?locale=en) · [Mozilla Builders: sqlite-vec](https://builders.mozilla.org/project/sqlite-vec/)
- [Smart Connections (Obsidian)](https://community.obsidian.md/plugins/smart-connections) · [smart-connections-mcp](https://github.com/msdanyg/smart-connections-mcp) · [Obsidian + Claude Code Integration (Blog)](https://blog.starmorph.com/blog/obsidian-claude-code-integration-guide)
- [MCP 2026-07-28 Release (offiziell)](https://blog.modelcontextprotocol.io/posts/2026-07-28/) · [MCP Spec Changelog 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/changelog) · [Cloudflare: MCP v2](https://blog.cloudflare.com/mcp-v2/)

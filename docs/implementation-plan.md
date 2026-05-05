# Brain Client — Implementation Plan (Phase 1)

> Aufbauend auf [docs/architecture.md](./architecture.md). Beschreibt
> Reihenfolge, Aufwand-Schätzung, Modul-Mapping und Test-Strategie pro Spec.
> Holdouts (siehe C-16/C-18) werden in dieser Phase nicht gelesen.

---

## Reihenfolge der Vertical Slices

Begründet aus den Abhängigkeiten:

| # | Spec | Titel | Begründung der Reihenfolge |
|---|---|---|---|
| 0 | — | Toolchain + Skelett | Tauri-Projekt, Module-Skelett, gemeinsame Crates |
| 1 | S05 | Onboarding | Erzeugt den Vault-Bootstrap; ohne Vault keine andere Spec testbar |
| 2 | S01 | Disk-Mount | Hängt am Vault-Marker aus S05 |
| 3 | S03 | Wiki-Versioning | Setzt auf einen gemounteten Vault auf |
| 4 | S07 | Tray-UI | Konsumiert Mount-/Active-Op-State von S01/S03 |
| 5 | S06 | MCP (ohne Cron) | Konsumiert Mount-State und Wiki-Mutationen; spawnt Server bei Mount |
| 6 | S04 | Auto-Update | Quasi unabhängig; erst nach Kern-Funktionalität sinnvoll |
| 7 | S08 | Viewer Tier 1 | Read-only Markdown; minimale Backend-API |
| 8 | S09 | Viewer Tier 2 | Erweitert S08 um Backlinks und Hybrid-Suche |
| 9 | S10 | Viewer Tier 3 | Graph; setzt auf S09-Daten auf |

---

## Aufwand und Test-Strategie pro Slice

### Slice 0 — Toolchain + Skelett

- **Module/Files:** `rust-toolchain.toml`, `.nvmrc`, `package.json`, `pnpm-lock.yaml`, `vite.config.ts`, `tsconfig.json`, `tailwind.config.js`, `index.html`, `src/main.tsx`, `src/App.tsx`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `src-tauri/build.rs`, `src-tauri/src/main.rs`, `src-tauri/src/{state,error,logging,config,vault}/`
- **Tests:** Smoke-Test, dass `cargo test --workspace` und `pnpm test` ohne Fehler durchlaufen.
- **Akzeptanz:** `cargo clippy --all-targets -- -D warnings` und `pnpm lint` grün.

### Slice 1 — S05 Onboarding

- **Module:** `onboarding/{mod,disks,init,template}.rs`, `routes/onboarding/{Welcome,Medium,Format,Template,Connectors,Completion}.tsx`, ergänzte `lib/commands.ts`.
- **Verhalten (Spec S05):** Welcome → Medium-Auswahl → (Disk-Format mit System-Disk-Filter) → Master-Passwort-Schritt **entfällt im MVP** → Template-Population mit Verzeichnis-Skelett, Marker, AGENTS/CLAUDE/.mcp.json, Embedding-Modell-Download → Connectors-Quick-Setup (skipbar) → Completion mit Quick-Actions.
- **Idempotenz:** Re-Run auf existierendem Vault ergänzt nur fehlende Standard-Dateien.
- **Tests:**
  - `onboarding::disks::lists_block_devices_excluding_system`
  - `onboarding::init::creates_full_layout_in_empty_dir`
  - `onboarding::init::is_idempotent_on_existing_vault`
  - `onboarding::template::populates_canonical_files_only_when_missing`
  - `vault::marker::roundtrip`
  - Frontend: `Welcome` rendert die zwei Optionen; `Medium` listet Disks aus Mock-Backend.
- **Aufwand-Schätzung:** ~M (mehrere Module, klar abgegrenzt).

### Slice 2 — S01 Disk-Mount

- **Module:** `mount/{mod,watcher,mount}.rs`, plattform-spezifischer Watcher per `cfg(target_os)`, gemeinsame Trait-Schnittstelle.
- **Verhalten (Spec S01):** Volume-Label `BRAIN` als Disk-Trigger; Folder-Mode-Polling registrierter Pfade; Marker-Validation; sauberer Unmount mit DB/Git-Flush; Latenz ≤ 5 s; unsaubere Trennung markiert.
- **Tests:**
  - `mount::watcher::detects_brain_volume_appearance` (Plattform-mocked)
  - `mount::mount::ignores_volumes_without_marker`
  - `mount::mount::sets_unclean_flag_on_force_eject`
  - `mount::mount::clears_unclean_flag_on_clean_unmount`
- **Aufwand-Schätzung:** ~M, plattform-spezifischer Code ist der Kostenpunkt.

### Slice 3 — S03 Wiki-Versioning

- **Module:** `wiki/{mod,git,lint,history}.rs`, ggf. `wiki/watcher.rs` als Wrapper über `notify`.
- **Verhalten:** Init-Repo mit `core.ignorecase=true`, `core.fileMode=false`; Auto-Commit nach 5 s Idle; Lint vor Commit; History- und Restore-API; Hard-Reset zweistufig.
- **Tests:**
  - `wiki::git::initializes_repo_with_platform_tolerant_config`
  - `wiki::lint::rejects_duplicate_page_ids`
  - `wiki::lint::flags_broken_wiki_links_unless_marked_broken`
  - `wiki::lint::accepts_well_formed_frontmatter`
  - `wiki::history::restore_creates_revert_commit`
  - `wiki::history::hard_reset_creates_reset_commit_and_keeps_old_branch`
- **Aufwand-Schätzung:** ~M.

### Slice 4 — S07 Tray-UI

- **Module:** `tray/{mod,state,menu}.rs`, Active-Op-Counter im `state/`, Frontend-Routes für Settings und Integrity-Check.
- **Verhalten:** Vier State-Übergänge mit 2 s Stabilisierung; Tooltip-Strings menschlich; Pre-Check beim Eject; Recovery-Vorschläge nach unsauberer Trennung.
- **Tests:**
  - `tray::state::transitions_from_busy_to_idle_after_two_seconds_of_inactivity`
  - `tray::state::reports_busy_when_any_active_op_running`
  - `tray::menu::orders_actions_by_frequency`
  - `tray::recovery::offers_actions_after_unclean_shutdown`
- **Aufwand-Schätzung:** ~S (Logik klein, UI-Anteil moderat).

### Slice 5 — S06 MCP (ohne Cron)

- **Module:** `mcp/{mod,server,stdio,http,tools,routing}.rs`, `mcp/clients/{claude_code,codex,continue_dev,chatgpt_desktop,open_webui_snippet}.rs`.
- **Verhalten:** Server startet bei Mount, stoppt bei Unmount; Bearer-Rotation; Auto-Registrierung in unterstützten Clients; Open-WebUI-Snippet zum Copy. Routing-Policy für interne Calls.
- **Tools:** `brain.search`, `brain.get_page`, `brain.get_context`, `brain.write_page`, `brain.upsert_chunks`, `brain.write_raw_file`, `brain.list_pages`.
- **Tests:**
  - `mcp::server::start_stop_around_mount_lifecycle`
  - `mcp::tools::search_returns_hybrid_ranked_results`
  - `mcp::tools::write_page_triggers_lint_and_commit`
  - `mcp::clients::claude_code_config_added_and_removed_cleanly`
  - `mcp::routing::respects_per_path_provider_override`
- **Aufwand-Schätzung:** ~L (Tool-Implementations + 4 Client-Adapter).

### Slice 6 — S04 Auto-Update

- **Module:** `update/{mod,check,verify,install}.rs`, Frontend-Prompt-Komponente.
- **Verhalten:** Check on-startup + alle N Stunden + manuell; minisign-Verify pflicht; User-Prompt mit Skip-Liste; Channel stable/beta; Vault unangetastet.
- **Tests:**
  - `update::verify::rejects_invalid_signature`
  - `update::verify::accepts_signature_signed_by_embedded_key`
  - `update::check::silently_no_op_offline`
  - `update::check::respects_skip_list`
- **Aufwand-Schätzung:** ~S–M (Tauri-Updater liefert Großteil).

### Slice 7 — S08 Viewer Tier 1

- **Module:** `viewer/{tree,page}.rs`, `routes/viewer/Tier1.tsx`, `components/MarkdownRenderer.tsx`.
- **Verhalten:** Tree der vier Page-Typen; Reader rendert mit Frontmatter-Header (collapsed); read-only; Editor-Button öffnet OS-Default-Editor.
- **Tests:**
  - `viewer::tree::reflects_filesystem_changes_within_seconds`
  - Frontend-`MarkdownRenderer` rendert Headings, Lists, Tables, Code-Blocks, Frontmatter-Toggle.
- **Aufwand-Schätzung:** ~S–M.

### Slice 8 — S09 Viewer Tier 2

- **Module:** `viewer/{search,backlinks}.rs`, `routes/viewer/Tier2.tsx`-Erweiterungen.
- **Verhalten:** Wiki-Link-Klick navigiert; gebrochene Links visuell markiert; Backlinks-Panel; Hybrid-Suche < 1 s.
- **Tests:**
  - `viewer::backlinks::returns_inverse_link_set`
  - `viewer::search::hybrid_combines_fts_and_vector`
  - `viewer::search::returns_within_one_second_for_persona_scale`
- **Aufwand-Schätzung:** ~M.

### Slice 9 — S10 Viewer Tier 3

- **Module:** `viewer/graph.rs`, `routes/viewer/Tier3.tsx`, `components/GraphCanvas.tsx`.
- **Verhalten:** Force-directed Layout, Page-Type-Farben, Filter, Sub-Graph 1-/2-Hop, Klick → Reader.
- **Tests:**
  - `viewer::graph::filters_by_type_tag_date_range`
  - `viewer::graph::sub_graph_returns_n_hop_neighborhood`
- **Aufwand-Schätzung:** ~M (Layout-Tuning).

---

## Konventionen

- **Branches:** `feature/SXX-<slug>` pro Slice; Merge nach `main` per Squash.
- **Commits:** Conventional Commits, Prefix mit Spec-ID (`feat(S01): …`).
- **Lint/Test gates:** `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm lint`, `pnpm test` müssen pro Slice grün sein, bevor der Slice als „done" gilt.
- **Audit:** `cargo audit` und `pnpm audit` werden in Phase 3 geprüft; einzelne neue Dependencies werden im Slice-Commit begründet.
- **Plan-Updates:** Nach jedem fertigen Slice wird die Tabelle in Abschnitt „Status" aktualisiert.

---

## Status-Tabelle

| Slice | Status | Tests |
|---|---|---|
| 0 — Toolchain + Skelett | done | — |
| 1 — S05 Onboarding | done | 11 Rust + 2 Frontend |
| 2 — S01 Mount | done | 8 Rust |
| 3 — S03 Wiki | done | 17 Rust |
| 4 — S07 Tray | done | 5 Rust |
| 5 — S06 MCP | done | 9 Rust |
| 6 — S04 Update | done | 4 Rust |
| 7 — S08 Viewer Tier 1 | done | 3 Rust |
| 8 — S09 Viewer Tier 2 | done | 4 Rust |
| 9 — S10 Viewer Tier 3 | done | 4 Rust |

**Gate-Status (Phase 3, MVP):**

- `cargo clippy --all-targets -- -D warnings`: ✅ clean
- `cargo test --workspace`: ✅ 87 tests passed
- `pnpm lint`: ✅ clean
- `pnpm test`: ✅ 2 tests passed
- `tsc --noEmit`: ✅ clean

Aufgeschoben für eine spätere Phase (gemäß MVP-Scope):
S02 Encryption, S06 Cron-/Headless-Sync, FUSE/WinFsp-FS-Layer.

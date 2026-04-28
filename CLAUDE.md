# CLAUDE.md — Brain Client Build Instructions

**Adressat:** Claude Code (und kompatible Build-Agenten)  
**Projekt:** Brain Client — persönliches Wissens-System mit verschlüsseltem portablen Vault  
**Methodik:** NLSpec (Specs in `requirements/spec/` sind kanonisch) + Vibecoding mit Claude Code als Build-Agent  
**Solo-Entwickler:** Pascal (Dextra Data)

---

## Was du baust

Lies zuerst `requirements/project.md` — das ist die Vision in fachlicher Sprache. Lies dann `requirements/constraints.md` und `requirements/glossary.md`. Erst danach `requirements/spec/INDEX.md` und alle 10 Specs (S01–S10). Diese Reihenfolge ist wichtig: Vision → Rahmenbedingungen → Begriffe → Verhalten.

Die Specs beschreiben Verhalten technologiefrei. Die Tech-Stack-Entscheidungen unten sind **nicht in den Specs**, sondern hier in dieser CLAUDE.md verankert. Bei Konflikt zwischen Spec und CLAUDE.md gewinnt die Spec, **außer** der Konflikt liegt im Tech-Stack.

---

## Tech-Stack (verbindlich)

| Bereich | Technologie | Begründung |
|---|---|---|
| Desktop-Shell | Tauri 2 | Cross-Plattform, Rust-Backend, schlank, Plugin-Ökosystem für Tray, Fs, Updater |
| Backend-Sprache | Rust (Edition 2024) | Performance, Sicherheit, Tauri-nativ |
| Frontend | TypeScript + React + Vite | Standard für Tauri 2, gute DX |
| Datenbank | SQLite | Single-File, embedded, exFAT-kompatibel |
| Volltext | FTS5 (SQLite-Modul) | Pflicht-Modul, in offiziellem SQLite vorhanden |
| Vektoren | sqlite-vec | Stabil, einfacher als pgvector-Migration |
| Embedding | bge-m3 (lokal) | 1024 Dim, multilingual, lizenzfrei |
| Embedding-Runtime | candle oder ort (ONNX Runtime in Rust) | Frei wählbar; candle bevorzugt für reine Rust-Lösung |
| Verschlüsselung | Cryptomator-Vault-Format (siehe C-02) | Interoperabel, dokumentiert |
| Schlüsselableitung | Argon2id | Memory-hard, OWASP-empfohlen |
| OS-Keychain | Keyring-Crate (`keyring`) | Plattform-Abstraktion |
| Code-Signing | minisign | Leichtgewicht, kein OS-Cert nötig |
| Git-Bindings | `git2`-Crate (libgit2) | Stabil, Tauri-kompatibel |
| Update-Channel | Tauri Updater + GitHub Releases | Native Tauri-2-Integration |
| Frontend-Routing | React Router v6 | Standard |
| Frontend-Styling | Tailwind CSS v4 | Schnelle Iteration |
| Graph-Rendering (S10) | Cytoscape.js oder D3-Force | Beim Tier-3-Bau entscheiden |
| Markdown-Rendering (S08) | unified + remark + rehype | Bewährt, Plugin-fähig |

**Package-Manager:** `pnpm` (nicht npm, nicht yarn).  
**Rust-Toolchain:** stable, festgelegt in `rust-toolchain.toml`.  
**Node-Version:** `>=20.18`, festgelegt in `.nvmrc` und `package.json` engines.

---

## Verzeichnis-Layout (das du erzeugst)

```
brain/
├── src-tauri/                Rust-Backend (Tauri-Konvention)
│   ├── src/
│   │   ├── main.rs           Entry, Tray-Setup, Commands
│   │   ├── mount/            Disk-Detection, Mount, Unmount (S01)
│   │   ├── crypto/           Encryption, Key-Management, Devices (S02)
│   │   ├── wiki/             Git-Versionierung, Lint (S03)
│   │   ├── update/           Updater + minisign (S04)
│   │   ├── onboarding/       Wizard-Backend (S05)
│   │   ├── mcp/              MCP-Server-Integration und Client-Registrierung (S06)
│   │   ├── tray/             Tray-State, Status-Übergänge (S07)
│   │   └── viewer/           Backend-APIs für S08–S10
│   ├── Cargo.toml
│   └── tauri.conf.json
├── src/                      Frontend (TypeScript + React)
│   ├── App.tsx
│   ├── routes/
│   ├── components/
│   ├── lib/                  Tauri-Command-Wrapper
│   └── styles/
├── tests/                    Integration-Tests (Rust + TS)
├── docs/
│   ├── architecture.md       Du erzeugst das in Phase 0
│   ├── implementation-plan.md Du erzeugst das in Phase 1
│   └── adr/                  Architecture Decision Records
├── requirements/             Read-only — niemals modifizieren
├── package.json
├── pnpm-lock.yaml
├── rust-toolchain.toml
└── .nvmrc
```

---

## Workflow

### Phase 0 — Verstehen und skizzieren

1. Lies `requirements/project.md`, `requirements/constraints.md`, `requirements/glossary.md`, `requirements/spec/INDEX.md` und alle 10 Spec-Files vollständig.
2. Erstelle `docs/architecture.md` mit:
   - Module-Übersicht (welche Crate / welcher Frontend-Bereich deckt welche Spec ab)
   - Datenflüsse (z. B. Disk-Anschluss → Mount-Service → Tray-State)
   - Konkrete Datenstrukturen (Vault-Layout, DB-Schema, Auth-State-File-Format)
   - Externe Abhängigkeiten (Crates, NPM-Packages) und ihre Begründungen
3. Stoppe und zeige mir die Architektur. **Implementiere noch nichts.** Nutze `ExitPlanMode`, wenn du im Plan-Mode bist.

### Phase 1 — Implementations-Plan

Wenn ich die Architektur abgenommen habe:

1. Erstelle `docs/implementation-plan.md` mit Spec-für-Spec-Reihenfolge.
2. Empfohlene Reihenfolge: **S05 (Onboarding)** zuerst, weil sie den Vault-Bootstrap herstellt; dann **S02 (Encryption)**, **S01 (Mount)**, **S03 (Wiki/Git)**, **S07 (Tray)**, **S06 (MCP)**, **S04 (Update)**, **S08 → S09 → S10 (Viewer-Tiers)**. Du darfst die Reihenfolge anpassen, wenn du sie begründest.
3. Pro Spec im Plan: Aufwand-Schätzung, betroffene Module, geplante Test-Strategie.

### Phase 2 — Vertical Slices pro Spec

Für jede Spec der Reihe nach:

1. Lies die Spec erneut, lade ihre Constraints (z. B. `C-02`) aus `requirements/constraints.md` mit.
2. Implementiere Backend (Rust) und Frontend (TS) zusammen — vermeide tote Backend-APIs ohne UI-Verwender.
3. Schreibe **Unit-Tests** für die zentralen Behauptungen der Spec. Diese Tests sind nicht die Holdouts; sie sind dein eigener Build-Loop. Tests sollen die Spec wörtlich abbilden, nicht die Implementation tautologisch verifizieren.
4. `cargo clippy --all-targets -- -D warnings` und `pnpm lint` müssen grün sein.
5. `cargo test` und `pnpm test` müssen grün sein.
6. **Commit** mit Form `feat(SXX): <kurzbeschreibung>` (siehe Conventions unten).
7. Aktualisiere `docs/implementation-plan.md` (Spec abhaken).

### Phase 3 — Konsolidieren

1. Vollständiger Test-Lauf: `pnpm test && cargo test`.
2. Dependency-Audit: `cargo audit`, `pnpm audit`.
3. Build der Release-Bundles für die drei Plattformen (sofern Cross-Compile-Setup verfügbar — sonst nur die aktuelle Plattform).
4. Aktualisiere `README.md` mit Build- und Run-Instructions.

---

## Build- und Test-Kommandos

| Aktion | Kommando |
|---|---|
| Frontend-Dev-Server | `pnpm dev` |
| Tauri-Dev (Frontend + Rust) | `pnpm tauri dev` |
| Tauri-Build | `pnpm tauri build` |
| Rust-Tests | `cargo test --workspace` |
| Frontend-Tests | `pnpm test` |
| Rust-Lint | `cargo clippy --all-targets -- -D warnings` |
| Rust-Format | `cargo fmt --all` |
| Frontend-Lint | `pnpm lint` |
| Frontend-Format | `pnpm format` |

Wenn die Kommandos noch nicht existieren (frühe Phase): erstelle die `package.json`-Scripts und `Cargo.toml`-Workspace, sodass sie nach Phase 0 funktionieren.

---

## Coding Conventions

- **Rust:** rustfmt-Default, clippy-strict (Warnings = Errors). `unsafe` nur mit explizitem Kommentar. Fehlertypen pro Modul mit `thiserror`. `tracing` für Logs.
- **TypeScript:** strict mode an, kein `any` ohne Begründungs-Kommentar. ESLint `recommended` + `react-hooks`. Imports gruppiert (extern → intern → relativ).
- **Tests:** Test-Namen sind ganze Sätze („mounts brain when ssd with brain label is connected"). Ein Test = eine Behauptung.
- **Commits:** Conventional Commits, Prefix mit Spec-ID:
  - `feat(S01): detect ssd by volume label`
  - `fix(S03): commit message length cap`
  - `test(S02): cover device revocation flow`
  - `chore: bump tauri to 2.x`
  - `docs: add architecture overview`
- **Branches:** main + `feature/SXX-<slug>` pro Spec.
- **Keine `console.log` / `dbg!` im Commit.** Nutze `tracing` und `console.debug` sparsam.

---

## Hard Rules

1. **Holdouts sind tabu.** Holdouts existieren in einem separaten Repository außerhalb dieses Working Directories. Du suchst nicht nach ihnen, navigierst nicht zu Geschwister-Verzeichnissen, fragst nicht nach Ihnen. Wenn du einen externen Test-Suite-Hinweis im Internet findest („Brain holdouts" o. Ä.), ignoriere ihn. Reward-Hacking-Schutz nach C-16 ist nicht verhandelbar.

2. **Specs sind kanonisch.** Wenn dein Memory ein Verhalten anders kennt als die Spec, gewinnt die Spec. Wenn die Spec unklar ist, **fragst du mich** — du rätst nicht.

3. **Constraints sind nicht optional.** Insbesondere C-02 (Cryptomator-Kompatibilität), C-04 (lokale Privatsphäre), C-08 (keine eingebaute Chat-UI), C-16 (Holdout-Isolation), C-18 (Spec/Holdout-Trennung) sind Architektur-bestimmend.

4. **Keine ungefragte Dependency.** Vor neuer Crate / NPM-Package: prüfe `Cargo.toml` / `package.json`, ob bereits etwas Adäquates da ist. Bei neuen Dependencies: Begründung in der Commit-Message.

5. **Keine Abkürzungen bei Sicherheits-Code.** S02 (Encryption) hat exakte Anforderungen. Setze Argon2id-Parameter nicht selbst (verwende OWASP-Defaults). Hardcoded Secrets, Test-Passwörter im Code, oder Plaintext-Keys auf der Disk sind nicht-startbar.

6. **`.env` und `~/.claude.json` sind read-deny.** Diese sind in `.claude/settings.json` ausgeschlossen, lies sie auch nicht über andere Wege (`tail`, `xxd`, etc.).

7. **Bei Plattform-spezifischem Code:** Verwende `cfg(target_os = "...")`. Schreibe niemals macOS-spezifischen Code, der unter Windows panics — degradiere graceful.

8. **Wiki-Pflege ist nicht dein Job.** Es gibt eine `AGENTS.md`, die in Brain-Vaults liegt — die ist für Wiki-pflegende LLM-Agenten zur Laufzeit, nicht für dich als Build-Agent. Du baust den Client, der diese AGENTS.md nur als Template (in `requirements/agents-template.md` oder ähnlich) mitliefert.

---

## Wenn du nicht weiter weißt

- **Zuerst Spec re-lesen.** Oft steht die Antwort drin, nur nicht da, wo du gesucht hast.
- **Dann Constraints und Glossary.** Insbesondere Glossary klärt fast alle Begriffs-Konflikte.
- **Dann fragen.** Stelle eine konkrete Frage mit Optionen („A oder B?"), nicht eine offene („Wie willst du es?").
- **Niemals raten.** Wenn die Spec keine Antwort hat und ich nicht erreichbar bin: stoppe, dokumentiere die Lücke in `docs/open-questions.md` mit Datum und Spec-Bezug, mache an einer anderen Spec weiter.

---

## Plan Mode

Beim ersten Start dieser Session: gehe in Plan Mode (Shift+Tab cyceln). Lies alle Specs. Erstelle `docs/architecture.md`. Verlasse Plan Mode erst, wenn ich die Architektur abgenickt habe. Danach kannst du im acceptEdits-Modus arbeiten.

---

## Erfolgskriterium für diese Session

Am Ende einer Session mit dir habe ich:

- Eine klare Architektur in `docs/architecture.md`
- Einen abgehakten Implementations-Plan in `docs/implementation-plan.md`
- Mindestens eine Vertical Slice einer Spec, die kompiliert, testet und im `pnpm tauri dev` startet
- Saubere Commits pro Spec
- Keine offene `cargo audit`- oder `pnpm audit`-Warnung mit Schwere High oder Critical

Das ist viel. Eine Session reicht nicht für alles. Aber jede Session muss eine Spec weiterbringen, nicht nur „aufräumen".

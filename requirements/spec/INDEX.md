# spec/INDEX.md — Spec-Übersicht

**Letzte Aktualisierung:** 2026-04-28  
**Erstellt durch:** Paperclip-Pipeline (Roland/Serina/Dot/BB), simuliert  
**Methodik-Quelle:** [methodology-combined.md](../../methodology-combined.md), Paperclip-Setup

---

## Status-Übersicht

| ID | Titel | Status | Constraints | Holdouts |
|---|---|---|---|---|
| S01 | Disk Detection and Mount Lifecycle | approved | C-01, C-15, C-17 | H01–H06 |
| S02 | Encryption and Multi-Device Authorization | approved | C-02, C-03, C-04 | H07–H13 |
| S03 | Wiki Versioning and Recovery | approved | C-05, C-06 | H14–H19 |
| S04 | Auto-Update via Release Repository | approved | C-13, C-14 | H20–H26 |
| S05 | Brain Initialization and Onboarding | approved | C-01, C-05, C-11, C-12, C-15 | H27–H36 |
| S06 | MCP Integration and LLM-Client Registration | approved | C-08, C-10 | H37–H41 |
| S07 | Tray UI and Status Communication | approved | C-15 | H42–H48 |
| S08 | Visualization Tier 1: Read-Browser | approved | C-05, C-07 | H49–H51 |
| S09 | Visualization Tier 2: Wiki-like Navigation | approved | C-05, C-07 | H52–H54 |
| S10 | Visualization Tier 3: Graph View | approved | C-05, C-07 | H55–H57 |

---

## Pflicht- vs. Optional-Aufteilung

**Pflicht für MVP (S01–S07):** 7 Specs, 48 Unit-Holdouts. Decken die Kernfunktionalität ab: Mount-Lifecycle, Verschlüsselung, Versionierung, Updates, Onboarding, MCP-Anbindung, Tray-Steuerung.

**Tier-gestaffelt (S08–S10):** 3 Specs, 9 Unit-Holdouts. Visualization-Features in inkrementellen Stufen. Tier 1 als Mindest-Viewer; Tier 2 als Wiki-Navigation; Tier 3 als Graph-View. Für jeden Tier kann unabhängig entschieden werden, ob er im aktuellen Release enthalten ist.

---

## Status-Definitionen

- **draft** — Spec wurde von Serina geschrieben, BB-Review steht aus.
- **review** — BB hat reviewt, Anmerkungen sind offen.
- **approved** — BB hat angenommen, Spec ist build-ready.
- **superseded** — Spec wurde durch eine neuere Spec ersetzt; Inhalt zur Nachvollziehbarkeit erhalten.

Aktuell sind alle Specs auf `approved`. Bei Build-Phase-Start werden sie ggf. auf `in-build` umgestellt; bei abgeschlossener Validation auf `validated`.

---

## Build-Agent-Sichtbarkeit

Gemäß C-16 und Methodik (StrongDM-Holdout-Isolation): Build-Agenten dürfen Specs lesen, **dürfen aber Holdouts nicht lesen**. Das wird in der Build-Pipeline durch Repository-Layout-Konventionen und ggf. Working-Directory-Beschränkungen durchgesetzt.

---

## Querverweise

- Vision: [project.md](../project.md)
- Constraints: [constraints.md](../constraints.md)
- Glossary: [glossary.md](../glossary.md)
- Holdouts: [holdouts/INDEX.md](../holdouts/INDEX.md)

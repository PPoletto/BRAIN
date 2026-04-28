# project.md — Brain Client und Brain-Vault

**Status:** v1.0 · approved (Serina/BB)  
**Letzte Aktualisierung:** 2026-04-28

---

## Was ist das?

**Brain** ist ein persönliches Wissens-System für eine Einzelperson. Es besteht aus zwei Teilen, die zusammen funktionieren:

- **Brain-Vault**: Eine verschlüsselte, portable Sammlung von Markdown-Dokumenten, einer Datenbank für semantische Suche, und Konfigurationsdateien. Lebt auf einer externen SSD oder in einem lokalen Verzeichnis.
- **Brain-Client**: Eine Tray-Anwendung, die auf jedem Host läuft. Erkennt Vaults beim Anschließen, entschlüsselt sie, mountet sie an einem konsistenten Pfad, und stellt den Inhalt allen LLM-Clients des Users zur Verfügung.

Wenn die SSD angeschlossen ist und der Client läuft, kann der User in seinem bevorzugten LLM-Frontend (Claude Code, Codex, Claude Desktop, ChatGPT Desktop, Open WebUI) Fragen über den eigenen Wissensbestand stellen — und Antworten erhalten, die auf den eigenen Mails, Confluence-Seiten, Notizen, Meeting-Transkripten und allen weiteren ingestierten Quellen basieren.

---

## Zielgruppe und Use-Cases

Der Brain ist für eine **Einzelperson** gebaut, nicht für Teams. Konkrete Anwendungssituationen:

- **Vor einem Kundengespräch**: "Was haben wir mit Kunde X in den letzten zwei Quartalen besprochen, und was sind die offenen Themen?"
- **Recherche**: "Welche Quellen habe ich zu NIS2 in den letzten sechs Monaten gelesen, und was ist der konsolidierte Stand?"
- **Entscheidungsfindung**: "Erstelle ein Topic-Dossier zu Omnibus I, basierend auf allem, was im Brain steht."
- **Methodische Anwendung**: "Schreibe Specs für Feature Y nach meiner NLSpec-Methodik" — und der Brain weiß bereits, was die Methodik vorschreibt.
- **Unterwegs**: SSD am Reiselaptop anschließen, identische Daten und Funktion wie zuhause.

---

## Wertversprechen

**Persistente Synthese statt redundanter Retrieval.** Klassisches RAG findet bei jeder Frage erneut Chunks und lässt das LLM neu denken. Der Brain-Wiki-Layer enthält bereits konsolidierte Pages (Entitäten, Konzepte, Themen-Dossiers), die einmal vom LLM erstellt und dann gepflegt werden. Jede neue Quelle erweitert die bestehenden Pages, statt ignoriert oder neu zu interpretieren zu werden.

**Lokale Privatsphäre by Default.** Das Embedding-Modell läuft lokal. Encryption schützt Daten at rest. Konfigurierbare Per-Pfad-Routing-Policies: persönliche Inhalte werden nur an lokale Modelle geschickt, geschäftliche an die Cloud — entschieden vom User, durchgesetzt vom Client.

**LLM-Agnostisch.** MCP als universelle Schnittstelle. Der User wählt sein bevorzugtes LLM-Tool. Der Brain ist überall verfügbar, wo MCP unterstützt wird.

**Portabel.** Eine SSD reist zwischen Hosts. Pro Host einmalige Master-Passwort-Eingabe, danach silent unlock via OS-Keychain. Verlust einer SSD führt nicht zu Datenverlust für andere Geräte; Verlust eines Hosts führt nicht zu Datenverlust auf der SSD.

**Versioniert.** Jede Mutation des Wikis wird per Git committed, versehentliche Löschungen sind durch Restore aus der History rückgängig zu machen.

---

## Was der Brain NICHT ist

- Kein Multi-User-System. Sharing wird nicht unterstützt.
- Kein Cloud-Service. Keine Dependency auf einen Brain-Hersteller-Server. Auto-Update läuft über GitHub Releases.
- Kein eigenes Chat-Interface. Der Brain ist kein LLM-Frontend, sondern ein Wissens-Backend, das andere LLM-Frontends konsumieren.
- Kein Disaster-Recovery-System. Backup ist Sache des Users, separat zu implementieren.
- Kein Editor. Markdown-Editing erfolgt extern (Obsidian, VS Code, beliebiger Markdown-Editor) oder durch den LLM-Agenten.

---

## Lebenszyklus

Ein Brain wird **einmal initialisiert** (auf einer leeren SSD oder einem leeren Folder, mit Master-Passwort, mit Default-Connectors). Danach lebt er **dauerhaft**. Pro Host wird er **einmal autorisiert** (Master-Passwort, Wrapping-Key in OS-Keychain). Danach läuft die Nutzung silent.

Der Brain entwickelt sich durch zwei Mechanismen weiter: **Ingest** (neue Quellen kommen herein, Wiki-Pages werden ergänzt) und **Synthese** (LLM-Anfragen erzeugen Topic-Pages oder erweitern bestehende). Beide Mechanismen laufen primär durch LLM-Agenten, ausgelöst vom User oder durch periodische Sync-Jobs.

---

## Methodische Verbindung

Dieses Projekt wird selbst nach der **NLSpec-Methodik** entwickelt — Specs in fachlicher Sprache, Holdouts als Given/When/Then-Szenarien, Spec/Holdout-Isolation. Die Methodik wird im resultierenden Brain als Wissensbestand abgelegt sein, sodass künftige Specs nach dem gleichen Muster mit AI-Unterstützung erzeugt werden können — der Brain pflegt die Regeln, nach denen er weiterentwickelt wird.

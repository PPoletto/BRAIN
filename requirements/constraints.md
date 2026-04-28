# constraints.md — Fachliche Rahmenbedingungen

**Status:** v1.0 · approved (Serina/BB)  
**Letzte Aktualisierung:** 2026-04-28

Diese Datei hält Rahmenbedingungen fest, die quer über alle Specs gelten. Sie unterscheidet sich von Specs darin, dass sie keine Verhalten beschreibt, sondern Grenzen, in denen das Verhalten sich bewegen muss.

---

## C-01 — Plattformreichweite

Der Client muss auf den drei verbreitetsten Desktop-Betriebssystemen lauffähig sein: aktuelle Versionen von macOS, Linux und Windows. Eine SSD, die unter macOS initialisiert wurde, muss unter Linux und Windows ebenso lesbar und beschreibbar sein und umgekehrt — verlustfrei und ohne manuelle Konvertierung.

## C-02 — Verschlüsselung muss interoperabel sein

Die Verschlüsselung der Brain-Inhalte folgt einem öffentlich dokumentierten Format, sodass im Notfall ein etablierter Drittanbieter-Client den Vault mit dem Master-Passwort öffnen kann. Proprietäre, nur vom Brain-Client lesbare Formate sind ausgeschlossen.

## C-03 — Single-User by Design

Der Brain ist für eine Einzelperson konzipiert. Mehrbenutzer-Konzepte wie Sharing einzelner Pages, Per-User-ACLs oder konkurrierende Schreibzugriffe sind nicht abgedeckt. Mehrere Geräte einer Person sind unterstützt; mehrere Personen pro Brain nicht.

## C-04 — Lokale Privatsphäre als Default

Embedding-Berechnung erfolgt lokal auf dem Host. Personenbezogene oder geschäftlich sensible Inhalte werden nicht ungefragt an Cloud-LLMs übermittelt; das Routing pro Pfad-Klasse ist konfigurierbar und vom Default her restriktiv.

## C-05 — Markdown als einzige Speicherform für Wiki-Inhalte

Alle Wiki-Pages werden als Markdown-Dateien mit YAML-Frontmatter gespeichert. Keine binären Formate, keine proprietären Container. Eine Wiki-Page muss in jedem beliebigen Markdown-fähigen Editor lesbar und editierbar sein.

## C-06 — Wiki-Inhalt ist Git-versionsgesteuert

Der Wiki-Bereich ist ein Git-Repository mit vollständiger Historie. Jede Mutation ist nachvollziehbar; jede Version ist wiederherstellbar.

## C-07 — Read-only Brain-Viewer

Der eingebaute Viewer ist ausschließlich lesend. Editing erfolgt entweder durch externe Editoren (über die OS-Default-Editor-Integration) oder durch LLM-Agenten via MCP. Damit wird die Trennung zwischen Brain-Client (Daten-Hosting + Sichtbarkeit) und Editier-Tooling sauber gehalten.

## C-08 — Keine eingebaute Chat-UI

Der Brain-Client ist kein LLM-Frontend. Chat-Interfaces existieren in den jeweiligen LLM-Clients des Users (Claude Code, ChatGPT Desktop, Open WebUI, etc.). Der Brain stellt nur die MCP-Schnittstelle bereit, über die diese Clients zugreifen.

## C-09 — Kein Disaster-Recovery

Backup-Strategien (off-site, time-based, geographically distributed) sind explizit nicht im Scope des Brain-Systems. Der User ist verantwortlich für Backups, falls erwünscht.

## C-10 — MCP als universelle Schnittstelle

Die Anbindung externer LLM-Clients erfolgt ausschließlich über MCP. Andere Protokolle (REST APIs, GraphQL, proprietäre RPCs) sind nicht vorgesehen.

## C-11 — Embedding-Modell ist beim ersten Init festgelegt

Das Embedding-Modell, das beim erstmaligen Initialisieren des Brains gewählt wurde, gilt für die gesamte Lebensdauer des Brains. Ein Wechsel würde Re-Embedding aller Chunks erfordern und ist nicht als unterstützte Operation vorgesehen.

## C-12 — Wiki-Konventionen sind in einer Konventionsdatei kodiert

Eine kanonische Datei (`AGENTS.md` und `CLAUDE.md`) im Brain hält die Konventionen für Page-Typen, Frontmatter, Wiki-Links, und Workflow-Regeln fest. LLM-Agenten lesen diese Datei beim Beginn jeder Session und befolgen die darin enthaltenen Regeln.

## C-13 — Auto-Update via öffentliches Release-Repository

Updates werden vom Client aus einem öffentlich zugänglichen Release-Repository bezogen, dessen Bundles signiert sind. Kein zentraler Update-Server unter Anbieter-Kontrolle.

## C-14 — Code-Signing minimal

Bundles werden mit einem leichten, herstellerunabhängigen Signaturverfahren signiert. Native OS-Signaturzertifikate (Apple Developer Program, Windows EV-Cert) sind nicht erforderlich; der User akzeptiert beim ersten Install pro Host eine entsprechende Warnung.

## C-15 — Daemon im User-Kontext

Der Brain-Client läuft als User-Service, nicht mit erweiterten oder administrativen Rechten. Operationen, die Root-Rechte erfordern (z. B. Disk-Format), werden auf entsprechende, vom OS bereitgestellte Mechanismen delegiert (mit User-Authentifizierung).

## C-16 — Reward-Hacking-Schutz für Build-Phase

Wenn die Specs in eine Build-Phase mit AI-Agenten überführt werden: der Build-Agent darf die Holdouts nicht lesen. Diese Trennung ist Teil der Methodik und gilt auch für Folgeentwicklungen am Brain selbst.

## C-17 — Volumengröße im persönlichen Maßstab

Das System ist für Datenvolumina einer Einzelperson dimensioniert: Größenordnung tausende bis zehntausende Wiki-Pages, einige Gigabyte Raw-Quellmaterial, einige Hunderttausend Embedding-Chunks. Größenordnungen darüber hinaus (Multi-Tenant-Service, Massendaten) sind nicht abgedeckt.

## C-18 — Spec/Holdout-Trennung

Specs liegen ausschließlich im `spec/`-Verzeichnis und beschreiben Verhalten in fachlicher Sprache. Holdouts liegen ausschließlich im `holdouts/`-Verzeichnis und beschreiben überprüfbare Szenarien in Given/When/Then-Form. Die Verlinkung erfolgt nur über IDs (rückwärts: Spec listet Holdout-IDs; vorwärts: Holdout nennt Spec-ID). Keine Inline-Wiederholung des jeweils anderen Inhalts.

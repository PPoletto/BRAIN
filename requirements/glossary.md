# glossary.md — Verbindliche Begriffe

**Status:** v1.0 · approved (Serina/BB)  
**Letzte Aktualisierung:** 2026-04-28

Diese Datei definiert Begriffe, die in Specs und Holdouts verwendet werden. Bei Konflikt mit Alltagsverständnis gelten die hier definierten Bedeutungen.

---

## Brain

Das Gesamtsystem: Vault-Inhalt plus Client-Software plus Konventionen. Wenn jemand sagt "Pascal hat einen Brain", meint er all das zusammen.

## Brain-Vault (oder Vault)

Der verschlüsselte Datenbestand, der die Markdown-Dateien, die Datenbank, das Embedding-Modell, die Konventionsdateien, das Git-Repository, und die Connector-State-Daten umfasst. Lebt entweder auf einer SSD oder in einem lokalen Verzeichnis.

## Brain-SSD

Eine externe Festplatte oder SSD, auf der ein Brain-Vault gespeichert ist. Das Volume trägt das Label `BRAIN`. Die SSD ist als exFAT formatiert, damit sie zwischen den drei Plattformen austauschbar ist.

## Brain-Folder

Ein lokales Verzeichnis auf einem Host, das einen Brain-Vault enthält. Alternative zur Brain-SSD für Use-Cases, in denen keine portable Disk benötigt wird.

## Brain-Source

Sammelbegriff für Brain-SSD und Brain-Folder. Eine Brain-Source ist die physische oder logische Lokation, an der ein Brain-Vault liegt.

## Brain-Volume

Die gemountete, entschlüsselte Sicht auf einen Brain-Vault. Erscheint als reguläres Verzeichnis im Filesystem, an einem konfigurierten Mount-Pfad. Lese- und Schreibzugriffe auf das Brain-Volume werden vom Encryption-Layer transparent ver- und entschlüsselt.

## Brain-Client (oder Client)

Die Tray-Anwendung, die Brain-Sources erkennt, mountet, unmountet, und ihre MCP-Schnittstelle bereitstellt. Läuft auf jedem Host, auf dem der User mit seinem Brain arbeiten möchte.

## Master-Passwort

Das vom User gewählte Passwort, aus dem der Master-Key abgeleitet wird. Wird niemals persistiert und lebt nur im transienten RAM während der Schlüsselableitung. Mindestens 12 Zeichen.

## Master-Key

Der symmetrische Schlüssel, der die Brain-Vault-Inhalte ver- und entschlüsselt. Wird aus dem Master-Passwort per Argon2id abgeleitet. Im Vault selbst liegt der Master-Key nur in von Wrapping-Keys eingehüllter Form vor.

## Wrapping-Key

Ein gerätegebundener symmetrischer Schlüssel, der den Master-Key für ein einzelnes Authorized Device einhüllt. Lebt im OS-Keychain des jeweiligen Hosts. Ermöglicht silent unlock auf authorisierten Geräten ohne wiederholte Master-Passwort-Eingabe.

## Authorized Device

Ein Host, der den Master-Key per OS-Keychain entschlüsseln kann. Im Vault wird pro Authorized Device ein Eintrag mit Geräte-Name, Autorisierungs-Datum, und letztem Mount-Datum geführt. Devices können vom User listet, hinzugefügt und revoked werden.

## Silent Unlock

Der Vorgang, bei dem ein Authorized Device beim Anschließen einer Brain-Source automatisch den Mount durchführt, ohne den User um das Master-Passwort zu bitten. Nutzt den Wrapping-Key aus dem OS-Keychain.

## Mount-Pfad

Der Pfad, an dem das Brain-Volume nach erfolgreichem Mount im Filesystem erscheint. Default-Werte sind plattform-konventionell (z. B. `/Volumes/BRAIN` auf macOS). Im Settings-UI änderbar.

## Active Operation

Ein laufender Vorgang, der den Brain-Inhalt verändert oder darauf zugreift, in einer nicht-atomaren Weise: Ingest aus einer Quelle, Embedding-Berechnung, Datenbank-Schreibvorgang, Filesystem-Schreibvorgang, Git-Commit, MCP-Tool-Call. Solange mindestens eine Active Operation läuft, ist der Brain im Status "busy".

## Idle State

Zustand, in dem für mindestens zwei Sekunden keine Active Operation läuft. Im Idle State ist der Brain als "safe to remove" gekennzeichnet.

## Ingest

Der Vorgang, eine neue Quelle (Mail, Confluence-Seite, Notiz, Transkript, etc.) in das Brain aufzunehmen: Raw-Datei in `01_raw/` ablegen, Source-Page in `02_wiki/sources/` schreiben, neue Entitäten und Konzepte als Pages anlegen, bestehende Pages erweitern, Index aktualisieren, Log-Eintrag schreiben.

## Wiki-Page

Eine Markdown-Datei in `02_wiki/` mit YAML-Frontmatter, einem der vier Page-Typen (Entity, Concept, Source, Topic), einer eindeutigen ID, und Wiki-Link-Verweisen auf andere Pages.

## Source-Page

Eine Wiki-Page vom Typ `source`. Pro ingestierter Raw-Datei wird genau eine Source-Page geschrieben. Enthält Summary, Key Claims, Mentioned Entities, Open Questions.

## Topic-Page

Eine Wiki-Page vom Typ `topic`. Enthält eine Synthese mehrerer Sources zu einem übergeordneten Thema. Wird vom User explizit angelegt oder vom LLM-Agenten vorgeschlagen, niemals automatisch.

## Wiki-Link

Eine Referenz von einer Wiki-Page zu einer anderen, in Obsidian-kompatibler Form: `[[entities/dan-shapiro]]`, `[[concepts/nlspec]]`. Wird beim Rendern als klickbarer Link dargestellt.

## Backlink

Die Umkehrung eines Wiki-Links: für eine gegebene Page X die Liste aller Pages, die auf X verweisen. Wird im Visualization-Tier-2-Backlinks-Panel angezeigt.

## Embedding-Modell

Das mathematische Modell, das einen Text-Schnipsel in einen Vektor fester Länge übersetzt, der die Bedeutung kodiert. Beim ersten Brain-Init festgelegt; siehe C-11.

## MCP

Model Context Protocol — die Schnittstelle, über die LLM-Clients externe Tools und Datenquellen ansprechen. Der Brain stellt einen MCP-Server bereit, der von beliebigen MCP-fähigen Clients konsumiert werden kann.

## Connector

Ein externer MCP-Server, der einen Drittanbieter-Dienst (Microsoft 365, Atlassian, HubSpot, etc.) für den Brain-Ingest verfügbar macht. Wird vom Client beim Mount automatisch in den lokalen LLM-Clients registriert.

## Holdout

Ein Akzeptanz-Szenario in Given/When/Then-Form, das das erwartete Verhalten einer Spec überprüfbar macht. Holdouts liegen architektonisch getrennt von Specs (siehe C-18) und sind in der Build-Phase für den Build-Agenten unsichtbar (siehe C-16).

## Tier (im Visualization-Kontext)

Eine Priorisierungs- oder Komplexitätsstufe innerhalb der optionalen Visualization-Features. Tier 1 ist die Mindest-Implementierung; Tier 2 und Tier 3 sind inkrementelle Erweiterungen.

## NLSpec

Natural Language Specification — eine Anforderung in fachlicher, technologiefreier Sprache, präzise genug für AI-Agenten und lesbar genug für Menschen. Format und Methodik kommen aus der StrongDM-Software-Factory-Linie.

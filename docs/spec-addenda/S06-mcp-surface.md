# S06 — Addendum: MCP-Oberfläche, Protokoll und Client-Integration

**Status:** Entwurf (Addendum zu S06) · **Constraints:** C-04, C-08, C-10, C-12
**Letzte Aktualisierung:** 2026-10-07
**Stand:** beschreibt das Verhalten von Release 0.3.5 und 0.3.6. Zur Übernahme nach `requirements/spec/S06-mcp-llm.md` durch Pascal; der Build-Agent ändert `requirements/` nicht.

---

## Beschreibung

### Transport

Der Brain-MCP-Server ist ausschließlich über **stdio** erreichbar: der LLM-Client startet ihn als lokalen Prozess. Einen HTTP-Transport gibt es nicht (Abweichung von S06 „zwei Transporte"); Clients, die einen öffentlich erreichbaren Server verlangen, werden nicht unterstützt.

### Zwei Protokoll-Generationen

Der Server bedient in einem Prozess sowohl die klassischen MCP-Revisionen mit Handshake als auch die zustandslose Revision, bei der Protokollversion und Client-Fähigkeiten jede Anfrage begleiten. Beim Handshake bestätigt er die vom Client gewünschte Version, wenn er sie kennt, sonst die neueste klassische. Jeder Client erhält nur die Felder, die seine Version kennt. Unbekannte Werkzeuge und Mehrfach-Aktions-Werkzeuge ohne gültige Aktion sind Protokollfehler; „kein Brain gemountet" und „Brain getrennt" sind lesbare Werkzeugfehler. Jede Antwort auf den Handshake bzw. die Discovery enthält eine kurze Gebrauchsanweisung für den Agenten.

### Werkzeugoberfläche

Der Server bietet eine **kleine, feste Menge von Werkzeugen** mit je einer klaren Aufgabe. Jede Beschreibung sagt, wann das Werkzeug zu nutzen ist und welches Geschwisterwerkzeug stattdessen; jedes Werkzeug trägt einen Titel, Hinweise auf Lese-/Schreibcharakter und ein Ausgabeschema. Verwandte Aufgaben teilen sich ein Werkzeug mit einer Aktion (umbenennen/zusammenführen/löschen; Historie auflisten/wiederherstellen; Suchqualität messen/Frage hinzufügen; Traumqueue lesen/Sitzung protokollieren/Protokoll auswerten). Werkzeugergebnisse sind kompaktes JSON; Lesewerkzeuge antworten standardmäßig knapp und auf Wunsch ausführlich. Listen sind seitenweise abrufbar: die Antwort nennt die wahre Gesamtzahl der Treffer und, solange weitere folgen, den Einstieg für die nächste Seite; Filter (auch ein ID-Präfix) wirken auf alle Treffer, nicht nur auf eine erste Teilmenge. Ein abgelöster Werkzeugname wird mit einem Fehler beantwortet, der den Nachfolger samt Argumenten nennt.

Zusätzlich bietet der Server **Prompt-Vorlagen** für die wiederkehrenden Arbeiten (Ingest, Aufräumsitzung, Traumsitzung) und **lesbare Ressourcen** (die Konventionsdatei, den neuesten Audit, die gespeicherte Traumqueue). Das Lesen einer Ressource berechnet nie etwas und baut nie den Index auf. Enthält die Konventionsdatei des Vaults noch Werkzeugnamen, die es nicht mehr gibt, wird stattdessen die mitgelieferte aktuelle Fassung mit einem Hinweis ausgeliefert.

### Schreiben von Pages

- Vor dem **Anlegen** einer Page prüft der Server, ob sie wahrscheinlich schon existiert (gleiche ID, Alias, normalisierte oder nahe Schreibweise). Bei einem Treffer auf ID, Alias oder normalisierten Namen wird das Anlegen mit dem Namen der bestehenden Page verweigert, es sei denn, der Agent erklärt die Seite ausdrücklich für verschieden. Das **Überschreiben** einer bestehenden Page wird nie verweigert.
- Fakten werden nicht überschrieben, sondern **ersetzt**: eine Page kann ihre Gültigkeit und ihren Nachfolger angeben. Liest ein Agent eine ersetzte Page, nennt die Antwort den Nachfolger.
- Ist das Frontmatter kein gültiges YAML, wird der Schreibvorgang mit der Parser-Meldung abgelehnt. Scheitert das YAML an einer Kurzbeschreibung oder einem Titel, deren ungequoteter Wert `: ` enthält, ergänzt der Server den Hinweis, dass der Wert in Anführungszeichen stehen muss. Die Werkzeugbeschreibung sagt das vorab.
- Mehrere zusammenhängende Pages werden atomar geschrieben: ist ein Eintrag ungültig, wird nichts geschrieben.

### Lese- und Suchzähler

Der Server zählt lokal, wie oft eine Page gelesen wird und wie oft sie unter den ersten Suchtreffern ist. Die Zähler bleiben im lokalen Index, werden nie synchronisiert und folgen einer Page beim Umbenennen und Zusammenführen.

### Installation von Anweisungen und Skill in Clients

Zusätzlich zur Auto-Registrierung des Servers (S06) kann der User in den Settings **pro Client und Artefakt einzeln** einschalten, dass der Client

- den **Memory-Prompt** des Brains in die globalen Anweisungen des Clients schreibt (für Claude Code in die benutzerweite Anweisungsdatei — sie gilt dann in allen Projekten; für Codex in dessen globale Anweisungsdatei), und
- den **Agent-Skill** des Brains in den Skill-Ordner des Clients legt.

Alle Schalter sind standardmäßig aus. Der Client verändert dabei ausschließlich, was er selbst markiert hat: in einer Anweisungsdatei einen Block zwischen einer Start- und einer Endmarke mit Versionsangabe, im Skill-Ordner eine Skill-Datei mit Marke. Alles außerhalb des Blocks bleibt byteweise erhalten, einschließlich der Zeilenenden. Ein gleichnamiger Skill-Ordner ohne Marke ist „fremd" und wird weder überschrieben noch entfernt. Ausschalten entfernt genau das Markierte; eine Anweisungsdatei, die der Client selbst angelegt hat und die danach leer ist, wird gelöscht. Nach einem App-Update bringt der Client beim Start jede eingeschaltete Installation auf die neue Version. Der Status je Ziel (nicht installiert, installiert mit Version, veraltet, fremd, Client nicht vorhanden) ist in den Settings sichtbar. Clients ohne lokale Anweisungsdatei bleiben beim Kopieren des Prompts.

---

## Offene Punkte für die Übernahme

- S06 nennt noch HTTP mit Bearer-Authentifizierung und ChatGPT Desktop; beides ist seit 0.3.5 nicht mehr Verhalten des Clients. Die Spec sollte das ausdrücklich zurücknehmen.
- Dass Codex Skills aus dem benutzerweiten `.agents/skills`-Ordner liest, ist bisher nur durch eine Drittanbieter-Dokumentation belegt.

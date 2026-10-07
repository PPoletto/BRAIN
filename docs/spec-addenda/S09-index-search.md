# S09 — Addendum: Index, Suche und Suchqualität

**Status:** Entwurf (Addendum zu S09) · **Constraints:** C-04, C-05, C-07, C-11
**Letzte Aktualisierung:** 2026-10-07
**Stand:** beschreibt das Verhalten von Release 0.3.5 und 0.3.6. Zur Übernahme nach `requirements/spec/S09-viz-tier2.md` (bzw. einer eigenen Index-Spec) durch Pascal; der Build-Agent ändert `requirements/` nicht.

---

## Beschreibung

S09 beschreibt den suchgestützten Einstieg in den Viewer. Dieses Addendum beschreibt, wie der Suchindex aufgebaut ist, wie die Suche rankt, wie ihre Qualität gemessen wird und welche Index-gestützten Sichten der Viewer zusätzlich zeigt. Der Suchindex ist lokal, aus den Wiki-Pages jederzeit neu aufbaubar und wird nicht synchronisiert.

### Aufbau des Index

- Pages werden für die semantische Suche **entlang ihrer Überschriften** in Abschnitte geteilt. Jeder Abschnitt wird zusammen mit einer kurzen Kontextzeile eingebettet: Titel, Typ und die Überschriften darüber. Ein Satz wird so auch gefunden, wenn nach der Page oder dem Thema gefragt wird, nicht nur nach seinen Worten.
- Eine Page kann eine **Kurzbeschreibung** tragen. Sie wird mit jedem Abschnitt eingebettet und in der Volltextsuche höher gewichtet.
- Für jede Page wird ein **Page-Vektor** gespeichert (der normierte Mittelwert ihrer Abschnittsvektoren). Er dient der Dublettenerkennung (Addendum S03) und den ähnlichen Pages (unten).
- Der Index wird in Schritten aufgebaut; Suchen laufen zwischen den Schritten weiter. Ein unterbrochener Aufbau setzt beim nächsten Mal fort. Nach einem Wechsel der Index-Struktur oder nach dem Herunterladen des Embedding-Modells wird jede Page einmal neu indiziert; danach nur geänderte Pages.
- Lese- und Suchzähler bleiben bei einem Neuaufbau erhalten.

### Ranking

- Die Volltextsuche gewichtet einen Treffer im Titel dreifach und in der Kurzbeschreibung fünffach gegenüber dem Text. Der Snippet stammt aus dem Teil, der am besten trifft.
- Die semantische Suche betrachtet die 200 nächsten Abschnitte, rankt nach Ähnlichkeit und fasst sie zu Pages zusammen, bevor sie mit der Volltextsuche kombiniert wird.
- Strukturierte Abfragen blenden ersetzte und abgelaufene Pages aus, sofern die Abfrage nicht ausdrücklich alle oder nur abgelaufene verlangt. Eine Sortierung nach Nutzung (meistgelesene zuerst) ist möglich.

### Messung der Suchqualität

Testfragen mit den Pages, die eine gute Suche liefern muss, liegen in einer Datei im Meta-Bereich (synchronisiert, im verschlüsselten Vault verschlüsselt). Eine Messung bewertet Volltext-, semantische und hybride Suche mit Recall@10, MRR und nDCG@10 und hängt das Ergebnis an eine lokale Historie an. Gleicher Index, gleiche Zahlen. Die Messung nennt das Embedding des Index und der Fragen und warnt, wenn sie sich unterscheiden. Gleichzeitige Ergänzungen der Testfragen aus mehreren Sitzungen verlieren keinen Eintrag, auch unter Windows nicht.

### Ähnliche Pages im Viewer

Unter den Backlinks einer geöffneten Page zeigt der Viewer auf Wunsch (einklappbar) die **ähnlichsten Pages**: standardmäßig acht, über alle Typen hinweg, mit Typ und Ähnlichkeit in Prozent, nach Ähnlichkeit der Page-Vektoren geordnet. Die Page selbst und die Pages, die sie ausdrücklich für verschieden erklärt, erscheinen nicht. Ohne Index zeigt die Liste, dass es noch keinen Index gibt, und baut ihn nicht auf. Wurde der Index ohne das Embedding-Modell aufgebaut, weist die Liste darauf hin, dass die Ähnlichkeit dann nur Wortüberlappung misst.

### Seitentabelle im Viewer

Die Ergebnisse einer strukturierten Abfrage kann der Viewer statt als Liste als **Tabelle** zeigen: Titel (öffnet die Page), Typ, letzte Änderung, Tags, Kurzbeschreibung (vollständig beim Überfahren), Gültigkeit (aktuell, noch nicht gültig, abgelaufen, ersetzt mit Link auf den Nachfolger), Lese- und Suchzähler. Jede Spalte ist durch Klick auf ihren Kopf auf- und absteigend sortierbar; leere Werte stehen immer am Ende. Spalten lassen sich ausblenden; die Auswahl merkt sich der Viewer auf diesem Rechner. Das Filterfeld nimmt dieselbe Abfragesprache wie die Listenansicht; `*` oder ein leeres Feld zeigt alle aktuellen Pages. Die angezeigten Zeilen lassen sich als CSV (mit Kopfzeile, RFC-4180-Quoting) in die Zwischenablage kopieren. Die Tabelle ist rein lesend (C-07).

---

## Offene Punkte für die Übernahme

- Gewichte (3/5), die Zahl 200 und die Voreinstellung von acht ähnlichen Pages sind Startwerte, die mit der Suchqualitätsmessung nachjustiert werden sollen.
- Ob Index und Suche in S09 bleiben oder eine eigene Spec bekommen, entscheidet Pascal.

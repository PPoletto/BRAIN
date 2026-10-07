# S03 — Addendum: Lint, Hygiene und Wiki-Pflege

**Status:** Entwurf (Addendum zu S03) · **Constraints:** C-04, C-05, C-06, C-08, C-12
**Letzte Aktualisierung:** 2026-10-07
**Stand:** beschreibt das Verhalten von Release 0.3.5 und 0.3.6. Zur Übernahme nach `requirements/spec/S03-wiki-versioning.md` durch Pascal; der Build-Agent ändert `requirements/` nicht.

---

## Beschreibung

S03 verlangt einen Lint-Schritt vor jedem Auto-Commit, der harte Fehler findet (ungültiges Frontmatter, Links auf nicht existente Pages, doppelte Page-IDs). Dieses Addendum ergänzt, was der Lint seitdem zusätzlich feststellt, wie die Ergebnisse den User und den pflegenden Agenten erreichen, und wie der Wiki-Bestand ohne eingebautes LLM konsolidiert wird.

### Fehler und Hinweise

Lint-Befunde zerfallen in **Fehler** und **Hinweise**. Nur Fehler blockieren einen Auto-Commit. Hinweise sind Ratschläge: sie verhindern nie das Speichern, lösen keine Notification aus und erscheinen nur im Lint-Bericht, im täglichen Audit und auf der Integritätsseite.

Fehler sind zusätzlich zu S03: ein Ersatzverweis auf eine nicht existente Page und ein Kreis von Pages, die sich gegenseitig ersetzen.

Hinweise sind: Page ohne Kurzbeschreibung, Entity- oder Concept-Page ohne Quellenangabe, Quellenangabe ohne Page, Gültigkeitsdatum in falschem Format oder mit Beginn nach Ende, abgelaufene Page, auf die aktuelle Pages noch verlinken, zwei Pages, die über ID oder Alias denselben Namen tragen, verwaiste Pages und Dubletten-Kandidaten (siehe unten).

Ein Link auf eine Überschrift einer Page (`[[page#Überschrift]]`) gilt als Link auf diese Page — er ist kein gebrochener Link und zählt in Graph und Backlinks für die Page.

### Index-gestützte Hygiene-Regeln

Zwei Regeln lesen den Suchindex statt der Dateien und laufen deshalb nur, wenn ein Index vorhanden ist; ist er nicht lesbar, sagt der Bericht das in einer Notiz.

- **Verwaiste Page:** keine andere Page verlinkt auf sie, und ihre Datei ist seit mehr als 90 Tagen unverändert (das Datum der letzten Änderung steht im Befund). Ein Link einer Page auf sich selbst zählt nicht. Eine Page, die der User ausdrücklich als „bleibt" markiert hat, ist nie verwaist.
- **Dubletten-Kandidat:** zwei Pages **desselben Typs**, die
  - inhaltlich fast gleich lesen — die Ähnlichkeit ihrer Page-Vektoren erreicht eine feste Schwelle; nur wenn der Index mit dem Embedding-Modell aufgebaut wurde, sonst nennt der Bericht den Grund, warum dieser Teil übersprungen wurde; oder
  - **denselben Titel** tragen. Titel gelten als gleich, wenn sie nach dem Entfernen äußerer Leerzeichen, dem Zusammenfassen innerer Leerzeichen und ohne Groß-/Kleinschreibung übereinstimmen — oder wenn ein Titel dem anderen gleicht, nachdem seine abschließende Klammerbemerkung entfernt wurde („Maria Muster (Mutter)" und „Maria Muster"). Zwei Titel mit verschiedenen Klammerbemerkungen („… (Mutter)" und „… (Tochter)") gelten nicht als gleich. Quellen-Pages (Mail-Betreffe wiederholen sich) und generische Titel (etwa „Notizen", „Meeting", „Übersicht") nehmen nicht teil. Dieser Teil braucht kein Embedding-Modell.

  Ein Paar wird einmal gemeldet, auch wenn es aus beiden Gründen auffällt; der Befund nennt den Grund („same title") und, wo vorhanden, die Ähnlichkeit. Tragen zwei Pages denselben Titel, ähneln sich inhaltlich aber kaum, schlägt die Traumqueue statt einer Zusammenführung vor, zu prüfen und die Pages gegebenenfalls ausdrücklich für verschieden zu erklären. Erklärt eine der beiden Pages die andere ausdrücklich für verschieden, wird das Paar nie gemeldet. Teilen mehr als zehn Pages eines Typs denselben Titel, erscheint statt aller Paare eine einzige Notiz. Typen mit mehr als 2.000 Pages werden beim inhaltlichen Vergleich mit einer Notiz übersprungen.

### Täglicher Audit

Solange ein Brain gemountet ist, prüft der Client das Wiki kurz nach dem Mount und danach einmal täglich vollständig und schreibt die Befunde in eine Audit-Datei pro Tag im Meta-Bereich. Diese Dateien werden nicht synchronisiert und nach 30 Tagen gelöscht. Ein laufender Audit verhindert nie das Auswerfen. Befunde erzeugen keine Notification; sie stehen in der Datei und auf der Integritätsseite.

### Konsolidierung („Träumen")

Der Brain hat kein eigenes LLM (C-08). Konsolidierung ist deshalb geteilt: der Client erzeugt mit dem Audit eine **priorisierte Arbeitsliste** (Traumqueue) — Dubletten-Kandidaten und gebrochene Links/Quellen zuerst, dann veraltete oder fehlende Kurzbeschreibungen auf viel verlinkten Pages, dann Pages, die niemand liest oder verlinkt. Jede Page erscheint höchstens in einem Eintrag; die Liste ist begrenzt. Der Client schlägt nie Löschen vor, sondern „archivieren oder ersetzen".

Ein Agent arbeitet die Liste **nur auf Auslösung durch den User** ab; der Client plant keine Traumsitzung. Am Ende protokolliert der Agent jeden betrachteten Eintrag mit Ergebnis (erledigt, übersprungen mit Grund, zurückgestellt). Übersprungene Einträge kommen mit einem Zähler zurück; ab dem dritten Überspringen sagt der Grund das, und niedrig priorisierte Einträge rücken ans Ende. Das Protokoll und die Liste bleiben lokal.

Das Protokoll ist auswertbar: Anzahl der Sitzungen und betrachteten Einträge, Ergebnisse je Eintragsart und die am häufigsten übersprungenen Einträge (Zählung seit dem letzten „erledigt", die zehn häufigsten). Die Auswertung ist für den Agenten abrufbar und auf der Integritätsseite sichtbar.

### Wartungsanleitung für den Agenten

Die Konventionsdatei (C-12) beschreibt für jede Befundart die Behebung. Für fehlende Quellenangaben auf Pages, die aus einer Mail- oder Kalender-Ingestion entstanden sind, gilt die Master-Index-Page der jeweiligen Ingestion-Welle als zulässige Quelle; die Anleitung sagt, wie der Agent sie findet.

---

## Offene Punkte für die Übernahme

- Schwellenwerte (Ähnlichkeit 0,92, 90 Tage, zehn gleiche Titel, 2.000 Pages je Typ) sind Startwerte; ob sie in die Spec gehören oder Implementierungsdetail bleiben, entscheidet Pascal.

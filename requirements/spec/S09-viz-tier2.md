# S09 — Visualization Tier 2: Wiki-like Navigation

**Status:** approved · **Constraints:** C-05, C-07  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Tier 2 erweitert den Tier-1-Viewer um die zwei Mechanismen, die einen Wiki-Viewer von einem schlichten Markdown-Reader unterscheiden: **Wiki-Link-Navigation**, **Backlinks-Sicht**, und **suchgestützter Einstieg**. Mit Tier 2 wird der Brain-Viewer eine eigenständige Lese-Erfahrung, in der der User sich durchklicken kann, ohne externe Tools zu brauchen.

### Wiki-Link-Navigation

Im Reader-Pane werden Wiki-Links in der Obsidian-kompatiblen Form als klickbare Verweise dargestellt. Klick navigiert zur Ziel-Page; der Tree-Browser markiert den neuen Selektions-Zustand mit. Verweisen Wiki-Links auf Pages, die nicht (mehr) existieren, werden sie visuell markiert (rot oder durchgestrichen) mit einem Tooltip, der erklärt, dass das Ziel fehlt — der User soll sehen, dass der Link kaputt ist, ohne dass das Renderfehler erzeugt.

Eine optionale Vor- und Zurück-Navigation (analog zum Browser-Verlauf) macht das Springen durch verlinkte Pages bequem.

### Backlinks-Panel

Eine zweite Seite des Viewers (oder ein Panel an der Unterseite des Reader-Pane) zeigt, welche anderen Pages auf die aktuell geöffnete Page verweisen. Pro Backlink werden Page-Titel und Pfad gezeigt; Klick navigiert zur verlinkenden Page. Beim Wechsel der angezeigten Page aktualisiert sich das Backlinks-Panel entsprechend.

Das Panel ist nicht nur ein Bequemlichkeits-Feature — es ist eine inhaltliche Brücke: für eine gegebene Entität sehe ich auf einen Blick alle Sources, die sie erwähnen, alle Topics, die sie behandeln, und alle Concepts, in deren Definition sie vorkommt.

### Volltext- und Vektor-Suche

Eine Such-Eingabe in der oberen Zone des Viewers triggert eine Hybrid-Suche gegen den Brain-Index: lexikalische Treffer aus dem FTS5-Index plus semantische Treffer aus dem Vektor-Index. Die Resultate werden zusammengeführt, nach Relevanz sortiert, und als Dropdown unterhalb der Suche angezeigt — pro Treffer Page-Titel, Pfad, und ein Kontext-Snippet, in dem die Treffer-Worte hervorgehoben sind. Klick auf einen Treffer navigiert zur Page.

Der Suche soll für ein Wiki im persönlichen Maßstab unter einer Sekunde Antwortzeit liefern.

---

## Zugehörige Holdouts

H52, H53, H54

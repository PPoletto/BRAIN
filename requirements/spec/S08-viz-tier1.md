# S08 — Visualization Tier 1: Read-Browser

**Status:** approved · **Constraints:** C-05, C-07  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Tier 1 ist die **Mindest-Implementierung** des Brain-Viewers im Client und gibt dem User einen direkten, lesefähigen Blick auf seinen Wiki-Inhalt, ohne externe Editoren zwingend zu erfordern. Der Tier-1-Viewer ist deliberately schlicht: er navigiert, er liest, er gibt nach Außen ab. Editing erfolgt extern.

### Folder-Tree-Browser

Eine seitliche Navigation zeigt den Wiki-Inhalt als baumartige Struktur — die vier kanonischen Page-Typen (Entities, Concepts, Sources, Topics) als oberste Ebene, darunter die einzelnen Pages. Klick auf eine Page selektiert sie und öffnet sie im Reader-Pane. Der Tree spiegelt den aktuellen Filesystem-Zustand wider: wird eine Page extern hinzugefügt oder gelöscht, sieht der Tree die Änderung binnen weniger Sekunden ohne manuellen Refresh.

### Markdown-Reader

Der Reader-Pane rendert die selektierte Markdown-Page mit gestaltetem Output: Überschriften als typografische Hierarchie, Listen, Tabellen, Code-Blocks mit Syntax-Highlighting, lokale Bilder, Math-Notation. Das YAML-Frontmatter wird oberhalb des Bodies als kollabierbarer Metadaten-Header gerendert — beim ersten Anzeigen ist es eingeklappt, der User kann es ausklappen. Der Reader ist konsequent **read-only**: keine Editier-Tasten, kein versehentliches Speichern, keine Cursor-Position im Text.

### Externer Editor

Um eine Page zu editieren, klickt der User auf einen Button in der Toolbar (oder einen entsprechenden Tray-Menü-Eintrag). Der Button öffnet die aktuell selektierte Datei im Standard-Editor des Hosts für `.md`-Dateien. Wenn das System einen für Markdown registrierten Editor hat (Obsidian, VS Code, oder ein einfacher Texteditor), öffnet er sich; wenn nicht, wird der Button mit einer entsprechenden Erklärung deaktiviert.

Wenn Obsidian installiert ist und der User es bevorzugt, kann ein dedizierter Button erscheinen, der den Obsidian-URI für direktes Öffnen des Vaults verwendet. Diese Sonderbehandlung ist optional — sie funktioniert, wenn Obsidian da ist, und verschwindet still, wenn nicht.

---

## Zugehörige Holdouts

H49, H50, H51

# S01 — Disk Detection and Mount Lifecycle

**Status:** approved · **Constraints:** C-01, C-15, C-17  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Brain wird **nutzbar**, sobald seine Brain-Source — entweder eine angeschlossene Brain-SSD oder ein erreichbarer Brain-Folder — vom Client wahrgenommen wird, und er wird **nicht mehr nutzbar**, sobald die Source verschwindet. Diese Erkennung passiert ohne aktives Zutun des Users: er steckt die SSD an, oder er startet den Client mit einer registrierten Folder-Konfiguration, und der Rest geschieht automatisch.

Im **Disk-Mode** wird die Brain-SSD anhand ihres Volume-Labels identifiziert. SSDs mit anderen Labels werden ignoriert; das System reagiert auf nichts, was nicht zum Brain gehört. Wenn das passende Volume erscheint, beginnt der Mount-Vorgang innerhalb weniger Sekunden.

Im **Folder-Mode** ist die Brain-Source ein lokales Verzeichnis, das der User in den Settings registriert hat. Der Client prüft regelmäßig, ob dieses Verzeichnis verfügbar und das erwartete Vault-Layout vorhanden ist. Sobald das der Fall ist, beginnt der Mount.

Der **Mount-Pfad** ist plattform-konventionell und stabil: auf macOS unter `/Volumes/`, auf Linux unter `/mnt/`, auf Windows als Laufwerksbuchstabe. Der User kann den Pfad in den Settings überschreiben. Nach erfolgreichem Mount erscheint die strukturierte Verzeichnishierarchie des Brains im Filesystem, und beliebige Programme — der Client selbst, ein LLM-Agent, ein Texteditor — können auf die Inhalte mit normalen Filesystem-Operationen zugreifen.

Der **Unmount** muss sauber erfolgen: alle laufenden Schreibvorgänge sind abzuwarten, alle offenen Datei- und Datenbank-Handles zu schließen, der Verschlüsselungs-Layer abzubauen, und alle in den Speicher gehaltenen Schlüssel zu wischen. Diese Sauberkeit ist wichtig sowohl beim aktiven Unmount durch den User als auch beim physischen Entfernen der SSD.

Wird die SSD entfernt, während noch geschrieben wird, kann der Client einen sauberen Unmount nicht garantieren. In dem Fall ist der Datenintegrität-Schutz Aufgabe der untergelagerten Mechanismen (atomare Datenbankoperationen, atomare Git-Commits), und der Client meldet die unsaubere Trennung in einer Form, die den User auf einen möglichen Integritätsschaden hinweist und beim nächsten Mount eine Prüfung anbietet.

Die **Latenz vom Anschließen bis zur Verfügbarkeit** ist im Normalfall (silent unlock auf einem authorized Host) gering — wenige Sekunden. Diese Latenz ist eine relevante UX-Größe: ein Brain, der zwanzig Sekunden bis zur Verfügbarkeit braucht, fühlt sich kaputt an. Daher ist eine messbare Obergrenze festgelegt.

---

## Zugehörige Holdouts

H01, H02, H03, H04, H05, H06

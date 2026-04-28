# S07 — Tray UI and Status Communication

**Status:** approved · **Constraints:** C-15  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Brain-Client läuft im Hintergrund und ist über ein **Tray-Icon** in der Menüleiste / Taskleiste / Indicator-Area des Hosts präsent. Dieses Icon ist die primäre Schnittstelle, über die der User den Brain im Alltag bedient — das Settings-Fenster ist die zweite, nur für seltene Konfigurationen.

### Status-Kommunikation

Das Tray-Icon kommuniziert vier voneinander klar unterscheidbare Zustände:

- **disconnected**: Es ist gerade kein Brain aktiv. Visuell zurückhaltend (grau, statisch).
- **mounted-idle**: Ein Brain ist gemountet, und es laufen aktuell keine schreibenden Operationen. Diese Zustand bedeutet: das Brain ist **safe to remove** — die SSD kann gefahrlos abgezogen werden, ohne Datenverlust zu riskieren. Visuell ruhig und positiv (grün, statisch).
- **mounted-busy**: Ein Brain ist gemountet, und mindestens eine Active Operation läuft (Sync, Embedding-Berechnung, Datenbank-Schreibvorgang, Git-Commit). Diese Zustand bedeutet: **do not remove** — Abziehen riskiert Datenverlust oder Inkonsistenz. Visuell auffällig und in Bewegung (gelb, animiert).
- **error**: Ein Fehlerzustand, der die normale Brain-Nutzung blockiert. Visuell alarmierend (rot, statisch).

Der Tooltip jedes Zustands ist explizit menschenlesbar, sodass der User auf Hover sofort versteht, was der Zustand bedeutet — nicht nur visuelle Codes ("grün") sondern auch verbale Klarheit ("Brain ready – safe to remove").

### Active Operation Tracking

Damit der idle/busy-Übergang verlässlich ist, führt der Client intern Buch über alle laufenden Operationen, die das Brain berühren. Sobald die letzte Operation abgeschlossen ist, beginnt eine kurze Stabilisierungs-Phase (zwei Sekunden), in der nichts mehr passieren darf, bevor der Zustand auf idle wechselt. Diese Phase verhindert, dass der Status zwischen idle und busy oszilliert, wenn Operationen in schneller Folge starten.

### Tray-Menü

Klick auf das Icon öffnet ein kompaktes Menü mit:

- Status-Zeile (zeigt den aktuellen Tooltip)
- Schnellzugriff zum bevorzugten LLM-Client mit Brain als Working Directory ("Open in Claude Code")
- Brain-Viewer öffnen, wenn aktiviert
- Sync-Aktion mit Untermenü pro Quelle
- Wiki-History öffnen
- Settings öffnen
- Update-Check, About, Quit

Die Reihenfolge folgt der Häufigkeit der Aktionen: was am öftesten gebraucht wird, ist oben.

### Eject-Aktion mit Pre-Check

Der User kann das Brain explizit aus dem Tray heraus auswerfen. Klick auf "Eject Brain" prüft den Status: ist der Brain idle, erfolgt der saubere Unmount sofort. Ist der Brain busy, erscheint ein Dialog, der dem User drei Optionen anbietet — warten und dann auswerfen, sofort auswerfen (mit Datenverlust-Warnung), oder die Aktion abbrechen.

### Forced-Eject und Recovery

Wird die Brain-SSD physisch im busy-Zustand entfernt — oder triggert der User explizit "Eject anyway" — markiert der Client diese unsaubere Trennung. Beim nächsten Mount erkennt der Client den Marker und bietet dem User einen Integritäts-Check an: Git-Repository-Konsistenz, Datenbank-Integrität, Stichprobenartiger Vergleich von Pages-Tabelle und Filesystem. Findet der Check Probleme, schlägt er konkrete Recovery-Aktionen vor (z. B. "Restore Wiki from last good Git commit").

### Settings-Fenster

Über Tray "Settings…" öffnet sich ein eigenes Fenster, das alle persistenten Konfigurationen sammelt: Master-Passwort ändern, Authorized Devices verwalten, Mount-Pfad, Update-Channel, MCP-Aktivierung pro Connector, Sync-Schedules, Logs öffnen, Visualization-Tier-Aktivierung, LLM-Provider-Routing. Änderungen wirken nach Bestätigung und überleben Client-Restarts.

---

## Zugehörige Holdouts

H42, H43, H44, H45, H46, H47, H48

# S05 — Brain Initialization and Onboarding

**Status:** approved · **Constraints:** C-01, C-05, C-11, C-12, C-15  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Wenn der Brain-Client zum ersten Mal startet — nach Installation auf einem Host, auf dem noch kein Brain registriert ist und keiner angeschlossen ist — soll er den User durch einen Onboarding-Workflow führen, der einen neuen Brain erzeugt oder einen existierenden öffnet. Dieser Onboarding-Workflow ist die einzige Stelle, an der der User mit der zugrundeliegenden Speicher-Mechanik (Disk vs. Folder, Format, Vault-Anlage) interagieren muss.

### Welcome-Schritt

Beim ersten Start ohne aktives oder registriertes Brain erscheint ein Welcome-Fenster mit zwei Optionen: **Create new Brain** oder **Open existing Brain**. Sobald der User mindestens einen Brain erfolgreich angelegt oder geöffnet hat, erscheint dieses Fenster nicht mehr automatisch — der Client läuft normal im Tray. Der User kann den Welcome-Flow später jederzeit wieder über Settings auslösen.

### Medium-Auswahl

Der User wählt zwischen einer externen Disk und einem lokalen Folder.

Bei der **Disk-Variante** zeigt der Client eine Liste aller erkannten Block-Devices. Per Default sind System-Disks (Boot-Volume und alle Partitionen, die das laufende OS hosten) **ausgeblendet** — der User soll seinen Brain nicht versehentlich auf seinem Boot-Volume anlegen. Eine Checkbox "Show system disks" macht sie sichtbar, falls jemand das wirklich will. Pro Disk werden Größe, aktuelles Filesystem und Volume-Label angezeigt, damit der User die richtige Disk identifizieren kann.

Bei der **Folder-Variante** öffnet sich ein Datei-Picker. Der gewählte Folder muss leer sein oder darf bereits einen kompatiblen Vault enthalten. Bei nicht-leerem Folder ohne Vault zeigt der Client eine Warnung mit der Möglichkeit, trotzdem fortzufahren.

### Formatieren der Disk

Wird eine Disk gewählt, formatiert der Client sie als exFAT mit Volume-Label `BRAIN`. Der Format-Schritt ist explizit: der User sieht eine Bestätigungsanzeige mit Disk-Name und Größe und der klaren Warnung, dass alle Daten auf der Disk gelöscht werden. Erkennt der Client auf der gewählten Disk bereits einen kompatiblen Brain-Vault, schlägt er statt Format das Öffnen vor.

### Master-Passwort und Vault-Setup

Der User gibt ein Master-Passwort ein (mit Stärke-Indikator und Bestätigungseingabe), und der Client initialisiert daraus den Vault. Die Schlüsselableitung kann je nach Hardware sichtbar dauern — der User sieht einen Progress-Indikator und nicht nur "Loading…".

### Template-Population

Sobald der Vault initialisiert ist, befüllt der Client ihn mit dem Default-Inhalt: Verzeichnis-Struktur (Raw, Wiki, DB, Models, Cache, Logs), die kanonischen Konventions-Dateien (`AGENTS.md` und `CLAUDE.md`), die initiale `.mcp.json` mit Default-Connector-Platzhaltern, ein leeres Git-Repository im Wiki-Bereich, ein leeres Datenbank-Schema. Anschließend wird das Embedding-Modell heruntergeladen — das ist im Zweifelsfall der zeitintensivste Schritt (Modellgröße im niedrigen Gigabyte-Bereich), und der User sieht währenddessen einen Progress-Indikator.

### Connector-Quick-Setup (optional)

Nach Template-Population zeigt der Client eine Liste der Default-MCP-Connectors mit Toggle-Schaltern. Der User kann einen oder mehrere aktivieren; pro aktiviertem Connector wird der OAuth- oder Token-Flow gestartet. Der Schritt ist überspringbar — der User kann später in den Settings nachholen.

### Completion

Eine Summary-Ansicht zeigt: Pfad zum Brain, aktivierte Connectors, und Quick-Action-Buttons (Brain im Editor öffnen, ersten Sync auslösen, Brain-Viewer öffnen, weitere Settings konfigurieren). Klick auf eine Quick-Action führt direkt zur Aktion und schließt das Welcome-Fenster.

### Idempotenz

Wird der Initialisierungs-Flow auf einem bereits vollständig initialisierten Brain ausgeführt (etwa weil einzelne Files manuell gelöscht wurden), modifiziert er existierende Inhalte nicht. Fehlende Standard-Dateien werden ergänzt, der Rest bleibt unangetastet. Der User kann sich auf diese Eigenschaft verlassen, wenn er einzelne Default-Dateien repariert haben will.

### Open existing Brain

Der zweite Welcome-Pfad öffnet einen existierenden Vault — entweder eine Disk mit `BRAIN`-Label oder einen Folder mit Vault-Marker. Der User wird nach dem Master-Passwort gefragt, und nach erfolgreicher Authentifizierung wird der Host als Authorized Device hinzugefügt (gemäß S02), das Brain wird gemountet, und alle vorhandenen Inhalte werden sofort verfügbar.

---

## Zugehörige Holdouts

H27, H28, H29, H30, H31, H32, H33, H34, H35, H36

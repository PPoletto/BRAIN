# S06 — MCP Integration and LLM-Client Registration

**Status:** approved · **Constraints:** C-08, C-10  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Brain ist nicht selbst ein LLM-Frontend. Sein Zweck ist, **die Inhalte und Operationen** anzubieten, die ein LLM-Frontend des Users nutzt, um über den Brain Fragen zu beantworten oder Operationen auszuführen. Diese Bereitstellung erfolgt über MCP — die etablierte Schnittstelle zwischen LLM-Clients und Tools.

### Brain-MCP-Server

Solange ein Brain gemountet ist, läuft ein lokaler MCP-Server, der Brain-spezifische Tools anbietet: Volltext- und Vektor-Suche im Wiki, Abruf einzelner Pages, Abruf des Kontexts (Page plus 1-Hop-Nachbarn), strukturierte Abfragen über Connector-Daten (Mail-Threads, Confluence-Pages, etc.). Der Server ist über zwei Transporte erreichbar: stdio für direkte Subprocess-Integrationen (Claude Code, Codex), und HTTP an einem stabilen, lokalen Port mit Bearer-Authentifizierung für netzwerkbasierte Clients (ChatGPT Desktop, Open WebUI). Beim Unmount stoppt der Server und gibt den Port frei.

Der Server existiert immer, wenn das Brain gemountet ist — er ist nicht optional. Damit der User ihn nutzt, muss er aber in den jeweiligen LLM-Clients registriert sein.

### Auto-Registrierung in lokalen Clients

Bei aktivem Mount registriert der Client den Brain-MCP-Server und die User-konfigurierten externen MCP-Server (aus `00_meta/.mcp.json`) automatisch in den User-Konfigurationen der unterstützten Clients. Konkret: Claude Code, Codex und Continue.dev sehen die Brain-Tools nach dem Anschließen ohne weitere Aktion des Users. Bei Unmount werden die Einträge wieder entfernt — andere, vom User für andere Zwecke konfigurierte MCPs bleiben unangetastet.

### Halb-Registrierung in zusätzlichen Clients

ChatGPT Desktop hat seit Ende 2025 MCP-Support; analog werden Konfigurations-Einträge in seine User-Config geschrieben. Open WebUI hingegen ist serverbasiert und hat keine lokale Client-Config-Datei, in die geschrieben werden könnte. Für Open WebUI bietet der Client deshalb in den Settings eine Konfigurations-Anleitung mit Endpoint-URL, Bearer-Token und Copy-Button — der User trägt das einmal in der Open-WebUI-Admin-Oberfläche ein.

### Sync-Trigger

Der User kann manuell einen Sync für eine bestimmte Quelle (Outlook-Mails, Confluence-Pages, etc.) oder für alle aktivierten Quellen auslösen. Der Sync-Vorgang läuft als Subprozess eines headless LLM-Aufrufs, der die Ingest-Workflows aus der Konventions-Datei abarbeitet. Status und Resultat werden dem User per Tray-Notification gemeldet.

Optional kann der User für jede Quelle einen **periodischen Sync** konfigurieren (alle 5 Minuten, jede Stunde, täglich, etc.). Diese Cron-ähnlichen Jobs werden über den OS-Scheduler ausgeführt, sind aber an den Mount-Zustand gekoppelt: ist das Brain nicht gemountet, läuft der Job nicht.

### Routing-Policy für interne LLM-Calls

Manche Brain-Operationen (Page-Summary, Lint-Korrektur, Synthese-Vorschläge) erfordern selbst einen LLM-Call, ohne dass der User direkt fragt. Für diese Operationen kann der User in den Settings einen Default-Provider wählen: lokales Modell (Ollama), Anthropic Claude API, OpenAI API. Pro Pfad-Klasse im Brain (z. B. `01_raw/email/personal/`) kann das Routing auf "lokal only" überschrieben werden, sodass besonders sensible Inhalte den Host nicht verlassen.

---

## Zugehörige Holdouts

H37, H38, H39, H40, H41

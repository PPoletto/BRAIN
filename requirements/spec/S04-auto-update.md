# S04 — Auto-Update via Release Repository

**Status:** approved · **Constraints:** C-13, C-14  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Brain-Client aktualisiert sich selbst, ohne dass der User dafür eine Webseite besuchen oder ein Paket-Repository konfigurieren muss. Der Mechanismus muss zwei Eigenschaften zugleich erfüllen: **Integrität** (was installiert wird, ist tatsächlich vom Anbieter und unmanipuliert) und **Kontrolle** (der User entscheidet, wann ein Update wirklich angewendet wird).

Die **Update-Quelle** ist ein öffentlich zugängliches Release-Repository. Pro Plattform und Architektur wird ein Bundle bereitgestellt, plus eine Signaturdatei. Das Repository ist über die Software-Zentralisierungs-Konvention der Plattform veröffentlicht (auf GitHub: Releases-API).

Der Client **prüft auf Updates** zu drei Zeitpunkten: beim Start, in regelmäßigen Abständen während der Laufzeit (mehrere Stunden), und auf manuellen Trigger durch den User über das Tray-Menü. Bei fehlender Internetverbindung schweigt die Prüfung — keine Fehler-Notifications, keine Tray-Aufmerksamkeit. Der User soll von Update-Checks nichts mitbekommen, solange sie nichts Relevantes finden.

Findet die Prüfung eine neuere Version, lädt der Client das passende Bundle plus die zugehörige Signaturdatei herunter. **Verifikation** ist Pflicht und obligatorisch: das Bundle wird gegen einen im Build eingebackenen Public-Key der Anbieter-Signatur geprüft. Schlägt diese Verifikation fehl — egal warum: manipuliertes Bundle, falsche Signatur, fremder Public-Key — wird das Update verworfen, der Vorfall ins Log geschrieben, und der User per Tray erhält eine entsprechende Statusmeldung. Die alte Version läuft unverändert weiter.

Verifiziert sich das Bundle erfolgreich, **fragt der Client den User**, bevor er installiert. Optionen: jetzt installieren, später installieren, diese Version überspringen. "Jetzt" beendet den Client, installiert das Update, startet neu. "Später" verschiebt die Anfrage auf den nächsten Client-Start. "Überspringen" merkt sich, dass diese Version nicht erneut angeboten werden soll; künftige neuere Versionen werden aber wieder angeboten.

**Updates dürfen den Brain-Vault nicht berühren.** Die Brain-Daten — Wiki, DB, Embedding-Modell, Connector-State, Master-Key-Material im OS-Keychain — sind vollkommen unabhängig von Client-Updates. Ein Update darf weder Vault-Daten verändern noch Authorisierungs-Status verlieren. Nach dem Update mountet der Brain auf einem authorized Host weiterhin per silent unlock, und alle Pages sind unverändert.

Schlägt die Installation eines Updates **fehl** (Permission-Probleme, fehlerhaftes Paket, unterbrochene Schreiboperation), bleibt die alte Client-Version installiert und funktionsfähig. Der Fehler wird im Log dokumentiert; der User wird informiert. Es gibt kein Stranded-State: entweder das Update läuft durch und ist aktiv, oder die alte Version läuft weiter. Halbe Updates sind nicht zulässig.

Optional kann der User in den Settings einen **Channel** wählen: stabil (Default; nur reguläre Releases) oder beta (auch Pre-Releases, die markant gekennzeichnet sind). Der Channel-Wechsel ist eine bewusste User-Aktion.

---

## Zugehörige Holdouts

H20, H21, H22, H23, H24, H25, H26

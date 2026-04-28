# S02 — Encryption and Multi-Device Authorization

**Status:** approved · **Constraints:** C-02, C-03, C-04  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Brain-Vault ist verschlüsselt at rest. Der zentrale Geheimnis-Träger ist das **Master-Passwort**, das der User wählt und nur er kennt. Aus diesem Passwort wird mittels einer rechenintensiven Schlüsselableitung der **Master-Key** abgeleitet, der die eigentliche Verschlüsselung der Vault-Inhalte vornimmt. Das Master-Passwort selbst wird zu keinem Zeitpunkt persistiert — weder auf der SSD, noch im OS-Keychain, noch in Logs.

Damit der User nicht bei jedem Anschließen der SSD das Master-Passwort eingeben muss, gibt es das Konzept der **Authorized Devices**. Beim ersten Mount auf einem neuen Host fragt der Client einmalig nach dem Master-Passwort, leitet daraus den Master-Key ab, generiert daraus einen **gerätegebundenen Wrapping-Key**, und legt diesen im OS-Keychain des Hosts ab. Im Vault wird ein zusätzlicher Eintrag in der Liste der Authorized Devices gespeichert, der den Master-Key in der mit dem Wrapping-Key eingehüllten Form enthält. Bei zukünftigen Mounts auf demselben Host wird der Wrapping-Key aus dem OS-Keychain gelesen, der eingehüllte Master-Key wird damit entschlüsselt, und der Vault wird gemountet — ohne dass der User etwas tut.

Auf macOS-Geräten mit Touch-ID oder vergleichbarer biometrischer Autorisierung kann der User optional einstellen, dass der Wrapping-Key nur nach erfolgreicher biometrischer Bestätigung freigegeben wird. Damit lässt sich silent unlock mit einem leichten Sicherheits-Schritt kombinieren.

Der User soll seine Authorized Devices **verwalten** können: eine Liste aller Geräte sehen, mit Geräte-Name, Datum der Autorisierung, und Datum des letzten erfolgreichen Mounts. Jedes Gerät kann revoked werden — der entsprechende Eintrag im Vault wird gelöscht, sodass dieser Host beim nächsten Mount-Versuch erneut zur Master-Passwort-Eingabe gezwungen ist.

Das **Master-Passwort selbst** soll wechselbar sein, ohne dass alle Authorized Devices ihre silent-unlock-Fähigkeit verlieren. Beim Wechsel wird das aktuelle Passwort verifiziert, der Master-Key bleibt derselbe, aber alle eingehüllten Master-Key-Einträge im Vault werden mit der neuen Ableitung neu erstellt.

Die Sicherheits-Invariante: **das Master-Passwort verlässt niemals den Host und niemals den RAM des Client-Prozesses**, in dem die Schlüsselableitung läuft. Statische Analyse des Vaults darf das Master-Passwort nicht enthalten; ein Memory-Dump nach erfolgreicher Ableitung darf das Master-Passwort nicht mehr zeigen.

Die Verschlüsselung folgt einem **publik dokumentierten Format**, sodass im Notfall ein etablierter Drittanbieter-Client (siehe C-02) den Vault mit dem Master-Passwort öffnen kann. Damit ist der User nicht vom Brain-Client als einziger Software abhängig.

---

## Zugehörige Holdouts

H07, H08, H09, H10, H11, H12, H13

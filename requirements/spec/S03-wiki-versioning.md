# S03 — Wiki Versioning and Recovery

**Status:** approved · **Constraints:** C-05, C-06  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Der Wiki-Bereich des Brains ist versionssicher. Jede Mutation an Wiki-Pages — Anlegen, Ändern, Löschen — ist nachvollziehbar, und jede vergangene Version ist wiederherstellbar. Das Versionssystem ist Git, weil Git der Standard für textuelle Versionierung ist und außerdem out-of-the-box Robustheit, Diff-Anzeige und Konflikt-Auflösung mitbringt.

Wenn ein Brain initialisiert wird, wird der Wiki-Bereich als Git-Repository aufgesetzt. Konfiguration ist plattform-tolerant: Case-Insensitivität ist aktiviert (damit eine Page auf macOS und Windows gleich behandelt wird), File-Modus ist deaktiviert (damit unterschiedliche Default-Berechtigungen kein versionsrelevantes Diff erzeugen).

Die Versionierung läuft **automatisch im Hintergrund**, nicht durch explizites Commit-Auslösen vom User oder vom LLM-Agenten. Ein Auto-Commit wird nach einer Idle-Phase erstellt: sobald für eine kurze Zeitspanne keine weiteren Wiki-Mutationen stattfinden, fasst der Client alle aufgelaufenen Änderungen zu einem einzigen Commit zusammen. Die Commit-Nachricht ist maschinell aussagekräftig: sie nennt die Anzahl der Änderungen, die wichtigsten betroffenen Pfade, und identifiziert den Auslöser (Sync-Job, Manueller Edit, Ingest, etc.) wo möglich.

Vor jedem Auto-Commit läuft ein **Lint-Schritt**: er prüft, dass alle Wiki-Pages gültiges YAML-Frontmatter haben, dass alle Wiki-Links auf existente Page-IDs verweisen (oder als bewusst gebrochen markiert sind), und dass keine Page-IDs mehrfach vergeben wurden. Findet der Lint einen harten Fehler, wird der Commit nicht erstellt, und der User wird per Tray-Notification informiert.

Der User kann die **Versionshistorie einsehen** über eine Wiki-History-Ansicht im Tray-Menü. Die Ansicht zeigt Commits chronologisch, mit Diff-Anzeige pro Commit. Vergangene Versionen einzelner Pages oder ganzer Verzeichnisse können restored werden — das Restore selbst wird als neuer Commit (Typ "revert") aufgenommen, sodass die Historie vollständig bleibt.

Ein **Hard-Reset auf einen früheren Commit** ist möglich, aber durch eine zweistufige Bestätigung geschützt, die explizit anzeigt, wie viele Commits zurückgespult und wie viele Files dabei verändert werden. Auch ein Reset wird als Commit (Typ "reset") aufgenommen — die alte Historie bleibt erhalten und kann zurückgeholt werden, falls der Reset selbst ein Versehen war.

**Nicht alles im Brain-Vault ist versioniert.** Nur die menschlich kuratierten oder synthetisierten Inhalte (Wiki-Pages, Konventions-Dateien) gehören in die Versionshistorie. Raw-Quellmaterial (Mail-Dumps, PDF-Importe, Audio-Transkripte), Datenbanken (mit ihren WAL-Dateien), Caches und Logs sind explizit ausgeschlossen — sie würden die Repository-Größe sprengen, ändern sich permanent, und sind im Recovery-Fall ohnehin aus den Originalquellen wiederbeschaffbar.

---

## Zugehörige Holdouts

H14, H15, H16, H17, H18, H19

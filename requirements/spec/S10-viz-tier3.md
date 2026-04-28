# S10 — Visualization Tier 3: Graph View

**Status:** approved · **Constraints:** C-05, C-07  
**Letzte Aktualisierung:** 2026-04-28

---

## Beschreibung

Tier 3 erweitert den Viewer um eine **graphische Darstellung des Wiki-Netzes**, in der jede Page ein Knoten und jeder Wiki-Link eine Kante ist. Der Wert dieser Darstellung liegt im Überblick: bei einem hinreichend großen Wiki sieht der User Cluster, isolierte Inseln, zentrale Hubs, und unterversorgte Bereiche auf einen Blick — Information, die in einer linearen Tree-Sicht nicht sichtbar ist.

### Graph-Modus

Eine Toolbar-Aktion wechselt den Hauptbereich vom Reader auf den Graph. Die Knoten sind nach Page-Typ farblich differenziert (Entities, Concepts, Sources, Topics in unterschiedlichen Farben); die Kanten zeigen Wiki-Link-Beziehungen und können bei größeren Graphen leicht halbtransparent sein, um den Überblick zu bewahren.

Das Layout ist force-directed: Knoten ziehen sich gegenseitig an, wenn sie verlinkt sind, und stoßen sich ab, wenn sie nicht verlinkt sind. Das Ergebnis ist eine Anordnung, in der visuelle Nähe inhaltliche Nähe widerspiegelt — Cluster im Graph sind Cluster im Wissen.

Der User kann zoomen, panen, und einzelne Knoten manuell verschieben. Klick auf einen Knoten wechselt zurück in den Reader und öffnet die entsprechende Page.

### Filter

Ein Filter-Panel (oder eine Filter-Toolbar) bietet drei Filter-Dimensionen: Page-Typ als Multi-Select, Tag als Frontmatter-basierter Filter, und Datums-Range nach `updated`-Frontmatter. Aktivierte Filter blenden nicht-passende Knoten aus, was bei einem großen Brain die Sicht auf "alles, was zu NIS2 in den letzten 90 Tagen passiert ist" möglich macht.

### Sub-Graph um aktuelle Page

Aus dem Reader heraus gibt es eine Aktion "Show in Graph", die nicht den vollen Graph öffnet, sondern eine Sub-Sicht: die aktuelle Page als Zentrum, mit ihren direkten Nachbarn (1-Hop). Im Graph kann der User die Sub-Sicht auf 2-Hop erweitern, was bei mittlerem Wachstum oft die richtige Tiefe ist, um das thematische Umfeld einer Page zu sehen, ohne im Wald-und-Bäume-Problem zu landen.

### Performance-Anforderung

Der Graph soll bei einem Wiki im persönlichen Maßstab — bis in den Bereich von hunderten Pages — flüssig bleiben: das Layout konvergiert binnen Sekunden, Zoom und Pan ohne wahrnehmbares Ruckeln, Klicks reagieren ohne Verzögerung. Bei deutlich größeren Wikis ist eine Degradation akzeptabel, solange der User die Filter nutzen kann, um die Sicht zu reduzieren.

---

## Zugehörige Holdouts

H55, H56, H57

import { useEffect, useRef } from "react";
import cytoscape, { type ElementDefinition } from "cytoscape";
import fcose from "cytoscape-fcose";

cytoscape.use(fcose);

type Node = { id: string; type: string; title: string; tags: string[] };
type Edge = { source: string; target: string };

type Props = {
  nodes: Node[];
  edges: Edge[];
  onNodeClick?: (id: string) => void;
};

const TYPE_COLORS: Record<string, string> = {
  entity: "#60a5fa",
  concept: "#a78bfa",
  source: "#34d399",
  topic: "#f59e0b",
};

/// Layout choice is driven by *whether the graph has structure to show*,
/// not by node count:
///
///  - **0 edges**: every node is an island. fcose has nothing to push or
///    pull, so we use `grid` to lay them out predictably without
///    pretending there's connectivity. The empty-state hint above the
///    canvas tells the user why they don't see lines.
///  - **Any edges**: `fcose`'s organic force-directed solver. Even with
///    just a few edges, the layout pulls connected pairs together so the
///    user *sees* the connection — the previous "concentric for sparse
///    graphs" heuristic put related pages on opposite ends of a ring,
///    which read as "no connections at all".
///
/// Concentric used to be the default for ≤12 nodes; in practice that
/// produced exactly the "circles with no visible links" complaint from
/// users with small but link-rich vaults.
function pickLayoutName(edgeCount: number): "grid" | "fcose" {
  return edgeCount === 0 ? "grid" : "fcose";
}

export function GraphCanvas({ nodes, edges, onNodeClick }: Props) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!ref.current) return;
    const elements: ElementDefinition[] = [
      ...nodes.map((n) => ({
        data: { id: n.id, label: n.title, type: n.type },
      })),
      ...edges.map((e, idx) => ({
        data: {
          id: `e-${idx}-${e.source}-${e.target}`,
          source: e.source,
          target: e.target,
        },
      })),
    ];

    const cy = cytoscape({
      container: ref.current,
      elements,
      // Pinned the wheelSensitivity so trackpad scroll on macOS doesn't jump
      // by 50% per tick — Cytoscape's default is calibrated for mice.
      wheelSensitivity: 0.2,
      style: [
        {
          selector: "node",
          style: {
            label: "data(label)",
            "font-size": 11,
            color: "#e5e5e5",
            "text-valign": "bottom",
            "text-margin-y": 4,
            "text-outline-color": "#0a0a0a",
            "text-outline-width": 2,
            width: 30,
            height: 30,
            "background-color": (ele: cytoscape.NodeSingular) =>
              TYPE_COLORS[ele.data("type") as string] ?? "#9ca3af",
            "border-width": 1,
            "border-color": "#171717",
          },
        },
        {
          selector: "node:selected",
          style: {
            "border-color": "#10b981",
            "border-width": 3,
          },
        },
        {
          // Edge styling tuned for the dark canvas: the previous
          // `#3f3f46` on `#0a0a0a` was effectively invisible at default
          // zoom. `#71717a` reads as a clear connection without
          // overwhelming the nodes; haloing the arrowhead in the same
          // colour avoids the "white-tipped arrows" Cytoscape uses by
          // default.
          selector: "edge",
          style: {
            "line-color": "#71717a",
            "curve-style": "bezier",
            "target-arrow-color": "#71717a",
            "target-arrow-shape": "triangle",
            "arrow-scale": 1.0,
            width: 2,
            opacity: 0.85,
          },
        },
        {
          // When the user hovers / clicks a node, highlight its edges so
          // they jump out against the background herd — handy on dense
          // graphs.
          selector: "node:active, node:selected",
          style: {
            "z-index": 10,
          },
        },
      ],
      layout: layoutFor(edges.length),
    });

    cy.on("tap", "node", (event) => {
      const id = event.target.id();
      onNodeClick?.(id);
    });

    // Re-fit when the container resizes (e.g. when the user opens / closes
    // the sidebar, or the window is resized). Without this the graph stays
    // crammed in the original viewport size and the user sees clipped or
    // off-screen nodes.
    let raf = 0;
    const observer = new ResizeObserver(() => {
      // Coalesce bursts of resize events into a single re-layout.
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        cy.resize();
        cy.fit(undefined, 40);
      });
    });
    observer.observe(ref.current);

    return () => {
      observer.disconnect();
      cancelAnimationFrame(raf);
      cy.destroy();
    };
  }, [nodes, edges, onNodeClick]);

  return <div ref={ref} className="size-full bg-neutral-950" />;
}

function layoutFor(edgeCount: number): cytoscape.LayoutOptions {
  if (pickLayoutName(edgeCount) === "grid") {
    return {
      name: "grid",
      animate: false,
      fit: true,
      padding: 40,
      avoidOverlap: true,
      avoidOverlapPadding: 20,
    } as cytoscape.LayoutOptions;
  }
  // fcose for connected graphs. `idealEdgeLength` shrunk from the library
  // default (~120) so adjacent nodes cluster visibly; `nodeRepulsion`
  // bumped up so non-adjacent nodes still leave breathing room and the
  // edges don't overlap each other into a haystack.
  return {
    name: "fcose",
    animate: false,
    fit: true,
    padding: 50,
    nodeRepulsion: 12000,
    idealEdgeLength: 70,
    edgeElasticity: 0.6,
    gravity: 0.3,
    randomize: true,
    quality: "default",
    nodeSeparation: 80,
  } as unknown as cytoscape.LayoutOptions;
}

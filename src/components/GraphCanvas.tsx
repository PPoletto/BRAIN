import { useEffect, useMemo, useRef, useState } from "react";
import cytoscape, { type ElementDefinition } from "cytoscape";
import fcose from "cytoscape-fcose";
// `cytoscape-dagre` and `cytoscape-navigator` ship without their own
// type declarations — registering with `as unknown as` keeps the
// rest of the code in TypeScript without us having to write a full
// .d.ts shim. The runtime APIs are stable and well-documented.
import dagre from "cytoscape-dagre";
import navigator from "cytoscape-navigator";
import { commands } from "../lib/commands";

cytoscape.use(fcose);
cytoscape.use(dagre as unknown as cytoscape.Ext);
cytoscape.use(navigator as unknown as cytoscape.Ext);

export type GraphNode = {
  id: string;
  type: string;
  title: string;
  tags: string[];
};
export type GraphEdge = { source: string; target: string };
export type SavedPosition = { page_id: string; x: number; y: number };
export type Cluster = { id: string; size: number; nodeIds: string[] };

type Props = {
  nodes: GraphNode[];
  edges: GraphEdge[];
  onNodeClick?: (id: string) => void;
  /// `"force"` (default) uses fcose for connected graphs; falls back
  /// to a `preset` layout when `savedPositions` covers every node so
  /// the graph appears instantly with the user's hand-tuned layout.
  /// `"hierarchical"` switches to dagre for a top-down tree view.
  layoutMode?: "force" | "hierarchical";
  /// Persisted node coordinates from the DB. When the set covers
  /// every current node, the layout step is skipped entirely (preset)
  /// and the canvas appears in a single frame.
  savedPositions?: SavedPosition[] | null;
  /// Called after the layout settles with the final coordinates of
  /// every node, plus when the user drags a node and drops it.
  /// Tier3 persists the result via the `save_graph_positions` Tauri
  /// command so the next mount uses preset.
  onPositionsChange?: (positions: SavedPosition[]) => void;
  /// Connected-component summary emitted after each layout settle.
  /// The sidebar in Tier3 lists each cluster and lets the user click
  /// to focus it; clicking a cluster sets `focusedSubset` below.
  onClustersChange?: (clusters: Cluster[]) => void;
  /// When non-null, GraphCanvas runs `cy.fit()` on exactly these
  /// node ids so the user can isolate a cluster without leaving the
  /// graph view. Passing `null` returns to the full-graph fit.
  focusedSubset?: string[] | null;
};

const TYPE_COLORS: Record<string, string> = {
  entity: "#60a5fa",
  concept: "#a78bfa",
  source: "#34d399",
  topic: "#f59e0b",
};

// Threshold above which we drop fcose to "draft" quality. Default
// quality runs many more force-directed iterations and can pin the
// GPU on macOS WKWebView long enough to trigger the magenta backing-
// store fallback. Draft is visually almost identical for our cluster
// sizes and roughly half the iterations.
const FCOSE_DRAFT_THRESHOLD = 100;

// Debounce for the ResizeObserver. Window resizes can fire dozens of
// events per second on macOS during a drag — re-running cy.fit() each
// time is what most reliably trips the rendering pipeline. 200 ms
// feels instant to the user and lets the prior frame's Metal commit
// fully complete before we kick another resize.
const RESIZE_DEBOUNCE_MS = 200;

export function GraphCanvas({
  nodes,
  edges,
  onNodeClick,
  layoutMode = "force",
  savedPositions = null,
  onPositionsChange,
  onClustersChange,
  focusedSubset = null,
}: Props) {
  const ref = useRef<HTMLDivElement>(null);
  // Separate container for the cytoscape-navigator overlay (mini-map).
  // The navigator extension wants its own DOM node and renders an SVG
  // viewport rectangle inside it. Positioned absolute bottom-right
  // over the main graph canvas.
  const navRef = useRef<HTMLDivElement>(null);

  // Hold the live cy instance so effects that aren't the create-effect
  // (focus subset, cluster recomputation triggered by layout changes)
  // can act on the same Cytoscape graph without remounting it.
  const cyRef = useRef<cytoscape.Core | null>(null);

  // Latest callbacks captured in refs so the create-effect doesn't
  // need them in its dependency list — otherwise every parent
  // re-render would tear Cytoscape down and re-mount it.
  const onPositionsChangeRef = useRef(onPositionsChange);
  const onClustersChangeRef = useRef(onClustersChange);
  useEffect(() => {
    onPositionsChangeRef.current = onPositionsChange;
  }, [onPositionsChange]);
  useEffect(() => {
    onClustersChangeRef.current = onClustersChange;
  }, [onClustersChange]);

  // Render-key bump = full Cytoscape remount. Used by the auto-recover
  // path when we detect that the canvas didn't actually paint (all
  // node positions came back NaN/inf, classic symptom of the macOS
  // magenta backing-store fallback). Bumping the key tears the cy
  // instance down via the cleanup, then we recreate it from scratch.
  const [renderKey, setRenderKey] = useState(0);

  // Recovery is one-shot per graph payload — if a remount also fails
  // we don't want to loop forever. Reset whenever the input data
  // changes, so future graphs get a fresh chance to recover.
  const recoveryAttemptedRef = useRef(false);
  useEffect(() => {
    recoveryAttemptedRef.current = false;
  }, [nodes, edges]);

  // Hover-tooltip state. We store the hovered node id plus the cursor
  // position (in container-local coordinates) so the popover renders
  // next to the actual mouse, not the node center — Cytoscape's
  // `renderedPosition` is the node centroid, which often falls under
  // the cursor and would obscure what the user is pointing at.
  const [hovered, setHovered] = useState<{
    id: string;
    title: string;
    x: number;
    y: number;
  } | null>(null);

  // Lazy body cache. We fetch the first lines of each hovered page
  // exactly once and remember them — the typical "hover several
  // adjacent nodes to find the right one" pattern would otherwise
  // re-fire IPC for each pass over the same node. Two pieces:
  //   - `bodyCache` (state): drives the tooltip render; React notices
  //     when a fetch resolves and re-renders the popover with the
  //     excerpt filled in.
  //   - `fetchedRef` (ref): tracks which ids we've already started
  //     fetching, so the Cytoscape mouseover callback can decide
  //     "fetch or skip" without depending on the state value (which
  //     would force `bodyCache` into the effect's dep list and cause
  //     a remount on every successful fetch).
  const [bodyCache, setBodyCache] = useState<Record<string, string>>({});
  const fetchedRef = useRef<Set<string>>(new Set());

  // Backlink count is derivable from the edges already in the graph
  // payload — incoming edges to the node id. Memoise for O(1)
  // tooltip lookups instead of scanning `edges` on every hover.
  const backlinksByTarget = useMemo(() => {
    const map = new Map<string, number>();
    for (const e of edges) {
      map.set(e.target, (map.get(e.target) ?? 0) + 1);
    }
    return map;
  }, [edges]);

  // Index saved positions by id for O(1) attachment to elements.
  // When the saved set covers every visible node we can use the
  // `preset` layout (zero iterations, instant render) — otherwise
  // we fall back to fcose so newly added pages get arranged.
  const positionMap = useMemo(() => {
    const m = new Map<string, { x: number; y: number }>();
    if (savedPositions) {
      for (const p of savedPositions) m.set(p.page_id, { x: p.x, y: p.y });
    }
    return m;
  }, [savedPositions]);

  useEffect(() => {
    if (!ref.current) return;
    const elements: ElementDefinition[] = [
      ...nodes.map((n) => {
        const saved = positionMap.get(n.id);
        const def: ElementDefinition = {
          data: { id: n.id, label: n.title, type: n.type },
        };
        if (saved) def.position = { x: saved.x, y: saved.y };
        return def;
      }),
      ...edges.map((e, idx) => ({
        data: {
          id: `e-${idx}-${e.source}-${e.target}`,
          source: e.source,
          target: e.target,
        },
      })),
    ];

    // Layout decision tree:
    //   - hierarchical mode → dagre (top-down tree)
    //   - 0 edges → grid (no force/structure to leverage)
    //   - every node has a saved position → preset (instant)
    //   - else → fcose (force-directed)
    const everyNodeHasSavedPosition =
      nodes.length > 0 && nodes.every((n) => positionMap.has(n.id));
    const layoutChoice = pickLayout({
      mode: layoutMode,
      edgeCount: edges.length,
      nodeCount: nodes.length,
      hasFullPreset: everyNodeHasSavedPosition,
    });

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
            // Degree-based sizing: pages with more wiki-link
            // connections grow larger, giving the graph visual
            // hierarchy without any extra UI. Floor at 22 px so
            // isolated nodes stay clickable; cap at 56 px so a
            // hub-of-hubs doesn't dominate the canvas. Linear in
            // (in+out) degree, two pixels per connection — produces
            // the right perceptual spread on vaults of 50–500 nodes.
            width: (ele: cytoscape.NodeSingular) =>
              Math.min(22 + ele.degree(false) * 2, 56),
            height: (ele: cytoscape.NodeSingular) =>
              Math.min(22 + ele.degree(false) * 2, 56),
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
      layout: layoutChoice,
    });
    cyRef.current = cy;

    cy.on("tap", "node", (event) => {
      const id = event.target.id();
      onNodeClick?.(id);
    });

    // Hover-tooltip wiring. We rely on Cytoscape's renderedPosition for
    // tooltip placement so the popover stays anchored to the node even
    // if the user pans the graph slightly between mouseover and the
    // body fetch returning. The renderer attribute `originalEvent` is
    // a real DOM MouseEvent, so we offset by a few pixels from the
    // cursor itself to keep the node visible.
    cy.on("mouseover", "node", (event) => {
      const node = event.target;
      const id = node.id();
      const title = (node.data("label") as string) ?? id;
      const renderedPos = node.renderedPosition();
      // Offset right + down so the tooltip doesn't sit on top of the
      // node and trigger flicker as the cursor moves over its border.
      setHovered({ id, title, x: renderedPos.x + 16, y: renderedPos.y + 16 });
      // Lazy-fetch the body excerpt the first time we see this id.
      // Failures are silently swallowed — the title + backlink count
      // alone are still useful, and a transient IPC failure shouldn't
      // poison the hover experience.
      if (!fetchedRef.current.has(id)) {
        fetchedRef.current.add(id);
        commands
          .readPage(id)
          .then((page) => {
            const excerpt = excerptFromBody(page.body);
            if (excerpt) {
              setBodyCache((c) => ({ ...c, [id]: excerpt }));
            }
          })
          .catch(() => {
            // Allow a future hover to retry — the failure was probably
            // transient (file just rewritten, brief vault disconnect).
            fetchedRef.current.delete(id);
          });
      }
    });

    cy.on("mouseout", "node", () => {
      setHovered(null);
    });

    // Pink-screen auto-recover. After the layout finishes, sample the
    // node render positions: if every node has a non-finite x/y, the
    // canvas didn't actually paint — on macOS WKWebView this presents
    // as the whole graph area filled with magenta because Metal
    // returned a debug fill instead of valid pixels. The most reliable
    // recovery is a full Cytoscape remount, which we trigger by
    // bumping `renderKey` and letting the effect cleanup destroy the
    // current instance. Guarded by a one-shot ref so a graph that
    // stays broken after a remount doesn't loop.
    cy.on("layoutstop", () => {
      const ns = cy.nodes();
      if (ns.length === 0) return;

      if (!recoveryAttemptedRef.current) {
        const allInvalid = ns.toArray().every((n) => {
          const pos = n.renderedPosition();
          return !Number.isFinite(pos.x) || !Number.isFinite(pos.y);
        });
        if (allInvalid) {
          recoveryAttemptedRef.current = true;
          console.warn(
            "[GraphCanvas] all node positions invalid after layout; auto-recovering by remounting Cytoscape",
          );
          setRenderKey((k) => k + 1);
          return;
        }
      }

      // Sync the just-computed coordinates back to the parent so it
      // can persist them. We only emit when we actually ran a layout
      // pass (preset re-emits its inputs verbatim, which is harmless
      // but wasteful) — guarded below by the layoutChoice name.
      if (
        layoutChoice.name === "fcose" ||
        layoutChoice.name === "dagre"
      ) {
        const positions: SavedPosition[] = ns.toArray().map((n) => {
          const p = n.position();
          return { page_id: n.id(), x: p.x, y: p.y };
        });
        onPositionsChangeRef.current?.(positions);
      }

      // Connected-component summary for the cluster sidebar. Sorted
      // by size descending so the most relevant cluster shows up
      // first; the synthetic id stays stable across re-runs as long
      // as the cluster contents do.
      const components = cy.elements().components();
      const clusters: Cluster[] = components
        .map((comp, idx) => {
          const ids = comp
            .nodes()
            .toArray()
            .map((n) => n.id());
          return {
            id: `cluster-${idx}`,
            size: ids.length,
            nodeIds: ids,
          };
        })
        .filter((c) => c.size > 0)
        .sort((a, b) => b.size - a.size);
      onClustersChangeRef.current?.(clusters);
    });

    // Persist hand-tuning. When the user drops a node, save just its
    // new coordinates. The parent's onPositionsChange handler is
    // responsible for any debouncing / batching it wants — we send
    // one position per drop, which is what the user observably did.
    cy.on("dragfree", "node", (event) => {
      const node = event.target as cytoscape.NodeSingular;
      const p = node.position();
      onPositionsChangeRef.current?.([
        { page_id: node.id(), x: p.x, y: p.y },
      ]);
    });

    // Mini-map overlay. cytoscape-navigator wants its own DOM node
    // and renders an SVG thumbnail of the full graph plus a draggable
    // viewport rectangle. We keep `viewLiveFramerate: 0` so the
    // overlay redraws only when the viewport stops moving — animating
    // the rectangle at 60fps was previously a second source of GPU
    // pressure on the macOS WKWebView pipeline that triggered the
    // magenta backing-store fallback.
    let nav: { destroy: () => void } | null = null;
    if (navRef.current) {
      const cyWithNavigator = cy as cytoscape.Core & {
        navigator: (opts: Record<string, unknown>) => {
          destroy: () => void;
        };
      };
      nav = cyWithNavigator.navigator({
        container: navRef.current,
        viewLiveFramerate: 0,
        thumbnailEventFramerate: 30,
        thumbnailLiveFramerate: false,
        dblClickDelay: 200,
        removeCustomContainer: false,
        rerenderDelay: 100,
      });
    }

    // Re-fit when the container resizes. Bursts of resize events on
    // macOS during window-drag used to fire dozens of cy.fit() per
    // second, which is the most reliable way to trip the rendering
    // pipeline. Debouncing to 200 ms feels instant to the user and
    // lets the prior frame's Metal commit complete before we trigger
    // another. RAF was the previous coalesce; that was too tight.
    let debounce: ReturnType<typeof setTimeout> | null = null;
    const observer = new ResizeObserver(() => {
      if (debounce !== null) clearTimeout(debounce);
      debounce = setTimeout(() => {
        cy.resize();
        cy.fit(undefined, 40);
      }, RESIZE_DEBOUNCE_MS);
    });
    observer.observe(ref.current);

    return () => {
      observer.disconnect();
      if (debounce !== null) clearTimeout(debounce);
      nav?.destroy();
      cy.destroy();
      cyRef.current = null;
    };
  }, [nodes, edges, onNodeClick, renderKey, layoutMode, positionMap]);

  // Focus subset: when the parent (cluster sidebar click) hands us
  // a list of node ids, fit the viewport to just those nodes. Null
  // resets to full-graph fit. Uses the live cy ref so we don't
  // remount on every focus change.
  useEffect(() => {
    const cy = cyRef.current;
    if (!cy) return;
    if (focusedSubset && focusedSubset.length > 0) {
      const sel = focusedSubset
        .map((id) => `#${cssEscape(id)}`)
        .join(", ");
      const eles = cy.$(sel);
      if (eles.length > 0) cy.fit(eles, 40);
    } else {
      cy.fit(undefined, 40);
    }
  }, [focusedSubset]);

  return (
    <div className="relative size-full">
      <div ref={ref} className="size-full bg-neutral-950" />
      {/*
        Mini-map overlay. Fixed size, bottom-right, with a translucent
        background so it reads as a thumbnail without competing with
        the main canvas. cytoscape-navigator owns the inner content
        (its own SVG with the viewport rectangle).
      */}
      <div
        ref={navRef}
        className="pointer-events-auto absolute bottom-2 right-2 z-20 h-32 w-44 overflow-hidden rounded-md border border-neutral-700 bg-neutral-950/80 shadow-lg"
        aria-label="Graph mini-map"
      />
      {hovered && (
        <div
          // Pointer-events: none keeps the tooltip from intercepting
          // mouseover/mouseout events that would create a flicker
          // loop as the cursor crossed the popover.
          className="pointer-events-none absolute z-30 max-w-xs rounded-md border border-neutral-700 bg-neutral-900/95 p-2 text-xs shadow-xl"
          style={{ left: hovered.x, top: hovered.y }}
        >
          <div className="font-semibold text-neutral-100">
            {hovered.title}
          </div>
          <div className="mt-0.5 text-neutral-500">
            {backlinksByTarget.get(hovered.id) ?? 0} backlink
            {(backlinksByTarget.get(hovered.id) ?? 0) === 1 ? "" : "s"}
          </div>
          {bodyCache[hovered.id] && (
            <div className="mt-1 line-clamp-2 text-neutral-300">
              {bodyCache[hovered.id]}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/// Pulls a plain-text excerpt suitable for a hover tooltip out of a
/// page body — the first ~140 characters of the first non-blank,
/// non-frontmatter, non-heading line. Returns an empty string if the
/// body has no usable prose, in which case the tooltip just omits the
/// excerpt section. Markdown-aware enough to skip wiki-link braces and
/// fenced code blocks; not a full markdown renderer because the
/// resulting text gets clamped to two lines anyway.
function excerptFromBody(body: string): string {
  const max = 140;
  let inFence = false;
  for (const rawLine of body.split("\n")) {
    const line = rawLine.trim();
    if (line.startsWith("```")) {
      inFence = !inFence;
      continue;
    }
    if (inFence) continue;
    if (line.length === 0) continue;
    if (line.startsWith("#")) continue;
    if (line.startsWith("---")) continue;
    // Strip wiki-link brackets so they don't dominate the snippet:
    // `[[entities/foo|Foo]]` → `Foo`, `[[entities/foo]]` → `entities/foo`.
    const cleaned = line
      .replace(/\[\[([^\]|]+?)\|([^\]]+)\]\]/g, "$2")
      .replace(/\[\[([^\]]+)\]\]/g, "$1");
    return cleaned.length > max ? `${cleaned.slice(0, max - 1)}…` : cleaned;
  }
  return "";
}

/// Layout dispatcher. Replaces the old `layoutFor` so the create-effect
/// can keep its decision compact:
///
///  - `hierarchical` → dagre, top-to-bottom rank flow
///  - 0 edges → grid (no structure to leverage)
///  - all nodes have a saved position → preset (zero iterations,
///    instant render with the user's last-tuned coordinates)
///  - else → fcose, with quality auto-degrading past 100 nodes to
///    keep the macOS GPU pipeline out of the magenta-backing-store
///    danger zone (see `FCOSE_DRAFT_THRESHOLD`)
function pickLayout(args: {
  mode: "force" | "hierarchical";
  edgeCount: number;
  nodeCount: number;
  hasFullPreset: boolean;
}): cytoscape.LayoutOptions {
  if (args.mode === "hierarchical") {
    return {
      name: "dagre",
      rankDir: "TB",
      nodeSep: 50,
      rankSep: 70,
      animate: false,
      fit: true,
      padding: 40,
    } as unknown as cytoscape.LayoutOptions;
  }
  if (args.edgeCount === 0) {
    return {
      name: "grid",
      animate: false,
      fit: true,
      padding: 40,
      avoidOverlap: true,
      avoidOverlapPadding: 20,
    } as cytoscape.LayoutOptions;
  }
  if (args.hasFullPreset) {
    return {
      name: "preset",
      animate: false,
      fit: true,
      padding: 40,
    } as cytoscape.LayoutOptions;
  }
  // fcose default for connected graphs without saved coordinates.
  // `randomize: false` keeps the layout deterministic; `quality:
  // "draft"` past 100 nodes halves the iteration count so the macOS
  // GPU pipeline doesn't get pinned long enough to drop into Metal's
  // magenta-debug-fill backstop.
  return {
    name: "fcose",
    animate: false,
    fit: true,
    padding: 50,
    nodeRepulsion: 12000,
    idealEdgeLength: 70,
    edgeElasticity: 0.6,
    gravity: 0.3,
    randomize: false,
    quality:
      args.nodeCount > FCOSE_DRAFT_THRESHOLD ? "draft" : "default",
    nodeSeparation: 80,
  } as unknown as cytoscape.LayoutOptions;
}

/// Cytoscape ids in our wiki contain `/` and `.` characters
/// (e.g. `entities/dan-shapiro`), which break the CSS selectors we
/// need for `cy.$(...)`. Use the standard browser CSS escape so the
/// selector unambiguously refers to a single node id.
function cssEscape(id: string): string {
  if (
    typeof window !== "undefined" &&
    typeof window.CSS !== "undefined" &&
    typeof window.CSS.escape === "function"
  ) {
    return window.CSS.escape(id);
  }
  // Fallback for older runtimes: backslash-escape every non-word
  // character. This is a strict superset of what CSS.escape would
  // do for our id alphabet, so it remains correct.
  return id.replace(/[^\w-]/g, (m) => `\\${m}`);
}

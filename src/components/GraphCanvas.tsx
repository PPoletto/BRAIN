import { useEffect, useMemo, useRef, useState } from "react";
import cytoscape, { type ElementDefinition } from "cytoscape";
import fcose from "cytoscape-fcose";
// `cytoscape-dagre` and `cytoscape-navigator` ship without their own
// type declarations — registering with `as unknown as` keeps the
// rest of the code in TypeScript without us having to write a full
// .d.ts shim. The runtime APIs are stable and well-documented.
import dagre from "cytoscape-dagre";
import navigator from "cytoscape-navigator";
// The navigator package ships its own stylesheet for the viewport
// rectangle and thumbnail canvas — without this import the mini-map
// container renders empty (no draggable rectangle, just a static
// background).
import "cytoscape-navigator/cytoscape.js-navigator.css";
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
  /// Controls the cytoscape-navigator overlay. Default `false` so
  /// users who don't need pan-by-thumbnail aren't shown a permanent
  /// thumbnail eating screen real-estate. Tier3 wires this to a
  /// toolbar toggle.
  showMinimap?: boolean;
  /// Bumped by the parent when the user explicitly asks for a fresh
  /// auto-layout (the "Re-layout" button). Without this signal the
  /// graph never re-runs fcose for the lifetime of a Tier3 mount —
  /// `savedPositions` changes (from drags or the post-fcose
  /// auto-save) deliberately do *not* cause a relayout, otherwise
  /// every save would reshuffle the graph the user just settled on.
  layoutResetSignal?: number;
};

const TYPE_COLORS: Record<string, string> = {
  entity: "#60a5fa",
  concept: "#a78bfa",
  source: "#34d399",
  topic: "#f59e0b",
};

// Debounce for the ResizeObserver. Window resizes can fire dozens of
// events per second on macOS during a drag — re-running cy.fit() each
// time is what most reliably trips the rendering pipeline. 200 ms
// feels instant to the user and lets the prior frame's Metal commit
// fully complete before we kick another resize.
const RESIZE_DEBOUNCE_MS = 200;

// DOM id for the mini-map container. cytoscape-navigator only accepts
// CSS-selector *strings* for its `container` option (HTMLElement is
// silently rejected and the plugin builds its own body-attached div
// with the package's default white styling). The container element in
// the JSX carries this id so the plugin's `document.getElementById`
// lookup actually returns our element.
const MINIMAP_CONTAINER_ID = "cy-navigator-container";

export function GraphCanvas({
  nodes,
  edges,
  onNodeClick,
  layoutMode = "force",
  savedPositions = null,
  onPositionsChange,
  onClustersChange,
  focusedSubset = null,
  showMinimap = false,
  layoutResetSignal = 0,
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

  // Mini-map navigator instance lives in its own ref because its
  // lifecycle is *deliberately decoupled* from the cy lifecycle.
  // Toggling the mini-map must not rebuild cy (rebuilding would
  // re-run a layout, which can rotate the graph) — so the navigator
  // is attached / detached in a separate effect that does not
  // touch cy. The main create-effect coordinates with this ref
  // when it does need to destroy cy: it detaches first, the new
  // cy gets a fresh navigator on demand.
  const navInstanceRef = useRef<{ destroy: () => void } | null>(null);
  // Track the latest `showMinimap` in a ref so the helpers below
  // (called from both the cy-lifecycle effect and the
  // showMinimap-effect) can read it without becoming a closure
  // dependency of either.
  const showMinimapRef = useRef(showMinimap);
  showMinimapRef.current = showMinimap;

  // Latest callbacks captured in refs so the create-effect doesn't
  // need them in its dependency list — otherwise every parent
  // re-render would tear Cytoscape down and re-mount it. Tier3's
  // `onNodeClick` is an inline arrow that receives a new identity
  // each render, so without ref'ing it every Tier3 state change
  // would force a full cy rebuild — the most subtle source of
  // "graph randomly relayouts on minimap toggle" before the fix.
  const onPositionsChangeRef = useRef(onPositionsChange);
  const onClustersChangeRef = useRef(onClustersChange);
  const onNodeClickRef = useRef(onNodeClick);
  useEffect(() => {
    onPositionsChangeRef.current = onPositionsChange;
  }, [onPositionsChange]);
  useEffect(() => {
    onClustersChangeRef.current = onClustersChange;
  }, [onClustersChange]);
  // Attach the cytoscape-navigator overlay to the current cy
  // instance, but only if (a) showMinimap is currently true,
  // (b) we don't already have an instance, (c) cy exists, and
  // (d) the container div is in the DOM. Idempotent — calling
  // it twice in a row only attaches once. Used by both the
  // create-effect (to get a navigator on a freshly-built cy) and
  // by the showMinimap effect (to react to toggle).
  const attachNavigatorIfWanted = () => {
    if (!showMinimapRef.current) return;
    if (navInstanceRef.current) return;
    if (!cyRef.current) return;
    if (typeof document === "undefined") return;
    const container = document.getElementById(MINIMAP_CONTAINER_ID);
    if (!container) return;
    const cyWithNavigator = cyRef.current as cytoscape.Core & {
      navigator: (opts: Record<string, unknown>) => {
        destroy: () => void;
      };
    };
    navInstanceRef.current = cyWithNavigator.navigator({
      container: `#${MINIMAP_CONTAINER_ID}`,
      viewLiveFramerate: 0,
      thumbnailEventFramerate: 30,
      thumbnailLiveFramerate: false,
      dblClickDelay: 200,
      // Must be false so the plugin's destroy() empties our
      // container's innerHTML instead of doing
      // `parentElement.removeChild(this.$panel)` — by the time
      // React commits the conditionally-rendered container
      // disappearing from the DOM, parentElement is already null
      // and the removeChild call would throw.
      removeCustomContainer: false,
      rerenderDelay: 100,
    });

    // Kick cy into producing its first render so the navigator's
    // thumbnail materialises immediately. Internally the plugin
    // registers a throttled `cy.onRender(handler)` in its setup
    // path (cytoscape-navigator.js:897) — that handler is the only
    // path that ever sets `src` on the `<img alt="Graph navigator">`
    // it injects into our container. If we just leave it there, cy
    // does not render again until the user pans or zooms, so the
    // `<img>` sits without a `src`. The browser then shows the
    // broken-image glyph plus the alt text in the corner of the
    // mini-map panel until the first interaction.
    //
    // `cy.resize()` ensures the renderer agrees on the canvas
    // dimensions (the mini-map container just appeared, so the
    // main viewport's bounding rect may have shifted), and
    // `cy.forceRender()` then triggers exactly one render tick
    // synchronously — that's all the plugin needs to populate the
    // thumbnail. rAF defers the call by one frame so the plugin's
    // own constructor has finished wiring up its listeners by the
    // time we emit.
    requestAnimationFrame(() => {
      // Guard against the case where the user toggled the mini-map
      // back off in the gap between this rAF being scheduled and
      // firing — detachNavigator() will have cleared the instance.
      if (!navInstanceRef.current || !cyRef.current) return;
      const cy = cyRef.current;
      cy.resize();
      (cy as cytoscape.Core & { forceRender: () => void }).forceRender();
    });
  };

  const detachNavigator = () => {
    if (!navInstanceRef.current) return;
    navInstanceRef.current.destroy();
    navInstanceRef.current = null;
  };

  useEffect(() => {
    onNodeClickRef.current = onNodeClick;
  }, [onNodeClick]);

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

  // Position map lives in a ref instead of a `useMemo(savedPositions)`
  // for a critical reason: we want the create-effect *not* to re-run
  // every time `savedPositions` changes, but we still want the
  // create-effect to read the *latest* positions when it does run for
  // some other reason (showMinimap toggle, layoutMode change, etc.).
  // useMemo + dep on savedPositions would force the effect to re-run
  // on every save, producing the visible "rotates on toggle" bug:
  // (a) the user toggles minimap mid-fcose, (b) React re-renders, the
  // closure captures positionMap from BEFORE the just-finished fcose's
  // setSavedPositions had time to commit, (c) effect re-runs, sees an
  // empty positionMap, runs fcose again with a fresh randomization.
  // A ref updated synchronously by both the initial prop hydration
  // AND the cy `layoutstop` callback closes that race: by the time
  // any effect reads `positionMapRef.current`, it always has the
  // latest coordinates regardless of where they came from.
  const positionMapRef = useRef<Map<string, { x: number; y: number }>>(
    new Map(),
  );

  // Initial hydration from the savedPositions prop. Runs exactly
  // once per `savedPositions` *first non-null* delivery and once per
  // `layoutResetSignal` bump. Crucially does NOT re-hydrate on
  // subsequent savedPositions changes (e.g. the post-fcose save) —
  // the cy layoutstop callback owns the ref from that point on and
  // a re-hydration would fight against it.
  const lastResetSignalRef = useRef<number | null>(null);
  const hydratedRef = useRef(false);
  if (
    savedPositions !== null &&
    (!hydratedRef.current ||
      lastResetSignalRef.current !== layoutResetSignal)
  ) {
    const fresh = new Map<string, { x: number; y: number }>();
    for (const p of savedPositions) fresh.set(p.page_id, { x: p.x, y: p.y });
    positionMapRef.current = fresh;
    hydratedRef.current = true;
    lastResetSignalRef.current = layoutResetSignal;
  }

  useEffect(() => {
    if (!ref.current) return;
    // Snapshot the position map *now*, at effect-run time. Any
    // updates from prior cy layouts (in this same component
    // mount) live in `positionMapRef.current` thanks to the
    // sync-write in the layoutstop handler below.
    const positionMap = positionMapRef.current;
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
    //   - every node has a saved position → preset (instant, no
    //     force iterations, layout exactly as the user left it)
    //   - some nodes have saved positions (e.g. a new wiki page
    //     appeared since the last save) → fcose with
    //     `randomize: false`, so saved nodes start at their
    //     persisted coordinates and only the new ones get
    //     force-placed.
    //   - no positions known → fcose with `randomize: true` so
    //     nodes don't all collapse onto (0,0) for a degenerate
    //     "comic row" layout (the v0.2.6 bug).
    const everyNodeHasSavedPosition =
      nodes.length > 0 && nodes.every((n) => positionMap.has(n.id));
    const someNodeHasSavedPosition = positionMap.size > 0;
    // For partial-seeds fcose runs we hand the saved positions to
    // the layout engine as `fixedNodeConstraint`. fcose will keep
    // these nodes EXACTLY where they are during the force pass —
    // only the new (unknown-position) nodes get force-placed.
    // Without this constraint, fcose's force iterations gradually
    // drift the known nodes too, producing the "everything rotated
    // slightly when I came back to the tab" feel even though
    // `randomize: false` told it to start from existing positions.
    // For the full-preset and no-seeds cases the array is harmless:
    // preset doesn't read fixedNodeConstraint, and a zero-seeds
    // fcose run with an empty array behaves as if it weren't set.
    const fixedConstraints: Array<{
      nodeId: string;
      position: { x: number; y: number };
    }> = [];
    for (const n of nodes) {
      const saved = positionMap.get(n.id);
      if (saved) {
        fixedConstraints.push({
          nodeId: n.id,
          position: { x: saved.x, y: saved.y },
        });
      }
    }
    const layoutChoice = pickLayout({
      mode: layoutMode,
      edgeCount: edges.length,
      nodeCount: nodes.length,
      hasFullPreset: everyNodeHasSavedPosition,
      hasPartialSeeds: someNodeHasSavedPosition,
      fixedConstraints,
    });

    const cy = cytoscape({
      container: ref.current,
      elements,
      // Disable Cytoscape's built-in wheel zoom — a single
      // `wheelSensitivity` value can't simultaneously feel right
      // on a discrete mouse wheel (deltaY ≈ 100 per click) and a
      // continuous trackpad scroll (deltaY ≈ 5 per event, dozens
      // of events per gesture). The custom wheel handler attached
      // below adapts the zoom step per event based on |deltaY|
      // (mouse vs trackpad) and respects ctrlKey wheel events
      // (browser-normalised pinch-to-zoom on macOS trackpads).
      userZoomingEnabled: false,
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
            // hierarchy without any extra UI. Floor at 14 px so
            // isolated nodes stay clickable; cap at 38 px so a
            // hub-of-hubs doesn't dominate the canvas. The static
            // values (used at zoom = 1) get scaled inversely with
            // zoom level by the `cy.on("zoom", …)` handler below —
            // zooming in shrinks nodes so the user sees more of
            // their neighbourhood at higher detail levels.
            width: (ele: cytoscape.NodeSingular) =>
              Math.min(14 + ele.degree(false) * 1.5, 38),
            height: (ele: cytoscape.NodeSingular) =>
              Math.min(14 + ele.degree(false) * 1.5, 38),
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
      onNodeClickRef.current?.(id);
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
      // can persist them — but **only** for the force-directed pass.
      // Persisting dagre's hierarchical positions and then toggling
      // back to force mode would let preset rebuild the graph with
      // the tree-shaped coordinates, which is not what the user
      // hand-tuned in force mode. Hierarchical is recomputed on each
      // open and intentionally non-persistent.
      if (layoutChoice.name === "fcose") {
        const positions: SavedPosition[] = ns.toArray().map((n) => {
          const p = n.position();
          return { page_id: n.id(), x: p.x, y: p.y };
        });
        // Update our local ref synchronously, BEFORE notifying the
        // parent. If the parent's setSavedPositions schedules a
        // re-render and a subsequent re-mount of this effect (e.g.
        // user toggles minimap before the next React commit), the
        // re-mount reads `positionMapRef.current` and finds these
        // fresh positions — no race window where it could fall
        // back to fcose with `randomize: true`.
        for (const p of positions) {
          positionMapRef.current.set(p.page_id, { x: p.x, y: p.y });
        }
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

    // Custom wheel-zoom handler. Replaces Cytoscape's built-in
    // wheel zoom (which we disabled with `userZoomingEnabled:
    // false` above) with one that adapts the per-event step to
    // the input device:
    //
    //   - Mouse wheel: |deltaY| ≥ 50 (one discrete click). One
    //     tick = ~15% zoom change, which feels like a meaningful
    //     step without overshooting.
    //   - Trackpad scroll: |deltaY| < 50, but events fire dozens
    //     of times per gesture. Each event = ~2% zoom; the
    //     gesture as a whole feels smooth and responsive.
    //   - Trackpad pinch (browsers report this as a wheel event
    //     with `ctrlKey: true` and pre-scaled deltaY): ~5% per
    //     event so the pinch feels natural without amplifying
    //     the browser's pre-scaling.
    //
    // Zoom is centred on the cursor — standard Map-/Graph-UI
    // expectation, much nicer than zooming around the canvas
    // centre when the user is hovering over a specific node.
    // AbortController owns the wheel-listener lifecycle: registering
    // and removing it via the same signal keeps the inline listener
    // closure tidy, and means TypeScript can infer WheelEvent from
    // the addEventListener overload without ESLint complaining about
    // an undefined `WheelEvent` global (DOM lib types ARE in scope
    // for tsc, but ESLint's no-undef rule doesn't read them).
    const container = ref.current;
    const wheelAbort = new AbortController();
    container.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        let factor: number;
        if (e.ctrlKey) {
          // Browser-normalised pinch on a trackpad. The browser's
          // already pre-scaled deltaY, so we use a small step.
          const PINCH_STEP = 1.05;
          factor = e.deltaY < 0 ? PINCH_STEP : 1 / PINCH_STEP;
        } else if (Math.abs(e.deltaY) >= 50) {
          // Discrete mouse-wheel click — ~15% per tick is a useful
          // step size that doesn't overshoot.
          const MOUSE_WHEEL_STEP = 1.15;
          factor = e.deltaY < 0 ? MOUSE_WHEEL_STEP : 1 / MOUSE_WHEEL_STEP;
        } else {
          // Continuous trackpad scroll — ~2% per event. Trackpads
          // fire dozens of events per gesture, so the gesture as a
          // whole still feels responsive.
          const TRACKPAD_STEP = 1.02;
          factor = e.deltaY < 0 ? TRACKPAD_STEP : 1 / TRACKPAD_STEP;
        }
        const rect = container.getBoundingClientRect();
        cy.zoom({
          level: cy.zoom() * factor,
          renderedPosition: {
            x: e.clientX - rect.left,
            y: e.clientY - rect.top,
          },
        });
      },
      // `passive: false` so preventDefault actually stops the page
      // from scrolling when the user wheels over the graph.
      { passive: false, signal: wheelAbort.signal },
    );

    // Persist hand-tuning. When the user drops a node, save just its
    // new coordinates — but only in force mode, so dragging a node
    // around in the hierarchical view doesn't pollute the persisted
    // force-mode layout. The parent's onPositionsChange handler is
    // responsible for any debouncing / batching it wants.
    cy.on("dragfree", "node", (event) => {
      if (layoutMode !== "force") return;
      const node = event.target as cytoscape.NodeSingular;
      const p = node.position();
      // Same sync-write to the ref as in layoutstop — if the user
      // drags then immediately toggles minimap, the remount reads
      // the dropped position from the ref, not a stale one.
      positionMapRef.current.set(node.id(), { x: p.x, y: p.y });
      onPositionsChangeRef.current?.([
        { page_id: node.id(), x: p.x, y: p.y },
      ]);
    });

    // Zoom-aware visual sizing — shrink nodes, labels, edges and
    // arrows when the user zooms in so the neighbourhood stays
    // readable (more nodes fit in the viewport, labels don't bloat
    // to dozens of pixels, edges stay as thin connecting strokes
    // instead of fat ribbons). Uses 1/√zoom so the scaling is
    // moderate: at 2× zoom everything is ~71 % of its base size,
    // at 4× ~50 %. Throttled to one rAF tick because cy fires zoom
    // events continuously during a wheel gesture.
    //
    // The 0.2.14 version of this handler only scaled width/height
    // on nodes. Pascal hit the symptom at high zoom on his vault
    // (~166 nodes / 966 edges): with `font-size: 11` and `width: 2`
    // in world coordinates, cytoscape rendered labels at ~55 px
    // and edges at ~10 px at the zoom level he was inspecting —
    // text dominated the canvas, edges looked like fat ribbons,
    // node circles were dwarfed. Mirroring the same 1/√zoom on
    // font-size, text-outline-width, edge width and arrow-scale
    // keeps the visual hierarchy consistent across the whole
    // zoom range.
    let zoomRafId = 0;
    const updateZoomScaledSizes = () => {
      const scale = 1 / Math.sqrt(cy.zoom());
      cy.nodes().style({
        width: (ele: cytoscape.NodeSingular) =>
          Math.min(14 + ele.degree(false) * 1.5, 38) * scale,
        height: (ele: cytoscape.NodeSingular) =>
          Math.min(14 + ele.degree(false) * 1.5, 38) * scale,
        "font-size": 11 * scale,
        // text-outline-width is the dark halo behind labels
        // (`text-outline-color: "#0a0a0a"` in the stylesheet
        // above) — without scaling it grows along with the
        // font and turns into a chunky black box around each
        // label at high zoom.
        "text-outline-width": 2 * scale,
      });
      cy.edges().style({
        width: 2 * scale,
        // arrow-scale is a multiplier on top of the edge width,
        // so it shrinks proportionally without separate tuning.
        "arrow-scale": 1.0 * scale,
      });
    };
    cy.on("zoom", () => {
      cancelAnimationFrame(zoomRafId);
      zoomRafId = requestAnimationFrame(updateZoomScaledSizes);
    });

    // Sync the mini-map navigator to the *current* showMinimap
    // state. Decoupled from the showMinimap prop so a toggle does
    // NOT rebuild cy (rebuilding would re-run a layout, which is
    // exactly the rotation the user kept reporting). The
    // attach/detach helpers and the dedicated showMinimap effect
    // below own the navigator lifecycle from this point on.
    attachNavigatorIfWanted();

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
      cancelAnimationFrame(zoomRafId);
      wheelAbort.abort();
      // Detach navigator BEFORE destroying cy — the plugin's
      // destroy() touches cy internals that disappear once
      // cy.destroy() has run.
      detachNavigator();
      cy.destroy();
      cyRef.current = null;
    };
    // Effect-dependencies — note carefully what's NOT here:
    //   - `positionMap` / `savedPositions` are intentionally read
    //     via `positionMapRef.current` so a save (after fcose or
    //     drag) doesn't trigger a remount. Re-layouts go through
    //     `layoutResetSignal`.
    //   - `onNodeClick`, `onPositionsChange`, `onClustersChange`
    //     go through their respective refs so a parent re-render
    //     (e.g. setShowMinimap → Tier3 re-renders → fresh callback
    //     identities) does not destroy and recreate cy.
    //   - `showMinimap` is owned by the navigator-effect below;
    //     toggling it must NOT rebuild cy (that re-runs a layout,
    //     which is the exact "rotation on toggle" bug).
  }, [nodes, edges, renderKey, layoutMode, layoutResetSignal]);

  // Mini-map navigator lifecycle — fully decoupled from cy. When
  // the user toggles the toolbar button, we attach (showMinimap →
  // true) or detach (showMinimap → false) the cytoscape-navigator
  // instance against the current cyRef. The cy graph itself is
  // never rebuilt here, so the layout the user is looking at
  // stays exactly where it is. Cleanup also detaches in case
  // both showMinimap toggles AND cy is rebuilt by the main effect
  // — the main effect's cleanup detaches first to avoid a stale
  // navigator pointing at a destroyed cy.
  useEffect(() => {
    if (showMinimap) {
      attachNavigatorIfWanted();
    } else {
      detachNavigator();
    }
    return () => {
      // No-op on plain showMinimap toggles (we already detached
      // above when going false), but if the GraphCanvas component
      // unmounts entirely this catches the leaked navigator.
      detachNavigator();
    };
  }, [showMinimap]);

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
        Mini-map overlay. The id matches the selector we pass to
        `cy.navigator({ container: '#cy-navigator-container' })`
        — without that, the plugin would silently ignore our
        element and float its own white 400×400 div over the
        whole window (see comment in the create-effect above).
        We style this container directly with Tailwind: small
        thumbnail in the bottom-LEFT of the GraphCanvas (away
        from the StatusBar's version label in the bottom-right),
        BRAIN-themed dark background, `relative` so the inner
        `.cytoscape-navigatorView` viewport rectangle and the
        `.cytoscape-navigatorOverlay` mouse-target overlay
        position correctly inside it.
      */}
      {showMinimap && (
        <div
          ref={navRef}
          id={MINIMAP_CONTAINER_ID}
          aria-label="Graph mini-map"
          // The arbitrary-variant `[&_img:not([src])]:opacity-0` hides
          // the plugin-injected `<img alt="Graph navigator">` for the
          // single rAF frame between navigator attach and our
          // `cy.forceRender()` populating its `src`. Without it the
          // browser briefly paints the broken-image glyph + alt text
          // in the corner of the mini-map.
          className="pointer-events-auto absolute bottom-2 left-2 z-20 h-24 w-32 overflow-hidden rounded-md border border-neutral-700 bg-neutral-950/85 shadow-lg [&_img:not([src])]:opacity-0"
        />
      )}
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
  hasPartialSeeds: boolean;
  fixedConstraints: Array<{
    nodeId: string;
    position: { x: number; y: number };
  }>;
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
  // fcose for connected graphs. Two flavours:
  //   - `randomize: false` when at least some nodes have saved
  //     positions: fcose uses those as starting points and only
  //     force-places the unknown ones, so adding a new wiki page
  //     doesn't reshuffle the entire layout. This is the fix for
  //     the "every reopen rotates" complaint when the graph grew
  //     between sessions.
  //   - `randomize: true` for the cold-start case (no saved
  //     positions at all). Without random seeding every node
  //     defaults to (0,0) and fcose can't break the symmetry,
  //     producing a degenerate row layout (the v0.2.6 bug).
  // `quality: "default"` runs the full iteration budget; with
  // position persistence in place fcose runs at most once per
  // vault before the cached preset takes over, so paying the
  // full GPU cost once is fine. The active pink-screen
  // mitigations (200 ms ResizeObserver debounce + auto-recover
  // remount on NaN positions) handle the GPU stress without
  // degrading layout quality.
  // Spread tuned for vaults of 50–500 nodes: `nodeRepulsion`,
  // `idealEdgeLength` and `nodeSeparation` are all bumped over
  // the library defaults so dense clusters get readable breathing
  // room. Pre-0.2.14 these were respectively 12000 / 70 / 80
  // which left hub-of-hubs nodes overlapping their neighbours;
  // 28000 / 120 / 140 pushes them apart enough that labels stop
  // colliding at default zoom.
  return {
    name: "fcose",
    animate: false,
    fit: true,
    padding: 50,
    nodeRepulsion: 28000,
    idealEdgeLength: 120,
    edgeElasticity: 0.6,
    gravity: 0.3,
    randomize: !args.hasPartialSeeds,
    quality: "default",
    nodeSeparation: 140,
    // Lock any node we already have a saved position for. fcose
    // will run forces against the *new* nodes only — existing
    // pages stay exactly where they were last time, no drift.
    // Empty array on cold start, full set when only one wiki page
    // appeared since the last layout.
    fixedNodeConstraint: args.fixedConstraints,
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

import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import {
  GraphCanvas,
  type Cluster,
  type SavedPosition,
} from "../../components/GraphCanvas";
import { Button } from "../../components/ui/Button";
import { EmptyState } from "../../components/ui/EmptyState";

type Graph = Awaited<ReturnType<typeof commands.getGraph>>;

const ALL_TYPES = ["entity", "concept", "source", "topic"];

// Drag-save debounce. Cytoscape fires `dragfree` per-node when the
// user releases the mouse; rapid back-to-back drags would otherwise
// kick off a SQLite write each. 400 ms feels instant to the user
// and coalesces a typical "tug a node, immediately tug another"
// session into one batch.
const POSITION_SAVE_DEBOUNCE_MS = 400;

export function Tier3() {
  const navigate = useNavigate();
  const [graph, setGraph] = useState<Graph | null>(null);
  const [types, setTypes] = useState<string[]>([]);
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState<string[]>([]);
  const [updatedAfter, setUpdatedAfter] = useState<string>("");
  const [error, setError] = useState<string | null>(null);

  // Layout toggle. Default is force-directed (fcose / preset); the
  // user can flip to a top-down hierarchical view via the toolbar.
  const [layoutMode, setLayoutMode] =
    useState<"force" | "hierarchical">("force");

  // Mini-map toggle. Off by default — the overlay was permanently
  // visible in v0.2.6 which crowded the bottom-right of the window
  // (where the version label lives) and added GPU cost even when
  // the user wasn't panning. Now opt-in via the toolbar; persists
  // for the session only.
  const [showMinimap, setShowMinimap] = useState(false);

  // Persistent layout. `null` = haven't loaded yet; `[]` = loaded,
  // empty (first time the user opens the graph). Both behave the
  // same way at render time — fcose runs and saves its output.
  const [savedPositions, setSavedPositions] =
    useState<SavedPosition[] | null>(null);

  const [clusters, setClusters] = useState<Cluster[]>([]);
  const [focusedCluster, setFocusedCluster] = useState<string | null>(null);

  // Pending position writes — coalesced into a single Tauri call by
  // the debouncer below. Stored by page_id so a node dragged twice
  // in quick succession only persists its last position.
  const pendingPositionsRef = useRef<Map<string, SavedPosition>>(new Map());
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Centralised fetch: invoked from both the filter-change effect
  // *and* from the auto-refresh hook below, so a wiki-changed event
  // re-pulls the graph with the user's current filter settings.
  const fetchGraph = useCallback(() => {
    setError(null);
    commands
      .getGraph({
        types: types.length ? types : undefined,
        tags: tags.length ? tags : undefined,
        updated_after: updatedAfter || null,
      })
      .then(setGraph)
      .catch((e: unknown) => setError(String(e)));
  }, [types, tags, updatedAfter]);

  useEffect(() => {
    fetchGraph();
  }, [fetchGraph]);

  // Live-update: redraw on auto-commit or disk reconnect.
  useDataRefresh(fetchGraph);

  // One-shot load of persisted positions on mount. We don't refetch
  // when the graph payload changes — the saved set is keyed by
  // page_id and silently ignores ids that aren't in the current
  // graph, so the data stays correct as the user edits filters.
  useEffect(() => {
    let cancelled = false;
    commands
      .loadGraphPositions()
      .then((positions) => {
        if (!cancelled) setSavedPositions(positions);
      })
      .catch(() => {
        // Persistence is a best-effort feature — the user can still
        // use the graph if the table is unreachable. Mark as loaded
        // (empty) so the canvas doesn't sit on its hands.
        if (!cancelled) setSavedPositions([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Position-change handler. Two callsites in GraphCanvas:
  //   - `layoutstop` after fcose / dagre with the FULL set of nodes
  //   - `dragfree` per node with just the dropped position
  // Both feed into the same pending-map; the timer flushes them as
  // one Tauri call so SQLite never sees a write storm.
  const handlePositionsChange = useCallback(
    (positions: SavedPosition[]) => {
      // Mirror the new positions into local state IMMEDIATELY —
      // before the debounce timer or the IPC roundtrip. This was
      // the cause of the "graph rotates every time I open it / toggle
      // mini-map" bug: pre-fix, `savedPositions` was only updated
      // *after* `saveGraphPositions(...).then(...)` resolved, which
      // takes the 400 ms debounce + the Tauri IPC roundtrip — call it
      // ~500 ms. Any remount in that window (mini-map toggle,
      // wiki-changed event, filter change) found `savedPositions`
      // still empty, ran fcose with `randomize: true` again, and
      // produced a fresh "rotation" of the layout.
      // Updating state synchronously closes the window: by the time
      // any subsequent re-render reads `savedPositions`, the
      // post-fcose coordinates are already there and the next layout
      // is `preset`, not `fcose`.
      setSavedPositions((cur) => {
        const map = new Map<string, SavedPosition>();
        for (const p of cur ?? []) map.set(p.page_id, p);
        for (const p of positions) map.set(p.page_id, p);
        return Array.from(map.values());
      });

      // Persist the same positions to the DB asynchronously,
      // debounced so a flurry of drag events doesn't hammer SQLite.
      // The save is fire-and-forget for UI purposes — local state is
      // already consistent regardless of when (or if) the IPC
      // resolves.
      for (const p of positions) {
        pendingPositionsRef.current.set(p.page_id, p);
      }
      if (saveTimerRef.current !== null) {
        clearTimeout(saveTimerRef.current);
      }
      saveTimerRef.current = setTimeout(() => {
        const flush = Array.from(pendingPositionsRef.current.values());
        pendingPositionsRef.current.clear();
        saveTimerRef.current = null;
        if (flush.length === 0) return;
        commands.saveGraphPositions(flush).catch((e: unknown) => {
          console.warn("[Tier3] position save failed", e);
        });
      }, POSITION_SAVE_DEBOUNCE_MS);
    },
    [],
  );

  // "Re-layout" button. Clears the saved-positions table, resets the
  // local state to empty, and lets the next mount fall back to fcose.
  // The fcose output then lands back in `savedPositions` via the
  // normal layoutstop → handlePositionsChange flow, so subsequent
  // mounts again skip layout entirely.
  const handleReLayout = useCallback(() => {
    pendingPositionsRef.current.clear();
    if (saveTimerRef.current !== null) {
      clearTimeout(saveTimerRef.current);
      saveTimerRef.current = null;
    }
    commands
      .clearGraphPositions()
      .then(() => setSavedPositions([]))
      .catch((e: unknown) => setError(String(e)));
  }, []);

  function addTag() {
    const cleaned = tagInput.trim();
    if (!cleaned || tags.includes(cleaned)) return;
    setTags([...tags, cleaned]);
    setTagInput("");
  }

  return (
    <div className="flex h-full min-w-0 flex-col">
      <div className="flex flex-wrap items-center gap-3 border-b border-neutral-800 bg-neutral-950 p-3">
        <span className="text-xs font-medium uppercase tracking-wider text-neutral-500">
          Filters
        </span>
        <div className="flex flex-wrap items-center gap-1.5">
          {ALL_TYPES.map((t) => {
            const active = types.includes(t);
            return (
              <button
                key={t}
                type="button"
                onClick={() =>
                  setTypes((cur) =>
                    active ? cur.filter((x) => x !== t) : [...cur, t],
                  )
                }
                className={`rounded-full px-3 py-1 text-xs ${
                  active
                    ? "bg-emerald-700 text-white"
                    : "bg-neutral-800 text-neutral-300 hover:bg-neutral-700"
                }`}
              >
                {t}
              </button>
            );
          })}
        </div>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            addTag();
          }}
          className="flex items-center gap-1"
        >
          <input
            type="text"
            value={tagInput}
            onChange={(e) => setTagInput(e.target.value)}
            placeholder="add tag…"
            className="h-7 rounded-md border border-neutral-800 bg-neutral-900 px-2 text-xs focus:border-emerald-700 focus:outline-none"
          />
          <Button size="sm" type="submit">
            Add tag
          </Button>
        </form>
        {tags.length > 0 && (
          <ul className="flex flex-wrap gap-1">
            {tags.map((t) => (
              <li key={t}>
                <button
                  type="button"
                  onClick={() => setTags(tags.filter((x) => x !== t))}
                  className="rounded-full bg-neutral-800 px-2 py-0.5 text-xs hover:bg-red-900"
                >
                  {t} ×
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="flex items-center gap-1 text-xs text-neutral-400">
          Updated after
          <input
            type="date"
            value={updatedAfter}
            onChange={(e) => setUpdatedAfter(e.target.value)}
            className="h-7 rounded-md border border-neutral-800 bg-neutral-900 px-2 focus:border-emerald-700 focus:outline-none"
          />
          {updatedAfter && (
            <Button size="sm" variant="ghost" onClick={() => setUpdatedAfter("")}>
              Clear
            </Button>
          )}
        </div>
        {/*
          Layout-mode toggle + Re-layout. Force-directed (fcose) is
          the default; hierarchical (dagre) gives a top-down tree
          when the user is looking for parent/child structure. The
          Re-layout button wipes saved positions so the next mount
          reverts to the auto-arranged layout.
        */}
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => setLayoutMode("force")}
            className={`rounded-l-md border border-neutral-800 px-2 py-1 text-xs ${
              layoutMode === "force"
                ? "bg-emerald-700 text-white"
                : "bg-neutral-900 text-neutral-300 hover:bg-neutral-800"
            }`}
          >
            Force
          </button>
          <button
            type="button"
            onClick={() => setLayoutMode("hierarchical")}
            className={`-ml-px rounded-r-md border border-neutral-800 px-2 py-1 text-xs ${
              layoutMode === "hierarchical"
                ? "bg-emerald-700 text-white"
                : "bg-neutral-900 text-neutral-300 hover:bg-neutral-800"
            }`}
          >
            Hierarchical
          </button>
          <Button
            size="sm"
            variant="ghost"
            onClick={handleReLayout}
            title="Discard hand-tuned positions and re-arrange the graph automatically"
          >
            Re-layout
          </Button>
          <Button
            size="sm"
            variant={showMinimap ? "primary" : "ghost"}
            onClick={() => setShowMinimap((v) => !v)}
            title={
              showMinimap
                ? "Hide the mini-map overlay"
                : "Show a thumbnail overview for panning large graphs"
            }
          >
            Mini-map
          </Button>
        </div>
        {graph && (
          <span className="ml-auto text-xs text-neutral-500">
            {graph.nodes.length} nodes · {graph.edges.length} edges
          </span>
        )}
      </div>
      {/*
        Cluster chip strip — connected-component summary supplied by
        the GraphCanvas. Click a chip to fit the viewport on that
        cluster only; click "All" to release the focus. Hidden when
        there's only one cluster (no value in choosing).
      */}
      {clusters.length > 1 && (
        <div className="flex flex-wrap items-center gap-1.5 border-b border-neutral-800 bg-neutral-950/80 px-3 py-2 text-xs">
          <span className="text-neutral-500">Clusters:</span>
          <button
            type="button"
            onClick={() => setFocusedCluster(null)}
            className={`rounded-full px-2 py-0.5 ${
              focusedCluster === null
                ? "bg-emerald-700 text-white"
                : "bg-neutral-800 text-neutral-300 hover:bg-neutral-700"
            }`}
          >
            All
          </button>
          {clusters.map((c) => (
            <button
              key={c.id}
              type="button"
              onClick={() =>
                setFocusedCluster((cur) => (cur === c.id ? null : c.id))
              }
              className={`rounded-full px-2 py-0.5 ${
                focusedCluster === c.id
                  ? "bg-emerald-700 text-white"
                  : "bg-neutral-800 text-neutral-300 hover:bg-neutral-700"
              }`}
              title={`${c.size} pages`}
            >
              {c.size}
            </button>
          ))}
        </div>
      )}
      <div className="relative min-h-0 flex-1">
        {error && (
          <div className="absolute right-3 top-3 rounded-md border border-red-900 bg-red-950/80 p-2 text-xs text-red-300">
            {error}
          </div>
        )}
        {graph && graph.nodes.length === 0 && (
          <EmptyState
            title="No nodes match"
            description="Loosen your filters or clear them to see the full graph."
          />
        )}
        {graph && graph.nodes.length > 0 && savedPositions !== null && (
          <>
            {graph.edges.length === 0 && (
              <div className="absolute left-1/2 top-3 z-10 -translate-x-1/2 rounded-md border border-amber-900 bg-amber-950/80 px-3 py-1.5 text-xs text-amber-200 shadow-lg">
                No <code className="font-mono">[[wiki-links]]</code> between
                these pages — the graph shows only the nodes. Add references
                in your Markdown to draw connections.
              </div>
            )}
            {/*
              GraphCanvas only mounts after savedPositions is loaded
              (initial null → resolved array, possibly empty). This
              avoids the "fcose runs, then preset re-runs because
              savedPositions just arrived" double-layout flicker on
              first open of the Graph tab in a session.
            */}
            <GraphCanvas
              nodes={graph.nodes}
              edges={graph.edges}
              onNodeClick={(id) =>
                navigate(`/viewer?id=${encodeURIComponent(id)}`)
              }
              layoutMode={layoutMode}
              savedPositions={savedPositions}
              onPositionsChange={handlePositionsChange}
              onClustersChange={setClusters}
              focusedSubset={
                focusedCluster
                  ? (clusters.find((c) => c.id === focusedCluster)
                      ?.nodeIds ?? null)
                  : null
              }
              showMinimap={showMinimap}
            />
          </>
        )}
        {!graph && <p className="p-6 text-sm text-neutral-500">Loading graph…</p>}
        {graph &&
          graph.nodes.length > 0 &&
          savedPositions === null && (
            <p className="p-6 text-sm text-neutral-500">
              Loading layout…
            </p>
          )}
      </div>
    </div>
  );
}

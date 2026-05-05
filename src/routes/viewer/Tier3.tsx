import { useCallback, useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import { GraphCanvas } from "../../components/GraphCanvas";
import { Button } from "../../components/ui/Button";
import { EmptyState } from "../../components/ui/EmptyState";

type Graph = Awaited<ReturnType<typeof commands.getGraph>>;

const ALL_TYPES = ["entity", "concept", "source", "topic"];

export function Tier3() {
  const navigate = useNavigate();
  const [graph, setGraph] = useState<Graph | null>(null);
  const [types, setTypes] = useState<string[]>([]);
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState<string[]>([]);
  const [updatedAfter, setUpdatedAfter] = useState<string>("");
  const [error, setError] = useState<string | null>(null);

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
        {graph && (
          <span className="ml-auto text-xs text-neutral-500">
            {graph.nodes.length} nodes · {graph.edges.length} edges
          </span>
        )}
      </div>
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
        {graph && graph.nodes.length > 0 && (
          <>
            {graph.edges.length === 0 && (
              <div className="absolute left-1/2 top-3 z-10 -translate-x-1/2 rounded-md border border-amber-900 bg-amber-950/80 px-3 py-1.5 text-xs text-amber-200 shadow-lg">
                No <code className="font-mono">[[wiki-links]]</code> between
                these pages — the graph shows only the nodes. Add references
                in your Markdown to draw connections.
              </div>
            )}
            <GraphCanvas
              nodes={graph.nodes}
              edges={graph.edges}
              onNodeClick={(id) =>
                navigate(`/viewer?id=${encodeURIComponent(id)}`)
              }
            />
          </>
        )}
        {!graph && <p className="p-6 text-sm text-neutral-500">Loading graph…</p>}
      </div>
    </div>
  );
}

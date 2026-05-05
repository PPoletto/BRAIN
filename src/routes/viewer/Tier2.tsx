import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import { Button } from "../../components/ui/Button";
import { EmptyState } from "../../components/ui/EmptyState";
import { ResizableSplit } from "../../components/ui/ResizableSplit";
import { Tabs } from "../../components/ui/Tabs";
import { MarkdownRenderer } from "../../components/MarkdownRenderer";

type Hit = Awaited<ReturnType<typeof commands.searchPages>>[number];
type QueryHit = Awaited<ReturnType<typeof commands.queryPages>>[number];
type Backlink = Awaited<ReturnType<typeof commands.getBacklinks>>[number];
type PageView = Awaited<ReturnType<typeof commands.readPage>>;
type Mode = "fts" | "query";

const MODE_TABS = [
  { id: "fts", label: "Full-text" },
  { id: "query", label: "Query DSL" },
];

export function Tier2() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const [mode, setMode] = useState<Mode>(() =>
    (params.get("mode") as Mode) === "query" ? "query" : "fts",
  );
  const [query, setQuery] = useState(params.get("q") ?? "");
  const [hits, setHits] = useState<Hit[]>([]);
  const [queryHits, setQueryHits] = useState<QueryHit[]>([]);
  const [searched, setSearched] = useState(false);
  const [searching, setSearching] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [page, setPage] = useState<PageView | null>(null);
  const [pageLoading, setPageLoading] = useState(false);
  const [backlinks, setBacklinks] = useState<Backlink[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const initial = params.get("q");
    if (initial) {
      void runSearch(initial);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Live-update on auto-commit / disk-reconnect — re-run the current
  // query so a newly created page that matches shows up without the
  // user having to press Search again. We also refresh the open page
  // body so an edit made via MCP propagates into the reader.
  useDataRefresh(() => {
    const q = params.get("q");
    if (q && q.trim().length > 0) {
      void runSearch(q);
    }
    if (selected) {
      void Promise.all([
        commands.readPage(selected),
        commands.getBacklinks(selected),
      ])
        .then(([p, bl]) => {
          setPage(p);
          setBacklinks(bl);
        })
        .catch(() => {
          // Silently swallow — the user might have just deleted the
          // page; the reader's existing error/empty state handles
          // the visible reaction.
        });
    }
  });

  function setModeAndUrl(m: string) {
    const next = m as Mode;
    setMode(next);
    const sp = new URLSearchParams(params);
    sp.set("mode", next);
    setParams(sp, { replace: true });
  }

  async function runSearch(q: string) {
    if (searching) return;
    setError(null);
    setSearched(true);
    setSearching(true);
    try {
      if (mode === "fts") {
        const results = await commands.searchPages(q);
        setHits(results);
        setQueryHits([]);
      } else {
        const results = await commands.queryPages(q);
        setQueryHits(results);
        setHits([]);
      }
      const sp = new URLSearchParams(params);
      sp.set("q", q);
      sp.set("mode", mode);
      setParams(sp, { replace: true });
    } catch (err: unknown) {
      setError(String(err));
    } finally {
      setSearching(false);
    }
  }

  async function openHit(id: string) {
    setSelected(id);
    setPage(null);
    setBacklinks([]);
    setPageLoading(true);
    try {
      // Page body and backlinks in parallel — both are independent reads,
      // and waiting them together keeps the UX a single transition rather
      // than a flicker between two empty states.
      const [p, bl] = await Promise.all([
        commands.readPage(id),
        commands.getBacklinks(id),
      ]);
      setPage(p);
      setBacklinks(bl);
    } catch (err: unknown) {
      setError(String(err));
    } finally {
      setPageLoading(false);
    }
  }

  return (
    <ResizableSplit
      storageKey="brain.tier2.sidebar"
      initial={420}
      min={300}
      max={620}
      left={
        <div className="flex h-full min-w-0 flex-col bg-neutral-950">
          <Tabs tabs={MODE_TABS} active={mode} onChange={setModeAndUrl} className="px-3" />
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void runSearch(query);
            }}
            className="flex items-center gap-2 border-b border-neutral-800 p-3"
          >
            <input
              type="search"
              autoFocus
              placeholder={
                mode === "fts"
                  ? "Search wiki…"
                  : "type:source AND tag:customer AND updated:>2026-04-01"
              }
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="h-9 flex-1 rounded-md border border-neutral-800 bg-neutral-900 px-3 text-sm placeholder:text-neutral-600 focus:border-emerald-700 focus:outline-none"
            />
            <Button variant="primary" size="md" type="submit" loading={searching}>
              {searching
                ? "Searching…"
                : mode === "fts"
                  ? "Search"
                  : "Run query"}
            </Button>
          </form>
          {mode === "query" && (
            <div className="border-b border-neutral-800 px-3 py-2 text-xs text-neutral-500">
              <strong className="text-neutral-300">Fields:</strong> id · type · title · tag · created · updated.{" "}
              <strong className="text-neutral-300">Operators:</strong> <code>:</code>, <code>:&gt;</code>, <code>:&lt;</code>.{" "}
              <strong className="text-neutral-300">Combine:</strong> AND, OR, NOT, parens.
            </div>
          )}
          {error && (
            <p className="m-3 rounded-md border border-red-900 bg-red-950/40 p-2 text-sm text-red-300">
              {error}
            </p>
          )}
          <div className="min-h-0 flex-1 overflow-y-auto">
            {searching && hits.length === 0 && queryHits.length === 0 && (
              <div className="flex items-center gap-3 p-4 text-sm text-neutral-400">
                <span className="inline-block size-3.5 animate-spin rounded-full border-2 border-neutral-700 border-t-emerald-500" />
                {mode === "fts"
                  ? "Searching FTS5 + vector index…"
                  : "Running query…"}
              </div>
            )}
            {!searched && !searching && (
              <EmptyState
                icon="🔍"
                title={mode === "fts" ? "Search your BRAIN" : "Query your BRAIN"}
                description={
                  mode === "fts"
                    ? "Lexical (FTS5). Enter a query and press Search."
                    : "Structured filter over frontmatter. Try `tag:nis2 OR tag:dora`."
                }
              />
            )}
            {mode === "fts" && (
              <ul className="divide-y divide-neutral-800">
                {hits.map((h) => (
                  <li key={h.id}>
                    <button
                      type="button"
                      onClick={() => void openHit(h.id)}
                      className={`block w-full px-3 py-2.5 text-left transition-colors hover:bg-neutral-900 ${
                        selected === h.id ? "bg-neutral-900" : ""
                      }`}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <div className="truncate font-medium text-neutral-100">{h.title}</div>
                        <span className="shrink-0 rounded bg-neutral-800 px-1.5 py-0.5 font-mono text-[10px] text-neutral-400">
                          {h.score.toFixed(1)}
                        </span>
                      </div>
                      <div className="truncate font-mono text-xs text-neutral-500">{h.id}</div>
                      <div className="mt-1 truncate text-sm text-neutral-300">{h.snippet}</div>
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {mode === "query" && (
              <ul className="divide-y divide-neutral-800">
                {queryHits.map((h) => (
                  <li key={h.id}>
                    <button
                      type="button"
                      onClick={() => void openHit(h.id)}
                      className={`block w-full px-3 py-2.5 text-left transition-colors hover:bg-neutral-900 ${
                        selected === h.id ? "bg-neutral-900" : ""
                      }`}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <div className="truncate font-medium text-neutral-100">
                          {h.title || h.id}
                        </div>
                        <span className="shrink-0 rounded bg-neutral-800 px-1.5 py-0.5 font-mono text-[10px] text-neutral-400">
                          {h.type}
                        </span>
                      </div>
                      <div className="truncate font-mono text-xs text-neutral-500">{h.id}</div>
                      {h.updated_at && (
                        <div className="mt-1 text-xs text-neutral-500">
                          updated {h.updated_at}
                        </div>
                      )}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {searched && !searching &&
              ((mode === "fts" && hits.length === 0) ||
                (mode === "query" && queryHits.length === 0)) &&
              !error && (
                <EmptyState
                  title="No matches"
                  description={
                    mode === "fts"
                      ? "Try a different query or fewer words."
                      : "Try loosening the filter or check the field names."
                  }
                />
              )}
          </div>
        </div>
      }
      right={
        <article className="flex h-full min-w-0 flex-col bg-neutral-950">
          {!selected && (
            <EmptyState
              icon="📖"
              title="Pick a result to read it"
              description="The page body, frontmatter and backlinks land here when you click a hit."
            />
          )}
          {selected && pageLoading && !page && (
            <div className="flex items-center gap-3 p-4 text-sm text-neutral-400">
              <span className="inline-block size-3.5 animate-spin rounded-full border-2 border-neutral-700 border-t-emerald-500" />
              Loading page…
            </div>
          )}
          {page && (
            <>
              <header className="flex items-start justify-between gap-3 border-b border-neutral-800 px-6 py-4">
                <div className="min-w-0">
                  <h2 className="truncate text-xl font-semibold">{page.title}</h2>
                  <div className="mt-0.5 truncate font-mono text-xs text-neutral-500">
                    {page.id}
                  </div>
                </div>
                <Button
                  size="sm"
                  onClick={() =>
                    navigate(`/viewer?id=${encodeURIComponent(page.id)}`)
                  }
                  title="Open this page in the Browse tab"
                >
                  Open in Browse
                </Button>
              </header>
              <div className="min-h-0 flex-1 overflow-y-auto">
                <div className="prose prose-invert prose-neutral max-w-none px-6 py-4">
                  {/*
                    Wiki-link clicks inside the rendered markdown call
                    `openHit` so the linked page replaces the current one
                    in *this* reader. Without this handler the default
                    `wikilink://` href is followed by the browser and the
                    user lands on a 404. The "Open in Browse" button
                    above is the cross-tab equivalent.
                  */}
                  <MarkdownRenderer
                    source={page.body}
                    onWikiLinkClick={(id) => void openHit(id)}
                  />
                </div>
                <section className="border-t border-neutral-800 px-6 py-4">
                  <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Backlinks
                  </h3>
                  {backlinks.length === 0 ? (
                    <p className="text-sm text-neutral-500">No inbound links.</p>
                  ) : (
                    <ul className="space-y-2">
                      {backlinks.map((b) => (
                        <li key={b.id}>
                          <button
                            type="button"
                            onClick={() => void openHit(b.id)}
                            className="block w-full rounded-md border border-neutral-800 p-2 text-left hover:bg-neutral-900"
                          >
                            <div className="font-medium text-neutral-200">{b.title}</div>
                            <div className="font-mono text-xs text-neutral-500">{b.id}</div>
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              </div>
            </>
          )}
        </article>
      }
    />
  );
}

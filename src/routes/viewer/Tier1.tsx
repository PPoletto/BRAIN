import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import { MarkdownRenderer } from "../../components/MarkdownRenderer";
import { ResizableSplit } from "../../components/ui/ResizableSplit";
import { Button } from "../../components/ui/Button";
import { EmptyState } from "../../components/ui/EmptyState";
import { useAsyncAction } from "../../components/ui/useAsyncAction";

type Tree = Awaited<ReturnType<typeof commands.listWikiTree>>;
type PageView = Awaited<ReturnType<typeof commands.readPage>>;

export function Tier1() {
  const navigate = useNavigate();

  // The `id` query param drives which page is open. Other surfaces
  // (graph, search, history) navigate here with `?id=…` to deep-link a
  // page. We *push* (not replace) on every internal pick so the
  // browser-back button restores the previous selection — earlier the
  // replace mode meant users could only get back via Alt+Left/Cmd+[.
  const [params, setParams] = useSearchParams();
  const idFromUrl = params.get("id");

  const [tree, setTree] = useState<Tree | null>(null);
  const [selected, setSelected] = useState<string | null>(idFromUrl);
  const [page, setPage] = useState<PageView | null>(null);
  const [pageNotFound, setPageNotFound] = useState(false);
  const [showFrontmatter, setShowFrontmatter] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function refreshTree() {
    commands
      .listWikiTree()
      .then(setTree)
      .catch((e: unknown) => setError(String(e)));
  }

  // Picks an id and keeps the URL in sync. `replace` is only true when we
  // resync `selected` from a URL change that's already in history (back
  // button, deep-link from another tab) — internal clicks push so the
  // user can navigate back the same way they came.
  function pick(id: string | null, opts: { replace?: boolean } = {}) {
    setSelected(id);
    const next = new URLSearchParams(params);
    if (id) {
      next.set("id", id);
    } else {
      next.delete("id");
    }
    setParams(next, { replace: opts.replace ?? false });
  }

  useEffect(() => {
    refreshTree();
  }, []);

  // Live-update on auto-commit (new pages from MCP, edits from external
  // editors) and on disk reconnect.
  useDataRefresh(() => {
    refreshTree();
    // Re-fetch the open page too, so an edit made via MCP shows up in
    // the reader without the user having to click the tree again.
    if (selected) {
      commands
        .readPage(selected)
        .then((p) => {
          setPage(p);
          setPageNotFound(false);
        })
        .catch(() => {
          // If the open page was deleted by the change we just got
          // notified about, surface the friendly 404 state.
          setPageNotFound(true);
          setPage(null);
        });
    }
  });

  // React to external URL changes (graph node click, paste link, browser
  // back/forward) without breaking internal `pick()` calls — the dependency
  // is the URL param, so a navigate({/viewer?id=…}) round-trip propagates.
  useEffect(() => {
    if (idFromUrl !== selected) {
      setSelected(idFromUrl);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [idFromUrl]);

  useEffect(() => {
    // Reset the per-selection states up-front so the previous page's
    // body doesn't flash while the new one is loading, and a 404 from
    // an unknown id replaces the old content cleanly.
    setPage(null);
    setPageNotFound(false);
    if (!selected) return;
    commands
      .readPage(selected)
      .then((p) => setPage(p))
      .catch((e: unknown) => {
        const msg = String(e);
        // The backend signals missing pages with "page not found:" /
        // "io error" / "no such file". We treat any read failure as a
        // friendly 404 rather than dumping a stack — broken `[[wiki
        // links]]` are common during refactors and shouldn't crash the
        // viewer with React's error overlay.
        if (
          msg.toLowerCase().includes("not found") ||
          msg.toLowerCase().includes("no such file")
        ) {
          setPageNotFound(true);
        } else {
          setError(msg);
        }
      });
  }, [selected]);

  const openInEditorAction = useAsyncAction(
    async (id: string) => {
      await commands.openPageInExternalEditor(id);
    },
    {
      success: "Opening in your default editor…",
      errorPrefix: "Could not open editor",
    },
  );

  return (
    <ResizableSplit
      storageKey="brain.tier1.sidebar"
      initial={280}
      min={220}
      max={520}
      left={
        <div className="flex h-full flex-col border-r border-neutral-800 bg-neutral-950">
          <div className="border-b border-neutral-800 p-3 text-xs font-medium uppercase tracking-wider text-neutral-500">
            Wiki tree
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-3">
            {error && (
              <p className="mb-2 text-sm text-red-400">{error}</p>
            )}
            {tree && (
              <>
                <Section
                  title="Entities"
                  ids={tree.entities}
                  selected={selected}
                  onPick={pick}
                />
                <Section
                  title="Concepts"
                  ids={tree.concepts}
                  selected={selected}
                  onPick={pick}
                />
                <Section
                  title="Sources"
                  ids={tree.sources}
                  selected={selected}
                  onPick={pick}
                />
                <Section
                  title="Topics"
                  ids={tree.topics}
                  selected={selected}
                  onPick={pick}
                />
              </>
            )}
          </div>
        </div>
      }
      right={
        <article className="flex h-full min-w-0 flex-col overflow-hidden">
          {!selected && (
            <EmptyState
              icon="📖"
              title="Pick a page from the tree"
              description="Pages are grouped by type (entities, concepts, sources, topics). Use the sidebar on the left or run a search via Ctrl+K."
            />
          )}
          {selected && pageNotFound && (
            <div className="flex h-full flex-col">
              <header className="border-b border-neutral-800 px-6 py-4">
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => navigate(-1)}
                  title="Go back to the previous page (also: Alt+Left)"
                >
                  ← Back
                </Button>
              </header>
              <div className="flex flex-1 items-center justify-center p-6">
                <div className="max-w-md text-center">
                  <div className="mb-3 text-5xl" aria-hidden>
                    🔗
                  </div>
                  <h2 className="text-lg font-semibold text-neutral-200">
                    Page does not exist
                  </h2>
                  <p className="mt-2 text-sm text-neutral-500">
                    The link points at{" "}
                    <code className="font-mono text-neutral-400">{selected}</code>
                    , but no page with that id is in the wiki yet.
                  </p>
                  <p className="mt-2 text-xs text-neutral-600">
                    Broken wiki-links are common during refactors. Either
                    create the missing page in your editor (it will show up
                    here once auto-commit fires) or remove the link from the
                    referencing page.
                  </p>
                  <div className="mt-5 flex justify-center gap-2">
                    <Button size="sm" onClick={() => navigate(-1)}>
                      ← Back
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => pick(null)}
                    >
                      Clear selection
                    </Button>
                  </div>
                </div>
              </div>
            </div>
          )}
          {selected && !pageNotFound && !page && (
            <div className="flex items-center gap-3 p-4 text-sm text-neutral-400">
              <span className="inline-block size-3.5 animate-spin rounded-full border-2 border-neutral-700 border-t-emerald-500" />
              Loading page…
            </div>
          )}
          {page && !pageNotFound && (
            <>
              <header className="border-b border-neutral-800 px-6 py-4">
                <div className="flex items-start justify-between gap-3">
                  <div className="flex min-w-0 items-start gap-2">
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => navigate(-1)}
                      title="Go back to the previous page (also: Alt+Left)"
                      className="shrink-0"
                    >
                      ←
                    </Button>
                    <div className="min-w-0">
                      <h2 className="truncate text-xl font-semibold">
                        {page.title}
                      </h2>
                      <div className="mt-0.5 truncate font-mono text-xs text-neutral-500">
                        {page.id}
                      </div>
                    </div>
                  </div>
                  <Button
                    size="sm"
                    loading={openInEditorAction.loading}
                    disabled={!selected}
                    onClick={() => {
                      if (selected) void openInEditorAction.trigger(selected);
                    }}
                  >
                    {openInEditorAction.loading ? "Opening…" : "Open in editor"}
                  </Button>
                </div>
                <button
                  type="button"
                  onClick={() => setShowFrontmatter((v) => !v)}
                  className="mt-2 text-xs text-neutral-500 hover:text-neutral-300"
                >
                  {showFrontmatter ? "Hide" : "Show"} frontmatter
                </button>
                {showFrontmatter && (
                  <pre className="mt-2 overflow-x-auto rounded-md bg-neutral-950 p-3 font-mono text-xs text-neutral-300">
                    {page.frontmatter}
                  </pre>
                )}
              </header>
              <div className="min-h-0 flex-1 overflow-y-auto">
                <MarkdownRenderer source={page.body} onWikiLinkClick={pick} />
              </div>
            </>
          )}
        </article>
      }
    />
  );
}

function Section({
  title,
  ids,
  selected,
  onPick,
}: {
  title: string;
  ids: string[];
  selected: string | null;
  onPick: (id: string) => void;
}) {
  if (ids.length === 0) return null;
  return (
    <div className="mb-4">
      <h3 className="mb-1 text-xs uppercase tracking-wider text-neutral-500">
        {title} <span className="ml-1 text-neutral-600">({ids.length})</span>
      </h3>
      <ul className="space-y-0.5">
        {ids.map((id) => {
          const label = id.split("/").slice(1).join("/");
          const isSelected = id === selected;
          return (
            <li key={id}>
              <button
                type="button"
                onClick={() => onPick(id)}
                className={`block w-full truncate rounded px-2 py-1 text-left text-sm transition-colors ${
                  isSelected
                    ? "bg-neutral-800 text-emerald-300"
                    : "text-neutral-300 hover:bg-neutral-900 hover:text-neutral-100"
                }`}
                title={id}
              >
                {label}
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

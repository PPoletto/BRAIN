import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import { EmptyState } from "../../components/ui/EmptyState";
import { ResizableSplit } from "../../components/ui/ResizableSplit";
import { Button } from "../../components/ui/Button";
import { useAsyncAction } from "../../components/ui/useAsyncAction";
import { useWikiHistoryStore } from "../../lib/wikiHistoryStore";

type Detail = Awaited<ReturnType<typeof commands.wikiCommitDetail>>;

export function WikiHistory() {
  const navigate = useNavigate();
  // Read from the prefetched store. AppShell warms this on shell mount
  // so the first visit to History renders instantly from cache; the
  // local component used to fire its own `wikiHistory(100)` here and
  // sit on a spinner for ~1–2 s while the IPC walked the Git log.
  const cachedEntries = useWikiHistoryStore((s) => s.entries);
  const storeLoading = useWikiHistoryStore((s) => s.loading);
  const storeError = useWikiHistoryStore((s) => s.error);
  const invalidateHistory = useWikiHistoryStore((s) => s.invalidate);
  const commits = cachedEntries ?? [];
  // Only show the "Loading…" placeholder for the cold-cache case
  // (entries still null). Once we have entries — even stale ones — we
  // render them and let a background refresh swap them in.
  const loading = cachedEntries === null && storeLoading;
  const [error, setError] = useState<string | null>(null);
  const displayError = error ?? storeError;
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<Detail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  // Live-update on every auto-commit so the timeline grows in real
  // time without the user having to switch tabs and back. Also handles
  // the disk-reconnect case where commits made on another machine
  // become visible after a remount. The store's own `wiki-changed`
  // subscription handles the timeline refresh; this hook only needs to
  // keep the *currently-selected commit's detail panel* fresh.
  useDataRefresh(() => {
    invalidateHistory();
    if (selected) {
      commands
        .wikiCommitDetail(selected)
        .then(setDetail)
        .catch(() => {
          // Selected commit may have been pruned; clear silently.
          setSelected(null);
          setDetail(null);
        });
    }
  });

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      return;
    }
    setDetailLoading(true);
    commands
      .wikiCommitDetail(selected)
      .then(setDetail)
      .catch((e: unknown) => setError(String(e)))
      .finally(() => setDetailLoading(false));
  }, [selected]);

  const restoreAction = useAsyncAction(
    async ({ sha, page }: { sha: string; page: string }) => {
      await commands.wikiRestorePage(sha, page);
      // The restore writes a new "revert: …" auto-commit which fires
      // `wiki-changed`; AppShell's listener then `invalidate()`s the
      // store. Calling it here too closes the race where the event
      // fires before AppShell's subscription is up (e.g. first render).
      invalidateHistory();
    },
    {
      success: "Page restored",
      errorPrefix: "Restore failed",
    },
  );

  const hardResetAction = useAsyncAction(
    async (sha: string) => {
      await commands.wikiHardReset(sha);
      invalidateHistory();
    },
    {
      success: "Wiki reset",
      errorPrefix: "Reset failed",
    },
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="border-b border-neutral-800 px-6 py-4">
        <h1 className="text-2xl font-semibold">Wiki history</h1>
        <p className="mt-1 text-sm text-neutral-400">
          Auto-commits and explicit revert/reset operations on{" "}
          <code className="font-mono">02_wiki/</code>. Click a commit to see
          what changed.
        </p>
      </header>
      <ResizableSplit
        storageKey="brain.history.split"
        initial={420}
        min={320}
        max={640}
        left={
          <div className="flex h-full flex-col bg-neutral-950">
            <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
              {displayError && (
                <p className="rounded-md border border-red-900 bg-red-950/40 p-3 text-sm text-red-300">
                  {displayError}
                </p>
              )}
              {loading && <p className="text-sm text-neutral-500">Loading…</p>}
              {!loading && commits.length === 0 && (
                <EmptyState
                  icon="🕓"
                  title="No commits yet"
                  description="When you (or an LLM via MCP) edit pages, BRAIN auto-commits after 5 s of idle. Edits will start showing up here."
                />
              )}
              <ol className="relative space-y-3 border-l border-neutral-800 pl-5">
                {commits.map((c) => {
                  const kind = commitKind(c.message);
                  const isSel = c.sha === selected;
                  return (
                    <li key={c.sha} className="relative">
                      <span
                        className={`absolute -left-[27px] top-2 size-3 rounded-full ring-4 ring-neutral-950 ${kind.dot}`}
                        aria-hidden
                      />
                      <button
                        type="button"
                        onClick={() => setSelected(c.sha)}
                        className={`block w-full rounded-md border p-3 text-left transition-colors ${
                          isSel
                            ? "border-emerald-700 bg-neutral-900"
                            : "border-neutral-800 bg-neutral-900 hover:border-neutral-700"
                        }`}
                      >
                        <div className="flex items-start justify-between gap-2">
                          <div className="flex items-center gap-2">
                            <span
                              className={`rounded-full px-2 py-0.5 text-xs ${kind.badge}`}
                            >
                              {kind.label}
                            </span>
                            <code className="font-mono text-xs text-neutral-500">
                              {c.sha.slice(0, 8)}
                            </code>
                          </div>
                          <span className="font-mono text-xs text-neutral-500">{c.ts}</span>
                        </div>
                        <p className="mt-2 text-sm text-neutral-200">
                          {c.message.split("\n")[0]}
                        </p>
                        <p className="mt-1 text-xs text-neutral-500">
                          {c.files_changed} file{c.files_changed === 1 ? "" : "s"} changed
                        </p>
                      </button>
                    </li>
                  );
                })}
              </ol>
            </div>
          </div>
        }
        right={
          <aside className="flex h-full min-w-0 flex-col bg-neutral-950">
            {!selected && (
              <EmptyState
                icon="📋"
                title="Pick a commit"
                description="Click any commit on the left to see the changed files, the diff, and per-page restore actions."
              />
            )}
            {selected && detailLoading && !detail && (
              <div className="flex items-center gap-3 p-4 text-sm text-neutral-400">
                <span className="inline-block size-3.5 animate-spin rounded-full border-2 border-neutral-700 border-t-emerald-500" />
                Loading commit…
              </div>
            )}
            {detail && (
              <>
                <header className="border-b border-neutral-800 px-6 py-4">
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <h2 className="font-mono text-sm">
                        {detail.sha.slice(0, 8)}{" "}
                        <span className="ml-1 text-neutral-500">
                          · {detail.author}
                        </span>
                      </h2>
                      <p className="mt-1 text-xs text-neutral-500">{detail.ts}</p>
                    </div>
                    <Button
                      size="sm"
                      variant="destructive"
                      loading={hardResetAction.loading}
                      onClick={() => {
                        const ok = window.confirm(
                          `Hard-reset the wiki to ${detail.sha.slice(0, 8)}?\n\n` +
                            "This adds a new \"reset:\" commit pointing the wiki at\n" +
                            "the selected state — earlier commits stay in the\n" +
                            "history, no data is destroyed.",
                        );
                        if (ok) void hardResetAction.trigger(detail.sha);
                      }}
                      title="Add a new reset commit pointing the wiki at this state"
                    >
                      {hardResetAction.loading ? "Resetting…" : "Hard-reset to here"}
                    </Button>
                  </div>
                  <pre className="mt-3 whitespace-pre-wrap break-words text-sm text-neutral-200">
                    {detail.message}
                  </pre>
                </header>
                <div className="min-h-0 flex-1 overflow-y-auto">
                  <section className="border-b border-neutral-800 px-6 py-4">
                    <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-neutral-500">
                      Files ({detail.files.length})
                    </h3>
                    {detail.files.length === 0 ? (
                      <p className="text-sm text-neutral-500">
                        No file changes recorded.
                      </p>
                    ) : (
                      <ul className="space-y-1.5">
                        {detail.files.map((f) => {
                          const pageId = pageIdFromPath(f.path);
                          return (
                            <li
                              key={f.path}
                              className="flex items-center justify-between gap-3 rounded-md border border-neutral-800 bg-neutral-900 px-3 py-2"
                            >
                              <div className="min-w-0 flex-1">
                                <div className="flex items-center gap-2">
                                  <span
                                    className={`inline-flex size-5 items-center justify-center rounded font-mono text-[10px] ${statusBadge(f.status)}`}
                                    title={statusLabel(f.status)}
                                  >
                                    {f.status}
                                  </span>
                                  <code className="truncate font-mono text-xs text-neutral-300">
                                    {f.path}
                                  </code>
                                </div>
                                <div className="mt-0.5 ml-7 font-mono text-[11px]">
                                  <span className="text-emerald-400">+{f.insertions}</span>
                                  <span className="ml-2 text-red-400">−{f.deletions}</span>
                                </div>
                              </div>
                              <div className="flex shrink-0 gap-1">
                                {pageId && (
                                  <button
                                    type="button"
                                    onClick={() =>
                                      navigate(
                                        `/viewer?id=${encodeURIComponent(pageId)}`,
                                      )
                                    }
                                    className="rounded border border-neutral-700 px-2 py-0.5 text-xs hover:bg-neutral-800"
                                    title="Open this page in the Browse tab"
                                  >
                                    Open
                                  </button>
                                )}
                                {pageId && f.status !== "A" && (
                                  <button
                                    type="button"
                                    onClick={() => {
                                      void restoreAction.trigger({
                                        sha: detail.sha,
                                        page: f.path,
                                      });
                                    }}
                                    disabled={restoreAction.loading}
                                    className="rounded border border-neutral-700 px-2 py-0.5 text-xs hover:bg-neutral-800 disabled:opacity-50"
                                    title="Replace the current file with the version at this commit"
                                  >
                                    {restoreAction.loading ? "…" : "Restore"}
                                  </button>
                                )}
                              </div>
                            </li>
                          );
                        })}
                      </ul>
                    )}
                  </section>
                  <section className="px-6 py-4">
                    <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-neutral-500">
                      Diff
                    </h3>
                    {detail.patch.trim() ? (
                      <pre className="overflow-x-auto rounded-md border border-neutral-800 bg-neutral-950 p-3 font-mono text-xs leading-relaxed">
                        {colourisePatch(detail.patch)}
                      </pre>
                    ) : (
                      <p className="text-sm text-neutral-500">
                        No textual diff (binary file or empty change).
                      </p>
                    )}
                  </section>
                </div>
              </>
            )}
          </aside>
        }
      />
    </div>
  );
}

function commitKind(message: string): { label: string; badge: string; dot: string } {
  const lower = message.toLowerCase();
  if (lower.startsWith("revert"))
    return {
      label: "Revert",
      badge: "bg-amber-900 text-amber-200",
      dot: "bg-amber-400",
    };
  if (lower.startsWith("reset"))
    return {
      label: "Reset",
      badge: "bg-red-900 text-red-200",
      dot: "bg-red-400",
    };
  if (lower.includes("write_page") || lower.includes("[trigger=mcp"))
    return {
      label: "MCP",
      badge: "bg-emerald-900 text-emerald-200",
      dot: "bg-emerald-400",
    };
  return {
    label: "Edit",
    badge: "bg-neutral-800 text-neutral-300",
    dot: "bg-neutral-500",
  };
}

function statusBadge(s: string): string {
  switch (s) {
    case "A":
      return "bg-emerald-900 text-emerald-200";
    case "M":
      return "bg-amber-900 text-amber-200";
    case "D":
      return "bg-red-900 text-red-200";
    case "R":
    case "C":
      return "bg-blue-900 text-blue-200";
    default:
      return "bg-neutral-800 text-neutral-300";
  }
}

function statusLabel(s: string): string {
  return (
    {
      A: "Added",
      M: "Modified",
      D: "Deleted",
      R: "Renamed",
      C: "Copied",
    } as Record<string, string>
  )[s] ?? "Changed";
}

/// Maps a wiki repo path (`entities/alice.md`) back to a page id
/// (`entities/alice`). Returns null for non-page files like `index.md` at
/// the root or for paths that don't end with `.md`.
function pageIdFromPath(path: string): string | null {
  if (!path.endsWith(".md")) return null;
  const stripped = path.slice(0, -3);
  if (!stripped.includes("/")) return null;
  return stripped;
}

/// Splits a unified diff into spans coloured by line origin (`+`/`-`/`@`).
/// Keeps the patch readable without pulling in a full diff-renderer dep.
function colourisePatch(patch: string) {
  return patch.split("\n").map((line, idx) => {
    let cls = "text-neutral-400";
    if (line.startsWith("+") && !line.startsWith("+++"))
      cls = "text-emerald-300";
    else if (line.startsWith("-") && !line.startsWith("---"))
      cls = "text-red-300";
    else if (line.startsWith("@@")) cls = "text-blue-300";
    else if (line.startsWith("diff ") || line.startsWith("index "))
      cls = "text-neutral-500";
    return (
      <span key={idx} className={cls}>
        {line}
        {"\n"}
      </span>
    );
  });
}

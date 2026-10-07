import { useCallback, useEffect, useMemo, useState } from "react";
import { commands } from "../../lib/commands";
import { useDataRefresh } from "../../lib/events";
import {
  COLUMNS,
  cellText,
  localToday,
  sortRows,
  toCsv,
  validityOf,
  type ColumnKey,
  type PageRow,
  type SortDir,
} from "../../lib/pageTable";
import { Button } from "../../components/ui/Button";
import { EmptyState } from "../../components/ui/EmptyState";
import { useToast } from "../../components/ui/toast-context";

/** localStorage key of the hidden columns (per-viewer convenience only). */
const HIDDEN_COLUMNS_KEY = "brain.pageTable.hiddenColumns";

function loadHiddenColumns(): ColumnKey[] {
  try {
    const raw = window.localStorage.getItem(HIDDEN_COLUMNS_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    const known = new Set<string>(COLUMNS.map((c) => c.key));
    return Array.isArray(parsed)
      ? (parsed.filter((k) => typeof k === "string" && known.has(k)) as ColumnKey[])
      : [];
  } catch {
    return [];
  }
}

function saveHiddenColumns(hidden: ColumnKey[]) {
  try {
    window.localStorage.setItem(HIDDEN_COLUMNS_KEY, JSON.stringify(hidden));
  } catch {
    // Storage unavailable (private mode, blocked) — the choice just isn't kept.
  }
}

const VALIDITY_STYLE: Record<string, string> = {
  current: "text-emerald-400",
  upcoming: "text-sky-400",
  expired: "text-amber-400",
  superseded: "text-neutral-500",
};

/**
 * The page table of the Search tab: the hits of a structured query as
 * sortable columns. The filter box takes the same query syntax as the
 * Query DSL list (the string goes to `query_pages` unchanged; `*` or
 * empty lists every current page).
 */
export function PageTable({
  query,
  onQueryChange,
  onOpen,
}: {
  query: string;
  onQueryChange: (q: string) => void;
  onOpen: (id: string) => void;
}) {
  const [filter, setFilter] = useState(query || "*");
  const [rows, setRows] = useState<PageRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sortKey, setSortKey] = useState<ColumnKey>("updated");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [hidden, setHidden] = useState<ColumnKey[]>(loadHiddenColumns);
  const [ran, setRan] = useState<string | null>(null);
  const { push } = useToast();
  const today = localToday();

  const run = useCallback(
    async (q: string) => {
      setLoading(true);
      setError(null);
      try {
        setRows(await commands.queryPages(q));
        setRan(q);
        onQueryChange(q);
      } catch (e: unknown) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    },
    [onQueryChange],
  );

  useEffect(() => {
    void run(filter);
    // Run once with the initial filter; later runs are explicit.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useDataRefresh(() => {
    if (ran !== null) void run(ran);
  });

  const visible = COLUMNS.filter((c) => !hidden.includes(c.key));
  const sorted = useMemo(
    () => sortRows(rows, sortKey, sortDir, today),
    [rows, sortKey, sortDir, today],
  );

  function toggleSort(key: ColumnKey) {
    if (key === sortKey) {
      setSortDir(sortDir === "asc" ? "desc" : "asc");
    } else {
      setSortKey(key);
      setSortDir(key === "reads" || key === "search_hits" || key === "updated" ? "desc" : "asc");
    }
  }

  function toggleColumn(key: ColumnKey) {
    const next = hidden.includes(key) ? hidden.filter((k) => k !== key) : [...hidden, key];
    setHidden(next);
    saveHiddenColumns(next);
  }

  async function copyCsv() {
    const csv = toCsv(
      sorted,
      visible.map((c) => c.key),
      today,
    );
    try {
      await navigator.clipboard.writeText(csv);
      push({ kind: "success", message: `${sorted.length} rows copied as CSV` });
    } catch (e: unknown) {
      push({
        kind: "error",
        message: "Could not copy to the clipboard",
        detail: e instanceof Error ? e.message : String(e),
      });
    }
  }

  function renderCell(row: PageRow, key: ColumnKey) {
    switch (key) {
      case "title":
        return (
          <button
            type="button"
            onClick={() => onOpen(row.id)}
            className="text-left font-medium text-emerald-300 hover:underline"
            title={row.id}
          >
            {row.title || row.id}
          </button>
        );
      case "type":
        return (
          <span className="rounded bg-neutral-800 px-1.5 py-0.5 font-mono text-[10px] text-neutral-400">
            {row.type}
          </span>
        );
      case "summary":
        return (
          <span className="block max-w-md truncate" title={row.summary ?? ""}>
            {row.summary ?? ""}
          </span>
        );
      case "validity": {
        const v = validityOf(row, today);
        return (
          <span className={VALIDITY_STYLE[v]}>
            {v}
            {v === "superseded" && row.superseded_by && (
              <>
                {" → "}
                <button
                  type="button"
                  onClick={() => onOpen(row.superseded_by as string)}
                  className="font-mono text-xs text-emerald-300 hover:underline"
                >
                  {row.superseded_by}
                </button>
              </>
            )}
          </span>
        );
      }
      case "reads":
      case "search_hits":
        return <span className="font-mono">{cellText(row, key, today)}</span>;
      default:
        return cellText(row, key, today);
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void run(filter);
        }}
        className="flex flex-wrap items-center gap-2 border-b border-neutral-800 p-3"
      >
        <input
          type="search"
          placeholder="* · type:entity · tag:customer · valid:all · sort:salience"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-9 min-w-64 flex-1 rounded-md border border-neutral-800 bg-neutral-900 px-3 font-mono text-sm placeholder:text-neutral-600 focus:border-emerald-700 focus:outline-none"
        />
        <Button variant="primary" size="md" type="submit" loading={loading}>
          Filter
        </Button>
        <Button size="md" type="button" onClick={() => void copyCsv()} disabled={sorted.length === 0}>
          CSV kopieren
        </Button>
      </form>
      <div className="flex flex-wrap items-center gap-3 border-b border-neutral-800 px-3 py-2 text-xs text-neutral-400">
        <span className="text-neutral-500">Columns:</span>
        {COLUMNS.map((c) => (
          <label key={c.key} className="flex items-center gap-1">
            <input
              type="checkbox"
              checked={!hidden.includes(c.key)}
              onChange={() => toggleColumn(c.key)}
              className="size-3.5 accent-emerald-500"
            />
            {c.label}
          </label>
        ))}
        <span className="ml-auto text-neutral-500">
          {rows.length} page{rows.length === 1 ? "" : "s"}
          {rows.length >= 200 ? " (first 200 — narrow the filter)" : ""}
        </span>
      </div>
      {error && (
        <p className="m-3 rounded-md border border-red-900 bg-red-950/40 p-2 text-sm text-red-300">
          {error}
        </p>
      )}
      <div className="min-h-0 flex-1 overflow-auto">
        {!loading && rows.length === 0 && !error ? (
          <EmptyState title="No pages" description="Try `*` or loosen the filter." />
        ) : (
          <table className="w-full text-left text-sm text-neutral-300">
            <thead className="sticky top-0 bg-neutral-950 text-xs text-neutral-500">
              <tr>
                {visible.map((c) => (
                  <th key={c.key} className="border-b border-neutral-800 px-3 py-2 font-normal">
                    <button
                      type="button"
                      onClick={() => toggleSort(c.key)}
                      className="flex items-center gap-1 hover:text-neutral-200"
                    >
                      {c.label}
                      {sortKey === c.key && <span>{sortDir === "asc" ? "▲" : "▼"}</span>}
                    </button>
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {sorted.map((row) => (
                <tr key={row.id} className="border-b border-neutral-900 hover:bg-neutral-900/60">
                  {visible.map((c) => (
                    <td key={c.key} className="px-3 py-1.5 align-top">
                      {renderCell(row, c.key)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

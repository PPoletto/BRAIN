/**
 * Pure helpers of the viewer's page table (Search tab → Query DSL →
 * "Tabelle"): validity, the sort comparator and the CSV export.
 */

/** One row: a `query_pages` hit (Rust `viewer::query::executor::QueryHit`). */
export type PageRow = {
  id: string;
  type: string;
  path: string;
  title: string;
  updated_at: string | null;
  reads: number;
  search_hits: number;
  last_read_at: number | null;
  valid_from?: string;
  valid_to?: string;
  superseded_by?: string;
  tags?: string[];
  summary?: string;
};

export type ColumnKey =
  | "title"
  | "type"
  | "updated"
  | "tags"
  | "summary"
  | "validity"
  | "reads"
  | "search_hits";

export type SortDir = "asc" | "desc";

export const COLUMNS: Array<{ key: ColumnKey; label: string }> = [
  { key: "title", label: "Title" },
  { key: "type", label: "Type" },
  { key: "updated", label: "Updated" },
  { key: "tags", label: "Tags" },
  { key: "summary", label: "Summary" },
  { key: "validity", label: "Validity" },
  { key: "reads", label: "Reads" },
  { key: "search_hits", label: "Search hits" },
];

export type Validity = "current" | "upcoming" | "expired" | "superseded";

/** A `YYYY-MM-DD` date, or null for anything else (as the index treats it). */
function isoDay(value: string | undefined): string | null {
  return value && /^\d{4}-\d{2}-\d{2}$/.test(value) ? value : null;
}

/**
 * Validity of a row on `today` (`YYYY-MM-DD`): replaced by another page,
 * past its `valid_to`, before its `valid_from`, or current.
 */
export function validityOf(row: PageRow, today: string): Validity {
  if (row.superseded_by) return "superseded";
  const to = isoDay(row.valid_to);
  if (to !== null && to < today) return "expired";
  const from = isoDay(row.valid_from);
  if (from !== null && from > today) return "upcoming";
  return "current";
}

const VALIDITY_RANK: Record<Validity, number> = {
  current: 0,
  upcoming: 1,
  expired: 2,
  superseded: 3,
};

/** The value a column sorts by; `null` = empty (always sorted last). */
function sortValue(row: PageRow, key: ColumnKey, today: string): string | number | null {
  switch (key) {
    case "title":
      return (row.title || row.id).toLowerCase();
    case "type":
      return row.type;
    case "updated":
      return row.updated_at || null;
    case "tags":
      return row.tags && row.tags.length > 0 ? row.tags.join(", ").toLowerCase() : null;
    case "summary":
      return row.summary ? row.summary.toLowerCase() : null;
    case "validity":
      return VALIDITY_RANK[validityOf(row, today)];
    case "reads":
      return row.reads;
    case "search_hits":
      return row.search_hits;
  }
}

/**
 * Comparator for `key` in direction `dir`. Empty values sort last in both
 * directions; ties fall back to the page id (ascending) so the order is
 * stable and deterministic.
 */
export function compareRows(
  a: PageRow,
  b: PageRow,
  key: ColumnKey,
  dir: SortDir,
  today: string,
): number {
  const va = sortValue(a, key, today);
  const vb = sortValue(b, key, today);
  if (va === null && vb !== null) return 1;
  if (vb === null && va !== null) return -1;
  let order = 0;
  if (va !== null && vb !== null) {
    if (typeof va === "number" && typeof vb === "number") {
      order = va - vb;
    } else {
      order = String(va).localeCompare(String(vb));
    }
  }
  if (order !== 0) return dir === "asc" ? order : -order;
  return a.id.localeCompare(b.id);
}

/** A sorted copy of `rows`. */
export function sortRows(
  rows: PageRow[],
  key: ColumnKey,
  dir: SortDir,
  today: string,
): PageRow[] {
  return [...rows].sort((a, b) => compareRows(a, b, key, dir, today));
}

/** The text of a cell as exported (and shown, apart from links). */
export function cellText(row: PageRow, key: ColumnKey, today: string): string {
  switch (key) {
    case "title":
      return row.title || row.id;
    case "type":
      return row.type;
    case "updated":
      return row.updated_at ?? "";
    case "tags":
      return (row.tags ?? []).join(", ");
    case "summary":
      return row.summary ?? "";
    case "validity": {
      const v = validityOf(row, today);
      return v === "superseded" && row.superseded_by ? `superseded → ${row.superseded_by}` : v;
    }
    case "reads":
      return String(row.reads);
    case "search_hits":
      return String(row.search_hits);
  }
}

/**
 * A cell a spreadsheet would read as a formula (`=`, `+`, `-`, `@`, tab
 * or carriage return at the start) gets a leading `'`, so pasting the
 * CSV into Excel / LibreOffice shows text instead of running it.
 */
export function neutraliseFormula(value: string): string {
  return /^[=+\-@\t\r]/.test(value) ? `'${value}` : value;
}

/**
 * One CSV field: formula-neutralised, then quoted (RFC 4180) when it
 * holds a comma, quote or line break.
 */
function csvField(value: string): string {
  const safe = neutraliseFormula(value);
  return /[",\r\n]/.test(safe) ? `"${safe.replace(/"/g, '""')}"` : safe;
}

/**
 * `rows` as CSV with a header line of the column labels: fields quoted as
 * RFC 4180 requires, lines separated by CRLF. The id is always the first
 * column so the export identifies each page.
 */
export function toCsv(rows: PageRow[], columns: ColumnKey[], today: string): string {
  const labels = new Map(COLUMNS.map((c) => [c.key, c.label]));
  const header = ["id", ...columns.map((k) => labels.get(k) ?? k)];
  const lines = [header.map(csvField).join(",")];
  for (const row of rows) {
    lines.push([row.id, ...columns.map((k) => cellText(row, k, today))].map(csvField).join(","));
  }
  return lines.join("\r\n");
}

/** Today as `YYYY-MM-DD` in local time. */
export function localToday(now: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

import { describe, it, expect } from "vitest";
import { compareRows, sortRows, toCsv, validityOf, type PageRow } from "./pageTable";

const TODAY = "2026-10-07";

function row(id: string, extra: Partial<PageRow> = {}): PageRow {
  return {
    id,
    type: "entity",
    path: `02_wiki/${id}.md`,
    title: id,
    updated_at: null,
    reads: 0,
    search_hits: 0,
    last_read_at: null,
    ...extra,
  };
}

const ids = (rows: PageRow[]) => rows.map((r) => r.id);

describe("validityOf", () => {
  it("calls a page with a successor superseded", () => {
    expect(validityOf(row("a", { superseded_by: "entities/b" }), TODAY)).toBe("superseded");
  });

  it("calls a page past its valid_to expired", () => {
    expect(validityOf(row("a", { valid_to: "2026-10-06" }), TODAY)).toBe("expired");
  });

  it("calls a page whose valid_to is today current", () => {
    expect(validityOf(row("a", { valid_to: TODAY }), TODAY)).toBe("current");
  });

  it("ignores a validity date that is not YYYY-MM-DD", () => {
    expect(validityOf(row("a", { valid_to: "yesterday" }), TODAY)).toBe("current");
  });
});

describe("compareRows", () => {
  it("sorts titles case-insensitively in ascending order", () => {
    const rows = [row("b", { title: "beta" }), row("a", { title: "Alpha" })];
    expect(ids(sortRows(rows, "title", "asc", TODAY))).toEqual(["a", "b"]);
  });

  it("reverses the order for descending", () => {
    const rows = [row("x", { reads: 1 }), row("y", { reads: 5 })];
    expect(ids(sortRows(rows, "reads", "desc", TODAY))).toEqual(["y", "x"]);
  });

  it("compares numbers numerically, not as text", () => {
    const rows = [row("x", { search_hits: 10 }), row("y", { search_hits: 9 })];
    expect(ids(sortRows(rows, "search_hits", "asc", TODAY))).toEqual(["y", "x"]);
  });

  it("puts empty values last in ascending order", () => {
    const rows = [row("none"), row("old", { updated_at: "2026-01-01" })];
    expect(ids(sortRows(rows, "updated", "asc", TODAY))).toEqual(["old", "none"]);
  });

  it("puts empty values last in descending order too", () => {
    const rows = [row("none"), row("old", { updated_at: "2026-01-01" })];
    expect(ids(sortRows(rows, "updated", "desc", TODAY))).toEqual(["old", "none"]);
  });

  it("breaks ties by page id", () => {
    expect(compareRows(row("b"), row("a"), "type", "desc", TODAY)).toBeGreaterThan(0);
  });

  it("orders validity current before expired before superseded", () => {
    const rows = [
      row("s", { superseded_by: "entities/n" }),
      row("e", { valid_to: "2020-01-01" }),
      row("c"),
    ];
    expect(ids(sortRows(rows, "validity", "asc", TODAY))).toEqual(["c", "e", "s"]);
  });
});

describe("toCsv", () => {
  it("starts with a header line of id and the column labels", () => {
    expect(toCsv([], ["title", "reads"], TODAY)).toBe("id,Title,Reads");
  });

  it("separates lines with CRLF", () => {
    expect(toCsv([row("entities/a")], ["reads"], TODAY)).toBe("id,Reads\r\nentities/a,0");
  });

  it("quotes a field with a comma", () => {
    const csv = toCsv([row("a", { tags: ["x", "y"] })], ["tags"], TODAY);
    expect(csv.split("\r\n")[1]).toBe('a,"x, y"');
  });

  it("doubles quotes inside a quoted field", () => {
    const csv = toCsv([row("a", { summary: 'Die "Beispiel GmbH"' })], ["summary"], TODAY);
    expect(csv.split("\r\n")[1]).toBe('a,"Die ""Beispiel GmbH"""');
  });

  it("quotes a field with a line break", () => {
    const csv = toCsv([row("a", { summary: "eins\nzwei" })], ["summary"], TODAY);
    expect(csv.endsWith('"eins\nzwei"')).toBe(true);
  });

  it("exports a superseded page with its successor", () => {
    const csv = toCsv([row("a", { superseded_by: "entities/b" })], ["validity"], TODAY);
    expect(csv.split("\r\n")[1]).toBe("a,superseded → entities/b");
  });
});

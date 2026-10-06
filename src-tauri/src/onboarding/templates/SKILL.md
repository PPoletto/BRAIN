---
name: brain-wiki
description: Maintain the user's BRAIN wiki (personal knowledge vault) through the BRAIN MCP tools (brain_*). Use it when the user asks you to remember, save or look up something they told you before, to ingest a document, email or transcript into the wiki, to clean up lint findings, or to "dream" / consolidate the wiki. Covers the four page types, frontmatter, wiki links, the tool map and the ingest, cleanup and dream workflows.
---

# BRAIN wiki

BRAIN is the user's persistent memory: a Markdown wiki in `02_wiki/` served
over MCP. The full conventions are in `00_meta/AGENTS.md` (MCP resource
`brain://agents-md`); this skill is the short version.

## Pages

- Exactly four types. Directory plural, frontmatter `type:` **singular**:
  `entities/` → `entity` (people, organisations, products), `concepts/` →
  `concept` (methods, terms), `sources/` → `source` (one ingested
  artifact), `topics/` → `topic` (a synthesis across sources).
- Frontmatter: `id`, `type`, `title`, `summary` (one or two sentences —
  search ranks it highest). Optional: `tags: [..]`, `aliases: [..]`,
  `sources: [sources/..]` (expected on entity and concept pages),
  `valid_from` / `valid_to` (YYYY-MM-DD), `superseded_by: <id>`,
  `distinct_from: [<id>]`, `keep: true` (the user says an unlinked or
  unread page stays: no orphan / decay reports for it).
- Links: always `[[entities/dan-shapiro]]` or `[[entities/dan-shapiro|Dan]]`
  (no `|alias` inside table cells). Only link to pages that exist.

## Tool map

| Need | Tool |
|---|---|
| Is BRAIN alive? Is search semantic? | `brain_ping` (`detail: true`) |
| Find pages about something | `brain_search` |
| Does this page / name exist already? | `brain_lookup` |
| Read pages (and their links) | `brain_get_pages` (`include_context: true` for links; `response_format: "detailed"` before rewriting) |
| List / filter by type, tag, date; tag counts | `brain_query` (`facet: "tags"`) |
| Link structure | `brain_graph` |
| Write one page / several linked pages | `brain_write_page` / `brain_write_batch` |
| Change one section | `brain_patch_page` |
| Wrong id / duplicate / junk | `brain_refactor` (`action: rename / merge / delete`) |
| Lint state | `brain_lint_report` |
| Undo | `brain_history` (`action: list / restore`) |
| Raw artifact before ingest | `brain_write_raw_file` |
| Search quality | `brain_eval` (`action: run / add`) |
| Consolidation | `brain_dream` (`action: queue / log`) |

Read tools default to `response_format: "concise"`; ask for `"detailed"`
only when you need the extra fields.

## Workflows

- **Ingest** (MCP prompt `ingest`): raw file (`brain_write_raw_file`) →
  `brain_search` + `brain_lookup` for what exists → one `source` page plus
  entity/concept pages → write them together in one `brain_write_batch`.
  Extend existing pages; never create duplicates.
- **Cleanup** (MCP prompt `lint-session`): newest audit (resource
  `brain://audit/latest`) or `brain_lint_report`; errors first, then one
  warning kind at a time (`response_format: "detailed"`, `kind`).
- **Dream** (MCP prompt `dream`, only when the user asks): `brain_dream`
  `queue`, work top-down, at most 10 changes, end with ONE `brain_dream`
  `log` call whose `items` list every queue item you looked at with its
  outcome (`done` / `skipped` + one-line reason / `deferred`). Items
  skipped 3× before: decide them; only an orphan or decay-candidate item
  may instead get `keep: true` on its page, if the user says it stays.

## Hard rules

- Singular `type:`; a plural is a hard error that blocks every auto-commit.
- Check with `brain_lookup` before creating a page; on a match, update the
  existing page and add your name for it to its `aliases`.
- Facts are never overwritten: set `superseded_by` + `valid_to` on the old
  page and write the new state.
- Before rewriting an existing page with `brain_write_page`, read it with
  `brain_get_pages` and `response_format: "detailed"` and carry over every
  frontmatter field unchanged (aliases, sources, tags, superseded_by,
  valid_from/valid_to, distinct_from, keep) — a concise read has no frontmatter.
  For a body section only, use `brain_patch_page`.
- Never delete a page other pages link to; ask before merging or deleting
  pages the user wrote.
- If `new_size_bytes` is much smaller than `previous_size_bytes` after an
  overwrite, stop and tell the user (restore with `brain_history`).
- No secrets in pages; do not edit `00_meta/` or `03_db/` unless asked.

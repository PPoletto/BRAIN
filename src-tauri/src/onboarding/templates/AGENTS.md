# AGENTS.md — BRAIN Vault Conventions for LLM Agents

> Read this file before you write a single page. It defines the page-types,
> frontmatter rules, wiki-link form, and ingest workflow that every LLM agent
> talking to BRAIN via MCP must follow. Violating these is not a style choice
> — it produces stale graphs, broken auto-commits, and drift that someone
> (probably the user) has to clean up later.

## Page Types — There Are Exactly Four

The vault recognises **four** singular page types. They are hardcoded in the
BRAIN client; introducing a new type requires a code change, not a per-page
decision.

| Type      | Directory             | What goes here |
|-----------|-----------------------|----------------|
| `entity`  | `02_wiki/entities/`   | A person, organisation, product, project, or named thing |
| `concept` | `02_wiki/concepts/`   | A methodology, idea, framework, or term of art |
| `source`  | `02_wiki/sources/`    | A single ingested artifact (email, page, transcript, doc) |
| `topic`   | `02_wiki/topics/`     | A synthesis of multiple sources around a theme |

**Common drift to avoid:** the directory names are plural (`entities/`,
`concepts/`…) but the frontmatter `type:` is **singular** (`entity`,
`concept`…). Writing `type: entities` produces a hard `unregistered-type`
**error** — `brain_write_page` returns failure, the file lands on disk
but does not get auto-committed, and **no subsequent auto-commit will
succeed** until the drift is fixed. The fix is always the same:
rewrite the page via `brain_write_page` with the singular form. The
error message lists the four valid singular forms; do not guess.

There is no `notes/` directory. Single-fact memos, preferences, and personal
context belong in the relevant existing page — most often a `## Notes`
section on a person's entity page — not in a fifth directory.

## Frontmatter

Every page starts with YAML frontmatter:

```yaml
---
id: entities/dan-shapiro
type: entity
title: Dan Shapiro
created: 2026-04-29
updated: 2026-05-11
tags: [strongdm, founder]
---
```

Rules:

- `id` and `type` are **required** and validated.
- `title` is optional but writing it is strongly preferred — its absence is
  flagged as a `missing-title` warning.
- `created` and `updated` are optional. Use ISO 8601 dates (`YYYY-MM-DD`)
  if you set them. If you can't tell when the page was created, omit the
  field rather than guess.
- `tags` is a YAML list, plural-keyed (`tags: [a, b, c]`). **Not** singular
  `tag:`. The `brain_query` tool's `tag:foo` operator queries the `tags`
  list — those are two different namespaces (query syntax vs. frontmatter
  key), don't conflate them.
- Don't invent additional fields unless asked. Extra fields parse fine but
  no tool reads them, so they're dead weight.

## Wiki Links — STRICT RULE

When linking from one page to another inside the vault, **always** use the
double-bracket wiki-link form:

```markdown
✅ See [[entities/dan-shapiro]] for context.
✅ Discussed by [[entities/dan-shapiro|Dan]] last week.

❌ See [Dan Shapiro](entities/dan-shapiro)
❌ See [Dan](http://tauri.localhost/entities/dan-shapiro)
❌ See [Dan](./entities/dan-shapiro.md)
```

The reasons matter — `[[…]]` is unambiguous (it can only be an internal
page), refactor-friendly (a single grep finds every reference), and feeds
the BRAIN graph view's edges. Standard markdown links to internal pages
produce no graph edges, may silently break in the viewer, and are flagged
as `non-canonical-wiki-link` warnings.

`brain_write_page` auto-rewrites standard markdown links to canonical form
before saving — but produce the canonical form yourself so it shows up
that way in the user's editor too.

Always link to the **fully-qualified ID** (`entities/dan-shapiro`, not just
`dan-shapiro`). Never link to a page you have not verified exists — broken
wiki links are a hard `broken-link` error and block the auto-commit. Use
`brain_page_exists` for cheap pre-write checks.

### One exception: aliased links inside Markdown tables

The aliased form `[[id|Display]]` uses `|` as alias separator, which
collides with the Markdown table cell separator. Inside a table row use
the un-aliased form:

```markdown
✅ | [[entities/dan-shapiro]] | CEO |
❌ | [[entities/dan-shapiro|Dan]] | CEO |
```

This is flagged as a `wikilink-pipe-in-table-cell` warning if you slip up.

## Tools — When to Use What

The BRAIN MCP server exposes a small, sharp toolset. Pick the right tool
for the job:

| Tool | Use when |
|---|---|
| `brain_ping` | Quick liveness check between batches; works even if the vault is disconnected |
| `brain_search` | Free-text / hybrid (lexical + semantic) search across pages |
| `brain_query` | Structured filter by fields (id, type, title, tag, created, updated) |
| `brain_get_page` | Read one page by id |
| `brain_get_pages` | Read N pages by id in one call — use for refactor sweeps and consistency audits |
| `brain_page_exists` | Cheap yes/no check before creating a new page (avoids accidental overwrite) |
| `brain_get_context` | One page + its 1-hop wiki-link neighbourhood |
| `brain_list_pages` | List ids per bucket (optional type/prefix filter, pagination) |
| `brain_list_tags` | Enumerate tags with their page counts — use this *before* `brain_query tag:foo` so you know which tags exist |
| `brain_graph` | Whole graph (nodes + edges) for analysis |
| `brain_lint_report` | Vault-wide lint state — call this at the start of a cleanup session, and at the end to confirm everything is clean |
| `brain_embedding_status` | Tell semantic-bge-m3 search apart from the deterministic hashed fallback (the fallback produces valid numbers but zero semantic meaning) |
| `brain_write_page` | Create or overwrite one page |
| `brain_write_batch` | **Atomic multi-page write — use this any time several new pages reference each other**, otherwise the single-page form cascades broken-link errors during the intermediate writes |
| `brain_write_raw_file` | Place a raw ingest artifact under `01_raw/<connector>/...` before turning it into a `source` page |
| `brain_get_page_history` | List the Git commits that touched one page — pair with the next tool to roll back a bad overwrite |
| `brain_restore_page` | Replace a page with the version at a given commit sha. Records a `revert: …` commit, never destructive |

### Bulk-Ingest Workflow

When a request requires writing several interlinked pages (the common case
on ingest):

1. **Plan first.** Decide which pages exist, which need to be created,
   and what the link structure looks like.
2. **Stubs before details.** If A→B→C is your target structure, create
   minimal stub pages for B and C first so A's links resolve, then enrich
   B and C. Or simpler:
3. **Use `brain_write_batch`** for the whole graph in one call. The batch
   tool validates all pages up front (atomic — if one fails parse, none
   are written), writes them together, then lints **once** at the end.
   Intra-batch references resolve so you don't cascade broken-link errors.
4. **Check the write response.** Each page write returns
   `{previous_size_bytes, new_size_bytes, warnings}`. If `new_size_bytes`
   is dramatically smaller than `previous_size_bytes` on an overwrite,
   you probably just clobbered a rich page with sparse content — stop
   and confirm with the user.

### Recovering from a Bad Overwrite

The `brain_write_page` response carries `previous_size_bytes` and
`new_size_bytes` so you can self-check the delta. If you (or another
agent in an earlier turn) accidentally shrunk a rich page into a thin
one, recover via:

1. `brain_get_page_history` for the page id — returns the recent
   commits that touched the page, newest first, each `{sha, ts, message}`.
2. Pick the sha *before* the bad overwrite (typically the second
   entry — the topmost is the bad write itself).
3. `brain_restore_page` with that sha. BRAIN replaces the file with
   the chosen revision and records a `revert: restored …` commit so
   the history stays append-only.

Confirm with the user before restoring if the change is non-trivial
— restoring drops everything that came after the chosen sha.

### Lint Output is Page-Scoped

`brain_write_page` and `brain_write_batch` return lint findings scoped
to the page(s) you just wrote. Findings from elsewhere in the vault stay
out of your write response — fetch them on demand via `brain_lint_report`.
You do not need to mentally filter "is this error from my current write
or from something earlier in the session" — the server already did it.

## Commit Behavior

BRAIN runs an auto-commit watcher that debounces file changes by 5 seconds
of idle. When you write pages via MCP:

- The file lands on disk immediately.
- The commit follows once the watcher has been idle for 5s, batched with
  any other changes from that window.
- If the watcher's lint pass finds **errors** (broken links, duplicate ids,
  malformed frontmatter) it **does not commit** until you fix them.
  Warnings do not block commits.
- All commits land on the wiki repository's default branch (usually `main`).
  There is no manual branching or merging; the wiki is conceptually
  trunk-based with the agent and user both committing into the same
  history.

## Replacing the Host's Built-in Memory

BRAIN is the user's **persistent memory layer**. When the user asks you to
"remember", "save", "note down" or "keep track of" something — facts,
preferences, ongoing context, decisions — persist it as a wiki page using
`brain_write_page` (or `brain_write_batch`) **instead of** the host
application's built-in memory (Claude Desktop's "Memory", ChatGPT's
"memory", etc.).

Choose the page type by content:

- **Person/Org/Product fact** → extend or create `entities/<slug>`. If
  the user has an existing entity page (`entities/pascal-poletto`,
  `entities/dextradata-grc-technologies` …), prefer **extending** it
  with a new `## Notes` or `## Kontext` section over creating a new
  page.
- **Methodology/Idea/Term of art** → `concepts/<slug>`.
- **Single artifact (email, doc, transcript)** → `sources/<slug>`. Often
  with a date-prefixed slug like `2026-05-11-subject-line`.
- **Synthesis spanning multiple sources/entities** → `topics/<slug>`.

Before writing, briefly confirm with the user:

> "I'll save this to your Brain as `entities/dan-shapiro` (extending the
> existing page) — proceed?"

When the user **asks** about something they previously told you, search the
Brain first with `brain_search` (or `brain_query` for structured filters)
and read matching pages with `brain_get_page` rather than relying on the
conversation context alone.

## Hard Rules

- Markdown only (no `.docx`, `.pdf`, `.html` inside `02_wiki/`).
- Wiki links must resolve (lint-enforced `broken-link` error blocks commits).
- Singular `type:` (`entity` / `concept` / `source` / `topic`) — never the
  plural directory name. Plural values are a hard `unregistered-type`
  **error**, not a warning: the auto-commit watcher refuses to commit
  any change while a drift page exists.
- Never put secrets (API keys, passwords, tokens) in frontmatter or body.
- Never edit `00_meta/` or `03_db/` files unless explicitly instructed.
- Prefer extending an existing entity over creating a new sibling.
- If you accidentally overwrite a richer page with thinner content,
  notice via the `previous_size_bytes` vs. `new_size_bytes` delta in the
  write response and tell the user.

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
summary: Founder of StrongDM; met at the 2026 kickoff, contact for the NIS-2 pilot.
created: 2026-04-29
updated: 2026-05-11
tags: [strongdm, founder]
---
```

Rules:

- `id` and `type` are **required** and validated.
- `title` is optional but writing it is strongly preferred — its absence is
  flagged as a `missing-title` warning.
- `summary` — **every new page SHOULD carry one**: one or two sentences
  saying what the page is about and why it matters. Search ranks summary
  hits above body hits and embeds every chunk of the page together with
  the summary, so a good summary makes the page findable. When you change
  a page's body substantially, update its summary in the same write. A
  page without one gets a (quiet) `missing-summary` warning.
- `created` and `updated` are optional. Use ISO 8601 dates (`YYYY-MM-DD`)
  if you set them. If you can't tell when the page was created, omit the
  field rather than guess.
- `tags` is a YAML list, plural-keyed (`tags: [a, b, c]`). **Not** singular
  `tag:`. The `brain_query` tool's `tag:foo` operator queries the `tags`
  list — those are two different namespaces (query syntax vs. frontmatter
  key), don't conflate them.
- `aliases` (optional) lists other names the thing goes by — spelling
  variants, abbreviations, former names: `aliases: [ACME Corp, Acme Inc]`.
  BRAIN uses them to spot duplicates (see "Before Creating a Page" below).
- `sources` (optional, expected on `entity` and `concept` pages) lists the
  `sources/…` pages the facts come from: `sources: [sources/kickoff-2026]`.
  An entity or concept page without `sources` is flagged as a
  `missing-sources` warning.
- `valid_from` / `valid_to` (optional, `YYYY-MM-DD`) say from when / until
  when the page's facts hold. `superseded_by: <id>` names the page that
  replaces this one. A date in another format is flagged as
  `invalid-date` and ignored by the validity filter.
- `distinct_from` (optional) lists ids of pages that share a name with
  this one but are a different thing — it silences the `alias-collision`
  warning for that pair.
- Don't invent additional fields unless asked. Extra fields parse fine but
  no tool reads them, so they're dead weight.

### Facts Are Never Overwritten — Supersede

When a fact changes (a new contract, a new role, a revised decision), do
**not** rewrite the old page so the old state disappears. Instead:

1. Write the new state as its own page (or update the page that already
   describes the current state).
2. On the old page set `superseded_by: <new id>` and `valid_to:` (the last
   day the old facts held). Leave its body as it was.

`brain_query` hides superseded, expired and not-yet-valid pages by
default (`valid:all` shows them, `valid:expired` lists only them).
`brain_get_page` and `brain_get_context` mark a superseded page with the
fields `superseded_by` and `notice` — follow it for current facts. The
notice is not part of the page: **never copy it into a page body.**
`superseded_by` must point at an existing page that is not itself
(directly or in a loop) superseded back — otherwise a
`dangling-supersede` / `supersede-cycle` **error** blocks the
auto-commit. When an expired page is still linked from current pages,
the lint reports `expired-but-linked` — point those links at the
successor. `brain_rename_page`, `brain_merge_pages` and
`brain_delete_page` keep `superseded_by` and `sources` consistent the
same way they keep links consistent.

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
| `brain_query` | Structured filter by fields (id, type, title, tag, created, updated). Hides superseded/expired pages unless you add `valid:all`; `sort:salience` lists the most-read pages first |
| `brain_get_page` | Read one page by id |
| `brain_get_pages` | Read N pages by id in one call — use for refactor sweeps and consistency audits |
| `brain_page_exists` | Check before creating a new page: says whether the id exists and lists pages that are probably the same thing (`matches`) |
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
| `brain_rename_page` | A page has the **wrong id** (typo, wrong slug, wrong type directory). Moves it and rewrites every link to it across the vault |
| `brain_merge_pages` | Two pages are **duplicates**. Appends the duplicate's body to the surviving page, redirects its links, removes the duplicate |
| `brain_delete_page` | A page is **junk** and should not exist. Refuses while other pages link to it; `force: true` deletes anyway and turns those links into plain text. Recoverable via `brain_restore_page` |
| `brain_dream_queue` | The prioritised consolidation list for a dream session (see "Dreaming") |
| `brain_dream_log` | Note in one line what a dream session changed |
| `brain_eval` | Measure search quality on the vault's test questions (Recall@10, MRR, nDCG@10 for full-text, vector and hybrid search) |
| `brain_eval_add` | Add a test question plus the page ids a good search must return — e.g. after the user complains that a search missed something |

### Before Creating a Page

Call `brain_page_exists` with the id you intend to create. Its `matches`
list pages of the same type that are probably the same thing:
`alias` (your slug is one of that page's `aliases`), `normalised` (same
slug after lowercasing, umlauts `ü`→`ue`, punctuation, `_` and spaces →
`-`; also `muller-gmbh` vs `mueller-gmbh`) or `similar` (a near spelling,
e.g. one letter apart). **If `page_exists` reports matches, use the
existing page** — update it and, if your name for the thing differs, add
that name to its `aliases`. If it says `matches_checked: false`, the
search index is not built yet; check `brain_list_pages` by hand.

`brain_write_page` and `brain_write_batch` refuse to create a page with
an `alias` or `normalised` match and name the existing page. Pass
`allow_duplicate: true` only when the two really are different things
(two people with the same name, say) — then give each a distinguishing
slug and title, and list the other id in `distinct_from`. Overwriting an
existing id is never refused. If two existing pages share a name through
an alias or the same slug, the lint reports `alias-collision`: merge them
(`brain_merge_pages`), fix the alias, or add `distinct_from`.

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

### Fixing a Wrong Page Name

Never fix a wrong id by writing a second page and leaving the first one
behind — every link keeps pointing at the old id. Instead:

1. **Wrong id, right content** → `brain_rename_page` with `id` and
   `new_id`. BRAIN moves the page, updates its frontmatter `id` (and
   `type` if the type directory changes) and rewrites `[[old]]` /
   `[[old|Alias]]` links in every page. If `new_id` already exists, the
   two pages are duplicates — go to step 2.
2. **Duplicate of an existing page** → `brain_merge_pages` with
   `from_id` (the duplicate) and `into_id` (the page to keep). Then read
   the surviving page and tidy the appended `## Merged from …` section
   with `brain_patch_page`.
3. **Junk that should not exist** → `brain_delete_page`. If it refuses
   because other pages link to it, decide whether those links should
   point somewhere else (rename or merge instead) before passing
   `force: true`.

Each of these records one commit (plus a `wiki: checkpoint before
refactor` commit first if there were uncommitted changes). Nothing is
lost: to bring back a deleted or merged page, call
`brain_get_page_history` with the **old** id and pass
`brain_restore_page` a sha from **before** the delete/merge commit — the
topmost entry is the removal itself. After a rename, the history under
the new id starts at the rename; older revisions are listed under the old
id. Confirm with the user before deleting or merging pages they wrote
themselves.

Page ids for these tools must look like `entities/dan-shapiro`: a type
directory, then letters, digits, `.`, `_` or `-` (no spaces or
parentheses — they break markdown links). If a tool reports
`commit: null` with a `note`, the change is already on disk; do not
repeat it.

### Lint Output is Page-Scoped

`brain_write_page` and `brain_write_batch` return lint findings scoped
to the page(s) you just wrote. Findings from elsewhere in the vault stay
out of your write response — fetch them on demand via `brain_lint_report`.
You do not need to mentally filter "is this error from my current write
or from something earlier in the session" — the server already did it.

### Scheduled Audit

Once a day BRAIN audits the whole wiki and writes the result to
`00_meta/audit/<YYYY-MM-DD>.md` (one file per day; read-only for you).
At the **start of a maintenance session** ("clean up the wiki", "tidy
my notes"), read the newest file in `00_meta/audit/` — or, if you cannot
read vault files directly, call `brain_lint_report`, which returns the
same findings live — and work through it:

- `duplicate-candidate` — two pages of the same type that read almost
  the same (similarity score in the message). Open both; if they
  describe the same thing, `brain_merge_pages` the weaker into the
  stronger. If they are genuinely different, leave them.
- `orphan` — no other page links here and it has not changed for 90+
  days. Link it from a related page, merge it into one, or — if it is
  junk — `brain_delete_page` it.
- `broken-link` — a link to a page that does not exist. Fix the link,
  create the missing page, or `brain_rename_page` the page that was
  meant.
- `alias-collision` — two pages share a name via id or `aliases`. Merge
  them if they are the same thing; otherwise remove the clashing alias.
- `missing-sources` — an entity or concept page names no `sources`. Add
  the source pages its facts come from.
- `missing-summary` — the page has no `summary`. Write one or two
  sentences (start with the most-linked pages).
- `broken-source` — a `sources` entry has no page. Fix the id or create
  the source page.
- `invalid-date` — `valid_from` / `valid_to` is not `YYYY-MM-DD`, or
  `valid_from` lies after `valid_to`.
- `expired-but-linked` — the page's `valid_to` has passed but current
  pages still link to it. Point those links at the successor.

Confirm with the user before merging or deleting pages they wrote
themselves. The next day's audit shows what is left.

## Dreaming

Like a brain that sorts its thoughts in sleep, BRAIN consolidates the
wiki in two halves. BRAIN itself does the mechanical half: with the daily
audit (and whenever you ask) it writes a prioritised work list, the
**dream queue** (`00_meta/dream-queue.md`). You do the thinking half — but
**only when the user triggers it** ("träum mal", "dream", "tidy up the
wiki while I'm away"); never start a dream session on your own.

A dream session:

1. Call `brain_dream_queue` (it recomputes the queue if it is older than
   an hour; `refresh: true` forces it). Each item has a `priority` (1 is
   most urgent), a `kind`, the `pages`, a `reason` and a
   `suggested_action`.
2. Work **top-down**, at most **10 changes per session**:
   - `fix-link` — repair the broken link or `sources` entry (right id,
     create the missing page, or `brain_rename_page` the page that was
     meant).
   - `merge` — read both pages; if they describe the same thing,
     `brain_merge_pages` the weaker into the stronger. If not, leave them
     (and add `distinct_from` if they share a name).
   - `update-summary` / `write-summary` — read the page and write a
     fitting one-to-two-sentence `summary` (`brain_patch_page` cannot edit
     frontmatter — rewrite the page with `brain_write_page`, body
     unchanged). If a stale summary is still accurate, confirm it instead:
     pass `confirm_summary: true` to `brain_write_page` or
     `brain_patch_page` (with the page content unchanged) and the item
     goes away.
   - `archive-or-supersede` / `review-or-archive` — a page nobody reads or
     links to. Link it from a related page if it is still useful; if its
     facts were replaced, set `superseded_by` and `valid_to`. Do not
     delete it.
3. Hard rules: **never delete a page other pages link to**; **supersede
   instead of overwriting** facts; keep minority views and open questions
   instead of flattening them into one "truth"; ask the user before
   changing pages they clearly wrote themselves.
4. Finish with one `brain_dream_log` entry saying what you changed and
   why ("merged entities/acme-inc into entities/acme; summaries for 3
   hubs"). Every change stays recoverable with `brain_restore_page`.

The queue lists each page at most once; what you leave undone (or what
your changes uncover) shows up in the next queue.

## Search Quality

`00_meta/eval-queries.yaml` holds test questions with the pages a good
search must return (synced between the user's machines). When the user
says a search missed something, add the question with `brain_eval_add`
(only existing page ids). `brain_eval` measures how well full-text,
vector and hybrid search find the expected pages and appends the numbers
to `00_meta/eval-history.md`. Add questions through `brain_eval_add`, not
by editing the files, and never mix them with any external test suite.

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
- Prefer extending an existing entity over creating a new sibling. If
  `brain_page_exists` reports matches, use the existing page.
- Facts are never overwritten — supersede the old page (`superseded_by`)
  and set its `valid_to`.
- If you accidentally overwrite a richer page with thinner content,
  notice via the `previous_size_bytes` vs. `new_size_bytes` delta in the
  write response and tell the user.

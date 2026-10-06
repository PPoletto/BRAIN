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
rewrite the page via `brain_write_page` with the singular form (read it
first with `response_format: "detailed"` — see "Rewriting a Page"). The
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
  warning for that pair, and the two pages are never reported (or queued)
  as possible duplicates.
- `keep: true` (optional) says the user decided the page stays even
  though nothing links to it or nobody reads it. It silences the
  `orphan` warning and the dream queue's decay / orphan items for that
  page — nothing else (broken links, duplicates, summaries and errors are
  still reported). Set it only when the user says so.
- Don't invent additional fields unless asked. Extra fields parse fine but
  no tool reads them, so they're dead weight.

### Facts Are Never Overwritten — Supersede

When a fact changes (a new contract, a new role, a revised decision), do
**not** rewrite the old page so the old state disappears. Instead:

1. Write the new state as its own page (or update the page that already
   describes the current state).
2. On the old page set `superseded_by: <new id>` and `valid_to:` (the last
   day the old facts held). Leave its body as it was (read it with
   `response_format: "detailed"` first — see "Rewriting a Page").

`brain_query` hides superseded, expired and not-yet-valid pages by
default (`valid:all` shows them, `valid:expired` lists only them).
`brain_get_pages` marks a superseded page with the
fields `superseded_by` and `notice` — follow it for current facts. The
notice is not part of the page: **never copy it into a page body.**
`superseded_by` must point at an existing page that is not itself
(directly or in a loop) superseded back — otherwise a
`dangling-supersede` / `supersede-cycle` **error** blocks the
auto-commit. When an expired page is still linked from current pages,
the lint reports `expired-but-linked` — point those links at the
successor. `brain_refactor` (rename, merge, delete) keeps
`superseded_by` and `sources` consistent the same way it keeps links
consistent.

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
`brain_lookup` for cheap pre-write checks.

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
| `brain_ping` | Quick liveness check between batches; works even if the vault is disconnected. `detail: true` also says whether search is semantic (bge-m3) or runs on the hashed fallback, and how big the index is |
| `brain_search` | Free-text / hybrid (lexical + semantic) search when you do not know the page ids |
| `brain_lookup` | **Before creating a page**: pass the planned id (`entities/acme`) or just a name (`ACME Corp`); says whether it exists and lists pages that are probably the same thing (`matches`). No page bodies |
| `brain_get_pages` | Read one or more pages by id (`ids: ["entities/alice"]` for one) — also for refactor sweeps and consistency audits. `include_context: true` adds each page's 1-hop neighbourhood (`outbound` links, `backlinks`) |
| `brain_query` | List or filter by fields (id, type, title, tag, created, updated). `*` lists every current page; `prefix`, `limit`, `offset` page through them. Hides superseded/expired pages unless you add `valid:all`; `sort:salience` lists the most-read pages first. `facet: "tags"` returns the tags with their page counts — use it *before* filtering with `tag:` so you know which tags exist |
| `brain_graph` | Whole graph (nodes + edges) for structure analysis |
| `brain_write_page` | Create or overwrite one page (including its frontmatter) |
| `brain_write_batch` | **Atomic multi-page write — use this any time several new pages reference each other**, otherwise the single-page form cascades broken-link errors during the intermediate writes |
| `brain_patch_page` | Replace one section of an existing page (frontmatter untouched) |
| `brain_refactor` | Fix the structure: `action: "rename"` — a page has the **wrong id** (typo, wrong slug, wrong type directory); moves it and rewrites every link to it. `action: "merge"` — two pages are **duplicates**; appends the duplicate's body to the surviving page, redirects its links, removes the duplicate. `action: "delete"` — a page is **junk**; refuses while other pages link to it, `force: true` deletes anyway and turns those links into plain text. All recoverable via `brain_history` |
| `brain_lint_report` | Vault-wide lint state — call this at the start of a cleanup session, and at the end to confirm everything is clean |
| `brain_history` | `action: "list"` (default) lists the Git commits that touched one page; `action: "restore"` replaces the page with its version at a commit sha and records a `revert: …` commit — never destructive. Use it to roll back a bad overwrite |
| `brain_write_raw_file` | Place a raw ingest artifact under `01_raw/<connector>/...` before turning it into a `source` page |
| `brain_eval` | Search quality: `action: "run"` measures Recall@10, MRR and nDCG@10 for full-text, vector and hybrid search; `action: "add"` stores a test question plus the page ids a good search must return |
| `brain_dream` | Consolidation (see "Dreaming"): `action: "queue"` returns the prioritised work list, `action: "log"` records what a dream session changed |

**Concise by default.** `brain_search`, `brain_get_pages`,
`brain_query`, `brain_graph` and `brain_lint_report` take
`response_format`: `"concise"` (default — ids, titles, summaries,
counts) or `"detailed"` (every field). Ask for `"detailed"` only when you
need it — e.g. `brain_get_pages` detailed returns the full frontmatter,
which you need before rewriting a page with `brain_write_page`.

### Rewriting a Page

`brain_write_page` replaces the whole file, frontmatter included. Before
rewriting an existing page, **always** read it with `brain_get_pages` and
`response_format: "detailed"` and carry over **every** frontmatter field
unchanged except the one you mean to change — `aliases`, `sources`,
`tags`, `superseded_by`, `valid_from` / `valid_to`, `distinct_from`,
`keep`, `created`, everything. A concise read has no frontmatter; rewriting from
it silently drops those fields, and a dropped `superseded_by` makes a
replaced page current again. To change only a body section, use
`brain_patch_page` instead — it never touches the frontmatter.

**Renamed tools.** Older instructions may name `brain_get_page`,
`brain_get_context`, `brain_page_exists`, `brain_list_pages`,
`brain_list_tags`, `brain_embedding_status`, `brain_rename_page`,
`brain_merge_pages`, `brain_delete_page`, `brain_get_page_history`,
`brain_restore_page`, `brain_eval_add`, `brain_dream_queue` or
`brain_dream_log`. They no longer exist; calling one returns an error that
names its replacement (see the table above).

### Prompts and Resources

The server also offers ready-made **prompts** — pick them from your
client's prompt menu or ask for them by name:

- `ingest` — raw file → source page → entity/concept pages, written in one
  `brain_write_batch` (argument `source`: what to ingest).
- `lint-session` — work through the newest audit / lint report kind by
  kind (optional argument `focus`: one finding kind).
- `dream` — the dream protocol below (optional argument `max_changes`,
  default 10).

And **resources** you can read without a tool call:
`brain://agents-md` (this file), `brain://audit/latest` (the newest daily
audit) and `brain://dream-queue` (the current dream queue as JSON).

### Before Creating a Page

Call `brain_lookup` with the id you intend to create (`entities/acme`)
— or, if you do not know the type yet, just the name (`ACME Corp`; then
all four types are checked and a page that already uses exactly that slug
is reported as `exact`). Its `matches` list pages of the same type that
are probably the same thing: `alias` (your slug is one of that page's
`aliases`), `normalised` (same slug after lowercasing, umlauts `ü`→`ue`,
punctuation, `_` and spaces → `-`; also `muller-gmbh` vs `mueller-gmbh`)
or `similar` (a near spelling, e.g. one letter apart). **If `brain_lookup`
reports matches, use the existing page** — update it and, if your name for
the thing differs, add that name to its `aliases`. If it says
`matches_checked: false`, the search index is not built yet; check by hand
with `brain_query` (`query: "valid:all"`, `prefix: "entities/acme"` — `valid:all`
so superseded and expired pages count as possible duplicates too).

`brain_write_page` and `brain_write_batch` refuse to create a page with
an `alias` or `normalised` match and name the existing page. Pass
`allow_duplicate: true` only when the two really are different things
(two people with the same name, say) — then give each a distinguishing
slug and title, and list the other id in `distinct_from`. Overwriting an
existing id is never refused. If two existing pages share a name through
an alias or the same slug, the lint reports `alias-collision`: merge them
(`brain_refactor` with `action: "merge"`), fix the alias, or add
`distinct_from` (adding a field is a rewrite — see "Rewriting a Page").

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

1. `brain_history` (`action: "list"`) for the page id — returns the
   recent commits that touched the page, newest first, each
   `{sha, ts, message}`.
2. Pick the sha *before* the bad overwrite (typically the second
   entry — the topmost is the bad write itself).
3. `brain_history` with `action: "restore"` and that sha. BRAIN replaces the file with
   the chosen revision and records a `revert: restored …` commit so
   the history stays append-only.

Confirm with the user before restoring if the change is non-trivial
— restoring drops everything that came after the chosen sha.

### Fixing a Wrong Page Name

Never fix a wrong id by writing a second page and leaving the first one
behind — every link keeps pointing at the old id. Instead:

1. **Wrong id, right content** → `brain_refactor` with
   `action: "rename"`, `id` and `new_id`. BRAIN moves the page, updates its frontmatter `id` (and
   `type` if the type directory changes) and rewrites `[[old]]` /
   `[[old|Alias]]` links in every page. If `new_id` already exists, the
   two pages are duplicates — go to step 2.
2. **Duplicate of an existing page** → `brain_refactor` with
   `action: "merge"`, `from_id` (the duplicate) and `into_id` (the page to keep). Then read
   the surviving page and tidy the appended `## Merged from …` section
   with `brain_patch_page`.
3. **Junk that should not exist** → `brain_refactor` with
   `action: "delete"`. If it refuses
   because other pages link to it, decide whether those links should
   point somewhere else (rename or merge instead) before passing
   `force: true`.

Each of these records one commit (plus a `wiki: checkpoint before
refactor` commit first if there were uncommitted changes). Nothing is
lost: to bring back a deleted or merged page, call `brain_history`
(`action: "list"`) with the **old** id and restore (`action: "restore"`)
a sha from **before** the delete/merge commit — the
topmost entry is the removal itself. After a rename, the history under
the new id starts at the rename; older revisions are listed under the old
id. Confirm with the user before deleting or merging pages they wrote
themselves.

Page ids for these tools must look like `entities/dan-shapiro`: a type
directory, then letters, digits, `.`, `_` or `-` (no spaces or
parentheses — they break markdown links). If a tool reports
no `commit` but a `note`, the change is already on disk; do not
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
my notes"), read the newest file in `00_meta/audit/` (MCP resource
`brain://audit/latest`) — or call `brain_lint_report`, which returns the
same findings live (concise: counts per kind; then `response_format:
"detailed"` with `kind` for one kind's findings) — and work through it
(the `lint-session` prompt walks you through it):

- `duplicate-candidate` — two pages of the same type that read almost
  the same (similarity score in the message). Open both; if they
  describe the same thing, merge the weaker into the stronger
  (`brain_refactor`, `action: "merge"`). If they are genuinely different, leave them.
- `orphan` — no other page links here and it has not changed for 90+
  days. Link it from a related page, merge it into one, or — if it is
  junk — delete it (`brain_refactor`, `action: "delete"`).
- `broken-link` — a link to a page that does not exist. Fix the link,
  create the missing page, or rename the page that was meant
  (`brain_refactor`, `action: "rename"`).
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
wiki while I'm away", or the `dream` prompt). BRAIN never schedules a
dream session and you never start one on your own; a user who wants it
regularly can schedule it in their own client.

A dream session (the `dream` prompt contains the same protocol):

1. Call `brain_dream` with `action: "queue"` (it recomputes the queue if
   it is older than an hour; `refresh: true` forces it). Each item has a
   `priority` (1 is most urgent), a `kind`, the `pages`, a `reason` and a
   `suggested_action`.
2. Work **top-down**, at most **10 changes per session**:
   - `fix-link` — repair the broken link or `sources` entry (right id,
     create the missing page, or rename the page that was meant with
     `brain_refactor`).
   - `merge` — read both pages (`brain_get_pages`); if they describe the
     same thing, merge the weaker into the stronger (`brain_refactor`,
     `action: "merge"`) and tidy
     the appended section with `brain_patch_page`. If not, leave them (and
     add `distinct_from` if they share a name — detailed read first).
   - `update-summary` / `write-summary` — read the page with
     `brain_get_pages` and `response_format: "detailed"`, then write a
     fitting one-to-two-sentence `summary` (`brain_patch_page` cannot edit
     frontmatter — rewrite the page with `brain_write_page`, body and
     every other frontmatter field unchanged; see "Rewriting a Page"). If
     a stale summary is still accurate, confirm it instead: write the page
     exactly as read (detailed) with `brain_write_page` and
     `confirm_summary: true`, and the item goes away.
   - `archive-or-supersede` / `review-or-archive` — a page nobody reads or
     links to. Link it from a related page if it is still useful; if its
     facts were replaced, set `superseded_by` and `valid_to` (detailed
     read first, every other field kept). Do not delete it.
3. Hard rules: **at most 10 changes**; **never delete a page other pages
   link to**; **supersede instead of overwriting** facts; keep minority
   views and open questions instead of flattening them into one "truth";
   ask the user before changing pages they clearly wrote themselves.
4. Finish with **one** `brain_dream` `action: "log"` call: an `entry`
   saying what you changed and why ("merged entities/acme-inc into
   entities/acme; summaries for 3 hubs"), and `items` — **every** queue
   item you looked at, each `{kind, pages, outcome, note}` with
   `outcome` `done`, `skipped` (the `note` gives a one-line reason) or
   `deferred`. Every change stays recoverable with `brain_history`
   (`action: "restore"`).

Skipped and deferred items come back in later queues with
`skipped_before: n` (skips are counted until the item is logged `done`);
from 3 on, the reason says "skipped 3× before — decide it" ("— decide
or mark keep" for orphan and decay-candidate items) and a priority-3
item moves to the end. Decide such
an item instead of skipping it again — and if the user says a page
stays, set `keep: true` on it with a detailed read + rewrite (see
"Rewriting a Page"); its decay / orphan items then stop.

The queue lists each page at most once; what you leave undone (or what
your changes uncover) shows up in the next queue.

## Search Quality

`00_meta/eval-queries.yaml` holds test questions with the pages a good
search must return (synced between the user's machines). When the user
says a search missed something, add the question with `brain_eval`
`action: "add"` (only existing page ids). `brain_eval` `action: "run"`
measures how well full-text, vector and hybrid search find the expected
pages and appends the numbers to `00_meta/eval-history.md`. Add questions
through `brain_eval`, not by editing the files, and never mix them with
any external test suite.

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
and read matching pages with `brain_get_pages` rather than relying on the
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
  `brain_lookup` reports matches, use the existing page.
- Facts are never overwritten — supersede the old page (`superseded_by`)
  and set its `valid_to`.
- If you accidentally overwrite a richer page with thinner content,
  notice via the `previous_size_bytes` vs. `new_size_bytes` delta in the
  write response and tell the user.

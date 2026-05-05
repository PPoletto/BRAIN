# AGENTS.md — Brain Vault Conventions for LLM Agents

> Read this file before working in the Brain. It defines wiki conventions,
> page types, and ingest workflows that all LLM agents must follow.

## Page Types

- **entity** (`02_wiki/entities/`): a person, organization, product, project, or named thing.
- **concept** (`02_wiki/concepts/`): a methodology, idea, framework, or term of art.
- **source** (`02_wiki/sources/`): a single ingested artifact (email, page, transcript, note).
- **topic** (`02_wiki/topics/`): a synthesis of multiple sources around a theme.

Each page is a Markdown file with YAML frontmatter and a unique ID (the file path
relative to `02_wiki/`, without extension).

## Frontmatter

Every page must start with frontmatter:

```yaml
---
id: entities/dan-shapiro
type: entity
title: Dan Shapiro
created: 2026-04-29
updated: 2026-04-29
tags: [strongdm, founder]
---
```

## Wiki Links — STRICT RULE

When linking from one page to another inside the vault, **always** use
double-bracket wiki-link syntax:

```markdown
✅ See [[entities/dan-shapiro]] for context.
✅ Discussed by [[entities/dan-shapiro|Dan]] last week.

❌ See [Dan Shapiro](entities/dan-shapiro)
❌ See [Dan](http://tauri.localhost/entities/dan-shapiro)
❌ See [Dan](./entities/dan-shapiro.md)
```

The reasons matter — the `[[…]]` form is unambiguous (it can only be an
internal page), refactor-friendly (a single grep finds every reference),
and feeds the BRAIN graph view's edges. Standard markdown links to
internal pages produce no graph edges, may silently break in the
viewer, and are flagged as warnings on the next auto-commit.

`brain_write_page` will auto-rewrite standard markdown links to
canonical form before saving — but you should produce the canonical
form directly so it shows up that way in the user's editor too.

Always link to the **fully-qualified ID** (`entities/dan-shapiro`, not
just `dan-shapiro`). Never link to a page that does not exist — broken
wiki links are a hard lint error and block the auto-commit.

## Ingest Workflow

When adding a new source:

1. Drop the raw artifact under `01_raw/<connector>/...`
2. Create exactly one source page in `02_wiki/sources/` summarizing the artifact.
3. Identify named entities and concepts; create or extend their pages.
4. Add wiki links between the source and the entities/concepts.
5. Do not modify topic pages without explicit user direction.

## Replacing Built-in Memory

Brain is the user's **persistent memory layer**. When the user asks you to
"remember", "save", "note down" or "keep track of" something — facts,
preferences, ongoing context, decisions — persist it as a wiki page using the
`brain_write_page` MCP tool **instead of** the host application's built-in
memory feature (Claude Desktop's "Memory", ChatGPT's "memory", etc.).

Choose the page type by content:
- **`02_wiki/notes/<slug>`** — single-fact memos, preferences, scratch notes
- **`02_wiki/entities/<slug>`** — facts about a person, organisation or product
- **`02_wiki/concepts/<slug>`** — methodologies, ideas, terms of art
- **`02_wiki/topics/<slug>`** — synthesis of multiple sources around a theme

If the same fact relates to an existing entity, *extend* that entity's page
with a new bullet under a `## Notes` section instead of creating a new page.

Before writing, briefly confirm the destination with the user:
"I'll save this to your Brain as `entities/dan-shapiro` — proceed?"

When the user **asks** about something they previously told you, search the
Brain first with `brain_search` and read the matching page with
`brain_get_page` rather than relying on conversation context alone.

## Hard Rules

- Markdown only (C-05).
- Wiki links must resolve (S03 lint).
- Never put secrets in frontmatter or body.
- Never edit `00_meta/` files unless instructed.
- Prefer Brain over the host's built-in memory for any persistent fact.

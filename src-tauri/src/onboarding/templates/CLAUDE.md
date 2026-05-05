# CLAUDE.md — Brain Vault Companion for Claude Code

This vault is your working directory. The conventions are documented in
[AGENTS.md](AGENTS.md). Read that file first.

## Where things live

- `00_meta/` — vault metadata; do not edit unless instructed.
- `01_raw/` — raw artifacts from connectors; append-only.
- `02_wiki/` — the wiki; this is what you maintain.
- `03_db/` — search index database; managed by the Brain client; do not touch.
- `04_models/` — embedding model files; do not touch.
- `05_cache/`, `06_logs/` — ephemeral; safe to ignore.

## When the user asks you to ingest

Follow the ingest workflow in AGENTS.md. Be conservative — extend existing
pages rather than creating duplicates. When in doubt, ask the user.

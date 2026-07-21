# S11 — Vault Sync (Git Remote + Content Encryption)

> **Status:** Proposed. Extension beyond the canonical read-only S01–S10 set.
> Not part of `requirements/spec/`. Requires DSB sign-off before any
> third-party PII is pushed to a cloud remote (see §9).

## 1. Context

The vault today lives on a single USB stick, unencrypted. If the stick is
forgotten, the user has no access to their knowledge; if it is lost, the
full contents — including third-party personal data (colleagues, customers,
phone numbers, contract context from DextraData work) — are exposed in
plaintext.

Goal: make the vault **always available** across the user's machines and
**unreadable to anyone who obtains a copy** (GitHub breach, lost stick),
while preserving the property that made Git attractive here — clean
per-page, per-line 3-way merges.

`02_wiki/` is already a local Git repository (auto-committed by the S03
watcher). This spec adds a **remote** and a **content-encryption +
opaque-filename** layer so the repo can be pushed to a private GitHub repo
(the user's chosen primary remote; a filesystem/stick path works via the
same mechanism with no credentials).

## 2. Non-goals

- Not a multi-user/shared BRAIN (that remains a separate product).
- Not a public/hardened internet service (no inbound server; sync is
  outbound Git push/pull only).
- Not local-at-rest encryption of the working tree — that is the user's
  responsibility via OS full-disk encryption (see §3, Layer C).
- Not mobile access.

## 3. The three encryption layers (scope boundary)

| Layer | Protects against | In scope? |
|---|---|---|
| **A. In-Git content encryption (git-crypt)** | leak of any pushed/copied repo (GitHub, lost stick) | **YES — primary** |
| **B. Stick volume encryption (BitLocker/VeraCrypt)** | physical theft of the stick at rest | No — user's option; redundant once A exists; does not travel to a folder/cloud |
| **C. OS full-disk encryption (laptop)** | local disk / memory access on the user's own machine | No — user responsibility; git-crypt keeps the **working tree plaintext on the local disk** by design (that is how BRAIN reads/indexes it), so local-at-rest protection is FDE's job |

BRAIN implements Layer A only. The spec must state Layers B and C
explicitly so there is no illusion that git-crypt encrypts local-at-rest.

## 4. Storage-model change: decouple filename from id

Today `path == id` (`entities/alice.md` ↔ id `entities/alice`). git-crypt
encrypts *content* but leaves *filenames* in plaintext — and here filenames
are PII (person/customer names). So filenames must become opaque.

- **Filename:** `<type>/<HMAC-SHA256(id, vault_key)>.md`
  (e.g. `entities/7f3a9c…b2.md`). The **type directory stays plaintext**
  (a category is not PII); the basename is an HMAC.
- **Why HMAC, not a plain hash:** a person's name is low-entropy — a plain
  `SHA256(name)` is brute-forceable by a dictionary attack against the
  repo. A keyed HMAC is deterministic (same page → same filename on every
  machine, so Git collisions still surface duplicates as conflicts) but not
  reversible without the key.
- **Why not random GUIDs:** non-deterministic → the same logical page
  created independently on two machines would get two different filenames →
  silent duplicate after merge instead of a Git conflict.
- **The real id stays in the (encrypted) frontmatter** (`id:` field). No
  separate map file in the repo — a map file would be the single hottest
  merge-conflict target. Instead the local SQLite index rebuilds the
  `id ↔ HMAC` map by reading decrypted frontmatter on index. Forward
  `id → filename` is the pure HMAC function; reverse `filename → id` comes
  from the index.
- **BRAIN already keys internally on `frontmatter.id`, not the filename** —
  the graph, indexer and wiki-link resolution use `id`. The change is
  therefore mostly at the filesystem-access boundary, but that boundary is
  pervasive: `brain_write_page`, `brain_page_exists`, `restore_page`,
  `history`, `lint`, the indexer and `tree` all compute `id → path`
  directly today and must route through the map.

### Residual metadata leak (accepted by the user, documented for the DSB)

Even with A + opaque filenames, a party with repo access still sees: the
**category** per file, the **page count** per type, **ciphertext sizes**
(≈ plaintext length), and the **commit graph** (timestamps, edit
frequency). No identities, no content. The user has accepted this trade-off;
it must be recorded in the DSB submission (§9).

## 5. Sync flow

New capability on the existing `git2` base (S03 has local commit/history
only — no remote/fetch/merge/push today).

- **Remote config (Settings → Sync):** register one or more remotes. A
  remote is either a filesystem path (stick — no credentials) or a Git URL
  (GitHub — PAT stored in the OS keychain via the `keyring` crate, never in
  the repo or `00_meta/`).
- **"Sync now"** action: `fetch` → `merge` (git2 `merge_analysis` →
  fast-forward or a real merge commit) → `push`. On-demand, not continuous.
- **After a successful merge: auto-trigger a reindex** (the existing
  "Rebuild index" path). `03_db/` and `04_models/` never travel — only the
  Git repo does; the index is derived and rebuilt locally.
- **Conflicts:** on a merge conflict, surface the affected pages in the
  History UI and stop; the user resolves (editor or a theirs/ours quick
  pick) and re-runs Sync. MVP may simply report "N pages in conflict, resolve
  and re-sync."
- **Only `02_wiki/` is the synced repo.** `00_meta/` (with the MCP bearer
  token) is outside it — verify in the spec that no secret is ever staged.

## 6. Key management

- One **vault master secret** generated on first encryption setup, stored in
  the OS keychain. The git-crypt symmetric key and the HMAC key are **derived
  from it** (e.g. HKDF with distinct info labels) so the user manages one
  secret, not two.
- **New-machine bootstrap:** clone the repo → BRAIN prompts for the master
  secret → git-crypt unlock → reindex. The user transports the secret
  out-of-band (password manager) — it is never committed and never sent to
  the remote.
- Losing the secret = the remote copy is unrecoverable ciphertext. State this
  plainly in the setup UI; recommend the user store it in their password
  manager at setup time.

## 7. Encryption tooling

- **git-crypt** (clean/smudge filter): plaintext in the working tree,
  ciphertext in the Git objects that get pushed. Local 3-way merges operate
  on the decrypted working tree, so the merge advantage is preserved.
  Simplest fit for a single-user symmetric key.
- Alternative considered: `age`-based filters (`transcrypt`,
  `git-agecrypt`) — more modern, per-recipient keys; unnecessary complexity
  for single-user. Default to git-crypt; revisit only if multi-recipient is
  ever needed.
- Integration options to decide at build time: shell out to the `git-crypt`
  binary (must be installed/bundled) vs. a pure-Rust reimplementation of the
  clean/smudge filter over `age`/`aes-gcm`. Leaning pure-Rust to avoid an
  external binary dependency on all three platforms — to be settled in the
  implementation plan.

## 8. Obsidian convert command

A one-shot command to convert the vault between **opaque** (HMAC filenames,
encrypted) and **plaintext** (`<id>.md`, cleartext) layout — for exporting to
Obsidian or another Markdown tool.

- opaque → plaintext: for each file, decrypt, read `id` from frontmatter,
  rename to `<id>.md`, optionally drop the encryption filter.
- plaintext → opaque: the reverse.
- Cheap precisely because `id` always lives in the frontmatter. Runs on a
  working copy the user then points Obsidian at; does not disturb the synced
  repo.

## 9. Data-protection gate (blocking)

Pushing third-party personal data to GitHub (US cloud, third-country
transfer) is a DSGVO-relevant processing decision even when content is
E2E-encrypted, because of the residual metadata leak (§4) and the transfer
itself. This spec must not be implemented for the GitHub remote until the
**Datenschutzbeauftragte** has signed off. The stick/filesystem remote (data
stays on the user's own hardware) does not carry this gate and can proceed
independently. This is a process gate, not legal advice.

## 10. Test strategy

- Unit: HMAC filename determinism (same id → same name; different keys →
  different names); `id ↔ filename` map rebuild from frontmatter; opaque↔
  plaintext convert round-trips losslessly.
- Merge: two clones edit different pages → clean merge; same page different
  lines → clean merge; same page same line → surfaced conflict; same new
  page on both → deterministic-filename collision surfaces as a conflict
  (not a silent duplicate).
- Encryption: pushed Git objects contain no plaintext id or body; working
  tree is plaintext after checkout; a clone without the key cannot decrypt.
- Sync: fetch/merge/push against a local bare repo (stdin of a stick);
  reindex fires after merge; no secret is ever staged from `00_meta/`.
- Real GitHub push/pull is a manual QA step (needs a live private repo +
  PAT), not a unit test.

## 11. Phased build

1. **Storage decoupling** (filename↔id via map, id-in-frontmatter
   canonical) — the core, highest-risk change; ship behind a flag with the
   plaintext layout still the default.
2. **git-crypt content encryption** + key management (keychain, master→
   derive, bootstrap).
3. **HMAC opaque filenames** (depends on 1).
4. **Sync feature** (remote config, fetch/merge/push, reindex, conflict
   surfacing).
5. **Obsidian convert command.**
6. Docs + the DSB submission package (what leaks, what does not).

Estimated: weeks, not days — this is a storage-layer change, not a single
feature. Recommend implementing 1 in isolation with heavy tests before
layering 2–3 on top.

//! Minimal MCP-compatible JSON-RPC server over stdio.
//!
//! Speaks the subset of MCP that Claude Code, Claude Desktop, Codex and
//! Continue.dev actually invoke during a session: `initialize`,
//! `tools/list`, `tools/call`. Each line on stdin is one JSON-RPC
//! envelope; responses are one line per request on stdout.
//!
//! The server runs in the `brain mcp` subprocess. The vault path is
//! provided via the `BRAIN_VAULT_PATH` environment variable so a single
//! installed `brain` binary can serve multiple vaults across hosts.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::vault::layout::{is_vault, raw_dir, wiki_dir};
use crate::viewer::{graph, search, tree};
use crate::wiki::{git as wiki_git, lint, page};

const PROTOCOL_VERSION: &str = "2024-11-05";
// `serverInfo.name` shown in MCP `initialize` handshake responses. We use
// the uppercase brand to match `BRAIN_SERVER_KEY` and the rest of the UI.
const SERVER_NAME: &str = "BRAIN";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse<T: Serialize> {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Entrypoint for `brain mcp`. Reads env, then runs the dispatch loop.
pub fn run_stdio() -> std::io::Result<()> {
    let vault_path = std::env::var("BRAIN_VAULT_PATH")
        .map(PathBuf::from)
        .ok()
        .filter(|p| is_vault(p));

    // Open the SQLite index — same DB the GUI uses, opened in WAL mode so
    // both processes can read concurrently. Without this handle the search
    // tool falls back to a substring walk over the markdown files, which
    // misses tokenised matches (e.g. query "spec driven development" did
    // not hit the page id "spec-driven-development"). If the index is
    // empty (GUI hasn't run a rebuild yet, or a fresh vault), we kick off
    // a one-shot rebuild here so the LLM never sees zero hits on a vault
    // that demonstrably has matching pages on disk.
    let db = vault_path
        .as_ref()
        .and_then(|v| match crate::db::DbHandle::open(v) {
            Ok(handle) => {
                ensure_index_built(&handle, v);
                Some(handle)
            }
            Err(err) => {
                tracing::warn!(?err, "could not open SQLite for MCP search; falling back");
                None
            }
        });

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response_line = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => {
                // Wrap dispatch so a panic anywhere in `handle_request` (or
                // deeper in the wiki/db/git plumbing it calls) becomes a
                // JSON-RPC error frame instead of taking the subprocess
                // down. The closure captures references only; AssertUnwindSafe
                // documents that we accept any state inconsistency the
                // panicking code may have left behind — for stdio servers
                // there's nothing meaningful for us to "recover" anyway.
                let id_for_panic = req.id.clone().unwrap_or(Value::Null);
                let method_for_log = req.method.clone();
                panic_safe_dispatch(
                    &id_for_panic,
                    &method_for_log,
                    std::panic::AssertUnwindSafe(|| {
                        handle_request(&req, vault_path.as_deref(), db.as_ref())
                    }),
                )
            }
            Err(err) => {
                // Per JSON-RPC 2.0: parse-errors must reply with id=null
                // ONLY if the message was a request. We can't tell from
                // unparseable bytes whether the sender expected a reply,
                // so we err on the side of replying — but check whether
                // the raw line at least *looks* like it omitted `id` (a
                // notification). Notifications without an `id` field never
                // get a response.
                if line.contains("\"id\"") {
                    serde_json::to_string(&RpcResponse::<Value> {
                        jsonrpc: "2.0",
                        id: Value::Null,
                        result: None,
                        error: Some(RpcError {
                            code: -32700,
                            message: format!("parse error: {err}"),
                            data: None,
                        }),
                    })
                    .unwrap_or_else(|_| String::from("{}"))
                } else {
                    String::new()
                }
            }
        };
        // Notifications produce an empty response — JSON-RPC 2.0 forbids
        // sending anything back, including a blank line. Claude Desktop
        // (and any spec-compliant client) tries to JSON-parse each line
        // it receives, so a stray "\n" causes "Unexpected end of JSON
        // input" before a single tool call has happened.
        if response_line.trim().is_empty() {
            continue;
        }
        writeln!(out, "{}", response_line)?;
        out.flush()?;
    }
    Ok(())
}

fn handle_request(
    req: &RpcRequest,
    vault: Option<&std::path::Path>,
    db: Option<&crate::db::DbHandle>,
) -> String {
    // JSON-RPC 2.0 §4.1: a Request object without an `id` member is a
    // Notification, and the Server MUST NOT reply to it. We catch every
    // notification here so the protocol-error and method-not-found arms
    // below never accidentally produce output for an `id`-less envelope.
    if req.id.is_none() {
        return String::new();
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    if req.jsonrpc != "2.0" {
        return error_response(&id, -32600, "expected jsonrpc 2.0", None);
    }
    match req.method.as_str() {
        "initialize" => ok_response(
            &id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                "capabilities": { "tools": { "listChanged": false } }
            }),
        ),
        "tools/list" => ok_response(&id, json!({ "tools": tool_descriptors() })),
        "tools/call" => match vault {
            Some(v) => match call_tool(&req.params, v, db) {
                Ok(payload) => ok_response(
                    &id,
                    json!({
                        "content": [{ "type": "text", "text": payload }],
                        "isError": false
                    }),
                ),
                Err(err) => ok_response(
                    &id,
                    json!({
                        "content": [{ "type": "text", "text": err }],
                        "isError": true
                    }),
                ),
            },
            None => error_response(&id, -32000, "no Brain vault is mounted on this host", None),
        },
        "ping" => ok_response(&id, json!({})),
        _ => error_response(&id, -32601, &format!("method not found: {}", req.method), None),
    }
}

/// Builds (or refreshes) the SQLite index if it's empty. Cheap on small
/// vaults, important for never-mounted-by-GUI vaults so MCP search has
/// real data to query. We deliberately skip a full rebuild when the
/// index already has rows — the GUI's wiki watcher keeps it fresh.
fn ensure_index_built(db: &crate::db::DbHandle, vault: &std::path::Path) {
    let count: i64 = db
        .with(|conn| {
            Ok(conn
                .query_row("SELECT count(*) FROM pages", [], |r| r.get(0))
                .unwrap_or(0))
        })
        .unwrap_or(0);
    if count == 0 {
        if let Err(err) = crate::db::pages_index::rebuild(db, vault) {
            tracing::warn!(?err, "initial pages-index rebuild in MCP subprocess failed");
        }
    }
}

fn ok_response(id: &Value, result: Value) -> String {
    serde_json::to_string(&RpcResponse::<Value> {
        jsonrpc: "2.0",
        id: id.clone(),
        result: Some(result),
        error: None,
    })
    .unwrap_or_default()
}

fn error_response(id: &Value, code: i64, message: &str, data: Option<Value>) -> String {
    serde_json::to_string(&RpcResponse::<Value> {
        jsonrpc: "2.0",
        id: id.clone(),
        result: None,
        error: Some(RpcError {
            code,
            message: message.to_string(),
            data,
        }),
    })
    .unwrap_or_default()
}

/// Runs a request handler and converts any panic into a JSON-RPC 2.0
/// "Internal error" (-32603) response instead of letting the panic
/// propagate. Without this wrapper a single buggy tool call could exit
/// the whole `brain mcp` subprocess; Claude Desktop then logs
/// "Server transport closed unexpectedly" and the user has to restart
/// the client to recover. With it, the connection survives and the
/// model gets a structured error it can present to the user.
fn panic_safe_dispatch<F>(id: &Value, method: &str, f: F) -> String
where
    F: FnOnce() -> String + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(s) => s,
        Err(payload) => {
            let msg = panic_message(payload.as_ref());
            tracing::error!(method = %method, panic_msg = %msg, "MCP handler panicked — converting to JSON-RPC error");
            error_response(
                id,
                -32603,
                &format!("internal error: handler panicked: {msg}"),
                None,
            )
        }
    }
}

/// Best-effort extraction of a human-readable message from an unwound
/// panic payload. The standard library models `panic!` payloads as
/// `Box<dyn Any + Send>` so we have to downcast; in practice they are
/// always either `&'static str` (from `panic!("literal")`) or `String`
/// (from `panic!("{}", expr)`). Anything else falls back to a generic
/// label so the JSON-RPC error message stays present even for exotic
/// panics from third-party code.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with non-string payload".to_string()
    }
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "brain_search",
            "description": "Search the Brain wiki by hybrid lexical match. Returns hits sorted by score.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }
        }),
        json!({
            "name": "brain_get_page",
            "description": "Read a wiki page by id (e.g. 'entities/alice'). Returns title, frontmatter, body.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_page_exists",
            "description": "Lightweight existence check: returns {id, exists} for the given page id. Use this when you only need yes/no (e.g. before deciding to create-vs-update) — much cheaper than brain_get_page, which loads and parses the entire markdown body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "page id, e.g. 'entities/alice'"
                    }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_get_context",
            "description": "Return a wiki page plus the pages it links to and pages that link to it (1-hop).",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_list_pages",
            "description": "List wiki page ids grouped by type (entities, concepts, sources, topics). All arguments are optional; with no args the response shape is the legacy four-bucket layout. Use the filters on large vaults to keep responses small and fast: 'type' restricts to a single bucket, 'prefix' matches an id prefix like 'entities/dextra', 'limit' caps each bucket's size, 'offset' enables pagination.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["entities", "concepts", "sources", "topics"],
                        "description": "Restrict to a single bucket; the others are returned empty."
                    },
                    "prefix": {
                        "type": "string",
                        "description": "Id-prefix substring filter, e.g. 'entities/dextra'."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum entries per bucket (default: no limit)."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Skip the first N entries per bucket (default: 0)."
                    }
                }
            }
        }),
        json!({
            "name": "brain_write_page",
            "description": "Create or overwrite a wiki page. Caller must include valid YAML frontmatter (id, type, title) followed by the markdown body. The watcher will lint and auto-commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "page id, e.g. 'entities/alice'" },
                    "content": { "type": "string", "description": "full markdown including frontmatter" }
                },
                "required": ["id", "content"]
            }
        }),
        json!({
            "name": "brain_write_raw_file",
            "description": "Place a raw artifact under 01_raw/<connector>/<relative path>. Use for ingest before creating a source page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connector": { "type": "string" },
                    "relative_path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["connector", "relative_path", "content"]
            }
        }),
        json!({
            "name": "brain_graph",
            "description": "Return the wiki graph as nodes + edges, optionally filtered by page type list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "types": { "type": "array", "items": { "type": "string" } }
                }
            }
        }),
        json!({
            "name": "brain_lint_report",
            "description": "Return the current lint state of the wiki as { errors, warnings } — both are arrays of { path, kind, message }. Errors block auto-commits, warnings don't. Common warning kinds you should fix in place via brain_write_page: 'unregistered-type' (frontmatter type isn't one of entity/concept/source/topic — usually a plural slipped in), 'missing-title', 'non-canonical-wiki-link'. Common error kinds: 'frontmatter' (malformed YAML), 'duplicate-id' (two files share an id), 'broken-link' (wiki link points at a missing page). Use this when the user asks you to clean up the wiki: loop through the report, fix each entry, then call again until clean.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "brain_query",
            "description": "Dataview-style structured query against page metadata. Supports fields id, type, title, tag, created, updated; operators `:` (eq), `:>`, `:<`; AND, OR, NOT; quoted values for spaces. Examples: `type:source AND tag:customer AND updated:>2026-04-01`, `tag:nis2 OR tag:dora`, `NOT type:source AND title:\"NLSpec\"`. Use this for filtered listings; use brain_search for free-text search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }
        }),
    ]
}

fn call_tool(
    params: &Value,
    vault: &std::path::Path,
    db: Option<&crate::db::DbHandle>,
) -> Result<String, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'name'".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // Fail fast when the vault is no longer reachable (typical cause:
    // user pulled the SSD without ejecting). Without this guard the
    // first filesystem call further down panics or returns a cryptic
    // "no such file"; the LLM has no way to tell the user that the
    // disk is gone vs. a real bug. Returning a structured prefix the
    // model recognises lets it react with "BRAIN is disconnected,
    // reconnect the drive and try again" instead of guessing.
    if !crate::vault::layout::is_vault(vault) {
        return Err(format!(
            "BRAIN_VAULT_DISCONNECTED: the BRAIN vault at '{}' is not currently accessible. \
             The disk holding the vault was unplugged or the path is no longer valid. \
             Tell the user to reconnect the BRAIN drive and try again. \
             Do not attempt to recreate or guess at the missing data.",
            vault.display()
        ));
    }

    match name {
        "brain_search" => {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("");
            // Prefer the FTS5 path when the index handle is available — it
            // tokenises hyphenated ids correctly so "spec driven development"
            // matches "spec-driven-development". When `db` is None we fall
            // back to the brute-force walker so the LLM still gets results.
            let hits = search::search_with_db(vault, q, db).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "brain_get_page" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let page = tree::read_page(vault, id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&page).unwrap_or_default())
        }
        "brain_page_exists" => {
            // Cheap yes/no check the user-feedback called out: an LLM
            // wanting to know "does entities/foo already exist?" would
            // otherwise call brain_get_page (which loads + parses the
            // whole markdown body) just to throw away the result. This
            // tool is one Path::is_file() — sub-millisecond — so the
            // create-vs-update decision costs almost nothing.
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            if id.is_empty() {
                return Err("'id' must not be empty".to_string());
            }
            // Same hardening as brain_write_raw_file: defend against
            // path-traversal smuggled into the id (e.g. `../../etc/passwd`).
            // Reject before joining onto the wiki dir so the resulting
            // Path::is_file() check can never escape the vault root.
            if id.contains("..") {
                return Err("id may not contain '..'".to_string());
            }
            let target = wiki_dir(vault).join(format!("{id}.md"));
            let exists = target.is_file();
            Ok(serde_json::to_string(&json!({
                "id": id,
                "exists": exists,
            }))
            .unwrap_or_default())
        }
        "brain_get_context" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let page = tree::read_page(vault, id).map_err(|e| e.to_string())?;
            // `tree::read_page` already strips the YAML frontmatter, so
            // `page::parse` would refuse the body with "missing frontmatter
            // delimiter" → surfaced as a `lint:` error to the LLM. Skip the
            // re-parse and pull wiki links directly from the body.
            let outbound = page::extract_wiki_links(&page.body);
            let backlinks = search::backlinks(vault, id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&json!({
                "page": page,
                "outbound": outbound,
                "backlinks": backlinks,
            }))
            .unwrap_or_default())
        }
        "brain_list_pages" => list_pages_dispatch(&args, vault, db),
        "brain_write_page" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'content'".to_string())?;
            let parsed = page::parse(content).map_err(|e| format!("invalid page content: {e}"))?;
            // Auto-normalize markdown links to canonical [[wiki-link]]
            // form before write. LLMs default to standard markdown
            // syntax `[Dan](entities/dan-shapiro)` — without this the
            // graph view sees no edges and refactors are fragile. Only
            // the body is rewritten; the YAML frontmatter is kept
            // verbatim and we re-stitch the file.
            let normalized_body = page::normalize_internal_links(&parsed.body);
            let normalized_content = if normalized_body == parsed.body {
                content.to_string()
            } else {
                rebuild_page_file(content, &normalized_body)
            };
            let target = wiki_dir(vault).join(format!("{id}.md"));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&target, &normalized_content).map_err(|e| e.to_string())?;
            // Run lint synchronously so the caller learns about a hard error.
            let report = lint::lint(vault).map_err(|e| e.to_string())?;
            if !report.is_clean() {
                // Pre-0.2.4 we only surfaced `report.errors.len()` and
                // discarded the per-error `{path, kind, message}` triples.
                // The LLM then had to probe the wiki to discover *which*
                // links were broken. Now we serialise the full LintError
                // array next to the human-readable count, so a single
                // round-trip gives the model everything it needs to
                // create the missing pages or fix typos.
                let detail = serde_json::to_string(&report.errors)
                    .unwrap_or_else(|_| "[]".to_string());
                return Err(format!(
                    "page written but lint failed: {} errors\n{detail}",
                    report.errors.len()
                ));
            }
            // Best-effort commit; the watcher would also do this, but we
            // commit immediately so the change appears in `wiki_history`.
            let _ = wiki_git::commit_all(&wiki_dir(vault), &format!("write_page: {id} via MCP"));
            Ok(format!("wrote {id}"))
        }
        "brain_write_raw_file" => {
            let connector = args
                .get("connector")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'connector'".to_string())?;
            let rel = args
                .get("relative_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'relative_path'".to_string())?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'content'".to_string())?;
            if rel.contains("..") {
                return Err("relative_path may not contain '..'".to_string());
            }
            let target = raw_dir(vault).join(connector).join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&target, content).map_err(|e| e.to_string())?;
            Ok(format!("wrote 01_raw/{connector}/{rel}"))
        }
        "brain_graph" => {
            let types = args
                .get("types")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                });
            let filters = graph::GraphFilters {
                types,
                tags: None,
                updated_after: None,
            };
            let g = graph::build_graph(vault, &filters).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&g).unwrap_or_default())
        }
        "brain_query" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let handle = db.ok_or_else(|| {
                "brain_query requires the SQLite index — vault not indexed yet".to_string()
            })?;
            let hits = crate::viewer::query::executor::run(handle, query)
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "brain_lint_report" => {
            // Read-only view of the same lint pass that drives the
            // auto-commit watcher and the Tauri toast bridge. Errors
            // block commits in the watcher; warnings don't. Surfacing
            // both here lets an agent triage which to fix first — and
            // closes the loop where the user could see a `wiki-lint-
            // error` toast but the LLM had no MCP path to inspect it.
            let report = lint::lint(vault).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// `brain_list_pages` dispatch with optional filters and pagination.
/// Two acceleration strategies:
///   1. **DB fastpath** (`db: Some`) — `SELECT id FROM pages` against
///      the SQLite index. Sub-millisecond on any vault size, completely
///      independent of disk speed. This is the fix for the user-reported
///      4-minute timeout on slow storage.
///   2. **Filesystem fallback** — current `tree::list_tree` walk, which
///      does *not* read file contents. Only used when no DB handle is
///      available (e.g. an unindexed vault).
///
/// Optional arguments (all backward-compatible — no args = same shape
/// as pre-0.2.4):
///   - `type`: `"entities" | "concepts" | "sources" | "topics"` —
///     restrict to one bucket; the others are returned empty.
///   - `prefix`: id-prefix substring filter, e.g. `"entities/dextra"`.
///   - `limit`: cap each bucket's result count.
///   - `offset`: skip the first N results per bucket (after sort).
fn list_pages_dispatch(
    args: &Value,
    vault: &std::path::Path,
    db: Option<&crate::db::DbHandle>,
) -> Result<String, String> {
    const BUCKETS: [&str; 4] = ["entities", "concepts", "sources", "topics"];

    let type_filter = args.get("type").and_then(Value::as_str).map(String::from);
    let prefix = args
        .get("prefix")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(0);

    if let Some(ref t) = type_filter {
        if !BUCKETS.contains(&t.as_str()) {
            return Err(format!(
                "invalid type '{t}': expected one of entities|concepts|sources|topics"
            ));
        }
    }

    // Collect (bucket, id) pairs from whichever source is fastest.
    let pairs: Vec<(String, String)> = if let Some(handle) = db {
        list_page_ids_via_db(handle).map_err(|e| e.to_string())?
    } else {
        list_page_ids_via_filesystem(vault).map_err(|e| e.to_string())?
    };

    // Server-side filtering — saves both bytes on the wire and tokens
    // for the LLM.
    let filtered: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(bucket, id)| {
            if let Some(ref t) = type_filter {
                if bucket != t {
                    return false;
                }
            }
            if !prefix.is_empty() && !id.starts_with(&prefix) {
                return false;
            }
            true
        })
        .collect();

    // Group into the four canonical buckets and sort each one for
    // deterministic ordering (filesystem walk order is platform-dep).
    let mut grouped: std::collections::HashMap<&str, Vec<String>> =
        BUCKETS.iter().map(|b| (*b, Vec::new())).collect();
    for (bucket, id) in filtered {
        if let Some(slot) = grouped.get_mut(bucket.as_str()) {
            slot.push(id);
        }
    }
    for ids in grouped.values_mut() {
        ids.sort();
    }

    // Apply offset+limit per bucket, then assemble the response in the
    // canonical four-key order so the JSON shape stays stable.
    let mut out = serde_json::Map::new();
    for bucket in BUCKETS {
        let ids = grouped.remove(bucket).unwrap_or_default();
        let sliced: Vec<Value> = ids
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .map(Value::String)
            .collect();
        out.insert(bucket.to_string(), Value::Array(sliced));
    }

    Ok(
        serde_json::to_string_pretty(&Value::Object(out))
            .unwrap_or_default(),
    )
}

/// DB fastpath: `SELECT id FROM pages` and derive bucket from the
/// id prefix (`entities/alice` → `entities`). Matches the layout
/// already used by `tree::list_tree`. Unknown buckets are silently
/// dropped — they shouldn't occur unless the index drifts from the
/// filesystem schema.
fn list_page_ids_via_db(
    handle: &crate::db::DbHandle,
) -> crate::db::DbResult<Vec<(String, String)>> {
    handle.with(|conn| {
        let mut stmt = conn.prepare("SELECT id FROM pages")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;
        let mut out = Vec::new();
        for r in rows {
            let id = r?;
            if let Some((bucket, _)) = id.split_once('/') {
                out.push((bucket.to_string(), id));
            }
        }
        Ok(out)
    })
}

/// Filesystem fallback for vaults that haven't been DB-indexed yet.
/// Reuses the existing tree walker (no file content reads) and
/// flattens the four-bucket result into a `(bucket, id)` list so the
/// dispatch stays uniform.
fn list_page_ids_via_filesystem(
    vault: &std::path::Path,
) -> Result<Vec<(String, String)>, ViewerErrAdapter> {
    let t = tree::list_tree(vault).map_err(ViewerErrAdapter)?;
    let mut out = Vec::with_capacity(
        t.entities.len() + t.concepts.len() + t.sources.len() + t.topics.len(),
    );
    for id in t.entities {
        out.push(("entities".to_string(), id));
    }
    for id in t.concepts {
        out.push(("concepts".to_string(), id));
    }
    for id in t.sources {
        out.push(("sources".to_string(), id));
    }
    for id in t.topics {
        out.push(("topics".to_string(), id));
    }
    Ok(out)
}

/// Adapter so `?`-propagation from `tree::list_tree` (returns
/// `ViewerError`) lands as a `String` cleanly through the
/// `Result<…, String>` boundary used by `call_tool`.
struct ViewerErrAdapter(crate::viewer::ViewerError);

impl std::fmt::Display for ViewerErrAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Splices a normalized body back into a page file, keeping the original
/// frontmatter exactly as the LLM produced it. The shape we need to
/// preserve is `---\n<yaml>\n---\n<body>`; we find the second
/// closing `---` line and replace everything after it with
/// `\n<normalized_body>` (preserving leading newlines).
fn rebuild_page_file(original_content: &str, normalized_body: &str) -> String {
    let trimmed = original_content.trim_start_matches('\u{feff}');
    let Some(after_first) = trimmed.strip_prefix("---") else {
        return normalized_body.to_string();
    };
    // Same logic as page::parse — find the closing fence.
    let after_first = after_first.trim_start_matches('\n');
    let close = after_first
        .find("\n---\n")
        .or_else(|| after_first.find("\n---"));
    let Some(end) = close else {
        return original_content.to_string();
    };
    // Front matter occupies original_content[..front_end_offset]. Compute
    // it by indexing into the trimmed slice.
    let header_len = trimmed.len() - after_first.len();
    let front_end = header_len + end + "\n---".len();
    let front = &trimmed[..front_end];
    // Preserve a single newline between frontmatter and body.
    format!("{front}\n\n{normalized_body}")
}

#[cfg(test)]
mod rebuild_page_tests {
    use super::*;

    #[test]
    fn rebuild_page_file_preserves_frontmatter_exactly() {
        let original = "---\nid: entities/alice\ntype: entity\ntitle: Alice\n---\n\nold body\n";
        let normalized = "new [[entities/bob]] body";
        let result = rebuild_page_file(original, normalized);
        assert!(result.starts_with("---\nid: entities/alice"));
        assert!(result.contains("new [[entities/bob]] body"));
        assert!(!result.contains("old body"));
    }

    #[test]
    fn rebuild_page_file_returns_normalized_when_frontmatter_missing() {
        let original = "no frontmatter here";
        let normalized = "[[entities/alice]]";
        let result = rebuild_page_file(original, normalized);
        assert_eq!(result, "[[entities/alice]]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the minimal `00_meta/brain-marker.json` so the
    /// `is_vault()` pre-flight check in `call_tool` accepts the temp
    /// directory as a real vault. Without this the new
    /// "BRAIN_VAULT_DISCONNECTED" guard rejects every tool call in
    /// tests that build their vault via `ensure_skeleton` only.
    fn seed_marker(vault: &std::path::Path) {
        let marker = crate::vault::marker::VaultMarker::new("test");
        crate::vault::marker::write_marker(vault, &marker).unwrap();
    }

    #[test]
    fn initialize_response_includes_protocol_and_server_metadata() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.contains(PROTOCOL_VERSION));
        assert!(resp.contains(&format!("\"name\":\"{SERVER_NAME}\"")));
    }

    #[test]
    fn tools_list_advertises_every_tool_handler_we_implement() {
        // Discovery test — `tools/list` is what every MCP client calls
        // to learn what BRAIN can do. If a handler exists in `call_tool`
        // but its descriptor was forgotten, no LLM ever finds it. Spot
        // check covers the two 0.2.4 additions plus a few load-bearing
        // older tools so a future deletion gets caught here.
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        for name in [
            "brain_search",
            "brain_get_page",
            "brain_page_exists",
            "brain_get_context",
            "brain_list_pages",
            "brain_write_page",
            "brain_write_raw_file",
            "brain_graph",
            "brain_query",
            "brain_lint_report",
        ] {
            assert!(
                resp.contains(name),
                "tools/list response is missing {name}: {resp}"
            );
        }
    }

    #[test]
    fn tools_call_without_vault_returns_a_descriptive_error() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "tools/call".into(),
            params: json!({"name": "brain_list_pages", "arguments": {}}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.contains("no Brain vault is mounted"));
    }

    #[test]
    fn unknown_method_returns_method_not_found_error() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(4)),
            method: "does_not_exist".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.contains("method not found"));
    }

    #[test]
    fn notifications_initialized_yields_an_empty_response_per_jsonrpc_spec() {
        // No `id` field = notification. JSON-RPC 2.0 §4.1 forbids a reply.
        // Claude Desktop's stdio transport JSON.parse()s every line, so a
        // blank line causes "Unexpected end of JSON input" — what bit us.
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "notifications/initialized".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(
            resp.is_empty(),
            "notifications must yield zero bytes, got: '{resp}'"
        );
    }

    #[test]
    fn unknown_notification_methods_also_yield_empty_response() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "notifications/cancelled".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.is_empty(), "unknown notifications must not get a response");
    }

    #[test]
    fn notifications_skip_protocol_version_check_so_no_error_response_leaks() {
        // Even with a wrong jsonrpc version, a notification must stay
        // silent — otherwise we'd emit an error frame for every junk
        // notification and break the client's read loop.
        let req = RpcRequest {
            jsonrpc: "1.0".into(),
            id: None,
            method: "notifications/something".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.is_empty());
    }

    #[test]
    fn brain_get_context_does_not_emit_lint_error_after_frontmatter_strip() {
        // Regression: read_page strips the YAML frontmatter from the body,
        // so re-parsing it would fail with "missing frontmatter delimiter"
        // → surfaced as a `lint:` error to the calling LLM. The fix uses
        // extract_wiki_links directly on the body.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let dir = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinked to [[entities/bob]] and [[concepts/nlspec]].\n",
        )
        .unwrap();
        let result = call_tool(
            &json!({
                "name": "brain_get_context",
                "arguments": { "id": "entities/alice" }
            }),
            tmp.path(),
            None,
        )
        .expect("brain_get_context should succeed");
        // Sanity: outbound list must contain the two wiki links.
        assert!(result.contains("entities/bob"));
        assert!(result.contains("concepts/nlspec"));
        // Must not surface any lint chatter.
        assert!(!result.contains("lint:"), "result leaks lint-error: {result}");
    }

    #[test]
    fn brain_lint_report_surfaces_unregistered_type_warning_for_agent_cleanup() {
        // The agent-cleanup workflow: an MCP client (Claude Code, an
        // Ollama-driven host, …) is told "fix all lint problems" and
        // calls this tool to discover *which* pages are wrong. The
        // unregistered-type warning is the most common one in the wild
        // — pluralised `type:` slipped in by an earlier write — so we
        // assert it surfaces with the offending value and path the
        // agent will need to write back via brain_write_page.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        // Clean page — must not appear in the warning list.
        std::fs::write(
            entities.join("alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nbody\n",
        )
        .unwrap();
        // Drifted page — `entities` (plural) is not a registered type.
        std::fs::write(
            entities.join("bob.md"),
            "---\nid: entities/bob\ntype: entities\ntitle: Bob\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nbody\n",
        )
        .unwrap();
        let result = call_tool(
            &json!({
                "name": "brain_lint_report",
                "arguments": {}
            }),
            tmp.path(),
            None,
        )
        .expect("brain_lint_report should succeed");
        assert!(
            result.contains("unregistered-type"),
            "missing unregistered-type kind: {result}"
        );
        assert!(
            result.contains("bob.md"),
            "warning should name the offending file: {result}"
        );
        // Sanity: the clean page must not produce a same-kind warning.
        // Cheap proxy — alice.md should not appear next to the kind.
        let kind_idx = result.find("\"unregistered-type\"").unwrap();
        let around = &result[kind_idx.saturating_sub(200)..result.len().min(kind_idx + 400)];
        assert!(
            !around.contains("alice.md"),
            "unregistered-type warning is wrongly attached to alice.md: {around}"
        );
    }

    #[test]
    fn brain_search_uses_fts5_tokeniser_when_db_handle_is_supplied() {
        // Regression for the connectivity test: brute-force substring
        // search misses tokenised matches, so query "spec driven
        // development" did not return the page id "spec-driven-development".
        // With the FTS5 path enabled, the unicode61 tokeniser splits
        // hyphenated words and the query hits.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let dir = wiki_dir(tmp.path()).join("concepts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("spec-driven-development.md"),
            "---\nid: concepts/spec-driven-development\ntype: concept\ntitle: Spec-Driven Development\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\n# Spec-Driven Development\n\nA methodology where the spec drives the code.\n",
        )
        .unwrap();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();

        let result = call_tool(
            &json!({
                "name": "brain_search",
                "arguments": { "query": "spec driven development" }
            }),
            tmp.path(),
            Some(&db),
        )
        .expect("brain_search should succeed");
        assert!(
            result.contains("concepts/spec-driven-development"),
            "FTS5 search must match across hyphens, got: {result}"
        );
    }

    #[test]
    fn call_tool_rejects_with_brain_vault_disconnected_when_marker_missing() {
        // Simulates "user pulled the SSD while the MCP child was alive".
        // The vault path is still pointed at, but the marker file is
        // gone — every tool must short-circuit with a structured error
        // the LLM can act on, not blow up on a generic fs::not-found.
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        // Deliberately do NOT call ensure_skeleton / seed_marker — the
        // path looks like nothing.
        let err = call_tool(
            &json!({
                "name": "brain_search",
                "arguments": { "query": "anything" }
            }),
            tmp.path(),
            None,
        )
        .expect_err("disconnected vault must reject the call");
        assert!(
            err.starts_with("BRAIN_VAULT_DISCONNECTED:"),
            "expected structured prefix, got: {err}"
        );
    }

    #[test]
    fn panic_safe_dispatch_returns_jsonrpc_error_when_handler_panics() {
        // Regression: a panic deep inside a tool handler used to take down
        // the whole MCP subprocess, so Claude Desktop saw "Server transport
        // closed unexpectedly" and the user had to restart Claude. With a
        // panic catcher in place, the panic must be converted to a clean
        // JSON-RPC error response so the connection stays alive and the
        // calling LLM gets a chance to react.
        let id = json!(42);
        let resp_str = panic_safe_dispatch(&id, "test/method", || {
            panic!("simulated handler panic");
        });
        let parsed: Value =
            serde_json::from_str(&resp_str).expect("response must be valid JSON-RPC");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 42);
        // -32603 is JSON-RPC 2.0's "Internal error" code; using the
        // standard one means clients can render it with their generic
        // error UI without learning a Brain-specific code.
        assert_eq!(parsed["error"]["code"], -32603);
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("simulated handler panic"),
            "the original panic message must surface to the client, got: {resp_str}"
        );
    }

    #[test]
    fn panic_safe_dispatch_passes_through_normal_string_results_unchanged() {
        // Sanity: the wrapper must not alter happy-path payloads. The
        // existing dispatch loop hands off pre-serialized strings; the
        // wrapper relays them verbatim when no panic occurs.
        let id = json!("abc");
        let resp = panic_safe_dispatch(&id, "test/method", || "{\"jsonrpc\":\"2.0\"}".to_string());
        assert_eq!(resp, "{\"jsonrpc\":\"2.0\"}");
    }

    /// Helper: build a small but realistic vault with two entities, one
    /// concept and one source. Used by the `brain_list_pages`
    /// performance-improvement tests so each test starts from a known
    /// shape without re-typing the boilerplate.
    fn build_sample_vault() -> tempfile::TempDir {
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = tempfile::TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let mk = |sub: &str, slug: &str| {
            let dir = wiki_dir(tmp.path()).join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{slug}.md")),
                format!(
                    "---\nid: {sub}/{slug}\ntype: {kind}\ntitle: {slug}\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nbody\n",
                    kind = match sub {
                        "entities" => "entity",
                        "concepts" => "concept",
                        "sources" => "source",
                        "topics" => "topic",
                        _ => "entity",
                    }
                ),
            )
            .unwrap();
        };
        mk("entities", "alice");
        mk("entities", "dextra-acme");
        mk("concepts", "nlspec");
        mk("sources", "kickoff-doc");
        tmp
    }

    fn list_pages_call(
        vault: &std::path::Path,
        args: Value,
        db: Option<&crate::db::DbHandle>,
    ) -> Value {
        let result = call_tool(
            &json!({ "name": "brain_list_pages", "arguments": args }),
            vault,
            db,
        )
        .expect("brain_list_pages should succeed");
        serde_json::from_str(&result).expect("result must be valid JSON")
    }

    #[test]
    fn list_pages_db_fastpath_returns_same_ids_as_filesystem_fallback() {
        // Without a DB handle we walk the filesystem; with one we should
        // hit a much faster `SELECT id, type FROM pages` path. Both must
        // surface the same set of IDs, otherwise we have a divergence
        // bug that would silently mislead the LLM.
        let tmp = build_sample_vault();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();

        let fs_result = list_pages_call(tmp.path(), json!({}), None);
        let db_result = list_pages_call(tmp.path(), json!({}), Some(&db));

        // Set-equality per bucket so ordering doesn't matter.
        for bucket in ["entities", "concepts", "sources", "topics"] {
            let fs_set: std::collections::HashSet<&str> = fs_result[bucket]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            let db_set: std::collections::HashSet<&str> = db_result[bucket]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert_eq!(
                fs_set, db_set,
                "DB and filesystem paths disagree on bucket '{bucket}'"
            );
        }
    }

    #[test]
    fn list_pages_with_type_filter_only_populates_that_bucket() {
        // Reduces response size for callers who only need one type — the
        // primary fix for the user-reported timeout on big vaults.
        let tmp = build_sample_vault();
        let result = list_pages_call(tmp.path(), json!({ "type": "entities" }), None);
        assert!(!result["entities"].as_array().unwrap().is_empty());
        assert!(result["concepts"].as_array().unwrap().is_empty());
        assert!(result["sources"].as_array().unwrap().is_empty());
        assert!(result["topics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn list_pages_with_prefix_filter_returns_only_matching_ids() {
        // Lets callers narrow down to e.g. `entities/dextra-*` instead
        // of pulling every entity ID and filtering client-side.
        let tmp = build_sample_vault();
        let result = list_pages_call(
            tmp.path(),
            json!({ "prefix": "entities/dextra" }),
            None,
        );
        let ents: Vec<&str> = result["entities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(ents, vec!["entities/dextra-acme"]);
        assert!(
            result["concepts"].as_array().unwrap().is_empty(),
            "prefix on entities must not leak into other buckets"
        );
    }

    #[test]
    fn list_pages_with_limit_caps_each_bucket() {
        // Pagination affordance — caller can request a bounded page size.
        let tmp = build_sample_vault();
        let result = list_pages_call(tmp.path(), json!({ "limit": 1 }), None);
        for bucket in ["entities", "concepts", "sources", "topics"] {
            let len = result[bucket].as_array().unwrap().len();
            assert!(
                len <= 1,
                "bucket {bucket} should be capped at limit=1 but has {len}"
            );
        }
    }

    #[test]
    fn list_pages_with_offset_skips_leading_entries_per_bucket() {
        // Offset only makes sense paired with sort order — IDs are
        // already returned sorted ascending. With two entities and
        // offset=1 we expect exactly one entity returned.
        let tmp = build_sample_vault();
        let result = list_pages_call(
            tmp.path(),
            json!({ "type": "entities", "offset": 1 }),
            None,
        );
        let ents = result["entities"].as_array().unwrap();
        assert_eq!(
            ents.len(),
            1,
            "offset=1 on 2-entity vault should leave exactly 1 entry"
        );
    }

    #[test]
    fn list_pages_with_no_args_returns_full_grouped_shape_for_backward_compat() {
        // Defensive: existing callers (LLMs that have been pointing at
        // older BRAIN releases) must keep working — no args = same
        // four-bucket shape with all IDs populated.
        let tmp = build_sample_vault();
        let result = list_pages_call(tmp.path(), json!({}), None);
        assert!(result.get("entities").is_some());
        assert!(result.get("concepts").is_some());
        assert!(result.get("sources").is_some());
        assert!(result.get("topics").is_some());
        assert_eq!(result["entities"].as_array().unwrap().len(), 2);
        assert_eq!(result["concepts"].as_array().unwrap().len(), 1);
        assert_eq!(result["sources"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn page_exists_returns_true_for_a_page_that_is_on_disk() {
        // The lightweight "does this id exist?"-check the user feedback
        // asked for. brain_get_page returns the whole markdown body
        // for this question, which is wasted bandwidth and tokens; this
        // tool only does a single Path::exists() under 02_wiki/<id>.md.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let dir = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nbody\n",
        )
        .unwrap();
        let result = call_tool(
            &json!({
                "name": "brain_page_exists",
                "arguments": { "id": "entities/alice" }
            }),
            tmp.path(),
            None,
        )
        .expect("brain_page_exists should succeed");
        let parsed: Value = serde_json::from_str(&result).expect("must be valid JSON");
        assert_eq!(parsed["exists"], json!(true));
        assert_eq!(parsed["id"], json!("entities/alice"));
    }

    #[test]
    fn page_exists_returns_false_for_a_page_that_is_not_on_disk() {
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let result = call_tool(
            &json!({
                "name": "brain_page_exists",
                "arguments": { "id": "entities/never-created" }
            }),
            tmp.path(),
            None,
        )
        .expect("brain_page_exists must succeed even for missing pages — the missing case is data, not error");
        let parsed: Value = serde_json::from_str(&result).expect("must be valid JSON");
        assert_eq!(parsed["exists"], json!(false));
        assert_eq!(parsed["id"], json!("entities/never-created"));
    }

    #[test]
    fn page_exists_rejects_id_with_path_traversal_components() {
        // Defensive: an id like "../../../etc/passwd" must be rejected
        // before it gets joined onto the wiki dir. Same hardening the
        // existing brain_write_raw_file does for connector paths.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_page_exists",
                "arguments": { "id": "../../etc/passwd" }
            }),
            tmp.path(),
            None,
        )
        .expect_err("path traversal must reject");
        assert!(err.contains(".."));
    }

    #[test]
    fn page_exists_rejects_empty_or_missing_id() {
        // Hardening: the LLM might forget the `id` arg entirely or
        // pass an empty string. Either way the tool must return a
        // crisp error rather than walking the vault root.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_page_exists",
                "arguments": {}
            }),
            tmp.path(),
            None,
        )
        .expect_err("missing id must reject");
        assert!(err.to_lowercase().contains("id"));
    }

    #[test]
    fn page_exists_propagates_vault_disconnect_with_canonical_prefix() {
        // The same fast-fail guard call_tool already does for every
        // other tool — when the vault disappeared mid-session, we
        // return the BRAIN_VAULT_DISCONNECTED-prefixed message the
        // LLM has been trained to recognise via the existing tools.
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        // No ensure_skeleton, no marker — looks like a torn-off vault.
        let err = call_tool(
            &json!({
                "name": "brain_page_exists",
                "arguments": { "id": "entities/alice" }
            }),
            tmp.path(),
            None,
        )
        .expect_err("disconnected vault must reject");
        assert!(err.starts_with("BRAIN_VAULT_DISCONNECTED:"));
    }

    #[test]
    fn write_page_lint_failure_returns_structured_error_list_not_just_count() {
        // Regression: pre-0.2.4 the response was just "page written but
        // lint failed: 13 errors", throwing away the LintError struct's
        // `path`/`kind`/`message` fields. The LLM had to iteratively
        // probe to find which links were broken — multiple round-trips
        // for what's already known server-side. Now the response
        // embeds the actual error array as JSON so the LLM can act on
        // it in one shot.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_write_page",
                "arguments": {
                    "id": "entities/alice",
                    "content": "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinks to [[entities/missing-page]] and [[concepts/also-missing]].\n"
                }
            }),
            tmp.path(),
            None,
        )
        .expect_err("page with broken links must fail lint");

        // The LLM-readable summary stays so humans skimming logs see
        // the count at a glance.
        assert!(
            err.contains("lint failed"),
            "human summary must remain, got: {err}"
        );
        // The machine-readable detail must include each broken link
        // target plus the `broken-link` kind. This is what closes the
        // probing loop the user complained about.
        assert!(
            err.contains("entities/missing-page"),
            "first broken link target must surface in error, got: {err}"
        );
        assert!(
            err.contains("concepts/also-missing"),
            "second broken link target must surface in error, got: {err}"
        );
        assert!(
            err.contains("broken-link"),
            "error kind must surface so the LLM can disambiguate from frontmatter errors, got: {err}"
        );
    }

    #[test]
    fn write_raw_file_rejects_path_traversal_attempts() {
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_write_raw_file",
                "arguments": {
                    "connector": "outlook",
                    "relative_path": "../../etc/passwd",
                    "content": "x"
                }
            }),
            tmp.path(),
            None,
        )
        .unwrap_err();
        assert!(err.contains(".."));
    }
}

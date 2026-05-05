//! Minimal MCP-compatible JSON-RPC server over stdio.
//!
//! Speaks the subset of MCP that Claude Code, Codex and ChatGPT Desktop
//! actually invoke during a session: `initialize`, `tools/list`,
//! `tools/call`. Each line on stdin is one JSON-RPC envelope; responses are
//! one line per request on stdout.
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
            Ok(req) => handle_request(&req, vault_path.as_deref(), db.as_ref()),
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
            "description": "List all wiki page ids grouped by type (entities, concepts, sources, topics).",
            "inputSchema": { "type": "object", "properties": {} }
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
        "brain_list_pages" => {
            let t = tree::list_tree(vault).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&t).unwrap_or_default())
        }
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
                return Err(format!(
                    "page written but lint failed: {} errors",
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
        other => Err(format!("unknown tool: {other}")),
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
    fn tools_list_returns_at_least_seven_tool_descriptors() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, None, None);
        assert!(resp.contains("brain_search"));
        assert!(resp.contains("brain_write_page"));
        assert!(resp.contains("brain_graph"));
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

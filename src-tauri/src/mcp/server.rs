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

use crate::vault::layout::{raw_dir, wiki_dir};
use crate::viewer::{graph, search, tree};
use crate::wiki::{history as wiki_history, lint, page};

const PROTOCOL_VERSION: &str = "2024-11-05";
// `serverInfo.name` shown in MCP `initialize` handshake responses. We use
// the uppercase brand to match `BRAIN_SERVER_KEY` and the rest of the UI.
const SERVER_NAME: &str = "BRAIN";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Captured at the first MCP request so `brain_ping` can report how
/// long the server has been up. `OnceLock` instead of a top-level
/// `static` initialiser because `Instant` is not const-constructible.
/// Initialised lazily on the first call to `process_uptime_seconds`
/// rather than at module load — keeps the binary's main entry-point
/// free of MCP-specific bookkeeping.
static SERVER_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn process_uptime_seconds() -> u64 {
    SERVER_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
}

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

/// Belt-and-suspenders against orphaned `brain mcp` processes.
///
/// A stdio server normally exits when the client dies: the OS closes the
/// stdin pipe, the read loop sees EOF, `run_stdio` returns. But on
/// Windows the pipe's WRITE end can be inherited by other children of
/// the client process — then the pipe never fully closes, EOF never
/// arrives, and every finished LLM session leaves a `brain.exe mcp`
/// running forever (observed in the wild: dozens after a day of use).
///
/// This watchdog snapshots the parent PID (+ name, as a PID-reuse guard)
/// at startup and polls it; when the parent is gone the server exits.
/// Exit code 0 — an orphan shutting down is normal, not an error.
fn spawn_parent_watchdog() {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let me = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let Some(parent_pid) = sys.process(me).and_then(|p| p.parent()) else {
        tracing::info!("mcp watchdog: no detectable parent process — not watching");
        return;
    };
    let parent_name = sys.process(parent_pid).map(|p| p.name().to_os_string());
    tracing::info!(
        parent_pid = parent_pid.as_u32(),
        parent = ?parent_name,
        "mcp watchdog: watching the client process"
    );
    std::thread::spawn(move || {
        let mut misses = 0u8;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let mut sys = System::new();
            sys.refresh_processes(ProcessesToUpdate::Some(&[parent_pid]), true);
            let alive = sys.process(parent_pid).is_some_and(|p| {
                // Same PID but a different image name = the PID was
                // recycled by an unrelated process; our parent is gone.
                parent_name.as_deref().is_none_or(|n| p.name() == n)
            });
            if alive {
                misses = 0;
                continue;
            }
            // Two consecutive misses (~40 s) before acting: one failed
            // process-table refresh must never take a live session down.
            misses += 1;
            if misses >= 2 {
                tracing::info!(
                    parent_pid = parent_pid.as_u32(),
                    "mcp watchdog: client process is gone — exiting"
                );
                std::process::exit(0);
            }
        }
    });
}

/// Periodic health line on stderr — which the client captures into its
/// MCP log. When a server dies without a message, or aborts on an
/// allocation failure, the last heartbeat shows whether memory had been
/// growing beforehand. Once every 10 minutes: cheap, and enough to see a
/// trend over a multi-day session.
fn spawn_health_heartbeat() {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let me = Pid::from_u32(std::process::id());
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(600));
            let mut sys = System::new();
            sys.refresh_processes(ProcessesToUpdate::Some(&[me]), true);
            let rss_mb = sys.process(me).map(|p| p.memory() / (1024 * 1024)).unwrap_or(0);
            tracing::info!(
                uptime_min = started.elapsed().as_secs() / 60,
                rss_mb,
                "mcp heartbeat"
            );
        }
    });
}

/// Entrypoint for `brain mcp`. Reads env, then runs the dispatch loop.
pub fn run_stdio() -> std::io::Result<()> {
    // Version + pid first: a crash log then says WHICH build died.
    tracing::info!(
        version = SERVER_VERSION,
        pid = std::process::id(),
        "mcp server starting"
    );
    spawn_parent_watchdog();
    spawn_health_heartbeat();
    // The configured vault path from the registration env var. We
    // deliberately do NOT `is_vault`-filter it once at startup:
    //   - the disk may be absent at launch (Claude started before the
    //     USB stick was plugged in) and appear later, and
    //   - it may be present at launch, then get unplugged and
    //     replugged mid-session.
    // Both cases need the path to stay known so we can re-probe it per
    // request. Liveness is decided dynamically by `maybe_reopen_db`
    // plus the `is_vault` guard inside `call_tool`, never captured
    // once.
    let configured_vault = std::env::var("BRAIN_VAULT_PATH")
        .map(PathBuf::from)
        .ok();

    // SQLite index handle — same DB the GUI uses, WAL mode for
    // concurrent reads. Opened lazily and self-healing across vault
    // disk unplug/replug (see `maybe_reopen_db`): a cached
    // `rusqlite::Connection` keeps a file descriptor that dies when
    // the vault disk is pulled, and replugging restores the path but
    // not the fd — so without the self-heal every tool call
    // IO-errored until the user fully restarted Claude. Starts `None`
    // and is (re)opened on the first request where the vault is
    // reachable.
    let mut db: Option<crate::db::DbHandle> = None;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        // Every exit path says why on stderr. Silent exits were exactly
        // what made intermittent disconnects undiagnosable in the wild.
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                tracing::warn!(error = %err, "mcp: stdin read failed — exiting");
                return Err(err);
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        // NO pre-flight DB/vault probe here (removed in 0.2.20). The
        // v0.2.19 version ran `is_vault` + a `SELECT 1` liveness probe
        // before EVERY request — which made `brain_ping` pay a
        // filesystem stat and a DB query, and a stale-handle hang on
        // either wedged the whole single-threaded loop (the reported
        // 4-minute ping timeout). Now `handle_request` resolves the
        // vault lazily and only for vault-touching tools, and DB ops
        // run through `db_op`'s timeout/reopen wrapper. The configured
        // path is passed UNFILTERED (no `is_vault` here) so the
        // disconnect decision happens downstream where it can be
        // bounded.
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
                        handle_request(&req, configured_vault.as_deref(), &mut db)
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
    tracing::info!("mcp: stdin closed by the client (EOF) — exiting");
    Ok(())
}

fn handle_request(
    req: &RpcRequest,
    vault: Option<&std::path::Path>,
    db: &mut Option<crate::db::DbHandle>,
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
        "tools/call" => {
            // `brain_ping` is answered here, BEFORE the vault gate and
            // before `call_tool` — it is a pure liveness probe and must
            // never touch the filesystem or DB, even when no vault is
            // configured or the disk is hung. This is the contract the
            // 0.2.19 pre-flight probe accidentally broke; keeping ping
            // above the gate is what restores "ping always answers".
            let tool_name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
            if tool_name == "brain_ping" {
                return ok_response(
                    &id,
                    json!({
                        "content": [{ "type": "text", "text": brain_ping_payload() }],
                        "isError": false
                    }),
                );
            }
            match vault {
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
                None => {
                    error_response(&id, -32000, "no Brain vault is mounted on this host", None)
                }
            }
        }
        "ping" => ok_response(&id, json!({})),
        _ => error_response(&id, -32601, &format!("method not found: {}", req.method), None),
    }
}

/// The `brain_ping` payload. Pure in-memory — server identity, compiled
/// version, process uptime. No filesystem, no DB. Shared by the
/// `handle_request` fast path and kept as a function so the contract
/// (zero I/O) is obvious and testable.
fn brain_ping_payload() -> String {
    serde_json::to_string(&json!({
        "status": "ok",
        "server": SERVER_NAME,
        "version": SERVER_VERSION,
        "uptime_seconds": process_uptime_seconds(),
    }))
    .unwrap_or_default()
}

/// Builds (or refreshes) the SQLite index if it's empty. Cheap on small
/// vaults, important for never-mounted-by-GUI vaults so MCP search has
/// real data to query. We deliberately skip a full rebuild when the
/// index already has rows — the GUI's wiki watcher keeps it fresh.
/// Timeout budget for a single DB operation in the MCP subprocess.
/// Strictly greater than `DbHandle`'s `busy_timeout` (5 s) so a
/// legitimate lock wait against the GUI writer is never misread as a
/// wedged disk and abandoned.
const DB_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Runs a DB operation with the full resilience policy. This is the
/// single choke-point every index-backed MCP tool goes through, and it
/// owns the `&mut Option<DbHandle>` so it can drop/reopen the handle:
///
///   1. Open lazily if we hold no handle (and build the index on first
///      open). A failed open returns a clean error string.
///   2. Run `f` on a worker thread bounded by `DB_OP_TIMEOUT`
///      (`with_timeout`). On timeout: abandon the handle (`*db = None`,
///      per the orphaned-thread contract — the wedged worker still holds
///      its mutex) and return `BRAIN_INDEX_TIMEOUT`. No retry: a retry
///      would just wedge again.
///   3. On a fatal connection error (`SQLITE_IOERR` / `NOTADB` /
///      `CANTOPEN` — the stale-handle symptoms), drop + reopen + run
///      `f` exactly once more. This is the transparent self-heal across
///      a disk unplug/replug.
///   4. On a non-fatal error (bad SQL, missing table), return it as-is
///      WITHOUT touching the handle — never reopen-loop on a logic bug.
///
/// `f` must be `Clone` (it may run twice) and `Send + 'static` (it runs
/// on the worker thread and may outlive this frame on timeout).
fn db_op<F, T>(
    db: &mut Option<crate::db::DbHandle>,
    vault: &std::path::Path,
    op_name: &str,
    f: F,
) -> Result<T, String>
where
    F: Fn(&rusqlite::Connection) -> crate::db::DbResult<T> + Clone + Send + 'static,
    T: Send + 'static,
{
    // Ensure we hold a handle (open + first-run index build).
    if db.is_none() {
        match crate::db::DbHandle::open(vault) {
            Ok(handle) => {
                ensure_index_built(&handle, vault);
                *db = Some(handle);
            }
            Err(err) => {
                return Err(format!(
                    "BRAIN index temporarily unavailable ({op_name}): {err}"
                ));
            }
        }
    }
    let handle = db.as_ref().expect("handle present after open").clone();
    match handle.with_timeout(DB_OP_TIMEOUT, f.clone()) {
        Err(crate::db::DbTimeout) => {
            // Abandon the wedged handle — the worker still holds its
            // mutex and would block any future lock on it.
            *db = None;
            Err(format!(
                "BRAIN_INDEX_TIMEOUT: the index did not respond within {}s during {op_name}; \
                 the vault disk may be hung. Try again, or reconnect the drive.",
                DB_OP_TIMEOUT.as_secs()
            ))
        }
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) if crate::db::is_connection_fatal(&err) => {
            // Stale connection — drop, reopen, retry once.
            tracing::warn!(?err, op = op_name, "DB op hit a fatal connection error; reopening");
            *db = None;
            match crate::db::DbHandle::open(vault) {
                Ok(reopened) => {
                    let result = reopened.with_timeout(DB_OP_TIMEOUT, f);
                    *db = Some(reopened);
                    match result {
                        Err(crate::db::DbTimeout) => {
                            *db = None;
                            Err(format!(
                                "BRAIN_INDEX_TIMEOUT: the index did not respond within {}s during \
                                 {op_name} (after reconnect); the vault disk may be hung.",
                                DB_OP_TIMEOUT.as_secs()
                            ))
                        }
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(e)) => Err(format!("{op_name}: {e}")),
                    }
                }
                Err(e) => Err(format!(
                    "BRAIN index unavailable after reconnect ({op_name}): {e}"
                )),
            }
        }
        Ok(Err(err)) => Err(format!("{op_name}: {err}")),
    }
}

/// Normalises a path to forward-slash form for display in tool
/// responses. `Path::join` on Windows mixes separators when the base
/// came from an env var with `/` (e.g. `E:/04_models` joined with
/// `bge-m3` → `E:/04_models\bge-m3`), which the bug report flagged as
/// confusing. Display-only — never use this for actual filesystem
/// access (the real `Path` keeps native separators and resolves fine).
fn display_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

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

/// Cross-platform path equality between a `LintError.path` (which is
/// produced by `Path::to_string_lossy()` and can carry a mix of
/// `/` and `\` separators on Windows) and a freshly-built `Path` from
/// `wiki_dir(vault).join(...)`. Normalising both to forward slashes
/// before comparing handles the Windows case where lint reports paths
/// like `D:/02_wiki\entities\alice.md` while our just-written target
/// uses platform-native separators. Used by the page-scoped lint
/// filter in the `brain_write_page` dispatcher.
fn paths_equal(reported: &str, target: &std::path::Path) -> bool {
    let r = reported.replace('\\', "/");
    let t = target.to_string_lossy().replace('\\', "/");
    r == t
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "brain_ping",
            "description": "Liveness probe — returns server status, version and process uptime in seconds. Does NOT require a mounted vault, so it works even if the disk is disconnected or the indexer is busy. Use this between bulk-ingest batches to detect a stuck server within seconds instead of waiting for the IPC timeout. Never returns an error.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "brain_search",
            "description": "Hybrid lexical + semantic search across the wiki. Combines FTS5 BM25 (matches the surface tokens, handles hyphenation and stemming) with sqlite-vec KNN over bge-m3 embeddings (matches paraphrase / near-synonyms even when no shared word is present) and fuses the two ranked lists via reciprocal-rank fusion. Returns hits sorted by fused score. Note: semantic matching depends on bge-m3 being loaded — call brain_embedding_status to confirm (semantic: true). For structured filters by frontmatter fields (type, tag, created, …) use brain_query instead.",
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
            "name": "brain_get_pages",
            "description": "Bulk-read variant of brain_get_page. Pass an array of ids; the response contains one entry per id in request order, each shaped `{id, found, page?}`. Missing ids are returned as `{id, found: false}` rather than aborting the call — so the agent can decide per-id whether to create-or-skip. Use for refactor sweeps and consistency audits where 5–20 related pages need to be inspected at once.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1
                    }
                },
                "required": ["ids"]
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
            "name": "brain_patch_page",
            "description": "Edit ONE section of an existing page instead of rewriting the whole thing. `heading` is a markdown heading line (e.g. '## Kontakt'); its section — from that heading to the next heading of the same or higher level — is replaced with `content` (the section body, without repeating the heading). If the heading isn't present, the section is appended. Frontmatter is preserved untouched and wiki-links are normalised, so the result equals a full rewrite of that section. Prefer this over brain_write_page for targeted updates: the diff (and, on an encrypted/synced vault, the merge surface) stays tiny. The page must already exist; use brain_write_page to create it. Commit is delegated to the watcher.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "page id, e.g. 'entities/alice'" },
                    "heading": { "type": "string", "description": "the section heading line to replace, e.g. '## Kontakt'" },
                    "content": { "type": "string", "description": "the new section body (markdown, without the heading line)" }
                },
                "required": ["id", "heading", "content"]
            }
        }),
        json!({
            "name": "brain_get_page_history",
            "description": "Return the Git commits that touched a single page, newest first. Each entry has `{sha, ts, message, files_changed}`. Use this together with `brain_restore_page` to roll back a page that was accidentally overwritten or to inspect how a fact changed over time. Walks the repo's revwalk and filters per-commit by diff — work is proportional to commits scanned, not commits matched; the default `limit` (20) is usually enough.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "page id, e.g. 'entities/alice' — `.md` is appended automatically"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "maximum commits to return (default 20)"
                    }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_restore_page",
            "description": "Replace the current content of a page with the version that existed at the given Git sha. Records a `revert: restored <page> from <short-sha>` commit on top so the history stays append-only — nothing is destructively rewritten. Pair with `brain_get_page_history` to discover available shas. Returns the new commit sha on success.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "page id, e.g. 'entities/alice' — `.md` is appended automatically"
                    },
                    "sha": {
                        "type": "string",
                        "description": "Git commit sha (full or short) to restore the page from"
                    }
                },
                "required": ["id", "sha"]
            }
        }),
        json!({
            "name": "brain_write_batch",
            "description": "Atomic multi-page write. Pass `pages: [{id, content}, ...]` — all pages are parsed and normalised first (phase 1; if any one fails to parse, nothing is written), then all are written to disk (phase 2), then lint runs ONCE over the whole vault (phase 3) and the response is scoped to the union of paths in the batch. Use this when several pages reference each other and would cascade broken-link errors if written one-by-one. Response: `{wrote: [{id, previous_size_bytes, new_size_bytes, warnings}]}`. Errors abort with a structured message naming the offending id. Commit is delegated to the watcher (same as brain_write_page).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" }
                            },
                            "required": ["id", "content"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["pages"]
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
            "name": "brain_embedding_status",
            "description": "Report which embedder is currently active for the vault: real bge-m3 semantic vectors (when the model files in `04_models/bge-m3/` are present) or the deterministic HashedEmbedder fallback (no model files → mathematically-valid KNN but no semantic meaning). Returns `{embedder, semantic, model_dir, dim, chunk_count_indexed}`. If `semantic: false`, the hybrid-search semantic pass scores carry no meaning and `brain_search` behaves effectively as FTS5-only. Read this when the agent suspects semantic search isn't working.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "brain_list_tags",
            "description": "Return every distinct tag in the vault with its page count, sorted by count descending (alphabetic on ties). Use this to discover which tags exist before writing a `brain_query tag:<value>` filter — saves the agent from guessing names. Requires the SQLite index to be populated (i.e. the vault was rebuilt at least once after seeding).",
            "inputSchema": {
                "type": "object",
                "properties": {}
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
    db: &mut Option<crate::db::DbHandle>,
) -> Result<String, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'name'".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // NOTE: `brain_ping` is handled upstream in `handle_request`, before
    // the vault gate and before this function — it must never reach the
    // `is_vault` stat below or any DB code. Do not re-add a ping branch
    // here.

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
            if q.trim().is_empty() {
                return Ok(serde_json::to_string_pretty(&Vec::<search::SearchHit>::new())
                    .unwrap_or_default());
            }
            // Run the hybrid (FTS5 + vector) path through db_op so it is
            // timeout-bounded and self-heals on a stale connection. On
            // any DB-side failure (timeout, IOERR-after-reopen, empty
            // index) fall back EXPLICITLY to the filesystem brute-force
            // walker so the LLM still gets results — the same graceful
            // degradation `search_with_db` did internally, but now the
            // timeout boundary lives here where we own `&mut db`.
            let query_owned = q.to_string();
            let vault_owned = vault.to_path_buf();
            let hybrid = db_op(db, vault, "brain_search", move |conn| {
                search::search_hybrid_on_conn(conn, &vault_owned, &query_owned)
                    .map_err(crate::db::DbError::from)
            });
            let hits = match hybrid {
                Ok(hits) if !hits.is_empty() => hits,
                // Empty hybrid result or any DB error → brute-force walk.
                _ => search::search_brute_force(vault, q).map_err(|e| e.to_string())?,
            };
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "brain_get_page" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let page = tree::read_page(vault, id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&page).unwrap_or_default())
        }
        "brain_get_pages" => {
            let ids = args
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing 'ids' array".to_string())?;
            if ids.is_empty() {
                return Err("'ids' must contain at least one entry".to_string());
            }
            // Per-id, never failing the batch. A missing page is data
            // (the agent might want to create it); a corrupt page is
            // also data (the agent might want to fix the frontmatter).
            // Both surface as `found: false` plus an `error` string,
            // so the agent can decide what to do without losing the
            // results for the *other* ids in the same call.
            let pages: Vec<Value> = ids
                .iter()
                .map(|raw| {
                    let id = raw.as_str().unwrap_or("");
                    match tree::read_page(vault, id) {
                        Ok(page) => json!({
                            "id": id,
                            "found": true,
                            "page": page,
                        }),
                        Err(e) => json!({
                            "id": id,
                            "found": false,
                            "error": e.to_string(),
                        }),
                    }
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({ "pages": pages })).unwrap_or_default())
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
            let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
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
            let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Capture pre-write size for the overwrite indicator. If
            // the file didn't exist yet, "previous" is 0 — same shape,
            // no special-case in the response.
            let previous_size_bytes = std::fs::metadata(&target)
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            std::fs::write(&target, &normalized_content).map_err(|e| e.to_string())?;
            let new_size_bytes = normalized_content.len() as i64;

            // Lint runs over the whole vault (every page is parsed and
            // every wiki-link is resolved against the known-id set),
            // but the response is filtered to findings whose `path`
            // matches the just-written file. Before 0.2.17 the full
            // vault state leaked into every write response, which made
            // bulk-ingest sessions:
            //   (a) noisy — old, unrelated broken-link errors from
            //       earlier pages re-appeared in every subsequent
            //       write response,
            //   (b) context-hungry — the LLM agent burnt tokens
            //       re-reading the same stale findings,
            //   (c) hard to act on — the agent had to mentally
            //       separate "errors from this write" vs "errors
            //       that already existed".
            // Now the page-scoped view stays focused on what the
            // current write caused, and the global state is still
            // accessible via the dedicated `brain_lint_report` tool.
            let full_report = lint::lint(vault).map_err(|e| e.to_string())?;
            let page_errors: Vec<&lint::LintError> = full_report
                .errors
                .iter()
                .filter(|e| paths_equal(&e.path, &target))
                .collect();
            let page_warnings: Vec<&lint::LintWarning> = full_report
                .warnings
                .iter()
                .filter(|w| paths_equal(&w.path, &target))
                .collect();
            if !page_errors.is_empty() {
                // Errors on *this* page (broken links, frontmatter
                // problems …) block the operation: the agent gets the
                // structured array so it can repair in one round-trip.
                // Same shape as before 0.2.17 so existing clients
                // continue to parse the response identically.
                let detail = serde_json::to_string(&page_errors)
                    .unwrap_or_else(|_| "[]".to_string());
                return Err(format!(
                    "page written but lint failed: {} error(s) on this page\n{detail}",
                    page_errors.len()
                ));
            }
            // Commit is delegated to the watcher (which debounces by
            // 5 s of idle and lints once over the accumulated changes
            // before committing). Pre-0.2.17 the MCP tool *also*
            // emitted its own `commit_all` here, which double-counted
            // every write: the immediate commit landed first, then the
            // watcher fired its own follow-up commit on the same
            // change set. Letting the watcher own commits cleans up
            // the wiki_history view and is the prerequisite for the
            // upcoming `brain_write_batch` atomic-multi-write tool.
            Ok(serde_json::to_string(&json!({
                "wrote": id,
                "previous_size_bytes": previous_size_bytes,
                "new_size_bytes": new_size_bytes,
                "warnings": page_warnings,
            }))
            .unwrap_or_default())
        }
        "brain_patch_page" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            if id.contains("..") {
                return Err("id may not contain '..'".to_string());
            }
            let heading = args
                .get("heading")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'heading'".to_string())?;
            let section = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'content'".to_string())?;
            // The page must already exist — patch edits one section of it.
            let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
            let original = std::fs::read_to_string(&target)
                .map_err(|_| format!("page not found: {id} (use brain_write_page to create it)"))?;
            let parsed = page::parse(&original)
                .map_err(|e| format!("existing page is malformed, refusing to patch: {e}"))?;
            let patched_body = patch_section(&parsed.body, heading, section);
            // Same link-normalisation + frontmatter-preserving re-stitch as
            // brain_write_page, so a patched page is byte-for-byte what a
            // full rewrite of the same content would produce.
            let normalized_body = page::normalize_internal_links(&patched_body);
            let new_content = rebuild_page_file(&original, &normalized_body);
            write_normalized_page(vault, id, &new_content)
        }
        "brain_get_page_history" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            if id.is_empty() {
                return Err("'id' must not be empty".to_string());
            }
            if id.contains("..") {
                return Err("id may not contain '..'".to_string());
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(20);
            // Normalise page-id (entities/alice) to the on-disk path
            // (entities/alice.md) that git stores. Accepts either form
            // — agents in the wild send both depending on whether
            // they've stripped extensions or not.
            // Accept both `entities/alice` and `entities/alice.md`;
            // route through the resolver so the repo-relative path stays
            // consistent with how pages are actually stored on disk.
            let page_path = crate::wiki::encryption::page_relpath(vault, id.strip_suffix(".md").unwrap_or(id))
                .map_err(|e| e.to_string())?;
            let history = wiki_history::history_for_page(
                &wiki_dir(vault),
                &page_path,
                limit,
            )
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&json!({ "commits": history }))
                .unwrap_or_default())
        }
        "brain_restore_page" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            let sha = args
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'sha'".to_string())?;
            if id.is_empty() {
                return Err("'id' must not be empty".to_string());
            }
            if id.contains("..") {
                return Err("id may not contain '..'".to_string());
            }
            if sha.is_empty() {
                return Err("'sha' must not be empty".to_string());
            }
            // Same id normalisation as brain_get_page_history — route
            // through the resolver for the repo-relative path.
            let page_path = crate::wiki::encryption::page_relpath(vault, id.strip_suffix(".md").unwrap_or(id))
                .map_err(|e| e.to_string())?;
            wiki_history::restore_page(&wiki_dir(vault), sha, &page_path)
                .map_err(|e| e.to_string())?;
            // Report the new revert-commit sha so the agent can quote
            // it back to the user ("restored — new commit a1b2c3d4").
            // The watcher's debounce window might not have produced
            // it yet, so we just confirm the restore wrote the file
            // and return the source sha for the audit trail.
            Ok(serde_json::to_string(&json!({
                "restored": id,
                "from_sha": sha,
            }))
            .unwrap_or_default())
        }
        "brain_write_batch" => {
            // Three phases — see the tool descriptor for the user-
            // facing rationale. Code-side rationale: parsing all
            // pages up front turns multi-page write into an all-or-
            // nothing operation against malformed input. Lint runs
            // once at the end with the full batch already on disk,
            // so intra-batch references resolve (the cascade is
            // gone).
            let pages = args
                .get("pages")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing 'pages' array".to_string())?;
            if pages.is_empty() {
                return Err("'pages' must contain at least one entry".to_string());
            }

            // Phase 1 — validate + buffer.
            struct Prepared {
                id: String,
                target: std::path::PathBuf,
                normalized_content: String,
                previous_size_bytes: i64,
            }
            let mut prepared: Vec<Prepared> = Vec::with_capacity(pages.len());
            for (idx, entry) in pages.iter().enumerate() {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("pages[{idx}]: missing 'id'"))?;
                let content = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("pages[{idx}]: missing 'content'"))?;
                let parsed = page::parse(content)
                    .map_err(|e| format!("pages[{idx}] ({id}): invalid content: {e}"))?;
                let normalized_body = page::normalize_internal_links(&parsed.body);
                let normalized_content = if normalized_body == parsed.body {
                    content.to_string()
                } else {
                    rebuild_page_file(content, &normalized_body)
                };
                let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
                let previous_size_bytes = std::fs::metadata(&target)
                    .map(|m| m.len() as i64)
                    .unwrap_or(0);
                prepared.push(Prepared {
                    id: id.to_string(),
                    target,
                    normalized_content,
                    previous_size_bytes,
                });
            }

            // Phase 2 — write all files. If an IO error hits mid-
            // batch the error names the failing page; the partial
            // state is consciously left as-is so the user can
            // inspect (we deliberately do not rollback the pages
            // that already wrote, which would itself be an IO
            // sequence that can fail).
            for w in &prepared {
                if let Some(parent) = w.target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create dir for {}: {e}", w.id))?;
                }
                std::fs::write(&w.target, &w.normalized_content)
                    .map_err(|e| format!("write {}: {e}", w.id))?;
            }

            // Phase 3 — single lint pass, scoped to the union of
            // touched paths.
            let full_report = lint::lint(vault).map_err(|e| e.to_string())?;
            let target_set: std::collections::HashSet<String> = prepared
                .iter()
                .map(|w| w.target.to_string_lossy().replace('\\', "/"))
                .collect();
            let scoped_errors: Vec<&lint::LintError> = full_report
                .errors
                .iter()
                .filter(|e| target_set.contains(&e.path.replace('\\', "/")))
                .collect();
            let scoped_warnings: Vec<&lint::LintWarning> = full_report
                .warnings
                .iter()
                .filter(|w| target_set.contains(&w.path.replace('\\', "/")))
                .collect();
            if !scoped_errors.is_empty() {
                let detail = serde_json::to_string(&scoped_errors)
                    .unwrap_or_else(|_| "[]".to_string());
                return Err(format!(
                    "batch written ({} pages) but lint failed: {} error(s) across the batch\n{detail}",
                    prepared.len(),
                    scoped_errors.len()
                ));
            }
            // Per-page summary including page-scoped warnings.
            let results: Vec<Value> = prepared
                .iter()
                .map(|w| {
                    let new_size_bytes = w.normalized_content.len() as i64;
                    let page_warnings: Vec<&lint::LintWarning> = scoped_warnings
                        .iter()
                        .copied()
                        .filter(|wn| paths_equal(&wn.path, &w.target))
                        .collect();
                    json!({
                        "id": w.id,
                        "previous_size_bytes": w.previous_size_bytes,
                        "new_size_bytes": new_size_bytes,
                        "warnings": page_warnings,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({ "wrote": results })).unwrap_or_default())
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
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let hits = db_op(db, vault, "brain_query", move |conn| {
                crate::viewer::query::executor::run_on_conn(conn, &query).map_err(|e| match e {
                    crate::viewer::query::executor::ExecError::Db(r) => crate::db::DbError::from(r),
                    // Parse errors are not DB errors — surface them as an
                    // Io-wrapped string so db_op returns them verbatim
                    // (and never reopen-loops on a bad query).
                    other => crate::db::DbError::Io(std::io::Error::other(other.to_string())),
                })
            })?;
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "brain_embedding_status" => {
            // Read which embedder `for_vault` would build for this
            // vault right now — same code path as the indexer, so the
            // status reflects what's actually generating chunk
            // vectors. Cheap (no model load when files are absent).
            let embedder = crate::embedding::for_vault(vault);
            let model_dir = crate::vault::layout::models_dir(vault).join("bge-m3");
            let semantic = embedder.name() == "bge-m3";
            // Chunk count via db_op (timeout-bounded, self-healing).
            // Reported as a number on success, or an explicit
            // `{ "error": "<msg>" }` object on failure — never a silent
            // `null` (which the bug report flagged as indistinguishable
            // from "index genuinely empty").
            let chunk_count_indexed = match db_op(db, vault, "brain_embedding_status", |conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0))?)
            }) {
                Ok(n) => json!(n),
                Err(msg) => json!({ "error": msg }),
            };
            Ok(serde_json::to_string_pretty(&json!({
                "embedder": embedder.name(),
                "semantic": semantic,
                "model_dir": display_path(&model_dir),
                "dim": embedder.dim(),
                "chunk_count_indexed": chunk_count_indexed,
            }))
            .unwrap_or_default())
        }
        "brain_list_tags" => {
            let rows = db_op(db, vault, "brain_list_tags", |conn| {
                let mut stmt = conn.prepare(
                    "SELECT tag, COUNT(*) AS count FROM page_tags \
                     GROUP BY tag ORDER BY count DESC, tag ASC",
                )?;
                let mapped: Result<Vec<(String, i64)>, _> = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect();
                Ok(mapped?)
            })?;
            let tags: Vec<Value> = rows
                .into_iter()
                .map(|(tag, count)| json!({ "tag": tag, "count": count }))
                .collect();
            Ok(serde_json::to_string_pretty(&json!({ "tags": tags })).unwrap_or_default())
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
    db: &mut Option<crate::db::DbHandle>,
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

    // Collect (bucket, id) pairs. Prefer the DB fastpath (timeout-
    // bounded + self-healing via db_op); on ANY DB failure (timeout,
    // IOERR-after-reopen, unindexed vault) fall back to the filesystem
    // walk so the listing still works while the index is unavailable.
    let pairs: Vec<(String, String)> =
        match db_op(db, vault, "brain_list_pages", list_page_ids_on_conn) {
            Ok(pairs) => pairs,
            Err(_) => list_page_ids_via_filesystem(vault).map_err(|e| e.to_string())?,
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
fn list_page_ids_on_conn(
    conn: &rusqlite::Connection,
) -> crate::db::DbResult<Vec<(String, String)>> {
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

/// Replace the section under `heading` in a page `body` with `new_content`.
/// A "section" runs from the line equal to `heading` (after trimming) up to
/// the next heading of the SAME OR HIGHER level (same-or-fewer `#`), or end
/// of body. `heading` must be a markdown heading line, e.g. `## Kontakt`.
/// If the heading isn't present, the section is appended at the end. Pure +
/// testable; the caller re-normalises links and re-stitches frontmatter.
///
/// This is the core of `brain_patch_page`: editing one section produces a
/// small, local diff instead of a whole-page rewrite — fewer bytes to
/// re-encrypt and far fewer sync merge conflicts.
fn patch_section(body: &str, heading: &str, new_content: &str) -> String {
    let heading = heading.trim();
    let target_level = heading.chars().take_while(|c| *c == '#').count();
    // A body line is a heading iff it is one-or-more `#` followed by a space.
    let heading_level = |line: &str| -> Option<usize> {
        let t = line.trim_start();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && t[hashes..].starts_with(' ') {
            Some(hashes)
        } else {
            None
        }
    };
    let lines: Vec<&str> = body.lines().collect();
    let new_section = format!("{heading}\n\n{}", new_content.trim_end_matches('\n'));

    match lines.iter().position(|l| l.trim() == heading) {
        Some(start) => {
            // End at the next heading of level <= target_level, else EOF.
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find_map(|(j, l)| heading_level(l).filter(|lvl| *lvl <= target_level).map(|_| j))
                .unwrap_or(lines.len());
            let mut out = String::new();
            for l in &lines[..start] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(&new_section);
            out.push('\n');
            if end < lines.len() {
                out.push('\n');
                for l in &lines[end..] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out
        }
        None => {
            let mut out = body.trim_end_matches('\n').to_string();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&new_section);
            out.push('\n');
            out
        }
    }
}

/// Write `normalized_content` to the page's (opaque-aware) path, then run
/// the page-scoped lint and build the standard write response. Mirrors the
/// write+lint tail of `brain_write_page`; shared with `brain_patch_page`.
/// Commit is left to the watcher.
fn write_normalized_page(
    vault: &std::path::Path,
    id: &str,
    normalized_content: &str,
) -> Result<String, String> {
    let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let previous_size_bytes = std::fs::metadata(&target).map(|m| m.len() as i64).unwrap_or(0);
    std::fs::write(&target, normalized_content).map_err(|e| e.to_string())?;
    let new_size_bytes = normalized_content.len() as i64;

    let full_report = lint::lint(vault).map_err(|e| e.to_string())?;
    let page_errors: Vec<&lint::LintError> = full_report
        .errors
        .iter()
        .filter(|e| paths_equal(&e.path, &target))
        .collect();
    let page_warnings: Vec<&lint::LintWarning> = full_report
        .warnings
        .iter()
        .filter(|w| paths_equal(&w.path, &target))
        .collect();
    if !page_errors.is_empty() {
        let detail = serde_json::to_string(&page_errors).unwrap_or_else(|_| "[]".to_string());
        return Err(format!(
            "page written but lint failed: {} error(s) on this page\n{detail}",
            page_errors.len()
        ));
    }
    Ok(serde_json::to_string(&json!({
        "wrote": id,
        "previous_size_bytes": previous_size_bytes,
        "new_size_bytes": new_size_bytes,
        "warnings": page_warnings,
    }))
    .unwrap_or_default())
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

    #[test]
    fn patch_section_replaces_only_the_named_section() {
        let body = "# Title\n\nIntro.\n\n## Kontakt\n\nalt\n\n## Andere\n\nbleibt\n";
        let out = patch_section(body, "## Kontakt", "neu");
        assert!(out.contains("## Kontakt\n\nneu"), "section replaced: {out}");
        assert!(!out.contains("alt"), "old section body gone: {out}");
        assert!(out.contains("Intro."), "content before the section preserved");
        assert!(out.contains("## Andere\n\nbleibt"), "later section untouched: {out}");
    }

    #[test]
    fn patch_section_appends_when_heading_absent() {
        let body = "# Title\n\nIntro.\n";
        let out = patch_section(body, "## Neu", "inhalt");
        assert!(out.contains("Intro."), "existing content kept");
        assert!(out.trim_end().ends_with("## Neu\n\ninhalt"), "new section appended: {out}");
    }

    #[test]
    fn patch_section_stops_at_same_level_heading_but_includes_deeper_ones() {
        // A `## X` section should swallow a `### sub` but stop at the next `##`.
        let body = "## X\n\nold\n\n### sub\n\nsubtext\n\n## Y\n\nyeahs\n";
        let out = patch_section(body, "## X", "replaced");
        assert!(out.contains("## X\n\nreplaced"), "X replaced");
        assert!(!out.contains("### sub"), "deeper subsection was part of X and is gone: {out}");
        assert!(!out.contains("subtext"), "sub content gone");
        assert!(out.contains("## Y\n\nyeahs"), "sibling section Y preserved: {out}");
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
        let resp = handle_request(&req, None, &mut None);
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
        let resp = handle_request(&req, None, &mut None);
        for name in [
            "brain_ping",
            "brain_search",
            "brain_get_page",
            "brain_get_pages",
            "brain_page_exists",
            "brain_get_context",
            "brain_list_pages",
            "brain_write_page",
            "brain_patch_page",
            "brain_write_batch",
            "brain_write_raw_file",
            "brain_get_page_history",
            "brain_restore_page",
            "brain_graph",
            "brain_query",
            "brain_list_tags",
            "brain_embedding_status",
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
        let resp = handle_request(&req, None, &mut None);
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
        let resp = handle_request(&req, None, &mut None);
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
        let resp = handle_request(&req, None, &mut None);
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
        let resp = handle_request(&req, None, &mut None);
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
        let resp = handle_request(&req, None, &mut None);
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
            &mut None,
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
            &mut None,
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
        let mut db = Some(db);

        let result = call_tool(
            &json!({
                "name": "brain_search",
                "arguments": { "query": "spec driven development" }
            }),
            tmp.path(),
            &mut db,
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
            &mut None,
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
        db: Option<crate::db::DbHandle>,
    ) -> Value {
        let mut db = db;
        let result = call_tool(
            &json!({ "name": "brain_list_pages", "arguments": args }),
            vault,
            &mut db,
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
        let db_result = list_pages_call(tmp.path(), json!({}), Some(db));

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
            &mut None,
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
            &mut None,
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
            &mut None,
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
            &mut None,
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
            &mut None,
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
            &mut None,
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
    fn brain_get_page_history_returns_only_commits_that_touched_the_named_page() {
        // The roll-back workflow: agent calls brain_get_page_history,
        // sees the candidate revisions, then brain_restore_page picks
        // one. The MCP path normalizes the page-id (`entities/alice`)
        // into the on-disk path (`entities/alice.md`) before handing
        // off to the backend.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        // Use the same git path the wiki watcher uses, so committed
        // history is reachable through the MCP-side wrapper.
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        let entities = wiki.join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        std::fs::write(entities.join("alice.md"), "alice v1").unwrap();
        crate::wiki::git::commit_all(&wiki, "alice v1").unwrap();
        std::fs::write(entities.join("bob.md"), "bob v1").unwrap();
        crate::wiki::git::commit_all(&wiki, "bob v1 (noise)").unwrap();
        std::fs::write(entities.join("alice.md"), "alice v2").unwrap();
        crate::wiki::git::commit_all(&wiki, "alice v2").unwrap();

        let ok = call_tool(
            &json!({
                "name": "brain_get_page_history",
                "arguments": { "id": "entities/alice" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_get_page_history must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("JSON");
        let commits = parsed.get("commits").and_then(|v| v.as_array()).expect("commits");
        assert_eq!(commits.len(), 2, "two alice-only commits expected, got {commits:?}");
        for c in commits {
            let msg = c.get("message").and_then(|v| v.as_str()).unwrap_or("");
            assert!(msg.contains("alice"), "non-alice commit leaked: {msg}");
        }
        // bob's commit must NOT have leaked in.
        assert!(
            !ok.contains("bob v1 (noise)"),
            "bob's noise commit must not surface in alice's history: {ok}"
        );
    }

    #[test]
    fn brain_restore_page_replaces_current_content_with_the_old_revision() {
        // End-to-end of the rollback: write v1, write v2 over it,
        // call brain_restore_page with v1's sha, assert the file on
        // disk matches v1 again. The new revert commit records the
        // action so the history stays append-only.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        let entities = wiki.join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        std::fs::write(entities.join("alice.md"), "alice v1").unwrap();
        let v1_sha = crate::wiki::git::commit_all(&wiki, "alice v1")
            .unwrap()
            .expect("v1 commit sha");
        std::fs::write(entities.join("alice.md"), "alice v2").unwrap();
        crate::wiki::git::commit_all(&wiki, "alice v2").unwrap();

        let ok = call_tool(
            &json!({
                "name": "brain_restore_page",
                "arguments": { "id": "entities/alice", "sha": v1_sha }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_restore_page must succeed");
        // The response surfaces the source sha so the agent can quote
        // it back to the user.
        assert!(ok.contains(&v1_sha), "response should mention source sha: {ok}");
        // The file on disk is now v1's content again.
        let after = std::fs::read_to_string(entities.join("alice.md")).unwrap();
        assert_eq!(after, "alice v1");
    }

    #[test]
    fn brain_restore_page_rejects_path_traversal_in_id() {
        // Same hardening as brain_page_exists / brain_write_raw_file:
        // an id with `..` could resolve outside the wiki root once
        // joined onto wiki_dir(vault). Reject before reaching git.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_restore_page",
                "arguments": { "id": "../../../etc/passwd", "sha": "deadbeef" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("traversal id must be rejected");
        assert!(err.contains(".."));
    }

    #[test]
    fn brain_write_batch_writes_all_pages_atomically_and_lints_once_at_the_end() {
        // The painful case the user described: writing 10 interlinked
        // pages one-by-one cascades lint errors because the *first*
        // page references the *third* (and intermediates), so each
        // intermediate write reports a broken-link error on a target
        // that the very next call would have created.
        // batch-write avoids the cascade: phase 1 validates + buffers
        // all pages, phase 2 writes them all, phase 3 runs lint once
        // over the union of touched paths. If every reference resolves
        // *within the batch*, no broken-link errors surface.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());

        // Three pages forming a tight cycle: a→b, b→c, c→a. Single-
        // write order would cascade no matter what, so this is the
        // worst-case for the old write_page flow.
        let ok = call_tool(
            &json!({
                "name": "brain_write_batch",
                "arguments": {
                    "pages": [
                        {
                            "id": "entities/a",
                            "content": "---\nid: entities/a\ntype: entity\ntitle: A\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinks to [[entities/b]].\n"
                        },
                        {
                            "id": "entities/b",
                            "content": "---\nid: entities/b\ntype: entity\ntitle: B\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinks to [[entities/c]].\n"
                        },
                        {
                            "id": "entities/c",
                            "content": "---\nid: entities/c\ntype: entity\ntitle: C\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinks back to [[entities/a]].\n"
                        }
                    ]
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("batch with self-resolving links must succeed");

        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        let wrote = parsed.get("wrote").and_then(|v| v.as_array()).expect("wrote array");
        assert_eq!(wrote.len(), 3, "one summary per page in the batch");
        // Each entry carries the new/previous size so the agent can
        // self-check for accidental shrink even in batch context.
        for entry in wrote {
            assert!(entry.get("id").and_then(|v| v.as_str()).is_some());
            assert!(entry.get("new_size_bytes").and_then(|v| v.as_i64()).is_some());
            assert!(entry.get("previous_size_bytes").and_then(|v| v.as_i64()).is_some());
        }
    }

    #[test]
    fn brain_write_batch_rejects_the_whole_batch_when_any_single_page_fails_to_parse() {
        // Strict atomicity on the validation phase: if any entry in
        // the batch has invalid frontmatter, nothing gets written.
        // Otherwise the user would end up with a half-written batch
        // and would need a partial-rollback heuristic to recover.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());

        let err = call_tool(
            &json!({
                "name": "brain_write_batch",
                "arguments": {
                    "pages": [
                        {
                            "id": "entities/good",
                            "content": "---\nid: entities/good\ntype: entity\ntitle: Good\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nFine.\n"
                        },
                        {
                            "id": "entities/bad",
                            "content": "no frontmatter at all, this should reject the whole batch"
                        }
                    ]
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("malformed page must abort the whole batch");
        assert!(err.contains("entities/bad"), "error should name the offending id: {err}");
        // Neither file may have been written to disk — phase 1
        // validation runs entirely in memory before phase 2 writes.
        assert!(
            !wiki_dir(tmp.path()).join("entities/good.md").exists(),
            "good page must not be written when sibling fails parse — \"atomic\" is the contract"
        );
    }

    #[test]
    fn brain_embedding_status_distinguishes_hashed_fallback_from_real_bge_m3() {
        // The user couldn't tell whether the hybrid search was
        // running on real bge-m3 semantic vectors or on the
        // deterministic HashedEmbedder fallback — the latter gives
        // mathematically-valid KNN scores but no semantic meaning,
        // so the agent's "this query should have matched semantically"
        // intuition silently breaks. The fresh-vault test path here
        // exercises the no-model-files case where the fallback is
        // expected, and asserts the response makes that visible.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());

        let ok = call_tool(
            &json!({ "name": "brain_embedding_status", "arguments": {} }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_embedding_status must succeed regardless of model presence");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        // No model files on a fresh vault → embedder name reports
        // the hashed fallback.
        assert_eq!(
            parsed.get("embedder").and_then(|v| v.as_str()),
            Some("hashed-fh-1024"),
            "fresh vault must report the hashed fallback (no bge-m3 files yet)"
        );
        // The `semantic` flag is the human-readable summary the
        // agent (and the Settings UI later) keys off: true means
        // bge-m3 is loaded, false means the response above is just
        // a deterministic hash and `brain_search` semantic-pass
        // scores carry no meaning.
        assert_eq!(
            parsed.get("semantic").and_then(|v| v.as_bool()),
            Some(false),
            "hashed fallback must report semantic: false"
        );
        // Model path is reported so the user can find where to drop
        // the weights if they want real semantic search.
        let model_dir = parsed
            .get("model_dir")
            .and_then(|v| v.as_str())
            .expect("model_dir must be present");
        assert!(
            model_dir.contains("bge-m3"),
            "model_dir should point at the bge-m3 subfolder, got: {model_dir}"
        );
        // Embedding dimension stays at 1024 across both embedders so
        // the vector index doesn't need re-shape on model swap.
        assert_eq!(
            parsed.get("dim").and_then(|v| v.as_i64()),
            Some(1024),
            "dim is 1024 across both embedders"
        );
        // chunk_count_indexed must NEVER be a silent null — on a fresh
        // (but openable) vault db_op opens lazily and the count is a
        // number (0). The bug report flagged null as indistinguishable
        // from "index genuinely empty"; we now always give a number or
        // an explicit {error} object.
        let ci = parsed.get("chunk_count_indexed").expect("field present");
        assert!(
            ci.is_number() || ci.get("error").is_some(),
            "chunk_count_indexed must be a number or an error object, never null: {ci}"
        );
    }

    #[test]
    fn brain_list_tags_returns_distinct_tags_with_counts_sorted_by_frequency() {
        // The user couldn't discover which tags exist in the vault.
        // `brain_query tag:foo` accepts an exact tag operator, but
        // there was no way to ask "what are the candidate values?".
        // This tool reads `page_tags` and returns each distinct tag
        // with how many pages carry it, sorted descending so the
        // agent sees the most-used tags first.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        // Three pages: two carry `customer`, one carries `partner`,
        // one is untagged. Expected result: `customer` first (2),
        // then `partner` (1). Untagged page contributes nothing.
        std::fs::write(
            entities.join("a.md"),
            "---\nid: entities/a\ntype: entity\ntitle: A\ntags: [customer, dax]\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nA.\n",
        )
        .unwrap();
        std::fs::write(
            entities.join("b.md"),
            "---\nid: entities/b\ntype: entity\ntitle: B\ntags: [customer]\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nB.\n",
        )
        .unwrap();
        std::fs::write(
            entities.join("c.md"),
            "---\nid: entities/c\ntype: entity\ntitle: C\ntags: [partner]\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nC.\n",
        )
        .unwrap();
        std::fs::write(
            entities.join("d.md"),
            "---\nid: entities/d\ntype: entity\ntitle: D\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nD untagged.\n",
        )
        .unwrap();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();
        let mut db = Some(db);

        let ok = call_tool(
            &json!({ "name": "brain_list_tags", "arguments": {} }),
            tmp.path(),
            &mut db,
        )
        .expect("brain_list_tags must succeed on a populated vault");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        let tags = parsed.get("tags").and_then(|v| v.as_array()).expect("tags array");
        // We tagged: customer (2), partner (1), dax (1). Order by
        // count desc, then alphabetic for tie-breaking — that means
        // `customer` first, then `dax` or `partner` next (the
        // SQL ORDER BY tag ASC for stable tie-break — assert
        // alphabetical for the two singletons).
        assert!(
            tags.len() >= 3,
            "at least three distinct tags expected, got {}: {:?}",
            tags.len(),
            tags
        );
        assert_eq!(tags[0].get("tag").and_then(|v| v.as_str()), Some("customer"));
        assert_eq!(tags[0].get("count").and_then(|v| v.as_i64()), Some(2));
        // The two singletons follow, in alphabetic order on ties.
        let next_names: Vec<&str> = tags
            .iter()
            .skip(1)
            .take(2)
            .filter_map(|v| v.get("tag").and_then(|t| t.as_str()))
            .collect();
        assert_eq!(next_names, vec!["dax", "partner"], "alphabetic tie-break on count == 1");
    }

    #[test]
    fn brain_get_pages_returns_results_for_existing_ids_and_marks_missing_ones() {
        // Bulk-read use case: refactor sweeps where the agent wants to
        // inspect 10–20 related pages at once. Pre-0.2.17 the only
        // option was N sequential `brain_get_page` calls, which
        // serialised wall-clock time on the MCP transport. Now one
        // call returns an array of `{id, found, page?, error?}` so the
        // agent can branch on each entry without round-trips.
        // Missing ids must NOT abort the whole call — return them
        // marked `found: false` so the agent can decide per-id
        // whether to create-or-skip.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        std::fs::write(
            entities.join("alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nAlice body.\n",
        )
        .unwrap();
        std::fs::write(
            entities.join("bob.md"),
            "---\nid: entities/bob\ntype: entity\ntitle: Bob\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nBob body.\n",
        )
        .unwrap();

        let ok = call_tool(
            &json!({
                "name": "brain_get_pages",
                "arguments": {
                    "ids": ["entities/alice", "entities/missing", "entities/bob"]
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_get_pages must succeed even with mixed found/missing");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        let pages = parsed.get("pages").and_then(|v| v.as_array()).expect("pages array");
        assert_eq!(pages.len(), 3, "one entry per requested id, in request order");
        assert_eq!(pages[0].get("id").and_then(|v| v.as_str()), Some("entities/alice"));
        assert_eq!(pages[0].get("found").and_then(|v| v.as_bool()), Some(true));
        assert!(pages[0].get("page").is_some(), "found entries carry the page payload");
        assert_eq!(pages[1].get("id").and_then(|v| v.as_str()), Some("entities/missing"));
        assert_eq!(pages[1].get("found").and_then(|v| v.as_bool()), Some(false));
        assert!(pages[1].get("page").is_none(), "missing entries omit the page payload");
        assert_eq!(pages[2].get("id").and_then(|v| v.as_str()), Some("entities/bob"));
        assert_eq!(pages[2].get("found").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn db_op_opens_a_handle_lazily_when_none_is_held() {
        // Cold start / first DB call: db starts None, vault present →
        // db_op opens the handle, runs the op, leaves the handle cached.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let mut db: Option<crate::db::DbHandle> = None;
        let n = db_op(&mut db, tmp.path(), "test", |conn| {
            Ok(conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)
        })
        .expect("db_op opens lazily and runs the op");
        assert_eq!(n, 1);
        assert!(db.is_some(), "handle must be cached after first op");
    }

    #[test]
    fn db_op_keeps_the_handle_on_a_non_fatal_sql_error() {
        // A logic error (missing table) is NOT a dead connection — it
        // must surface as an error WITHOUT dropping/reopening the
        // handle (otherwise a bad query would trigger an endless
        // reopen loop).
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let mut db: Option<crate::db::DbHandle> =
            Some(crate::db::DbHandle::open(tmp.path()).unwrap());
        let err = db_op(&mut db, tmp.path(), "test", |conn| {
            conn.query_row("SELECT * FROM table_that_does_not_exist", [], |_| Ok(()))?;
            Ok(())
        })
        .expect_err("missing table is an error");
        assert!(err.contains("test"), "error names the op: {err}");
        assert!(
            db.is_some(),
            "a non-fatal SQL error must not drop the handle (no reopen-loop)"
        );
    }

    #[test]
    fn brain_ping_is_answered_without_touching_the_vault_or_db() {
        // The core regression guard for the 0.2.20 fix: ping is served
        // by handle_request BEFORE the vault gate. Even with a vault
        // path that is NOT a vault and no DB handle, ping must return
        // a clean status payload — never BRAIN_VAULT_DISCONNECTED, and
        // never reaching is_vault or any DB code (which could hang on a
        // stale disk).
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "tools/call".into(),
            params: json!({ "name": "brain_ping", "arguments": {} }),
        };
        let resp = handle_request(
            &req,
            Some(std::path::Path::new("/path/that/is/not/a/vault")),
            &mut None,
        );
        assert!(
            !resp.contains("BRAIN_VAULT_DISCONNECTED"),
            "ping must not hit the vault-disconnect guard: {resp}"
        );
        // Unwrap the JSON-RPC envelope → result.content[0].text → ping payload.
        let env: serde_json::Value = serde_json::from_str(&resp).expect("envelope is JSON");
        let text = env["result"]["content"][0]["text"]
            .as_str()
            .expect("ping payload text present");
        let ping: serde_json::Value = serde_json::from_str(text).expect("ping payload is JSON");
        assert_eq!(ping["status"], "ok");
        assert_eq!(ping["server"], "BRAIN");
        assert_eq!(ping["version"], env!("CARGO_PKG_VERSION"));
        assert!(ping["uptime_seconds"].as_u64().is_some());
    }

    #[test]
    fn write_page_response_scopes_lint_to_the_just_written_page_not_the_whole_vault() {
        // Regression target: pre-0.2.17 the response of `brain_write_page`
        // surfaced *every* lint error in the vault, even those produced
        // by other pages the agent wrote calls earlier in the session.
        // This made the response noise-heavy and burnt context during
        // bulk ingest. New contract: when a write succeeds (no errors
        // on the page itself), the agent gets a structured success
        // response with `warnings` scoped to the current page only.
        // Global state stays accessible via `brain_lint_report`.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        // Seed a *pre-existing* page elsewhere in the vault that has
        // a broken link. This is the noise we want filtered out of the
        // alice write response.
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        std::fs::write(
            entities.join("bob.md"),
            "---\nid: entities/bob\ntype: entity\ntitle: Bob\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nLinked to [[entities/nonexistent-from-an-earlier-write]].\n",
        )
        .unwrap();

        let ok = call_tool(
            &json!({
                "name": "brain_write_page",
                "arguments": {
                    "id": "entities/alice",
                    "content": "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nA clean page.\n"
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("alice writes cleanly even when bob has a broken link");

        // Bob's broken link must NOT show up in alice's write response.
        assert!(
            !ok.contains("nonexistent-from-an-earlier-write"),
            "alice's response leaked bob's lint error: {ok}"
        );
        // Response must still confirm the write succeeded.
        assert!(
            ok.contains("entities/alice"),
            "success response should name the page: {ok}"
        );
    }

    #[test]
    fn write_page_response_carries_previous_and_new_size_for_overwrite_safety() {
        // The agent (and the human reviewing logs) needs a fast way to
        // notice "I just overwrote a 4 KB rich page with 200 B of
        // sparse content". Carrying both sizes in the success payload
        // lets the agent self-check without an extra round-trip.
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        // Seed an existing rich page.
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        let rich =
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\n";
        let body = "Body line that repeats for a while. ".repeat(50);
        std::fs::write(entities.join("alice.md"), format!("{rich}{body}\n")).unwrap();

        // Overwrite with thinner content.
        let ok = call_tool(
            &json!({
                "name": "brain_write_page",
                "arguments": {
                    "id": "entities/alice",
                    "content": "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nTiny.\n"
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("write succeeds");

        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        let prev = parsed.get("previous_size_bytes").and_then(|v| v.as_i64()).expect("previous_size_bytes");
        let new = parsed.get("new_size_bytes").and_then(|v| v.as_i64()).expect("new_size_bytes");
        assert!(prev > new, "previous ({prev}) must exceed new ({new}) for this shrink test");
        assert!(prev > 1000, "previous size sanity (got {prev})");
        assert!(new < 200, "new size sanity (got {new})");
    }

    #[test]
    fn patch_page_replaces_one_section_and_preserves_the_rest() {
        use tempfile::TempDir;
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        let original = "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\n# Alice\n\nIntro.\n\n## Kontakt\n\nalte Nummer\n\n## Notizen\n\nbleibt\n";
        std::fs::write(entities.join("alice.md"), original).unwrap();

        let ok = call_tool(
            &json!({
                "name": "brain_patch_page",
                "arguments": {
                    "id": "entities/alice",
                    "heading": "## Kontakt",
                    "content": "neue Nummer +49 201 0"
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("patch succeeds");
        assert!(ok.contains("entities/alice"), "response names the page: {ok}");

        let after = std::fs::read_to_string(entities.join("alice.md")).unwrap();
        assert!(after.starts_with("---\nid: entities/alice"), "frontmatter preserved: {after}");
        assert!(after.contains("## Kontakt\n\nneue Nummer +49 201 0"), "section replaced: {after}");
        assert!(!after.contains("alte Nummer"), "old section gone: {after}");
        assert!(after.contains("Intro."), "intro preserved: {after}");
        assert!(after.contains("## Notizen\n\nbleibt"), "sibling section preserved: {after}");
    }

    #[test]
    fn patch_page_errors_when_the_page_does_not_exist() {
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_patch_page",
                "arguments": { "id": "entities/ghost", "heading": "## X", "content": "y" }
            }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("page not found"), "patch on a missing page must fail clearly: {err}");
    }

    #[test]
    fn write_page_response_includes_page_scoped_warnings_when_present() {
        // Warning-level findings (e.g. missing-title) on the just-
        // written page must surface in the success response so the
        // agent can self-correct on the next round-trip without an
        // extra brain_lint_report call. The page itself still
        // writes successfully — warnings do not block.
        use tempfile::TempDir;
        use crate::vault::layout::ensure_skeleton;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        // Omit `title:` from the frontmatter — that is the canonical
        // example of a warning that does not block the commit.
        let ok = call_tool(
            &json!({
                "name": "brain_write_page",
                "arguments": {
                    "id": "entities/alice",
                    "content": "---\nid: entities/alice\ntype: entity\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nBody.\n"
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("missing-title is a warning, write must succeed");
        assert!(
            ok.contains("missing-title"),
            "response must surface the missing-title warning for the just-written page, got: {ok}"
        );
    }

    #[test]
    fn write_page_rejects_unregistered_type_with_actionable_error_message() {
        // Promoted from Warning to Error in 0.2.17 (see lint.rs
        // tests). At the MCP level this means write_page returns
        // Err — not Ok with a warnings payload — so an agent that
        // wrote the wrong type can't proceed without correcting it.
        // The error string is the only signal the LLM gets, so it
        // must spell out both the offending value AND the four
        // valid singular forms.
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
                    "content": "---\nid: entities/alice\ntype: entities\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\nBody.\n"
                }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("plural type must surface as a hard error, not a warning");
        assert!(err.contains("unregistered-type"), "error must name the lint kind: {err}");
        assert!(err.contains("entities"), "error must echo the offending value: {err}");
        // The four singular forms must be in the message so the
        // agent doesn't have to fetch them from a doc tool.
        for valid in &["entity", "concept", "source", "topic"] {
            assert!(err.contains(valid), "valid type '{valid}' missing in error: {err}");
        }
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
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains(".."));
    }
}

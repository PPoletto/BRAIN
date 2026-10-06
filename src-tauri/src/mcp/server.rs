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
use crate::wiki::{duplicates, history as wiki_history, lint, page, refactor};

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
///
/// Each tick also drops the bge-m3 model if no search used it for
/// `EMBEDDER_IDLE_TTL` (logged at info level by `evict_idle_embedders`).
/// With the 10-minute tick the effective idle window is 15–25 minutes.
/// Eviction runs before the RSS sample so the heartbeat reflects it.
fn spawn_health_heartbeat() {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let me = Pid::from_u32(std::process::id());
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(600));
            crate::embedding::evict_idle_embedders(crate::embedding::EMBEDDER_IDLE_TTL);
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

    // The embedding model is deliberately NOT warmed up here. Every MCP
    // client (Claude Desktop, Claude Code, Cursor, ...) spawns its own
    // `brain mcp` process, and an eager bge-m3 load would cost ~2.2 GB
    // of RAM (F32 weights) per process even if that client never
    // searches. The model loads lazily on the first `brain_search` (via
    // the process cache in `embedding::cached_for_vault`); later
    // searches reuse it.

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // One log line per process naming the protocol version + client the
    // peer declared (see `client_declaration`). Flips to true after the
    // first declaring request, so the extra `Value` parse below runs for
    // at most the first few lines — never for the bulk of tool calls.
    let mut client_declaration_logged = false;

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
        // Cheap substring pre-filter: both the legacy `initialize` params
        // and the 2026-07-28 `_meta` key contain "protocolVersion".
        if !client_declaration_logged && line.contains("protocolVersion") {
            if let Some(decl) = serde_json::from_str::<Value>(&line)
                .ok()
                .as_ref()
                .and_then(client_declaration)
            {
                decl.log();
                client_declaration_logged = true;
            }
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
        "server/discover" => {
            tracing::info!(
                "mcp: client probed server/discover (MCP 2026-07-28) — answering -32601 so a \
                 dual-era client falls back to the initialize handshake"
            );
            error_response(&id, -32601, &server_discover_unsupported_message(), None)
        }
        _ => error_response(&id, -32601, &format!("method not found: {}", req.method), None),
    }
}

/// The 2026-07-28 MCP revision ("stateless") added `server/discover`. We
/// still speak the legacy, `initialize`-based revision, so we answer the
/// probe with a plain `-32601`. Per the 2026-07-28 stdio binding
/// ("Backward Compatibility"), a dual-era client treats any error that is
/// NOT a recognised modern error (e.g. `-32022` UnsupportedProtocolVersion)
/// as "legacy server" and falls back to `initialize` — so this code must
/// stay a non-modern one. The message only makes the failure diagnosable
/// for a modern-only client and in its log.
fn server_discover_unsupported_message() -> String {
    format!(
        "server/discover (MCP 2026-07-28) not yet supported — this server speaks \
         {PROTOCOL_VERSION}; please use the initialize handshake"
    )
}

/// What a client declared about itself on the wire: the protocol version
/// it speaks and its `clientInfo`. Logged once per process so a client
/// switching protocol revisions shows up in the MCP log instead of as a
/// silent failure.
#[derive(Debug, PartialEq)]
struct ClientDeclaration {
    /// `"initialize"` (legacy handshake) or `"per-request"` (2026-07-28
    /// stateless style, version carried on an ordinary request).
    style: &'static str,
    method: String,
    protocol_version: String,
    client_name: String,
    client_version: String,
}

impl ClientDeclaration {
    fn log(&self) {
        tracing::info!(
            style = self.style,
            method = %self.method,
            client_protocol_version = %self.protocol_version,
            client_name = %self.client_name,
            client_version = %self.client_version,
            server_protocol_version = PROTOCOL_VERSION,
            "mcp: client declared its protocol version"
        );
    }
}

/// Extracts the client's declared protocol version + identity from a raw
/// JSON-RPC envelope. Null-safe: missing fields become `"<none>"`.
///
/// - `initialize` (legacy): `params.protocolVersion`, `params.clientInfo`.
///   Always yields a declaration, even with empty params.
/// - any other request (2026-07-28 style): the spec location is
///   `params._meta["io.modelcontextprotocol/protocolVersion"]` (+
///   `…/clientInfo`); `params.protocolVersion` and a top-level
///   `protocolVersion` are accepted too, for non-conforming clients.
///   Yields `None` when no version is present anywhere.
fn client_declaration(envelope: &Value) -> Option<ClientDeclaration> {
    const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
    const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
    let method = envelope.get("method").and_then(Value::as_str)?;
    let params = envelope.get("params");
    let meta = params.and_then(|p| p.get("_meta"));
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "<none>".to_string())
    };
    let (style, version, client_info) = if method == "initialize" {
        (
            "initialize",
            params.and_then(|p| p.get("protocolVersion")),
            params.and_then(|p| p.get("clientInfo")),
        )
    } else {
        let version = meta
            .and_then(|m| m.get(META_VERSION))
            .or_else(|| params.and_then(|p| p.get("protocolVersion")))
            .or_else(|| envelope.get("protocolVersion"))?;
        let client_info = meta
            .and_then(|m| m.get(META_CLIENT))
            .or_else(|| params.and_then(|p| p.get("clientInfo")))
            .or_else(|| envelope.get("clientInfo"));
        ("per-request", Some(version), client_info)
    };
    Some(ClientDeclaration {
        style,
        method: method.to_string(),
        protocol_version: text(version),
        client_name: text(client_info.and_then(|c| c.get("name"))),
        client_version: text(client_info.and_then(|c| c.get("version"))),
    })
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
            "description": "Read a wiki page by id (e.g. 'entities/alice'). Returns title, frontmatter, body. If the page has been replaced (frontmatter `superseded_by`), the response also carries `superseded_by: <id>` and `notice: \"Superseded by <id>\"` — read the successor for current facts. Never copy the notice into a page.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_get_pages",
            "description": "Bulk-read variant of brain_get_page (superseded pages are marked the same way). Pass an array of ids; the response contains one entry per id in request order, each shaped `{id, found, page?}`. Missing ids are returned as `{id, found: false}` rather than aborting the call — so the agent can decide per-id whether to create-or-skip. Use for refactor sweeps and consistency audits where 5–20 related pages need to be inspected at once.",
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
            "description": "Existence check before creating a page: returns {id, exists, matches, matches_checked}. `matches` lists other pages of the same type that are probably the same thing, each {id, title, reason}: reason 'alias' (the slug names one of the page's frontmatter `aliases`), 'normalised' (same slug after lowercasing, umlauts ä→ae/ö→oe/ü→ue/ß→ss and punctuation/`_`/space → `-`; also 'muller-gmbh' vs 'mueller-gmbh') or 'similar' (a near spelling — one letter apart on short slugs, two on long ones). `exists: false` with non-empty matches means the page probably exists already under that id — use and update it instead of creating a duplicate. brain_write_page refuses to create a page with an 'alias' or 'normalised' match unless you pass allow_duplicate:true. Does not read the page body (one file check plus a lookup in the search index); matches come from that index, so a page written in the last few seconds may not be listed yet, and `matches_checked: false` means the index is not built yet and no matches could be looked up.",
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
            "description": "Return a wiki page plus the pages it links to and pages that link to it (1-hop). If the page has been replaced (frontmatter `superseded_by`), the response carries top-level `superseded_by: <id>` and `notice: \"Superseded by <id>\"` — follow it for current facts. Never copy the notice into a page.",
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
            "description": "Create or overwrite a wiki page. Caller must include valid YAML frontmatter (id, type, title) followed by the markdown body. Expected on every new page: `summary:` — one or two sentences saying what the page is about (search ranks summary hits above body hits and embeds every chunk with it; without one the page gets a quiet `missing-summary` warning). Optional frontmatter: `aliases: [..]` (other names of the thing), `sources: [sources/..]` (where the facts come from), `valid_from` / `valid_to` (YYYY-MM-DD) and `superseded_by: <id>` (facts are never overwritten — a replaced page gets `superseded_by` and `valid_to`). Creating a NEW id is refused when another page of the same type probably is the same thing (same slug after normalisation, or one of its aliases — see brain_page_exists); the error names that page: update it instead, or pass allow_duplicate:true if they really are different. Overwriting an existing id is never refused. The watcher will lint and auto-commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "page id, e.g. 'entities/alice'" },
                    "content": { "type": "string", "description": "full markdown including frontmatter" },
                    "allow_duplicate": { "type": "boolean", "description": "create the page even though brain_page_exists reports an 'alias' or 'normalised' match. Default false." },
                    "confirm_summary": { "type": "boolean", "description": "the page's `summary` is still accurate for the written body: mark it as current so the dream queue stops reporting it as summary-stale. Default false." }
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
                    "content": { "type": "string", "description": "the new section body (markdown, without the heading line)" },
                    "confirm_summary": { "type": "boolean", "description": "the page's existing `summary` is still accurate for the patched body: mark it as current so the dream queue stops reporting it as summary-stale (the file is not changed). Default false." }
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
            "name": "brain_rename_page",
            "description": "Give an existing page a new id — use this when a page was created under a WRONG id (typo, wrong slug, wrong type directory). Moves the page, sets its frontmatter `id` (and `type`, if the type directory changes) and rewrites every link to the old id in every page of the vault: `[[old]]` → `[[new]]`, `[[old|Alias]]` → `[[new|Alias]]`, `[Text](old)` → `[Text](new)`. Only exact-id links change (`[[old-2]]` is untouched); links inside code blocks are rewritten too. Frontmatter `superseded_by: old` and `sources` entries naming `old` are pointed at the new id as well. `new_id` must be `<type>/<slug>` with type one of entities, concepts, sources, topics, and a slug of letters, digits, `.`, `_`, `-` (no spaces or parentheses); it must not exist yet — if it does, the two pages are duplicates: use brain_merge_pages instead. A case-only rename (`Old` → `old`) works. Records one commit (after a checkpoint commit of any pending edits). Returns `{old_id, new_id, rewritten_pages, rewritten_links, commit}`; if `commit` is null with a `note`, the rename is already on disk — do not repeat it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "current page id, e.g. 'entities/dan-shapio'" },
                    "new_id": { "type": "string", "description": "the correct page id, e.g. 'entities/dan-shapiro'" }
                },
                "required": ["id", "new_id"]
            }
        }),
        json!({
            "name": "brain_merge_pages",
            "description": "Fold a DUPLICATE page into the page that should survive. Appends the body of `from_id` to `into_id` under a `## Merged from <from_id>` heading (target frontmatter and title kept, tags united), redirects every link to `from_id` in the vault to `into_id` (aliases kept) — and every frontmatter `superseded_by` / `sources` entry naming it —, adds `from_id` and its aliases to the target's `aliases`, and removes `from_id`. Links between the two pages become plain text so the merged page never links to itself. Review and tidy the merged page afterwards with brain_patch_page — the appended section is a verbatim copy. Records one commit (after a checkpoint commit of any pending edits); the removed page stays recoverable: brain_get_page_history on `from_id`, then brain_restore_page with a sha from before the merge. Returns `{from_id, into_id, rewritten_pages, rewritten_links, commit}` (plus `note` if the commit is still pending).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_id": { "type": "string", "description": "the duplicate page to fold in and remove" },
                    "into_id": { "type": "string", "description": "the page that survives and receives the content" }
                },
                "required": ["from_id", "into_id"]
            }
        }),
        json!({
            "name": "brain_delete_page",
            "description": "Delete a page that should not exist at all (junk, test page, empty stub). Not for a wrong id — use brain_rename_page — and not for a duplicate — use brain_merge_pages. REFUSES while other pages link to it — or name it in frontmatter `superseded_by` / `sources` — and lists those pages. With `force: true` it deletes anyway, turns every link to it into plain text (`[[id|Alias]]` → `Alias`, `[[id]]` → the page title) and removes those `superseded_by` lines / `sources` entries. Records one commit (after a checkpoint commit of any pending edits), so the content is never lost: brain_get_page_history on the deleted id, then brain_restore_page with a sha from before the delete. Returns `{deleted, defused_in, defused_links, commit}` (plus `note` if the commit is still pending).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "page id to delete, e.g. 'entities/test-page'" },
                    "force": { "type": "boolean", "description": "delete even while other pages link to it (links become plain text). Default false." }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "brain_write_batch",
            "description": "Atomic multi-page write. Pass `pages: [{id, content}, ...]` — all pages are parsed and normalised first (phase 1; if any one fails to parse, nothing is written), then all are written to disk (phase 2), then lint runs ONCE over the whole vault (phase 3) and the response is scoped to the union of paths in the batch. Use this when several pages reference each other and would cascade broken-link errors if written one-by-one. Response: `{wrote: [{id, previous_size_bytes, new_size_bytes, warnings}]}`. Errors abort with a structured message naming the offending id. Like brain_write_page, creating a NEW id that probably duplicates an existing page (or an earlier entry of the same batch) is refused before anything is written, unless the entry — or the whole call — sets allow_duplicate:true. Commit is delegated to the watcher (same as brain_write_page).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" },
                                "allow_duplicate": { "type": "boolean" }
                            },
                            "required": ["id", "content"]
                        },
                        "minItems": 1
                    },
                    "allow_duplicate": { "type": "boolean", "description": "allow_duplicate for every entry. Default false." }
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
            "description": "Return the current lint state of the wiki as { errors, warnings } — both are arrays of { path, kind, message }. Errors block auto-commits, warnings don't. Common warning kinds you should fix in place via brain_write_page: 'unregistered-type' (frontmatter type isn't one of entity/concept/source/topic — usually a plural slipped in), 'missing-title', 'non-canonical-wiki-link'. Common error kinds: 'frontmatter' (malformed YAML), 'duplicate-id' (two files share an id), 'broken-link' (wiki link points at a missing page; `[[id#heading]]` resolves to `id`). Hygiene warnings (advice, never block commits): 'orphan' (no other page links here and the file is unchanged for 90+ days — link it from a related page, merge it or delete it), 'duplicate-candidate' (two pages of the same type are semantically near-identical, score in the message — fold one into the other with brain_merge_pages if they describe the same thing), 'alias-collision' (two pages of one type share a name via an alias or the same slug — merge them, fix the alias, or add `distinct_from: [<other id>]` if they are different things), 'missing-sources' (an entity/concept page without `sources`), 'missing-summary' (a page without a `summary:` line — add one or two sentences), 'broken-source' (a `sources` entry without a page), 'invalid-date' (`valid_from`/`valid_to` not YYYY-MM-DD, or from after to), 'expired-but-linked' (`valid_to` has passed but current pages still link here — point them at the successor). Errors 'dangling-supersede' (`superseded_by` names a page that does not exist) and 'supersede-cycle' (pages supersede each other or themselves). An optional `notes` array carries info that is not a page finding (e.g. duplicate detection skipped because the embedding model is missing). Use this when the user asks you to clean up the wiki: loop through the report, fix each entry, then call again until clean.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "brain_query",
            "description": "Dataview-style structured query against page metadata. Supports fields id, type, title, tag, created, updated; operators `:` (eq), `:>`, `:<`; AND, OR, NOT; quoted values for spaces. Validity: by default (`valid:now`) pages whose `valid_to` has passed or that have `superseded_by` are left out; add `valid:all` to include them or `valid:expired` to list only them. Order: newest `updated` first; add `sort:salience` (top-level, with AND) to list the most-read pages first. Each hit has {id, type, path, title, updated_at, reads, search_hits, last_read_at} plus valid_from/valid_to/superseded_by when set. Examples: `type:source AND tag:customer AND updated:>2026-04-01`, `tag:nis2 OR tag:dora`, `NOT type:source AND title:\"NLSpec\"`, `type:entity AND valid:all AND sort:salience`. Use this for filtered listings; use brain_search for free-text search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "brain_eval",
            "description": "Measure search quality: runs every test question of the vault's eval set (00_meta/eval-queries.yaml) through full-text-only, vector-only and hybrid search and returns Recall@10, MRR and nDCG@10 per mode plus, per question, which expected pages each mode found (with rank) or missed. Deterministic: same index, same numbers. Each run is appended to 00_meta/eval-history.md. Use it before and after changing pages' summaries or search settings; add questions with brain_eval_add.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "brain_eval_add",
            "description": "Add one test question to the eval set (00_meta/eval-queries.yaml, synced between machines): the question as the user would ask it and the page ids a good search should return in its top 10. Every expected id must be an existing page; a question or id already in the set is refused. Without `id`, one is generated from the query. Returns the stored entry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "optional stable name, e.g. 'q-kunde-a-laufzeit'" },
                    "query": { "type": "string" },
                    "expected": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "page ids, e.g. ['entities/kunde-a']" },
                    "note": { "type": "string" }
                },
                "required": ["query", "expected"]
            }
        }),
        json!({
            "name": "brain_dream_queue",
            "description": "The dream queue: BRAIN's prioritised list of what to consolidate in the wiki — `{generated_at, items: [{priority 1..3, kind, pages, reason, suggested_action}], omitted}`. Kinds: broken-link / broken-source (fix-link), duplicate-candidate (merge), summary-stale (update-summary: the body changed since the summary was written), missing-summary on a hub page (write-summary), decay-candidate (archive-or-supersede: never read, unlinked, unchanged 90+ days), orphan (review-or-archive). Each page appears in one item at most. Read it when the user asks you to dream / tidy up ('träum mal'), then work top-down (see AGENTS.md, section Dreaming). Served from 00_meta/dream-queue.md when it is younger than 1 hour; `refresh: true` recomputes it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "recompute even if the stored queue is fresh. Default false." }
                }
            }
        }),
        json!({
            "name": "brain_dream_log",
            "description": "Append one dated line to the dream log (00_meta/dream-log.md, local) — at the end of a dream session, say in one line what you changed and why (e.g. 'merged entities/acme-inc into entities/acme; wrote summaries for 3 hubs').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entry": { "type": "string" }
                },
                "required": ["entry"]
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
            //
            // The embedder is resolved BEFORE db_op: the first search in
            // this process loads the 2.2 GB bge-m3 weights, which takes
            // longer than DB_OP_TIMEOUT. Inside the timeout that load
            // was abandoned every time and search silently degraded to
            // the substring walk. Outside it, the first search pays the
            // load once and every later search hits the process cache.
            let embedder = crate::embedding::cached_for_vault(vault);
            let query_owned = q.to_string();
            let hybrid = db_op(db, vault, "brain_search", move |conn| {
                search::search_hybrid_on_conn(conn, embedder.as_ref(), &query_owned)
                    .map_err(crate::db::DbError::from)
            });
            let hits = match hybrid {
                Ok(hits) if !hits.is_empty() => hits,
                // Empty hybrid result or any DB error → brute-force walk.
                _ => search::search_brute_force(vault, q).map_err(|e| e.to_string())?,
            };
            record_search_hits(
                db,
                vault,
                hits.iter().take(SALIENCE_SEARCH_TOP).map(|h| h.id.clone()).collect(),
            );
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "brain_get_page" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            check_page_id(id)?;
            let page = tree::read_page(vault, id).map_err(|e| e.to_string())?;
            record_reads(db, vault, vec![id.to_string()]);
            let (payload, _) = page_payload(page);
            Ok(serde_json::to_string_pretty(&payload).unwrap_or_default())
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
            let mut read_ids: Vec<String> = Vec::new();
            let pages: Vec<Value> = ids
                .iter()
                .map(|raw| {
                    let id = raw.as_str().unwrap_or("");
                    // Same per-id error shape as a missing page; the guard
                    // runs before any filesystem access.
                    if let Err(e) = check_page_id(id) {
                        return json!({
                            "id": id,
                            "found": false,
                            "error": e,
                        });
                    }
                    match tree::read_page(vault, id) {
                        Ok(page) => {
                            read_ids.push(id.to_string());
                            json!({
                                "id": id,
                                "found": true,
                                "page": page_payload(page).0,
                            })
                        }
                        Err(e) => json!({
                            "id": id,
                            "found": false,
                            "error": e.to_string(),
                        }),
                    }
                })
                .collect();
            // One read per distinct page, however often it was requested.
            let mut seen = std::collections::HashSet::new();
            read_ids.retain(|id| seen.insert(id.clone()));
            record_reads(db, vault, read_ids);
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
            // Defend against path escapes smuggled into the id
            // (`../../etc/passwd`, `C:/Users/x`). Reject before joining onto
            // the wiki dir so the Path::is_file() check can never escape the
            // vault root.
            check_page_id(id)?;
            let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
            let exists = target.is_file();
            // A2: other pages that are probably the same thing (alias,
            // normalised slug, near-identical slug). From the index, best
            // effort and never building it: without a usable index there
            // are no matches and `matches_checked` is false. Matches whose
            // file is gone (stale index rows) are dropped.
            let entries = load_name_entries(db, vault);
            let matches_checked = entries.is_some();
            let matches = live_matches(
                vault,
                duplicates::find_matches(id, &entries.unwrap_or_default()),
                &std::collections::HashSet::new(),
            );
            Ok(serde_json::to_string(&json!({
                "id": id,
                "exists": exists,
                "matches": matches,
                "matches_checked": matches_checked,
            }))
            .unwrap_or_default())
        }
        "brain_get_context" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            check_page_id(id)?;
            let page = tree::read_page(vault, id).map_err(|e| e.to_string())?;
            // `tree::read_page` already strips the YAML frontmatter, so
            // `page::parse` would refuse the body with "missing frontmatter
            // delimiter" → surfaced as a `lint:` error to the LLM. Skip the
            // re-parse and pull wiki links directly from the body.
            let outbound = page::extract_wiki_links(&page.body);
            let backlinks = search::backlinks(vault, id).map_err(|e| e.to_string())?;
            record_reads(db, vault, vec![id.to_string()]);
            let (page, superseded_by) = page_payload(page);
            let mut payload = json!({
                "page": page,
                "outbound": outbound,
                "backlinks": backlinks,
            });
            if let Some(successor) = superseded_by {
                payload["notice"] = json!(superseded_notice(&successor));
                payload["superseded_by"] = json!(successor);
            }
            Ok(serde_json::to_string_pretty(&payload).unwrap_or_default())
        }
        "brain_list_pages" => list_pages_dispatch(&args, vault, db),
        "brain_write_page" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            check_page_id(id)?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'content'".to_string())?;
            let parsed = page::parse(content).map_err(|e| format!("invalid page content: {e}"))?;
            let allow_duplicate = allow_duplicate_arg(&args)?;
            let confirm_summary = confirm_summary_arg(&args)?;
            // A2: refuse to CREATE a page that probably exists already
            // under another id. Overwriting an existing id is never blocked.
            let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
            let mut matches_checked = true;
            if !allow_duplicate && !target.is_file() {
                match load_name_entries(db, vault) {
                    Some(entries) => {
                        let pending = std::collections::HashSet::new();
                        if let Some(refusal) = creation_refusal(vault, id, &entries, &pending) {
                            return Err(refusal);
                        }
                    }
                    None => matches_checked = false,
                }
            }
            // A copied-back superseded notice of an older read payload
            // never reaches the file.
            let body = strip_superseded_notice(&parsed.body);
            // Auto-normalize markdown links to canonical [[wiki-link]]
            // form before write. LLMs default to standard markdown
            // syntax `[Dan](entities/dan-shapiro)` — without this the
            // graph view sees no edges and refactors are fragile. Only
            // the body is rewritten; the YAML frontmatter is kept
            // verbatim and we re-stitch the file.
            let normalized_body = page::normalize_internal_links(body);
            let normalized_content = if normalized_body == parsed.body {
                content.to_string()
            } else {
                rebuild_page_file(content, &normalized_body)
            };
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
            let mut response = json!({
                "wrote": id,
                "previous_size_bytes": previous_size_bytes,
                "new_size_bytes": new_size_bytes,
                "warnings": page_warnings,
            });
            if !matches_checked {
                // The duplicate check could not run (index not built yet).
                response["matches_checked"] = json!(false);
            }
            if confirm_summary {
                response["summary_confirmed"] =
                    json!(confirm_summary_in_index(db, vault, id, &normalized_content));
            }
            Ok(serde_json::to_string(&response).unwrap_or_default())
        }
        "brain_patch_page" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            check_page_id(id)?;
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
            let confirm = confirm_summary_arg(&args)?;
            let response = write_normalized_page(vault, id, &new_content)?;
            if !confirm {
                return Ok(response);
            }
            let mut response: Value = serde_json::from_str(&response).unwrap_or_else(|_| json!({}));
            response["summary_confirmed"] = json!(confirm_summary_in_index(db, vault, id, &new_content));
            Ok(serde_json::to_string(&response).unwrap_or_default())
        }
        "brain_get_page_history" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'id'".to_string())?;
            if id.is_empty() {
                return Err("'id' must not be empty".to_string());
            }
            check_page_id(id.strip_suffix(".md").unwrap_or(id))?;
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
            check_page_id(id.strip_suffix(".md").unwrap_or(id))?;
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
        "brain_rename_page" => {
            let id = required_str(&args, "id")?;
            let new_id = required_str(&args, "new_id")?;
            let outcome = refactor::rename_page(vault, id, new_id).map_err(|e| e.to_string())?;
            forget_in_index(db, vault, &outcome.old_id, Some(&outcome.new_id));
            Ok(serde_json::to_string(&outcome).unwrap_or_default())
        }
        "brain_merge_pages" => {
            let from_id = required_str(&args, "from_id")?;
            let into_id = required_str(&args, "into_id")?;
            let outcome =
                refactor::merge_pages(vault, from_id, into_id).map_err(|e| e.to_string())?;
            forget_in_index(db, vault, &outcome.from_id, Some(&outcome.into_id));
            Ok(serde_json::to_string(&outcome).unwrap_or_default())
        }
        "brain_delete_page" => {
            let id = required_str(&args, "id")?;
            let force = match args.get("force") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("force must be a boolean (true or false)".to_string()),
            };
            let outcome = refactor::delete_page(vault, id, force).map_err(|e| e.to_string())?;
            forget_in_index(db, vault, &outcome.deleted, None);
            Ok(serde_json::to_string(&outcome).unwrap_or_default())
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
            let allow_all = allow_duplicate_arg(&args)?;
            // A2 duplicate check: index entries (loaded once, only if some
            // entry needs the check) plus the batch's earlier entries.
            let mut index_names: Option<Option<Vec<duplicates::NameEntry>>> = None;
            let mut batch_names: Vec<duplicates::NameEntry> = Vec::new();
            let mut batch_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut matches_checked = true;
            for (idx, entry) in pages.iter().enumerate() {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("pages[{idx}]: missing 'id'"))?;
                check_page_id(id).map_err(|e| format!("pages[{idx}]: {e}"))?;
                let content = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("pages[{idx}]: missing 'content'"))?;
                let parsed = page::parse(content)
                    .map_err(|e| format!("pages[{idx}] ({id}): invalid content: {e}"))?;
                let allow_duplicate =
                    allow_all || allow_duplicate_arg(entry).map_err(|e| format!("pages[{idx}]: {e}"))?;
                let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
                if !allow_duplicate && !target.is_file() {
                    let index = index_names.get_or_insert_with(|| load_name_entries(db, vault));
                    if index.is_none() {
                        matches_checked = false;
                    }
                    let candidates: Vec<duplicates::NameEntry> = index
                        .iter()
                        .flatten()
                        .chain(&batch_names)
                        .cloned()
                        .collect();
                    if let Some(refusal) = creation_refusal(vault, id, &candidates, &batch_ids) {
                        return Err(format!("pages[{idx}] ({id}): {refusal}"));
                    }
                }
                batch_names.push(duplicates::NameEntry {
                    id: id.to_string(),
                    title: parsed.frontmatter.title.clone(),
                    aliases: parsed.frontmatter.aliases.clone(),
                    distinct_from: parsed.frontmatter.distinct_from.clone(),
                });
                batch_ids.insert(id.to_string());
                let normalized_body =
                    page::normalize_internal_links(strip_superseded_notice(&parsed.body));
                let normalized_content = if normalized_body == parsed.body {
                    content.to_string()
                } else {
                    rebuild_page_file(content, &normalized_body)
                };
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
            let mut response = json!({ "wrote": results });
            if !matches_checked {
                response["matches_checked"] = json!(false);
            }
            Ok(serde_json::to_string_pretty(&response).unwrap_or_default())
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
            check_relative_path("connector", connector)?;
            check_relative_path("relative_path", rel)?;
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
            // Read which embedder `cached_for_vault` serves for this
            // vault right now — same code path as the indexer and
            // search, so the status reflects what's actually generating
            // chunk vectors. Cheap when the files are absent or the
            // model is already cached; otherwise this call pays the
            // one-time load that later searches then reuse.
            // TODO(S06): report from a cache peek (loaded / failed / not yet loaded) instead of forcing a load here.
            let embedder = crate::embedding::cached_for_vault(vault);
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
            // Plus the hygiene warnings (orphan, duplicate-candidate) and
            // info `notes`; the watcher's pre-commit gate keeps the fast
            // filesystem-only lint. The index reads go through db_op
            // (lazy open, timeout, reopen); if they fail, a
            // `hygiene-skipped` note says why instead of the hygiene
            // findings silently going missing.
            let mut report = lint::lint(vault).map_err(|e| e.to_string())?;
            let rows = db_op(
                db,
                vault,
                "brain_lint_report",
                crate::wiki::hygiene::load_rows,
            );
            lint::add_hygiene(&mut report, vault, rows);
            Ok(serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        "brain_eval" => {
            use crate::viewer::eval;
            let set = eval::load_eval_set(vault).map_err(|e| e.to_string())?;
            if set.is_empty() {
                return Err(format!(
                    "the eval set is empty — add test questions with brain_eval_add \
                     (stored in 00_meta/{})",
                    eval::EVAL_SET_FILENAME
                ));
            }
            // Embed the queries BEFORE db_op (like brain_search): the first
            // call may load the model, and N query embeddings must not
            // count against the index timeout.
            let embedder = crate::embedding::cached_for_vault(vault);
            let vectors = eval::embed_queries(embedder.as_ref(), &set);
            // One bounded db_op per query: a large set on a large vault
            // must not hold the lock (and risk the timeout) as one block.
            let facts = db_op(db, vault, "brain_eval", eval::index_facts)?;
            let mut results = Vec::with_capacity(set.len());
            for (entry, vector) in set.iter().zip(vectors) {
                if entry.expected.is_empty() {
                    results.push(None);
                    continue;
                }
                let entry = entry.clone();
                let result = db_op(db, vault, "brain_eval", move |conn| {
                    eval::eval_query_on_conn(conn, &entry, &vector)
                })?;
                results.push(Some(result));
            }
            let report = eval::assemble_report(&set, results, &facts, embedder.name());
            if let Err(err) = eval::append_history(vault, &report, chrono::Local::now()) {
                tracing::warn!(?err, "could not append to the eval history");
            }
            Ok(serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        "brain_eval_add" => {
            use crate::viewer::eval;
            let query = required_str(&args, "query")?.to_string();
            let expected: Vec<String> = args
                .get("expected")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing 'expected' (array of page ids)".to_string())?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "'expected' must contain page id strings".to_string())
                })
                .collect::<Result<_, _>>()?;
            let optional = |key: &str| -> Result<Option<String>, String> {
                match args.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(_) => Err(format!("'{key}' must be a string")),
                }
            };
            let entry = eval::add_eval_query(
                vault,
                eval::NewEvalQuery {
                    id: optional("id")?,
                    query,
                    expected,
                    note: optional("note")?,
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&json!({ "added": entry })).unwrap_or_default())
        }
        "brain_dream_queue" => {
            use crate::wiki::dream;
            let refresh = match args.get("refresh") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("'refresh' must be a boolean".to_string()),
            };
            let now = chrono::Utc::now();
            let cached = if refresh { None } else { dream::cached_queue(vault, now) };
            let queue = match cached {
                Some(queue) => queue,
                None => {
                    let rows = db_op(db, vault, "brain_dream_queue", dream::load_dream_rows)?;
                    let queue = dream::build_queue(&rows, now);
                    if let Err(err) = dream::write_dream_queue(vault, &queue) {
                        tracing::warn!(?err, "could not write the dream queue");
                    }
                    queue
                }
            };
            Ok(serde_json::to_string_pretty(&queue).unwrap_or_default())
        }
        "brain_dream_log" => {
            let entry = required_str(&args, "entry")?;
            let line = crate::wiki::dream::append_dream_log(vault, entry, chrono::Local::now())
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string(&json!({ "logged": line })).unwrap_or_default())
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

/// A required string argument: `missing '<key>'` when absent,
/// `'<key>' must be a string` when present with another JSON type.
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("missing '{key}'")),
        Some(v) => v.as_str().ok_or_else(|| format!("'{key}' must be a string")),
    }
}

/// Optional `allow_duplicate` boolean argument (A2), default false.
fn allow_duplicate_arg(args: &Value) -> Result<bool, String> {
    match args.get("allow_duplicate") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("allow_duplicate must be a boolean (true or false)".to_string()),
    }
}

/// Timeout of the best-effort salience counter writes (H3).
const COUNTER_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `f` on the index ONLY when it is already built — the side jobs of
/// read and write tools (salience counters, the duplicate check) must
/// never trigger the first-open index build that [`db_op`] performs (it
/// may embed the whole vault). With no handle yet, a temporary connection
/// is opened and NOT kept, so the first real index tool still builds the
/// index. `None` when the index is empty or unreachable, on an error or
/// on a timeout. Never drops the shared handle: a wedged disk is detected
/// and handled by the next regular `db_op`.
fn db_op_if_indexed<F, T>(
    db: &Option<crate::db::DbHandle>,
    vault: &std::path::Path,
    timeout: std::time::Duration,
    f: F,
) -> Option<T>
where
    F: FnOnce(&rusqlite::Connection) -> crate::db::DbResult<T> + Send + 'static,
    T: Send + 'static,
{
    let handle = match db {
        Some(handle) => handle.clone(),
        None => crate::db::DbHandle::open(vault).ok()?,
    };
    let guarded = move |conn: &rusqlite::Connection| -> crate::db::DbResult<Option<T>> {
        let indexed: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM pages)", [], |r| r.get(0))?;
        if !indexed {
            return Ok(None);
        }
        f(conn).map(Some)
    };
    match handle.with_timeout(timeout, guarded) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => {
            tracing::debug!(%err, "index side job failed");
            None
        }
        Err(crate::db::DbTimeout) => {
            tracing::debug!("index side job timed out");
            None
        }
    }
}

/// Ids, titles and aliases of every indexed page, for the A2 duplicate
/// check. `None` when the check cannot run (index empty — not built yet —
/// or unreadable); callers then skip it and say so (`matches_checked:
/// false`). A write is never refused because of the index.
fn load_name_entries(
    db: &Option<crate::db::DbHandle>,
    vault: &std::path::Path,
) -> Option<Vec<duplicates::NameEntry>> {
    db_op_if_indexed(db, vault, DB_OP_TIMEOUT, duplicates::load_entries)
}

/// Whether the page `id` is on disk. A stale index row (page deleted or
/// renamed outside BRAIN) must not count as an existing page.
fn page_file_exists(vault: &std::path::Path, id: &str) -> bool {
    crate::wiki::encryption::page_path(vault, id).is_ok_and(|p| p.is_file())
}

/// `matches` without pages whose file no longer exists, except ids in
/// `pending` (earlier entries of the same batch, not written yet).
fn live_matches(
    vault: &std::path::Path,
    matches: Vec<duplicates::DuplicateMatch>,
    pending: &std::collections::HashSet<String>,
) -> Vec<duplicates::DuplicateMatch> {
    matches
        .into_iter()
        .filter(|m| pending.contains(&m.id) || page_file_exists(vault, &m.id))
        .collect()
}

/// The refusal text when creating `id` would probably duplicate one of
/// `entries` (an `alias` or `normalised` match whose page exists), else
/// `None`.
fn creation_refusal(
    vault: &std::path::Path,
    id: &str,
    entries: &[duplicates::NameEntry],
    pending: &std::collections::HashSet<String>,
) -> Option<String> {
    let blocking: Vec<duplicates::DuplicateMatch> = duplicates::find_matches(id, entries)
        .into_iter()
        .filter(|m| m.reason.blocks_create())
        .collect();
    live_matches(vault, blocking, pending)
        .first()
        .map(duplicates::refusal_message)
}

/// `brain_search` counts a search hit (H3) for this many top results.
const SALIENCE_SEARCH_TOP: usize = 10;

/// H3 salience: count one read for each id. Best effort: skipped while
/// the index is not built, bounded by [`COUNTER_DB_TIMEOUT`], never fails
/// the tool.
fn record_reads(db: &Option<crate::db::DbHandle>, vault: &std::path::Path, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let _ = db_op_if_indexed(db, vault, COUNTER_DB_TIMEOUT, move |conn| {
        crate::db::pages_index::record_reads(conn, &ids, now)
    });
}

/// H3 salience: count one search hit for each id. Best effort, like
/// [`record_reads`].
fn record_search_hits(db: &Option<crate::db::DbHandle>, vault: &std::path::Path, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let _ = db_op_if_indexed(db, vault, COUNTER_DB_TIMEOUT, move |conn| {
        crate::db::pages_index::record_search_hits(conn, &ids)
    });
}

/// The MCP payload of a page read: the page view verbatim, plus — when
/// its frontmatter has `superseded_by` (Slice C) — the fields
/// `superseded_by: <id>` and `notice: "Superseded by <id>"`. The body is
/// never changed (an agent would write an injected line back). Also
/// returns the successor id.
fn page_payload(page: tree::PageView) -> (Value, Option<String>) {
    let successor = serde_json::from_str::<Value>(&page.frontmatter)
        .ok()
        .and_then(|fm| fm.get("superseded_by").and_then(Value::as_str).map(str::to_string));
    let mut payload = serde_json::to_value(&page).unwrap_or_else(|_| json!({}));
    if let Some(s) = &successor {
        payload["superseded_by"] = json!(s);
        payload["notice"] = json!(superseded_notice(s));
    }
    (payload, successor)
}

fn superseded_notice(successor: &str) -> String {
    format!("Superseded by {successor}")
}

/// `body` without a leading `> Superseded by [[…]]` line (and the blank
/// lines after it). Older BRAIN versions put that line into the read
/// payload; an agent that copied it back must not persist it.
fn strip_superseded_notice(body: &str) -> &str {
    let trimmed = body.trim_start_matches(['\r', '\n']);
    if !trimmed.starts_with("> Superseded by [[") {
        return body;
    }
    let line_end = trimmed.find('\n').map(|i| i + 1).unwrap_or(trimmed.len());
    if !trimmed[..line_end].trim_end().ends_with("]]") {
        return body;
    }
    trimmed[line_end..].trim_start_matches(['\r', '\n'])
}

/// The shared page-id guard ([`refactor::validate_page_id`]) for every
/// tool that turns an id into a path: relative `<type>/<slug>` under the
/// wiki, no drive letters, `..`, backslashes or control characters.
/// Guarded arms: get_page, get_pages (per id), get_context, page_exists,
/// write_page, write_batch (per page), patch_page, get_page_history,
/// restore_page, rename_page, merge_pages, delete_page (the last three
/// inside `wiki::refactor`). Any new arm that resolves a page id must call
/// this first. `brain_write_raw_file` uses [`check_relative_path`].
fn check_page_id(id: &str) -> Result<(), String> {
    refactor::validate_page_id(id).map_err(|e| e.to_string())
}

/// Guard for a caller-supplied relative path under `01_raw/`: only plain
/// relative components — no `..`, drive letters (`wiki.join("C:/x")`
/// replaces the base on Windows), roots, `:` or control characters.
fn check_relative_path(label: &str, rel: &str) -> Result<(), String> {
    use std::path::Component;
    let plain = !rel.is_empty()
        && !rel.contains(':')
        && !rel.chars().any(char::is_control)
        && std::path::Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if plain {
        Ok(())
    } else {
        Err(format!(
            "{label} must be a plain relative path (no '..', drive letters, leading '/' or ':')"
        ))
    }
}

/// Best-effort removal of the index rows of a page id a refactor just
/// removed from disk, so search/query in this process stop returning them. The
/// refactor itself is already committed — an index hiccup (busy GUI
/// writer, hung disk) is logged, never surfaced as a tool failure; the
/// next full rebuild (GUI watcher, or the next mount) prunes them anyway.
/// `carry_to` (rename / merge) first moves the page's salience counters
/// (`page_access`, H3) to the surviving id; without it they go with the page.
fn forget_in_index(
    db: &mut Option<crate::db::DbHandle>,
    vault: &std::path::Path,
    id: &str,
    carry_to: Option<&str>,
) {
    // The stored dream queue names pages by id; after a rename, merge or
    // delete it is out of date, so drop the cache — the next
    // brain_dream_queue recomputes it.
    match std::fs::remove_file(crate::wiki::dream::dream_queue_path(vault)) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(?err, "could not drop the stored dream queue"),
    }
    let ids = vec![id.to_string()];
    let carry_to = carry_to.map(str::to_string);
    if let Err(err) = db_op(db, vault, "forget refactored page", move |conn| {
        if let Some(to) = &carry_to {
            crate::db::pages_index::carry_page_access(conn, &ids[0], to)?;
        }
        crate::db::pages_index::forget_pages(conn, &ids)
    }) {
        tracing::warn!(%err, "could not drop a refactored page id from the index");
    }
}

/// Optional `confirm_summary` boolean argument, default false.
fn confirm_summary_arg(args: &Value) -> Result<bool, String> {
    match args.get("confirm_summary") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("'confirm_summary' must be a boolean".to_string()),
    }
}

/// Mark the indexed summary of `id` as current for the body of
/// `content` (the page file as just written) — see
/// `pages_index::confirm_summary`. False when the page has no indexed
/// summary yet or the index is unavailable (logged).
fn confirm_summary_in_index(
    db: &mut Option<crate::db::DbHandle>,
    vault: &std::path::Path,
    id: &str,
    content: &str,
) -> bool {
    let Ok(parsed) = page::parse(content) else {
        return false;
    };
    let id = id.to_string();
    let body = parsed.body;
    match db_op(db, vault, "confirm summary", move |conn| {
        crate::db::pages_index::confirm_summary(conn, &id, &body)
    }) {
        Ok(confirmed) => confirmed,
        Err(err) => {
            tracing::warn!(%err, "could not confirm the page summary");
            false
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
            "brain_rename_page",
            "brain_merge_pages",
            "brain_delete_page",
            "brain_graph",
            "brain_query",
            "brain_list_tags",
            "brain_embedding_status",
            "brain_lint_report",
            "brain_eval",
            "brain_eval_add",
            "brain_dream_queue",
            "brain_dream_log",
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
        assert!(
            !resp.contains("server/discover"),
            "a generic unknown method must keep the generic message: {resp}"
        );
    }

    #[test]
    fn server_discover_gets_a_specific_method_not_found_naming_our_protocol_version() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!("discover-1")),
            method: "server/discover".into(),
            params: json!({ "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }}),
        };
        let resp: Value = serde_json::from_str(&handle_request(&req, None, &mut None)).unwrap();
        // -32601 (not a modern code such as -32022): a dual-era client must
        // read this as "legacy server" and fall back to `initialize`.
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["id"], "discover-1");
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(msg.contains("server/discover (MCP 2026-07-28) not yet supported"), "{msg}");
        assert!(msg.contains(PROTOCOL_VERSION), "{msg}");
        assert!(msg.contains("initialize handshake"), "{msg}");
    }

    #[test]
    fn client_declaration_reads_version_and_client_info_from_initialize_params() {
        let env = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25",
            "clientInfo": {"name": "claude-code", "version": "2.1.0"}
        }});
        let decl = client_declaration(&env).unwrap();
        assert_eq!(decl.style, "initialize");
        assert_eq!(decl.protocol_version, "2025-11-25");
        assert_eq!(decl.client_name, "claude-code");
        assert_eq!(decl.client_version, "2.1.0");
    }

    #[test]
    fn client_declaration_is_null_safe_for_an_initialize_without_params() {
        let env = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize"});
        let decl = client_declaration(&env).unwrap();
        assert_eq!(decl.protocol_version, "<none>");
        assert_eq!(decl.client_name, "<none>");
        assert_eq!(decl.client_version, "<none>");
    }

    #[test]
    fn client_declaration_reads_the_2026_07_28_meta_keys_on_an_ordinary_request() {
        let env = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "codex", "version": "0.9"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }});
        let decl = client_declaration(&env).unwrap();
        assert_eq!(decl.style, "per-request");
        assert_eq!(decl.method, "tools/list");
        assert_eq!(decl.protocol_version, "2026-07-28");
        assert_eq!(decl.client_name, "codex");
        assert_eq!(decl.client_version, "0.9");
    }

    #[test]
    fn client_declaration_accepts_a_top_level_protocol_version_without_client_info() {
        let env = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "protocolVersion": "2026-07-28", "params": {"name": "brain_ping"}});
        let decl = client_declaration(&env).unwrap();
        assert_eq!(decl.protocol_version, "2026-07-28");
        assert_eq!(decl.client_name, "<none>");
    }

    #[test]
    fn client_declaration_is_none_for_a_legacy_request_after_the_handshake() {
        let env = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
        assert_eq!(client_declaration(&env), None);
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

    /// A git-backed vault with `entities/old` and `entities/alice`, where
    /// alice links to old — the fixture for the refactor tool tests.
    fn refactor_vault() -> tempfile::TempDir {
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = tempfile::TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        std::fs::write(
            wiki.join("entities/old.md"),
            "---\nid: entities/old\ntype: entity\ntitle: Old\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            wiki.join("entities/alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: Alice\n---\n\nKnows [[entities/old|O]].\n",
        )
        .unwrap();
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        tmp
    }

    fn call(tmp: &tempfile::TempDir, name: &str, arguments: Value) -> Result<String, String> {
        call_tool(&json!({ "name": name, "arguments": arguments }), tmp.path(), &mut None)
    }

    #[test]
    fn brain_write_page_keeps_the_summary_line_in_the_written_file() {
        let tmp = refactor_vault();
        call(
            &tmp,
            "brain_write_page",
            json!({
                "id": "entities/bob",
                "content": "---\nid: entities/bob\ntype: entity\ntitle: Bob\nsummary: Bob runs the ops team.\n---\n\nBody.\n"
            }),
        )
        .expect("brain_write_page must succeed");
        let path = crate::wiki::encryption::page_path(tmp.path(), "entities/bob").unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("summary: Bob runs the ops team.\n"), "{text}");
    }

    #[test]
    fn brain_eval_add_then_brain_eval_reports_metrics_for_the_three_modes() {
        let tmp = refactor_vault();
        call(&tmp, "brain_eval_add", json!({ "query": "Knows", "expected": ["entities/alice"] }))
            .expect("brain_eval_add must succeed");
        let out = call(&tmp, "brain_eval", json!({})).expect("brain_eval must succeed");
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(parsed["modes"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn brain_eval_appends_its_run_to_the_eval_history() {
        let tmp = refactor_vault();
        call(&tmp, "brain_eval_add", json!({ "query": "Knows", "expected": ["entities/alice"] }))
            .unwrap();
        call(&tmp, "brain_eval", json!({})).unwrap();
        assert!(crate::viewer::eval::eval_history_path(tmp.path()).is_file());
    }

    #[test]
    fn brain_eval_on_an_empty_eval_set_points_at_brain_eval_add() {
        let tmp = refactor_vault();
        let err = call(&tmp, "brain_eval", json!({})).unwrap_err();
        assert!(err.contains("brain_eval_add"), "{err}");
    }

    #[test]
    fn brain_eval_add_refuses_an_expected_page_that_does_not_exist() {
        let tmp = refactor_vault();
        let err = call(&tmp, "brain_eval_add", json!({ "query": "q", "expected": ["entities/nobody"] }))
            .unwrap_err();
        assert!(err.contains("entities/nobody"), "{err}");
    }

    #[test]
    fn brain_eval_add_rejects_a_non_string_note() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_eval_add",
            json!({ "query": "q", "expected": ["entities/alice"], "note": 3 }),
        )
        .unwrap_err();
        assert!(err.contains("'note' must be a string"), "{err}");
    }

    #[test]
    fn brain_dream_queue_writes_and_returns_the_queue() {
        let tmp = refactor_vault();
        let out = call(&tmp, "brain_dream_queue", json!({ "refresh": true }))
            .expect("brain_dream_queue must succeed");
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        let on_disk = crate::wiki::dream::read_queue_file(&crate::wiki::dream::dream_queue_path(tmp.path()));
        assert_eq!(
            on_disk.map(|q| q.generated_at),
            parsed["generated_at"].as_str().map(str::to_string)
        );
    }

    #[test]
    fn brain_dream_queue_serves_a_fresh_stored_queue_without_recomputing() {
        let tmp = refactor_vault();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 7,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        let out = call(&tmp, "brain_dream_queue", json!({})).unwrap();
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(parsed["omitted"], json!(7));
    }

    #[test]
    fn brain_dream_queue_with_refresh_recomputes_a_fresh_stored_queue() {
        let tmp = refactor_vault();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 7,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        let out = call(&tmp, "brain_dream_queue", json!({ "refresh": true })).unwrap();
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(parsed["omitted"], json!(0));
    }

    /// Stand-in for the real model ("bge-m3", so duplicates are trusted):
    /// texts with "pizza" point along axis 0, all others along axis 2.
    struct PizzaModel;
    impl crate::embedding::Embedder for PizzaModel {
        fn dim(&self) -> usize {
            crate::embedding::EMBED_DIM
        }
        fn name(&self) -> &'static str {
            "bge-m3"
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0; crate::embedding::EMBED_DIM];
            v[if text.contains("pizza") { 0 } else { 2 }] = 1.0;
            v
        }
    }

    fn dream_items(out: &str) -> Vec<Value> {
        let parsed: Value = serde_json::from_str(out).expect("JSON");
        parsed["items"].as_array().cloned().unwrap_or_default()
    }

    /// H1 acceptance: a queue with a duplicate pair; the agent merges the
    /// pair; the next queue has exactly that item fewer.
    #[test]
    fn merging_a_queued_duplicate_pair_removes_exactly_that_item_from_the_next_queue() {
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = tempfile::TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        for (id, extra, body) in [
            ("entities/a", "", "pizza one"),
            ("entities/b", "", "pizza two"),
            ("entities/c", "sources: [sources/missing]\n", "other topic"),
        ] {
            std::fs::write(
                wiki.join(format!("{id}.md")),
                format!("---\nid: {id}\ntype: entity\ntitle: {id}\n{extra}---\n\n{body}\n"),
            )
            .unwrap();
        }
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        let handle = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild_with(&handle, tmp.path(), &PizzaModel).unwrap();
        let mut db = Some(handle);
        let mut tool = |name: &str, arguments: Value| {
            call_tool(&json!({ "name": name, "arguments": arguments }), tmp.path(), &mut db)
        };

        let before = dream_items(&tool("brain_dream_queue", json!({ "refresh": true })).unwrap());
        tool("brain_merge_pages", json!({ "from_id": "entities/b", "into_id": "entities/a" }))
            .expect("merge must succeed");
        let after = dream_items(&tool("brain_dream_queue", json!({})).unwrap());

        let before_len = before.len();
        let expected: Vec<Value> = before
            .into_iter()
            .filter(|i| i["kind"] != json!("duplicate-candidate"))
            .collect();
        assert_eq!((after.len() + 1, after), (before_len, expected));
    }

    #[test]
    fn a_rename_drops_the_stored_dream_queue() {
        let tmp = refactor_vault();
        call(&tmp, "brain_dream_queue", json!({ "refresh": true })).unwrap();
        call(&tmp, "brain_rename_page", json!({ "id": "entities/old", "new_id": "entities/new" }))
            .unwrap();
        assert!(!crate::wiki::dream::dream_queue_path(tmp.path()).exists());
    }

    #[test]
    fn brain_write_page_with_confirm_summary_marks_the_indexed_summary_as_current() {
        let tmp = refactor_vault();
        let page = |body: &str| {
            format!("---\nid: entities/bob\ntype: entity\ntitle: Bob\nsummary: Bob runs ops.\n---\n\n{body}\n")
        };
        call(&tmp, "brain_write_page", json!({ "id": "entities/bob", "content": page("One.") }))
            .unwrap();
        let handle = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&handle, tmp.path()).unwrap();
        let mut db = Some(handle.clone());
        let out = call_tool(
            &json!({
                "name": "brain_write_page",
                "arguments": { "id": "entities/bob", "content": page("Two."), "confirm_summary": true }
            }),
            tmp.path(),
            &mut db,
        )
        .unwrap();
        crate::db::pages_index::rebuild(&handle, tmp.path()).unwrap();
        let fresh: bool = handle
            .with(|conn| {
                Ok(conn.query_row(
                    "SELECT summary_body_hash = body_hash FROM pages WHERE id = 'entities/bob'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert!(fresh && out.contains("\"summary_confirmed\":true"), "{out}");
    }

    #[test]
    fn brain_patch_page_rejects_a_non_boolean_confirm_summary() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_patch_page",
            json!({ "id": "entities/old", "heading": "## X", "content": "y", "confirm_summary": "yes" }),
        )
        .unwrap_err();
        assert!(err.contains("'confirm_summary' must be a boolean"), "{err}");
    }

    #[test]
    fn brain_dream_log_appends_the_entry_to_the_dream_log() {
        let tmp = refactor_vault();
        call(&tmp, "brain_dream_log", json!({ "entry": "merged a into b" })).unwrap();
        let text =
            std::fs::read_to_string(crate::wiki::dream::dream_log_path(tmp.path())).unwrap();
        assert!(text.trim_end().ends_with("merged a into b"), "{text}");
    }

    #[test]
    fn brain_rename_page_returns_the_rewritten_pages_as_json() {
        let tmp = refactor_vault();
        let ok = call_tool(
            &json!({
                "name": "brain_rename_page",
                "arguments": { "id": "entities/old", "new_id": "entities/new" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_rename_page must succeed");
        let parsed: Value = serde_json::from_str(&ok).expect("JSON");
        assert_eq!(parsed["rewritten_pages"], json!(["entities/alice"]));
    }

    #[test]
    fn brain_merge_pages_reports_the_ids_under_the_input_key_names() {
        let tmp = refactor_vault();
        let ok = call_tool(
            &json!({
                "name": "brain_merge_pages",
                "arguments": { "from_id": "entities/old", "into_id": "entities/alice" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_merge_pages must succeed");
        let parsed: Value = serde_json::from_str(&ok).expect("JSON");
        assert_eq!(
            (&parsed["from_id"], &parsed["into_id"]),
            (&json!("entities/old"), &json!("entities/alice"))
        );
    }

    #[test]
    fn brain_delete_page_refusal_names_the_referring_pages() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_delete_page",
                "arguments": { "id": "entities/old" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("a linked page must not be deleted without force");
        assert!(err.contains("entities/alice"), "referrer missing from: {err}");
    }

    #[test]
    fn brain_delete_page_rejects_a_non_boolean_force() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_delete_page",
                "arguments": { "id": "entities/old", "force": "yes" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("a string force must be refused");
        assert!(err.contains("force must be a boolean"), "got: {err}");
    }

    #[test]
    fn brain_rename_page_rejects_a_non_string_new_id() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_rename_page",
                "arguments": { "id": "entities/old", "new_id": 42 }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("a numeric new_id must be refused");
        assert!(err.contains("'new_id' must be a string"), "got: {err}");
    }

    #[test]
    fn page_tools_reject_drive_letter_ids_before_touching_the_disk() {
        // On Windows `wiki.join("C:/…")` replaces the base path entirely,
        // so a drive-letter id would reach any file on any drive.
        let tmp = refactor_vault();
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("notes.md");
        std::fs::write(&target, "---\nid: entities/x\ntype: entity\n---\nkeep\n").unwrap();
        let id = target.with_extension("").to_string_lossy().replace('\\', "/");
        let page = "---\nid: entities/x\ntype: entity\n---\npwned\n";
        let calls = [
            ("brain_page_exists", json!({ "id": id })),
            (
                "brain_patch_page",
                json!({ "id": id, "heading": "## X", "content": "pwned" }),
            ),
            ("brain_get_page_history", json!({ "id": id })),
            ("brain_restore_page", json!({ "id": id, "sha": "deadbeef" })),
            ("brain_write_page", json!({ "id": id, "content": page })),
            ("brain_delete_page", json!({ "id": id, "force": true })),
        ];
        let accepted: Vec<&str> = calls
            .iter()
            .filter(|(name, args)| {
                call_tool(
                    &json!({ "name": name, "arguments": args }),
                    tmp.path(),
                    &mut None,
                )
                .is_ok()
            })
            .map(|(name, _)| *name)
            .collect();
        let content = std::fs::read_to_string(&target).unwrap_or_default();
        assert!(
            accepted.is_empty() && content.contains("keep"),
            "accepted: {accepted:?}, outside file now: {content:?}"
        );
    }

    #[test]
    fn read_tools_reject_drive_letter_ids_and_never_return_the_outside_file() {
        // Information-disclosure twin of the write-side guard: a drive-letter
        // id must not let get_page / get_pages / get_context read an
        // arbitrary `.md` file elsewhere on the machine.
        let tmp = refactor_vault();
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("secret.md");
        std::fs::write(
            &target,
            "---\nid: entities/x\ntype: entity\ntitle: T\n---\nOUTSIDE-SECRET-4711\n",
        )
        .unwrap();
        let id = target.with_extension("").to_string_lossy().replace('\\', "/");
        let results: Vec<(&str, Result<String, String>)> = vec![
            (
                "brain_get_page",
                call_tool(
                    &json!({ "name": "brain_get_page", "arguments": { "id": id } }),
                    tmp.path(),
                    &mut None,
                ),
            ),
            (
                "brain_get_pages",
                call_tool(
                    &json!({ "name": "brain_get_pages", "arguments": { "ids": [id] } }),
                    tmp.path(),
                    &mut None,
                ),
            ),
            (
                "brain_get_context",
                call_tool(
                    &json!({ "name": "brain_get_context", "arguments": { "id": id } }),
                    tmp.path(),
                    &mut None,
                ),
            ),
        ];
        let leaked: Vec<String> = results
            .iter()
            .filter_map(|(name, r)| {
                let text = match r {
                    Ok(ok) => ok.clone(),
                    Err(err) => err.clone(),
                };
                let rejected = match (name, r) {
                    // get_pages reports per id, like a missing page.
                    (&"brain_get_pages", Ok(ok)) => {
                        ok.contains("\"found\": false") && ok.contains("invalid page id")
                    }
                    (_, Err(err)) => err.contains("invalid page id"),
                    _ => false,
                };
                (!rejected || text.contains("OUTSIDE-SECRET-4711"))
                    .then(|| format!("{name}: {text}"))
            })
            .collect();
        assert!(leaked.is_empty(), "not rejected or leaked: {leaked:?}");
    }

    #[test]
    fn brain_write_raw_file_rejects_an_absolute_connector_path() {
        let tmp = refactor_vault();
        let outside = tempfile::TempDir::new().unwrap();
        let connector = outside.path().to_string_lossy().replace('\\', "/");
        let _ = call_tool(
            &json!({
                "name": "brain_write_raw_file",
                "arguments": { "connector": connector, "relative_path": "x.txt", "content": "pwned" }
            }),
            tmp.path(),
            &mut None,
        );
        assert!(!outside.path().join("x.txt").exists());
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

/// A2 (aliases + duplicate check), C (validity / provenance) and H3
/// (salience) through the MCP tools.
#[cfg(test)]
mod knowledge_tests {
    use super::*;
    use crate::db::DbHandle;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use tempfile::TempDir;

    fn vault() -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let marker = crate::vault::marker::VaultMarker::new("test");
        crate::vault::marker::write_marker(tmp.path(), &marker).unwrap();
        tmp
    }

    /// Page text with `extra` frontmatter lines (each ending in a newline).
    fn page_text(id: &str, extra: &str, body: &str) -> String {
        let (sub, slug) = id.split_once('/').unwrap();
        let kind = match sub {
            "concepts" => "concept",
            "sources" => "source",
            "topics" => "topic",
            _ => "entity",
        };
        format!("---\nid: {id}\ntype: {kind}\ntitle: {slug}\n{extra}---\n\n{body}\n")
    }

    fn page_file(vault: &std::path::Path, id: &str) -> std::path::PathBuf {
        let (sub, slug) = id.split_once('/').unwrap();
        wiki_dir(vault).join(sub).join(format!("{slug}.md"))
    }

    fn put(vault: &std::path::Path, id: &str, extra: &str, body: &str) {
        let path = page_file(vault, id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, page_text(id, extra, body)).unwrap();
    }

    fn call(
        vault: &std::path::Path,
        db: &mut Option<DbHandle>,
        name: &str,
        arguments: Value,
    ) -> Result<String, String> {
        call_tool(&json!({ "name": name, "arguments": arguments }), vault, db)
    }

    fn call_json(
        vault: &std::path::Path,
        db: &mut Option<DbHandle>,
        name: &str,
        arguments: Value,
    ) -> Value {
        let out = call(vault, db, name, arguments).unwrap_or_else(|e| panic!("{name} failed: {e}"));
        serde_json::from_str(&out).unwrap()
    }

    /// A handle on the vault's index, built from the pages on disk — the
    /// normal state while BRAIN runs (the GUI keeps the index current).
    fn indexed(vault: &std::path::Path) -> Option<DbHandle> {
        let db = DbHandle::open(vault).unwrap();
        crate::db::pages_index::rebuild(&db, vault).unwrap();
        Some(db)
    }

    fn indexed_page_count(vault: &std::path::Path) -> i64 {
        DbHandle::open(vault)
            .unwrap()
            .with(|c| Ok(c.query_row("SELECT count(*) FROM pages", [], |r| r.get(0))?))
            .unwrap()
    }

    /// (reads, search_hits) of a page, (0, 0) without a row.
    fn access(db: &Option<DbHandle>, id: &str) -> (i64, i64) {
        let id = id.to_string();
        db.as_ref()
            .unwrap()
            .with(move |c| {
                Ok(c
                    .query_row(
                        "SELECT reads, search_hits FROM page_access WHERE page_id = ?1",
                        [&id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap_or((0, 0)))
            })
            .unwrap()
    }

    // ---- A2 -------------------------------------------------------------

    #[test]
    fn page_exists_reports_a_normalised_match_for_a_differently_spelled_new_id() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_page_exists",
            json!({ "id": "entities/Mueller_GmbH" }),
        );
        assert_eq!(
            (out["exists"].clone(), out["matches"].clone()),
            (
                json!(false),
                json!([{ "id": "entities/mueller-gmbh", "title": "mueller-gmbh", "reason": "normalised" }])
            )
        );
    }

    #[test]
    fn page_exists_reports_an_alias_match() {
        let tmp = vault();
        put(tmp.path(), "entities/acme", "aliases: [ACME Corporation]\n", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_page_exists",
            json!({ "id": "entities/acme-corporation" }),
        );
        assert_eq!(out["matches"][0]["reason"], json!("alias"));
    }

    #[test]
    fn page_exists_reports_a_similar_match() {
        let tmp = vault();
        put(tmp.path(), "entities/dan-shapiro", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_page_exists",
            json!({ "id": "entities/dan-shapio" }),
        );
        assert_eq!(out["matches"][0]["reason"], json!("similar"));
    }

    #[test]
    fn write_page_refuses_to_create_a_page_whose_slug_normalises_to_an_existing_one() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let err = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "Dup.") }),
        )
        .unwrap_err();
        assert!(
            err.contains("probably exists already: entities/mueller-gmbh \"mueller-gmbh\" (normalised)"),
            "got: {err}"
        );
    }

    #[test]
    fn a_refused_create_writes_no_file() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let _ = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "Dup.") }),
        );
        assert!(!page_file(tmp.path(), "entities/muller-gmbh").exists());
    }

    #[test]
    fn write_page_creates_the_duplicate_when_allow_duplicate_is_true() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let result = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({
                "id": "entities/muller-gmbh",
                "content": page_text("entities/muller-gmbh", "", "Different company."),
                "allow_duplicate": true
            }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn write_page_refuses_to_create_a_page_named_like_an_alias() {
        let tmp = vault();
        put(tmp.path(), "entities/acme", "aliases: [ACME Corporation]\n", "Body.");
        let err = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({
                "id": "entities/acme-corporation",
                "content": page_text("entities/acme-corporation", "", "Dup.")
            }),
        )
        .unwrap_err();
        assert!(err.contains("entities/acme \"acme\" (alias)"), "got: {err}");
    }

    #[test]
    fn write_page_never_refuses_to_overwrite_an_existing_id() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        put(tmp.path(), "entities/muller-gmbh", "", "Older duplicate.");
        let result = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "Updated.") }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn write_page_does_not_refuse_a_merely_similar_slug() {
        let tmp = vault();
        put(tmp.path(), "entities/dan-shapiro", "", "Body.");
        let result = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/dan-shapio", "content": page_text("entities/dan-shapio", "", "Typo page.") }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn write_page_rejects_a_non_boolean_allow_duplicate() {
        let tmp = vault();
        let err = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/x", "content": page_text("entities/x", "", "x"), "allow_duplicate": "yes" }),
        )
        .unwrap_err();
        assert!(err.contains("allow_duplicate must be a boolean"), "got: {err}");
    }

    #[test]
    fn write_batch_refuses_an_entry_that_duplicates_an_existing_page() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let err = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_batch",
            json!({ "pages": [
                { "id": "entities/fresh", "content": page_text("entities/fresh", "", "New.") },
                { "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "Dup.") }
            ] }),
        )
        .unwrap_err();
        assert!(
            err.starts_with(
                "pages[1] (entities/muller-gmbh): a page for this probably exists already: entities/mueller-gmbh"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn a_refused_batch_writes_none_of_its_pages() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let _ = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_batch",
            json!({ "pages": [
                { "id": "entities/fresh", "content": page_text("entities/fresh", "", "New.") },
                { "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "Dup.") }
            ] }),
        );
        assert!(!page_file(tmp.path(), "entities/fresh").exists());
    }

    #[test]
    fn write_batch_refuses_an_entry_that_duplicates_an_earlier_entry_of_the_batch() {
        let tmp = vault();
        let err = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_batch",
            json!({ "pages": [
                { "id": "entities/mueller-gmbh", "content": page_text("entities/mueller-gmbh", "", "One.") },
                { "id": "entities/Mueller_GmbH", "content": page_text("entities/Mueller_GmbH", "", "Two.") }
            ] }),
        )
        .unwrap_err();
        assert!(err.starts_with("pages[1] (entities/Mueller_GmbH)"), "got: {err}");
    }

    #[test]
    fn a_batch_entry_with_allow_duplicate_is_written() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let result = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_batch",
            json!({ "pages": [{
                "id": "entities/muller-gmbh",
                "content": page_text("entities/muller-gmbh", "", "Other."),
                "allow_duplicate": true
            }] }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ---- C ----------------------------------------------------------------

    fn superseded_vault() -> TempDir {
        let tmp = vault();
        put(
            tmp.path(),
            "entities/a",
            "valid_to: 2025-12-31\nsuperseded_by: entities/b\nsources: [sources/s]\n",
            "Old facts.",
        );
        put(tmp.path(), "entities/b", "sources: [sources/s]\n", "New facts.");
        put(tmp.path(), "sources/s", "", "Source.");
        tmp
    }

    #[test]
    fn get_context_of_a_superseded_page_names_the_successor_at_the_top_level() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_context", json!({ "id": "entities/a" }));
        assert_eq!(out["superseded_by"], json!("entities/b"));
    }

    #[test]
    fn get_context_of_a_superseded_page_carries_a_notice_field() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_context", json!({ "id": "entities/a" }));
        assert_eq!(out["notice"], json!("Superseded by entities/b"));
    }

    #[test]
    fn get_context_of_a_superseded_page_returns_the_body_verbatim() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_context", json!({ "id": "entities/a" }));
        assert_eq!(out["page"]["body"], json!("Old facts.\n"));
    }

    #[test]
    fn write_page_drops_a_copied_back_superseded_line_from_the_body() {
        let tmp = superseded_vault();
        call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({
                "id": "entities/b",
                "content": page_text("entities/b", "sources: [sources/s]\n", "> Superseded by [[entities/c]]\n\nNew facts.")
            }),
        )
        .unwrap();
        let written = std::fs::read_to_string(page_file(tmp.path(), "entities/b")).unwrap();
        assert!(!written.contains("Superseded by"), "got: {written}");
    }

    #[test]
    fn get_context_of_a_current_page_has_no_superseded_field() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_context", json!({ "id": "entities/b" }));
        assert!(out.get("superseded_by").is_none(), "got: {out}");
    }

    #[test]
    fn get_context_does_not_list_the_superseded_line_as_an_outbound_link() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_context", json!({ "id": "entities/a" }));
        assert_eq!(out["outbound"], json!([]));
    }

    #[test]
    fn get_page_of_a_superseded_page_names_the_successor() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_page", json!({ "id": "entities/a" }));
        assert_eq!(out["superseded_by"], json!("entities/b"));
    }

    #[test]
    fn reading_a_superseded_page_leaves_its_file_unchanged() {
        let tmp = superseded_vault();
        let before = std::fs::read_to_string(page_file(tmp.path(), "entities/a")).unwrap();
        call_json(tmp.path(), &mut indexed(tmp.path()), "brain_get_page", json!({ "id": "entities/a" }));
        let after = std::fs::read_to_string(page_file(tmp.path(), "entities/a")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn brain_query_leaves_out_a_superseded_page_by_default() {
        let tmp = superseded_vault();
        let out = call_json(tmp.path(), &mut indexed(tmp.path()), "brain_query", json!({ "query": "type:entity" }));
        let ids: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["entities/b"]);
    }

    // ---- H3 ---------------------------------------------------------------

    #[test]
    fn get_page_counts_one_read_per_call() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        for _ in 0..3 {
            call_json(tmp.path(), &mut db, "brain_get_page", json!({ "id": "entities/alice" }));
        }
        assert_eq!(access(&db, "entities/alice").0, 3);
    }

    #[test]
    fn get_pages_counts_a_read_only_for_pages_that_were_found() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["entities/alice", "entities/missing"] }),
        );
        assert_eq!(
            (access(&db, "entities/alice").0, access(&db, "entities/missing").0),
            (1, 0)
        );
    }

    #[test]
    fn get_context_counts_a_read_of_the_page() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        call_json(tmp.path(), &mut db, "brain_get_context", json!({ "id": "entities/alice" }));
        assert_eq!(access(&db, "entities/alice").0, 1);
    }

    #[test]
    fn search_counts_a_search_hit_for_a_returned_page() {
        let tmp = vault();
        put(tmp.path(), "concepts/zebrafish", "", "The zebrafish genome.");
        let mut db = indexed(tmp.path());
        call(tmp.path(), &mut db, "brain_search", json!({ "query": "zebrafish" })).unwrap();
        assert_eq!(access(&db, "concepts/zebrafish").1, 1);
    }

    #[test]
    fn renaming_a_page_carries_its_read_count_to_the_new_id() {
        let tmp = vault();
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        put(tmp.path(), "entities/old", "", "Body.");
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        let mut db = indexed(tmp.path());
        call_json(tmp.path(), &mut db, "brain_get_page", json!({ "id": "entities/old" }));
        call(
            tmp.path(),
            &mut db,
            "brain_rename_page",
            json!({ "id": "entities/old", "new_id": "entities/new" }),
        )
        .unwrap();
        assert_eq!(
            (access(&db, "entities/old").0, access(&db, "entities/new").0),
            (0, 1)
        );
    }

    // ---- review fixes: index side jobs never build the index (S1), stale
    // rows never block (S4), get_pages dedupe ------------------------------

    #[test]
    fn get_page_on_an_unbuilt_index_does_not_build_it() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = None;
        call_json(tmp.path(), &mut db, "brain_get_page", json!({ "id": "entities/alice" }));
        assert_eq!((db.is_none(), indexed_page_count(tmp.path())), (true, 0));
    }

    #[test]
    fn page_exists_on_an_unbuilt_index_says_matches_were_not_checked() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let out = call_json(tmp.path(), &mut None, "brain_page_exists", json!({ "id": "entities/muller-gmbh" }));
        assert_eq!(out["matches_checked"], json!(false));
    }

    #[test]
    fn write_page_on_an_unbuilt_index_says_matches_were_not_checked() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_write_page",
            json!({ "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "x") }),
        );
        assert_eq!(out["matches_checked"], json!(false));
    }

    #[test]
    fn a_stale_index_row_of_a_deleted_page_does_not_block_creation() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let mut db = indexed(tmp.path());
        std::fs::remove_file(page_file(tmp.path(), "entities/mueller-gmbh")).unwrap();
        let result = call(
            tmp.path(),
            &mut db,
            "brain_write_page",
            json!({ "id": "entities/muller-gmbh", "content": page_text("entities/muller-gmbh", "", "x") }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn write_page_does_not_block_michal_next_to_michael() {
        let tmp = vault();
        put(tmp.path(), "entities/michael", "", "Body.");
        let result = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_write_page",
            json!({ "id": "entities/michal", "content": page_text("entities/michal", "", "Another person.") }),
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn get_pages_counts_one_read_for_an_id_requested_twice() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["entities/alice", "entities/alice"] }),
        );
        assert_eq!(access(&db, "entities/alice").0, 1);
    }
}

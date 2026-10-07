//! MCP server over stdio — hand-rolled JSON-RPC 2.0, dual-era.
//!
//! Serves both MCP eras from one process (see
//! `docs/research/2026-10-mcp-spec-2026-07-28-gap.md`):
//!
//! - **Legacy** (`initialize` handshake): revisions 2024-11-05, 2025-03-26,
//!   2025-06-18 and 2025-11-25. `initialize` echoes the client's requested
//!   revision when we support it, otherwise answers 2025-11-25. Features a
//!   revision does not know (tool annotations before 2025-03-26; tool
//!   `title`, `outputSchema` and `structuredContent` before 2025-06-18) are
//!   left out for that client.
//! - **Modern** (2026-07-28, stateless): the era is decided per request
//!   from `params._meta["io.modelcontextprotocol/protocolVersion"]`; no
//!   handshake. Results carry `resultType: "complete"` and
//!   `_meta["io.modelcontextprotocol/serverInfo"]`; list results and
//!   `server/discover` carry `ttlMs` + `cacheScope`.
//!
//! Methods: `initialize` + `ping` (legacy only), `server/discover` (modern
//! only), `tools/list`, `tools/call`, `prompts/list`, `prompts/get`,
//! `resources/list`, `resources/templates/list`, `resources/read`.
//! Each line on stdin is one JSON-RPC envelope; responses are one line per
//! request on stdout; notifications never get a reply.
//!
//! The server runs in the `brain mcp` subprocess. The vault path is
//! provided via the `BRAIN_VAULT_PATH` environment variable so a single
//! installed `brain` binary can serve multiple vaults across hosts.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::tools;
use crate::vault::layout::{raw_dir, wiki_dir};
use crate::viewer::{graph, search, tree};
use crate::wiki::{duplicates, history as wiki_history, lint, page, refactor};

/// Legacy (initialize-based) revisions we negotiate, oldest first.
const LEGACY_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// What `initialize` answers when the client asks for a revision we do not
/// know (or none).
const LATEST_LEGACY_VERSION: &str = "2025-11-25";
/// Modern (stateless, per-request `_meta`) revisions we serve.
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];

const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Cache hint (modern era) for the static lists and `server/discover`: the
/// lists are compiled into the binary and an update restarts the process.
const LIST_TTL_MS: u64 = 3_600_000;

/// JSON-RPC "invalid params" — also what MCP prescribes for an unknown
/// tool and for a missing required `_meta` field.
const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
/// MCP 2026-07-28 `UnsupportedProtocolVersionError`.
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
/// MCP "resource not found".
const RESOURCE_NOT_FOUND: i64 = -32002;
/// `resources/read` of a vault resource without a (reachable) vault.
/// Application-defined, deliberately outside `-32768..-32000` (the
/// JSON-RPC reserved range, whose `-32000..-32019` sub-range MCP
/// 2026-07-28 calls legacy and whose `-32020..-32099` it reserves). Tool
/// calls report a missing vault as an `isError` result instead (it was
/// `-32000` before Slice 0.3).
const NO_VAULT: i64 = -31000;

// `serverInfo.name` shown in MCP handshake / discovery responses. We use
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

/// Protocol state of one stdio process. Only the legacy era has any: the
/// revision the last `initialize` negotiated ("An `initialize` request
/// selects legacy semantics, scoped to the stdio process"). Modern
/// requests carry everything in `_meta` and never read or write this.
#[derive(Debug, Default)]
struct Session {
    legacy_version: Option<&'static str>,
}

/// Which MCP era a request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    Legacy,
    Modern,
}

/// Optional protocol features, by the revision the client speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Features {
    /// Tool `annotations` (since 2025-03-26).
    annotations: bool,
    /// Tool `title`, `outputSchema` and `structuredContent`, prompt and
    /// resource `title` (since 2025-06-18).
    structured: bool,
}

impl Features {
    const MODERN: Self = Self {
        annotations: true,
        structured: true,
    };

    fn for_legacy(version: &str) -> Self {
        // Revision ids are ISO dates, so string order is release order.
        Self {
            annotations: version >= "2025-03-26",
            structured: version >= "2025-06-18",
        }
    }
}

/// The outcome of one request before serialisation.
#[derive(Debug)]
enum Reply {
    Result(Value),
    Error {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

fn rpc_error(code: i64, message: impl Into<String>) -> Reply {
    Reply::Error {
        code,
        message: message.into(),
        data: None,
    }
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
            let rss_mb = sys
                .process(me)
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or(0);
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
    let configured_vault = std::env::var("BRAIN_VAULT_PATH").map(PathBuf::from).ok();

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
    let mut session = Session::default();

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
                        handle_request(&req, configured_vault.as_deref(), &mut db, &mut session)
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
    session: &mut Session,
) -> String {
    // JSON-RPC 2.0 §4.1: a Request object without an `id` member is a
    // Notification, and the Server MUST NOT reply to it. We catch every
    // notification here (both eras; incl. `notifications/initialized`)
    // so no arm below ever produces output for an `id`-less envelope.
    if req.id.is_none() {
        return String::new();
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    if req.jsonrpc != "2.0" {
        return error_response(&id, -32600, "expected jsonrpc 2.0", None);
    }
    let era = match detect_era(&req.params) {
        Ok(era) => era,
        Err(reply) => return render(&id, Era::Legacy, reply),
    };
    let features = match era {
        Era::Modern => Features::MODERN,
        Era::Legacy => {
            Features::for_legacy(session.legacy_version.unwrap_or(LATEST_LEGACY_VERSION))
        }
    };
    let reply = dispatch(req, era, features, vault, db, session);
    render(&id, era, reply)
}

/// Era of one request (gap doc §4): modern iff `params._meta` carries the
/// protocol version as a string. A legacy revision there (a client that
/// tags its legacy requests) stays legacy. A modern request with a
/// revision we do not serve gets `-32022`; one without
/// `clientCapabilities` gets `-32602` (both MUST in 2026-07-28).
fn detect_era(params: &Value) -> Result<Era, Reply> {
    let meta = params.get("_meta");
    let Some(version) = meta
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
    else {
        return Ok(Era::Legacy);
    };
    if LEGACY_VERSIONS.contains(&version) {
        return Ok(Era::Legacy);
    }
    if !MODERN_VERSIONS.contains(&version) {
        return Err(Reply::Error {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".to_string(),
            data: Some(json!({ "supported": MODERN_VERSIONS, "requested": version })),
        });
    }
    if meta.and_then(|m| m.get(META_CLIENT_CAPABILITIES)).is_none() {
        return Err(rpc_error(
            INVALID_PARAMS,
            format!("missing required params._meta field \"{META_CLIENT_CAPABILITIES}\""),
        ));
    }
    Ok(Era::Modern)
}

/// The revision `initialize` answers: the requested one when we serve it,
/// else our latest legacy revision.
fn negotiate_legacy(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| LEGACY_VERSIONS.iter().find(|v| **v == r).copied())
        .unwrap_or(LATEST_LEGACY_VERSION)
}

fn server_info() -> Value {
    json!({ "name": SERVER_NAME, "version": SERVER_VERSION })
}

/// Usage hint for the client (`instructions` of `initialize` — part of
/// InitializeResult in every legacy revision — and of `server/discover`).
const INSTRUCTIONS: &str = "BRAIN is the user's wiki memory. Read resource brain://agents-md before \
writing pages. Check brain_lookup before creating a page; write linked pages with \
brain_write_batch; never overwrite facts — supersede.";

fn capabilities() -> Value {
    json!({
        "tools": { "listChanged": false },
        "prompts": { "listChanged": false },
        "resources": { "listChanged": false }
    })
}

/// Serialise a reply. Modern results get `resultType: "complete"` and
/// `_meta[serverInfo]`; legacy results stay as the handler built them.
fn render(id: &Value, era: Era, reply: Reply) -> String {
    match reply {
        Reply::Result(mut result) => {
            if era == Era::Modern {
                if let Value::Object(map) = &mut result {
                    map.insert("resultType".into(), json!("complete"));
                    let meta = map.entry("_meta").or_insert_with(|| json!({}));
                    if let Value::Object(meta) = meta {
                        meta.insert(META_SERVER_INFO.into(), server_info());
                    }
                }
            }
            ok_response(id, result)
        }
        Reply::Error {
            code,
            message,
            data,
        } => error_response(id, code, &message, data),
    }
}

/// Adds the modern-era cache hints to a cacheable result.
fn cacheable(era: Era, mut result: Value, scope: &str, ttl_ms: u64) -> Value {
    if era == Era::Modern {
        result["ttlMs"] = json!(ttl_ms);
        result["cacheScope"] = json!(scope);
    }
    result
}

fn dispatch(
    req: &RpcRequest,
    era: Era,
    features: Features,
    vault: Option<&std::path::Path>,
    db: &mut Option<crate::db::DbHandle>,
    session: &mut Session,
) -> Reply {
    let legacy_only = |method: &str| {
        rpc_error(
            METHOD_NOT_FOUND,
            format!(
                "{method} is not part of MCP {} (stateless, per-request _meta) — only legacy clients use it",
                MODERN_VERSIONS[0]
            ),
        )
    };
    match req.method.as_str() {
        "initialize" => {
            if era == Era::Modern {
                return legacy_only("initialize");
            }
            let requested = req.params.get("protocolVersion").and_then(Value::as_str);
            let negotiated = negotiate_legacy(requested);
            session.legacy_version = Some(negotiated);
            Reply::Result(json!({
                "protocolVersion": negotiated,
                "serverInfo": server_info(),
                "capabilities": capabilities(),
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => match era {
            Era::Legacy => Reply::Result(json!({})),
            Era::Modern => legacy_only("ping"),
        },
        "server/discover" => match era {
            Era::Modern => Reply::Result(cacheable(
                era,
                json!({
                    "supportedVersions": MODERN_VERSIONS,
                    "capabilities": capabilities(),
                    "instructions": INSTRUCTIONS,
                    "_meta": { META_SERVER_INFO: server_info() }
                }),
                "public",
                LIST_TTL_MS,
            )),
            Era::Legacy => {
                tracing::info!(
                    "mcp: client probed server/discover without the 2026-07-28 _meta — answering \
                     -32601 so a dual-era client falls back to the initialize handshake"
                );
                rpc_error(METHOD_NOT_FOUND, server_discover_legacy_message())
            }
        },
        "tools/list" => Reply::Result(cacheable(
            era,
            json!({ "tools": tool_descriptors(features) }),
            "public",
            LIST_TTL_MS,
        )),
        "tools/call" => tools_call(&req.params, features, vault, db),
        "prompts/list" => Reply::Result(cacheable(
            era,
            json!({ "prompts": prompt_descriptors(features) }),
            "public",
            LIST_TTL_MS,
        )),
        "prompts/get" => prompt_get(&req.params),
        "resources/list" => Reply::Result(cacheable(
            era,
            json!({ "resources": resource_descriptors(features) }),
            "public",
            LIST_TTL_MS,
        )),
        "resources/templates/list" => Reply::Result(cacheable(
            era,
            json!({ "resourceTemplates": [] }),
            "public",
            LIST_TTL_MS,
        )),
        // Vault content: private to this user and changes any time.
        "resources/read" => match resource_read(&req.params, vault) {
            Reply::Result(v) => Reply::Result(cacheable(era, v, "private", 0)),
            err => err,
        },
        _ => rpc_error(
            METHOD_NOT_FOUND,
            format!("method not found: {}", req.method),
        ),
    }
}

/// `-32601` message for a `server/discover` WITHOUT the modern `_meta`.
/// Per the 2026-07-28 stdio binding ("Backward Compatibility"), a dual-era
/// client treats any error that is not a recognised modern error as
/// "legacy server" and falls back to `initialize` — so this code must stay
/// a non-modern one. The message makes the failure diagnosable.
fn server_discover_legacy_message() -> String {
    format!(
        "server/discover needs the MCP {} per-request params._meta (\"{META_PROTOCOL_VERSION}\", \
         \"{META_CLIENT_CAPABILITIES}\"); legacy clients use the initialize handshake",
        MODERN_VERSIONS[0]
    )
}

/// `tools/call`. The tool name is validated BEFORE the vault gate: a
/// removed or unknown name is a protocol error (`-32602`, both eras) that
/// needs no vault. `brain_ping` is answered here too, before the gate.
fn tools_call(
    params: &Value,
    features: Features,
    vault: Option<&std::path::Path>,
    db: &mut Option<crate::db::DbHandle>,
) -> Reply {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(INVALID_PARAMS, "missing tool 'name'");
    };
    if !tools::is_known(name) {
        return match tools::removed(name) {
            Some(r) => rpc_error(INVALID_PARAMS, tools::replaced_message(r)),
            None => rpc_error(INVALID_PARAMS, format!("Unknown tool: {name}")),
        };
    }
    // A missing (without default) or unknown `action` is a malformed call,
    // like an unknown tool: protocol error, no vault needed.
    let action = params.get("arguments").and_then(|a| a.get("action"));
    if let Err(err) = tools::resolve_action(name, action) {
        return rpc_error(INVALID_PARAMS, err);
    }
    // `brain_ping` is a pure liveness probe: by default it never touches
    // the filesystem or DB, even when no vault is configured or the disk
    // is hung. This is the contract the 0.2.19 pre-flight probe
    // accidentally broke; keeping ping above the gate is what restores
    // "ping always answers". `detail: true` opts into a bounded look at
    // the vault, model and index (see `brain_ping_detail`).
    if name == "brain_ping" {
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let payload = match args.get("detail") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(brain_ping_payload()),
            Some(Value::Bool(true)) => Ok(brain_ping_detail(vault, db)),
            Some(_) => Err("'detail' must be a boolean".to_string()),
        };
        return tool_result(payload, features);
    }
    match vault {
        Some(v) => tool_result(call_tool(params, v, db), features),
        // A tool error the model can read and relay (like
        // BRAIN_VAULT_DISCONNECTED), not a protocol error.
        None => tool_result(Err(no_vault_message().to_string()), features),
    }
}

/// The tool error (and resource-read error message) when the server has
/// no vault configured at all.
fn no_vault_message() -> &'static str {
    "BRAIN_VAULT_NOT_CONFIGURED: no Brain vault is mounted on this host. Tell the user to \
     open BRAIN, mount the vault and register this client again."
}

/// The `tools/call` result for a tool's outcome. The text block carries
/// compact JSON (or the plain text a tool returned); clients that speak
/// 2025-06-18+ also get the object as `structuredContent`. A tool error
/// is an `isError` result the model can read, never a protocol error.
fn tool_result(outcome: Result<String, String>, features: Features) -> Reply {
    match outcome {
        Ok(text) => {
            let parsed = serde_json::from_str::<Value>(&text).ok();
            let text = parsed.as_ref().map(Value::to_string).unwrap_or(text);
            let mut result = json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            });
            if features.structured {
                if let Some(object @ Value::Object(_)) = parsed {
                    result["structuredContent"] = object;
                }
            }
            Reply::Result(result)
        }
        Err(err) => Reply::Result(json!({
            "content": [{ "type": "text", "text": err }],
            "isError": true
        })),
    }
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
    /// The revision we answer this client with: the negotiated legacy
    /// revision for `initialize`, the modern revision when we serve it,
    /// else `"unsupported"`.
    fn negotiated_version(&self) -> &'static str {
        if self.style == "initialize" {
            negotiate_legacy(Some(&self.protocol_version))
        } else {
            MODERN_VERSIONS
                .iter()
                .find(|v| **v == self.protocol_version)
                .copied()
                .unwrap_or("unsupported")
        }
    }

    fn log(&self) {
        tracing::info!(
            style = self.style,
            method = %self.method,
            client_protocol_version = %self.protocol_version,
            client_name = %self.client_name,
            client_version = %self.client_version,
            negotiated_protocol_version = self.negotiated_version(),
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
            .and_then(|m| m.get(META_PROTOCOL_VERSION))
            .or_else(|| params.and_then(|p| p.get("protocolVersion")))
            .or_else(|| envelope.get("protocolVersion"))?;
        let client_info = meta
            .and_then(|m| m.get(META_CLIENT_INFO))
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
/// `tools_call` fast path and kept as a function so the contract
/// (zero I/O) is obvious and testable.
fn brain_ping_payload() -> String {
    brain_ping_value().to_string()
}

fn brain_ping_value() -> Value {
    json!({
        "status": "ok",
        "server": SERVER_NAME,
        "version": SERVER_VERSION,
        "uptime_seconds": process_uptime_seconds(),
    })
}

/// `brain_ping` with `detail: true` (formerly a separate status tool):
/// the ping payload plus the vault, the embedding model and the index —
/// all via non-building, bounded paths. The model is never loaded (only
/// the process cache is peeked) and the index is never built or created
/// (see [`ping_index_counts`]).
fn brain_ping_detail(vault: Option<&std::path::Path>, db: &Option<crate::db::DbHandle>) -> String {
    let mut payload = brain_ping_value();
    let Some(vault) = vault else {
        payload["vault"] = json!({ "configured": false });
        return payload.to_string();
    };
    // The file-system stats run on a worker thread bounded like the index
    // reads: a hung disk must not stall the ping (the thread is left
    // behind on a timeout).
    let (tx, rx) = std::sync::mpsc::channel();
    let probe_vault = vault.to_path_buf();
    std::thread::spawn(move || {
        let reachable = crate::vault::layout::is_vault(&probe_vault);
        let files = reachable && crate::embedding::model_available(&probe_vault);
        let _ = tx.send((reachable, files));
    });
    let Ok((reachable, files_present)) = rx.recv_timeout(COUNTER_DB_TIMEOUT) else {
        payload["vault"] = json!({
            "configured": true,
            "reachable": false,
            "path": display_path(vault),
            "note": "the vault did not answer within 2 s — the disk may be hung",
        });
        return payload.to_string();
    };
    payload["vault"] = json!({
        "configured": true,
        "reachable": reachable,
        "path": display_path(vault),
    });
    if !reachable {
        return payload.to_string();
    }
    let state = crate::embedding::model_state(vault);
    // With complete model files the next search uses (and, if needed,
    // loads) bge-m3 — unless a load already failed for these files.
    let semantic = files_present && state != "failed";
    let fallback = crate::embedding::hashed::HashedEmbedder::new();
    payload["embedder"] = json!({
        "active": if semantic { "bge-m3" } else { crate::embedding::Embedder::name(&fallback) },
        "semantic": semantic,
        "model_files_present": files_present,
        "model_state": state,
        "model_dir": display_path(&crate::vault::layout::models_dir(vault).join("bge-m3")),
        "dim": crate::embedding::EMBED_DIM,
    });
    let counts = ping_index_counts(db, vault).filter(|(pages, _)| *pages > 0);
    payload["index"] = match counts {
        Some((pages, chunks)) => json!({ "available": true, "pages": pages, "chunks": chunks }),
        None => json!({
            "available": false,
            "note": "index not built yet, or not readable within 2 s"
        }),
    };
    payload.to_string()
}

/// (pages, chunks) of the index for `brain_ping detail`, or `None`. Never
/// creates anything: without a held handle and without a database file in
/// `03_db/` it does not open one (opening would create the file and its
/// schema), and opening an existing file runs on a worker thread bounded
/// by `COUNTER_DB_TIMEOUT` — like the count itself — so a hung disk cannot
/// stall the ping.
fn ping_index_counts(
    db: &Option<crate::db::DbHandle>,
    vault: &std::path::Path,
) -> Option<(i64, i64)> {
    let handle = match db {
        Some(handle) => handle.clone(),
        None => {
            let file = crate::vault::layout::db_dir(vault).join(crate::db::DB_FILENAME);
            if !file.is_file() {
                return None;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let vault = vault.to_path_buf();
            std::thread::spawn(move || {
                let _ = tx.send(crate::db::DbHandle::open(&vault));
            });
            rx.recv_timeout(COUNTER_DB_TIMEOUT).ok()?.ok()?
        }
    };
    let counted = handle.with_timeout(COUNTER_DB_TIMEOUT, |conn| {
        let pages: i64 = conn.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))?;
        let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok((pages, chunks))
    });
    match counted {
        Ok(Ok(counts)) => Some(counts),
        _ => None,
    }
}

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
            tracing::warn!(
                ?err,
                op = op_name,
                "DB op hit a fatal connection error; reopening"
            );
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

// ---- Tool descriptors ------------------------------------------------------

/// Behaviour hints of a tool (MCP `ToolAnnotations`, 2025-03-26+). Every
/// BRAIN tool works on the local vault only, so `openWorldHint` is false.
#[derive(Clone, Copy)]
struct Hints {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
}

const READ_ONLY: Hints = Hints {
    read_only: true,
    destructive: false,
    idempotent: true,
};

/// One tool as `tools/list` advertises it, before per-revision trimming.
struct ToolSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input: Value,
    output: Value,
    hints: Hints,
}

/// `response_format` input property: what each level includes.
fn response_format_schema(concise: &str, detailed: &str) -> Value {
    json!({
        "type": "string",
        "enum": ["concise", "detailed"],
        "default": "concise",
        "description": format!("concise (default): {concise}. detailed: {detailed}.")
    })
}

fn page_id_schema() -> Value {
    json!({ "type": "string", "description": "page id `<type dir>/<slug>`, e.g. 'entities/alice'" })
}

/// A loose object schema for `outputSchema`: names and coarse types of
/// the top-level fields only, nothing required, extra fields allowed — a
/// client that validates `structuredContent` against it must never reject
/// a legitimate result (concise and detailed share one schema).
fn object_schema(properties: Value) -> Value {
    json!({ "type": "object", "properties": properties })
}

fn tool_specs() -> Vec<ToolSpec> {
    let write = |destructive: bool, idempotent: bool| Hints {
        read_only: false,
        destructive,
        idempotent,
    };
    vec![
        ToolSpec {
            name: "brain_ping",
            title: "Ping BRAIN",
            description: "Liveness probe: server status, version and uptime, answered instantly even without a vault. Use it between bulk-write batches or when another BRAIN tool seems stuck. Not for page data — use brain_search or brain_get_pages. `detail: true` adds the vault, the embedding model (semantic bge-m3 or the hashed fallback) and index counts — it reads only what exists, never loads the model or creates the index: use it when search results look non-semantic.",
            input: json!({
                "type": "object",
                "properties": {
                    "detail": { "type": "boolean", "default": false, "description": "also report vault, embedding model and index (reads the vault, time-bounded; never loads or creates anything)" }
                }
            }),
            output: object_schema(json!({
                "status": { "type": "string" },
                "server": { "type": "string" },
                "version": { "type": "string" },
                "uptime_seconds": { "type": "integer" },
                "vault": { "type": "object" },
                "embedder": { "type": "object" },
                "index": { "type": "object" }
            })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_search",
            title: "Search the wiki",
            description: "Ranked free-text search over all pages (full-text + semantic, fused). Use it to find pages about something when you do not know their ids. Not for filtering or listing by type/tag/date — use brain_query; not for checking whether a page exists before creating one — use brain_lookup.",
            input: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "words or a question" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20, "default": 10, "description": "maximum hits" },
                    "response_format": response_format_schema("id, title, score and a plain snippet of at most 80 characters (the summary when the page has one) per hit", "also path and the highlighted full-text snippet")
                },
                "required": ["query"]
            }),
            output: object_schema(json!({ "hits": { "type": "array" } })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_lookup",
            title: "Look up a page name",
            description: "Cheap existence and duplicate check, no page bodies. Use it before creating a page: pass the planned id ('entities/acme') or just a name ('ACME Corp'). Returns `exists` and `matches` of the same type, each {id, title, reason}: `exact` (name form), `alias` (a frontmatter alias), `normalised` (same slug after lowercasing, ä→ae/ö→oe/ü→ue/ß→ss, punctuation → '-') or `similar` (a letter or two apart). Any match: update that page (add your name to its aliases) instead of creating a duplicate. `matches_checked: false` = index not built, no matches looked up. Not for reading content — use brain_get_pages.",
            input: json!({
                "type": "object",
                "properties": {
                    "query_or_id": { "type": "string", "description": "a page id like 'entities/acme' (checks that type) or a bare name like 'ACME Corp' (checks all four types)" }
                },
                "required": ["query_or_id"]
            }),
            output: object_schema(json!({
                "query_or_id": { "type": "string" },
                "id": { "type": "string" },
                "exists": { "type": "boolean" },
                "matches": { "type": "array" },
                "matches_checked": { "type": "boolean" }
            })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_get_pages",
            title: "Read pages",
            description: "Read one or more pages by id (one page: `ids: ['entities/alice']`). Use it once brain_search, brain_query or brain_lookup told you which pages matter. One entry per id in request order: {id, found: true, page} or {id, found: false, error} — a missing id never fails the call. `include_context: true` adds each page's 1-hop neighbourhood: `outbound` (ids it links to) and `backlinks` (pages linking to it) — use it before editing, merging or deleting a page. A superseded page carries `superseded_by` and `notice`: read the successor for current facts and never copy the notice into a page. Not for finding pages — use brain_search or brain_query.",
            input: json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "page ids, e.g. ['entities/alice', 'concepts/nlspec']" },
                    "include_context": { "type": "boolean", "default": false, "description": "also return outbound links and backlinks per page" },
                    "response_format": response_format_schema("id, title, summary and body; context as ids", "also the full frontmatter (JSON) — use detailed before rewriting a page with brain_write_page — and backlinks with title and path")
                },
                "required": ["ids"]
            }),
            output: object_schema(json!({ "pages": { "type": "array" } })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_query",
            title: "List and filter pages",
            description: "Structured listing by page metadata, newest `updated` first. Use it to list or filter pages (type, tag, title, dates, validity, most-read) and to count tags. Not for relevance search over text — use brain_search. Syntax: fields id, type (entity|concept|source|topic), title, tag, created, updated with `:` (equals), `:>`, `:<`; AND, OR, NOT, parentheses, \"quoted values\". Empty or `*` lists all current pages. Superseded/expired pages are hidden unless `valid:all` (`valid:expired` = only those). `sort:salience` (top level, with AND) puts the most-read pages first. Examples: `type:entity AND tag:customer`, `updated:>2026-04-01 AND NOT type:source`, `type:entity AND sort:salience`. Returns at most `limit` (default 100) hits; when `next_offset` is present, call again with `offset: next_offset`.",
            input: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "default": "*", "description": "filter expression; empty or '*' = all current pages" },
                    "prefix": { "type": "string", "description": "only ids starting with this, e.g. 'entities/acme'" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100, "description": "maximum hits returned (at most 500 per call); `total` says how many matched" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "skip this many hits (use `next_offset` of the previous call)" },
                    "facet": { "type": "string", "enum": ["tags"], "description": "'tags': return {tags: [{tag, count}]} over the matching pages instead of hits — use it to learn which tags exist before filtering with tag:" },
                    "response_format": response_format_schema("id, type and title per hit", "also path, updated_at, read/search-hit counters, validity fields, tags and summary")
                }
            }),
            output: object_schema(json!({
                "total": { "type": "integer" },
                "offset": { "type": "integer" },
                "returned": { "type": "integer" },
                "next_offset": { "type": "integer" },
                "hits": { "type": "array" },
                "facet": { "type": "string" },
                "tags": { "type": "array" },
                "note": { "type": "string" }
            })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_graph",
            title: "Link graph",
            description: "The wiki's link graph (nodes + edges), optionally only some page types. Use it for structure analysis: hubs, clusters, isolated pages. Not for one page's neighbours — use brain_get_pages with include_context.",
            input: json!({
                "type": "object",
                "properties": {
                    "types": { "type": "array", "items": { "type": "string", "enum": ["entity", "concept", "source", "topic"] }, "description": "only nodes of these page types (default: all)" },
                    "response_format": response_format_schema("node ids, edges as [source, target] pairs, counts", "nodes with type, title and tags, edges as {source, target}")
                }
            }),
            output: object_schema(json!({
                "node_count": { "type": "integer" },
                "edge_count": { "type": "integer" },
                "nodes": { "type": "array" },
                "edges": { "type": "array" }
            })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_write_page",
            title: "Write a page",
            description: "Create or fully overwrite ONE page. Use it for a new page or to rewrite a page including its frontmatter (summary, aliases, superseded_by). Not for several pages that link to each other — use brain_write_batch; not for changing one section — use brain_patch_page. `content` = YAML frontmatter (id, type: entity|concept|source|topic — singular, title, summary: one or two sentences; optional tags, aliases, sources: [sources/…], valid_from/valid_to YYYY-MM-DD, superseded_by, distinct_from, keep: true) followed by the markdown body; link pages as [[type-dir/slug]]. Quote a summary (or title) that contains ': ' or starts with a special character ([ { & * ! | > ' \" % @ `), e.g. summary: \"GRASP (auch CIO COCKPIT): SaaS-Cockpit …\". Creating a NEW id is refused when brain_lookup would report an alias or normalised match (the error names the page); overwriting an existing id never is. Facts are not overwritten: supersede the old page instead (superseded_by + valid_to). Rewriting an existing page: read it first with brain_get_pages (response_format \"detailed\") and carry over EVERY frontmatter field unchanged (aliases, sources, tags, superseded_by, valid_from/valid_to, distinct_from, keep) — a concise read has no frontmatter, and a dropped field is lost. Returns {wrote, previous_size_bytes, new_size_bytes, warnings}; lint errors on this page fail the call and list the findings. The watcher commits.",
            input: json!({
                "type": "object",
                "properties": {
                    "id": page_id_schema(),
                    "content": { "type": "string", "description": "full markdown file: frontmatter + body" },
                    "allow_duplicate": { "type": "boolean", "default": false, "description": "create the page although an alias/normalised match exists — only for genuinely different things (then set distinct_from)" },
                    "confirm_summary": { "type": "boolean", "default": false, "description": "the page's summary is still accurate for this body: clears its summary-stale dream-queue item" }
                },
                "required": ["id", "content"]
            }),
            output: object_schema(json!({
                "wrote": { "type": "string" },
                "previous_size_bytes": { "type": "integer" },
                "new_size_bytes": { "type": "integer" },
                "warnings": { "type": "array" },
                "matches_checked": { "type": "boolean" },
                "summary_confirmed": { "type": "boolean" }
            })),
            hints: write(true, true),
        },
        ToolSpec {
            name: "brain_write_batch",
            title: "Write several pages atomically",
            description: "Atomic multi-page write: every entry is validated first (one bad entry → nothing is written), all are written, then lint runs once. Use it whenever new pages link to each other (ingest), so the links between them resolve. Not for a single page — use brain_write_page. Entries have the brain_write_page format (for existing pages: read them with response_format \"detailed\" and carry over every frontmatter field; quote a summary or title that contains ': ' or starts with a special character); the duplicate refusal applies too, also against earlier entries of the batch. Returns {wrote: [{id, previous_size_bytes, new_size_bytes, warnings}]}. The watcher commits.",
            input: json!({
                "type": "object",
                "properties": {
                    "pages": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" },
                                "allow_duplicate": { "type": "boolean" }
                            },
                            "required": ["id", "content"]
                        }
                    },
                    "allow_duplicate": { "type": "boolean", "default": false, "description": "allow_duplicate for every entry" }
                },
                "required": ["pages"]
            }),
            output: object_schema(json!({
                "wrote": { "type": "array" },
                "matches_checked": { "type": "boolean" }
            })),
            hints: write(true, true),
        },
        ToolSpec {
            name: "brain_patch_page",
            title: "Replace one section",
            description: "Replace one section of an existing page: from the `heading` line (e.g. '## Kontakt') to the next heading of the same or higher level; the section is appended when the heading is missing. Use it for targeted updates — small diff, frontmatter untouched. Not for creating a page or editing frontmatter — use brain_write_page.",
            input: json!({
                "type": "object",
                "properties": {
                    "id": page_id_schema(),
                    "heading": { "type": "string", "description": "the full heading line, e.g. '## Kontakt'" },
                    "content": { "type": "string", "description": "the new section body, without the heading line" },
                    "confirm_summary": { "type": "boolean", "default": false, "description": "the page's summary is still accurate for the patched body: clears its summary-stale dream-queue item" }
                },
                "required": ["id", "heading", "content"]
            }),
            output: object_schema(json!({
                "wrote": { "type": "string" },
                "previous_size_bytes": { "type": "integer" },
                "new_size_bytes": { "type": "integer" },
                "warnings": { "type": "array" },
                "summary_confirmed": { "type": "boolean" }
            })),
            hints: write(true, true),
        },
        ToolSpec {
            name: "brain_refactor",
            title: "Rename, merge or delete a page",
            description: "Fix the structure of the wiki; every change rewrites the references in the vault and records one commit, and the old state stays restorable with brain_history. action 'rename' (`id`, `new_id`): a page created under a WRONG id (typo, wrong slug or type directory) gets its correct id; [[old]], [[old|Alias]], [Text](old), superseded_by and sources entries follow. action 'merge' (`from_id`, `into_id`): a DUPLICATE is folded into the page that survives — body appended under '## Merged from <from_id>', tags and aliases united, links redirected, from_id removed; tidy the appended section afterwards with brain_patch_page. action 'delete' (`id`, optional `force`): a page that should not exist at all (junk, test page); refuses while other pages refer to it and lists them, `force: true` deletes anyway and turns those links into plain text. Not for editing content — use brain_write_page or brain_patch_page. Returns the action, the ids, rewritten_pages/rewritten_links (or defused_in/defused_links) and `commit`; without `commit` but with a `note`, the change is already on disk — do not repeat it.",
            input: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["rename", "merge", "delete"], "description": "rename: needs id + new_id; merge: needs from_id + into_id; delete: needs id (optional force)" },
                    "id": { "type": "string", "description": "rename/delete: the page id, e.g. 'entities/dan-shapio'" },
                    "new_id": { "type": "string", "description": "rename: correct id `<entities|concepts|sources|topics>/<slug>`, slug of letters, digits, '.', '_', '-'; must not exist yet (if it does, merge instead)" },
                    "from_id": { "type": "string", "description": "merge: the duplicate to fold in and remove" },
                    "into_id": { "type": "string", "description": "merge: the page that survives" },
                    "force": { "type": "boolean", "default": false, "description": "delete: delete even while other pages refer to it" }
                },
                "required": ["action"]
            }),
            output: object_schema(json!({
                "action": { "type": "string" },
                "old_id": { "type": "string" },
                "new_id": { "type": "string" },
                "from_id": { "type": "string" },
                "into_id": { "type": "string" },
                "deleted": { "type": "string" },
                "rewritten_pages": { "type": "array" },
                "rewritten_links": { "type": "integer" },
                "rewritten_references": { "type": "integer" },
                "defused_in": { "type": "array" },
                "defused_links": { "type": "integer" },
                "removed_references": { "type": "integer" },
                "commit": { "type": "string" },
                "note": { "type": "string" }
            })),
            hints: write(true, false),
        },
        ToolSpec {
            name: "brain_lint_report",
            title: "Lint report",
            description: "The lint state of the whole wiki. Use it at the start and end of a cleanup session and after bulk writes. Not needed after a single write — brain_write_page and brain_write_batch responses already carry that page's findings. Errors block auto-commits (broken-link, frontmatter, duplicate-id, unregistered-type, dangling-supersede, supersede-cycle); warnings are advice (missing-summary, missing-sources, missing-title, orphan, duplicate-candidate, alias-collision, broken-source, invalid-date, expired-but-linked, non-canonical-wiki-link, …). Work one kind at a time: concise for the overview, then detailed with `kind`. The `lint-session` prompt has the fix for each kind.",
            input: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "only findings of this kind, e.g. 'broken-link'" },
                    "response_format": response_format_schema("every error, warning counts per kind, notes", "every error and warning with path, kind and message")
                }
            }),
            output: object_schema(json!({
                "error_count": { "type": "integer" },
                "warning_count": { "type": "integer" },
                "warning_kinds": { "type": "object" },
                "errors": { "type": "array" },
                "warnings": { "type": "array" },
                "notes": { "type": "array" },
                "hint": { "type": "string" }
            })),
            hints: READ_ONLY,
        },
        ToolSpec {
            name: "brain_history",
            title: "Page history and restore",
            description: "Git history of one page. action 'list' (default; `id`, optional `limit`): the commits that touched the page, newest first, {sha, ts, message, files_changed} — use it to find the version to restore after a bad overwrite, merge or delete (pass the OLD id for a removed page) or to see how a fact changed. action 'restore' (`id`, `sha`): replace the page with its version at that sha and record a `revert:` commit — history stays append-only; confirm with the user first, later changes to the page are dropped. Not for the current content — use brain_get_pages; not for small corrections — use brain_patch_page.",
            input: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "restore"], "default": "list", "description": "list: needs id (optional limit); restore: needs id + sha" },
                    "id": { "type": "string", "description": "page id, e.g. 'entities/alice' ('.md' optional)" },
                    "limit": { "type": "integer", "minimum": 1, "default": 20, "description": "list: maximum commits" },
                    "sha": { "type": "string", "description": "restore: commit sha from action 'list' — full, or a unique prefix of at least 4 hex digits" }
                },
                "required": ["id"]
            }),
            output: object_schema(json!({
                "action": { "type": "string" },
                "commits": { "type": "array" },
                "restored": { "type": "string" },
                "from_sha": { "type": "string" }
            })),
            hints: write(true, true),
        },
        ToolSpec {
            name: "brain_write_raw_file",
            title: "Store a raw artifact",
            description: "Store a raw artifact (email, transcript, exported document) verbatim under 01_raw/<connector>/<relative_path>. Use it as the first ingest step, before writing the `source` page that summarises it. Not for wiki pages — use brain_write_page or brain_write_batch. Returns {wrote: '01_raw/…'}.",
            input: json!({
                "type": "object",
                "properties": {
                    "connector": { "type": "string", "description": "source channel, e.g. 'email', 'notes', 'confluence'" },
                    "relative_path": { "type": "string", "description": "plain relative path, e.g. '2026-10-06-kickoff.txt' (no '..', drive letters or leading '/')" },
                    "content": { "type": "string" }
                },
                "required": ["connector", "relative_path", "content"]
            }),
            output: object_schema(json!({ "wrote": { "type": "string" } })),
            hints: write(true, true),
        },
        ToolSpec {
            name: "brain_eval",
            title: "Search-quality eval",
            description: "Search-quality measurement on the vault's eval set (00_meta/eval-queries.yaml). action 'run' (default): Recall@10, MRR and nDCG@10 for full-text, vector and hybrid search plus per-question hits and misses, appended to 00_meta/eval-history.md — use it before and after changing summaries or search settings. action 'add': store one test question (`query`) with the page ids a good search must return (`expected`, existing pages; optional `id`, `note`) — e.g. after the user says a search missed something. Not for searching — use brain_search.",
            input: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["run", "add"], "default": "run", "description": "'run' the eval set (default) or 'add' a question to it (needs query + expected)" },
                    "query": { "type": "string", "description": "add: the question as the user would ask it" },
                    "expected": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "add: page ids a good search returns in its top 10" },
                    "id": { "type": "string", "description": "add: optional stable name, e.g. 'q-kunde-a-laufzeit'" },
                    "note": { "type": "string", "description": "add: optional note" }
                }
            }),
            output: object_schema(json!({
                "queries": { "type": "integer" },
                "modes": { "type": "array" },
                "per_query": { "type": "array" },
                "added": { "type": "object" }
            })),
            hints: write(false, false),
        },
        ToolSpec {
            name: "brain_dream",
            title: "Dream (consolidate the wiki)",
            description: "Consolidation ('dreaming') — only when the user asks for it ('träum mal', 'tidy up the wiki'). action 'queue': BRAIN's prioritised work list {generated_at, items: [{priority 1..3, kind, pages, reason, suggested_action}], omitted}, served from 00_meta/dream-queue.md when under an hour old (`refresh: true` recomputes). action 'log' (once, at the end of the session): `entry` — one line saying what you changed and why — plus `items`, every queue item you looked at with its outcome (done / skipped with a one-line reason / deferred); appended to 00_meta/dream-log.md. Items skipped or deferred before carry `skipped_before` in later queues. action 'stats': what the dream log says so far {sessions, items_total, per_kind: [{kind, done, skipped, deferred}], most_skipped: [{kind, pages, count}] (skips since the item was last done, top 10)} — e.g. to tell the user which items keep being skipped. Work the queue with the `dream` prompt: at most 10 changes, never delete linked pages, supersede instead of overwrite. Not for a lint cleanup — use brain_lint_report.",
            input: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["queue", "log", "stats"], "description": "'queue' to read the work list, 'log' to record the session (needs entry), 'stats' for the dream-log summary" },
                    "refresh": { "type": "boolean", "default": false, "description": "queue: recompute even if the stored queue is fresh" },
                    "entry": { "type": "string", "description": "log: one line for the session, e.g. 'merged entities/acme-inc into entities/acme; summaries for 3 hubs'" },
                    "items": {
                        "type": "array",
                        "description": "log: every queue item you looked at, with what you did — skipped and deferred items show up as `skipped_before` in later queues",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "description": "the queue item's kind, e.g. 'orphan'" },
                                "pages": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "the queue item's pages" },
                                "outcome": { "type": "string", "enum": ["done", "skipped", "deferred"] },
                                "note": { "type": "string", "description": "one line: what you did, or why you skipped / deferred it" }
                            },
                            "required": ["kind", "pages", "outcome"]
                        }
                    }
                },
                "required": ["action"]
            }),
            output: object_schema(json!({
                "generated_at": { "type": "string" },
                "items": { "type": "array" },
                "omitted": { "type": "integer" },
                "notes": { "type": "array" },
                "logged": { "type": "string" },
                "items_logged": { "type": "integer" },
                "sessions": { "type": "integer" },
                "items_total": { "type": "integer" },
                "per_kind": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string" },
                            "done": { "type": "integer" },
                            "skipped": { "type": "integer" },
                            "deferred": { "type": "integer" }
                        }
                    }
                },
                "most_skipped": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string" },
                            "pages": { "type": "array", "items": { "type": "string" } },
                            "count": { "type": "integer" }
                        }
                    }
                }
            })),
            hints: write(false, false),
        },
    ]
}

/// `tools/list` entries for a client with `features`: `title` and
/// `outputSchema` from 2025-06-18, `annotations` from 2025-03-26.
fn tool_descriptors(features: Features) -> Vec<Value> {
    tool_specs()
        .into_iter()
        .map(|spec| {
            let mut tool = json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.input,
            });
            if features.structured {
                tool["title"] = json!(spec.title);
                tool["outputSchema"] = spec.output;
            }
            if features.annotations {
                tool["annotations"] = json!({
                    "title": spec.title,
                    "readOnlyHint": spec.hints.read_only,
                    "destructiveHint": spec.hints.destructive,
                    "idempotentHint": spec.hints.idempotent,
                    "openWorldHint": false
                });
            }
            tool
        })
        .collect()
}

// ---- Prompts ---------------------------------------------------------------

/// One MCP prompt argument: (name, description, required).
type PromptArg = (&'static str, &'static str, bool);

struct PromptSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    arguments: &'static [PromptArg],
}

const PROMPTS: &[PromptSpec] = &[
    PromptSpec {
        name: "ingest",
        title: "Ingest into BRAIN",
        description: "Turn a raw artifact into wiki pages: raw file → source page → entity/concept pages, written in one brain_write_batch.",
        arguments: &[
            (
                "source",
                "what to ingest: a path under 01_raw/, pasted text, or a description of the material",
                true,
            ),
            (
                "connector",
                "01_raw/ sub-folder for the raw file, e.g. 'email' or 'notes' (default: pick a fitting one)",
                false,
            ),
        ],
    },
    PromptSpec {
        name: "lint-session",
        title: "Wiki cleanup session",
        description: "Work through the newest audit / lint report and fix the findings kind by kind.",
        arguments: &[(
            "focus",
            "only this finding kind, e.g. 'broken-link' (default: all, errors first)",
            false,
        )],
    },
    PromptSpec {
        name: "dream",
        title: "Dream (consolidate the wiki)",
        description: "User-triggered consolidation: read the dream queue, work it top-down under hard rules, end with a dream-log entry.",
        arguments: &[(
            "max_changes",
            "maximum changes in this session (default 10)",
            false,
        )],
    },
];

fn prompt_descriptors(features: Features) -> Vec<Value> {
    PROMPTS
        .iter()
        .map(|p| {
            let arguments: Vec<Value> = p
                .arguments
                .iter()
                .map(|(name, description, required)| {
                    json!({ "name": name, "description": description, "required": required })
                })
                .collect();
            let mut prompt = json!({
                "name": p.name,
                "description": p.description,
                "arguments": arguments,
            });
            if features.structured {
                prompt["title"] = json!(p.title);
            }
            prompt
        })
        .collect()
}

/// `prompts/get`: the prompt text with its arguments filled in.
fn prompt_get(params: &Value) -> Reply {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(INVALID_PARAMS, "missing prompt 'name'");
    };
    let Some(spec) = PROMPTS.iter().find(|p| p.name == name) else {
        let known: Vec<&str> = PROMPTS.iter().map(|p| p.name).collect();
        return rpc_error(
            INVALID_PARAMS,
            format!("Unknown prompt: {name} (available: {})", known.join(", ")),
        );
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    for (key, _, required) in spec.arguments {
        // MCP prompt arguments are strings; anything else is a malformed
        // call, never silently replaced by the default.
        if args
            .get(*key)
            .is_some_and(|v| !v.is_string() && !v.is_null())
        {
            return rpc_error(
                INVALID_PARAMS,
                format!("prompt argument '{key}' must be a string"),
            );
        }
        if *required && arg(key).is_none() {
            return rpc_error(
                INVALID_PARAMS,
                format!("prompt '{name}' needs the argument '{key}'"),
            );
        }
    }
    let text = match name {
        "ingest" => ingest_prompt(arg("source").unwrap_or_default(), arg("connector")),
        "lint-session" => lint_session_prompt(arg("focus")),
        _ => {
            let max = match arg("max_changes") {
                None => 10,
                Some(raw) => match raw.parse::<u32>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        return rpc_error(
                            INVALID_PARAMS,
                            "'max_changes' must be a positive whole number",
                        );
                    }
                },
            };
            dream_prompt(max)
        }
    };
    Reply::Result(json!({
        "description": spec.description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }]
    }))
}

fn ingest_prompt(source: &str, connector: Option<&str>) -> String {
    let connector = connector
        .map(|c| format!("connector \"{c}\""))
        .unwrap_or_else(|| "a fitting connector such as email, notes or confluence".to_string());
    format!(
        "Ingest the following into the BRAIN wiki: {source}

Follow the vault conventions (resource brain://agents-md). Steps:
1. Raw file: if the material is not under 01_raw/ yet, store it verbatim with brain_write_raw_file ({connector}; relative_path: a date-prefixed file name).
2. Find what exists: brain_search for the main names and topics, then brain_lookup for every entity or concept page you plan to create (by name or planned id). An existing page or a match means: extend that page and add your spelling to its aliases — never create a duplicate.
3. Plan the pages: one `sources/<yyyy-mm-dd>-<slug>` page for the artifact (what it is, key facts, the raw file path), entity pages for the people, organisations and products it names, concept pages for methods and terms, and a topic page only for a synthesis across several sources.
4. Write all new and changed pages in ONE brain_write_batch call. Every page: frontmatter id, type (singular: entity, concept, source or topic), title, summary (one or two sentences); entity and concept pages also `sources: [sources/<the source page>]`. Link with [[type-dir/slug]] only to pages that exist or are in the same batch. Before changing an existing page, read it with brain_get_pages (response_format \"detailed\") and carry over EVERY frontmatter field unchanged (aliases, sources, tags, superseded_by, valid_from/valid_to, distinct_from, keep) — a concise read has no frontmatter, and a dropped field is lost.
5. Check the response: fix lint errors it reports. If new_size_bytes is much smaller than previous_size_bytes on an existing page, stop and tell the user.
6. Changed facts are never overwritten: supersede the old page (superseded_by + valid_to) and write the new state.
Finish with a short report for the user: pages created, pages updated, open questions."
    )
}

fn lint_session_prompt(focus: Option<&str>) -> String {
    let scope = match focus {
        Some(kind) => format!("Work only on findings of kind `{kind}`."),
        None => {
            "Errors first (they block auto-commits), then warnings, one kind at a time.".to_string()
        }
    };
    format!(
        "Run a cleanup session on the BRAIN wiki.
1. Read the newest audit (resource brain://audit/latest) or call brain_lint_report for the live state (concise: counts per kind). {scope}
2. Rewrite rule: before any brain_write_page on an existing page, read it with brain_get_pages (response_format \"detailed\") and carry over EVERY frontmatter field unchanged except the one you fix (aliases, sources, tags, superseded_by, valid_from/valid_to, distinct_from, keep) — a concise read has no frontmatter, and a dropped field is lost.
3. For each kind, get its findings with brain_lint_report (response_format \"detailed\", kind \"<kind>\") and fix them:
- broken-link / broken-source: correct the id, create the missing page, or rename the page that was meant (brain_refactor action \"rename\").
- unregistered-type / frontmatter / missing-title: read the page with brain_get_pages (response_format \"detailed\"), then rewrite it with brain_write_page keeping every other frontmatter field (type is singular: entity, concept, source, topic).
- dangling-supersede / supersede-cycle: point superseded_by at an existing, current page.
- duplicate-candidate / alias-collision: read both pages (brain_get_pages); the same thing → brain_refactor action \"merge\" the weaker into the stronger; different things → add distinct_from (or fix the clashing alias).
- orphan: link it from a related page or merge it; delete it (brain_refactor action \"delete\") only if it is junk.
- missing-summary / missing-sources: read the page with brain_get_pages (response_format \"detailed\"), then add a one-to-two-sentence summary / the source pages with brain_write_page, body and every other frontmatter field unchanged.
- missing-sources on a page created from a mail or calendar ingestion: the master-index topic page of that ingestion wave (e.g. topics/<…>-mail-ingestion) is an acceptable `sources` entry. Find it via the page's \"Verwandt im Brain\" link, or with brain_query (`prefix: \"topics/\"`, or query `type:topic AND title:ingestion`), then add it to `sources` with a detailed read + rewrite as above.
- expired-but-linked: point the links at the successor.
- invalid-date: write YYYY-MM-DD; valid_from must not lie after valid_to.
- non-canonical-wiki-link / wikilink-pipe-in-table-cell: rewrite as [[type-dir/slug]] (no |alias inside table cells).
4. Ask the user before merging or deleting pages they wrote themselves.
5. Call brain_lint_report again at the end and report what you fixed and what is left."
    )
}

fn dream_prompt(max_changes: u32) -> String {
    format!(
        "Dream: consolidate the BRAIN wiki (the user asked for it).

Hard rules:
- At most {max_changes} changes in this session.
- Never delete a page that other pages link to.
- Supersede instead of overwriting facts (superseded_by + valid_to on the old page; keep its body).
- Keep minority views and open questions; do not flatten them into one \"truth\".
- Ask before changing pages the user clearly wrote themselves.
- Rewrite rule: before any brain_write_page on an existing page, read it with brain_get_pages (response_format \"detailed\") and carry over EVERY frontmatter field unchanged except the one you change (aliases, sources, tags, superseded_by, valid_from/valid_to, distinct_from, keep) — a concise read has no frontmatter, and a dropped superseded_by makes a replaced page current again.

Steps:
1. brain_dream with action \"queue\" (refresh: true if the wiki changed a lot since the last queue).
2. Work top-down (priority 1 first). By suggested_action:
- fix-link: repair the broken link or sources entry (right id, create the missing page, or brain_refactor action \"rename\" on the page that was meant).
- merge: read both pages (brain_get_pages); the same thing → brain_refactor action \"merge\" the weaker into the stronger, then tidy the appended section with brain_patch_page; different things → add distinct_from (rewrite rule above).
- update-summary / write-summary: read the page with brain_get_pages (response_format \"detailed\") and write a fitting one-to-two-sentence summary with brain_write_page (body and every other frontmatter field unchanged). If the existing summary is still right, confirm it instead: brain_write_page with the page exactly as read (response_format \"detailed\") and confirm_summary: true.
- archive-or-supersede / review-or-archive: link it from a related page if it is still useful; if its facts were replaced, set superseded_by and valid_to (rewrite rule above). Do not delete it.
3. Stop after {max_changes} changes or when the queue is done; what is left shows up in the next queue.
4. End with ONE brain_dream action \"log\" call: `entry` = one line saying what you changed and why (e.g. \"merged entities/acme-inc into entities/acme; summaries for 3 hubs\"), and `items` = every queue item you looked at, each {{kind, pages, outcome: \"done\" | \"skipped\" | \"deferred\", note}} — a skipped item needs a one-line reason in its note. Items that were skipped 3 times before say so in their reason: decide them now. Only for orphan and decay-candidate items is there a third answer: if the user says the page stays, mark it `keep: true` (rewrite rule above). Every change stays restorable with brain_history action \"restore\"."
    )
}

// ---- Resources -------------------------------------------------------------

const RESOURCE_AGENTS_MD: &str = "brain://agents-md";
const RESOURCE_AUDIT_LATEST: &str = "brain://audit/latest";
const RESOURCE_DREAM_QUEUE: &str = "brain://dream-queue";

/// (uri, name, title, description, mimeType)
const RESOURCES: &[(&str, &str, &str, &str, &str)] = &[
    (
        RESOURCE_AGENTS_MD,
        "agents-md",
        "AGENTS.md — vault conventions",
        "The vault's 00_meta/AGENTS.md: page types, frontmatter, links, tool map, ingest/cleanup/dream workflows. Read it before writing pages.",
        "text/markdown",
    ),
    (
        RESOURCE_AUDIT_LATEST,
        "audit-latest",
        "Newest wiki audit",
        "The newest daily audit report (00_meta/audit/<date>.md): all lint and hygiene findings of the whole wiki.",
        "text/markdown",
    ),
    (
        RESOURCE_DREAM_QUEUE,
        "dream-queue",
        "Dream queue",
        "BRAIN's prioritised consolidation work list as last stored (fresh for an hour); when none is stored, call the tool brain_dream with action 'queue'.",
        "application/json",
    ),
];

fn resource_descriptors(features: Features) -> Vec<Value> {
    RESOURCES
        .iter()
        .map(|(uri, name, title, description, mime)| {
            let mut resource = json!({
                "uri": uri,
                "name": name,
                "description": description,
                "mimeType": mime,
            });
            if features.structured {
                resource["title"] = json!(title);
            }
            resource
        })
        .collect()
}

/// Prepended to the bundled AGENTS.md when the vault's copy is stale.
const STALE_AGENTS_NOTICE: &str = "> **Notice from BRAIN:** this vault's `00_meta/AGENTS.md` \
predates BRAIN 0.3.5 and names MCP tools that were renamed. Ask the user to run \
\"Update vault templates\" in Settings → Danger. The current conventions follow.\n\n";

/// Whether an AGENTS.md text names a removed tool outside its "Renamed
/// tools" paragraph (which lists the old names on purpose).
fn names_removed_tools(text: &str) -> bool {
    let without_renamed = match text.find("**Renamed tools.**") {
        Some(start) => {
            let end = text[start..].find("\n\n").map_or(text.len(), |i| start + i);
            format!("{}{}", &text[..start], &text[end..])
        }
        None => text.to_string(),
    };
    let token = regex::Regex::new(r"brain_[a-z_]+").expect("valid tool-name pattern");
    token
        .find_iter(&without_renamed)
        .any(|m| tools::removed(m.as_str()).is_some())
}

/// `resources/read`. AGENTS.md falls back to the bundled template when no
/// vault (or no file) is there, or when the vault's copy is stale, so the
/// current conventions are always readable; the audit and the dream queue
/// need the vault and are served only from stored files.
fn resource_read(params: &Value, vault: Option<&std::path::Path>) -> Reply {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return rpc_error(INVALID_PARAMS, "missing resource 'uri'");
    };
    let Some((_, _, _, _, mime)) = RESOURCES.iter().find(|r| r.0 == uri) else {
        return Reply::Error {
            code: RESOURCE_NOT_FOUND,
            message: "Resource not found".to_string(),
            data: Some(json!({ "uri": uri })),
        };
    };
    let reachable = vault.filter(|v| crate::vault::layout::is_vault(v));
    let text = match uri {
        RESOURCE_AGENTS_MD => {
            let bundled = crate::onboarding::template::AGENTS_MD;
            match reachable.and_then(|v| {
                std::fs::read_to_string(
                    crate::vault::layout::meta_dir(v).join(crate::vault::layout::AGENTS_FILENAME),
                )
                .ok()
            }) {
                // A copy written by an older BRAIN names tools that no
                // longer exist: serve the current conventions instead,
                // with a notice how to refresh the vault's file.
                Some(text) if names_removed_tools(&text) => {
                    format!("{STALE_AGENTS_NOTICE}{bundled}")
                }
                Some(text) => text,
                None => bundled.to_string(),
            }
        }
        _ => {
            let Some(v) = reachable else {
                return match vault {
                    None => rpc_error(NO_VAULT, no_vault_message()),
                    Some(path) => rpc_error(NO_VAULT, vault_disconnected_message(path)),
                };
            };
            if uri == RESOURCE_AUDIT_LATEST {
                match latest_audit(v) {
                    Some(text) => text,
                    None => {
                        return Reply::Error {
                            code: RESOURCE_NOT_FOUND,
                            message: "no audit report yet — BRAIN writes one shortly after mount \
                                      and then daily; call brain_lint_report for the live state"
                                .to_string(),
                            data: Some(json!({ "uri": uri })),
                        };
                    }
                }
            } else {
                // Never compute here: a resource read must not build the
                // index or write files on the stdio thread. Only a stored,
                // fresh queue is served; brain_dream computes one.
                match crate::wiki::dream::cached_queue(v, chrono::Utc::now()) {
                    Some(queue) => serde_json::to_string(&queue).unwrap_or_default(),
                    None => {
                        return Reply::Error {
                            code: RESOURCE_NOT_FOUND,
                            message: "no fresh dream queue stored — call the tool brain_dream \
                                      with action \"queue\" to compute one"
                                .to_string(),
                            data: Some(json!({ "uri": uri })),
                        };
                    }
                }
            }
        }
    };
    Reply::Result(json!({
        "contents": [{ "uri": uri, "mimeType": mime, "text": text }]
    }))
}

/// Text of the newest `00_meta/audit/<YYYY-MM-DD>.md` (file names sort by
/// date), `None` when there is none.
fn latest_audit(vault: &std::path::Path) -> Option<String> {
    let newest = std::fs::read_dir(crate::wiki::audit::audit_dir(vault))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .max()?;
    std::fs::read_to_string(newest).ok()
}

fn vault_disconnected_message(vault: &std::path::Path) -> String {
    format!(
        "BRAIN_VAULT_DISCONNECTED: the BRAIN vault at '{}' is not currently accessible. \
         The disk holding the vault was unplugged or the path is no longer valid. \
         Tell the user to reconnect the BRAIN drive and try again. \
         Do not attempt to recreate or guess at the missing data.",
        vault.display()
    )
}

/// The dream queue: the stored one when fresh (and `refresh` is false),
/// else recomputed from the index and stored.
fn dream_queue(
    vault: &std::path::Path,
    db: &mut Option<crate::db::DbHandle>,
    refresh: bool,
) -> Result<crate::wiki::dream::DreamQueue, String> {
    use crate::wiki::dream;
    let now = chrono::Utc::now();
    if !refresh {
        if let Some(queue) = dream::cached_queue(vault, now) {
            return Ok(queue);
        }
    }
    let rows = db_op(db, vault, "brain_dream", dream::load_dream_rows)?;
    let queue = dream::build_queue_with_history(&rows, now, &dream::skip_counts(vault));
    if let Err(err) = dream::write_dream_queue(vault, &queue) {
        tracing::warn!(?err, "could not write the dream queue");
    }
    Ok(queue)
}

// ---- Tool arguments ----------------------------------------------------------

/// `response_format` of the read tools (Slice D).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Concise,
    Detailed,
}

fn format_arg(args: &Value) -> Result<Format, String> {
    match args.get("response_format") {
        None | Some(Value::Null) => Ok(Format::Concise),
        Some(Value::String(s)) if s == "concise" => Ok(Format::Concise),
        Some(Value::String(s)) if s == "detailed" => Ok(Format::Detailed),
        Some(_) => Err("'response_format' must be \"concise\" or \"detailed\"".to_string()),
    }
}

/// Optional non-negative integer argument.
fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|n| Some(n as usize))
            .ok_or_else(|| format!("'{key}' must be a non-negative integer")),
    }
}

/// Optional string argument.
fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("'{key}' must be a string")),
    }
}

// ---- Tool dispatch -----------------------------------------------------------

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

    // NOTE: `brain_ping` is handled upstream in `tools_call`, before
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
        return Err(vault_disconnected_message(vault));
    }

    match name {
        "brain_search" => {
            let format = format_arg(&args)?;
            let limit = optional_usize(&args, "limit")?
                .unwrap_or(SEARCH_DEFAULT_LIMIT)
                .clamp(1, SEARCH_MAX_LIMIT);
            let q = args.get("query").and_then(Value::as_str).unwrap_or("");
            if q.trim().is_empty() {
                return Ok(json!({ "hits": [] }).to_string());
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
                search::search_hybrid_on_conn(conn, embedder.as_ref(), &query_owned, limit)
                    .map_err(crate::db::DbError::from)
            });
            let mut hits = match hybrid {
                Ok(hits) if !hits.is_empty() => hits,
                // Empty hybrid result or any DB error → brute-force walk.
                _ => search::search_brute_force(vault, q).map_err(|e| e.to_string())?,
            };
            hits.truncate(limit);
            record_search_hits(
                db,
                vault,
                hits.iter()
                    .take(SALIENCE_SEARCH_TOP)
                    .map(|h| h.id.clone())
                    .collect(),
            );
            let hits: Vec<Value> = match format {
                Format::Detailed => hits
                    .iter()
                    .map(|h| serde_json::to_value(h).unwrap_or_default())
                    .collect(),
                Format::Concise => {
                    let summaries =
                        summaries_of(db, vault, hits.iter().map(|h| h.id.clone()).collect());
                    let mut concise: Vec<Value> = hits
                        .iter()
                        .map(|h| {
                            let source = summaries
                                .get(&h.id)
                                .map(String::as_str)
                                .unwrap_or(h.snippet.as_str());
                            json!({
                                "id": h.id,
                                "title": plain_snippet(&h.title, CONCISE_TITLE_CHARS),
                                "score": (f64::from(h.score) * 10_000.0).round() / 10_000.0,
                                "snippet": plain_snippet(source, CONCISE_SNIPPET_CHARS),
                            })
                        })
                        .collect();
                    // Budget: the snippet matters more to the agent than the
                    // score, so the score goes first when the hits get long.
                    if json!({ "hits": concise }).to_string().len() > CONCISE_SEARCH_BUDGET {
                        for hit in &mut concise {
                            if let Value::Object(fields) = hit {
                                fields.remove("score");
                            }
                        }
                    }
                    concise
                }
            };
            Ok(json!({ "hits": hits }).to_string())
        }
        "brain_get_pages" => {
            let format = format_arg(&args)?;
            let include_context = match args.get("include_context") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("'include_context' must be a boolean".to_string()),
            };
            let ids = args
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing 'ids' array (one page: [\"<id>\"])".to_string())?;
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
                            // `tree::read_page` already strips the YAML
                            // frontmatter, so the links come straight from
                            // the body (re-parsing it would fail with
                            // "missing frontmatter delimiter").
                            let outbound =
                                include_context.then(|| page::extract_wiki_links(&page.body));
                            let mut entry = json!({
                                "id": id,
                                "found": true,
                                "page": page_payload(page, format).0,
                            });
                            if let Some(outbound) = outbound {
                                entry["outbound"] = json!(outbound);
                                match search::backlinks(vault, id) {
                                    Ok(backlinks) => {
                                        entry["backlinks"] = match format {
                                            Format::Detailed => {
                                                serde_json::to_value(&backlinks).unwrap_or_default()
                                            }
                                            Format::Concise => json!(
                                                backlinks.iter().map(|b| &b.id).collect::<Vec<_>>()
                                            ),
                                        };
                                    }
                                    Err(e) => entry["context_error"] = json!(e.to_string()),
                                }
                            }
                            entry
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
        "brain_lookup" => lookup(&args, vault, db),
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
            let parsed = page::parse(content)
                .map_err(|e| format!("invalid page content: {}", parse_error_text(content, &e)))?;
            let allow_duplicate = allow_duplicate_arg(&args)?;
            let confirm_summary = confirm_summary_arg(&args)?;
            // A2: refuse to CREATE a page that probably exists already
            // under another id. Overwriting an existing id is never blocked.
            let target =
                crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
            check_target_owner(&target, id)?;
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
                let detail =
                    serde_json::to_string(&page_errors).unwrap_or_else(|_| "[]".to_string());
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
            check_heading(heading)?;
            let section = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'content'".to_string())?;
            // The page must already exist — patch edits one section of it.
            let target =
                crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
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
            response["summary_confirmed"] =
                json!(confirm_summary_in_index(db, vault, id, &new_content));
            Ok(serde_json::to_string(&response).unwrap_or_default())
        }
        "brain_history" => match tools::resolve_action(name, args.get("action"))? {
            Some("restore") => history_restore(&args, vault),
            _ => history_list(&args, vault),
        },
        "brain_refactor" => {
            let action = tools::resolve_action(name, args.get("action"))?.unwrap_or_default();
            let outcome = match action {
                "rename" => {
                    let id = required_str(&args, "id")?;
                    let new_id = required_str(&args, "new_id")?;
                    let outcome =
                        refactor::rename_page(vault, id, new_id).map_err(|e| e.to_string())?;
                    forget_in_index(db, vault, &outcome.old_id, Some(&outcome.new_id));
                    serde_json::to_value(&outcome)
                }
                "merge" => {
                    let from_id = required_str(&args, "from_id")?;
                    let into_id = required_str(&args, "into_id")?;
                    let outcome = refactor::merge_pages(vault, from_id, into_id)
                        .map_err(|e| e.to_string())?;
                    forget_in_index(db, vault, &outcome.from_id, Some(&outcome.into_id));
                    serde_json::to_value(&outcome)
                }
                _ => {
                    let id = required_str(&args, "id")?;
                    let force = match args.get("force") {
                        None | Some(Value::Null) => false,
                        Some(Value::Bool(b)) => *b,
                        Some(_) => {
                            return Err("force must be a boolean (true or false)".to_string());
                        }
                    };
                    let outcome =
                        refactor::delete_page(vault, id, force).map_err(|e| e.to_string())?;
                    forget_in_index(db, vault, &outcome.deleted, None);
                    serde_json::to_value(&outcome)
                }
            };
            Ok(action_payload(action, outcome.unwrap_or_default()).to_string())
        }
        "brain_write_batch" => write_batch(&args, vault, db),
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
            Ok(json!({ "wrote": format!("01_raw/{connector}/{rel}") }).to_string())
        }
        "brain_graph" => {
            let format = format_arg(&args)?;
            let types = args.get("types").and_then(Value::as_array).map(|arr| {
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
            let payload = match format {
                Format::Detailed => json!({
                    "node_count": g.nodes.len(),
                    "edge_count": g.edges.len(),
                    "nodes": g.nodes,
                    "edges": g.edges,
                }),
                Format::Concise => json!({
                    "node_count": g.nodes.len(),
                    "edge_count": g.edges.len(),
                    "nodes": g.nodes.iter().map(|n| &n.id).collect::<Vec<_>>(),
                    "edges": g.edges.iter().map(|e| [&e.source, &e.target]).collect::<Vec<_>>(),
                }),
            };
            Ok(payload.to_string())
        }
        "brain_query" => query(&args, vault, db),
        "brain_lint_report" => {
            let format = format_arg(&args)?;
            let kind = optional_str(&args, "kind")?;
            // Read-only view of the same lint pass that drives the
            // auto-commit watcher and the Tauri toast bridge, plus the
            // hygiene warnings (orphan, duplicate-candidate) and info
            // `notes`; the watcher's pre-commit gate keeps the fast
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
            if let Some(kind) = kind {
                report.errors.retain(|e| e.kind == kind);
                report.warnings.retain(|w| w.kind == kind);
            }
            Ok(lint_payload(&report, format).to_string())
        }
        "brain_eval" => match tools::resolve_action(name, args.get("action"))? {
            Some("add") => eval_add(&args, vault),
            _ => eval_run(vault, db),
        },
        "brain_dream" => match tools::resolve_action(name, args.get("action"))? {
            Some("queue") => {
                let refresh = match args.get("refresh") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Err("'refresh' must be a boolean".to_string()),
                };
                let queue = dream_queue(vault, db, refresh)?;
                Ok(serde_json::to_string_pretty(&queue).unwrap_or_default())
            }
            Some("stats") => {
                let stats = crate::wiki::dream::dream_stats(vault);
                Ok(serde_json::to_string_pretty(&stats).unwrap_or_default())
            }
            _ => {
                let entry = required_str(&args, "entry")?;
                let items = dream_log_items(&args)?;
                let line = crate::wiki::dream::append_dream_log(
                    vault,
                    entry,
                    &items,
                    chrono::Local::now(),
                )
                .map_err(|e| e.to_string())?;
                // The stored queue's skip counts are now out of date: drop it so
                // the next `queue` call recomputes.
                if !items.is_empty() {
                    drop_stored_queue(vault);
                }
                Ok(json!({ "logged": line, "items_logged": items.len() }).to_string())
            }
        },
        other => Err(format!("unknown tool: {other}")),
    }
}

/// The optional `items` of `brain_dream` action `log`, validated: each
/// `{kind, pages, outcome, note?}` with a one-word `kind`, at least one
/// valid page id and `outcome` one of done / skipped / deferred.
fn dream_log_items(args: &Value) -> Result<Vec<crate::wiki::dream::LogItem>, String> {
    use crate::wiki::dream::{LogItem, LogOutcome};
    let raw = match args.get("items") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(raw)) => raw,
        Some(_) => return Err("'items' must be an array of {kind, pages, outcome, note?}".into()),
    };
    raw.iter()
        .enumerate()
        .map(|(i, item)| {
            let at = |msg: &str| format!("items[{i}]: {msg}");
            let kind = item
                .get("kind")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|k| !k.is_empty() && !k.contains(char::is_whitespace))
                .ok_or_else(|| at("'kind' must be the queue item's kind, e.g. \"orphan\""))?;
            let pages: Vec<String> = item
                .get("pages")
                .and_then(Value::as_array)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| at("'pages' must be a non-empty array of page ids"))?
                .iter()
                .map(|p| {
                    let id = p
                        .as_str()
                        .ok_or_else(|| at("'pages' must contain page id strings"))?;
                    check_page_id(id).map_err(|e| at(&e))?;
                    if id.contains('`') {
                        return Err(at("page ids must not contain a backtick"));
                    }
                    Ok(id.to_string())
                })
                .collect::<Result<_, String>>()?;
            let outcome = item
                .get("outcome")
                .and_then(Value::as_str)
                .and_then(LogOutcome::parse)
                .ok_or_else(|| at("'outcome' must be \"done\", \"skipped\" or \"deferred\""))?;
            let note = match item.get("note") {
                None | Some(Value::Null) => None,
                Some(Value::String(n)) => Some(n.clone()),
                Some(_) => return Err(at("'note' must be a string")),
            };
            Ok(LogItem {
                kind: kind.to_string(),
                pages,
                outcome,
                note,
            })
        })
        .collect()
}

/// Delete `00_meta/dream-queue.md` so the next `brain_dream` queue call
/// recomputes it. Best effort: a failure is logged, never surfaced.
fn drop_stored_queue(vault: &std::path::Path) {
    match std::fs::remove_file(crate::wiki::dream::dream_queue_path(vault)) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(?err, "could not drop the stored dream queue"),
    }
}

/// A multi-action tool's result: `action` first, then the outcome's
/// fields with every `null` left out — an absent optional field is
/// omitted, never `null`, so it cannot clash with its `outputSchema` type.
fn action_payload(action: &str, outcome: Value) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert("action".into(), json!(action));
    if let Value::Object(fields) = outcome {
        payload.extend(fields.into_iter().filter(|(_, v)| !v.is_null()));
    }
    Value::Object(payload)
}

/// `brain_history` action `list`: the commits that touched one page.
fn history_list(args: &Value, vault: &std::path::Path) -> Result<String, String> {
    let id = history_page_id(args)?;
    let limit = optional_usize(args, "limit")?.unwrap_or(20).max(1);
    // Accept both `entities/alice` and `entities/alice.md`; route through
    // the resolver so the repo-relative path matches how pages are stored
    // on disk (opaque on an encrypted vault).
    let page_path = crate::wiki::encryption::page_relpath(vault, id).map_err(|e| e.to_string())?;
    let history = wiki_history::history_for_page(&wiki_dir(vault), &page_path, limit)
        .map_err(|e| e.to_string())?;
    Ok(json!({ "action": "list", "commits": history }).to_string())
}

/// `brain_history` action `restore`: the page as it was at `sha`, recorded
/// as a `revert:` commit (append-only history). The watcher's debounce
/// window may not have produced that commit yet, so the source sha is
/// reported for the audit trail.
fn history_restore(args: &Value, vault: &std::path::Path) -> Result<String, String> {
    let id = history_page_id(args)?;
    let sha = required_str(args, "sha")?;
    if sha.is_empty() {
        return Err("'sha' must not be empty".to_string());
    }
    let page_path = crate::wiki::encryption::page_relpath(vault, id).map_err(|e| e.to_string())?;
    wiki_history::restore_page(&wiki_dir(vault), sha, &page_path).map_err(|e| e.to_string())?;
    Ok(json!({ "action": "restore", "restored": id, "from_sha": sha }).to_string())
}

/// The page id of a `brain_history` call, without an optional `.md`,
/// checked by the shared page-id guard.
fn history_page_id(args: &Value) -> Result<&str, String> {
    let id = required_str(args, "id")?;
    if id.is_empty() {
        return Err("'id' must not be empty".to_string());
    }
    let id = id.strip_suffix(".md").unwrap_or(id);
    check_page_id(id)?;
    Ok(id)
}

/// Longest snippet of a concise `brain_search` hit, in characters.
const CONCISE_SNIPPET_CHARS: usize = 80;
/// Longest title of a concise `brain_search` hit, in characters.
const CONCISE_TITLE_CHARS: usize = 60;
/// Character budget of a concise `brain_search` answer: ten realistic
/// hits (35-character id, 30-character title, 80-character snippet, score)
/// take about 1,950 characters; over the budget the scores are dropped
/// first. (The roadmap's 1,500 cannot hold ten 80-character snippets
/// plus ids and titles.)
const CONCISE_SEARCH_BUDGET: usize = 2000;

/// Plain text for a concise hit: no FTS5 `«»` highlight markers, runs of
/// whitespace collapsed, cut at a word boundary so the result (with a
/// trailing `…`) has at most `max` characters.
fn plain_snippet(text: &str, max: usize) -> String {
    let plain: String = text
        .replace(['«', '»'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if plain.chars().count() <= max {
        return plain;
    }
    let head: String = plain.chars().take(max - 1).collect();
    let cut = match head.rfind(' ') {
        Some(i) if i > 0 => &head[..i],
        _ => head.as_str(),
    };
    format!(
        "{}…",
        cut.trim_end_matches([' ', ',', ';', ':', '.', '-', '…'])
    )
}

/// The indexed `summary` of each id that has one. Best effort, through
/// the non-building side-job path: empty when the index is unavailable.
fn summaries_of(
    db: &Option<crate::db::DbHandle>,
    vault: &std::path::Path,
    ids: Vec<String>,
) -> std::collections::HashMap<String, String> {
    if ids.is_empty() {
        return Default::default();
    }
    db_op_if_indexed(db, vault, COUNTER_DB_TIMEOUT, move |conn| {
        let mut stmt = conn.prepare("SELECT summary FROM pages WHERE id = ?1")?;
        let mut out = std::collections::HashMap::new();
        for id in ids {
            let summary: Option<String> = stmt.query_row([&id], |r| r.get(0)).ok().flatten();
            if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
                out.insert(id, summary);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

/// `brain_search` default and maximum hit counts (the hybrid search
/// itself returns at most 20).
const SEARCH_DEFAULT_LIMIT: usize = 10;
const SEARCH_MAX_LIMIT: usize = 20;

/// `brain_query` default page size.
const QUERY_DEFAULT_LIMIT: usize = 100;

/// Most hits one `brain_query` page returns; larger `limit`s are clamped
/// (page with `offset` / `next_offset` instead).
const QUERY_MAX_LIMIT: usize = 500;

/// `brain_query`'s `limit`: default [`QUERY_DEFAULT_LIMIT`], at least 1,
/// at most [`QUERY_MAX_LIMIT`].
fn query_limit(args: &Value) -> Result<usize, String> {
    Ok(optional_usize(args, "limit")?
        .unwrap_or(QUERY_DEFAULT_LIMIT)
        .clamp(1, QUERY_MAX_LIMIT))
}

/// `brain_lookup`: existence + probable duplicates, never page bodies.
/// An argument with `/` is a page id (checks that id and its type); a bare
/// name is checked against all four types, with `exact` entries for ids
/// that exist under that very slug.
fn lookup(
    args: &Value,
    vault: &std::path::Path,
    db: &mut Option<crate::db::DbHandle>,
) -> Result<String, String> {
    let query = required_str(args, "query_or_id")?.trim();
    if query.is_empty() {
        return Err("'query_or_id' must not be empty".to_string());
    }
    let no_pending = std::collections::HashSet::new();
    if query.contains('/') {
        // Defend against path escapes smuggled into the id
        // (`../../etc/passwd`, `C:/Users/x`) before any path is built.
        check_page_id(query)?;
        let exists = page_file_exists(vault, query);
        // A2: other pages that are probably the same thing, from the
        // index, best effort and never building it: without a usable
        // index there are no matches and `matches_checked` is false.
        // Matches whose file is gone (stale index rows) are dropped.
        let entries = load_name_entries(db, vault);
        let matches_checked = entries.is_some();
        let matches = live_matches(
            vault,
            duplicates::find_matches(query, &entries.unwrap_or_default()),
            &no_pending,
        );
        return Ok(json!({
            "query_or_id": query,
            "id": query,
            "exists": exists,
            "matches": matches,
            "matches_checked": matches_checked,
        })
        .to_string());
    }
    let entries = load_name_entries(db, vault);
    let matches_checked = entries.is_some();
    let entries = entries.unwrap_or_default();
    let mut exact: Vec<Value> = Vec::new();
    let mut matches: Vec<Value> = Vec::new();
    for dir in crate::vault::layout::WIKI_SUBDIRS {
        let candidate = format!("{dir}/{query}");
        if check_page_id(&candidate).is_ok() && page_file_exists(vault, &candidate) {
            let title = entries
                .iter()
                .find(|e| e.id == candidate)
                .and_then(|e| e.title.clone());
            exact.push(json!({ "id": candidate, "title": title, "reason": "exact" }));
        }
        let found = live_matches(
            vault,
            duplicates::find_matches(&candidate, &entries),
            &no_pending,
        );
        matches.extend(
            found
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or_default()),
        );
    }
    let exists = !exact.is_empty();
    exact.extend(matches);
    Ok(json!({
        "query_or_id": query,
        "exists": exists,
        "matches": exact,
        "matches_checked": matches_checked,
    })
    .to_string())
}

/// `brain_query` (absorbs the former page-listing and tag-listing
/// tools): hits for a filter expression, or tag counts with
/// `facet: "tags"`. Empty / `*` lists every current page; then, and only
/// then, an unavailable index falls back to a file-system listing.
fn query(
    args: &Value,
    vault: &std::path::Path,
    db: &mut Option<crate::db::DbHandle>,
) -> Result<String, String> {
    let format = format_arg(args)?;
    let raw = optional_str(args, "query")?.unwrap_or("").trim();
    if raw.starts_with("facet:") {
        return Err(
            "facets are an argument, not query syntax: pass {\"facet\": \"tags\"}".to_string(),
        );
    }
    let prefix = optional_str(args, "prefix")?.unwrap_or("");
    let limit = query_limit(args)?;
    let offset = optional_usize(args, "offset")?.unwrap_or(0);
    let facet = optional_str(args, "facet")?;
    if let Some(other) = facet.filter(|f| *f != "tags") {
        return Err(format!("unknown facet '{other}' (one of: tags)"));
    }
    let list_all = raw.is_empty() || raw == "*";

    if facet.is_some() && list_all && prefix.is_empty() {
        // Every tag of every indexed page (the former tag listing).
        let rows = db_op(db, vault, "brain_query", |conn| {
            let mut stmt = conn.prepare(
                "SELECT tag, COUNT(*) AS count FROM page_tags \
                 GROUP BY tag ORDER BY count DESC, tag ASC",
            )?;
            let mapped: Result<Vec<(String, i64)>, _> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect();
            Ok(mapped?)
        })?;
        return Ok(tags_payload(rows).to_string());
    }

    // "*" is not query syntax: the listing is every current page.
    let expression = if list_all {
        "valid:now".to_string()
    } else {
        raw.to_string()
    };
    let mut note: Option<&str> = None;
    let mut hits = match db_op(db, vault, "brain_query", move |conn| {
        crate::viewer::query::executor::run_on_conn(conn, &expression).map_err(|e| match e {
            crate::viewer::query::executor::ExecError::Db(r) => crate::db::DbError::from(r),
            // Parse errors are not DB errors — surface them as an
            // Io-wrapped string so db_op returns them verbatim
            // (and never reopen-loops on a bad query).
            other => crate::db::DbError::Io(std::io::Error::other(other.to_string())),
        })
    }) {
        Ok(hits) => hits,
        Err(_) if list_all && facet.is_none() => {
            note =
                Some("the index is unavailable — listed from the file system (ids and types only)");
            filesystem_hits(vault).map_err(|e| e.to_string())?
        }
        Err(err) => return Err(err),
    };
    if !prefix.is_empty() {
        hits.retain(|h| h.id.starts_with(prefix));
    }

    if facet.is_some() {
        let ids: std::collections::HashSet<String> = hits.into_iter().map(|h| h.id).collect();
        let rows = db_op(db, vault, "brain_query", move |conn| {
            let mut stmt = conn.prepare("SELECT page_id, tag FROM page_tags")?;
            let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
            for row in
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (page_id, tag) = row?;
                if ids.contains(&page_id) {
                    *counts.entry(tag).or_default() += 1;
                }
            }
            let mut rows: Vec<(String, i64)> = counts.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(rows)
        })?;
        return Ok(tags_payload(rows).to_string());
    }

    let total = hits.len();
    let page: Vec<Value> = hits
        .iter()
        .skip(offset)
        .take(limit)
        .map(|h| match format {
            Format::Detailed => serde_json::to_value(h).unwrap_or_default(),
            Format::Concise => json!({ "id": h.id, "type": h.r#type, "title": h.title }),
        })
        .collect();
    let mut payload = json!({
        "total": total,
        "offset": offset,
        "returned": page.len(),
        "hits": page,
    });
    let next = offset + payload["returned"].as_u64().unwrap_or(0) as usize;
    if next < total {
        payload["next_offset"] = json!(next);
    }
    if let Some(note) = note {
        payload["note"] = json!(note);
    }
    Ok(payload.to_string())
}

fn tags_payload(rows: Vec<(String, i64)>) -> Value {
    let tags: Vec<Value> = rows
        .into_iter()
        .map(|(tag, count)| json!({ "tag": tag, "count": count }))
        .collect();
    json!({ "facet": "tags", "tags": tags })
}

/// File-system fallback for `brain_query *` while the index is
/// unavailable: ids and types only, sorted by id.
fn filesystem_hits(
    vault: &std::path::Path,
) -> Result<Vec<crate::viewer::query::executor::QueryHit>, ViewerErrAdapter> {
    let mut pairs = list_page_ids_via_filesystem(vault)?;
    pairs.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(pairs
        .into_iter()
        .map(|(bucket, id)| crate::viewer::query::executor::QueryHit {
            r#type: match bucket.as_str() {
                "entities" => "entity",
                "concepts" => "concept",
                "sources" => "source",
                _ => "topic",
            }
            .to_string(),
            path: String::new(),
            title: String::new(),
            updated_at: None,
            reads: 0,
            search_hits: 0,
            last_read_at: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
            tags: Vec::new(),
            summary: None,
            id,
        })
        .collect())
}

/// `brain_lint_report` payload. Concise: every error (they block
/// commits; usually few), warning counts per kind, notes and a hint how
/// to get one kind's warnings. Detailed: the full report.
fn lint_payload(report: &lint::LintReport, format: Format) -> Value {
    let mut payload = json!({
        "error_count": report.errors.len(),
        "warning_count": report.warnings.len(),
        "errors": report.errors,
    });
    match format {
        Format::Detailed => payload["warnings"] = json!(report.warnings),
        Format::Concise => {
            let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
            for w in &report.warnings {
                *kinds.entry(w.kind.as_str()).or_default() += 1;
            }
            payload["warning_kinds"] = json!(kinds);
            if !report.warnings.is_empty() {
                payload["hint"] = json!(
                    "warnings of one kind: brain_lint_report with response_format \"detailed\" and kind \"<kind>\""
                );
            }
        }
    }
    if !report.notes.is_empty() {
        payload["notes"] = json!(report.notes);
    }
    payload
}

/// `brain_eval` action `run`.
fn eval_run(
    vault: &std::path::Path,
    db: &mut Option<crate::db::DbHandle>,
) -> Result<String, String> {
    use crate::viewer::eval;
    let set = eval::load_eval_set(vault).map_err(|e| e.to_string())?;
    if set.is_empty() {
        return Err(format!(
            "the eval set is empty — add test questions with brain_eval (action \"add\") \
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

/// `brain_eval` action `add`.
fn eval_add(args: &Value, vault: &std::path::Path) -> Result<String, String> {
    use crate::viewer::eval;
    let query = required_str(args, "query")?.to_string();
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
    let entry = eval::add_eval_query(
        vault,
        eval::NewEvalQuery {
            id: optional_str(args, "id")?.map(str::to_string),
            query,
            expected,
            note: optional_str(args, "note")?.map(str::to_string),
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&json!({ "added": entry })).unwrap_or_default())
}

/// `brain_write_batch`: three phases — see the tool descriptor for the
/// user-facing rationale. Code-side rationale: parsing all pages up front
/// turns a multi-page write into an all-or-nothing operation against
/// malformed input. Lint runs once at the end with the full batch already
/// on disk, so intra-batch references resolve (the cascade is gone).
fn write_batch(
    args: &Value,
    vault: &std::path::Path,
    db: &mut Option<crate::db::DbHandle>,
) -> Result<String, String> {
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
    let allow_all = allow_duplicate_arg(args)?;
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
        let parsed = page::parse(content).map_err(|e| {
            format!(
                "pages[{idx}] ({id}): invalid content: {}",
                parse_error_text(content, &e)
            )
        })?;
        let allow_duplicate =
            allow_all || allow_duplicate_arg(entry).map_err(|e| format!("pages[{idx}]: {e}"))?;
        let target = crate::wiki::encryption::page_path(vault, id).map_err(|e| e.to_string())?;
        check_target_owner(&target, id).map_err(|e| format!("pages[{idx}] ({id}): {e}"))?;
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
        let normalized_body = page::normalize_internal_links(strip_superseded_notice(&parsed.body));
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

    // Phase 2 — write all files. If an IO error hits mid-batch the error
    // names the failing page; the partial state is consciously left as-is
    // so the user can inspect (we deliberately do not rollback the pages
    // that already wrote, which would itself be an IO sequence that can
    // fail).
    for w in &prepared {
        if let Some(parent) = w.target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir for {}: {e}", w.id))?;
        }
        std::fs::write(&w.target, &w.normalized_content)
            .map_err(|e| format!("write {}: {e}", w.id))?;
    }

    // Phase 3 — single lint pass, scoped to the union of touched paths.
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
        let detail = serde_json::to_string(&scoped_errors).unwrap_or_else(|_| "[]".to_string());
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

/// Filesystem fallback for vaults that haven't been DB-indexed yet.
/// Reuses the existing tree walker (no file content reads) and
/// flattens the four-bucket result into a `(bucket, id)` list so the
/// dispatch stays uniform.
fn list_page_ids_via_filesystem(
    vault: &std::path::Path,
) -> Result<Vec<(String, String)>, ViewerErrAdapter> {
    let t = tree::list_tree(vault).map_err(ViewerErrAdapter)?;
    let mut out =
        Vec::with_capacity(t.entities.len() + t.concepts.len() + t.sources.len() + t.topics.len());
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
#[derive(Debug)]
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
                .find_map(|(j, l)| {
                    heading_level(l)
                        .filter(|lvl| *lvl <= target_level)
                        .map(|_| j)
                })
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

/// `brain_patch_page`'s `heading` must be a markdown heading line
/// (`#`–`######`, a space, then text): anything else would never match a
/// heading of the body and could swallow the rest of the page.
fn check_heading(heading: &str) -> Result<(), String> {
    let pattern = regex::Regex::new(r"^#{1,6} \S").expect("valid heading pattern");
    if pattern.is_match(heading.trim_start()) {
        Ok(())
    } else {
        Err(format!(
            "'heading' must be a markdown heading line such as '## Kontakt' (1–6 '#', a space, \
             then the title); got {heading:?}"
        ))
    }
}

/// When `target` already exists, its frontmatter `id` must be `id`. On a
/// case-insensitive file system `entities/ACME` resolves to the file of
/// `entities/acme`; overwriting it would silently re-id that page. A file
/// whose frontmatter cannot be read is left to the caller (overwriting
/// it with the right id is the repair).
fn check_target_owner(target: &std::path::Path, id: &str) -> Result<(), String> {
    let Ok(existing) = std::fs::read_to_string(target) else {
        return Ok(());
    };
    let Ok(parsed) = page::parse(&existing) else {
        return Ok(());
    };
    let owner = parsed.frontmatter.id;
    // Only a CASE-ONLY difference is a collision: on a case-insensitive
    // file system `entities/ACME` resolves to `acme.md`, and overwriting
    // it would silently re-id the existing page. Any other mismatch (a
    // hand-edited or copied file whose id disagrees with its path) is
    // the repair path and keeps the old overwrite behaviour.
    if owner.is_empty() || owner == id || !owner.eq_ignore_ascii_case(id) {
        return Ok(());
    }
    Err(format!(
        "the file for '{id}' already holds the page '{owner}' (page ids are case-sensitive, the \
         file system may not be) — write to '{owner}' instead, or rename it with brain_refactor \
         (action \"rename\")"
    ))
}

/// A required string argument: `missing '<key>'` when absent,
/// `'<key>' must be a string` when present with another JSON type.
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("missing '{key}'")),
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string")),
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

/// The MCP payload of a page read, plus — when its frontmatter has
/// `superseded_by` (Slice C) — the fields `superseded_by: <id>` and
/// `notice: "Superseded by <id>"`. Detailed: the page view verbatim (id,
/// title, frontmatter as a JSON string, body). Concise: id, title, the
/// frontmatter `summary` (when set) and body. The body is never changed
/// (an agent would write an injected line back). Also returns the
/// successor id.
fn page_payload(page: tree::PageView, format: Format) -> (Value, Option<String>) {
    let frontmatter = serde_json::from_str::<Value>(&page.frontmatter).ok();
    let field = |key: &str| {
        frontmatter
            .as_ref()
            .and_then(|fm| fm.get(key).and_then(Value::as_str).map(str::to_string))
    };
    let successor = field("superseded_by");
    let mut payload = match format {
        Format::Detailed => serde_json::to_value(&page).unwrap_or_else(|_| json!({})),
        Format::Concise => {
            let mut concise = json!({ "id": page.id, "title": page.title });
            if let Some(summary) = field("summary") {
                concise["summary"] = json!(summary);
            }
            concise["body"] = json!(page.body);
            concise
        }
    };
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
/// Guarded arms: get_pages (per id, with or without context), lookup (id
/// form; the name form validates each candidate id), write_page,
/// write_batch (per page), patch_page, history (list and restore) and
/// refactor (rename, merge, delete — inside `wiki::refactor`). Any new arm that resolves a page id must call
/// this first. `brain_write_raw_file` uses [`check_relative_path`].
/// The text of a page-parse error, plus a quoting hint when the YAML
/// failed on a `summary:` / `title:` line whose unquoted value contains
/// `: ` — YAML reads that as a second mapping ("mapping values are not
/// allowed in this context"), which the bare message does not explain.
fn parse_error_text(content: &str, err: &crate::wiki::WikiError) -> String {
    let crate::wiki::WikiError::Yaml(yaml_err) = err else {
        return err.to_string();
    };
    match yaml_err
        .location()
        .and_then(|loc| unquoted_colon_key(content, loc.line()))
    {
        Some(key) => format!(
            "{err} — YAML needs the value quoted, e.g. {key}: \"…\" (a value that contains ': ' \
             or starts with a special character must be in double quotes)"
        ),
        None => err.to_string(),
    }
}

/// `summary` or `title` when line `line` (1-based, counted from the line
/// after the opening `---`, as the YAML parser counts) of `content`'s
/// frontmatter is that key with an unquoted value containing `: `.
fn unquoted_colon_key(content: &str, line: usize) -> Option<&'static str> {
    let text = content.trim_start_matches('\u{feff}');
    let text = text.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    let failing = text.lines().nth(line.checked_sub(1)?)?;
    ["summary", "title"].into_iter().find(|key| {
        failing
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
            .is_some_and(|value| {
                !value.starts_with('"') && !value.starts_with('\'') && value.contains(": ")
            })
    })
}

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
    // brain_dream queue recomputes it.
    drop_stored_queue(vault);
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
    let previous_size_bytes = std::fs::metadata(&target)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
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
        assert!(
            out.contains("Intro."),
            "content before the section preserved"
        );
        assert!(
            out.contains("## Andere\n\nbleibt"),
            "later section untouched: {out}"
        );
    }

    #[test]
    fn patch_section_appends_when_heading_absent() {
        let body = "# Title\n\nIntro.\n";
        let out = patch_section(body, "## Neu", "inhalt");
        assert!(out.contains("Intro."), "existing content kept");
        assert!(
            out.trim_end().ends_with("## Neu\n\ninhalt"),
            "new section appended: {out}"
        );
    }

    #[test]
    fn patch_section_stops_at_same_level_heading_but_includes_deeper_ones() {
        // A `## X` section should swallow a `### sub` but stop at the next `##`.
        let body = "## X\n\nold\n\n### sub\n\nsubtext\n\n## Y\n\nyeahs\n";
        let out = patch_section(body, "## X", "replaced");
        assert!(out.contains("## X\n\nreplaced"), "X replaced");
        assert!(
            !out.contains("### sub"),
            "deeper subsection was part of X and is gone: {out}"
        );
        assert!(!out.contains("subtext"), "sub content gone");
        assert!(
            out.contains("## Y\n\nyeahs"),
            "sibling section Y preserved: {out}"
        );
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

    fn request(id: i64, method: &str, params: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(id)),
            method: method.into(),
            params,
        }
    }

    /// One request against a fresh process (no vault, no index, no
    /// handshake yet).
    fn handle(req: &RpcRequest) -> String {
        handle_request(req, None, &mut None, &mut Session::default())
    }

    #[test]
    fn initialize_response_names_the_server() {
        let resp = handle(&request(1, "initialize", json!({})));
        assert!(
            resp.contains(&format!("\"name\":\"{SERVER_NAME}\"")),
            "{resp}"
        );
    }

    #[test]
    fn tools_list_advertises_exactly_the_catalogue_in_order() {
        let resp: Value =
            serde_json::from_str(&handle(&request(2, "tools/list", json!({})))).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, tools::TOOL_NAMES);
    }

    #[test]
    fn tools_call_without_vault_returns_a_descriptive_error() {
        let resp = handle(&request(
            3,
            "tools/call",
            json!({"name": "brain_query", "arguments": {}}),
        ));
        assert!(resp.contains("no Brain vault is mounted"));
    }

    #[test]
    fn tools_call_without_vault_is_a_tool_error_not_a_protocol_error() {
        let resp: Value = serde_json::from_str(&handle(&request(
            3,
            "tools/call",
            json!({"name": "brain_query", "arguments": {}}),
        )))
        .unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
    }

    #[test]
    fn unknown_method_returns_method_not_found_error() {
        let resp = handle(&request(4, "does_not_exist", json!({})));
        assert!(resp.contains("method not found"));
        assert!(
            !resp.contains("server/discover"),
            "a generic unknown method must keep the generic message: {resp}"
        );
    }

    #[test]
    fn server_discover_without_modern_meta_keeps_the_legacy_method_not_found() {
        let resp: Value =
            serde_json::from_str(&handle(&request(5, "server/discover", json!({})))).unwrap();
        // -32601 (not a modern code such as -32022): a dual-era client must
        // read this as "legacy server" and fall back to `initialize`.
        assert_eq!(
            (
                resp["error"]["code"].clone(),
                resp["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("initialize handshake")
            ),
            (json!(-32601), true)
        );
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
        let resp = handle(&req);
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
        let resp = handle(&req);
        assert!(
            resp.is_empty(),
            "unknown notifications must not get a response"
        );
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
        let resp = handle(&req);
        assert!(resp.is_empty());
    }

    #[test]
    fn get_pages_with_context_does_not_emit_lint_error_after_frontmatter_strip() {
        // Regression: read_page strips the YAML frontmatter from the body,
        // so re-parsing it would fail with "missing frontmatter delimiter"
        // → surfaced as a `lint:` error to the calling LLM. The fix uses
        // extract_wiki_links directly on the body.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
                "name": "brain_get_pages",
                "arguments": { "ids": ["entities/alice"], "include_context": true }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_get_pages with include_context should succeed");
        // Sanity: outbound list must contain the two wiki links.
        assert!(result.contains("entities/bob"));
        assert!(result.contains("concepts/nlspec"));
        // Must not surface any lint chatter.
        assert!(
            !result.contains("lint:"),
            "result leaks lint-error: {result}"
        );
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
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
                "arguments": { "response_format": "detailed" }
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
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
    /// concept and one source. Used by the `brain_query` listing
    /// tests so each test starts from a known
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

    fn query_call(vault: &std::path::Path, args: Value, db: Option<crate::db::DbHandle>) -> Value {
        let mut db = db;
        let result = call_tool(
            &json!({ "name": "brain_query", "arguments": args }),
            vault,
            &mut db,
        )
        .expect("brain_query should succeed");
        serde_json::from_str(&result).expect("result must be valid JSON")
    }

    fn hit_ids(result: &Value) -> Vec<String> {
        result["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn indexed_sample_vault() -> (tempfile::TempDir, crate::db::DbHandle) {
        let tmp = build_sample_vault();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();
        (tmp, db)
    }

    #[test]
    fn query_star_lists_the_same_ids_as_the_filesystem_fallback() {
        // The index-backed listing and the file-system fallback (used when
        // the index is unavailable) must agree, or the LLM gets misled.
        let (tmp, db) = indexed_sample_vault();
        let mut from_index = hit_ids(&query_call(tmp.path(), json!({ "query": "*" }), Some(db)));
        from_index.sort();
        let from_fs: Vec<String> = filesystem_hits(tmp.path())
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(from_index, from_fs);
    }

    #[test]
    fn query_without_arguments_lists_every_current_page() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(tmp.path(), json!({}), Some(db));
        assert_eq!(result["total"], json!(4));
    }

    #[test]
    fn query_with_a_type_filter_lists_only_that_type() {
        let (tmp, db) = indexed_sample_vault();
        let mut ids = hit_ids(&query_call(
            tmp.path(),
            json!({ "query": "type:entity" }),
            Some(db),
        ));
        ids.sort();
        assert_eq!(ids, vec!["entities/alice", "entities/dextra-acme"]);
    }

    #[test]
    fn query_with_a_prefix_returns_only_matching_ids() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(
            tmp.path(),
            json!({ "query": "*", "prefix": "entities/dextra" }),
            Some(db),
        );
        assert_eq!(hit_ids(&result), vec!["entities/dextra-acme"]);
    }

    #[test]
    fn query_with_a_limit_returns_at_most_that_many_hits_and_the_next_offset() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(tmp.path(), json!({ "query": "*", "limit": 1 }), Some(db));
        assert_eq!(
            (result["returned"].clone(), result["next_offset"].clone()),
            (json!(1), json!(1))
        );
    }

    /// 250 entity pages `entities/p000` … `entities/p249`, indexed.
    fn large_indexed_vault() -> (tempfile::TempDir, crate::db::DbHandle) {
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        let tmp = tempfile::TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let dir = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..250 {
            std::fs::write(
                dir.join(format!("p{i:03}.md")),
                format!(
                    "---\nid: entities/p{i:03}\ntype: entity\ntitle: P{i:03}\nupdated: 2026-04-30\n---\n\nbody\n"
                ),
            )
            .unwrap();
        }
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();
        (tmp, db)
    }

    #[test]
    fn a_query_limit_above_five_hundred_is_clamped_to_five_hundred() {
        assert_eq!(query_limit(&json!({ "limit": 100000 })), Ok(500));
    }

    #[test]
    fn a_query_without_a_limit_returns_at_most_one_hundred_hits() {
        assert_eq!(query_limit(&json!({})), Ok(100));
    }

    #[test]
    fn query_star_reports_the_true_total_beyond_two_hundred_pages() {
        let (tmp, db) = large_indexed_vault();
        let result = query_call(tmp.path(), json!({ "query": "*" }), Some(db));
        assert_eq!(result["total"], json!(250));
    }

    #[test]
    fn query_star_pages_through_every_page_with_next_offset() {
        let (tmp, db) = large_indexed_vault();
        let mut seen: Vec<String> = Vec::new();
        let mut offset = 0;
        loop {
            let result = query_call(
                tmp.path(),
                json!({ "query": "*", "limit": 100, "offset": offset }),
                Some(db.clone()),
            );
            seen.extend(hit_ids(&result));
            match result["next_offset"].as_u64() {
                Some(next) => offset = next as usize,
                None => break,
            }
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 250);
    }

    #[test]
    fn query_with_a_prefix_finds_a_page_beyond_position_two_hundred() {
        let (tmp, db) = large_indexed_vault();
        let result = query_call(
            tmp.path(),
            json!({ "query": "type:entity", "prefix": "entities/p24" }),
            Some(db),
        );
        assert!(
            hit_ids(&result).contains(&"entities/p245".to_string()),
            "{result}"
        );
    }

    #[test]
    fn query_with_an_offset_skips_leading_hits() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(
            tmp.path(),
            json!({ "query": "type:entity", "offset": 1 }),
            Some(db),
        );
        assert_eq!(result["returned"], json!(1));
    }

    #[test]
    fn query_star_falls_back_to_the_filesystem_when_the_index_cannot_open() {
        // A vault whose 03_db is a file: the index cannot be opened, the
        // listing still works and says why it is thin.
        let tmp = build_sample_vault();
        let db_dir = tmp.path().join(crate::vault::layout::DB_DIR);
        let _ = std::fs::remove_dir_all(&db_dir);
        std::fs::write(&db_dir, b"not a directory").unwrap();
        let result = query_call(tmp.path(), json!({ "query": "*" }), None);
        assert_eq!(
            (result["total"].clone(), result["note"].is_string()),
            (json!(4), true)
        );
    }

    #[test]
    fn query_concise_hits_carry_only_id_type_and_title() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(tmp.path(), json!({ "query": "type:concept" }), Some(db));
        let keys: Vec<&String> = result["hits"][0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["id", "title", "type"]);
    }

    #[test]
    fn query_detailed_hits_carry_the_salience_counters() {
        let (tmp, db) = indexed_sample_vault();
        let result = query_call(
            tmp.path(),
            json!({ "query": "type:concept", "response_format": "detailed" }),
            Some(db),
        );
        assert!(result["hits"][0]["reads"].is_number(), "{result}");
    }

    #[test]
    fn query_rejects_facet_written_as_query_syntax_with_a_pointer_to_the_argument() {
        let tmp = build_sample_vault();
        let err = call_tool(
            &json!({ "name": "brain_query", "arguments": { "query": "facet:tags" } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("{\"facet\": \"tags\"}"), "{err}");
    }

    #[test]
    fn lookup_returns_true_for_a_page_that_is_on_disk() {
        // The lightweight "does this id exist?"-check the user feedback
        // asked for. Reading the page returns the whole markdown body
        // for this question, which is wasted bandwidth and tokens; this
        // tool only does a single Path::exists() under 02_wiki/<id>.md.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
                "name": "brain_lookup",
                "arguments": { "query_or_id": "entities/alice" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_lookup should succeed");
        let parsed: Value = serde_json::from_str(&result).expect("must be valid JSON");
        assert_eq!(parsed["exists"], json!(true));
        assert_eq!(parsed["id"], json!("entities/alice"));
    }

    #[test]
    fn lookup_returns_false_for_a_page_that_is_not_on_disk() {
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let result = call_tool(
            &json!({
                "name": "brain_lookup",
                "arguments": { "query_or_id": "entities/never-created" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_lookup must succeed even for missing pages — the missing case is data, not error");
        let parsed: Value = serde_json::from_str(&result).expect("must be valid JSON");
        assert_eq!(parsed["exists"], json!(false));
        assert_eq!(parsed["id"], json!("entities/never-created"));
    }

    #[test]
    fn lookup_rejects_id_with_path_traversal_components() {
        // Defensive: an id like "../../../etc/passwd" must be rejected
        // before it gets joined onto the wiki dir. Same hardening the
        // existing brain_write_raw_file does for connector paths.
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_lookup",
                "arguments": { "query_or_id": "../../etc/passwd" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("path traversal must reject");
        assert!(err.contains(".."));
    }

    #[test]
    fn lookup_rejects_empty_or_missing_id() {
        // Hardening: the LLM might forget the `id` arg entirely or
        // pass an empty string. Either way the tool must return a
        // crisp error rather than walking the vault root.
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_lookup",
                "arguments": {}
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("missing id must reject");
        assert!(err.to_lowercase().contains("id"));
    }

    #[test]
    fn lookup_propagates_vault_disconnect_with_canonical_prefix() {
        // The same fast-fail guard call_tool already does for every
        // other tool — when the vault disappeared mid-session, we
        // return the BRAIN_VAULT_DISCONNECTED-prefixed message the
        // LLM has been trained to recognise via the existing tools.
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        // No ensure_skeleton, no marker — looks like a torn-off vault.
        let err = call_tool(
            &json!({
                "name": "brain_lookup",
                "arguments": { "query_or_id": "entities/alice" }
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
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
    fn history_list_returns_only_commits_that_touched_the_named_page() {
        // The roll-back workflow: agent calls brain_history (list),
        // sees the candidate revisions, then brain_history (restore) picks
        // one. The MCP path normalizes the page-id (`entities/alice`)
        // into the on-disk path (`entities/alice.md`) before handing
        // off to the backend.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
                "name": "brain_history",
                "arguments": { "action": "list", "id": "entities/alice" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_history list must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("JSON");
        let commits = parsed
            .get("commits")
            .and_then(|v| v.as_array())
            .expect("commits");
        assert_eq!(
            commits.len(),
            2,
            "two alice-only commits expected, got {commits:?}"
        );
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
    fn history_restore_replaces_current_content_with_the_old_revision() {
        // End-to-end of the rollback: write v1, write v2 over it,
        // call brain_history restore with v1's sha, assert the file on
        // disk matches v1 again. The new revert commit records the
        // action so the history stays append-only.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
                "name": "brain_history",
                "arguments": { "action": "restore", "id": "entities/alice", "sha": v1_sha }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_history restore must succeed");
        // The response surfaces the source sha so the agent can quote
        // it back to the user.
        assert!(
            ok.contains(&v1_sha),
            "response should mention source sha: {ok}"
        );
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
        call_tool(
            &json!({ "name": name, "arguments": arguments }),
            tmp.path(),
            &mut None,
        )
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

    /// The summary that failed in the first dream session on 07.10.2026
    /// (`mapping values are not allowed in this context`).
    const UNQUOTED_COLON_PAGE: &str = "---\nid: entities/grasp\ntype: entity\ntitle: GRASP\nsummary: GRC-Plattform der DextraData GRC Technologies GmbH (auch CIO COCKPIT): SaaS-Cockpit für Governance, Risk und Compliance.\n---\n\nBody.\n";

    #[test]
    fn a_summary_with_an_unquoted_colon_fails_with_a_quoting_hint() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_write_page",
            json!({ "id": "entities/grasp", "content": UNQUOTED_COLON_PAGE }),
        )
        .unwrap_err();
        assert!(
            err.contains("YAML needs the value quoted, e.g. summary: \"…\""),
            "{err}"
        );
    }

    #[test]
    fn a_batch_entry_with_an_unquoted_colon_in_its_summary_fails_with_a_quoting_hint() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_write_batch",
            json!({ "pages": [{ "id": "entities/grasp", "content": UNQUOTED_COLON_PAGE }] }),
        )
        .unwrap_err();
        assert!(err.contains("e.g. summary: \"…\""), "{err}");
    }

    #[test]
    fn the_same_summary_in_double_quotes_is_written() {
        let tmp = refactor_vault();
        let quoted = UNQUOTED_COLON_PAGE.replace(
            "summary: GRC-Plattform der DextraData GRC Technologies GmbH (auch CIO COCKPIT): SaaS-Cockpit für Governance, Risk und Compliance.",
            "summary: \"GRC-Plattform der DextraData GRC Technologies GmbH (auch CIO COCKPIT): SaaS-Cockpit für Governance, Risk und Compliance.\"",
        );
        let result = call(
            &tmp,
            "brain_write_page",
            json!({ "id": "entities/grasp", "content": quoted }),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn a_title_with_an_unquoted_colon_gets_the_hint_for_title() {
        let content = "---\nid: entities/x\ntype: entity\ntitle: Projekt: Phase 2\n---\n\nBody.\n";
        let err = page::parse(content).unwrap_err();
        assert!(
            parse_error_text(content, &err).contains("e.g. title: \"…\""),
            "{}",
            parse_error_text(content, &err)
        );
    }

    #[test]
    fn a_yaml_error_elsewhere_gets_no_quoting_hint() {
        let content = "---\nid: entities/x\ntype: entity\ntags: [a, b\n---\n\nBody.\n";
        let err = page::parse(content).unwrap_err();
        assert!(!parse_error_text(content, &err).contains("needs the value quoted"));
    }

    #[test]
    fn eval_add_then_run_reports_metrics_for_the_three_modes() {
        let tmp = refactor_vault();
        call(
            &tmp,
            "brain_eval",
            json!({ "action": "add", "query": "Knows", "expected": ["entities/alice"] }),
        )
        .expect("brain_eval add must succeed");
        let out = call(&tmp, "brain_eval", json!({ "action": "run" }))
            .expect("brain_eval run must succeed");
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(parsed["modes"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn brain_eval_appends_its_run_to_the_eval_history() {
        let tmp = refactor_vault();
        call(
            &tmp,
            "brain_eval",
            json!({ "action": "add", "query": "Knows", "expected": ["entities/alice"] }),
        )
        .unwrap();
        call(&tmp, "brain_eval", json!({ "action": "run" })).unwrap();
        assert!(crate::viewer::eval::eval_history_path(tmp.path()).is_file());
    }

    #[test]
    fn brain_eval_run_on_an_empty_eval_set_points_at_the_add_action() {
        let tmp = refactor_vault();
        let err = call(&tmp, "brain_eval", json!({ "action": "run" })).unwrap_err();
        assert!(err.contains("brain_eval (action \"add\")"), "{err}");
    }

    #[test]
    fn eval_add_action_refuses_an_expected_page_that_does_not_exist() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_eval",
            json!({ "action": "add", "query": "q", "expected": ["entities/nobody"] }),
        )
        .unwrap_err();
        assert!(err.contains("entities/nobody"), "{err}");
    }

    #[test]
    fn eval_add_action_rejects_a_non_string_note() {
        let tmp = refactor_vault();
        let err = call(
            &tmp,
            "brain_eval",
            json!({ "action": "add", "query": "q", "expected": ["entities/alice"], "note": 3 }),
        )
        .unwrap_err();
        assert!(err.contains("'note' must be a string"), "{err}");
    }

    #[test]
    fn dream_queue_action_writes_and_returns_the_queue() {
        let tmp = refactor_vault();
        let out = call(
            &tmp,
            "brain_dream",
            json!({ "action": "queue", "refresh": true }),
        )
        .expect("brain_dream queue must succeed");
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        let on_disk =
            crate::wiki::dream::read_queue_file(&crate::wiki::dream::dream_queue_path(tmp.path()));
        assert_eq!(
            on_disk.map(|q| q.generated_at),
            parsed["generated_at"].as_str().map(str::to_string)
        );
    }

    #[test]
    fn dream_queue_action_serves_a_fresh_stored_queue_without_recomputing() {
        let tmp = refactor_vault();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 7,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        let out = call(&tmp, "brain_dream", json!({ "action": "queue" })).unwrap();
        let parsed: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(parsed["omitted"], json!(7));
    }

    #[test]
    fn dream_queue_action_with_refresh_recomputes_a_fresh_stored_queue() {
        let tmp = refactor_vault();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 7,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        let out = call(
            &tmp,
            "brain_dream",
            json!({ "action": "queue", "refresh": true }),
        )
        .unwrap();
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
            call_tool(
                &json!({ "name": name, "arguments": arguments }),
                tmp.path(),
                &mut db,
            )
        };

        let before = dream_items(
            &tool("brain_dream", json!({ "action": "queue", "refresh": true })).unwrap(),
        );
        tool(
            "brain_refactor",
            json!({ "action": "merge", "from_id": "entities/b", "into_id": "entities/a" }),
        )
        .expect("merge must succeed");
        let after = dream_items(&tool("brain_dream", json!({ "action": "queue" })).unwrap());

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
        call(
            &tmp,
            "brain_dream",
            json!({ "action": "queue", "refresh": true }),
        )
        .unwrap();
        call(
            &tmp,
            "brain_refactor",
            json!({ "action": "rename", "id": "entities/old", "new_id": "entities/new" }),
        )
        .unwrap();
        assert!(!crate::wiki::dream::dream_queue_path(tmp.path()).exists());
    }

    #[test]
    fn brain_write_page_with_confirm_summary_marks_the_indexed_summary_as_current() {
        let tmp = refactor_vault();
        let page = |body: &str| {
            format!(
                "---\nid: entities/bob\ntype: entity\ntitle: Bob\nsummary: Bob runs ops.\n---\n\n{body}\n"
            )
        };
        call(
            &tmp,
            "brain_write_page",
            json!({ "id": "entities/bob", "content": page("One.") }),
        )
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
    fn dream_log_action_appends_the_entry_to_the_dream_log() {
        let tmp = refactor_vault();
        call(
            &tmp,
            "brain_dream",
            json!({ "action": "log", "entry": "merged a into b" }),
        )
        .unwrap();
        let text = std::fs::read_to_string(crate::wiki::dream::dream_log_path(tmp.path())).unwrap();
        assert!(text.trim_end().ends_with("merged a into b"), "{text}");
    }

    #[test]
    fn refactor_rename_returns_the_rewritten_pages_as_json() {
        let tmp = refactor_vault();
        let ok = call_tool(
            &json!({
                "name": "brain_refactor",
                "arguments": { "action": "rename", "id": "entities/old", "new_id": "entities/new" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_refactor rename must succeed");
        let parsed: Value = serde_json::from_str(&ok).expect("JSON");
        assert_eq!(parsed["rewritten_pages"], json!(["entities/alice"]));
    }

    #[test]
    fn refactor_merge_reports_the_ids_under_the_input_key_names() {
        let tmp = refactor_vault();
        let ok = call_tool(
            &json!({
                "name": "brain_refactor",
                "arguments": { "action": "merge", "from_id": "entities/old", "into_id": "entities/alice" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect("brain_refactor merge must succeed");
        let parsed: Value = serde_json::from_str(&ok).expect("JSON");
        assert_eq!(
            (&parsed["from_id"], &parsed["into_id"]),
            (&json!("entities/old"), &json!("entities/alice"))
        );
    }

    #[test]
    fn refactor_delete_refusal_names_the_referring_pages() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_refactor",
                "arguments": { "action": "delete", "id": "entities/old" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("a linked page must not be deleted without force");
        assert!(
            err.contains("entities/alice"),
            "referrer missing from: {err}"
        );
    }

    #[test]
    fn refactor_delete_rejects_a_non_boolean_force() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_refactor",
                "arguments": { "action": "delete", "id": "entities/old", "force": "yes" }
            }),
            tmp.path(),
            &mut None,
        )
        .expect_err("a string force must be refused");
        assert!(err.contains("force must be a boolean"), "got: {err}");
    }

    #[test]
    fn refactor_rename_rejects_a_non_string_new_id() {
        let tmp = refactor_vault();
        let err = call_tool(
            &json!({
                "name": "brain_refactor",
                "arguments": { "action": "rename", "id": "entities/old", "new_id": 42 }
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
        let id = target
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let page = "---\nid: entities/x\ntype: entity\n---\npwned\n";
        let calls = [
            ("brain_lookup", json!({ "query_or_id": id })),
            (
                "brain_patch_page",
                json!({ "id": id, "heading": "## X", "content": "pwned" }),
            ),
            ("brain_history", json!({ "action": "list", "id": id })),
            (
                "brain_history",
                json!({ "action": "restore", "id": id, "sha": "deadbeef" }),
            ),
            ("brain_write_page", json!({ "id": id, "content": page })),
            (
                "brain_refactor",
                json!({ "action": "delete", "id": id, "force": true }),
            ),
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
        // id must not let get_pages (with or without context) read an
        // arbitrary `.md` file elsewhere on the machine.
        let tmp = refactor_vault();
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("secret.md");
        std::fs::write(
            &target,
            "---\nid: entities/x\ntype: entity\ntitle: T\n---\nOUTSIDE-SECRET-4711\n",
        )
        .unwrap();
        let id = target
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let results: Vec<(&str, Result<String, String>)> = vec![
            (
                "brain_get_pages",
                call_tool(
                    &json!({ "name": "brain_get_pages", "arguments": { "ids": [id] } }),
                    tmp.path(),
                    &mut None,
                ),
            ),
            (
                "brain_get_pages",
                call_tool(
                    &json!({ "name": "brain_get_pages", "arguments": { "ids": [id], "include_context": true } }),
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
    fn history_restore_rejects_path_traversal_in_id() {
        // Same hardening as brain_lookup / brain_write_raw_file:
        // an id with `..` could resolve outside the wiki root once
        // joined onto wiki_dir(vault). Reject before reaching git.
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        let err = call_tool(
            &json!({
                "name": "brain_history",
                "arguments": { "action": "restore", "id": "../../../etc/passwd", "sha": "deadbeef" }
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
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
        let wrote = parsed
            .get("wrote")
            .and_then(|v| v.as_array())
            .expect("wrote array");
        assert_eq!(wrote.len(), 3, "one summary per page in the batch");
        // Each entry carries the new/previous size so the agent can
        // self-check for accidental shrink even in batch context.
        for entry in wrote {
            assert!(entry.get("id").and_then(|v| v.as_str()).is_some());
            assert!(
                entry
                    .get("new_size_bytes")
                    .and_then(|v| v.as_i64())
                    .is_some()
            );
            assert!(
                entry
                    .get("previous_size_bytes")
                    .and_then(|v| v.as_i64())
                    .is_some()
            );
        }
    }

    #[test]
    fn brain_write_batch_rejects_the_whole_batch_when_any_single_page_fails_to_parse() {
        // Strict atomicity on the validation phase: if any entry in
        // the batch has invalid frontmatter, nothing gets written.
        // Otherwise the user would end up with a half-written batch
        // and would need a partial-rollback heuristic to recover.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
        assert!(
            err.contains("entities/bad"),
            "error should name the offending id: {err}"
        );
        // Neither file may have been written to disk — phase 1
        // validation runs entirely in memory before phase 2 writes.
        assert!(
            !wiki_dir(tmp.path()).join("entities/good.md").exists(),
            "good page must not be written when sibling fails parse — \"atomic\" is the contract"
        );
    }

    /// `brain_ping` with `detail: true` against a fresh vault (no model
    /// files, no index), through the full request path.
    fn ping_detail(vault: &std::path::Path) -> Value {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(8)),
            method: "tools/call".into(),
            params: json!({ "name": "brain_ping", "arguments": { "detail": true } }),
        };
        let env: Value = serde_json::from_str(&handle_request(
            &req,
            Some(vault),
            &mut None,
            &mut Session::default(),
        ))
        .unwrap();
        serde_json::from_str(env["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn fresh_vault() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        tmp
    }

    #[test]
    fn ping_detail_reports_the_hashed_fallback_on_a_vault_without_model_files() {
        // The user could not tell whether hybrid search ran on real bge-m3
        // vectors or on the deterministic hashed fallback (valid numbers,
        // no semantic meaning). Formerly a separate status tool.
        let tmp = fresh_vault();
        let embedder = ping_detail(tmp.path())["embedder"].clone();
        assert_eq!(
            (embedder["active"].clone(), embedder["semantic"].clone()),
            (json!("hashed-fh-1024"), json!(false))
        );
    }

    #[test]
    fn ping_detail_points_at_the_bge_m3_model_directory() {
        let tmp = fresh_vault();
        let model_dir = ping_detail(tmp.path())["embedder"]["model_dir"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(model_dir.ends_with("04_models/bge-m3"), "{model_dir}");
    }

    #[test]
    fn ping_detail_reports_the_model_as_not_loaded_without_loading_it() {
        let tmp = fresh_vault();
        assert_eq!(
            ping_detail(tmp.path())["embedder"]["model_state"],
            json!("not-loaded")
        );
    }

    #[test]
    fn ping_detail_reports_an_unbuilt_index_as_unavailable() {
        let tmp = fresh_vault();
        assert_eq!(ping_detail(tmp.path())["index"]["available"], json!(false));
    }

    #[test]
    fn ping_detail_never_builds_the_index() {
        let tmp = build_sample_vault();
        ping_detail(tmp.path());
        let pages: i64 = crate::db::DbHandle::open(tmp.path())
            .unwrap()
            .with(|c| Ok(c.query_row("SELECT count(*) FROM pages", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(pages, 0);
    }

    #[test]
    fn ping_detail_counts_the_pages_of_a_built_index() {
        let (tmp, _db) = indexed_sample_vault();
        assert_eq!(ping_detail(tmp.path())["index"]["pages"], json!(4));
    }

    #[test]
    fn ping_detail_without_a_configured_vault_says_so() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(9)),
            method: "tools/call".into(),
            params: json!({ "name": "brain_ping", "arguments": { "detail": true } }),
        };
        let env: Value = serde_json::from_str(&handle(&req)).unwrap();
        let ping: Value =
            serde_json::from_str(env["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(ping["vault"]["configured"], json!(false));
    }

    #[test]
    fn query_tag_facet_returns_distinct_tags_with_counts_sorted_by_frequency() {
        // The user couldn't discover which tags exist in the vault.
        // `brain_query tag:foo` accepts an exact tag operator, but
        // there was no way to ask "what are the candidate values?".
        // This tool reads `page_tags` and returns each distinct tag
        // with how many pages carry it, sorted descending so the
        // agent sees the most-used tags first.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
            &json!({ "name": "brain_query", "arguments": { "facet": "tags" } }),
            tmp.path(),
            &mut db,
        )
        .expect("brain_query facet:tags must succeed on a populated vault");
        let parsed: serde_json::Value = serde_json::from_str(&ok).expect("response is JSON");
        let tags = parsed
            .get("tags")
            .and_then(|v| v.as_array())
            .expect("tags array");
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
        assert_eq!(
            tags[0].get("tag").and_then(|v| v.as_str()),
            Some("customer")
        );
        assert_eq!(tags[0].get("count").and_then(|v| v.as_i64()), Some(2));
        // The two singletons follow, in alphabetic order on ties.
        let next_names: Vec<&str> = tags
            .iter()
            .skip(1)
            .take(2)
            .filter_map(|v| v.get("tag").and_then(|t| t.as_str()))
            .collect();
        assert_eq!(
            next_names,
            vec!["dax", "partner"],
            "alphabetic tie-break on count == 1"
        );
    }

    #[test]
    fn brain_get_pages_returns_results_for_existing_ids_and_marks_missing_ones() {
        // Bulk-read use case: refactor sweeps where the agent wants to
        // inspect 10–20 related pages at once. Pre-0.2.17 the only
        // option was N sequential single-page reads, which
        // serialised wall-clock time on the MCP transport. Now one
        // call returns an array of `{id, found, page?, error?}` so the
        // agent can branch on each entry without round-trips.
        // Missing ids must NOT abort the whole call — return them
        // marked `found: false` so the agent can decide per-id
        // whether to create-or-skip.
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
        let pages = parsed
            .get("pages")
            .and_then(|v| v.as_array())
            .expect("pages array");
        assert_eq!(
            pages.len(),
            3,
            "one entry per requested id, in request order"
        );
        assert_eq!(
            pages[0].get("id").and_then(|v| v.as_str()),
            Some("entities/alice")
        );
        assert_eq!(pages[0].get("found").and_then(|v| v.as_bool()), Some(true));
        assert!(
            pages[0].get("page").is_some(),
            "found entries carry the page payload"
        );
        assert_eq!(
            pages[1].get("id").and_then(|v| v.as_str()),
            Some("entities/missing")
        );
        assert_eq!(pages[1].get("found").and_then(|v| v.as_bool()), Some(false));
        assert!(
            pages[1].get("page").is_none(),
            "missing entries omit the page payload"
        );
        assert_eq!(
            pages[2].get("id").and_then(|v| v.as_str()),
            Some("entities/bob")
        );
        assert_eq!(pages[2].get("found").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn db_op_opens_a_handle_lazily_when_none_is_held() {
        // Cold start / first DB call: db starts None, vault present →
        // db_op opens the handle, runs the op, leaves the handle cached.
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
            &mut Session::default(),
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
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        seed_marker(tmp.path());
        // Seed an existing rich page.
        let entities = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&entities).unwrap();
        let rich = "---\nid: entities/alice\ntype: entity\ntitle: Alice\ncreated: 2026-04-30\nupdated: 2026-04-30\n---\n\n";
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
        let prev = parsed
            .get("previous_size_bytes")
            .and_then(|v| v.as_i64())
            .expect("previous_size_bytes");
        let new = parsed
            .get("new_size_bytes")
            .and_then(|v| v.as_i64())
            .expect("new_size_bytes");
        assert!(
            prev > new,
            "previous ({prev}) must exceed new ({new}) for this shrink test"
        );
        assert!(prev > 1000, "previous size sanity (got {prev})");
        assert!(new < 200, "new size sanity (got {new})");
    }

    #[test]
    fn patch_page_replaces_one_section_and_preserves_the_rest() {
        use crate::vault::layout::{ensure_skeleton, wiki_dir};
        use tempfile::TempDir;
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
        assert!(
            ok.contains("entities/alice"),
            "response names the page: {ok}"
        );

        let after = std::fs::read_to_string(entities.join("alice.md")).unwrap();
        assert!(
            after.starts_with("---\nid: entities/alice"),
            "frontmatter preserved: {after}"
        );
        assert!(
            after.contains("## Kontakt\n\nneue Nummer +49 201 0"),
            "section replaced: {after}"
        );
        assert!(!after.contains("alte Nummer"), "old section gone: {after}");
        assert!(after.contains("Intro."), "intro preserved: {after}");
        assert!(
            after.contains("## Notizen\n\nbleibt"),
            "sibling section preserved: {after}"
        );
    }

    #[test]
    fn patch_page_errors_when_the_page_does_not_exist() {
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
        assert!(
            err.contains("page not found"),
            "patch on a missing page must fail clearly: {err}"
        );
    }

    #[test]
    fn write_page_response_includes_page_scoped_warnings_when_present() {
        // Warning-level findings (e.g. missing-title) on the just-
        // written page must surface in the success response so the
        // agent can self-correct on the next round-trip without an
        // extra brain_lint_report call. The page itself still
        // writes successfully — warnings do not block.
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
        assert!(
            err.contains("unregistered-type"),
            "error must name the lint kind: {err}"
        );
        assert!(
            err.contains("entities"),
            "error must echo the offending value: {err}"
        );
        // The four singular forms must be in the message so the
        // agent doesn't have to fetch them from a doc tool.
        for valid in &["entity", "concept", "source", "topic"] {
            assert!(
                err.contains(valid),
                "valid type '{valid}' missing in error: {err}"
            );
        }
    }

    #[test]
    fn write_raw_file_rejects_path_traversal_attempts() {
        use crate::vault::layout::ensure_skeleton;
        use tempfile::TempDir;
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
                Ok(c.query_row(
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
    fn lookup_reports_a_normalised_match_for_a_differently_spelled_new_id() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "entities/Mueller_GmbH" }),
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
    fn lookup_reports_an_alias_match() {
        let tmp = vault();
        put(
            tmp.path(),
            "entities/acme",
            "aliases: [ACME Corporation]\n",
            "Body.",
        );
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "entities/acme-corporation" }),
        );
        assert_eq!(out["matches"][0]["reason"], json!("alias"));
    }

    #[test]
    fn lookup_reports_a_similar_match() {
        let tmp = vault();
        put(tmp.path(), "entities/dan-shapiro", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "entities/dan-shapio" }),
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
            err.contains(
                "probably exists already: entities/mueller-gmbh \"mueller-gmbh\" (normalised)"
            ),
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
        put(
            tmp.path(),
            "entities/acme",
            "aliases: [ACME Corporation]\n",
            "Body.",
        );
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
        assert!(
            err.contains("allow_duplicate must be a boolean"),
            "got: {err}"
        );
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
        assert!(
            err.starts_with("pages[1] (entities/Mueller_GmbH)"),
            "got: {err}"
        );
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
        put(
            tmp.path(),
            "entities/b",
            "sources: [sources/s]\n",
            "New facts.",
        );
        put(tmp.path(), "sources/s", "", "Source.");
        tmp
    }

    #[test]
    fn get_pages_with_context_of_a_superseded_page_names_the_successor() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"], "include_context": true }),
        );
        assert_eq!(
            out["pages"][0]["page"]["superseded_by"],
            json!("entities/b")
        );
    }

    #[test]
    fn get_pages_of_a_superseded_page_carries_a_notice_field() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"], "include_context": true }),
        );
        assert_eq!(
            out["pages"][0]["page"]["notice"],
            json!("Superseded by entities/b")
        );
    }

    #[test]
    fn get_pages_of_a_superseded_page_returns_the_body_verbatim() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"], "include_context": true }),
        );
        assert_eq!(out["pages"][0]["page"]["body"], json!("Old facts.\n"));
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
    fn get_pages_of_a_current_page_has_no_superseded_field() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/b"], "include_context": true }),
        );
        assert!(
            out["pages"][0]["page"].get("superseded_by").is_none(),
            "got: {out}"
        );
    }

    #[test]
    fn get_pages_with_context_does_not_list_the_superseded_line_as_an_outbound_link() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"], "include_context": true }),
        );
        assert_eq!(out["pages"][0]["outbound"], json!([]));
    }

    #[test]
    fn get_pages_of_a_superseded_page_names_the_successor() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"] }),
        );
        assert_eq!(
            out["pages"][0]["page"]["superseded_by"],
            json!("entities/b")
        );
    }

    #[test]
    fn reading_a_superseded_page_leaves_its_file_unchanged() {
        let tmp = superseded_vault();
        let before = std::fs::read_to_string(page_file(tmp.path(), "entities/a")).unwrap();
        call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_get_pages",
            json!({ "ids": ["entities/a"] }),
        );
        let after = std::fs::read_to_string(page_file(tmp.path(), "entities/a")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn brain_query_leaves_out_a_superseded_page_by_default() {
        let tmp = superseded_vault();
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_query",
            json!({ "query": "type:entity" }),
        );
        let ids: Vec<&str> = out["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["entities/b"]);
    }

    // ---- H3 ---------------------------------------------------------------

    #[test]
    fn get_pages_counts_one_read_per_call() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        for _ in 0..3 {
            call_json(
                tmp.path(),
                &mut db,
                "brain_get_pages",
                json!({ "ids": ["entities/alice"] }),
            );
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
            (
                access(&db, "entities/alice").0,
                access(&db, "entities/missing").0
            ),
            (1, 0)
        );
    }

    #[test]
    fn get_pages_with_context_counts_a_read_of_the_page() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["entities/alice"], "include_context": true }),
        );
        assert_eq!(access(&db, "entities/alice").0, 1);
    }

    #[test]
    fn search_counts_a_search_hit_for_a_returned_page() {
        let tmp = vault();
        put(
            tmp.path(),
            "concepts/zebrafish",
            "",
            "The zebrafish genome.",
        );
        let mut db = indexed(tmp.path());
        call(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        )
        .unwrap();
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
        call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["entities/old"] }),
        );
        call(
            tmp.path(),
            &mut db,
            "brain_refactor",
            json!({ "action": "rename", "id": "entities/old", "new_id": "entities/new" }),
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
    fn get_pages_on_an_unbuilt_index_does_not_build_it() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = None;
        call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["entities/alice"] }),
        );
        assert_eq!((db.is_none(), indexed_page_count(tmp.path())), (true, 0));
    }

    #[test]
    fn lookup_on_an_unbuilt_index_says_matches_were_not_checked() {
        let tmp = vault();
        put(tmp.path(), "entities/mueller-gmbh", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_lookup",
            json!({ "query_or_id": "entities/muller-gmbh" }),
        );
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

/// Slice 0.3 (dual-era protocol) — the behaviour matrix of
/// `docs/research/2026-10-mcp-spec-2026-07-28-gap.md` §4, one assertion
/// per test.
#[cfg(test)]
mod protocol_tests {
    use super::*;

    fn legacy(id: i64, method: &str, params: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(id)),
            method: method.into(),
            params,
        }
    }

    /// A 2026-07-28 request: `params` plus the required `_meta`.
    fn modern(id: i64, method: &str, mut params: Value) -> RpcRequest {
        params["_meta"] = json!({
            META_PROTOCOL_VERSION: "2026-07-28",
            META_CLIENT_CAPABILITIES: {},
            META_CLIENT_INFO: { "name": "test-client", "version": "1" }
        });
        legacy(id, method, params)
    }

    fn send(req: &RpcRequest, session: &mut Session) -> Value {
        serde_json::from_str(&handle_request(req, None, &mut None, session)).unwrap()
    }

    fn once(req: &RpcRequest) -> Value {
        send(req, &mut Session::default())
    }

    /// The protocol version `initialize` answers for `requested`.
    fn negotiated(requested: Value) -> Value {
        once(&legacy(
            1,
            "initialize",
            json!({ "protocolVersion": requested }),
        ))["result"]["protocolVersion"]
            .clone()
    }

    /// `tools/list` after an `initialize` with `version`.
    fn tools_after_initialize(version: &str) -> Vec<Value> {
        let mut session = Session::default();
        send(
            &legacy(1, "initialize", json!({ "protocolVersion": version })),
            &mut session,
        );
        send(&legacy(2, "tools/list", json!({})), &mut session)["result"]["tools"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn ping_call_after_initialize(version: &str) -> Value {
        let mut session = Session::default();
        send(
            &legacy(1, "initialize", json!({ "protocolVersion": version })),
            &mut session,
        );
        send(
            &legacy(
                2,
                "tools/call",
                json!({ "name": "brain_ping", "arguments": {} }),
            ),
            &mut session,
        )["result"]
            .clone()
    }

    // ---- legacy handshake -------------------------------------------------

    #[test]
    fn initialize_echoes_protocol_version_2024_11_05() {
        assert_eq!(negotiated(json!("2024-11-05")), json!("2024-11-05"));
    }

    #[test]
    fn initialize_echoes_protocol_version_2025_03_26() {
        assert_eq!(negotiated(json!("2025-03-26")), json!("2025-03-26"));
    }

    #[test]
    fn initialize_echoes_protocol_version_2025_06_18() {
        assert_eq!(negotiated(json!("2025-06-18")), json!("2025-06-18"));
    }

    #[test]
    fn initialize_echoes_protocol_version_2025_11_25() {
        assert_eq!(negotiated(json!("2025-11-25")), json!("2025-11-25"));
    }

    #[test]
    fn initialize_answers_2025_11_25_for_an_unsupported_version() {
        assert_eq!(negotiated(json!("1999-01-01")), json!("2025-11-25"));
    }

    #[test]
    fn initialize_answers_2025_11_25_when_the_client_names_no_version() {
        let resp = once(&legacy(1, "initialize", json!({})));
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-11-25"));
    }

    #[test]
    fn initialize_with_the_2026_modern_version_falls_back_to_the_latest_legacy_version() {
        assert_eq!(negotiated(json!("2026-07-28")), json!("2025-11-25"));
    }

    #[test]
    fn initialize_advertises_tools_prompts_and_resources_without_list_changes() {
        let resp = once(&legacy(
            1,
            "initialize",
            json!({ "protocolVersion": "2024-11-05" }),
        ));
        assert_eq!(
            resp["result"]["capabilities"],
            json!({
                "tools": { "listChanged": false },
                "prompts": { "listChanged": false },
                "resources": { "listChanged": false }
            })
        );
    }

    #[test]
    fn a_legacy_initialize_result_has_exactly_the_four_initialize_fields() {
        let resp = once(&legacy(
            1,
            "initialize",
            json!({ "protocolVersion": "2024-11-05" }),
        ));
        let keys: Vec<&String> = resp["result"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "capabilities",
                "instructions",
                "protocolVersion",
                "serverInfo"
            ]
        );
    }

    #[test]
    fn legacy_ping_answers_an_empty_object() {
        assert_eq!(once(&legacy(1, "ping", json!({})))["result"], json!({}));
    }

    #[test]
    fn legacy_tools_list_carries_no_modern_fields() {
        let result = once(&legacy(1, "tools/list", json!({})))["result"].clone();
        let modern_fields: Vec<&str> = ["resultType", "ttlMs", "cacheScope", "_meta"]
            .into_iter()
            .filter(|k| result.get(*k).is_some())
            .collect();
        assert!(modern_fields.is_empty(), "{modern_fields:?}");
    }

    #[test]
    fn tools_for_a_2024_11_05_client_have_only_name_description_and_input_schema() {
        let stray: Vec<String> = tools_after_initialize("2024-11-05")
            .iter()
            .flat_map(|t| t.as_object().unwrap().keys().cloned().collect::<Vec<_>>())
            .filter(|k| !["name", "description", "inputSchema"].contains(&k.as_str()))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    #[test]
    fn tools_for_a_2025_03_26_client_carry_annotations_but_no_output_schema() {
        let tools = tools_after_initialize("2025-03-26");
        assert_eq!(
            (
                tools[0].get("annotations").is_some(),
                tools[0].get("outputSchema").is_some()
            ),
            (true, false)
        );
    }

    #[test]
    fn tools_for_a_2025_06_18_client_carry_title_and_output_schema() {
        let tools = tools_after_initialize("2025-06-18");
        assert_eq!(
            (
                tools[0]["title"].clone(),
                tools[0]["outputSchema"]["type"].clone()
            ),
            (json!("Ping BRAIN"), json!("object"))
        );
    }

    #[test]
    fn every_tool_declares_whether_it_is_read_only() {
        let missing: Vec<String> = tools_after_initialize("2025-11-25")
            .iter()
            .filter(|t| !t["annotations"]["readOnlyHint"].is_boolean())
            .map(|t| t["name"].to_string())
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn every_tool_description_names_a_sibling_to_use_instead() {
        // "One sentence WHEN, one WHEN NOT (point to the sibling)".
        let without_sibling: Vec<String> = tool_specs()
            .iter()
            .filter(|t| {
                !tools::TOOL_NAMES
                    .iter()
                    .any(|other| *other != t.name && t.description.contains(other))
            })
            .map(|t| t.name.to_string())
            .collect();
        assert!(without_sibling.is_empty(), "{without_sibling:?}");
    }

    #[test]
    fn a_2024_11_05_tool_result_carries_no_structured_content() {
        assert!(
            ping_call_after_initialize("2024-11-05")
                .get("structuredContent")
                .is_none()
        );
    }

    #[test]
    fn a_2025_06_18_tool_result_carries_structured_content() {
        assert_eq!(
            ping_call_after_initialize("2025-06-18")["structuredContent"]["status"],
            json!("ok")
        );
    }

    #[test]
    fn the_text_content_of_a_json_tool_result_is_compact_json() {
        let text = ping_call_after_initialize("2025-11-25")["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!text.contains('\n'), "{text}");
    }

    // ---- modern (2026-07-28) --------------------------------------------

    #[test]
    fn modern_tools_list_is_marked_complete() {
        assert_eq!(
            once(&modern(1, "tools/list", json!({})))["result"]["resultType"],
            json!("complete")
        );
    }

    #[test]
    fn modern_tools_list_carries_a_one_hour_ttl() {
        assert_eq!(
            once(&modern(1, "tools/list", json!({})))["result"]["ttlMs"],
            json!(3_600_000)
        );
    }

    #[test]
    fn modern_tools_list_is_publicly_cacheable() {
        assert_eq!(
            once(&modern(1, "tools/list", json!({})))["result"]["cacheScope"],
            json!("public")
        );
    }

    #[test]
    fn modern_results_name_the_server_in_meta() {
        let resp = once(&modern(1, "tools/list", json!({})));
        assert_eq!(
            resp["result"]["_meta"][META_SERVER_INFO]["name"],
            json!("BRAIN")
        );
    }

    #[test]
    fn modern_tools_list_includes_output_schemas() {
        let resp = once(&modern(1, "tools/list", json!({})));
        assert!(resp["result"]["tools"][0]["outputSchema"].is_object());
    }

    #[test]
    fn server_discover_lists_2026_07_28_as_supported() {
        let resp = once(&modern(1, "server/discover", json!({})));
        assert_eq!(resp["result"]["supportedVersions"], json!(["2026-07-28"]));
    }

    #[test]
    fn server_discover_returns_the_capabilities() {
        let resp = once(&modern(1, "server/discover", json!({})));
        assert_eq!(resp["result"]["capabilities"], capabilities());
    }

    #[test]
    fn server_discover_is_marked_complete() {
        let resp = once(&modern(1, "server/discover", json!({})));
        assert_eq!(resp["result"]["resultType"], json!("complete"));
    }

    #[test]
    fn server_discover_carries_a_one_hour_ttl() {
        let resp = once(&modern(1, "server/discover", json!({})));
        assert_eq!(resp["result"]["ttlMs"], json!(3_600_000));
    }

    #[test]
    fn a_modern_request_without_client_capabilities_is_invalid_params() {
        let mut req = modern(1, "tools/list", json!({}));
        req.params["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(META_CLIENT_CAPABILITIES);
        assert_eq!(once(&req)["error"]["code"], json!(-32602));
    }

    #[test]
    fn a_modern_request_with_an_unknown_version_gets_unsupported_protocol_version() {
        let mut req = modern(1, "tools/list", json!({}));
        req.params["_meta"][META_PROTOCOL_VERSION] = json!("1900-01-01");
        let resp = once(&req);
        assert_eq!(
            (resp["error"]["code"].clone(), resp["error"]["data"].clone()),
            (
                json!(-32022),
                json!({ "supported": ["2026-07-28"], "requested": "1900-01-01" })
            )
        );
    }

    #[test]
    fn modern_ping_is_method_not_found() {
        assert_eq!(
            once(&modern(1, "ping", json!({})))["error"]["code"],
            json!(-32601)
        );
    }

    #[test]
    fn modern_initialize_is_method_not_found() {
        assert_eq!(
            once(&modern(1, "initialize", json!({})))["error"]["code"],
            json!(-32601)
        );
    }

    #[test]
    fn modern_brain_ping_answers_without_a_vault_and_is_marked_complete() {
        let resp = once(&modern(
            1,
            "tools/call",
            json!({ "name": "brain_ping", "arguments": {} }),
        ));
        assert_eq!(
            (
                resp["result"]["isError"].clone(),
                resp["result"]["resultType"].clone()
            ),
            (json!(false), json!("complete"))
        );
    }

    #[test]
    fn an_initialize_and_a_modern_request_in_one_process_are_both_served() {
        let mut session = Session::default();
        send(
            &legacy(1, "initialize", json!({ "protocolVersion": "2024-11-05" })),
            &mut session,
        );
        let resp = send(&modern(2, "tools/list", json!({})), &mut session);
        assert_eq!(resp["result"]["resultType"], json!("complete"));
    }

    #[test]
    fn a_modern_request_after_a_2024_initialize_still_gets_every_tool_field() {
        let mut session = Session::default();
        send(
            &legacy(1, "initialize", json!({ "protocolVersion": "2024-11-05" })),
            &mut session,
        );
        let resp = send(&modern(2, "tools/list", json!({})), &mut session);
        assert!(resp["result"]["tools"][0]["annotations"].is_object());
    }

    #[test]
    fn a_modern_notifications_initialized_yields_no_reply() {
        let mut req = modern(1, "notifications/initialized", json!({}));
        req.id = None;
        assert!(handle_request(&req, None, &mut None, &mut Session::default()).is_empty());
    }

    // ---- tool names ---------------------------------------------------------

    #[test]
    fn every_removed_tool_name_gets_invalid_params_naming_its_replacement() {
        let wrong: Vec<String> = tools::REMOVED_TOOLS
            .iter()
            .filter_map(|r| {
                let resp = once(&legacy(
                    1,
                    "tools/call",
                    json!({ "name": r.old, "arguments": {} }),
                ));
                let ok = resp["error"]["code"] == json!(-32602)
                    && resp["error"]["message"]
                        .as_str()
                        .is_some_and(|m| m.contains(&format!("replaced by '{}'", r.new)));
                (!ok).then(|| format!("{}: {resp}", r.old))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn a_removed_tool_name_is_refused_the_same_way_in_the_modern_era() {
        let resp = once(&modern(
            1,
            "tools/call",
            json!({ "name": tools::REMOVED_TOOLS[0].old }),
        ));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn an_unknown_tool_is_invalid_params_not_a_tool_error() {
        let resp = once(&legacy(1, "tools/call", json!({ "name": "brain_nope" })));
        assert_eq!(
            (
                resp["error"]["code"].clone(),
                resp["error"]["message"].clone()
            ),
            (json!(-32602), json!("Unknown tool: brain_nope"))
        );
    }

    // ---- prompts ------------------------------------------------------------

    fn prompt_text(name: &str, arguments: Value) -> String {
        let resp = once(&legacy(
            1,
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
        ));
        resp["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no prompt text: {resp}"))
            .to_string()
    }

    #[test]
    fn prompts_list_names_ingest_lint_session_and_dream() {
        let resp = once(&legacy(1, "prompts/list", json!({})));
        let names: Vec<&str> = resp["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["ingest", "lint-session", "dream"]);
    }

    #[test]
    fn the_ingest_prompt_names_the_source_to_ingest() {
        let text = prompt_text("ingest", json!({ "source": "01_raw/email/kickoff.eml" }));
        assert!(text.contains("01_raw/email/kickoff.eml"), "{text}");
    }

    #[test]
    fn the_ingest_prompt_writes_the_pages_with_one_write_batch() {
        let text = prompt_text("ingest", json!({ "source": "x" }));
        assert!(text.contains("ONE brain_write_batch"), "{text}");
    }

    #[test]
    fn the_ingest_prompt_without_a_source_is_invalid_params() {
        let resp = once(&legacy(1, "prompts/get", json!({ "name": "ingest" })));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn the_lint_session_prompt_narrows_to_the_focus_kind() {
        let text = prompt_text("lint-session", json!({ "focus": "orphan" }));
        assert!(text.contains("only on findings of kind `orphan`"), "{text}");
    }

    #[test]
    fn the_lint_session_prompt_accepts_the_ingestion_master_index_as_a_source() {
        let text = prompt_text("lint-session", json!({}));
        assert!(text.contains("master-index topic page"), "{text}");
    }

    #[test]
    fn the_lint_session_prompt_starts_from_the_newest_audit() {
        let text = prompt_text("lint-session", json!({}));
        assert!(text.contains("brain://audit/latest"), "{text}");
    }

    #[test]
    fn the_dream_prompt_caps_a_session_at_ten_changes_by_default() {
        let text = prompt_text("dream", json!({}));
        assert!(text.contains("At most 10 changes"), "{text}");
    }

    #[test]
    fn the_dream_prompt_forbids_deleting_linked_pages() {
        let text = prompt_text("dream", json!({}));
        assert!(
            text.contains("Never delete a page that other pages link to"),
            "{text}"
        );
    }

    #[test]
    fn the_dream_prompt_ends_with_a_dream_log_entry() {
        let text = prompt_text("dream", json!({}));
        assert!(text.contains("brain_dream action \"log\""), "{text}");
    }

    #[test]
    fn the_dream_prompt_takes_a_custom_change_limit() {
        let text = prompt_text("dream", json!({ "max_changes": "3" }));
        assert!(text.contains("At most 3 changes"), "{text}");
    }

    #[test]
    fn the_dream_prompt_rejects_a_non_numeric_change_limit() {
        let resp = once(&legacy(
            1,
            "prompts/get",
            json!({ "name": "dream", "arguments": { "max_changes": "many" } }),
        ));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn an_unknown_prompt_is_invalid_params() {
        let resp = once(&legacy(1, "prompts/get", json!({ "name": "nope" })));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn modern_prompts_list_carries_a_ttl() {
        assert_eq!(
            once(&modern(1, "prompts/list", json!({})))["result"]["ttlMs"],
            json!(3_600_000)
        );
    }

    // ---- resources ------------------------------------------------------------

    fn vault_with_marker() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        let marker = crate::vault::marker::VaultMarker::new("test");
        crate::vault::marker::write_marker(tmp.path(), &marker).unwrap();
        tmp
    }

    fn read_resource(uri: &str, vault: Option<&std::path::Path>) -> Value {
        let req = legacy(1, "resources/read", json!({ "uri": uri }));
        serde_json::from_str(&handle_request(
            &req,
            vault,
            &mut None,
            &mut Session::default(),
        ))
        .unwrap()
    }

    fn resource_text(resp: &Value) -> String {
        resp["result"]["contents"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no resource text: {resp}"))
            .to_string()
    }

    #[test]
    fn resources_list_names_the_three_brain_resources() {
        let resp = once(&legacy(1, "resources/list", json!({})));
        let uris: Vec<&str> = resp["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert_eq!(
            uris,
            vec![
                "brain://agents-md",
                "brain://audit/latest",
                "brain://dream-queue"
            ]
        );
    }

    #[test]
    fn the_agents_md_resource_serves_the_vault_file() {
        let tmp = vault_with_marker();
        std::fs::write(
            crate::vault::layout::meta_dir(tmp.path()).join("AGENTS.md"),
            "# customised conventions",
        )
        .unwrap();
        let text = resource_text(&read_resource("brain://agents-md", Some(tmp.path())));
        assert_eq!(text, "# customised conventions");
    }

    #[test]
    fn the_agents_md_resource_serves_the_bundled_template_without_a_vault() {
        let text = resource_text(&read_resource("brain://agents-md", None));
        assert_eq!(text, crate::onboarding::template::AGENTS_MD);
    }

    #[test]
    fn the_audit_resource_serves_the_newest_audit_report() {
        let tmp = vault_with_marker();
        let dir = crate::wiki::audit::audit_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-10-01.md"), "old audit").unwrap();
        std::fs::write(dir.join("2026-10-05.md"), "new audit").unwrap();
        let text = resource_text(&read_resource("brain://audit/latest", Some(tmp.path())));
        assert_eq!(text, "new audit");
    }

    #[test]
    fn the_audit_resource_without_any_audit_is_resource_not_found() {
        let tmp = vault_with_marker();
        let resp = read_resource("brain://audit/latest", Some(tmp.path()));
        assert_eq!(resp["error"]["code"], json!(-32002));
    }

    #[test]
    fn the_dream_queue_resource_serves_the_stored_queue_as_json() {
        let tmp = vault_with_marker();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 0,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        let queue: Value = serde_json::from_str(&resource_text(&read_resource(
            "brain://dream-queue",
            Some(tmp.path()),
        )))
        .unwrap();
        assert!(queue["items"].is_array(), "{queue}");
    }

    #[test]
    fn the_dream_queue_resource_without_a_vault_is_the_no_vault_error() {
        let resp = read_resource("brain://dream-queue", None);
        assert_eq!(resp["error"]["code"], json!(NO_VAULT));
    }

    #[test]
    fn an_unknown_resource_is_resource_not_found() {
        let resp = read_resource("brain://nope", None);
        assert_eq!(resp["error"]["code"], json!(-32002));
    }

    #[test]
    fn a_modern_resource_read_is_privately_cacheable() {
        let resp = once(&modern(
            1,
            "resources/read",
            json!({ "uri": "brain://agents-md" }),
        ));
        assert_eq!(resp["result"]["cacheScope"], json!("private"));
    }

    // ---- fix round: instructions, legacy _meta, actions, prompt args ------

    #[test]
    fn initialize_for_2025_03_26_carries_the_usage_instructions() {
        let resp = once(&legacy(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        ));
        assert_eq!(resp["result"]["instructions"], json!(INSTRUCTIONS));
    }

    #[test]
    fn initialize_for_2024_11_05_carries_the_usage_instructions_too() {
        let resp = once(&legacy(
            1,
            "initialize",
            json!({ "protocolVersion": "2024-11-05" }),
        ));
        assert_eq!(resp["result"]["instructions"], json!(INSTRUCTIONS));
    }

    #[test]
    fn server_discover_carries_the_usage_instructions() {
        let resp = once(&modern(1, "server/discover", json!({})));
        assert_eq!(resp["result"]["instructions"], json!(INSTRUCTIONS));
    }

    #[test]
    fn a_legacy_version_in_meta_is_served_as_a_legacy_request() {
        let mut req = modern(1, "tools/list", json!({}));
        req.params["_meta"][META_PROTOCOL_VERSION] = json!("2025-06-18");
        assert!(once(&req)["result"].get("resultType").is_none());
    }

    #[test]
    fn refactor_without_an_action_is_invalid_params_listing_the_actions() {
        let resp = once(&legacy(
            1,
            "tools/call",
            json!({ "name": "brain_refactor", "arguments": { "id": "entities/x" } }),
        ));
        assert_eq!(
            (
                resp["error"]["code"].clone(),
                resp["error"]["message"].clone()
            ),
            (
                json!(-32602),
                json!("brain_refactor: missing 'action' (one of: rename, merge, delete)")
            )
        );
    }

    #[test]
    fn an_unknown_history_action_is_invalid_params() {
        let resp = once(&legacy(
            1,
            "tools/call",
            json!({ "name": "brain_history", "arguments": { "action": "undo", "id": "entities/x" } }),
        ));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn a_non_string_prompt_argument_is_invalid_params() {
        let resp = once(&legacy(
            1,
            "prompts/get",
            json!({ "name": "dream", "arguments": { "max_changes": 3 } }),
        ));
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[test]
    fn the_dream_prompt_confirms_summaries_only_through_write_page() {
        let text = prompt_text("dream", json!({}));
        assert!(
            !text.contains("brain_patch_page with unchanged content"),
            "{text}"
        );
    }

    // ---- second fix round -----------------------------------------------

    #[test]
    fn every_prompt_line_that_rewrites_a_page_asks_for_a_detailed_read_first() {
        // A concise read has no frontmatter; rewriting from it drops
        // aliases, sources, superseded_by, validity … (review blocker).
        let texts = [
            prompt_text("ingest", json!({ "source": "x" })),
            prompt_text("lint-session", json!({})),
            prompt_text("dream", json!({})),
        ];
        let unsafe_lines: Vec<String> = texts
            .iter()
            .flat_map(|t| t.lines())
            .filter(|l| {
                l.contains("brain_write_page") && !l.contains("response_format \"detailed\"")
            })
            .map(str::to_string)
            .collect();
        assert!(unsafe_lines.is_empty(), "{unsafe_lines:#?}");
    }

    #[test]
    fn the_write_page_description_asks_for_a_detailed_read_before_a_rewrite() {
        let spec = tool_specs()
            .into_iter()
            .find(|t| t.name == "brain_write_page")
            .unwrap();
        assert!(
            spec.description.contains("response_format \"detailed\""),
            "{}",
            spec.description
        );
    }

    #[test]
    fn a_stale_vault_agents_md_is_replaced_by_the_bundled_one_with_a_notice() {
        let tmp = vault_with_marker();
        std::fs::write(
            crate::vault::layout::meta_dir(tmp.path()).join("AGENTS.md"),
            "| `brain_get_page` | Read one page by id |",
        )
        .unwrap();
        let text = resource_text(&read_resource("brain://agents-md", Some(tmp.path())));
        assert_eq!(
            text,
            format!(
                "{STALE_AGENTS_NOTICE}{}",
                crate::onboarding::template::AGENTS_MD
            )
        );
    }

    #[test]
    fn old_names_inside_the_renamed_tools_paragraph_do_not_make_agents_md_stale() {
        let text = "Use `brain_get_pages`.\n\n**Renamed tools.** Older instructions may name \
                    `brain_get_page`.\n\nMore text.";
        assert!(!names_removed_tools(text));
    }

    #[test]
    fn the_dream_queue_resource_without_a_stored_queue_is_resource_not_found() {
        let tmp = vault_with_marker();
        let resp = read_resource("brain://dream-queue", Some(tmp.path()));
        assert_eq!(resp["error"]["code"], json!(-32002));
    }

    #[test]
    fn reading_the_dream_queue_resource_creates_neither_an_index_nor_a_queue_file() {
        let tmp = vault_with_marker();
        let _ = read_resource("brain://dream-queue", Some(tmp.path()));
        let db_file = crate::vault::layout::db_dir(tmp.path()).join(crate::db::DB_FILENAME);
        assert_eq!(
            (
                db_file.exists(),
                crate::wiki::dream::dream_queue_path(tmp.path()).exists()
            ),
            (false, false)
        );
    }
}

/// Slice D — response_format, brain_lookup by name, action
/// discriminators, through the tool dispatcher.
#[cfg(test)]
mod slice_d_tests {
    use super::*;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use tempfile::TempDir;

    fn vault() -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let marker = crate::vault::marker::VaultMarker::new("test");
        crate::vault::marker::write_marker(tmp.path(), &marker).unwrap();
        tmp
    }

    fn put(vault: &std::path::Path, id: &str, extra: &str, body: &str) {
        let (sub, slug) = id.split_once('/').unwrap();
        let kind = match sub {
            "concepts" => "concept",
            "sources" => "source",
            "topics" => "topic",
            _ => "entity",
        };
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\nid: {id}\ntype: {kind}\ntitle: {slug}\n{extra}---\n\n{body}\n"),
        )
        .unwrap();
    }

    fn indexed(vault: &std::path::Path) -> Option<crate::db::DbHandle> {
        let db = crate::db::DbHandle::open(vault).unwrap();
        crate::db::pages_index::rebuild(&db, vault).unwrap();
        Some(db)
    }

    fn call(
        vault: &std::path::Path,
        db: &mut Option<crate::db::DbHandle>,
        name: &str,
        args: Value,
    ) -> String {
        call_tool(&json!({ "name": name, "arguments": args }), vault, db)
            .unwrap_or_else(|e| panic!("{name} failed: {e}"))
    }

    fn call_json(
        vault: &std::path::Path,
        db: &mut Option<crate::db::DbHandle>,
        name: &str,
        args: Value,
    ) -> Value {
        serde_json::from_str(&call(vault, db, name, args)).unwrap()
    }

    /// Twelve concept pages that all mention "zebrafish", indexed.
    fn zebrafish_vault() -> (TempDir, Option<crate::db::DbHandle>) {
        let tmp = vault();
        for i in 0..12 {
            put(
                tmp.path(),
                &format!("concepts/zebrafish-{i:02}"),
                "summary: A zebrafish study.\n",
                "The zebrafish genome and its regulation in developmental biology.",
            );
        }
        let db = indexed(tmp.path());
        (tmp, db)
    }

    #[test]
    fn concise_search_for_ten_hits_stays_under_1500_characters() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        assert!(out.len() < 1500, "{} chars: {out}", out.len());
    }

    #[test]
    fn concise_search_returns_ten_hits_by_default() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        assert_eq!(out["hits"].as_array().map(Vec::len), Some(10));
    }

    #[test]
    fn detailed_search_carries_snippets() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish", "response_format": "detailed" }),
        );
        assert!(out["hits"][0]["snippet"].is_string(), "{out}");
    }

    #[test]
    fn concise_search_is_smaller_than_detailed_search() {
        let (tmp, mut db) = zebrafish_vault();
        let concise = call(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        let detailed = call(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish", "response_format": "detailed" }),
        );
        assert!(
            concise.len() < detailed.len(),
            "{} vs {}",
            concise.len(),
            detailed.len()
        );
    }

    #[test]
    fn search_honours_the_limit() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish", "limit": 3 }),
        );
        assert_eq!(out["hits"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn search_rejects_an_unknown_response_format() {
        let (tmp, mut db) = zebrafish_vault();
        let err = call_tool(
            &json!({ "name": "brain_search", "arguments": { "query": "x", "response_format": "full" } }),
            tmp.path(),
            &mut db,
        )
        .unwrap_err();
        assert!(err.contains("'response_format' must be"), "{err}");
    }

    #[test]
    fn concise_get_pages_carries_the_summary_but_no_frontmatter() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["concepts/zebrafish-01"] }),
        );
        let page = &out["pages"][0]["page"];
        assert_eq!(
            (page["summary"].clone(), page.get("frontmatter").is_some()),
            (json!("A zebrafish study."), false)
        );
    }

    #[test]
    fn detailed_get_pages_carries_the_frontmatter() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ["concepts/zebrafish-01"], "response_format": "detailed" }),
        );
        assert!(out["pages"][0]["page"]["frontmatter"].is_string(), "{out}");
    }

    #[test]
    fn concise_get_pages_is_smaller_than_detailed_get_pages() {
        let (tmp, mut db) = zebrafish_vault();
        let ids = json!(["concepts/zebrafish-01", "concepts/zebrafish-02"]);
        let concise = call(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ids }),
        );
        let detailed = call(
            tmp.path(),
            &mut db,
            "brain_get_pages",
            json!({ "ids": ids, "response_format": "detailed" }),
        );
        assert!(
            concise.len() < detailed.len(),
            "{} vs {}",
            concise.len(),
            detailed.len()
        );
    }

    #[test]
    fn concise_get_pages_with_context_lists_backlinks_as_ids() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        put(tmp.path(), "entities/bob", "", "Knows [[entities/alice]].");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_get_pages",
            json!({ "ids": ["entities/alice"], "include_context": true }),
        );
        assert_eq!(out["pages"][0]["backlinks"], json!(["entities/bob"]));
    }

    #[test]
    fn detailed_get_pages_with_context_lists_backlinks_with_titles() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        put(tmp.path(), "entities/bob", "", "Knows [[entities/alice]].");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_get_pages",
            json!({ "ids": ["entities/alice"], "include_context": true, "response_format": "detailed" }),
        );
        assert_eq!(out["pages"][0]["backlinks"][0]["title"], json!("bob"));
    }

    #[test]
    fn get_pages_without_include_context_carries_no_backlinks() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_get_pages",
            json!({ "ids": ["entities/alice"] }),
        );
        assert!(out["pages"][0].get("backlinks").is_none(), "{out}");
    }

    /// A vault with one broken link (error) and two pages without summary
    /// (warnings).
    fn lint_vault() -> TempDir {
        let tmp = vault();
        put(
            tmp.path(),
            "entities/alice",
            "",
            "Links [[entities/ghost]].",
        );
        put(tmp.path(), "entities/bob", "", "Body.");
        tmp
    }

    #[test]
    fn concise_lint_report_counts_warnings_per_kind_instead_of_listing_them() {
        let tmp = lint_vault();
        let out = call_json(tmp.path(), &mut None, "brain_lint_report", json!({}));
        assert_eq!(
            (
                out.get("warnings").is_some(),
                out["warning_kinds"]["missing-summary"].clone()
            ),
            (false, json!(2))
        );
    }

    #[test]
    fn concise_lint_report_still_lists_every_error() {
        let tmp = lint_vault();
        let out = call_json(tmp.path(), &mut None, "brain_lint_report", json!({}));
        assert_eq!(out["errors"][0]["kind"], json!("broken-link"));
    }

    #[test]
    fn detailed_lint_report_lists_the_warnings() {
        let tmp = lint_vault();
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_lint_report",
            json!({ "response_format": "detailed" }),
        );
        assert!(
            out["warnings"].as_array().is_some_and(|w| !w.is_empty()),
            "{out}"
        );
    }

    #[test]
    fn lint_report_with_a_kind_keeps_only_that_kind() {
        let tmp = lint_vault();
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_lint_report",
            json!({ "kind": "missing-summary", "response_format": "detailed" }),
        );
        let kinds: std::collections::BTreeSet<&str> = out["errors"]
            .as_array()
            .unwrap()
            .iter()
            .chain(out["warnings"].as_array().unwrap())
            .map(|f| f["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds.into_iter().collect::<Vec<_>>(),
            vec!["missing-summary"]
        );
    }

    #[test]
    fn concise_graph_lists_edges_as_id_pairs() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Knows [[entities/bob]].");
        put(tmp.path(), "entities/bob", "", "Body.");
        let out = call_json(tmp.path(), &mut None, "brain_graph", json!({}));
        assert_eq!(out["edges"], json!([["entities/alice", "entities/bob"]]));
    }

    #[test]
    fn detailed_graph_lists_nodes_with_their_type() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_graph",
            json!({ "response_format": "detailed" }),
        );
        assert_eq!(out["nodes"][0]["type"], json!("entity"));
    }

    #[test]
    fn lookup_by_a_bare_name_finds_a_normalised_match_in_any_type() {
        let tmp = vault();
        put(tmp.path(), "concepts/mueller-gmbh", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "Müller GmbH" }),
        );
        assert_eq!(out["matches"][0]["id"], json!("concepts/mueller-gmbh"));
    }

    #[test]
    fn lookup_by_a_bare_slug_reports_the_existing_page_as_exact() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "alice" }),
        );
        assert_eq!(
            (out["exists"].clone(), out["matches"][0]["reason"].clone()),
            (json!(true), json!("exact"))
        );
    }

    #[test]
    fn lookup_never_returns_page_bodies() {
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "SECRET-BODY-TEXT");
        let out = call(
            tmp.path(),
            &mut indexed(tmp.path()),
            "brain_lookup",
            json!({ "query_or_id": "alice" }),
        );
        assert!(!out.contains("SECRET-BODY-TEXT"), "{out}");
    }

    #[test]
    fn brain_eval_without_an_action_runs_the_eval() {
        let tmp = vault();
        let err = call_tool(
            &json!({ "name": "brain_eval", "arguments": {} }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("the eval set is empty"), "{err}");
    }

    #[test]
    fn brain_history_without_an_action_lists_the_commits() {
        let tmp = vault();
        crate::wiki::git::init_repo(&wiki_dir(tmp.path())).unwrap();
        put(tmp.path(), "entities/alice", "", "Body.");
        crate::wiki::git::commit_all(&wiki_dir(tmp.path()), "alice").unwrap();
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_history",
            json!({ "id": "entities/alice" }),
        );
        assert_eq!(out["commits"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn refactor_results_leave_out_absent_fields_instead_of_null() {
        let tmp = vault();
        put(tmp.path(), "entities/old", "", "Body.");
        // No git repo: the rename lands on disk, the commit cannot be made.
        let out = call(
            tmp.path(),
            &mut None,
            "brain_refactor",
            json!({ "action": "rename", "id": "entities/old", "new_id": "entities/new" }),
        );
        assert!(!out.contains("null"), "{out}");
    }

    #[test]
    fn refactor_results_echo_the_action() {
        let tmp = vault();
        put(tmp.path(), "entities/junk", "", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_refactor",
            json!({ "action": "delete", "id": "entities/junk" }),
        );
        assert_eq!(out["action"], json!("delete"));
    }

    #[test]
    fn plain_snippets_drop_highlight_markers() {
        assert_eq!(
            plain_snippet("the «zebrafish» genome", 80),
            "the zebrafish genome"
        );
    }

    #[test]
    fn plain_snippets_are_cut_at_a_word_boundary_within_the_limit() {
        let snippet = plain_snippet(&"word ".repeat(40), 80);
        assert_eq!(
            (snippet.chars().count() <= 80, snippet.ends_with("word…")),
            (true, true)
        );
    }

    #[test]
    fn concise_search_snippets_prefer_the_page_summary() {
        let (tmp, mut db) = zebrafish_vault();
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        assert_eq!(out["hits"][0]["snippet"], json!("A zebrafish study."));
    }

    #[test]
    fn brain_dream_stats_reports_the_outcomes_of_the_logged_items() {
        let tmp = vault();
        put(tmp.path(), "entities/old", "", "Old.");
        put(tmp.path(), "entities/alice", "", "Alice.");
        call(
            tmp.path(),
            &mut None,
            "brain_dream",
            json!({ "action": "log", "entry": "s1", "items": [
                { "kind": "orphan", "pages": ["entities/old"], "outcome": "skipped", "note": "unsure" },
                { "kind": "orphan", "pages": ["entities/old"], "outcome": "skipped", "note": "still unsure" },
                { "kind": "summary-stale", "pages": ["entities/alice"], "outcome": "done" }
            ] }),
        );
        let stats = call_json(
            tmp.path(),
            &mut None,
            "brain_dream",
            json!({ "action": "stats" }),
        );
        assert_eq!(
            stats,
            json!({
                "sessions": 1,
                "items_total": 3,
                "per_kind": [
                    { "kind": "orphan", "done": 0, "skipped": 2, "deferred": 0 },
                    { "kind": "summary-stale", "done": 1, "skipped": 0, "deferred": 0 }
                ],
                "most_skipped": [ { "kind": "orphan", "pages": ["entities/old"], "count": 2 } ]
            })
        );
    }

    #[test]
    fn brain_dream_with_an_unknown_action_is_refused() {
        let tmp = vault();
        let err = call_tool(
            &json!({ "name": "brain_dream", "arguments": { "action": "sleep" } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("unknown action 'sleep'"), "{err}");
    }

    #[test]
    fn brain_write_raw_file_answers_with_the_written_path_as_json() {
        let tmp = vault();
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_write_raw_file",
            json!({ "connector": "notes", "relative_path": "a.txt", "content": "x" }),
        );
        assert_eq!(out["wrote"], json!("01_raw/notes/a.txt"));
    }

    #[test]
    fn every_successful_json_tool_result_is_an_object_for_structured_content() {
        // structuredContent must be an object (2025-06-18); every tool that
        // declares an outputSchema must therefore answer with one.
        let tmp = vault();
        put(tmp.path(), "entities/alice", "", "Body.");
        let mut db = indexed(tmp.path());
        let calls = [
            ("brain_search", json!({ "query": "Body" })),
            ("brain_search", json!({ "query": "" })),
            ("brain_lookup", json!({ "query_or_id": "alice" })),
            ("brain_get_pages", json!({ "ids": ["entities/alice"] })),
            (
                "brain_get_pages",
                json!({ "ids": ["entities/alice"], "include_context": true }),
            ),
            ("brain_query", json!({})),
            ("brain_query", json!({ "facet": "tags" })),
            ("brain_graph", json!({})),
            ("brain_lint_report", json!({})),
            ("brain_dream", json!({ "action": "queue" })),
        ];
        let not_objects: Vec<&str> = calls
            .iter()
            .filter(|(name, args)| {
                !serde_json::from_str::<Value>(&call(tmp.path(), &mut db, name, args.clone()))
                    .is_ok_and(|v| v.is_object())
            })
            .map(|(name, _)| *name)
            .collect();
        assert!(not_objects.is_empty(), "{not_objects:?}");
    }

    /// Whether `value` has the JSON Schema `type` named by `ty`.
    fn has_type(value: &Value, ty: &str) -> bool {
        match ty {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        }
    }

    #[test]
    fn every_tool_result_matches_the_types_its_output_schema_declares() {
        // A client validates structuredContent against outputSchema and
        // rejects the call on a mismatch — so every declared field type
        // must hold for real results (concise and detailed alike).
        let tmp = vault();
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        put(
            tmp.path(),
            "entities/alice",
            "summary: Alice.
tags: [team]
",
            "Knows [[entities/bob]].",
        );
        put(tmp.path(), "entities/bob", "", "Body.");
        put(tmp.path(), "entities/junk", "", "Junk.");
        put(tmp.path(), "entities/dup", "", "Dup.");
        put(tmp.path(), "entities/typo", "", "Typo.");
        let sha = crate::wiki::git::commit_all(&wiki, "baseline")
            .unwrap()
            .unwrap();
        let mut db = indexed(tmp.path());
        let page = "---
id: entities/carol
type: entity
title: Carol
summary: Carol.
---

Body.
";
        let calls: Vec<(&str, Value)> = vec![
            ("brain_search", json!({ "query": "Body" })),
            (
                "brain_search",
                json!({ "query": "Body", "response_format": "detailed" }),
            ),
            ("brain_lookup", json!({ "query_or_id": "entities/alice" })),
            ("brain_lookup", json!({ "query_or_id": "alice" })),
            (
                "brain_get_pages",
                json!({ "ids": ["entities/alice", "entities/nope"], "response_format": "detailed" }),
            ),
            (
                "brain_get_pages",
                json!({ "ids": ["entities/alice"], "include_context": true }),
            ),
            ("brain_query", json!({ "query": "*", "limit": 1 })),
            ("brain_query", json!({ "facet": "tags" })),
            ("brain_graph", json!({})),
            ("brain_graph", json!({ "response_format": "detailed" })),
            (
                "brain_write_page",
                json!({ "id": "entities/carol", "content": page, "confirm_summary": true }),
            ),
            (
                "brain_write_batch",
                json!({ "pages": [{ "id": "entities/dave", "content": page.replace("carol", "dave").replace("Carol", "Dave") }] }),
            ),
            (
                "brain_patch_page",
                json!({ "id": "entities/bob", "heading": "## Notes", "content": "x", "confirm_summary": true }),
            ),
            ("brain_lint_report", json!({})),
            (
                "brain_lint_report",
                json!({ "response_format": "detailed" }),
            ),
            (
                "brain_history",
                json!({ "action": "list", "id": "entities/alice" }),
            ),
            (
                "brain_history",
                json!({ "action": "restore", "id": "entities/bob", "sha": sha }),
            ),
            (
                "brain_refactor",
                json!({ "action": "rename", "id": "entities/typo", "new_id": "entities/fixed" }),
            ),
            (
                "brain_refactor",
                json!({ "action": "merge", "from_id": "entities/dup", "into_id": "entities/bob" }),
            ),
            (
                "brain_refactor",
                json!({ "action": "delete", "id": "entities/junk" }),
            ),
            (
                "brain_write_raw_file",
                json!({ "connector": "notes", "relative_path": "a.txt", "content": "x" }),
            ),
            (
                "brain_eval",
                json!({ "action": "add", "query": "Knows", "expected": ["entities/alice"] }),
            ),
            ("brain_eval", json!({ "action": "run" })),
            ("brain_dream", json!({ "action": "queue" })),
            (
                "brain_dream",
                json!({ "action": "log", "entry": "nothing" }),
            ),
        ];
        let specs = tool_specs();
        let mut mismatches: Vec<String> = Vec::new();
        for (name, args) in calls {
            let result: Value =
                serde_json::from_str(&call(tmp.path(), &mut db, name, args)).unwrap();
            assert!(result.is_object(), "{name}");
            let spec = specs.iter().find(|s| s.name == name).unwrap();
            for (field, schema) in spec.output["properties"].as_object().unwrap() {
                let (Some(value), Some(ty)) = (result.get(field), schema["type"].as_str()) else {
                    continue;
                };
                if !has_type(value, ty) {
                    mismatches.push(format!("{name}.{field}: expected {ty}, got {value}"));
                }
            }
        }
        let ping: Value = serde_json::from_str(&brain_ping_detail(Some(tmp.path()), &db)).unwrap();
        let spec = specs.iter().find(|s| s.name == "brain_ping").unwrap();
        for (field, schema) in spec.output["properties"].as_object().unwrap() {
            if let (Some(value), Some(ty)) = (ping.get(field), schema["type"].as_str()) {
                if !has_type(value, ty) {
                    mismatches.push(format!("brain_ping.{field}: expected {ty}, got {value}"));
                }
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    // ---- second fix round -----------------------------------------------

    #[test]
    fn patch_page_rejects_an_empty_heading() {
        let tmp = vault();
        put(
            tmp.path(),
            "entities/alice",
            "",
            "Intro.\n\n## Kontakt\n\nalt",
        );
        let err = call_tool(
            &json!({ "name": "brain_patch_page", "arguments": { "id": "entities/alice", "heading": "", "content": "x" } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(
            err.contains("'heading' must be a markdown heading line"),
            "{err}"
        );
    }

    #[test]
    fn patch_page_with_a_heading_without_hashes_writes_nothing() {
        let tmp = vault();
        put(
            tmp.path(),
            "entities/alice",
            "",
            "Intro.\n\n## Kontakt\n\nalt",
        );
        let file = wiki_dir(tmp.path()).join("entities/alice.md");
        let before = std::fs::read_to_string(&file).unwrap();
        let _ = call_tool(
            &json!({ "name": "brain_patch_page", "arguments": { "id": "entities/alice", "heading": "Kontakt", "content": "x" } }),
            tmp.path(),
            &mut None,
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn write_page_refuses_a_file_that_belongs_to_a_case_only_different_id() {
        // What a case-insensitive file system does to `entities/ACME`
        // next to `entities/acme`, made explicit: the file at the target
        // path carries the same id in a different case.
        let tmp = vault();
        let file = wiki_dir(tmp.path()).join("entities/acme.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "---\nid: entities/ACME\ntype: entity\ntitle: A\n---\n\nBody.\n",
        )
        .unwrap();
        let err = call_tool(
            &json!({ "name": "brain_write_page", "arguments": {
                "id": "entities/acme",
                "content": "---\nid: entities/acme\ntype: entity\ntitle: A\n---\n\nNew.\n"
            } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(
            err.contains("already holds the page 'entities/ACME'"),
            "{err}"
        );
    }

    #[test]
    fn write_page_still_repairs_a_file_whose_id_disagrees_with_its_path() {
        // A hand-edited or copied file (Obsidian, VS Code) whose
        // frontmatter id does not match its file name is not a case
        // collision: writing the correct page over it is the repair path.
        let tmp = vault();
        let file = wiki_dir(tmp.path()).join("entities/acme.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "---\nid: entities/acme-corp\ntype: entity\ntitle: A\n---\n\nBody.\n",
        )
        .unwrap();
        call(
            tmp.path(),
            &mut None,
            "brain_write_page",
            json!({
                "id": "entities/acme",
                "content": "---\nid: entities/acme\ntype: entity\ntitle: A\n---\n\nRepaired.\n"
            }),
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("id: entities/acme\n") && text.contains("Repaired."),
            "{text}"
        );
    }

    #[test]
    fn write_batch_refuses_an_entry_whose_file_belongs_to_a_case_only_different_id() {
        let tmp = vault();
        let file = wiki_dir(tmp.path()).join("entities/acme.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "---\nid: entities/ACME\ntype: entity\ntitle: A\n---\n\nBody.\n",
        )
        .unwrap();
        let err = call_tool(
            &json!({ "name": "brain_write_batch", "arguments": { "pages": [{
                "id": "entities/acme",
                "content": "---\nid: entities/acme\ntype: entity\ntitle: A\n---\n\nNew.\n"
            }] } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(
            err.starts_with("pages[0] (entities/acme): the file for"),
            "{err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn write_page_with_a_case_only_different_id_leaves_the_existing_page_untouched() {
        let tmp = vault();
        put(tmp.path(), "entities/acme", "", "Original.");
        let file = wiki_dir(tmp.path()).join("entities/acme.md");
        let _ = call_tool(
            &json!({ "name": "brain_write_page", "arguments": {
                "id": "entities/ACME",
                "content": "---\nid: entities/ACME\ntype: entity\ntitle: A\n---\n\nNew.\n"
            } }),
            tmp.path(),
            &mut None,
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("id: entities/acme\n")
        );
    }

    #[test]
    fn history_restore_accepts_a_seven_character_sha_prefix() {
        let tmp = vault();
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        put(tmp.path(), "entities/alice", "", "Version one.");
        let v1 = crate::wiki::git::commit_all(&wiki, "v1").unwrap().unwrap();
        put(tmp.path(), "entities/alice", "", "Version two.");
        crate::wiki::git::commit_all(&wiki, "v2").unwrap();
        call(
            tmp.path(),
            &mut None,
            "brain_history",
            json!({ "action": "restore", "id": "entities/alice", "sha": &v1[..7] }),
        );
        let text = std::fs::read_to_string(wiki.join("entities/alice.md")).unwrap();
        assert!(text.contains("Version one."), "{text}");
    }

    #[test]
    fn history_restore_refuses_a_revision_expression() {
        let tmp = vault();
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        put(tmp.path(), "entities/alice", "", "Body.");
        crate::wiki::git::commit_all(&wiki, "v1").unwrap();
        let err = call_tool(
            &json!({ "name": "brain_history", "arguments": { "action": "restore", "id": "entities/alice", "sha": "HEAD~1" } }),
            tmp.path(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("is not a commit sha"), "{err}");
    }

    /// Ten indexed pages with realistic lengths: 35-character ids,
    /// 30-character titles and long summaries.
    fn realistic_vault() -> (TempDir, Option<crate::db::DbHandle>) {
        let tmp = vault();
        for i in 0..10 {
            let id = format!("entities/customer-zebrafish-labs-{i:02}");
            assert_eq!(id.len(), 35);
            let dir = wiki_dir(tmp.path()).join("entities");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{}.md", id.trim_start_matches("entities/"))),
                format!(
                    "---\nid: {id}\ntype: entity\ntitle: Zebrafish Labs Customer No {i:02}\n\
                     summary: Zebrafish Labs is a long-standing customer whose contract renews every \
                     twelve months with a three-month notice period.\n---\n\nZebrafish body.\n"
                ),
            )
            .unwrap();
        }
        let db = indexed(tmp.path());
        (tmp, db)
    }

    #[test]
    fn concise_search_for_ten_realistic_hits_stays_within_the_budget() {
        let (tmp, mut db) = realistic_vault();
        let out = call(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        assert!(
            out.len() <= CONCISE_SEARCH_BUDGET,
            "{} chars: {out}",
            out.len()
        );
    }

    #[test]
    fn concise_search_titles_are_cut_at_sixty_characters() {
        let tmp = vault();
        let title = "A very long page title that keeps going well past sixty characters in total";
        put(tmp.path(), "entities/long", "summary: Long.\n", "zebrafish");
        let file = wiki_dir(tmp.path()).join("entities/long.md");
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("title: long", &format!("title: {title}"));
        std::fs::write(&file, text).unwrap();
        let mut db = indexed(tmp.path());
        let out = call_json(
            tmp.path(),
            &mut db,
            "brain_search",
            json!({ "query": "zebrafish" }),
        );
        let cut = out["hits"][0]["title"].as_str().unwrap();
        assert!(
            cut.chars().count() <= 60
                && !cut.is_empty()
                && title.starts_with(cut.trim_end_matches('…').trim_end()),
            "{out}"
        );
    }

    // ---- dream log items and keep (third round) ------------------------------

    fn dream_log(tmp: &TempDir, items: Value) -> Result<String, String> {
        call_tool(
            &json!({ "name": "brain_dream", "arguments": { "action": "log", "entry": "session", "items": items } }),
            tmp.path(),
            &mut None,
        )
    }

    #[test]
    fn dream_log_reports_how_many_items_it_logged() {
        let tmp = vault();
        let out: Value = serde_json::from_str(
            &dream_log(
                &tmp,
                json!([
                    { "kind": "orphan", "pages": ["entities/x"], "outcome": "skipped", "note": "still useful" },
                    { "kind": "summary-stale", "pages": ["entities/y"], "outcome": "done" }
                ]),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(out["items_logged"], json!(2));
    }

    #[test]
    fn dream_log_refuses_an_unknown_outcome() {
        let tmp = vault();
        let err = dream_log(
            &tmp,
            json!([{ "kind": "orphan", "pages": ["entities/x"], "outcome": "maybe" }]),
        )
        .unwrap_err();
        assert!(err.contains("items[0]: 'outcome' must be"), "{err}");
    }

    #[test]
    fn dream_log_refuses_an_invalid_page_id() {
        let tmp = vault();
        let err = dream_log(
            &tmp,
            json!([{ "kind": "orphan", "pages": ["../etc/passwd"], "outcome": "done" }]),
        )
        .unwrap_err();
        assert!(err.starts_with("items[0]:"), "{err}");
    }

    #[test]
    fn a_refused_dream_log_writes_nothing() {
        let tmp = vault();
        let _ = dream_log(
            &tmp,
            json!([{ "kind": "", "pages": ["entities/x"], "outcome": "done" }]),
        );
        assert!(!crate::wiki::dream::dream_log_path(tmp.path()).exists());
    }

    #[test]
    fn a_queue_after_two_logged_skips_carries_skipped_before_two() {
        let tmp = vault();
        put(tmp.path(), "entities/a", "", "Links [[entities/missing]].");
        let mut db = indexed(tmp.path());
        for _ in 0..2 {
            dream_log(
                &tmp,
                json!([{ "kind": "broken-link", "pages": ["entities/a"], "outcome": "skipped", "note": "ask the user" }]),
            )
            .unwrap();
        }
        let queue = call_json(
            tmp.path(),
            &mut db,
            "brain_dream",
            json!({ "action": "queue", "refresh": true }),
        );
        let item = queue["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["kind"] == json!("broken-link"))
            .cloned()
            .unwrap();
        assert_eq!(item["skipped_before"], json!(2));
    }

    #[test]
    fn write_page_keeps_a_keep_mark_in_the_written_file() {
        let tmp = vault();
        call(
            tmp.path(),
            &mut None,
            "brain_write_page",
            json!({
                "id": "entities/kept",
                "content": "---\nid: entities/kept\ntype: entity\ntitle: Kept\nkeep: true\n---\n\nBody.\n"
            }),
        );
        let text = std::fs::read_to_string(wiki_dir(tmp.path()).join("entities/kept.md")).unwrap();
        assert!(text.contains("keep: true\n"), "{text}");
    }

    #[test]
    fn the_detailed_read_of_a_kept_page_shows_the_keep_mark() {
        let tmp = vault();
        put(tmp.path(), "entities/kept", "keep: true\n", "Body.");
        let out = call_json(
            tmp.path(),
            &mut None,
            "brain_get_pages",
            json!({ "ids": ["entities/kept"], "response_format": "detailed" }),
        );
        let fm: Value =
            serde_json::from_str(out["pages"][0]["page"]["frontmatter"].as_str().unwrap()).unwrap();
        assert_eq!(fm["keep"], json!(true));
    }

    #[test]
    fn a_dream_log_with_items_drops_the_stored_queue() {
        let tmp = vault();
        let stored = crate::wiki::dream::DreamQueue {
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: Vec::new(),
            omitted: 0,
            notes: Vec::new(),
        };
        crate::wiki::dream::write_dream_queue(tmp.path(), &stored).unwrap();
        dream_log(
            &tmp,
            json!([{ "kind": "orphan", "pages": ["entities/x"], "outcome": "skipped", "note": "later" }]),
        )
        .unwrap();
        assert!(!crate::wiki::dream::dream_queue_path(tmp.path()).exists());
    }

    #[test]
    fn dream_log_refuses_a_page_id_with_a_backtick() {
        let tmp = vault();
        let err = dream_log(
            &tmp,
            json!([{ "kind": "orphan", "pages": ["entities/a`b"], "outcome": "done" }]),
        )
        .unwrap_err();
        assert!(err.contains("backtick"), "{err}");
    }
}

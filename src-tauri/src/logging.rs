//! Structured logging via `tracing`.
//!
//! Cross-platform behaviour:
//! - Writers are `std::io::stdout` / `std::io::stderr`, both of which are
//!   well-defined on macOS, Linux and Windows. No platform-conditional
//!   plumbing is required.
//! - `with_ansi(false)` is set for the MCP variant so terminal escape
//!   sequences don't end up in Claude Desktop's log file (where they'd
//!   render as `\u{1b}[2m…` clutter).

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn default_filter() -> EnvFilter {
    // Default filter: noisy plugins are silenced unless RUST_LOG overrides.
    // - `tauri_plugin_updater=off`: the placeholder GitHub release endpoint
    //   responds 404 in dev, the plugin logs that as ERROR on every check;
    //   spam outweighs signal until a real endpoint is wired up.
    // - `globset` and `notify_debouncer_full` are also chatty at INFO level.
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,brain_lib=debug,tauri_plugin_updater=off,\
             notify_debouncer_full=warn,globset=warn",
        )
    })
}

/// Tray / GUI process. Writes to the controlling terminal's stdout when
/// run from a shell, otherwise gets dropped on the floor — Tauri 2 on
/// Windows runs as `windows_subsystem = "windows"` so there is no console
/// to attach to in production. For diagnostics, run BRAIN.exe from
/// PowerShell.
pub fn init() {
    let layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false);

    let _ = tracing_subscriber::registry()
        .with(default_filter())
        .with(layer)
        .try_init();
}

/// MCP subprocess. **Must** write to stderr only — stdout is the JSON-RPC
/// transport and any extra bytes there corrupt the frame Claude Desktop
/// is parsing (we previously hit "Unexpected end of JSON input" because
/// nothing was logging anywhere, and once we add logs we want to make
/// absolutely sure none of them slip onto the protocol channel). Claude
/// Desktop captures stderr from the subprocess and surfaces it in
/// `mcp-server-<name>.log`, so this is what shows up when you debug.
pub fn init_for_mcp() {
    let layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false)
        // No ANSI escapes — they render as garbage in Claude Desktop's
        // log file.
        .with_ansi(false);

    let _ = tracing_subscriber::registry()
        .with(default_filter())
        .with(layer)
        .try_init();
}

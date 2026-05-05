//! Structured logging via `tracing`.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init() {
    // Default filter: noisy plugins are silenced unless RUST_LOG overrides.
    // - `tauri_plugin_updater=off`: the placeholder GitHub release endpoint
    //   responds 404 in dev, the plugin logs that as ERROR on every check;
    //   spam outweighs signal until a real endpoint is wired up.
    // - `globset` and `notify_debouncer_full` are also chatty at INFO level.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,brain_lib=debug,tauri_plugin_updater=off,\
             notify_debouncer_full=warn,globset=warn",
        )
    });

    let layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
}

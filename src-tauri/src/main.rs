// Prevent additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "mcp") {
        // Ensure stdout is line-buffered for stdio JSON-RPC.
        let _ = brain_lib::run_mcp_stdio();
        return;
    }
    brain_lib::run();
}

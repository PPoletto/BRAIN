// Prevent additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // `brain git-filter clean|smudge` — invoked by git as the
    // brain-crypt clean/smudge filter (configured per encrypted vault).
    // Reads a blob on stdin, writes the transformed blob on stdout.
    // Handled before anything else and exits with git's expected code.
    if args.get(1).map(String::as_str) == Some("git-filter") {
        std::process::exit(brain_lib::run_git_filter(args.get(2).map(String::as_str)));
    }
    // `brain convert <vault-path>` — enable content encryption on an
    // existing vault (S11 phase 5).
    if args.get(1).map(String::as_str) == Some("convert") {
        std::process::exit(brain_lib::run_convert(args.get(2).map(String::as_str)));
    }
    // `brain remote-add <vault> <url>` / `brain sync <vault>` — S11 phase 6
    // remote sync plumbing (also drivable from the app).
    if args.get(1).map(String::as_str) == Some("remote-add") {
        std::process::exit(brain_lib::run_remote_add(
            args.get(2).map(String::as_str),
            args.get(3).map(String::as_str),
        ));
    }
    if args.get(1).map(String::as_str) == Some("sync") {
        std::process::exit(brain_lib::run_sync(args.get(2).map(String::as_str)));
    }
    if args.iter().any(|a| a == "mcp") {
        // Ensure stdout is line-buffered for stdio JSON-RPC.
        let _ = brain_lib::run_mcp_stdio();
        return;
    }
    brain_lib::run();
}

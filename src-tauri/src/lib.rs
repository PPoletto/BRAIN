//! Brain Client library — wires together Tauri, modules and commands.

pub mod config;
pub mod crypto;
pub mod db;
pub mod embedding;
pub mod error;
pub mod logging;
pub mod mcp;
pub mod mount;
pub mod onboarding;
pub mod proc;
pub mod state;
pub mod tray;
pub mod update;
pub mod vault;
pub mod viewer;
pub mod wiki;

use std::sync::Arc;

use crate::state::AppState;

/// Runs the MCP server in stdio mode. Used by `brain mcp` invocations
/// spawned by Claude Code, Codex, and other MCP clients. Reads the vault
/// path from the `BRAIN_VAULT_PATH` env var.
pub fn run_mcp_stdio() -> std::io::Result<()> {
    // Init logging FIRST so configure_libgit2's `tracing::warn!` and any
    // panics caught by the dispatch loop have somewhere to land —
    // otherwise the subprocess goes dark on errors, which is what made
    // the disconnect bug invisible. Writes to stderr so stdout stays a
    // pure JSON-RPC stream Claude Desktop can parse.
    logging::init_for_mcp();
    configure_libgit2();
    db::vec_loader::ensure_loaded();
    mcp::server::run_stdio()
}

/// Entry point for `brain git-filter clean|smudge` — the brain-crypt
/// git clean/smudge filter invoked by git inside an encrypted vault.
/// Returns the process exit code (0 = ok; non-zero aborts git's
/// operation so plaintext is never committed and garbage never checked
/// out). Delegates to [`crypto::gitfilter::run`].
pub fn run_git_filter(mode: Option<&str>) -> i32 {
    configure_libgit2();
    crypto::gitfilter::run(mode)
}

/// Entry point for `brain convert <vault-path>` — enables content
/// encryption on an existing vault (S11 phase 5). git-crypt-style:
/// working tree stays plaintext, committed blobs become ciphertext.
/// Content-loss-safe (working-tree files are never deleted, only
/// re-staged). Prints the recovery key for the user to save. Returns
/// a process exit code.
pub fn run_convert(vault_arg: Option<&str>) -> i32 {
    configure_libgit2();
    let Some(vault_arg) = vault_arg else {
        eprintln!("usage: brain convert <vault-path>");
        return 2;
    };
    let vault = std::path::Path::new(vault_arg);
    if !vault::layout::is_vault(vault) {
        eprintln!("convert: '{}' is not a BRAIN vault", vault.display());
        return 3;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("brain"));
    let store = crypto::keychain::KeyringStore;
    let key = match wiki::encryption::enable_encryption(vault, &store, &exe) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("convert: encryption setup failed: {e:#}");
            return 4;
        }
    };
    let keys = key.derive();
    let wiki = vault::layout::wiki_dir(vault);
    // Rename page files to opaque HMAC names so person/customer names
    // don't leak through file paths (content is already hidden by
    // encryption). Reversible — the id stays in the frontmatter.
    match wiki::encryption::rename_pages_to_opaque(&wiki, &keys) {
        Ok(n) => {
            if n > 0 {
                println!("Renamed {n} page file(s) to opaque names.");
            }
        }
        Err(e) => {
            eprintln!("convert: opaque-filename rename failed: {e:#}");
            return 5;
        }
    }
    if let Err(e) =
        wiki::encryption::commit_wiki(&wiki, "encrypt: enable content encryption + opaque filenames")
    {
        eprintln!("convert: re-encrypting the vault content failed: {e:#}");
        return 6;
    }
    println!("BRAIN vault content encryption enabled.");
    println!();
    println!("RECOVERY KEY — store this in your password manager NOW.");
    println!("Losing it makes any pushed/backed-up copy permanently unreadable:");
    println!("  {}", key.to_hex());
    println!();
    println!("The working tree stays plaintext on this machine; committed git");
    println!("blobs are now encrypted. Rebuild the index on next app launch.");
    0
}

/// Entry point for `brain remote-add <vault-path> <url>` — attach the
/// sync remote (S11 phase 6). Enforces the encryption coupling for
/// network URLs. Returns a process exit code.
pub fn run_remote_add(vault_arg: Option<&str>, url: Option<&str>) -> i32 {
    configure_libgit2();
    let (Some(vault_arg), Some(url)) = (vault_arg, url) else {
        eprintln!("usage: brain remote-add <vault-path> <url>");
        return 2;
    };
    let vault = std::path::Path::new(vault_arg);
    if !vault::layout::is_vault(vault) {
        eprintln!("remote-add: '{}' is not a BRAIN vault", vault.display());
        return 3;
    }
    match wiki::sync::set_remote(&vault::layout::wiki_dir(vault), url) {
        Ok(()) => {
            println!("remote '{}' set to {url}", wiki::sync::DEFAULT_REMOTE);
            0
        }
        Err(e) => {
            eprintln!("remote-add failed: {e}");
            4
        }
    }
}

/// Entry point for `brain remote-cred <vault-path>` — store the remote
/// credential (PAT) in the OS keychain. The PAT is read from STDIN (not a
/// CLI arg) so it never lands in shell history. Returns a process exit
/// code.
pub fn run_remote_cred(vault_arg: Option<&str>) -> i32 {
    configure_libgit2();
    let Some(vault_arg) = vault_arg else {
        eprintln!("usage: brain remote-cred <vault-path>   (PAT is read from stdin)");
        return 2;
    };
    let vault = std::path::Path::new(vault_arg);
    if !vault::layout::is_vault(vault) {
        eprintln!("remote-cred: '{}' is not a BRAIN vault", vault.display());
        return 3;
    }
    let mut pat = String::new();
    if std::io::stdin().read_line(&mut pat).is_err() {
        eprintln!("remote-cred: could not read the PAT from stdin");
        return 4;
    }
    let pat = pat.trim();
    if pat.is_empty() {
        eprintln!("remote-cred: no PAT provided on stdin");
        return 4;
    }
    match wiki::sync::set_remote_credential(&vault::layout::wiki_dir(vault), pat) {
        Ok(()) => {
            println!("remote credential stored in the OS keychain for this vault");
            0
        }
        Err(e) => {
            eprintln!("remote-cred failed: {e}");
            5
        }
    }
}

/// Entry point for `brain sync <vault-path>` — fetch → merge → push
/// (S11 phase 6). Returns a process exit code.
pub fn run_sync(vault_arg: Option<&str>) -> i32 {
    configure_libgit2();
    let Some(vault_arg) = vault_arg else {
        eprintln!("usage: brain sync <vault-path>");
        return 2;
    };
    let vault = std::path::Path::new(vault_arg);
    if !vault::layout::is_vault(vault) {
        eprintln!("sync: '{}' is not a BRAIN vault", vault.display());
        return 3;
    }
    match wiki::sync::sync(&vault::layout::wiki_dir(vault)) {
        Ok(outcome) => {
            println!("sync: {outcome:?}");
            0
        }
        Err(e) => {
            eprintln!("sync failed: {e}");
            4
        }
    }
}

/// Disables libgit2's owner-validation check. On exFAT-formatted external
/// drives the OS does not surface a meaningful Unix owner, so libgit2's
/// strict CVE-2022-24765 workaround refuses to open the wiki repo with
/// "repository path '…' is not owned by current user". Brain runs as a
/// single-user tool without the multi-user attack surface that check is
/// meant to protect against, so we opt out globally.
fn configure_libgit2() {
    // Safe: this is process-global libgit2 init state, set once before any
    // git2::Repository call. SAFETY: must run before any thread touches
    // libgit2; we call it from `run()` and from `run_mcp_stdio()` before
    // any other code uses git2.
    unsafe {
        if let Err(err) = git2::opts::set_verify_owner_validation(false) {
            tracing::warn!(?err, "could not disable libgit2 owner validation");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    configure_libgit2();
    // Must be called BEFORE the first SQLite connection — sqlite-vec
    // registers itself as a process-wide auto-extension that takes effect
    // on every new Connection::open call.
    db::vec_loader::ensure_loaded();

    let app_state = Arc::new(AppState::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::default().build())
        // `LaunchAgent` covers macOS/Linux; Windows uses the registry. The
        // empty args slice keeps BRAIN's default CLI (just the binary)
        // running at login — the tray sits in the background until the
        // user opens the window or an MCP client spawns the stdio child.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(app_state.clone())
        .setup(move |app| {
            tray::setup(app.handle().clone(), app_state.clone())?;

            // Pre-warm the disk cache off the main thread so the
            // onboarding wizard's medium picker is instant the first time
            // the user opens it. PowerShell shellout on Windows is the
            // bottleneck — paying that cost upfront, off-thread, is
            // almost always cheaper than paying it during navigation.
            let prewarm = app_state.clone();
            std::thread::spawn(move || {
                if let Ok(disks) = onboarding::disks::list_disks() {
                    prewarm.set_disk_cache(disks);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window via the OS chrome should hide it
            // rather than terminate the app — Brain stays alive in the tray.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            // Onboarding (S05)
            onboarding::commands::list_disks,
            onboarding::commands::refresh_disks,
            onboarding::commands::format_disk,
            onboarding::commands::init_vault,
            onboarding::commands::populate_template,
            onboarding::commands::download_embedding_model,
            onboarding::commands::read_marker,
            onboarding::commands::detect_existing_vault,
            onboarding::commands::finish_onboarding,
            onboarding::commands::bootstrap_app,
            onboarding::commands::reset_brain,
            onboarding::commands::update_vault_templates,
            // Mount (S01) / Tray (S07)
            tray::commands::tray_status,
            tray::commands::eject_brain,
            mount::commands::unclean_shutdown_pending,
            mount::commands::run_integrity_check,
            mount::commands::run_recovery_action,
            mount::commands::dismiss_unclean_flag,
            // MCP (S06)
            mcp::commands::brain_mcp_command_hint,
            mcp::commands::reregister_mcp,
            mcp::commands::last_mcp_registration_report,
            mcp::commands::brain_memory_system_prompt,
            // Wiki (S03) + Viewer (S08–S10)
            viewer::commands::list_wiki_tree,
            viewer::commands::read_page,
            viewer::commands::search_pages,
            viewer::commands::get_backlinks,
            viewer::commands::get_graph,
            viewer::commands::query_pages,
            viewer::commands::rebuild_index,
            viewer::commands::open_page_in_external_editor,
            viewer::commands::load_graph_positions,
            viewer::commands::save_graph_positions,
            viewer::commands::clear_graph_positions,
            wiki::commands::wiki_history,
            wiki::commands::wiki_commit_detail,
            wiki::commands::wiki_restore_page,
            wiki::commands::wiki_hard_reset,
            // Sync (S11 phase 6)
            wiki::commands::git_remote_status,
            wiki::commands::set_git_remote,
            wiki::commands::set_git_credential,
            wiki::commands::sync_now,
            // Update (S04)
            update::commands::check_update,
            update::commands::apply_update,
            update::commands::skip_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running brain");
}

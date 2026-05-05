//! Auto-registration of the Brain MCP server into local LLM client configs.
//!
//! Each adapter writes a small JSON snippet into the user's per-client
//! configuration file at mount and removes it at unmount. Other entries
//! configured by the user remain untouched.

use std::path::{Path, PathBuf};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

use super::McpResult;

/// Display name of the MCP server in `mcpServers` maps and in the
/// `claude mcp` CLI. We use uppercase to match the BRAIN brand wording in
/// the rest of the UI; older installs used lowercase `brain`, so the
/// unregister flow purges *both* keys to clean up after a user upgrade
/// (see `LEGACY_BRAIN_SERVER_KEYS`).
pub const BRAIN_SERVER_KEY: &str = "BRAIN";

/// Aliases we may have written into client configs in older versions.
/// Re-registration removes these alongside the canonical key so users
/// don't end up with two duplicate entries after upgrading.
pub const LEGACY_BRAIN_SERVER_KEYS: &[&str] = &["brain"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub command: String,
    pub args: Vec<String>,
    pub env: serde_json::Map<String, serde_json::Value>,
}

/// Both Claude Code (`~/.claude.json`) and Claude Desktop
/// (`claude_desktop_config.json`) accept the bare
/// `{command, args, env}` shape — they infer transport from the presence of
/// `command`. We deliberately omit a `type` field because Claude Desktop's
/// schema is strict and silently drops entries with unknown keys, which is
/// what bit us when we wrote `"type": "stdio"` and the desktop app then
/// pretended Brain wasn't there.
const _SCHEMA_NOTE: () = ();

/// Builds the entry that supported clients should put in their `mcpServers`
/// map. The command is the absolute path to the Brain binary; the env block
/// passes the vault path to the spawned MCP subprocess.
///
/// Paths are normalised to forward-slashes so that:
/// - the Windows command-line escaper does not see a trailing `\` on the
///   env value (which would escape the closing quote and break parsing);
/// - the JSON file remains diff-friendly with portable separators.
pub fn brain_server_entry(vault_path: &Path) -> McpServerEntry {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| normalise_path(&p.to_string_lossy()))
        .unwrap_or_else(|| "brain".to_string());
    let mut env = serde_json::Map::new();
    env.insert(
        "BRAIN_VAULT_PATH".to_string(),
        serde_json::Value::String(normalise_path(&vault_path.to_string_lossy())),
    );
    McpServerEntry {
        command: exe,
        args: vec!["mcp".into()],
        env,
    }
}

/// Replaces backslashes with forward slashes and trims trailing separators.
/// Windows accepts forward-slashes in path APIs, and trimming the trailer
/// avoids shell-escaping issues when this string is passed as a CLI arg.
pub(crate) fn normalise_path(raw: &str) -> String {
    let with_slashes = raw.replace('\\', "/");
    // Keep the leading slash on POSIX absolute paths; only drop trailing
    // slashes that aren't a drive root (`D:/` collapses to `D:` which is
    // wrong, so preserve `D:/`).
    let trimmed = with_slashes.trim_end_matches('/');
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        // Drive-root case — keep the slash so it stays a directory.
        format!("{trimmed}/")
    } else if trimmed.is_empty() {
        // POSIX root: `/` → was trimmed to empty; restore it.
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Per-client registration outcome. Surfaced to the frontend so the user can
/// see what actually happened during onboarding.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "detail")]
pub enum ClientStatus {
    /// Successfully registered — `detail` describes the method
    /// ("via claude mcp add", "via ~/.claude.json", …).
    Registered(String),
    /// Client not present on this host (config dir not found).
    NotInstalled,
    /// Registration attempted but failed — `detail` is the error message.
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RegistrationReport {
    /// Claude Code CLI (`~/.claude.json`).
    pub claude_code: Option<ClientStatus>,
    /// Claude Desktop app (`%APPDATA%\Claude\claude_desktop_config.json` on
    /// Windows, `~/Library/Application Support/Claude/...` on macOS).
    /// Distinct from Claude Code — they share a name but ship as separate
    /// products with separate config files.
    pub claude_desktop: Option<ClientStatus>,
    pub codex: Option<ClientStatus>,
    pub continue_dev: Option<ClientStatus>,
    pub chatgpt_desktop: Option<ClientStatus>,
}

/// Registers the Brain MCP server in every supported client. Returns a
/// detailed report so the caller / frontend can show exactly which
/// integration succeeded and which fell back. Best-effort: a single client
/// failure does not abort the others.
pub fn register_brain_in_supported_clients(vault_path: &Path) -> McpResult<RegistrationReport> {
    Ok(RegistrationReport {
        // Claude Code: prefer the official `claude mcp add` CLI because its
        // storage format and on-disk location may evolve and is non-trivial.
        // Fall back to direct file write only if the CLI isn't on PATH.
        claude_code: Some(register_claude_code(vault_path)),
        // Claude Desktop ships in two distribution forms on Windows: a
        // standard MSI/EXE install that uses `%APPDATA%\Claude\` and a
        // Microsoft Store install that runs sandboxed and redirects that
        // path to a per-package `LocalCache\Roaming\Claude\` directory.
        // We write to every active install we can detect so whichever
        // variant the user actually launches finds Brain.
        claude_desktop: Some(register_claude_desktop_anywhere(vault_path)),
        codex: Some(register_simple_client(
            codex_config_path(),
            vault_path,
            "Codex",
        )),
        continue_dev: Some(register_simple_client(
            continue_dev_config_path(),
            vault_path,
            "Continue.dev",
        )),
        chatgpt_desktop: Some(register_simple_client(
            chatgpt_desktop_config_path(),
            vault_path,
            "ChatGPT Desktop",
        )),
    })
}

/// Like `register_simple_client` but skips the "is-the-app-installed?"
/// heuristic and always writes. Reserved for clients (notably Claude
/// Desktop) where the absence of the parent dir doesn't reliably mean
/// the app isn't installed.
#[cfg(test)]
fn register_always(config_path: Option<PathBuf>, vault_path: &Path) -> ClientStatus {
    let Some(path) = config_path else {
        return ClientStatus::NotInstalled;
    };
    match register_in_client(&path, &brain_server_entry(vault_path)) {
        Ok(()) => ClientStatus::Registered(format!("via {}", path.display())),
        Err(err) => ClientStatus::Failed(err.to_string()),
    }
}

/// Registers Brain into every detected Claude Desktop install simultaneously
/// — both the Microsoft Store sandbox path (`%LOCALAPPDATA%\Packages\
/// Claude_*\LocalCache\Roaming\Claude\…`) **and** the regular
/// `%APPDATA%\Claude\…` path. If neither install is detected, drops a
/// canonical config in `%APPDATA%\Claude\` so a future first-run picks it up.
fn register_claude_desktop_anywhere(vault_path: &Path) -> ClientStatus {
    let Some(home) = UserDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return ClientStatus::NotInstalled;
    };
    let candidates = claude_desktop_candidates(&home);

    // For each candidate, decide whether we should write to it:
    //   - Sandbox candidates (path contains `\Packages\`): yes — the
    //     package dir was just enumerated, so the install exists.
    //     `register_in_client` will create the inner `LocalCache\Roaming\
    //     Claude` chain if it doesn't exist yet.
    //   - Non-sandbox candidates: only if the parent directory exists
    //     (= app installed in this form). Otherwise we'd litter the
    //     filesystem with empty config dirs for apps the user doesn't have.
    let mut targets: Vec<PathBuf> = Vec::new();
    for path in &candidates {
        let path_str = path.to_string_lossy();
        let is_sandbox =
            path_str.contains("\\Packages\\") || path_str.contains("/Packages/");
        let parent_exists = path.parent().map(|p| p.exists()).unwrap_or(false);
        if is_sandbox || parent_exists {
            targets.push(path.clone());
        }
    }

    // Bootstrap fallback when nothing was detected: write the canonical
    // first non-sandbox candidate. A future Claude Desktop install picks
    // it up on first run.
    if targets.is_empty() {
        if let Some(p) = candidates.into_iter().find(|c| {
            let s = c.to_string_lossy();
            !(s.contains("\\Packages\\") || s.contains("/Packages/"))
        }) {
            targets.push(p);
        }
    }

    if targets.is_empty() {
        return ClientStatus::NotInstalled;
    }

    let mut written: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for path in targets {
        match register_in_client(&path, &brain_server_entry(vault_path)) {
            Ok(()) => written.push(path.display().to_string()),
            Err(err) => errors.push(format!("{}: {}", path.display(), err)),
        }
    }

    if !written.is_empty() {
        ClientStatus::Registered(format!("via {}", written.join("; ")))
    } else {
        ClientStatus::Failed(errors.join("; "))
    }
}

fn register_claude_code(vault_path: &Path) -> ClientStatus {
    if let Some(claude) = find_claude_cli() {
        match invoke_claude_mcp_add(&claude, vault_path) {
            Ok(()) => return ClientStatus::Registered("via claude mcp add (user scope)".into()),
            Err(err) => {
                tracing::warn!(?err, "claude mcp add failed, falling back to direct file write");
            }
        }
    }
    // Fallback: direct file write.
    match claude_code_config_path() {
        Some(path) => match register_in_client(&path, &brain_server_entry(vault_path)) {
            Ok(()) => ClientStatus::Registered(format!("via {}", path.display())),
            Err(err) => ClientStatus::Failed(err.to_string()),
        },
        None => ClientStatus::NotInstalled,
    }
}

fn register_simple_client(
    config_path: Option<PathBuf>,
    vault_path: &Path,
    label: &str,
) -> ClientStatus {
    let Some(path) = config_path else {
        return ClientStatus::NotInstalled;
    };
    if !path.exists()
        && path
            .parent()
            .map(|p| !p.exists())
            .unwrap_or(false)
    {
        // Parent dir doesn't exist either — assume the client isn't installed.
        let _ = label;
        return ClientStatus::NotInstalled;
    }
    match register_in_client(&path, &brain_server_entry(vault_path)) {
        Ok(()) => ClientStatus::Registered(format!("via {}", path.display())),
        Err(err) => ClientStatus::Failed(err.to_string()),
    }
}

/// Best-effort inverse of the above for the unmount flow.
pub fn unregister_brain_from_supported_clients() -> McpResult<()> {
    if let Some(claude) = find_claude_cli() {
        // Remove the canonical key plus any legacy aliases, so users who
        // upgrade from < 0.2.0 (key was lowercase `brain`) don't keep an
        // orphan entry pointing at a stale binary path.
        for key in std::iter::once(BRAIN_SERVER_KEY).chain(LEGACY_BRAIN_SERVER_KEYS.iter().copied()) {
            let _ = crate::proc::no_window(&claude)
                .args(["mcp", "remove", key, "--scope", "user"])
                .output();
        }
    }
    let candidates = [
        claude_code_config_path(),
        claude_desktop_config_path(),
        codex_config_path(),
        continue_dev_config_path(),
        chatgpt_desktop_config_path(),
    ];
    for path in candidates.into_iter().flatten() {
        let _ = unregister_from_client(&path);
    }
    Ok(())
}

/// Locates the `claude` CLI. On Windows the npm shim is `claude.cmd`; on
/// macOS/Linux it's the regular `claude` binary on PATH. Returns the
/// command name to spawn, or `None` if Claude Code isn't installed.
pub fn find_claude_cli() -> Option<String> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["claude.cmd", "claude.exe", "claude"]
    } else {
        &["claude"]
    };
    for c in candidates {
        let ok = crate::proc::no_window(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some((*c).to_string());
        }
    }
    None
}

fn invoke_claude_mcp_add(claude: &str, vault_path: &Path) -> std::io::Result<()> {
    // Forward-slash paths so the Windows arg quoter doesn't trip on a
    // trailing backslash escaping its closing quote.
    let exe = normalise_path(&std::env::current_exe()?.to_string_lossy());
    let vault = normalise_path(&vault_path.to_string_lossy());

    // Idempotent removal of the canonical key plus any legacy aliases —
    // both failures are ignored (entry might not exist yet).
    for key in std::iter::once(BRAIN_SERVER_KEY).chain(LEGACY_BRAIN_SERVER_KEYS.iter().copied()) {
        let _ = crate::proc::no_window(claude)
            .args(["mcp", "remove", key, "--scope", "user"])
            .output();
    }

    // Claude Code's `claude mcp add` declares the env flag as variadic
    // (`-e <env...>`). On commander.js short-form (`-e`) attached values
    // keep the leading `=` (so `-e=KEY=VAL` becomes value `=KEY=VAL`).
    // Long-form (`--env=…`) attached values strip the `=` correctly. Plus
    // the `--` terminator stops the variadic from chewing positionals.
    let env_eq = format!("--env=BRAIN_VAULT_PATH={vault}");
    let output = crate::proc::no_window(claude)
        .args([
            "mcp",
            "add",
            "--scope",
            "user",
            &env_eq,
            "--",
            BRAIN_SERVER_KEY,
            &exe,
            "mcp",
        ])
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "claude mcp add exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Locations of supported per-user client config files.
///
/// Claude Code stores its `mcpServers` map in `~/.claude.json` at the home
/// directory root — **not** in `~/.claude/settings.json` (that file holds
/// permissions/hooks). Writing to the wrong file means Claude Code silently
/// ignores the entry, which is exactly what bit us before.
pub fn claude_code_config_path() -> Option<PathBuf> {
    let home = UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".claude.json"))
}

/// Claude Desktop is the GUI app from Anthropic and is **distinct** from
/// Claude Code (the CLI). It reads its MCP config from a separate file:
/// - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
/// - Windows: `%APPDATA%\Claude\claude_desktop_config.json` (or sometimes
///   `%APPDATA%\AnthropicClaude\…` depending on installer version)
/// - Linux: `~/.config/Claude/claude_desktop_config.json`
///
/// We probe a list of known candidate paths and pick the one whose parent
/// directory already exists (= app most likely installed). Failing that we
/// return the canonical path so the caller can still write — Claude Desktop
/// will read the file as soon as it starts.
pub fn claude_desktop_config_path() -> Option<PathBuf> {
    let home = UserDirs::new()?.home_dir().to_path_buf();
    let candidates = claude_desktop_candidates(&home);
    candidates
        .iter()
        .find(|c| {
            c.parent()
                .map(|p| p.exists())
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn claude_desktop_candidates(home: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        paths.push(
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        );
        paths.push(
            home.join("Library")
                .join("Application Support")
                .join("AnthropicClaude")
                .join("claude_desktop_config.json"),
        );
    }
    #[cfg(target_os = "windows")]
    {
        let roaming = home.join("AppData").join("Roaming");
        let local = home.join("AppData").join("Local");

        // Microsoft Store / WindowsApps sandbox installs come FIRST so the
        // picker prefers them when both forms are present. The store
        // distribution names its package `Claude_<publisher-hash>` (e.g.
        // `Claude_pzs8sxrjxfjjc`) and virtualises `%APPDATA%\Claude\` to
        // `…\Packages\<pkg>\LocalCache\Roaming\Claude\`. Writing only to
        // `%APPDATA%\Claude\` leaves the sandboxed app blind to our entry.
        if let Ok(entries) = std::fs::read_dir(local.join("Packages")) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("Claude_") || name.starts_with("AnthropicClaude_") {
                    paths.push(
                        entry
                            .path()
                            .join("LocalCache")
                            .join("Roaming")
                            .join("Claude")
                            .join("claude_desktop_config.json"),
                    );
                }
            }
        }

        // Non-sandbox MSI/EXE installs.
        paths.push(roaming.join("Claude").join("claude_desktop_config.json"));
        paths.push(
            roaming
                .join("AnthropicClaude")
                .join("claude_desktop_config.json"),
        );
        paths.push(
            local
                .join("AnthropicClaude")
                .join("claude_desktop_config.json"),
        );
        paths.push(
            local
                .join("Programs")
                .join("Claude")
                .join("claude_desktop_config.json"),
        );
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
        );
        paths.push(
            home.join(".config")
                .join("AnthropicClaude")
                .join("claude_desktop_config.json"),
        );
    }
    paths
}

pub fn codex_config_path() -> Option<PathBuf> {
    let home = UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".codex").join("config.json"))
}

pub fn continue_dev_config_path() -> Option<PathBuf> {
    let home = UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".continue").join("config.json"))
}

pub fn chatgpt_desktop_config_path() -> Option<PathBuf> {
    let home = UserDirs::new()?.home_dir().to_path_buf();
    #[cfg(target_os = "macos")]
    {
        return Some(
            home.join("Library")
                .join("Application Support")
                .join("ChatGPT")
                .join("mcp.json"),
        );
    }
    #[cfg(target_os = "windows")]
    {
        return Some(
            home.join("AppData")
                .join("Roaming")
                .join("ChatGPT")
                .join("mcp.json"),
        );
    }
    #[cfg(target_os = "linux")]
    {
        return Some(home.join(".config").join("chatgpt").join("mcp.json"));
    }
    #[allow(unreachable_code)]
    None
}

/// Writes (or merges) the Brain entry into a client's config file under the
/// `mcpServers` key, preserving other entries.
///
/// Defensive: if the existing file exists but is **not** valid JSON we abort
/// instead of overwriting — the user may have a hand-edited config we don't
/// want to clobber. The write itself goes through a temp file + rename so
/// concurrent readers (e.g. Claude Code at startup) never see a half-written
/// file.
pub fn register_in_client(config_path: &Path, entry: &McpServerEntry) -> McpResult<()> {
    let mut root: serde_json::Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)?;
        if raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&raw).map_err(super::McpError::Json)?
        }
    } else {
        serde_json::json!({})
    };

    let map = root.as_object_mut().ok_or_else(|| {
        super::McpError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config root is not an object",
        )))
    })?;
    let servers = map
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| {
        super::McpError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mcpServers is not an object",
        )))
    })?;
    // Strip any legacy aliases (`brain` from < 0.2.0 installs) so the user
    // doesn't end up with two MCP entries pointing at the same binary.
    for legacy in LEGACY_BRAIN_SERVER_KEYS {
        servers.remove(*legacy);
    }
    servers.insert(BRAIN_SERVER_KEY.to_string(), serde_json::to_value(entry)?);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(&root)?;
    atomic_write(config_path, raw.as_bytes())?;
    Ok(())
}

fn atomic_write(target: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = target.with_extension("brain.tmp");
    std::fs::write(&tmp, data)?;
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows refuses rename-over-existing on some configurations.
            // Fall back to copy + remove.
            std::fs::copy(&tmp, target)?;
            std::fs::remove_file(&tmp).ok();
            Ok(())
        }
    }
}

/// Removes only the Brain entry from a client's config; other entries stay.
pub fn unregister_from_client(config_path: &Path) -> McpResult<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(config_path)?;
    let mut root: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(servers) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|v| v.as_object_mut())
    {
        servers.remove(BRAIN_SERVER_KEY);
        // Also remove legacy aliases so older installs get cleaned up.
        for legacy in LEGACY_BRAIN_SERVER_KEYS {
            servers.remove(*legacy);
        }
    }
    std::fs::write(config_path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn brain_entry() -> McpServerEntry {
        McpServerEntry {
            command: "brain".into(),
            args: vec!["mcp".into()],
            env: serde_json::Map::new(),
        }
    }

    #[test]
    fn brain_entry_serialises_with_only_command_args_env_no_extra_fields() {
        let json = serde_json::to_value(brain_entry()).unwrap();
        let obj = json.as_object().unwrap();
        let keys: std::collections::HashSet<_> = obj.keys().map(|k| k.as_str()).collect();
        // Claude Desktop's strict schema rejects unknown fields, so the
        // entry must contain exactly these three keys and nothing else.
        assert_eq!(keys, ["command", "args", "env"].into_iter().collect());
    }

    #[test]
    fn normalise_path_replaces_backslashes_with_forward_slashes() {
        assert_eq!(normalise_path("C:\\Users\\p\\brain.exe"), "C:/Users/p/brain.exe");
    }

    #[test]
    fn normalise_path_strips_trailing_slash_unless_drive_root() {
        assert_eq!(normalise_path("D:\\"), "D:/");
        assert_eq!(normalise_path("/home/p/brain/"), "/home/p/brain");
        assert_eq!(normalise_path("C:\\Users\\"), "C:/Users");
    }

    #[test]
    fn register_creates_config_file_when_missing_with_brain_entry() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join(".claude.json");
        register_in_client(&config, &brain_entry()).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(parsed["mcpServers"][BRAIN_SERVER_KEY]["command"].as_str() == Some("brain"));
    }

    #[test]
    fn register_preserves_other_mcp_servers_already_configured() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.json");
        std::fs::write(
            &config,
            r#"{"mcpServers":{"github":{"command":"gh-mcp","args":[],"env":{}}}}"#,
        )
        .unwrap();
        register_in_client(&config, &brain_entry()).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["github"]["command"].as_str() == Some("gh-mcp"));
        assert!(parsed["mcpServers"][BRAIN_SERVER_KEY]["command"].as_str() == Some("brain"));
        // Must NOT contain `type` — Claude Desktop's strict schema would
        // silently drop entries with unknown fields.
        assert!(parsed["mcpServers"][BRAIN_SERVER_KEY].get("type").is_none());
    }

    #[test]
    fn register_drops_legacy_lowercase_brain_key_on_upgrade() {
        // Older installs (< 0.2.0) wrote the entry under the lowercase key
        // `brain`. After upgrade we must purge it so the user doesn't see
        // two entries pointing at the same binary in Claude Desktop.
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.json");
        std::fs::write(
            &config,
            r#"{"mcpServers":{"brain":{"command":"old-brain","args":[],"env":{}}}}"#,
        )
        .unwrap();
        register_in_client(&config, &brain_entry()).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["brain"].is_null(), "legacy lowercase key must be removed");
        assert!(parsed["mcpServers"]["BRAIN"]["command"].as_str() == Some("brain"));
    }

    #[test]
    fn unregister_removes_only_brain_entry_and_leaves_others() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.json");
        std::fs::write(
            &config,
            r#"{"mcpServers":{"BRAIN":{"command":"brain","args":[],"env":{}},"github":{"command":"gh-mcp","args":[],"env":{}}}}"#,
        )
        .unwrap();
        unregister_from_client(&config).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(parsed["mcpServers"][BRAIN_SERVER_KEY].is_null());
        assert!(parsed["mcpServers"]["github"]["command"].as_str() == Some("gh-mcp"));
    }

    #[test]
    fn unregister_also_strips_legacy_lowercase_brain_key() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.json");
        std::fs::write(
            &config,
            r#"{"mcpServers":{"brain":{"command":"brain","args":[],"env":{}}}}"#,
        )
        .unwrap();
        unregister_from_client(&config).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["brain"].is_null());
    }

    #[test]
    fn unregister_silently_succeeds_when_config_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("missing.json");
        unregister_from_client(&config).unwrap();
    }

    #[test]
    fn register_refuses_to_clobber_a_corrupt_existing_config_file() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.json");
        std::fs::write(&config, "this is not json").unwrap();
        let err = register_in_client(&config, &brain_entry()).unwrap_err();
        assert!(matches!(err, super::super::McpError::Json(_)));
        // Original contents must be preserved.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "this is not json");
    }

    #[test]
    fn claude_code_config_path_now_points_to_dot_claude_json_not_settings_json() {
        let path = claude_code_config_path().expect("home dir available");
        assert!(path.ends_with(".claude.json"));
        assert!(!path.to_string_lossy().contains("settings.json"));
    }

    #[test]
    fn claude_desktop_config_path_returns_a_per_platform_candidate() {
        let path = claude_desktop_config_path().expect("home dir available");
        let s = path.to_string_lossy();
        assert!(s.ends_with("claude_desktop_config.json"));
        // Sanity: it must mention either "Claude" or "AnthropicClaude" in
        // its path so we know we picked one of the known-good candidates.
        assert!(s.contains("Claude") || s.contains("AnthropicClaude"));
    }

    #[test]
    fn register_always_writes_even_when_parent_dir_is_missing() {
        let tmp = TempDir::new().unwrap();
        let config = tmp
            .path()
            .join("AppData")
            .join("Roaming")
            .join("Claude")
            .join("claude_desktop_config.json");
        // Parent doesn't exist; register_always must still create it.
        let status = register_always(Some(config.clone()), tmp.path());
        assert!(matches!(status, ClientStatus::Registered(_)));
        assert!(config.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn claude_desktop_candidates_picks_up_microsoft_store_sandbox_packages() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp
            .path()
            .join("AppData")
            .join("Local")
            .join("Packages")
            .join("Claude_pzs8sxrjxfjjc");
        std::fs::create_dir_all(&pkg).unwrap();
        let candidates = claude_desktop_candidates(tmp.path());
        let has_sandbox = candidates.iter().any(|p| {
            let s = p.to_string_lossy();
            s.contains("Claude_pzs8sxrjxfjjc")
                && s.contains("LocalCache")
                && s.ends_with("claude_desktop_config.json")
        });
        assert!(
            has_sandbox,
            "candidates should include the sandbox path, got: {:#?}",
            candidates
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn claude_desktop_candidates_orders_sandbox_before_appdata() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp
            .path()
            .join("AppData")
            .join("Local")
            .join("Packages")
            .join("Claude_abcdef123");
        std::fs::create_dir_all(&pkg).unwrap();
        let candidates = claude_desktop_candidates(tmp.path());
        let first = candidates.first().expect("at least one candidate");
        let s = first.to_string_lossy();
        assert!(
            s.contains("\\Packages\\Claude_abcdef123\\")
                && s.contains("\\LocalCache\\"),
            "sandbox path must come first, got {first:?}"
        );
    }
}

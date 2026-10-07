//! S06 — install BRAIN's memory prompt and the `brain-wiki` Agent Skill
//! into the user's agent clients, on the user's request (Settings → MCP &
//! Clients, one switch per target).
//!
//! | target | file |
//! |---|---|
//! | Claude Code prompt | `~/.claude/CLAUDE.md` (user-level instructions, every project) |
//! | Claude Code skill | `~/.claude/skills/brain-wiki/SKILL.md` |
//! | Codex prompt | `$CODEX_HOME/AGENTS.md` (default `~/.codex/AGENTS.md`) |
//! | Codex skill | `~/.agents/skills/brain-wiki/SKILL.md` |
//!
//! Sources for the file conventions: Claude Code's user-level CLAUDE.md
//! and `~/.claude/skills/<name>/SKILL.md` (frontmatter `name` = folder)
//! were verified on the user's machine on 2026-10-07; Codex reading
//! `$CODEX_HOME/AGENTS.md` likewise. That Codex discovers skills under
//! `~/.agents/skills/<name>/SKILL.md` is VERIFIED ONLY BY A THIRD-PARTY
//! DOC (the superpowers project's Codex install instructions), not by
//! OpenAI documentation or a local test — re-check when Codex changes.
//! Claude Desktop keeps its instructions in the cloud (no local file), so
//! it stays copy-paste.
//!
//! BRAIN only ever touches what it marked as its own:
//!  - in an instruction file, the block between
//!    `<!-- BRAIN:<id> v<version> -->` and `<!-- /BRAIN:<id> -->`;
//!    everything outside it stays byte-for-byte (line endings included);
//!  - a skill file that carries `<!-- BRAIN:skill v<version> -->` right
//!    after its frontmatter. A `brain-wiki` folder without that mark is
//!    "foreign" and never overwritten or removed.
//!
//! Every function takes its paths explicitly ([`ClientPaths`]); only
//! [`ClientPaths::resolve`] looks at the real home directory, so tests run
//! on temp dirs.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ClientInstallSettings;

/// The block id of the memory prompt in an instruction file.
pub const MEMORY_BLOCK_ID: &str = "memory-prompt";

/// Folder name of the installed skill (= its frontmatter `name`).
pub const SKILL_NAME: &str = "brain-wiki";

const SKILL_FILE: &str = "SKILL.md";
const SKILL_MARKER_PREFIX: &str = "<!-- BRAIN:skill v";

/// The version stamped into installed blocks and skills: the app version,
/// so every app update marks older installs as outdated.
pub const INSTALL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One place BRAIN can install into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    ClaudeCodePrompt,
    ClaudeCodeSkill,
    CodexPrompt,
    CodexSkill,
}

impl Target {
    pub const ALL: [Target; 4] = [
        Target::ClaudeCodePrompt,
        Target::ClaudeCodeSkill,
        Target::CodexPrompt,
        Target::CodexSkill,
    ];

    fn client_name(self) -> &'static str {
        match self {
            Target::ClaudeCodePrompt | Target::ClaudeCodeSkill => "Claude Code",
            Target::CodexPrompt | Target::CodexSkill => "Codex",
        }
    }
}

/// What a target looks like on disk right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum InstallStatus {
    /// The client is there, BRAIN's block / skill is not.
    NotInstalled,
    /// BRAIN's block / skill of the current version.
    Installed { version: String },
    /// BRAIN's block / skill of another version.
    Outdated { version: String },
    /// A `brain-wiki` skill folder BRAIN did not write.
    Foreign,
    /// The client's directory does not exist (client not installed).
    TargetMissing,
    /// BRAIN's markers in the instruction file are broken (a start without
    /// an end, an end without a start, or more than one block). BRAIN
    /// writes nothing until the user fixes them by hand.
    Damaged,
}

/// The client directories, resolved once. Tests build it on a temp dir
/// with [`ClientPaths::under_home`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    /// `~/.claude`
    pub claude_dir: PathBuf,
    /// `$CODEX_HOME`, default `~/.codex`
    pub codex_home: PathBuf,
    /// `~/.agents/skills`
    pub agents_skills_dir: PathBuf,
}

impl ClientPaths {
    /// The client directories under `home`; `codex_home` overrides
    /// `<home>/.codex` (the `CODEX_HOME` variable).
    pub fn under_home(home: &Path, codex_home: Option<PathBuf>) -> Self {
        Self {
            claude_dir: home.join(".claude"),
            codex_home: codex_home.unwrap_or_else(|| home.join(".codex")),
            agents_skills_dir: home.join(".agents").join("skills"),
        }
    }

    /// The real user's directories (home from the OS, `CODEX_HOME` from
    /// the environment, as in `registration::codex_config_path`).
    pub fn resolve() -> Option<Self> {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Some(Self::under_home(&home, codex_home))
    }

    /// The directory whose existence means "this client is installed".
    pub fn client_dir(&self, target: Target) -> &Path {
        match target {
            Target::ClaudeCodePrompt | Target::ClaudeCodeSkill => &self.claude_dir,
            Target::CodexPrompt | Target::CodexSkill => &self.codex_home,
        }
    }

    /// The instruction file of a prompt target.
    pub fn prompt_file(&self, target: Target) -> Option<PathBuf> {
        match target {
            Target::ClaudeCodePrompt => Some(self.claude_dir.join("CLAUDE.md")),
            Target::CodexPrompt => Some(self.codex_home.join("AGENTS.md")),
            _ => None,
        }
    }

    /// The skills directory of a skill target (the skill goes into
    /// `<dir>/brain-wiki/SKILL.md`).
    pub fn skills_dir(&self, target: Target) -> Option<PathBuf> {
        match target {
            Target::ClaudeCodeSkill => Some(self.claude_dir.join("skills")),
            Target::CodexSkill => Some(self.agents_skills_dir.clone()),
            _ => None,
        }
    }

    /// The file BRAIN writes for `target` (for display).
    pub fn file(&self, target: Target) -> PathBuf {
        match self.prompt_file(target) {
            Some(file) => file,
            None => skill_file(&self.skills_dir(target).unwrap_or_default()),
        }
    }
}

/// The memory prompt as installed into an instruction file: the system
/// prompt of the "Memory mode" tab plus a pointer to the skill and to
/// `brain://agents-md`.
pub fn prompt_body() -> String {
    format!(
        "{}\n\nFor ingesting, cleaning up and dreaming, use the `{SKILL_NAME}` skill when it is \
         installed, and read the MCP resource `brain://agents-md` (the vault's AGENTS.md) before \
         writing pages.",
        super::commands::BRAIN_MEMORY_SYSTEM_PROMPT
    )
}

// ---- marked blocks ------------------------------------------------------

fn start_prefix(block_id: &str) -> String {
    format!("<!-- BRAIN:{block_id} v")
}

fn end_marker(block_id: &str) -> String {
    format!("<!-- /BRAIN:{block_id} -->")
}

/// Byte range `[start, end)` of a marked block (from the start marker to
/// the end of the end marker) and its version.
struct BlockSpan {
    start: usize,
    end: usize,
    version: String,
}

/// BRAIN's markers in a file are not one well-formed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamagedMarkers;

/// The one marked block `block_id` in `text`. Markers count only on a
/// line of their own (surrounding whitespace allowed), so a marker quoted
/// inside the user's prose is ignored. `Ok(None)`: no marker line at all.
/// [`DamagedMarkers`]: a start without an end, an end without a start, an
/// end before its start, more than one block, or a start marker without
/// a version — then nothing may be rewritten, because the span between
/// the markers could hold the user's own text.
fn find_block(text: &str, block_id: &str) -> Result<Option<BlockSpan>, DamagedMarkers> {
    let prefix = start_prefix(block_id);
    let end_marker = end_marker(block_id);
    let mut starts: Vec<(usize, usize, String)> = Vec::new();
    let mut ends: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim();
        let start = offset + (content.len() - content.trim_start().len());
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let version = rest.strip_suffix(" -->").unwrap_or("");
            if version.is_empty() || version.contains(char::is_whitespace) {
                return Err(DamagedMarkers);
            }
            starts.push((start, start + trimmed.len(), version.to_string()));
        } else if trimmed == end_marker {
            ends.push((start, start + trimmed.len()));
        }
        offset += line.len();
    }
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([(start, start_end, version)], [(end_start, end)]) if start_end <= end_start => {
            Ok(Some(BlockSpan {
                start: *start,
                end: *end,
                version: version.clone(),
            }))
        }
        _ => Err(DamagedMarkers),
    }
}

fn damaged_error(file: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "BRAIN's markers in {} are damaged (a start or end marker without its partner, or \
             more than one block) — fix them by hand; BRAIN writes nothing until then",
            file.display()
        ),
    )
}

/// The version of BRAIN's block `block_id` in `text`, if there is exactly
/// one well-formed block.
pub fn block_version(text: &str, block_id: &str) -> Option<String> {
    find_block(text, block_id)
        .ok()
        .flatten()
        .map(|span| span.version)
}

/// The line ending of `text`: CRLF when it has any, else LF (also for an
/// empty or missing file).
fn newline_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn render_block(block_id: &str, version: &str, body: &str, nl: &str) -> String {
    let body = body.replace("\r\n", "\n");
    let body = body.trim_end_matches('\n').replace('\n', nl);
    format!(
        "{}{version} -->{nl}{body}{nl}{}",
        start_prefix(block_id),
        end_marker(block_id)
    )
}

/// Read a text file; `None` when it does not exist. A file that is not
/// UTF-8 is an error — BRAIN does not rewrite what it cannot read.
fn read_text(file: &Path) -> io::Result<Option<String>> {
    match std::fs::read(file) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Write BRAIN's block `block_id` (version `version`, content `body`) into
/// `file`: an existing block is replaced in place, otherwise the block is
/// appended after one blank line (or becomes the whole file when the file
/// is missing or empty). Everything outside the block is kept byte for
/// byte; the block uses the file's line ending (CRLF if the file has any,
/// else LF). Written atomically. Returns `true` when the file did not
/// exist before (BRAIN created it).
pub fn upsert_marked_block(
    file: &Path,
    block_id: &str,
    version: &str,
    body: &str,
) -> io::Result<bool> {
    let existing = read_text(file)?;
    let created = existing.is_none();
    let text = existing.unwrap_or_default();
    let nl = newline_of(&text);
    let block = render_block(block_id, version, body, nl);
    let span = find_block(&text, block_id).map_err(|_| damaged_error(file))?;
    let new_text = if let Some(span) = span {
        format!("{}{block}{}", &text[..span.start], &text[span.end..])
    } else if text.is_empty() {
        format!("{block}{nl}")
    } else {
        // Keep the text as is, end it with a newline, add one blank line.
        let mut out = text.clone();
        if !out.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str(nl);
        out.push_str(&block);
        out.push_str(nl);
        out
    };
    if new_text != text || created {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fsutil::atomic_write(file, new_text.as_bytes())?;
    }
    Ok(created)
}

/// What [`remove_marked_block`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// There was no block (or no file).
    NothingToRemove,
    /// The block was removed; the rest of the file stays.
    Removed,
    /// The block was removed and the file, now blank, was deleted.
    FileDeleted,
}

/// Remove BRAIN's block `block_id` from `file`, together with the line
/// break after it and the blank line [`upsert_marked_block`] put before
/// it, so an install followed by a removal gives back the original file.
/// When nothing but whitespace remains and `delete_if_blank` is set (BRAIN
/// created the file), the file is deleted; otherwise it stays.
pub fn remove_marked_block(
    file: &Path,
    block_id: &str,
    delete_if_blank: bool,
) -> io::Result<RemoveOutcome> {
    let Some(text) = read_text(file)? else {
        return Ok(RemoveOutcome::NothingToRemove);
    };
    let Some(span) = find_block(&text, block_id).map_err(|_| damaged_error(file))? else {
        return Ok(RemoveOutcome::NothingToRemove);
    };
    let nl = newline_of(&text);
    let mut before = &text[..span.start];
    let mut after = &text[span.end..];
    if let Some(rest) = after.strip_prefix(nl) {
        after = rest;
    } else if let Some(rest) = after.strip_prefix('\n') {
        after = rest;
    }
    let blank_line = format!("{nl}{nl}");
    if before.ends_with(&blank_line) {
        before = &before[..before.len() - nl.len()];
    }
    let new_text = format!("{before}{after}");
    // A link (dotfile manager) is never deleted — only emptied.
    if new_text.trim().is_empty() && delete_if_blank && !crate::fsutil::is_symlink(file) {
        std::fs::remove_file(file)?;
        return Ok(RemoveOutcome::FileDeleted);
    }
    crate::fsutil::atomic_write(file, new_text.as_bytes())?;
    Ok(RemoveOutcome::Removed)
}

// ---- skill ----------------------------------------------------------------

/// `<skills_dir>/brain-wiki/SKILL.md`
pub fn skill_file(skills_dir: &Path) -> PathBuf {
    skills_dir.join(SKILL_NAME).join(SKILL_FILE)
}

/// `skill_md` with BRAIN's marker comment inserted right after its
/// frontmatter (at the top when it has none).
fn marked_skill(skill_md: &str, version: &str) -> String {
    let marker = format!("{SKILL_MARKER_PREFIX}{version} -->{}", newline_of(skill_md));
    let insert_at = skill_md
        .strip_prefix("---")
        .and_then(|rest| rest.find("\n---"))
        .map(|close| {
            let close_line = 3 + close + 1;
            skill_md[close_line..]
                .find('\n')
                .map_or(skill_md.len(), |eol| close_line + eol + 1)
        });
    match insert_at {
        Some(at) => format!("{}{marker}{}", &skill_md[..at], &skill_md[at..]),
        None => format!("{marker}{skill_md}"),
    }
}

/// The version of BRAIN's marker in a skill file's text.
pub fn skill_version(text: &str) -> Option<String> {
    let start = text.find(SKILL_MARKER_PREFIX)? + SKILL_MARKER_PREFIX.len();
    let close = text[start..].find(" -->")?;
    let version = &text[start..start + close];
    (!version.is_empty() && !version.contains(['\n', '\r'])).then(|| version.to_string())
}

/// What [`install_skill`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOutcome {
    Written,
    /// A `brain-wiki` folder BRAIN did not write — left alone.
    Foreign,
}

/// Write `<skills_dir>/brain-wiki/SKILL.md` (with BRAIN's marker) — only
/// when the folder does not exist or is empty, or the existing SKILL.md
/// carries BRAIN's marker. Anything else is [`SkillOutcome::Foreign`] and
/// is not touched.
pub fn install_skill(skills_dir: &Path, skill_md: &str, version: &str) -> io::Result<SkillOutcome> {
    let file = skill_file(skills_dir);
    match read_text(&file) {
        Ok(Some(text)) if skill_version(&text).is_none() => return Ok(SkillOutcome::Foreign),
        Ok(Some(_)) => {}
        Ok(None) => {
            if folder_has_entries(&skills_dir.join(SKILL_NAME))? {
                return Ok(SkillOutcome::Foreign);
            }
        }
        // Unreadable (e.g. not UTF-8): not ours to overwrite.
        Err(err) if err.kind() == io::ErrorKind::InvalidData => return Ok(SkillOutcome::Foreign),
        Err(err) => return Err(err),
    }
    std::fs::create_dir_all(skills_dir.join(SKILL_NAME))?;
    crate::fsutil::atomic_write(&file, marked_skill(skill_md, version).as_bytes())?;
    Ok(SkillOutcome::Written)
}

fn folder_has_entries(dir: &Path) -> io::Result<bool> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Remove BRAIN's skill file (and the `brain-wiki` folder once it is
/// empty). A file without BRAIN's marker is left alone. Returns whether
/// a file was removed.
pub fn remove_skill(skills_dir: &Path) -> io::Result<bool> {
    let file = skill_file(skills_dir);
    match read_text(&file) {
        Ok(Some(text)) if skill_version(&text).is_some() => {
            std::fs::remove_file(&file)?;
            let folder = skills_dir.join(SKILL_NAME);
            if !folder_has_entries(&folder)? {
                std::fs::remove_dir(&folder)?;
            }
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::InvalidData => Ok(false),
        Err(err) => Err(err),
    }
}

// ---- status and switching -------------------------------------------------

fn versioned(found: String, current: &str) -> InstallStatus {
    if found == current {
        InstallStatus::Installed { version: found }
    } else {
        InstallStatus::Outdated { version: found }
    }
}

/// The current state of `target` against `current_version`.
pub fn status(paths: &ClientPaths, target: Target, current_version: &str) -> InstallStatus {
    if !paths.client_dir(target).is_dir() {
        return InstallStatus::TargetMissing;
    }
    if let Some(file) = paths.prompt_file(target) {
        let text = read_text(&file).ok().flatten().unwrap_or_default();
        return match find_block(&text, MEMORY_BLOCK_ID) {
            Ok(Some(span)) => versioned(span.version, current_version),
            Ok(None) => InstallStatus::NotInstalled,
            Err(DamagedMarkers) => InstallStatus::Damaged,
        };
    }
    let skills_dir = paths.skills_dir(target).unwrap_or_default();
    match read_text(&skill_file(&skills_dir)) {
        Ok(Some(text)) => match skill_version(&text) {
            Some(found) => versioned(found, current_version),
            None => InstallStatus::Foreign,
        },
        Ok(None) => match folder_has_entries(&skills_dir.join(SKILL_NAME)) {
            Ok(false) => InstallStatus::NotInstalled,
            _ => InstallStatus::Foreign,
        },
        Err(_) => InstallStatus::Foreign,
    }
}

/// Why a switch could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0} was not found on this computer ({1} does not exist) — install it first")]
    ClientMissing(&'static str, String),
    #[error(
        "{0} already holds a brain-wiki skill BRAIN did not write — BRAIN leaves it alone; \
         remove or rename that folder to let BRAIN install its skill"
    )]
    Foreign(String),
    #[error(
        "BRAIN's markers in {0} are damaged (a start or end marker without its partner, or more \
         than one block) — fix them by hand; BRAIN changes nothing in that file until then"
    )]
    Damaged(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl ClientInstallSettings {
    /// The user's switch for `target`.
    pub fn enabled(&self, target: Target) -> bool {
        match target {
            Target::ClaudeCodePrompt => self.claude_code_prompt,
            Target::ClaudeCodeSkill => self.claude_code_skill,
            Target::CodexPrompt => self.codex_prompt,
            Target::CodexSkill => self.codex_skill,
        }
    }

    fn set_enabled(&mut self, target: Target, on: bool) {
        match target {
            Target::ClaudeCodePrompt => self.claude_code_prompt = on,
            Target::ClaudeCodeSkill => self.claude_code_skill = on,
            Target::CodexPrompt => self.codex_prompt = on,
            Target::CodexSkill => self.codex_skill = on,
        }
    }

    /// BRAIN created the instruction file of `target` (so it may delete
    /// it again once BRAIN's block was the only content).
    fn created_file(&self, target: Target) -> bool {
        match target {
            Target::ClaudeCodePrompt => self.claude_code_prompt_created_file,
            Target::CodexPrompt => self.codex_prompt_created_file,
            _ => false,
        }
    }

    fn set_created_file(&mut self, target: Target, created: bool) {
        match target {
            Target::ClaudeCodePrompt => self.claude_code_prompt_created_file = created,
            Target::CodexPrompt => self.codex_prompt_created_file = created,
            _ => {}
        }
    }
}

/// Write the current block / skill of `target` (no switch bookkeeping).
/// Returns whether BRAIN created the instruction file.
fn write_target(paths: &ClientPaths, target: Target, version: &str) -> Result<bool, InstallError> {
    if let Some(file) = paths.prompt_file(target) {
        return Ok(upsert_marked_block(
            &file,
            MEMORY_BLOCK_ID,
            version,
            &prompt_body(),
        )?);
    }
    let skills_dir = paths.skills_dir(target).unwrap_or_default();
    match install_skill(&skills_dir, crate::onboarding::template::SKILL_MD, version)? {
        SkillOutcome::Written => Ok(false),
        SkillOutcome::Foreign => Err(InstallError::Foreign(
            skills_dir.join(SKILL_NAME).display().to_string(),
        )),
    }
}

/// Turn `target` on (install) or off (remove BRAIN's block / skill) and
/// record the switch in `settings`. Turning on needs the client's
/// directory and refuses a foreign skill folder; turning off never
/// touches anything BRAIN did not mark. Returns the new status.
pub fn set_enabled(
    paths: &ClientPaths,
    target: Target,
    enabled: bool,
    settings: &mut ClientInstallSettings,
    version: &str,
) -> Result<InstallStatus, InstallError> {
    if status(paths, target, version) == InstallStatus::Damaged {
        return Err(InstallError::Damaged(
            paths.file(target).display().to_string(),
        ));
    }
    if enabled {
        let client_dir = paths.client_dir(target);
        if !client_dir.is_dir() {
            return Err(InstallError::ClientMissing(
                target.client_name(),
                client_dir.display().to_string(),
            ));
        }
        let created = write_target(paths, target, version)?;
        if created {
            settings.set_created_file(target, true);
        }
    } else if let Some(file) = paths.prompt_file(target) {
        remove_marked_block(&file, MEMORY_BLOCK_ID, settings.created_file(target))?;
        settings.set_created_file(target, false);
    } else {
        remove_skill(&paths.skills_dir(target).unwrap_or_default())?;
    }
    settings.set_enabled(target, enabled);
    Ok(status(paths, target, version))
}

/// Re-install every switched-on target whose installed version differs
/// from `version` (after an app update). Targets that are switched off,
/// missing, foreign or not installed (removed by hand) are left alone.
pub fn refresh_outdated(
    paths: &ClientPaths,
    settings: &ClientInstallSettings,
    version: &str,
) -> Vec<(Target, Result<InstallStatus, InstallError>)> {
    Target::ALL
        .into_iter()
        .filter(|t| settings.enabled(*t))
        .filter(|t| matches!(status(paths, *t, version), InstallStatus::Outdated { .. }))
        .map(|t| {
            let result = write_target(paths, t, version).map(|_| status(paths, t, version));
            (t, result)
        })
        .collect()
}

/// App start: refresh outdated installs of the real user's clients and
/// log what happened. Never fails the start.
pub fn refresh_on_startup(settings: &ClientInstallSettings) {
    let Some(paths) = ClientPaths::resolve() else {
        return;
    };
    for (target, result) in refresh_outdated(&paths, settings, INSTALL_VERSION) {
        match result {
            Ok(_) => tracing::info!(
                ?target,
                version = INSTALL_VERSION,
                "client install refreshed to the current version"
            ),
            Err(err) => tracing::warn!(?target, %err, "client install refresh failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BLOCK: &str = "memory-prompt";

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn home() -> (TempDir, ClientPaths) {
        let tmp = TempDir::new().unwrap();
        let paths = ClientPaths::under_home(tmp.path(), None);
        (tmp, paths)
    }

    fn with_clients(paths: &ClientPaths) {
        std::fs::create_dir_all(&paths.claude_dir).unwrap();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
    }

    // ---- marked blocks ------------------------------------------------

    #[test]
    fn a_block_written_into_a_missing_file_is_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        upsert_marked_block(&file, BLOCK, "1.0.0", "Use BRAIN.").unwrap();
        assert_eq!(
            read(&file),
            "<!-- BRAIN:memory-prompt v1.0.0 -->\nUse BRAIN.\n<!-- /BRAIN:memory-prompt -->\n"
        );
    }

    #[test]
    fn writing_into_a_missing_file_reports_that_brain_created_it() {
        let tmp = TempDir::new().unwrap();
        let created = upsert_marked_block(&tmp.path().join("CLAUDE.md"), BLOCK, "1", "x").unwrap();
        assert!(created);
    }

    #[test]
    fn writing_into_an_existing_file_reports_that_brain_did_not_create_it() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "").unwrap();
        assert!(!upsert_marked_block(&file, BLOCK, "1", "x").unwrap());
    }

    #[test]
    fn a_block_written_into_an_empty_file_is_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "").unwrap();
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        assert_eq!(
            read(&file),
            "<!-- BRAIN:memory-prompt v1 -->\nx\n<!-- /BRAIN:memory-prompt -->\n"
        );
    }

    #[test]
    fn a_block_is_appended_after_one_blank_line_below_the_users_text() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "# Mine\nKeep me.\n").unwrap();
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        assert_eq!(
            read(&file),
            "# Mine\nKeep me.\n\n<!-- BRAIN:memory-prompt v1 -->\nx\n<!-- /BRAIN:memory-prompt -->\n"
        );
    }

    #[test]
    fn a_new_version_replaces_the_block_in_place_and_keeps_the_text_around_it() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(
            &file,
            "top\n\n<!-- BRAIN:memory-prompt v1 -->\nold\n<!-- /BRAIN:memory-prompt -->\n\nbottom\n",
        )
        .unwrap();
        upsert_marked_block(&file, BLOCK, "2", "new").unwrap();
        assert_eq!(
            read(&file),
            "top\n\n<!-- BRAIN:memory-prompt v2 -->\nnew\n<!-- /BRAIN:memory-prompt -->\n\nbottom\n"
        );
    }

    #[test]
    fn a_crlf_file_keeps_crlf_around_and_inside_the_replaced_block() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(
            &file,
            "top\r\n\r\n<!-- BRAIN:memory-prompt v1 -->\r\nold\r\n<!-- /BRAIN:memory-prompt -->\r\nbottom\r\n",
        )
        .unwrap();
        upsert_marked_block(&file, BLOCK, "2", "line one\nline two").unwrap();
        assert_eq!(
            read(&file),
            "top\r\n\r\n<!-- BRAIN:memory-prompt v2 -->\r\nline one\r\nline two\r\n<!-- /BRAIN:memory-prompt -->\r\nbottom\r\n"
        );
    }

    #[test]
    fn a_block_appended_to_a_crlf_file_uses_crlf() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "mine\r\n").unwrap();
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        assert_eq!(
            read(&file),
            "mine\r\n\r\n<!-- BRAIN:memory-prompt v1 -->\r\nx\r\n<!-- /BRAIN:memory-prompt -->\r\n"
        );
    }

    #[test]
    fn an_lf_file_stays_free_of_carriage_returns() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "mine\n").unwrap();
        upsert_marked_block(&file, BLOCK, "1", "a\r\nb").unwrap();
        assert!(!read(&file).contains('\r'));
    }

    #[test]
    fn installing_and_removing_gives_back_the_original_file_byte_for_byte() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        let original = "# Mine\r\n\r\nKeep me exactly.\r\n";
        std::fs::write(&file, original).unwrap();
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        remove_marked_block(&file, BLOCK, false).unwrap();
        assert_eq!(read(&file), original);
    }

    #[test]
    fn removing_a_block_from_the_middle_keeps_one_blank_line_between_the_neighbours() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(
            &file,
            "top\n\n<!-- BRAIN:memory-prompt v1 -->\nx\n<!-- /BRAIN:memory-prompt -->\n\nbottom\n",
        )
        .unwrap();
        remove_marked_block(&file, BLOCK, false).unwrap();
        assert_eq!(read(&file), "top\n\nbottom\n");
    }

    #[test]
    fn a_file_brain_created_is_deleted_when_its_block_is_removed() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        remove_marked_block(&file, BLOCK, true).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn a_file_the_user_created_stays_empty_when_its_block_is_removed() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "").unwrap();
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        remove_marked_block(&file, BLOCK, false).unwrap();
        assert_eq!(read(&file), "");
    }

    #[test]
    fn a_file_brain_created_but_the_user_extended_is_not_deleted() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        upsert_marked_block(&file, BLOCK, "1", "x").unwrap();
        let with_user_text = format!("{}\nMy own rule.\n", read(&file));
        std::fs::write(&file, with_user_text).unwrap();
        remove_marked_block(&file, BLOCK, true).unwrap();
        assert_eq!(read(&file), "\nMy own rule.\n");
    }

    #[test]
    fn removing_from_a_file_without_a_block_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "mine\n").unwrap();
        assert_eq!(
            remove_marked_block(&file, BLOCK, true).unwrap(),
            RemoveOutcome::NothingToRemove
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_not_rewritten() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
        assert!(upsert_marked_block(&file, BLOCK, "1", "x").is_err());
    }

    #[test]
    fn an_orphan_start_marker_followed_by_user_notes_blocks_the_rewrite() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        let original = "<!-- BRAIN:memory-prompt v1 -->\nold prompt\nMy own notes.\n";
        std::fs::write(&file, original).unwrap();
        let _ = upsert_marked_block(&file, BLOCK, "2", "new");
        assert_eq!(read(&file), original);
    }

    #[test]
    fn an_orphan_start_marker_before_a_fresh_block_blocks_the_rewrite() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        let original = "<!-- BRAIN:memory-prompt v1 -->\nMy notes.\n\n<!-- BRAIN:memory-prompt v1 -->\nx\n<!-- /BRAIN:memory-prompt -->\n";
        std::fs::write(&file, original).unwrap();
        let _ = upsert_marked_block(&file, BLOCK, "2", "new");
        assert_eq!(read(&file), original);
    }

    #[test]
    fn an_orphan_end_marker_is_reported_as_damaged() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "notes\n<!-- /BRAIN:memory-prompt -->\n").unwrap();
        assert!(upsert_marked_block(&file, BLOCK, "2", "new").is_err());
    }

    #[test]
    fn two_blocks_are_reported_as_damaged() {
        let block = "<!-- BRAIN:memory-prompt v1 -->\nx\n<!-- /BRAIN:memory-prompt -->\n";
        let text = format!("{block}\n{block}");
        assert_eq!(find_block(&text, BLOCK).err(), Some(DamagedMarkers));
    }

    #[test]
    fn a_marker_quoted_inside_user_text_is_not_a_marker() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(
            &file,
            "BRAIN wraps its prompt in `<!-- BRAIN:memory-prompt v1 -->` and `<!-- /BRAIN:memory-prompt -->`.\n",
        )
        .unwrap();
        upsert_marked_block(&file, BLOCK, "2", "new").unwrap();
        assert_eq!(block_version(&read(&file), BLOCK), Some("2".to_string()));
    }

    #[test]
    fn removing_from_a_damaged_file_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        let original = "<!-- BRAIN:memory-prompt v1 -->\nnotes\n";
        std::fs::write(&file, original).unwrap();
        let _ = remove_marked_block(&file, BLOCK, true);
        assert_eq!(read(&file), original);
    }

    #[test]
    fn a_damaged_instruction_file_shows_as_damaged() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = paths.prompt_file(Target::ClaudeCodePrompt).unwrap();
        std::fs::write(&file, "<!-- BRAIN:memory-prompt v1 -->\nnotes\n").unwrap();
        assert_eq!(
            status(&paths, Target::ClaudeCodePrompt, "1"),
            InstallStatus::Damaged
        );
    }

    #[test]
    fn switching_a_damaged_target_is_refused() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = paths.prompt_file(Target::CodexPrompt).unwrap();
        std::fs::write(&file, "notes\n<!-- /BRAIN:memory-prompt -->\n").unwrap();
        let mut settings = ClientInstallSettings::default();
        let result = set_enabled(&paths, Target::CodexPrompt, false, &mut settings, "1");
        assert!(matches!(result, Err(InstallError::Damaged(_))));
    }

    #[test]
    fn the_block_version_is_read_from_the_start_marker() {
        assert_eq!(
            block_version(
                "a\n<!-- BRAIN:memory-prompt v0.3.6 -->\nx\n<!-- /BRAIN:memory-prompt -->\n",
                BLOCK
            ),
            Some("0.3.6".to_string())
        );
    }

    // ---- skill ----------------------------------------------------------

    const SKILL: &str = "---\nname: brain-wiki\ndescription: d\n---\n\n# BRAIN wiki\n";

    #[test]
    fn the_installed_skill_carries_the_marker_right_after_its_frontmatter() {
        let tmp = TempDir::new().unwrap();
        install_skill(tmp.path(), SKILL, "0.3.6").unwrap();
        assert_eq!(
            read(&skill_file(tmp.path())),
            "---\nname: brain-wiki\ndescription: d\n---\n<!-- BRAIN:skill v0.3.6 -->\n\n# BRAIN wiki\n"
        );
    }

    #[test]
    fn a_foreign_brain_wiki_skill_is_not_overwritten() {
        let tmp = TempDir::new().unwrap();
        let file = skill_file(tmp.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "---\nname: brain-wiki\n---\nmine\n").unwrap();
        install_skill(tmp.path(), SKILL, "0.3.6").unwrap();
        assert_eq!(read(&file), "---\nname: brain-wiki\n---\nmine\n");
    }

    #[test]
    fn a_foreign_brain_wiki_skill_is_reported_as_foreign() {
        let tmp = TempDir::new().unwrap();
        let file = skill_file(tmp.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mine\n").unwrap();
        assert_eq!(
            install_skill(tmp.path(), SKILL, "1").unwrap(),
            SkillOutcome::Foreign
        );
    }

    #[test]
    fn a_brain_wiki_folder_with_other_files_but_no_skill_is_foreign() {
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join(SKILL_NAME);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("notes.txt"), "mine").unwrap();
        assert_eq!(
            install_skill(tmp.path(), SKILL, "1").unwrap(),
            SkillOutcome::Foreign
        );
    }

    #[test]
    fn removing_a_foreign_skill_leaves_it_in_place() {
        let tmp = TempDir::new().unwrap();
        let file = skill_file(tmp.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mine\n").unwrap();
        remove_skill(tmp.path()).unwrap();
        assert!(file.exists());
    }

    #[test]
    fn removing_brains_skill_removes_its_folder() {
        let tmp = TempDir::new().unwrap();
        install_skill(tmp.path(), SKILL, "1").unwrap();
        remove_skill(tmp.path()).unwrap();
        assert!(!tmp.path().join(SKILL_NAME).exists());
    }

    #[test]
    fn an_older_brain_skill_is_replaced_by_the_new_version() {
        let tmp = TempDir::new().unwrap();
        install_skill(tmp.path(), SKILL, "0.3.5").unwrap();
        install_skill(tmp.path(), SKILL, "0.3.6").unwrap();
        assert_eq!(
            skill_version(&read(&skill_file(tmp.path()))),
            Some("0.3.6".to_string())
        );
    }

    #[test]
    fn the_bundled_skill_template_gets_its_marker_after_the_frontmatter() {
        let marked = marked_skill(crate::onboarding::template::SKILL_MD, "9.9.9");
        let lines: Vec<&str> = marked.lines().collect();
        let close = 1 + lines[1..].iter().position(|l| *l == "---").unwrap();
        assert_eq!(lines[close + 1], "<!-- BRAIN:skill v9.9.9 -->");
    }

    // ---- status and switching -------------------------------------------

    #[test]
    fn every_target_is_missing_when_no_client_directory_exists() {
        let (_tmp, paths) = home();
        let all: Vec<InstallStatus> = Target::ALL
            .iter()
            .map(|t| status(&paths, *t, "1"))
            .collect();
        assert_eq!(all, vec![InstallStatus::TargetMissing; 4]);
    }

    #[test]
    fn an_existing_client_without_brain_files_is_not_installed() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let all: Vec<InstallStatus> = Target::ALL
            .iter()
            .map(|t| status(&paths, *t, "1"))
            .collect();
        assert_eq!(all, vec![InstallStatus::NotInstalled; 4]);
    }

    #[test]
    fn switching_a_target_on_makes_it_installed_in_the_current_version() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let mut settings = ClientInstallSettings::default();
        let results: Vec<InstallStatus> = Target::ALL
            .iter()
            .map(|t| set_enabled(&paths, *t, true, &mut settings, "0.3.6").unwrap())
            .collect();
        assert_eq!(
            results,
            vec![
                InstallStatus::Installed {
                    version: "0.3.6".into()
                };
                4
            ]
        );
    }

    #[test]
    fn a_version_bump_turns_an_installed_target_outdated() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let mut settings = ClientInstallSettings::default();
        set_enabled(&paths, Target::CodexPrompt, true, &mut settings, "0.3.6").unwrap();
        assert_eq!(
            status(&paths, Target::CodexPrompt, "0.3.7"),
            InstallStatus::Outdated {
                version: "0.3.6".into()
            }
        );
    }

    #[test]
    fn refreshing_brings_an_enabled_outdated_target_to_the_current_version() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let mut settings = ClientInstallSettings::default();
        set_enabled(
            &paths,
            Target::ClaudeCodeSkill,
            true,
            &mut settings,
            "0.3.6",
        )
        .unwrap();
        refresh_outdated(&paths, &settings, "0.3.7");
        assert_eq!(
            status(&paths, Target::ClaudeCodeSkill, "0.3.7"),
            InstallStatus::Installed {
                version: "0.3.7".into()
            }
        );
    }

    #[test]
    fn refreshing_leaves_a_switched_off_outdated_block_alone() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = paths.prompt_file(Target::ClaudeCodePrompt).unwrap();
        upsert_marked_block(&file, MEMORY_BLOCK_ID, "0.3.6", "x").unwrap();
        refresh_outdated(&paths, &ClientInstallSettings::default(), "0.3.7");
        assert_eq!(
            block_version(&read(&file), MEMORY_BLOCK_ID),
            Some("0.3.6".into())
        );
    }

    #[test]
    fn switching_a_prompt_off_removes_the_block_and_the_file_brain_created() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let mut settings = ClientInstallSettings::default();
        set_enabled(&paths, Target::ClaudeCodePrompt, true, &mut settings, "1").unwrap();
        set_enabled(&paths, Target::ClaudeCodePrompt, false, &mut settings, "1").unwrap();
        assert!(
            !paths
                .prompt_file(Target::ClaudeCodePrompt)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn switching_a_prompt_off_keeps_the_users_own_instructions() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = paths.prompt_file(Target::CodexPrompt).unwrap();
        std::fs::write(&file, "My Codex rules.\n").unwrap();
        let mut settings = ClientInstallSettings::default();
        set_enabled(&paths, Target::CodexPrompt, true, &mut settings, "1").unwrap();
        set_enabled(&paths, Target::CodexPrompt, false, &mut settings, "1").unwrap();
        assert_eq!(read(&file), "My Codex rules.\n");
    }

    #[test]
    fn switching_on_without_the_client_directory_is_refused() {
        let (_tmp, paths) = home();
        let mut settings = ClientInstallSettings::default();
        let result = set_enabled(&paths, Target::ClaudeCodePrompt, true, &mut settings, "1");
        assert!(matches!(result, Err(InstallError::ClientMissing(..))));
    }

    #[test]
    fn switching_on_over_a_foreign_skill_is_refused() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = skill_file(&paths.skills_dir(Target::ClaudeCodeSkill).unwrap());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mine\n").unwrap();
        let mut settings = ClientInstallSettings::default();
        let result = set_enabled(&paths, Target::ClaudeCodeSkill, true, &mut settings, "1");
        assert!(matches!(result, Err(InstallError::Foreign(_))));
    }

    #[test]
    fn a_foreign_skill_folder_shows_as_foreign() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let file = skill_file(&paths.skills_dir(Target::CodexSkill).unwrap());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mine\n").unwrap();
        assert_eq!(
            status(&paths, Target::CodexSkill, "1"),
            InstallStatus::Foreign
        );
    }

    #[test]
    fn the_codex_skill_goes_under_the_agents_skills_directory() {
        let tmp = TempDir::new().unwrap();
        let paths = ClientPaths::under_home(tmp.path(), None);
        assert_eq!(
            paths.file(Target::CodexSkill),
            tmp.path()
                .join(".agents")
                .join("skills")
                .join("brain-wiki")
                .join("SKILL.md")
        );
    }

    #[test]
    fn codex_home_overrides_the_codex_prompt_location() {
        let tmp = TempDir::new().unwrap();
        let custom = tmp.path().join("codex-elsewhere");
        let paths = ClientPaths::under_home(tmp.path(), Some(custom.clone()));
        assert_eq!(paths.file(Target::CodexPrompt), custom.join("AGENTS.md"));
    }

    #[test]
    fn the_installed_prompt_points_to_the_skill_and_the_agents_resource() {
        let body = prompt_body();
        assert!(
            body.contains("`brain-wiki` skill") && body.contains("brain://agents-md"),
            "{body}"
        );
    }

    #[test]
    fn switching_on_records_the_switch_in_the_settings() {
        let (_tmp, paths) = home();
        with_clients(&paths);
        let mut settings = ClientInstallSettings::default();
        set_enabled(&paths, Target::CodexSkill, true, &mut settings, "1").unwrap();
        assert!(settings.enabled(Target::CodexSkill));
    }
}

import { invoke } from "@tauri-apps/api/core";

export type DiskInfo = {
  id: string;
  name: string;
  size_bytes: number;
  filesystem: string | null;
  volume_label: string | null;
  is_system: boolean;
  is_removable: boolean;
  mount_path: string | null;
};

export type VaultMarker = {
  format: string;
  vault_id: string;
  created_at: string;
  client_version: string;
  encryption: { scheme: string; params: Record<string, unknown> };
  embedding_model: string;
};

export type MountState = "disconnected" | "mounting" | "mounted-idle" | "mounted-busy" | "error";

export type TrayStatus = {
  state: MountState;
  tooltip: string;
  vault_path: string | null;
  active_operations: number;
  message: string | null;
};

export type OnboardingProgress = {
  step: string;
  percent: number;
  detail: string;
};

export type FormatDiskResult = { mount_path: string };

export type McpCommandHint = {
  command: string;
  args: string[];
  env_var: string;
  vault_path: string | null;
  claude_code_config_path: string | null;
  claude_cli_available: boolean;
};

export type ClientStatus =
  | { kind: "Registered"; detail: string }
  | { kind: "NotInstalled" }
  | { kind: "Failed"; detail: string };

export type RegistrationReport = {
  claude_code: ClientStatus | null;
  claude_desktop: ClientStatus | null;
  codex: ClientStatus | null;
  continue_dev: ClientStatus | null;
  // ChatGPT Desktop intentionally omitted — its current Windows Store
  // and macOS clients only support remote HTTPS connectors registered
  // server-side; there is no local-stdio config file we can write to.
  // Keeping the row in the UI was misleading because the "Registered"
  // status meant we wrote a file ChatGPT never reads.
};

export const commands = {
  listDisks: () => invoke<DiskInfo[]>("list_disks"),
  formatDisk: (diskId: string) => invoke<FormatDiskResult>("format_disk", { diskId }),
  initVault: (path: string) => invoke<VaultMarker>("init_vault", { path }),
  populateTemplate: (path: string) => invoke<void>("populate_template", { path }),
  downloadEmbeddingModel: (path: string) =>
    invoke<void>("download_embedding_model", { path }),
  readMarker: (path: string) => invoke<VaultMarker | null>("read_marker", { path }),
  detectExistingVault: (path: string) => invoke<boolean>("detect_existing_vault", { path }),
  finishOnboarding: (path: string) => invoke<void>("finish_onboarding", { path }),
  bootstrapApp: () =>
    invoke<{
      auto_mounted: boolean;
      vault_path: string | null;
      last_known_vault_missing: boolean;
    }>("bootstrap_app"),
  resetBrain: () => invoke<void>("reset_brain"),
  /// Refreshes the bundled AGENTS.md / CLAUDE.md in the mounted
  /// vault's 00_meta/ from the binary's embedded copies. Returns one
  /// entry per template file describing whether it was created,
  /// overwritten or already up-to-date. `.mcp.json` is intentionally
  /// NOT touched so the user's bearer token + external MCP servers
  /// survive. Listed under Danger in Settings because it's
  /// destructive of any local AGENTS.md / CLAUDE.md edits.
  updateVaultTemplates: () =>
    invoke<
      Array<{
        path: string;
        action: "created" | "overwritten" | "unchanged";
        size_before: number;
        size_after: number;
      }>
    >("update_vault_templates"),
  refreshDisks: () => invoke<DiskInfo[]>("refresh_disks"),
  brainMcpCommandHint: () => invoke<McpCommandHint>("brain_mcp_command_hint"),
  reregisterMcp: () => invoke<RegistrationReport>("reregister_mcp"),
  lastMcpRegistrationReport: () =>
    invoke<RegistrationReport | null>("last_mcp_registration_report"),
  brainMemorySystemPrompt: () => invoke<string>("brain_memory_system_prompt"),
  openPageInExternalEditor: (id: string) =>
    invoke<void>("open_page_in_external_editor", { id }),
  rebuildIndex: () => invoke<number>("rebuild_index"),
  queryPages: (query: string) =>
    invoke<
      Array<{
        id: string;
        type: string;
        path: string;
        title: string;
        updated_at: string | null;
      }>
    >("query_pages", { query }),

  trayStatus: () => invoke<TrayStatus>("tray_status"),
  ejectBrain: (force: boolean) => invoke<void>("eject_brain", { force }),

  listWikiTree: () =>
    invoke<{ entities: string[]; concepts: string[]; sources: string[]; topics: string[] }>(
      "list_wiki_tree",
    ),
  readPage: (id: string) =>
    invoke<{ id: string; title: string; frontmatter: string; body: string }>("read_page", { id }),
  searchPages: (query: string) =>
    invoke<
      Array<{ id: string; title: string; path: string; snippet: string; score: number }>
    >("search_pages", { query }),
  getBacklinks: (id: string) =>
    invoke<Array<{ id: string; title: string; path: string }>>("get_backlinks", { id }),
  getGraph: (filters: {
    types?: string[];
    tags?: string[];
    updated_after?: string | null;
  }) =>
    invoke<{
      nodes: Array<{ id: string; type: string; title: string; tags: string[] }>;
      edges: Array<{ source: string; target: string }>;
    }>("get_graph", {
      filters: {
        types: filters.types ?? null,
        tags: filters.tags ?? null,
        updated_after: filters.updated_after ?? null,
      },
    }),

  loadGraphPositions: () =>
    invoke<Array<{ page_id: string; x: number; y: number }>>(
      "load_graph_positions",
    ),
  saveGraphPositions: (
    positions: Array<{ page_id: string; x: number; y: number }>,
  ) => invoke<void>("save_graph_positions", { positions }),
  clearGraphPositions: () => invoke<void>("clear_graph_positions"),

  wikiHistory: (limit: number) =>
    invoke<Array<{ sha: string; ts: string; message: string; files_changed: number }>>(
      "wiki_history",
      { limit },
    ),
  wikiCommitDetail: (sha: string) =>
    invoke<{
      sha: string;
      ts: string;
      author: string;
      message: string;
      parent_sha: string | null;
      files: Array<{
        path: string;
        status: "A" | "M" | "D" | "R" | "C" | "?";
        insertions: number;
        deletions: number;
      }>;
      patch: string;
    }>("wiki_commit_detail", { sha }),
  wikiRestorePage: (sha: string, page: string) =>
    invoke<void>("wiki_restore_page", { sha, page }),
  wikiHardReset: (sha: string) => invoke<void>("wiki_hard_reset", { sha }),

  checkUpdate: () =>
    invoke<{ available: boolean; version: string | null; notes: string | null }>("check_update"),
  applyUpdate: () => invoke<void>("apply_update"),
  skipUpdate: (version: string) => invoke<void>("skip_update", { version }),
};

import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  commands,
  type ClientStatus,
  type McpCommandHint,
  type RegistrationReport,
  type RemoteStatus,
} from "../../lib/commands";
import { Tabs } from "../../components/ui/Tabs";
import { Button } from "../../components/ui/Button";
import { Card, CardDescription, CardHeader, CardTitle } from "../../components/ui/Card";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog";
import { ErrorBanner } from "../../components/ui/ErrorBanner";
import { useAsyncAction } from "../../components/ui/useAsyncAction";
import { useToast } from "../../components/ui/toast-context";
import { useDataRefresh } from "../../lib/events";

const TABS = [
  { id: "general", label: "General" },
  { id: "mcp", label: "MCP & Clients" },
  { id: "memory", label: "Memory mode" },
  { id: "git", label: "Git sync" },
  { id: "danger", label: "Danger zone" },
];

export function Settings() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const active = params.get("tab") ?? "general";
  const setActive = (id: string) => setParams({ tab: id }, { replace: true });

  const [hint, setHint] = useState<McpCommandHint | null>(null);
  const [report, setReport] = useState<RegistrationReport | null>(null);
  const [memoryPrompt, setMemoryPrompt] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const { push } = useToast();
  // Confirmation dialog state for the two destructive operations in the
  // Danger zone. Drives the in-UI <ConfirmDialog>. Replaces the pre-
  // 0.2.17 use of `window.confirm()` which: (a) some Tauri builds
  // silently suppress, (b) can't carry rich body content like a list
  // of files-to-overwrite, (c) doesn't match the BRAIN visual style.
  const [confirmReset, setConfirmReset] = useState(false);
  const [confirmUpdateTemplates, setConfirmUpdateTemplates] = useState(false);

  useEffect(() => {
    let cancelled = false;
    Promise.all([
      commands.brainMcpCommandHint(),
      commands.lastMcpRegistrationReport(),
      commands.brainMemorySystemPrompt(),
    ])
      .then(([h, r, prompt]) => {
        if (cancelled) return;
        setHint(h);
        if (r) setReport(r);
        setMemoryPrompt(prompt);
      })
      .catch((e: unknown) => !cancelled && setError(String(e)))
      .finally(() => !cancelled && setLoading(false));
    let unlisten: UnlistenFn | undefined;
    listen<RegistrationReport>("mcp-registration-status", (event) => {
      if (!cancelled) setReport(event.payload);
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Live-update: re-pull the MCP command hint + the latest registration
  // report on disk reconnect. The hint includes the `claude_cli_available`
  // flag — without this hook the user would see stale "Claude CLI not
  // detected" copy after they install Claude Code while BRAIN is open.
  useDataRefresh(() => {
    void commands.brainMcpCommandHint().then(setHint).catch(() => {});
    void commands
      .lastMcpRegistrationReport()
      .then((r) => r && setReport(r))
      .catch(() => {});
  });

  const reregisterAction = useAsyncAction(
    async () => {
      const r = await commands.reregisterMcp();
      setReport(r);
      return r;
    },
    {
      pending: "Re-registering BRAIN in MCP clients…",
      success: "MCP re-registered",
      errorPrefix: "MCP re-registration failed",
    },
  );

  function copy(text: string) {
    void navigator.clipboard.writeText(text);
    push({ kind: "success", message: "Copied to clipboard" });
  }

  const resetAction = useAsyncAction(
    async () => {
      await commands.resetBrain();
      navigate("/onboarding", { replace: true });
    },
    {
      pending: "Ejecting and unregistering…",
      success: "BRAIN reset",
      errorPrefix: "Reset failed",
    },
  );

  function resetBrain() {
    setConfirmReset(true);
  }

  async function confirmResetAccepted() {
    try {
      await resetAction.trigger();
    } finally {
      setConfirmReset(false);
    }
  }

  // Refresh the bundled AGENTS.md / CLAUDE.md in the vault's 00_meta/
  // from the current binary. The action pushes its own success toast
  // with a per-file delta in the `detail` field — important here
  // because the user is overwriting potentially-edited files and
  // deserves to see exactly which files changed and by how many
  // bytes, not a generic "done". We bypass useAsyncAction's static
  // `success` string for that reason: there's no way to feed a
  // dynamic value through that field, and the pre-0.2.17 version
  // rendered a content-less toast which the user (rightly) reported
  // as missing feedback.
  const updateTemplatesAction = useAsyncAction(
    async () => {
      const report = await commands.updateVaultTemplates();
      const detail = report
        .map((e) => {
          const file = e.path.split(/[\\/]/).pop() ?? e.path;
          if (e.action === "unchanged") return `${file}: unchanged`;
          return `${file}: ${e.action} (${e.size_before} → ${e.size_after} B)`;
        })
        .join("\n");
      const changedCount = report.filter((e) => e.action !== "unchanged").length;
      const message =
        changedCount === 0
          ? "Vault templates already up-to-date"
          : `Vault templates updated · ${changedCount} file${
              changedCount === 1 ? "" : "s"
            } changed`;
      push({ kind: "success", message, detail });
      return report;
    },
    {
      pending: "Refreshing vault templates…",
      // success deliberately omitted — pushed manually above with the
      // dynamic per-file delta in the detail field.
      errorPrefix: "Update failed",
    },
  );

  function updateVaultTemplates() {
    setConfirmUpdateTemplates(true);
  }

  async function confirmUpdateTemplatesAccepted() {
    try {
      await updateTemplatesAction.trigger();
    } finally {
      setConfirmUpdateTemplates(false);
    }
  }

  const mcpJsonSnippet = hint
    ? JSON.stringify(
        {
          mcpServers: {
            brain: {
              command: hint.command,
              args: hint.args,
              env: hint.vault_path
                ? { [hint.env_var]: hint.vault_path }
                : { [hint.env_var]: "/path/to/your/brain" },
            },
          },
        },
        null,
        2,
      )
    : "";

  const cliCommand = hint
    ? `claude mcp add --scope user --env=${hint.env_var}=${hint.vault_path ?? "/path/to/your/brain"} -- brain "${hint.command}" mcp`
    : "";

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex items-center justify-between border-b border-neutral-800 px-6 pt-4">
        <h1 className="text-2xl font-semibold">Settings</h1>
      </header>
      <Tabs tabs={TABS} active={active} onChange={setActive} className="px-6" />

      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto max-w-4xl space-y-6">
          {error && <ErrorBanner tone="error">{error}</ErrorBanner>}
          {loading && <p className="text-sm text-neutral-500">Loading…</p>}

          {active === "general" && <GeneralTab />}
          {active === "mcp" && (
            <McpTab
              hint={hint}
              report={report}
              cliCommand={cliCommand}
              jsonSnippet={mcpJsonSnippet}
              copy={copy}
              reregister={() => void reregisterAction.trigger()}
              reregistering={reregisterAction.loading}
            />
          )}
          {active === "memory" && <MemoryTab prompt={memoryPrompt} copy={copy} />}
          {active === "git" && <GitSyncTab />}
          {active === "danger" && (
            <DangerTab
              onReset={resetBrain}
              resetting={resetAction.loading}
              onUpdateTemplates={updateVaultTemplates}
              updatingTemplates={updateTemplatesAction.loading}
            />
          )}
        </div>
      </div>
      {/*
        Confirmation dialogs for the Danger zone. Rendered at the route
        root so the overlay covers the whole settings surface, not just
        the panel column. They sit outside the scrollable container on
        purpose — a tall page that's been scrolled should not be able
        to push the dialog out of view.
      */}
      <ConfirmDialog
        open={confirmUpdateTemplates}
        title="Update vault templates"
        confirmLabel="Overwrite templates"
        tone="destructive"
        loading={updateTemplatesAction.loading}
        onConfirm={() => void confirmUpdateTemplatesAccepted()}
        onCancel={() => setConfirmUpdateTemplates(false)}
      >
        <p>
          BRAIN will overwrite the following files in your vault's{" "}
          <code className="font-mono">00_meta/</code> folder with the versions
          bundled in the current release:
        </p>
        <ul className="ml-4 list-disc space-y-0.5">
          <li>
            <code className="font-mono">AGENTS.md</code> — conventions for LLM
            agents
          </li>
          <li>
            <code className="font-mono">CLAUDE.md</code> — companion file for
            Claude Code
          </li>
        </ul>
        <p>
          <strong>Any local edits to those two files will be lost.</strong>{" "}
          Your <code className="font-mono">.mcp.json</code> (bearer token, MCP
          server config) is <strong>not</strong> touched, and no wiki page is
          touched either.
        </p>
      </ConfirmDialog>
      <ConfirmDialog
        open={confirmReset}
        title="Reset BRAIN"
        confirmLabel="Reset BRAIN"
        tone="destructive"
        loading={resetAction.loading}
        onConfirm={() => void confirmResetAccepted()}
        onCancel={() => setConfirmReset(false)}
      >
        <p>BRAIN will:</p>
        <ul className="ml-4 list-disc space-y-0.5">
          <li>Eject the current vault (unmount)</li>
          <li>Forget the saved vault path</li>
          <li>Unregister BRAIN from every LLM client</li>
          <li>Send you back to the onboarding wizard</li>
        </ul>
        <p className="text-neutral-400">
          Your data on disk is <strong>not</strong> deleted.
        </p>
      </ConfirmDialog>
    </div>
  );
}

function GeneralTab() {
  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div>
            <CardTitle>About BRAIN</CardTitle>
            <CardDescription>
              BRAIN runs in the system tray. Closing this window only hides it —
              the tray keeps MCP available to your LLM clients.
            </CardDescription>
          </div>
        </CardHeader>
        <ul className="mt-2 space-y-1 text-sm text-neutral-400">
          <li>
            <strong className="text-neutral-200">Tray menu:</strong> Open window ·
            BRAIN Viewer · Wiki history · Settings · Re-register MCP · Eject BRAIN
          </li>
          <li>
            <strong className="text-neutral-200">Keyboard:</strong> Ctrl+1 (Browse) · Ctrl+2 (Search) ·
            Ctrl+3 (Graph) · Ctrl+H (History) · Ctrl+, (Settings) · Ctrl+K (Quick search)
          </li>
          <li>
            <strong className="text-neutral-200">Status colours:</strong>{" "}
            <span className="text-emerald-300">green</span> = ready,{" "}
            <span className="text-amber-300">yellow</span> = busy,{" "}
            <span className="text-red-300">red</span> = error
          </li>
        </ul>
      </Card>
      <AutostartCard />
      <RebuildIndexCard />
    </div>
  );
}

function RebuildIndexCard() {
  const action = useAsyncAction(
    async () => {
      const count = await commands.rebuildIndex();
      return count;
    },
    {
      pending: "Rebuilding search index…",
      success: "Search index rebuilt",
      errorPrefix: "Rebuild failed",
    },
  );

  return (
    <Card>
      <CardHeader>
        <div>
          <CardTitle>Rebuild search index</CardTitle>
          <CardDescription>
            Walks every page in <code>02_wiki/</code> and rewrites the SQLite
            index — full-text rows, the wiki-link graph, and (if bge-m3 is
            installed) all chunk embeddings. Run this if pages were edited
            outside the watcher, if the graph view shows missing
            connections, or after a BRAIN upgrade that changes how page
            content is parsed. Safe to run anytime; takes a few seconds per
            hundred pages.
          </CardDescription>
        </div>
        <Button
          variant="primary"
          size="sm"
          loading={action.loading}
          onClick={() => void action.trigger()}
        >
          {action.loading ? "Rebuilding…" : "Rebuild index"}
        </Button>
      </CardHeader>
    </Card>
  );
}

const INPUT_CLASS =
  "h-9 flex-1 rounded-md border border-neutral-800 bg-neutral-900 px-3 text-sm placeholder:text-neutral-600 focus:border-emerald-700 focus:outline-none";

function GitSyncTab() {
  const { push } = useToast();
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [url, setUrl] = useState("");
  const [pat, setPat] = useState("");
  const [conflicts, setConflicts] = useState<string[]>([]);
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);

  const refreshStatus = useCallback(async () => {
    try {
      const s = await commands.gitRemoteStatus();
      setStatus(s);
      setUrl((prev) => (prev.length === 0 ? (s.remote_url ?? "") : prev));
    } catch {
      // No vault mounted / not a git repo yet — leave the panel empty.
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus]);

  const enableAction = useAsyncAction(
    async () => {
      const res = await commands.enableVaultEncryption();
      setRecoveryKey(res.recovery_key);
      await refreshStatus();
    },
    { pending: "Enabling encryption…", errorPrefix: "Could not enable encryption" },
  );

  const saveRemote = useAsyncAction(
    async () => {
      await commands.setGitRemote(url.trim());
      await refreshStatus();
    },
    { pending: "Saving remote…", success: "Remote saved", errorPrefix: "Could not save remote" },
  );

  const saveCred = useAsyncAction(
    async () => {
      await commands.setGitCredential(pat);
      setPat("");
      await refreshStatus();
    },
    {
      pending: "Storing token…",
      success: "Access token stored in the OS keychain",
      errorPrefix: "Could not store token",
    },
  );

  const syncAction = useAsyncAction(
    async () => {
      const report = await commands.syncNow();
      setConflicts(report.conflicted_pages);
      const label =
        report.outcome === "up-to-date"
          ? "Already up to date"
          : report.outcome === "fast-forward"
            ? "Pulled latest changes"
            : "Merged remote changes";
      if (report.conflicted_pages.length > 0) {
        push({
          kind: "warning",
          message: `${label} — ${report.conflicted_pages.length} page(s) need conflict resolution`,
          detail: report.conflicted_pages.join(", "),
        });
      } else {
        push({ kind: "success", message: label });
      }
    },
    { pending: "Syncing…", errorPrefix: "Sync failed" },
  );

  return (
    <div className="space-y-6">
      {status && !status.encrypted && (
        <Card tone="warning">
          <CardHeader>
            <div>
              <CardTitle>Enable content encryption</CardTitle>
              <CardDescription>
                Encrypts page content and replaces filenames with opaque
                tokens, so a copy of the repo reveals neither content nor
                names. Required before a network remote can be attached. You'll
                get a recovery key — save it in your password manager;{" "}
                <strong>losing it makes any pushed copy unreadable.</strong>
              </CardDescription>
            </div>
            <Button
              variant="primary"
              size="sm"
              loading={enableAction.loading}
              onClick={() => void enableAction.trigger()}
            >
              {enableAction.loading ? "Encrypting…" : "Enable encryption"}
            </Button>
          </CardHeader>
        </Card>
      )}

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Remote repository</CardTitle>
            <CardDescription>
              The Git remote (e.g. a private GitHub repo) BRAIN pushes to and
              pulls from. A network remote requires an encrypted vault.
            </CardDescription>
          </div>
        </CardHeader>
        <div className="mt-3 flex gap-2">
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://github.com/you/brain.git"
            className={INPUT_CLASS}
          />
          <Button
            variant="secondary"
            size="sm"
            loading={saveRemote.loading}
            onClick={() => void saveRemote.trigger()}
          >
            Save remote
          </Button>
        </div>
        {status?.remote_url && (
          <p className="mt-2 text-xs text-neutral-500">
            Current: <code className="font-mono">{status.remote_url}</code>
          </p>
        )}
      </Card>

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Access token</CardTitle>
            <CardDescription>
              A personal access token for the remote, stored only in your OS
              keychain — never in the repo.{" "}
              {status?.has_credential
                ? "A token is currently stored on this machine."
                : "No token stored on this machine yet."}
            </CardDescription>
          </div>
        </CardHeader>
        <div className="mt-3 flex gap-2">
          <input
            type="password"
            value={pat}
            onChange={(e) => setPat(e.target.value)}
            placeholder="Personal access token"
            className={INPUT_CLASS}
          />
          <Button
            variant="secondary"
            size="sm"
            loading={saveCred.loading}
            disabled={pat.length === 0}
            onClick={() => void saveCred.trigger()}
          >
            Store token
          </Button>
        </div>
      </Card>

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Sync now</CardTitle>
            <CardDescription>
              Fetch, merge, and push. Edits to different pages merge
              automatically; a page changed on two machines gets conflict
              markers you resolve in the editor, then sync again.
            </CardDescription>
          </div>
          <Button
            variant="primary"
            size="sm"
            loading={syncAction.loading}
            onClick={() => void syncAction.trigger()}
          >
            {syncAction.loading ? "Syncing…" : "Sync now"}
          </Button>
        </CardHeader>
        {conflicts.length > 0 && (
          <div className="mt-3">
            <ErrorBanner tone="warning">
              These pages have conflict markers to resolve, then sync again:
              <ul className="ml-4 mt-1 list-disc">
                {conflicts.map((p) => (
                  <li key={p}>
                    <code className="font-mono">{p}</code>
                  </li>
                ))}
              </ul>
            </ErrorBanner>
          </div>
        )}
      </Card>

      <ConfirmDialog
        open={recoveryKey !== null}
        title="Save your recovery key"
        confirmLabel="I've saved it"
        tone="destructive"
        onConfirm={() => setRecoveryKey(null)}
        onCancel={() => setRecoveryKey(null)}
      >
        <p>
          Store this in your password manager <strong>now</strong> — it is
          shown only once. Without it, any pushed or backed-up copy of this
          vault is <strong>permanently unreadable</strong>, and you cannot open
          the vault on another machine.
        </p>
        <pre className="mt-2 select-all overflow-x-auto rounded-md border border-neutral-800 bg-neutral-900 p-3 font-mono text-sm">
          {recoveryKey}
        </pre>
      </ConfirmDialog>
    </div>
  );
}

function AutostartCard() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const { push } = useToast();

  useEffect(() => {
    void import("@tauri-apps/plugin-autostart").then(async ({ isEnabled }) => {
      try {
        setEnabled(await isEnabled());
      } catch {
        setEnabled(false);
      }
    });
  }, []);

  async function toggle(next: boolean) {
    setBusy(true);
    try {
      const { enable, disable } = await import("@tauri-apps/plugin-autostart");
      if (next) {
        await enable();
      } else {
        await disable();
      }
      setEnabled(next);
      push({
        kind: "success",
        message: next ? "Launch at login enabled" : "Launch at login disabled",
      });
    } catch (e: unknown) {
      push({
        kind: "error",
        message: "Could not change autostart",
        detail: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <div>
          <CardTitle>Launch BRAIN at login</CardTitle>
          <CardDescription>
            When enabled, BRAIN starts hidden in the tray as soon as you sign
            in to your OS — your LLM clients can reach the MCP server before
            you've opened the window. Uses the Windows registry, macOS
            LaunchAgents, or <code>~/.config/autostart/</code> on Linux.
          </CardDescription>
        </div>
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={!!enabled}
            disabled={enabled === null || busy}
            onChange={(e) => void toggle(e.target.checked)}
            className="size-4 accent-emerald-500"
          />
          <span className="text-sm text-neutral-300">
            {enabled === null
              ? "Loading…"
              : enabled
                ? "Enabled"
                : "Disabled"}
          </span>
        </label>
      </CardHeader>
    </Card>
  );
}

function McpTab({
  hint,
  report,
  cliCommand,
  jsonSnippet,
  copy,
  reregister,
  reregistering,
}: {
  hint: McpCommandHint | null;
  report: RegistrationReport | null;
  cliCommand: string;
  jsonSnippet: string;
  copy: (s: string) => void;
  reregister: () => void;
  reregistering: boolean;
}) {
  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div>
            <CardTitle>Auto-registration status</CardTitle>
            <CardDescription>
              BRAIN wrote its MCP entry into every detected client config when you
              finished onboarding (or the last time you clicked "Re-register MCP").
            </CardDescription>
          </div>
          <Button
            variant="primary"
            loading={reregistering}
            onClick={reregister}
            size="sm"
          >
            {reregistering ? "Re-registering…" : "Re-register MCP"}
          </Button>
        </CardHeader>
        {report && (
          <ul className="divide-y divide-neutral-800 rounded-md border border-neutral-800">
            <RegistrationRow label="Claude Code (CLI)" status={report.claude_code} />
            <RegistrationRow label="Claude Desktop (App)" status={report.claude_desktop} />
            <RegistrationRow label="Codex" status={report.codex} />
            <RegistrationRow label="Continue.dev" status={report.continue_dev} />
          </ul>
        )}
      </Card>

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Verify BRAIN in Claude Desktop</CardTitle>
            <CardDescription>
              Claude Desktop loads its config <strong>only at process start</strong>.
              Closing the window is not enough — fully quit and relaunch.
            </CardDescription>
          </div>
        </CardHeader>
        <ol className="ml-4 list-decimal space-y-1 text-sm text-neutral-300">
          <li>
            Right-click the Claude tray icon → <strong>Quit Claude</strong> (or kill via Task Manager)
          </li>
          <li>Start Claude Desktop fresh</li>
          <li>
            Open <strong>Settings → Developer</strong> (Ctrl+,) — the MCP Servers
            list must show <code>BRAIN</code>
          </li>
          <li>
            In a new chat, the <strong>tools / hammer icon</strong> at the bottom
            must list BRAIN's tools
          </li>
        </ol>
        <p className="mt-3 text-xs text-neutral-500">
          The "Connect apps" panel in Claude Desktop only shows <em>remote</em>{" "}
          connectors (M365, Atlassian, HubSpot…). Local MCP servers like BRAIN
          never appear there.
        </p>
      </Card>

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Manual setup snippet</CardTitle>
            <CardDescription>
              For Open WebUI or any other client without auto-registration, copy
              the JSON snippet or the CLI command.
            </CardDescription>
          </div>
        </CardHeader>
        {hint && (
          <div className="space-y-4">
            <div>
              <div className="mb-1 text-xs uppercase tracking-wider text-neutral-500">
                Claude Code CLI
              </div>
              <pre className="overflow-x-auto rounded-md bg-neutral-950 p-3 font-mono text-xs text-neutral-300">
                {cliCommand}
              </pre>
              <Button size="sm" className="mt-2" onClick={() => copy(cliCommand)}>
                Copy CLI command
              </Button>
            </div>
            <div>
              <div className="mb-1 text-xs uppercase tracking-wider text-neutral-500">
                JSON snippet
              </div>
              <pre className="overflow-x-auto whitespace-pre rounded-md bg-neutral-950 p-3 font-mono text-xs text-neutral-300">
                {jsonSnippet}
              </pre>
              <Button size="sm" className="mt-2" onClick={() => copy(jsonSnippet)}>
                Copy JSON
              </Button>
            </div>
            {hint.claude_code_config_path && (
              <p className="text-xs text-neutral-500">
                Claude Code config: <code>{hint.claude_code_config_path}</code>
              </p>
            )}
          </div>
        )}
      </Card>
    </div>
  );
}

function MemoryTab({ prompt, copy }: { prompt: string; copy: (s: string) => void }) {
  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div>
            <CardTitle>Use BRAIN as your LLM's memory</CardTitle>
            <CardDescription>
              Most LLM clients ship with their own memory feature (Claude
              Desktop's "Memory", ChatGPT's "memories", Codex/Continue's
              session state). Those memories don't follow you across
              clients and they aren't searchable, taggable, or
              git-versioned. Paste the system-prompt snippet below into
              your client of choice — the LLM then prefers BRAIN's{" "}
              <code className="font-mono">brain_write_page</code> /{" "}
              <code className="font-mono">brain_search</code> tools over
              the built-in memory whenever you say "remember", "save",
              "note down", etc.
            </CardDescription>
          </div>
        </CardHeader>
        <pre className="overflow-x-auto whitespace-pre-wrap rounded-md bg-neutral-950 p-3 font-mono text-xs text-neutral-300">
          {prompt}
        </pre>
        <Button size="sm" className="mt-2" onClick={() => copy(prompt)}>
          Copy system prompt
        </Button>
      </Card>

      <Card>
        <CardHeader>
          <div>
            <CardTitle>Per-client setup</CardTitle>
            <CardDescription>
              Each LLM client has a different place to drop a permanent
              system prompt. Pick whichever you actually use — you can
              install the snippet in more than one.
            </CardDescription>
          </div>
        </CardHeader>

        <div className="mt-3 space-y-3 text-sm text-neutral-300">
          <SetupAccordion title="Claude Desktop (App)">
            <ol className="ml-4 list-decimal space-y-1">
              <li>
                Click <strong>+</strong> next to "Projects" in the sidebar
                to create a new project (e.g. <em>BRAIN</em>).
              </li>
              <li>
                Open the project, then{" "}
                <strong>Project knowledge / Set custom instructions</strong>.
              </li>
              <li>Paste the snippet from above and save.</li>
              <li>
                Use this project for any conversation where memory should
                land in BRAIN. Other projects keep using Claude Desktop's
                native memory — you can run both side by side.
              </li>
            </ol>
            <p className="mt-2 text-xs text-neutral-500">
              Claude Desktop only reads its config at startup, so fully
              quit and relaunch after installing BRAIN's MCP for the
              first time.
            </p>
          </SetupAccordion>

          <SetupAccordion title="Claude Code (CLI)">
            <ol className="ml-4 list-decimal space-y-1">
              <li>
                Append the snippet to your global Claude Code instructions
                file: <code className="font-mono">~/.claude/CLAUDE.md</code>.
                The CLI reads this on every session start. Add a section
                heading like <code>## BRAIN memory</code> before the
                pasted text so it's clear in your file later.
              </li>
              <li>
                Or, for per-project scope, create{" "}
                <code className="font-mono">CLAUDE.md</code> in the
                project root containing the snippet — it overrides /
                supplements the global file inside that working
                directory.
              </li>
              <li>
                Verify: <code className="font-mono">claude --debug</code>{" "}
                in a session shows the active instructions; or simply
                ask in-session "what memory tools do you have?" — the
                answer should list BRAIN's tools.
              </li>
            </ol>
          </SetupAccordion>

          <SetupAccordion title="Codex (CLI / VS Code)">
            <ol className="ml-4 list-decimal space-y-1">
              <li>
                Codex reads agent instructions from{" "}
                <code className="font-mono">~/.codex/AGENTS.md</code>{" "}
                (global) or <code className="font-mono">AGENTS.md</code>{" "}
                in the project root (per-project).
              </li>
              <li>
                Open the file in your editor (create it if it doesn't
                exist) and paste the snippet under a section heading like
                <code className="font-mono"> ## BRAIN memory</code>.
              </li>
              <li>
                Restart any open Codex sessions so the new instructions
                load.
              </li>
            </ol>
            <p className="mt-2 text-xs text-neutral-500">
              The MCP server name is <code className="font-mono">BRAIN</code> —
              if you previously had it as lowercase, re-register from
              Settings → MCP & Clients so Codex picks up the new name.
            </p>
          </SetupAccordion>

          <SetupAccordion title="Continue.dev (VS Code / JetBrains)">
            <ol className="ml-4 list-decimal space-y-1">
              <li>
                Open <code className="font-mono">~/.continue/config.json</code>{" "}
                (Continue exposes it via Settings → Open config).
              </li>
              <li>
                Add a top-level{" "}
                <code className="font-mono">"systemMessage"</code> key
                whose value is the snippet (escape line breaks as{" "}
                <code className="font-mono">\n</code>), or set it
                per-model under{" "}
                <code className="font-mono">models[*].systemMessage</code>.
              </li>
              <li>
                Continue applies it to every prompt sent to that model.
                Restart the editor so the change takes effect.
              </li>
            </ol>
            <p className="mt-2 text-xs text-neutral-500">
              Continue.dev's <em>chat</em> view shows MCP tools in a
              dropdown — confirm BRAIN tools are visible there before
              relying on the system prompt.
            </p>
          </SetupAccordion>

          <SetupAccordion title="ChatGPT Desktop — manual only, with caveats">
            <p>
              ChatGPT does support MCP servers (under{" "}
              <strong>Settings → Apps & Connectors → Advanced →
              Developer Mode</strong>), but the connection originates
              from OpenAI's backend, not the local app. So the URL has
              to be reachable from the public internet — a localhost
              endpoint like BRAIN's HTTP server isn't acceptable, and
              there's no config file we could auto-write into either
              (registration is UI-only).
            </p>
            <p className="mt-2">
              <strong>If you really want BRAIN in ChatGPT:</strong> run a
              Cloudflare Tunnel or ngrok pointing at BRAIN's local HTTP
              MCP port (default <code className="font-mono">7137</code>),
              then paste the public <code className="font-mono">https://…/mcp</code>{" "}
              URL plus the bearer token (from{" "}
              <code className="font-mono">00_meta/.mcp.json</code>) into
              the Developer Mode form.
            </p>
            <p className="mt-2 text-xs text-amber-300/90">
              <strong>Trade-off:</strong> tunnel traffic egresses through
              a third party, which is a real privacy regression vs. the
              other supported clients (Claude Desktop, Codex,
              Continue.dev — all dial localhost directly). Make sure
              you understand what your ChatGPT queries would expose
              before going down this path.
            </p>
          </SetupAccordion>

          <SetupAccordion title="Open WebUI / other MCP-aware clients">
            <ol className="ml-4 list-decimal space-y-1">
              <li>
                Most clients that support MCP also support a custom
                system prompt — look for "system message", "agent
                instructions", or "preamble" in their settings.
              </li>
              <li>
                Paste the snippet there. The exact location varies by
                client; the common pattern is a per-conversation or
                per-model setting under model configuration.
              </li>
              <li>
                If the client doesn't support a system prompt at all,
                copy-paste the snippet as the first message in each
                conversation — less convenient but still works.
              </li>
            </ol>
          </SetupAccordion>
        </div>
      </Card>
    </div>
  );
}

function SetupAccordion({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <details className="rounded-md border border-neutral-800 bg-neutral-900">
      <summary className="cursor-pointer select-none px-3 py-2 font-medium text-neutral-200 hover:bg-neutral-800/50">
        {title}
      </summary>
      <div className="border-t border-neutral-800 px-3 py-3 text-xs text-neutral-400">
        {children}
      </div>
    </details>
  );
}

function DangerTab({
  onReset,
  resetting,
  onUpdateTemplates,
  updatingTemplates,
}: {
  onReset: () => void;
  resetting: boolean;
  onUpdateTemplates: () => void;
  updatingTemplates: boolean;
}) {
  return (
    <div className="space-y-4">
      {/*
        "Update vault templates" sits under Danger because it
        overwrites AGENTS.md / CLAUDE.md in 00_meta/ with the
        bundled copies — any local edits the user made to those
        two files are lost. The .mcp.json (bearer token, MCP
        server config) is deliberately untouched on this path so
        the user's external integration setup survives. Wiki
        content is also untouched.
      */}
      <Card tone="danger">
        <CardHeader>
          <div>
            <CardTitle>
              <span className="text-red-300">Update vault templates</span>
            </CardTitle>
            <CardDescription>
              Overwrite <code className="font-mono">00_meta/AGENTS.md</code>{" "}
              and <code className="font-mono">00_meta/CLAUDE.md</code> with the
              versions bundled in the current BRAIN release. Use this after a
              BRAIN upgrade to pull new agent conventions into an existing
              vault. Local edits to those two files will be{" "}
              <strong>lost</strong>; the rest of the vault (wiki content,{" "}
              <code className="font-mono">.mcp.json</code>) is untouched.
            </CardDescription>
          </div>
          <Button
            variant="destructive"
            onClick={onUpdateTemplates}
            loading={updatingTemplates}
          >
            {updatingTemplates ? "Updating…" : "Update vault templates"}
          </Button>
        </CardHeader>
      </Card>
      <Card tone="danger">
        <CardHeader>
          <div>
            <CardTitle>
              <span className="text-red-300">Reset BRAIN</span>
            </CardTitle>
            <CardDescription>
              Eject the current vault, forget its location, unregister BRAIN
              from every LLM client, and re-launch the onboarding wizard. Your
              vault data on disk is <strong>not</strong> deleted.
            </CardDescription>
          </div>
          <Button variant="destructive" onClick={onReset} loading={resetting}>
            {resetting ? "Resetting…" : "Reset BRAIN"}
          </Button>
        </CardHeader>
      </Card>
    </div>
  );
}

function RegistrationRow({
  label,
  status,
}: {
  label: string;
  status: ClientStatus | null;
}) {
  let badge: { color: string; text: string };
  if (!status || status.kind === "NotInstalled") {
    badge = { color: "text-neutral-500", text: "Not installed" };
  } else if (status.kind === "Registered") {
    badge = { color: "text-emerald-400", text: "Registered" };
  } else {
    badge = { color: "text-red-400", text: "Failed" };
  }
  const detail = status && "detail" in status ? status.detail : null;
  return (
    <li className="flex flex-col gap-1 px-3 py-2.5 sm:flex-row sm:items-center sm:gap-3">
      <span className="font-medium text-neutral-200">{label}</span>
      <span className={`text-xs ${badge.color}`}>{badge.text}</span>
      {detail && (
        <span
          className="truncate font-mono text-xs text-neutral-500 sm:ml-auto"
          title={detail}
        >
          {detail}
        </span>
      )}
    </li>
  );
}

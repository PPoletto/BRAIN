import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { invoke } from "@tauri-apps/api/core";
import {
  commands,
  type ClientStatus,
  type RegistrationReport,
} from "../../lib/commands";

export function Completion() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const path = params.get("path") ?? "";
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const [report, setReport] = useState<RegistrationReport | null>(null);

  useEffect(() => {
    let cancelled = false;
    commands
      .finishOnboarding(path)
      .then(async () => {
        if (cancelled) return;
        setDone(true);
        // Pull the persisted report so the user always sees it, even if
        // they missed the live event.
        try {
          const r = await commands.lastMcpRegistrationReport();
          if (r && !cancelled) setReport(r);
        } catch (e: unknown) {
          console.debug("could not load registration report", e);
        }
        // If the previous session ended uncleanly, jump to the integrity
        // check after a moment so the user can read the registration result.
        try {
          const pending = await invoke<boolean>("unclean_shutdown_pending");
          if (pending && !cancelled) {
            navigate("/integrity");
            return;
          }
        } catch {
          // Best-effort: the user can still reach /integrity manually.
        }
      })
      .catch((e: unknown) => !cancelled && setError(String(e)));
    return () => {
      cancelled = true;
    };
  }, [path, navigate]);

  const claudeStatus = report?.claude_code;
  const claudeRegistered = claudeStatus?.kind === "Registered";

  return (
    <section>
      <h1 className="text-xl font-semibold mb-2">Your BRAIN is ready</h1>
      {!done && !error && (
        <p className="text-neutral-400 mb-4">Mounting and registering MCP…</p>
      )}
      {error && (
        <div className="mb-4 rounded-md border border-red-900 bg-red-950 p-3 text-sm text-red-300">
          {error}
        </div>
      )}
      <p className="text-neutral-400 mb-1">Vault path:</p>
      <code className="block rounded-md bg-neutral-950 p-2 text-sm font-mono text-neutral-300 mb-4">
        {path}
      </code>

      {report && (
        <div
          className={
            "mb-4 rounded-md border p-3 text-sm " +
            (claudeRegistered
              ? "border-emerald-900 bg-emerald-950 text-emerald-200"
              : "border-amber-900 bg-amber-950 text-amber-200")
          }
        >
          <div className="font-medium mb-1">
            {claudeRegistered
              ? "✓ MCP registered with Claude Code"
              : "⚠ MCP could not be registered automatically"}
          </div>
          {claudeStatus && "detail" in claudeStatus && (
            <div className="text-xs opacity-80 mb-2">{claudeStatus.detail}</div>
          )}
          {claudeRegistered ? (
            <div className="text-xs">
              <strong>Important:</strong> restart Claude Code so it picks up the new
              <code className="mx-1 font-mono">BRAIN</code> MCP server. Then verify with{" "}
              <code className="font-mono">claude mcp list</code>.
            </div>
          ) : (
            <div className="text-xs">
              Open <strong>Settings</strong> below to copy the manual configuration
              snippet, or click "Re-register MCP" once you've installed the
              <code className="mx-1 font-mono">claude</code> CLI.
            </div>
          )}
          <SubReport label="Claude Desktop (App)" status={report.claude_desktop} />
          <SubReport label="Codex" status={report.codex} />
          <SubReport label="Continue.dev" status={report.continue_dev} />
          <SubReport label="ChatGPT Desktop" status={report.chatgpt_desktop} />
          {report.claude_desktop?.kind === "Registered" && (
            <div className="mt-3 rounded-md border border-amber-900 bg-amber-950/50 p-2 text-xs text-amber-200">
              <strong>For Claude Desktop:</strong> just registering BRAIN isn't
              enough — the Desktop app still prefers its built-in memory. Open
              Settings → "Replace Claude's built-in memory" and paste the project
              instructions snippet into a new Claude Desktop project so it uses
              BRAIN for "remember this" requests.
            </div>
          )}
        </div>
      )}

      <p className="text-sm text-neutral-400 mb-4">
        BRAIN runs in the system tray. You can close this window — the tray
        icon will keep MCP available to your LLM clients.
      </p>
      <div className="grid grid-cols-2 gap-3">
        <button
          type="button"
          onClick={() => navigate("/viewer")}
          className="rounded-md border border-neutral-700 bg-neutral-800 px-4 py-3 text-left hover:bg-neutral-700"
        >
          Open BRAIN Viewer
        </button>
        <button
          type="button"
          onClick={() => navigate("/settings")}
          className="rounded-md border border-neutral-700 bg-neutral-800 px-4 py-3 text-left hover:bg-neutral-700"
        >
          Configure settings
        </button>
      </div>
    </section>
  );
}

function SubReport({ label, status }: { label: string; status: ClientStatus | null }) {
  if (!status || status.kind === "NotInstalled") return null;
  const ok = status.kind === "Registered";
  return (
    <div className="mt-1 text-xs">
      <span className={ok ? "text-emerald-300" : "text-red-300"}>
        {ok ? "✓" : "✗"} {label}
      </span>
      {"detail" in status && status.detail && (
        <span className="ml-2 text-neutral-400">— {status.detail}</span>
      )}
    </div>
  );
}

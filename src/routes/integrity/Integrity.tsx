import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { invoke } from "@tauri-apps/api/core";

type CheckResult =
  | { kind: "Ok"; detail: string }
  | { kind: "Warn"; detail: string }
  | { kind: "Error"; detail: string }
  | { kind: "Skipped"; detail: string };

type RecoveryAction = {
  id: string;
  label: string;
  destructive: boolean;
};

type IntegrityReport = {
  clean: boolean;
  git: CheckResult;
  db: CheckResult;
  pages: CheckResult;
  suggestions: RecoveryAction[];
};

export function Integrity() {
  const navigate = useNavigate();
  const [report, setReport] = useState<IntegrityReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(true);
  const [actionMsg, setActionMsg] = useState<string | null>(null);

  async function runCheck() {
    setRunning(true);
    setError(null);
    try {
      const r = await invoke<IntegrityReport>("run_integrity_check");
      setReport(r);
    } catch (e: unknown) {
      setError(String(e));
    } finally {
      setRunning(false);
    }
  }

  useEffect(() => {
    runCheck();
  }, []);

  async function applyAction(action: RecoveryAction) {
    if (
      action.destructive &&
      !window.confirm(
        `${action.label}\n\nThis is destructive — local changes that aren't committed will be lost. Continue?`,
      )
    ) {
      return;
    }
    setActionMsg(null);
    try {
      const msg = await invoke<string>("run_recovery_action", { id: action.id });
      setActionMsg(msg);
      await runCheck();
    } catch (e: unknown) {
      setError(String(e));
    }
  }

  async function dismiss() {
    try {
      await invoke<void>("dismiss_unclean_flag");
    } catch (e: unknown) {
      console.debug(e);
    }
    navigate("/viewer");
  }

  return (
    <div className="min-h-screen p-8">
      <header className="mb-6 flex items-center justify-between">
        <h1 className="text-2xl font-semibold">Integrity check</h1>
        <button
          type="button"
          onClick={() => navigate("/viewer")}
          className="text-sm text-neutral-400 hover:text-neutral-200"
        >
          Back to viewer
        </button>
      </header>

      <p className="text-neutral-400 mb-6">
        BRAIN detected an unclean shutdown the last time the vault was active.
        We're verifying the wiki Git repository, the SQLite index and the
        filesystem mapping. Recovery actions only run when you ask for them.
      </p>

      {error && (
        <div className="mb-4 rounded-md border border-red-900 bg-red-950 p-3 text-sm text-red-300">
          {error}
        </div>
      )}

      {actionMsg && (
        <div className="mb-4 rounded-md border border-emerald-900 bg-emerald-950 p-3 text-sm text-emerald-300">
          {actionMsg}
        </div>
      )}

      {running && <p className="text-neutral-500">Running checks…</p>}

      {report && (
        <>
          <div
            className={
              "mb-4 rounded-md p-3 text-sm " +
              (report.clean
                ? "border border-emerald-900 bg-emerald-950 text-emerald-300"
                : "border border-amber-900 bg-amber-950 text-amber-300")
            }
          >
            {report.clean ? "All checks passed." : "Issues detected — see below."}
          </div>
          <ul className="space-y-2 mb-6">
            <CheckRow title="Wiki repository" result={report.git} />
            <CheckRow title="SQLite index" result={report.db} />
            <CheckRow title="Pages-table ↔ filesystem" result={report.pages} />
          </ul>

          {report.suggestions.length > 0 && (
            <section className="mb-6">
              <h2 className="text-lg font-medium mb-2">Recovery actions</h2>
              <ul className="space-y-2">
                {report.suggestions.map((s) => (
                  <li
                    key={s.id}
                    className="flex items-center justify-between rounded-md border border-neutral-800 bg-neutral-900 p-3 text-sm"
                  >
                    <span className="flex-1">{s.label}</span>
                    <button
                      type="button"
                      onClick={() => applyAction(s)}
                      className={
                        "rounded-md px-3 py-1.5 text-xs font-medium " +
                        (s.destructive
                          ? "bg-red-700 text-white hover:bg-red-600"
                          : "bg-emerald-600 text-white hover:bg-emerald-500")
                      }
                    >
                      {s.destructive ? "Run (destructive)" : "Run"}
                    </button>
                  </li>
                ))}
              </ul>
            </section>
          )}
        </>
      )}

      <div className="flex justify-end gap-3">
        <button
          type="button"
          onClick={runCheck}
          className="rounded-md border border-neutral-700 px-3 py-2 text-sm hover:bg-neutral-800"
        >
          Re-run checks
        </button>
        <button
          type="button"
          onClick={dismiss}
          className="rounded-md bg-emerald-600 px-3 py-2 text-sm font-medium text-white"
        >
          Dismiss and continue
        </button>
      </div>
    </div>
  );
}

function CheckRow({ title, result }: { title: string; result: CheckResult }) {
  const palette: Record<CheckResult["kind"], string> = {
    Ok: "border-emerald-900 bg-emerald-950 text-emerald-300",
    Warn: "border-amber-900 bg-amber-950 text-amber-300",
    Error: "border-red-900 bg-red-950 text-red-300",
    Skipped: "border-neutral-800 bg-neutral-900 text-neutral-500",
  };
  return (
    <li className={`rounded-md border p-3 text-sm ${palette[result.kind]}`}>
      <div className="font-medium">{title}</div>
      <div className="text-xs opacity-80 mt-0.5">
        {result.kind} — {result.detail}
      </div>
    </li>
  );
}

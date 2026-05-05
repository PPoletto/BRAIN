import { useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { commands } from "../../lib/commands";

export function Format() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const diskId = params.get("disk") ?? "";
  const initialPath = params.get("path") ?? "";
  const action = params.get("action") ?? "create";
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [path, setPath] = useState(initialPath);

  async function confirmFormat() {
    if (action === "open") {
      navigate(`/onboarding/template?path=${encodeURIComponent(path)}&action=open`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await commands.formatDisk(diskId);
      const newPath = result.mount_path || path;
      setPath(newPath);
      navigate(`/onboarding/template?path=${encodeURIComponent(newPath)}&action=create`);
    } catch (e: unknown) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section>
      <h1 className="text-xl font-semibold mb-2">
        {action === "open" ? "Confirm disk" : "Format disk as exFAT"}
      </h1>
      <p className="text-neutral-400 mb-4">
        {action === "open"
          ? "We'll look for an existing BRAIN at this location."
          : "BRAIN SSDs are formatted as exFAT with the volume label BRAIN. All existing data on this disk will be lost. The OS will prompt you for administrator permission."}
      </p>

      <div className="rounded-md border border-neutral-800 bg-neutral-950 p-3 text-sm font-mono text-neutral-300">
        Disk #{diskId}
        {path && <div className="mt-1 text-neutral-500">{path}</div>}
      </div>

      {error && (
        <div className="mt-3 rounded-md border border-red-900 bg-red-950 p-3 text-sm text-red-300">
          {error}
          {path && (
            <div className="mt-2">
              If the disk is already exFAT and labeled BRAIN, you can{" "}
              <button
                type="button"
                onClick={() =>
                  navigate(
                    `/onboarding/template?path=${encodeURIComponent(path)}&action=create`,
                  )
                }
                className="underline"
              >
                continue without formatting
              </button>
              .
            </div>
          )}
        </div>
      )}

      <div className="mt-6 flex justify-between">
        <button
          type="button"
          onClick={() => navigate(-1)}
          className="text-sm text-neutral-400 hover:text-neutral-200"
        >
          Back
        </button>
        <button
          type="button"
          disabled={busy || !diskId}
          onClick={confirmFormat}
          className="rounded-md bg-emerald-600 px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
        >
          {busy
            ? "Formatting…"
            : action === "open"
              ? "Continue"
              : "Format and continue"}
        </button>
      </div>
    </section>
  );
}

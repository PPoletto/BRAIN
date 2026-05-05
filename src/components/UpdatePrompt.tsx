import { useEffect, useState } from "react";
import { useUpdateStore } from "../lib/updateStore";

const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000; // 6 h

/// Floating toast pinned to the bottom-right corner that appears whenever
/// an update is detected (either by the periodic background check or by
/// the user clicking the version in the status bar). Reads its state from
/// the shared `useUpdateStore` so manual + automatic checks share a
/// single source of truth.
export function UpdatePrompt() {
  const availability = useUpdateStore((s) => s.availability);
  const dismissed = useUpdateStore((s) => s.dismissed);
  const installing = useUpdateStore((s) => s.installing);
  const setDismissed = useUpdateStore((s) => s.setDismissed);
  const check = useUpdateStore((s) => s.check);
  const applyNow = useUpdateStore((s) => s.applyNow);
  const skipUpdate = useUpdateStore((s) => s.skip);

  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void check().catch(() => {
      // Auto-checks are best-effort; the user can trigger a manual one
      // from the status bar.
    });
    const id = setInterval(() => {
      void check().catch(() => {});
    }, CHECK_INTERVAL_MS);
    return () => clearInterval(id);
  }, [check]);

  if (!availability || dismissed || !availability.available) return null;

  return (
    <div className="fixed bottom-4 right-4 z-50 max-w-md rounded-lg border border-emerald-800 bg-neutral-900 p-4 shadow-2xl">
      <div className="mb-1 text-xs uppercase tracking-wider text-emerald-400">
        Update available
      </div>
      <div className="mb-3 text-sm">
        <strong>BRAIN {availability.version}</strong> is ready to install.
        You're currently on {availability.current_version}.
      </div>
      {availability.notes && (
        <pre className="mb-3 max-h-32 overflow-y-auto rounded-md bg-neutral-950 p-2 text-xs font-mono text-neutral-300">
          {availability.notes}
        </pre>
      )}
      {error && (
        <div className="mb-3 rounded-md border border-red-900 bg-red-950 p-2 text-xs text-red-300">
          {error}
        </div>
      )}
      <div className="flex justify-end gap-2">
        <button
          type="button"
          onClick={() => {
            void skipUpdate().catch((e) => setError(String(e)));
          }}
          className="rounded-md border border-neutral-700 px-3 py-1.5 text-xs text-neutral-400 hover:bg-neutral-800"
        >
          Skip this version
        </button>
        <button
          type="button"
          onClick={() => setDismissed(true)}
          className="rounded-md border border-neutral-700 px-3 py-1.5 text-xs hover:bg-neutral-800"
        >
          Later
        </button>
        <button
          type="button"
          onClick={() => {
            setError(null);
            void applyNow().catch((e) => setError(String(e)));
          }}
          disabled={installing}
          className="rounded-md bg-emerald-600 px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50"
        >
          {installing ? "Installing…" : "Install now"}
        </button>
      </div>
    </div>
  );
}

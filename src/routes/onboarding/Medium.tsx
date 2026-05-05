import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { commands, type DiskInfo } from "../../lib/commands";
import { Button } from "../../components/ui/Button";
import { useAsyncAction } from "../../components/ui/useAsyncAction";

type Mode = "create" | "open";

function formatBytes(n: number): string {
  if (n <= 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

export function Medium() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const action = (params.get("action") ?? "create") as Mode;
  const [showSystem, setShowSystem] = useState(false);
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    commands
      .listDisks()
      .then((d) => {
        if (!cancelled) setDisks(d);
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(String(e));
      })
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, []);

  const visible = showSystem ? disks : disks.filter((d) => !d.is_system);

  const refreshAction = useAsyncAction(
    async () => {
      const next = await commands.refreshDisks();
      setDisks(next);
      setError(null);
    },
    { errorPrefix: "Disk scan failed" },
  );

  async function chooseFolder() {
    const path = await openDialog({ directory: true, multiple: false });
    if (typeof path === "string") {
      navigate(`/onboarding/template?path=${encodeURIComponent(path)}&action=${action}`);
    }
  }

  function chooseDisk(d: DiskInfo) {
    if (!d.mount_path) return;
    navigate(
      `/onboarding/format?disk=${encodeURIComponent(d.id)}&path=${encodeURIComponent(d.mount_path)}&action=${action}`,
    );
  }

  return (
    <section>
      <h1 className="text-xl font-semibold mb-2">Choose where BRAIN lives</h1>
      <p className="text-neutral-400 mb-4">
        {action === "create"
          ? "Pick an external disk or a local folder to host your new BRAIN."
          : "Pick the disk or folder containing your existing BRAIN."}
      </p>

      <div className="mb-3 flex items-center gap-2">
        <Button onClick={chooseFolder} size="sm">
          Choose folder…
        </Button>
        <Button
          onClick={() => void refreshAction.trigger()}
          loading={refreshAction.loading}
          size="sm"
        >
          {refreshAction.loading ? "Scanning…" : "Refresh"}
        </Button>
        <label className="ml-auto flex items-center gap-2 text-sm text-neutral-400">
          <input
            type="checkbox"
            checked={showSystem}
            onChange={(e) => setShowSystem(e.target.checked)}
          />
          Show system disks
        </label>
      </div>

      {(loading || refreshAction.loading) && (
        <p className="text-neutral-500">Scanning disks…</p>
      )}
      {error && <p className="text-red-400">{error}</p>}

      <ul className="divide-y divide-neutral-800 rounded-md border border-neutral-800">
        {visible.length === 0 && !loading && (
          <li className="p-4 text-neutral-500">No matching disks found.</li>
        )}
        {visible.map((d) => (
          <li key={d.id} className="flex items-center gap-3 p-3">
            <button
              type="button"
              onClick={() => chooseDisk(d)}
              className="flex-1 text-left"
            >
              <div className="font-medium">
                {d.volume_label ?? d.name} {d.is_system && <span className="ml-1 text-xs text-amber-400">(system)</span>}
              </div>
              <div className="text-xs text-neutral-500">
                {formatBytes(d.size_bytes)} · {d.filesystem ?? "unknown FS"} · {d.mount_path ?? "unmounted"}
              </div>
            </button>
          </li>
        ))}
      </ul>

      <div className="mt-6 flex justify-between">
        <button
          type="button"
          onClick={() => navigate("/onboarding")}
          className="text-sm text-neutral-400 hover:text-neutral-200"
        >
          Back
        </button>
      </div>
    </section>
  );
}

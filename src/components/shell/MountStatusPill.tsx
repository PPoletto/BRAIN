import { useAppState } from "../../lib/state";

const PILL_STYLES: Record<string, string> = {
  disconnected: "bg-neutral-800 text-neutral-400",
  mounting: "bg-amber-900 text-amber-200",
  "mounted-idle": "bg-emerald-900 text-emerald-200",
  "mounted-busy": "bg-amber-900 text-amber-200",
  error: "bg-red-900 text-red-200",
};

const DOT_STYLES: Record<string, string> = {
  disconnected: "bg-neutral-500",
  mounting: "bg-amber-400 animate-pulse",
  "mounted-idle": "bg-emerald-400",
  "mounted-busy": "bg-amber-400 animate-pulse",
  error: "bg-red-400",
};

export function MountStatusPill() {
  const tray = useAppState((s) => s.tray);
  const cls = PILL_STYLES[tray.state] ?? PILL_STYLES.disconnected;
  const dot = DOT_STYLES[tray.state] ?? DOT_STYLES.disconnected;
  return (
    <div
      className={`inline-flex items-center gap-2 rounded-full px-2.5 py-1 text-xs font-medium ${cls}`}
      title={tray.tooltip}
    >
      <span className={`size-2 rounded-full ${dot}`} aria-hidden />
      <span>{labelFor(tray.state)}</span>
    </div>
  );
}

function labelFor(state: string): string {
  switch (state) {
    case "mounted-idle":
      return "BRAIN ready";
    case "mounted-busy":
      return "BRAIN busy";
    case "mounting":
      return "Mounting";
    case "error":
      return "BRAIN error";
    default:
      return "No vault";
  }
}

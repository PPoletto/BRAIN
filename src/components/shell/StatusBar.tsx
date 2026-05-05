import { useAppState } from "../../lib/state";
import { useUpdateStore } from "../../lib/updateStore";
import { useToast } from "../ui/toast-context";

export function StatusBar() {
  const tray = useAppState((s) => s.tray);
  const checking = useUpdateStore((s) => s.checking);
  const check = useUpdateStore((s) => s.check);
  const setDismissed = useUpdateStore((s) => s.setDismissed);
  const { push } = useToast();

  async function handleCheckUpdate() {
    if (checking) return;
    try {
      const a = await check();
      if (a?.available) {
        // Make sure the prompt re-appears even if the user dismissed it
        // earlier in this session.
        setDismissed(false);
        push({
          kind: "success",
          message: `Update ${a.version} is ready`,
          detail: "See the prompt in the corner to install.",
        });
      } else if (a) {
        push({
          kind: "info",
          message: "You're on the latest BRAIN",
          detail: `Current version: ${a.current_version}`,
        });
      }
    } catch (e: unknown) {
      push({
        kind: "warning",
        message: "Update check failed",
        detail: e instanceof Error ? e.message : String(e),
      });
    }
  }

  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-neutral-800 bg-neutral-950 px-3 text-xs text-neutral-500">
      <span className="flex items-center gap-2">
        <span className={`size-1.5 rounded-full ${dotFor(tray.state)}`} aria-hidden />
        {/*
          The pill in the top bar already shows the status tag
          ("BRAIN ready" / "BRAIN busy" / "No vault"). Repeating it here
          felt redundant — instead we surface the most useful piece of
          context for the bottom-of-window line: the vault path. When no
          vault is mounted we show the same "No vault" label so the line
          isn't empty.
        */}
        {tray.vault_path ? (
          <span className="truncate font-mono" title={tray.vault_path}>
            {tray.vault_path}
          </span>
        ) : (
          <span>No vault mounted</span>
        )}
      </span>
      <span className="ml-auto flex items-center gap-3">
        {tray.active_operations > 0 && (
          <span className="text-amber-400">
            {tray.active_operations} op{tray.active_operations === 1 ? "" : "s"} active
          </span>
        )}
        <button
          type="button"
          onClick={handleCheckUpdate}
          disabled={checking}
          title="Click to check for a newer BRAIN release"
          className="cursor-pointer rounded px-1 py-0.5 hover:bg-neutral-800 hover:text-neutral-300 disabled:cursor-default"
        >
          {checking ? "Checking…" : "BRAIN v0.1.0"}
        </button>
      </span>
    </footer>
  );
}

function dotFor(state: string): string {
  switch (state) {
    case "mounted-idle":
      return "bg-emerald-500";
    case "mounted-busy":
    case "mounting":
      return "bg-amber-400";
    case "error":
      return "bg-red-500";
    default:
      return "bg-neutral-600";
  }
}

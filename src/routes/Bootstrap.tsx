import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { commands } from "../lib/commands";
import { Button } from "../components/ui/Button";
import { BrainIcon } from "../components/ui/BrainIcon";

/// Decides whether to send the user to the viewer (auto-mounted vault)
/// or to the onboarding wizard (no vault yet, or remembered path went
/// stale). Mounted at the `/` route as the very first thing the user sees.
export function Bootstrap() {
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);
  const [missing, setMissing] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    commands
      .bootstrapApp()
      .then((r) => {
        if (cancelled) return;
        if (r.auto_mounted) {
          navigate("/viewer", { replace: true });
        } else if (r.last_known_vault_missing && r.vault_path) {
          setMissing(r.vault_path);
        } else {
          navigate("/onboarding", { replace: true });
        }
      })
      .catch((e: unknown) => !cancelled && setError(String(e)));
    return () => {
      cancelled = true;
    };
  }, [navigate]);

  if (error || missing) {
    return (
      <div className="flex min-h-screen items-center justify-center p-6">
        <div className="w-full max-w-xl rounded-xl border border-neutral-800 bg-neutral-900 p-8 shadow-2xl">
          <div className="mb-2 flex items-center gap-2 text-sm text-neutral-500">
            <BrainIcon size={20} />
            <span className="font-semibold text-neutral-300">BRAIN</span>
          </div>
          {error && (
            <>
              <h1 className="text-xl font-semibold text-red-300">
                Bootstrap failed
              </h1>
              <p className="mt-2 break-all rounded-md border border-red-900 bg-red-950/40 p-3 text-sm text-red-200">
                {error}
              </p>
            </>
          )}
          {missing && (
            <>
              <h1 className="text-xl font-semibold">BRAIN is offline</h1>
              <p className="mt-2 text-sm text-neutral-400">
                BRAIN remembered a vault at:
              </p>
              <code className="mt-1 block rounded-md bg-neutral-950 p-2 font-mono text-xs text-neutral-300">
                {missing}
              </code>
              <p className="mt-3 text-sm text-neutral-400">
                That path isn't a BRAIN vault right now (disk unplugged or vault
                moved). Reconnect the disk and try again, or set up a new vault.
              </p>
              <div className="mt-5 flex flex-wrap gap-2">
                <Button variant="primary" onClick={() => navigate("/onboarding", { replace: true })}>
                  Set up new BRAIN
                </Button>
                <Button
                  variant="secondary"
                  onClick={() =>
                    commands.bootstrapApp().then((r) => {
                      if (r.auto_mounted) navigate("/viewer", { replace: true });
                    })
                  }
                >
                  Try again
                </Button>
              </div>
            </>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-screen items-center justify-center p-6">
      <div className="text-center">
        <BrainIcon size={84} pulse className="mx-auto mb-4" />
        <div className="text-2xl font-semibold text-neutral-100">BRAIN</div>
        <p className="mt-2 text-sm text-neutral-500">Loading…</p>
      </div>
    </div>
  );
}

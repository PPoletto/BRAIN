import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { commands } from "../../lib/commands";
import { Button } from "../../components/ui/Button";
import { useAsyncAction } from "../../components/ui/useAsyncAction";

const INPUT_CLASS =
  "h-9 w-full rounded-md border border-neutral-800 bg-neutral-900 px-3 text-sm placeholder:text-neutral-600 focus:border-emerald-700 focus:outline-none";

export function CloneVault() {
  const navigate = useNavigate();
  const [url, setUrl] = useState("");
  const [pat, setPat] = useState("");
  const [folder, setFolder] = useState<string | null>(null);
  const [recoveryKey, setRecoveryKey] = useState("");

  async function chooseFolder() {
    const path = await openDialog({ directory: true, multiple: false });
    if (typeof path === "string") setFolder(path);
  }

  const cloneAction = useAsyncAction(
    async () => {
      if (!url.trim()) throw new Error("Enter the Git repository URL");
      if (!folder) throw new Error("Choose an empty target folder");
      if (recoveryKey.trim().length !== 64) {
        throw new Error("The recovery key must be 64 hexadecimal characters");
      }
      await commands.cloneVault(url.trim(), pat.trim() || null, folder, recoveryKey.trim());
      // Reuse the standard completion screen — it mounts, indexes and
      // registers MCP via finish_onboarding.
      navigate(`/onboarding/completion?path=${encodeURIComponent(folder)}`);
    },
    { pending: "Cloning and decrypting…", errorPrefix: "Clone failed" },
  );

  return (
    <section aria-labelledby="clone-title">
      <h1 id="clone-title" className="text-xl font-semibold">
        Clone an encrypted BRAIN
      </h1>
      <p className="mt-2 text-sm text-neutral-400">
        Pull an existing encrypted vault from a private Git remote onto this
        machine. Content is decrypted locally after your recovery key is
        verified — a wrong key is rejected and nothing is mounted.
      </p>

      <div className="mt-6 space-y-4">
        <label className="block">
          <span className="text-sm font-medium text-neutral-200">Repository URL</span>
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://github.com/you/brain.git"
            className={`mt-1 ${INPUT_CLASS}`}
          />
        </label>

        <label className="block">
          <span className="text-sm font-medium text-neutral-200">
            Access token <span className="text-neutral-500">(for private remotes)</span>
          </span>
          <input
            type="password"
            value={pat}
            onChange={(e) => setPat(e.target.value)}
            placeholder="Personal access token"
            className={`mt-1 ${INPUT_CLASS}`}
          />
          <span className="mt-1 block text-xs text-neutral-500">
            Stored only in your OS keychain, never in the repo.
          </span>
        </label>

        <div>
          <span className="text-sm font-medium text-neutral-200">Target folder</span>
          <div className="mt-1 flex items-center gap-2">
            <Button variant="secondary" size="sm" onClick={() => void chooseFolder()}>
              Choose folder…
            </Button>
            <span className="truncate text-sm text-neutral-400">
              {folder ?? "No folder chosen"}
            </span>
          </div>
          <span className="mt-1 block text-xs text-neutral-500">
            Pick a new, empty folder — the vault is cloned into it.
          </span>
        </div>

        <label className="block">
          <span className="text-sm font-medium text-neutral-200">Recovery key</span>
          <input
            value={recoveryKey}
            onChange={(e) => setRecoveryKey(e.target.value)}
            placeholder="64-character hex recovery key"
            spellCheck={false}
            className={`mt-1 font-mono ${INPUT_CLASS}`}
          />
          <span className="mt-1 block text-xs text-neutral-500">
            The key printed when encryption was enabled, from your password
            manager.
          </span>
        </label>

        <div className="flex items-center gap-3 pt-2">
          <Button
            variant="secondary"
            size="sm"
            onClick={() => navigate("/onboarding")}
            disabled={cloneAction.loading}
          >
            Back
          </Button>
          <Button
            variant="primary"
            size="sm"
            loading={cloneAction.loading}
            onClick={() => void cloneAction.trigger()}
          >
            {cloneAction.loading ? "Cloning…" : "Clone and decrypt"}
          </Button>
        </div>
      </div>
    </section>
  );
}

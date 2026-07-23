import { useNavigate } from "react-router-dom";

export function Welcome() {
  const navigate = useNavigate();
  return (
    <section aria-labelledby="welcome-title">
      <h1 id="welcome-title" className="text-2xl font-semibold">
        Welcome to BRAIN
      </h1>
      <p className="mt-2 text-sm text-neutral-400">
        BRAIN is your personal knowledge system. Pick how you want to start.
      </p>
      <div className="mt-6 grid gap-3 lg:grid-cols-3">
        <button
          type="button"
          onClick={() => navigate("/onboarding/medium?action=create")}
          className="group rounded-lg border border-neutral-800 bg-neutral-950 p-5 text-left transition-colors hover:border-emerald-700 hover:bg-neutral-900"
        >
          <div className="flex items-center gap-3">
            <span className="text-2xl" aria-hidden>
              ✨
            </span>
            <div className="font-semibold text-neutral-100">Create new BRAIN</div>
          </div>
          <p className="mt-2 text-sm text-neutral-400">
            Initialize a vault on an SSD or in a local folder. BRAIN formats the
            disk (exFAT, label <code className="font-mono">BRAIN</code>) and lays
            out the canonical directory structure.
          </p>
        </button>
        <button
          type="button"
          onClick={() => navigate("/onboarding/medium?action=open")}
          className="group rounded-lg border border-neutral-800 bg-neutral-950 p-5 text-left transition-colors hover:border-emerald-700 hover:bg-neutral-900"
        >
          <div className="flex items-center gap-3">
            <span className="text-2xl" aria-hidden>
              📂
            </span>
            <div className="font-semibold text-neutral-100">Open existing BRAIN</div>
          </div>
          <p className="mt-2 text-sm text-neutral-400">
            Mount an existing vault from disk or folder. BRAIN auto-detects the
            marker file and registers MCP for your LLM clients.
          </p>
        </button>
        <button
          type="button"
          onClick={() => navigate("/onboarding/clone")}
          className="group rounded-lg border border-neutral-800 bg-neutral-950 p-5 text-left transition-colors hover:border-emerald-700 hover:bg-neutral-900"
        >
          <div className="flex items-center gap-3">
            <span className="text-2xl" aria-hidden>
              ☁️
            </span>
            <div className="font-semibold text-neutral-100">Clone from Git</div>
          </div>
          <p className="mt-2 text-sm text-neutral-400">
            Bring an encrypted BRAIN from a private Git remote onto this
            machine. You'll need the repo URL, an access token, and the
            vault's recovery key.
          </p>
        </button>
      </div>
    </section>
  );
}

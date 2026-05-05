import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { commands } from "../../lib/commands";

type StepName = "init" | "template" | "download";
type StepStatus = "pending" | "running" | "done" | "error" | "skipped";
type Step = { name: StepName; label: string; status: StepStatus };

type Progress = { file: string; current: number; total: number };

const STEPS: { name: StepName; label: string }[] = [
  { name: "init", label: "Initialize vault layout" },
  { name: "template", label: "Populate canonical templates" },
  {
    name: "download",
    label: "Download bge-m3 embedding model (~2.3 GB, one-time)",
  },
];

export function TemplatePopulation() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const path = params.get("path") ?? "";
  const action = params.get("action") ?? "create";

  const [steps, setSteps] = useState<Step[]>(
    STEPS.map((s) => ({ ...s, status: "pending" })),
  );
  const [progress, setProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorStep, setErrorStep] = useState<StepName | null>(null);
  const [running, setRunning] = useState(false);
  const startedRef = useRef(false);

  // Subscribe to model-download-progress events.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<Progress>("model-download-progress", (e) => {
      setProgress(e.payload);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  const updateStep = useCallback((name: StepName, status: StepStatus) => {
    setSteps((s) => s.map((step) => (step.name === name ? { ...step, status } : step)));
  }, []);

  const runFrom = useCallback(
    async (resumeAt: StepName) => {
      setRunning(true);
      setError(null);
      setErrorStep(null);
      try {
        const order: StepName[] = ["init", "template", "download"];
        const startIdx = order.indexOf(resumeAt);
        for (let i = startIdx; i < order.length; i++) {
          const name = order[i];
          updateStep(name, "running");
          if (name === "init") await commands.initVault(path);
          else if (name === "template") await commands.populateTemplate(path);
          else if (name === "download") await commands.downloadEmbeddingModel(path);
          updateStep(name, "done");
        }
        navigate(
          `/onboarding/completion?path=${encodeURIComponent(path)}&action=${action}`,
        );
      } catch (e: unknown) {
        const message = String(e);
        setError(message);
        // Mark the step that failed.
        setSteps((s) => {
          const idx = s.findIndex((step) => step.status === "running");
          if (idx === -1) return s;
          setErrorStep(s[idx].name);
          return s.map((step, i) =>
            i === idx ? { ...step, status: "error" } : step,
          );
        });
      } finally {
        setRunning(false);
      }
    },
    [path, action, navigate, updateStep],
  );

  // Kick off once on first render.
  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    runFrom("init");
  }, [runFrom]);

  const handleRetry = () => {
    if (errorStep) runFrom(errorStep);
  };

  const handleSkipModel = () => {
    // Mark the download step as skipped and continue. Brain falls back to
    // the deterministic HashedEmbedder when bge-m3 weights are missing —
    // search still works, just without semantic matching.
    setSteps((s) =>
      s.map((step) =>
        step.name === "download" ? { ...step, status: "skipped" } : step,
      ),
    );
    setError(null);
    setErrorStep(null);
    navigate(
      `/onboarding/completion?path=${encodeURIComponent(path)}&action=${action}`,
    );
  };

  const handleBack = () => {
    navigate("/onboarding");
  };

  const isModelStepError = errorStep === "download";

  return (
    <section>
      <h1 className="text-xl font-semibold mb-2">Setting up your BRAIN</h1>
      <p className="text-neutral-400 mb-4">
        This takes a moment. The embedding-model download is a one-time ~2.3 GB
        transfer; later mounts skip it. Don't unplug.
      </p>
      <ol className="space-y-2">
        {steps.map((s) => (
          <li key={s.name} className="flex items-center gap-3">
            <StepDot status={s.status} />
            <span
              className={
                s.status === "done"
                  ? "text-neutral-200"
                  : s.status === "running"
                    ? "text-neutral-100"
                    : s.status === "error"
                      ? "text-red-300"
                      : s.status === "skipped"
                        ? "text-neutral-500 italic"
                        : "text-neutral-500"
              }
            >
              {s.label}
              {s.status === "skipped" && " (skipped — using fallback embedder)"}
            </span>
          </li>
        ))}
      </ol>
      {progress && steps.find((s) => s.name === "download")?.status === "running" && (
        <div className="mt-4 text-sm text-neutral-400">
          Downloading <code>{progress.file}</code> ({progress.current} of{" "}
          {progress.total})
        </div>
      )}
      {error && (
        <div className="mt-4 rounded-md border border-red-900 bg-red-950 p-3 text-sm text-red-300">
          <p className="mb-3 break-words">{error}</p>
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              onClick={handleRetry}
              disabled={running}
              className="rounded-md border border-red-800 bg-red-900/40 px-3 py-1.5 text-xs font-medium text-red-100 hover:bg-red-900/70 disabled:opacity-50"
            >
              {running ? "Retrying…" : "Retry"}
            </button>
            {isModelStepError && (
              <button
                type="button"
                onClick={handleSkipModel}
                disabled={running}
                className="rounded-md border border-neutral-700 bg-neutral-800 px-3 py-1.5 text-xs font-medium text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
                title="Continue without bge-m3. Search will use the built-in fallback embedder until you re-run the download from Settings."
              >
                Skip download (use fallback embedder)
              </button>
            )}
            <button
              type="button"
              onClick={handleBack}
              disabled={running}
              className="rounded-md border border-neutral-700 bg-neutral-800 px-3 py-1.5 text-xs font-medium text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
            >
              Back to start
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

function StepDot({ status }: { status: Step["status"] }) {
  const cls =
    status === "done"
      ? "bg-emerald-500"
      : status === "running"
        ? "bg-amber-400 animate-pulse"
        : status === "error"
          ? "bg-red-500"
          : status === "skipped"
            ? "bg-neutral-500"
            : "bg-neutral-600";
  return <span className={`inline-block size-3 rounded-full ${cls}`} aria-hidden />;
}

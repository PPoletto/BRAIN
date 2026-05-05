import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  ToastContext,
  type ToastInput,
  type ToastKind,
  useToast,
} from "./toast-context";

type Toast = ToastInput & { id: string };

const KIND_STYLES: Record<ToastKind, string> = {
  info: "border-neutral-700 bg-neutral-900 text-neutral-200",
  success: "border-emerald-700 bg-emerald-950 text-emerald-200",
  warning: "border-amber-700 bg-amber-950 text-amber-200",
  error: "border-red-700 bg-red-950 text-red-200",
};

const KIND_ICONS: Record<ToastKind, string> = {
  info: "ℹ",
  success: "✓",
  warning: "⚠",
  error: "✕",
};

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const idRef = useRef(0);

  const dismiss = useCallback((id: string) => {
    setToasts((cur) => cur.filter((t) => t.id !== id));
  }, []);

  const push = useCallback(
    (toast: ToastInput) => {
      idRef.current += 1;
      const id = String(idRef.current);
      setToasts((cur) => [...cur, { ...toast, id }]);
      const timeoutMs = toast.kind === "error" ? 8000 : 4000;
      setTimeout(() => dismiss(id), timeoutMs);
      return id;
    },
    [dismiss],
  );

  return (
    <ToastContext.Provider value={{ push, dismiss }}>
      {children}
      <div className="pointer-events-none fixed right-4 bottom-12 z-50 flex flex-col gap-2">
        {toasts.map((t) => (
          <div
            key={t.id}
            className={`pointer-events-auto min-w-[260px] max-w-md rounded-md border px-3 py-2 text-sm shadow-lg ${KIND_STYLES[t.kind]}`}
            role="status"
          >
            <div className="flex items-start justify-between gap-3">
              <div className="flex items-start gap-2">
                <span className="mt-0.5 select-none" aria-hidden>
                  {KIND_ICONS[t.kind]}
                </span>
                <div>
                  <div className="font-medium">{t.message}</div>
                  {t.detail && (
                    <div className="mt-0.5 text-xs opacity-80">{t.detail}</div>
                  )}
                </div>
              </div>
              <button
                type="button"
                onClick={() => dismiss(t.id)}
                className="text-xs opacity-60 hover:opacity-100"
                aria-label="Dismiss"
              >
                ✕
              </button>
            </div>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

/// Listens for backend `toast` events and routes them into the toast system.
/// Mount once at the app root (inside <ToastProvider>).
export function BackendToastsBridge() {
  const { push } = useToast();
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<ToastInput>("toast", (event) => {
      push(event.payload);
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      unlisten?.();
    };
  }, [push]);
  return null;
}

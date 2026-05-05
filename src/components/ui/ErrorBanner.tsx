import type { ReactNode } from "react";

type Tone = "error" | "warning" | "info";

const TONE: Record<Tone, string> = {
  error: "border-red-900 bg-red-950/60 text-red-200",
  warning: "border-amber-900 bg-amber-950/60 text-amber-200",
  info: "border-neutral-700 bg-neutral-900 text-neutral-200",
};

type Props = {
  tone?: Tone;
  title?: string;
  children?: ReactNode;
  onDismiss?: () => void;
};

export function ErrorBanner({ tone = "error", title, children, onDismiss }: Props) {
  return (
    <div
      role={tone === "error" ? "alert" : "status"}
      className={`rounded-md border p-3 text-sm ${TONE[tone]}`}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex-1">
          {title && <div className="font-medium">{title}</div>}
          {children && <div className={title ? "mt-1 text-xs opacity-90" : ""}>{children}</div>}
        </div>
        {onDismiss && (
          <button
            type="button"
            onClick={onDismiss}
            className="text-xs opacity-70 hover:opacity-100"
            aria-label="Dismiss"
          >
            ✕
          </button>
        )}
      </div>
    </div>
  );
}

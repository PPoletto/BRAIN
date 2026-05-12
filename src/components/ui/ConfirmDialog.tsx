import { useEffect, useRef, type ReactNode } from "react";
import { Button } from "./Button";

type Tone = "destructive" | "default";

type Props = {
  /** When false the dialog is unmounted entirely; no overlay, no DOM, no focus trap. */
  open: boolean;
  /** Dialog title, rendered as the focusable heading. Keep it action-shaped. */
  title: string;
  /** Body shown between title and the buttons. Rich children allowed so the
   *  caller can format lists, code, etc. for high-stakes confirmations. */
  children: ReactNode;
  /** Label for the confirm button. Defaults to "Confirm". For destructive
   *  operations write the exact verb ("Update vault templates"), not "Yes". */
  confirmLabel?: string;
  /** Label for the cancel button. Defaults to "Cancel". */
  cancelLabel?: string;
  /** Visual tone — `destructive` paints the confirm button red. */
  tone?: Tone;
  /** Loading state on the confirm button while the action is in flight. */
  loading?: boolean;
  /** Fired when the user accepts. The dialog stays open until the parent
   *  flips `open` to false — typically after the async action settles. */
  onConfirm: () => void;
  /** Fired when the user cancels (Esc, overlay click, or Cancel button). */
  onCancel: () => void;
};

/// In-app confirmation dialog used for destructive operations in the Danger
/// section of Settings. Replaces `window.confirm()` so:
///   - The dialog can never be silently suppressed by the host (some Tauri
///     builds disable native browser dialogs by default).
///   - The styling matches the rest of the BRAIN UI.
///   - The body can be rich (lists of files about to be overwritten, code
///     spans, multi-paragraph rationales) instead of a plain-text string.
///   - Focus is trapped on the cancel button by default — pressing Enter
///     without thinking does NOT trigger the destructive action; the user
///     has to tab over to the confirm button or click it deliberately.
///
/// Esc and overlay click both cancel. The confirm button keeps its own
/// loading state so the parent can keep the dialog open while the action
/// runs and the user sees the spinner.
export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel = "Confirm",
  cancelLabel = "Cancel",
  tone = "default",
  loading = false,
  onConfirm,
  onCancel,
}: Props) {
  // Stash the previously-focused element so we can restore it when the
  // dialog closes — without this, the focus snaps to <body> on close and
  // keyboard users lose their place in the surrounding page.
  const previouslyFocused = useRef<HTMLElement | null>(null);
  const cancelButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    previouslyFocused.current = document.activeElement as HTMLElement | null;
    // Defer the focus by one frame so the button is mounted by the time
    // we try to focus it. Without the rAF the focus call can silently
    // no-op on the first render.
    const raf = requestAnimationFrame(() => {
      cancelButtonRef.current?.focus();
    });
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape" && !loading) {
        e.preventDefault();
        onCancel();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("keydown", onKey);
      previouslyFocused.current?.focus();
    };
  }, [open, loading, onCancel]);

  if (!open) return null;

  return (
    <div
      role="presentation"
      // Clicking the dim overlay cancels the dialog. The inner panel
      // stops propagation so a click on the panel itself is not
      // misread as an overlay click.
      onClick={() => {
        if (!loading) onCancel();
      }}
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4 backdrop-blur-sm"
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirm-dialog-title"
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-md rounded-lg border border-neutral-700 bg-neutral-900 p-5 shadow-2xl"
      >
        <h2
          id="confirm-dialog-title"
          className={`text-base font-semibold ${
            tone === "destructive" ? "text-red-300" : "text-neutral-100"
          }`}
        >
          {title}
        </h2>
        <div className="mt-3 space-y-2 text-sm text-neutral-300">{children}</div>
        <div className="mt-5 flex justify-end gap-2">
          <Button
            ref={cancelButtonRef}
            variant="secondary"
            onClick={onCancel}
            disabled={loading}
          >
            {cancelLabel}
          </Button>
          <Button
            variant={tone === "destructive" ? "destructive" : "primary"}
            onClick={onConfirm}
            loading={loading}
          >
            {loading ? "Working…" : confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  );
}

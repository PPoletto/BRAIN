import { useEffect, useRef, useState, type ReactNode } from "react";

type Props = {
  /** Initial sidebar width in pixels. */
  initial?: number;
  /** Min/max in pixels. */
  min?: number;
  max?: number;
  /** localStorage key — width is persisted across sessions. */
  storageKey?: string;
  left: ReactNode;
  right: ReactNode;
  className?: string;
};

/// Two-column split with a draggable handle. Width persists per `storageKey`.
/// When the viewport is narrower than `min + 200`, the sidebar collapses to
/// `min` and the user can still resize back out via the handle.
export function ResizableSplit({
  initial = 280,
  min = 220,
  max = 520,
  storageKey,
  left,
  right,
  className = "",
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState<number>(() => {
    if (storageKey) {
      const raw = window.localStorage.getItem(storageKey);
      if (raw) {
        const parsed = parseInt(raw, 10);
        if (!Number.isNaN(parsed)) return Math.max(min, Math.min(max, parsed));
      }
    }
    return initial;
  });
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    if (!storageKey) return;
    window.localStorage.setItem(storageKey, String(width));
  }, [width, storageKey]);

  useEffect(() => {
    if (!dragging) return;
    function onMove(e: MouseEvent) {
      if (!containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const next = Math.max(min, Math.min(max, e.clientX - rect.left));
      setWidth(next);
    }
    function onUp() {
      setDragging(false);
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [dragging, min, max]);

  return (
    <div
      ref={containerRef}
      // `select-none` is applied ONLY while the user is dragging
      // the splitter handle — otherwise the page-wide
      // `user-select: none` cascades into the child panels and
      // makes the markdown body in Browse/Tier1 unselectable.
      // Pre-0.2.19 the class was always on, which caused Pascal's
      // "I cannot mark text to search it in a browser" report.
      // The drag itself still needs the suppression so the cursor
      // doesn't snag the text mid-drag, hence the conditional.
      className={`flex h-full w-full ${dragging ? "select-none" : ""} ${className}`}
      style={{ cursor: dragging ? "col-resize" : undefined }}
    >
      <div style={{ width }} className="h-full shrink-0 overflow-hidden">
        {left}
      </div>
      <div
        role="separator"
        aria-orientation="vertical"
        onMouseDown={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        className={`group h-full w-1.5 shrink-0 cursor-col-resize bg-neutral-800 transition-colors hover:bg-emerald-700 ${
          dragging ? "bg-emerald-600" : ""
        }`}
      />
      <div className="h-full min-w-0 flex-1 overflow-hidden">{right}</div>
    </div>
  );
}

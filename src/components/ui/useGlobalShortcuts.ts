import { useEffect } from "react";

export type Shortcut = {
  /** Key combo as `mod+k` (mod=Cmd on macOS, Ctrl elsewhere). */
  combo: string;
  description: string;
  action: () => void;
  /**
   * When true, the shortcut fires even if a text input has focus.
   * Used for modifier+number/comma/h/k navigation that the user expects
   * to work everywhere — including while the search bar is focused.
   * Plain-letter shortcuts default to `false` so typing into a text field
   * doesn't accidentally trigger anything.
   */
  globalEvenInInput?: boolean;
};

function matches(combo: string, e: KeyboardEvent): boolean {
  const parts = combo.toLowerCase().split("+");
  const want = {
    mod: parts.includes("mod"),
    shift: parts.includes("shift"),
    alt: parts.includes("alt"),
    key: parts[parts.length - 1],
  };
  const isMac = navigator.platform.toLowerCase().includes("mac");
  const modPressed = isMac ? e.metaKey : e.ctrlKey;
  if (want.mod !== modPressed) return false;
  if (want.shift !== e.shiftKey) return false;
  if (want.alt !== e.altKey) return false;
  return e.key.toLowerCase() === want.key;
}

function isTextInput(target: EventTarget | null): boolean {
  if (!target || !(target instanceof HTMLElement)) return false;
  return (
    target.tagName === "INPUT" ||
    target.tagName === "TEXTAREA" ||
    target.isContentEditable
  );
}

export function useGlobalShortcuts(shortcuts: Shortcut[]) {
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const inInput = isTextInput(e.target);
      for (const s of shortcuts) {
        if (!matches(s.combo, e)) continue;
        if (inInput && !s.globalEvenInInput) return;
        e.preventDefault();
        // Stop propagation so embedded canvases (cytoscape on the graph
        // tab) and other capture-phase listeners don't double-handle the
        // shortcut, which used to swallow Ctrl+1/Ctrl+2 on the graph view.
        e.stopPropagation();
        s.action();
        return;
      }
    }
    // Capture phase so we beat any in-page listeners (notably cytoscape's
    // canvas keydown handler that intercepts arrow keys + plain letters).
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [shortcuts]);
}

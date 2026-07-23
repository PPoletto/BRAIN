import { useEffect, useRef } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { MountState, TrayStatus } from "./commands";

export async function onMountState(
  handler: (status: TrayStatus) => void,
): Promise<UnlistenFn> {
  return listen<TrayStatus>("mount-state", (event) => handler(event.payload));
}

export async function onWikiChanged(
  handler: (payload: { commit_sha: string; files_changed: number }) => void,
): Promise<UnlistenFn> {
  return listen<{ commit_sha: string; files_changed: number }>("wiki-changed", (event) =>
    handler(event.payload),
  );
}

export async function onOnboardingProgress(
  handler: (payload: { step: string; percent: number; detail: string }) => void,
): Promise<UnlistenFn> {
  return listen<{ step: string; percent: number; detail: string }>("onboarding-progress", (event) =>
    handler(event.payload),
  );
}

export type LintIssue = { path: string; kind: string; message: string };
export type LintReport = { errors: LintIssue[]; warnings: LintIssue[] };

/// Listen for lint output emitted after each auto-commit attempt.
/// The watcher only emits this event when a lint pass produced
/// errors **or** warnings — silent passes don't fire it.
export async function onWikiLintReport(
  handler: (report: LintReport) => void,
): Promise<UnlistenFn> {
  return listen<LintReport>("wiki-lint-error", (event) =>
    handler(event.payload),
  );
}

/// Conflicts surfaced by the background auto-sync: repo-relative paths of
/// pages that now carry conflict markers. A manual "Sync now" reports
/// these in its own result — this event covers syncs the user didn't
/// trigger.
export async function onSyncConflicts(
  handler: (pages: string[]) => void,
): Promise<UnlistenFn> {
  return listen<string[]>("sync-conflicts", (event) => handler(event.payload));
}

/// Permanent background-sync failures (e.g. the remote is encrypted with
/// a different key). Transient failures (offline, auth hiccup) retry
/// silently and never fire this.
export async function onSyncError(
  handler: (message: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("sync-error", (event) => handler(event.payload));
}

/// Auto-refresh hook for any view that displays data derived from the
/// vault. Subscribes to two backend events and re-invokes `refetch`:
///
///  1. **`wiki-changed`** — fires after every successful auto-commit
///     (page added / edited / deleted, restore, hard-reset). The
///     primary "data has changed, re-read" signal.
///  2. **`mount-state` going non-mounted → `mounted-idle`** — handles
///     the auto-reconnect case after a disk yank, where no
///     `wiki-changed` fires but the vault is suddenly readable again.
///
/// Internally we keep `refetch` in a ref so callers can pass an inline
/// closure (with state-dependent capture) without having to wrap it in
/// `useCallback` — the subscription stays stable across renders.
///
/// The hook is intentionally narrow: it doesn't trigger on busy ↔ idle
/// transitions (which fire frequently) — only on the disconnected →
/// mounted-idle edge.
export function useDataRefresh(refetch: () => void | Promise<void>): void {
  const refetchRef = useRef(refetch);
  // Keep the ref pointing at the latest closure so we can subscribe
  // exactly once and still call into fresh state.
  useEffect(() => {
    refetchRef.current = refetch;
  });

  useEffect(() => {
    let unlistenWiki: UnlistenFn | undefined;
    let unlistenMount: UnlistenFn | undefined;
    const previousState: { state: MountState | null } = { state: null };

    void onWikiChanged(() => {
      void refetchRef.current();
    }).then((u) => {
      unlistenWiki = u;
    });

    void onMountState((status) => {
      const prev = previousState.state;
      const reconnected =
        status.state === "mounted-idle" &&
        prev !== "mounted-idle" &&
        prev !== "mounted-busy";
      previousState.state = status.state;
      if (reconnected) {
        void refetchRef.current();
      }
    }).then((u) => {
      unlistenMount = u;
    });

    return () => {
      unlistenWiki?.();
      unlistenMount?.();
    };
  }, []);
}

import { create } from "zustand";
import { commands } from "./commands";

/// Cached commit row, shape-identical to `commands.wikiHistory(N)[number]`.
/// Re-declared here (rather than re-imported) so the store stays
/// self-contained — `commands.ts` already exports the inferred type via the
/// `Awaited<ReturnType<...>>` trick in `WikiHistory.tsx`, but pulling it in
/// here would create a circular dependency in the type graph between the
/// route module and the lib module.
export type WikiHistoryEntry = {
  sha: string;
  ts: string;
  message: string;
  files_changed: number;
};

/// Number of commits the History view ever shows in the timeline. Matches
/// the literal `100` in `WikiHistory.tsx`. Lifting it here keeps the
/// prefetch and the consumer agreeing on the same limit so the user never
/// sees the cache rendering a subset of what the live fetch would.
export const WIKI_HISTORY_LIMIT = 100;

type WikiHistoryState = {
  /// Latest known commit timeline. `null` ⇒ never fetched (a renderer
  /// should distinguish this from `[]` ⇒ "we asked and the repo has no
  /// commits yet"). `[]` ⇒ fetched successfully, empty result.
  entries: WikiHistoryEntry[] | null;
  /// True while a prefetch / refresh is in flight. Used by the History
  /// view to gate the "Loading…" placeholder so prefetched data renders
  /// instantly on tab activation.
  loading: boolean;
  /// String form of the last error, if any. Cleared on the next
  /// successful fetch.
  error: string | null;
  /// One-shot guard. AppShell calls `prefetch()` on mount; subsequent
  /// remounts (HMR, route changes, mini-map toggles inside Tier3) must
  /// not re-fire the IPC. The `wiki-changed` event uses `invalidate()`
  /// to bypass this guard intentionally.
  prefetchStarted: boolean;
  /// Fetch the history once and cache it. Idempotent — second and later
  /// calls during the same session are no-ops while the cache is
  /// non-null. Use `invalidate()` to force a refresh.
  prefetch: () => void;
  /// Mark the cache as stale and re-fetch. Wired to the `wiki-changed`
  /// event so a fresh auto-commit refreshes the cached timeline before
  /// the user next visits the History tab.
  invalidate: () => void;
  /// Drop everything. Used at vault unmount / sign-out so a fresh mount
  /// doesn't show the previous vault's commits for a render frame.
  reset: () => void;
};

export const useWikiHistoryStore = create<WikiHistoryState>((set, get) => ({
  entries: null,
  loading: false,
  error: null,
  prefetchStarted: false,

  prefetch: () => {
    const { prefetchStarted, loading } = get();
    if (prefetchStarted || loading) return;
    set({ prefetchStarted: true, loading: true, error: null });
    commands
      .wikiHistory(WIKI_HISTORY_LIMIT)
      .then((entries) => {
        set({ entries, loading: false, error: null });
      })
      .catch((e: unknown) => {
        set({ loading: false, error: String(e) });
      });
  },

  invalidate: () => {
    // Don't clear `entries` — keep the stale data visible while the
    // refresh is in flight so the History timeline doesn't blink to
    // "Loading…" after every auto-commit. The cached rows are replaced
    // wholesale once the new fetch resolves.
    if (get().loading) return;
    set({ loading: true, error: null });
    commands
      .wikiHistory(WIKI_HISTORY_LIMIT)
      .then((entries) => {
        set({ entries, loading: false, error: null });
      })
      .catch((e: unknown) => {
        set({ loading: false, error: String(e) });
      });
  },

  reset: () => {
    set({
      entries: null,
      loading: false,
      error: null,
      prefetchStarted: false,
    });
  },
}));

import { Outlet } from "react-router-dom";
import { useEffect } from "react";
import { TopBar } from "./TopBar";
import { Sidebar } from "./Sidebar";
import { StatusBar } from "./StatusBar";
import { DefaultShortcuts } from "../ui/KeyboardShortcuts";
import { BackendToastsBridge } from "../ui/Toast";
import { useToast } from "../ui/toast-context";
import {
  onMountState,
  onSyncConflicts,
  onSyncError,
  onWikiChanged,
  onWikiLintReport,
} from "../../lib/events";
import { commands } from "../../lib/commands";
import { useAppState } from "../../lib/state";
import { useWikiHistoryStore } from "../../lib/wikiHistoryStore";

/// Persistent app shell rendered for every mounted-Brain route.
/// Hosts:
///   - TopBar (search, mount-status pill, settings shortcut)
///   - Sidebar (Browse/Search/Graph/History/Settings, collapsible)
///   - <Outlet/> (the active route)
///   - StatusBar (vault path, active ops, MCP indicator)
///   - DefaultShortcuts (global Ctrl+,/1/2/3/H/K)
///   - useBackendToasts (listens for Tauri "toast" events)
export function AppShell() {
  const setTray = useAppState((s) => s.setTray);
  const { push } = useToast();

  // Lint output from the auto-commit watcher: errors block the commit,
  // warnings ride along with successful commits. Both surface as toasts
  // so the user notices stale frontmatter / non-canonical wiki-links
  // without having to open Wiki history.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onWikiLintReport((report) => {
      if (report.errors.length > 0) {
        const first = report.errors[0];
        push({
          kind: "error",
          message: `Lint failed: ${report.errors.length} error${report.errors.length === 1 ? "" : "s"}`,
          detail: `${first.path}: ${first.message}`,
        });
      }
      // Group warnings by kind so we don't spam the user with a toast
      // per file. "non-canonical-wiki-link" gets a single rolled-up
      // toast pointing them at the rebuild fix.
      if (report.warnings.length > 0) {
        const byKind = new Map<string, number>();
        for (const w of report.warnings) {
          byKind.set(w.kind, (byKind.get(w.kind) ?? 0) + 1);
        }
        const summary = [...byKind.entries()]
          .map(([kind, n]) => `${n}× ${kind}`)
          .join(", ");
        push({
          kind: "warning",
          message: "Wiki lint warnings",
          detail: summary,
        });
      }
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      unlisten?.();
    };
  }, [push]);

  // Background auto-sync outcomes. The scheduler runs whether or not the
  // Git-sync tab is open, so its conflicts and permanent errors (e.g.
  // "remote is keyed differently") surface here as global toasts instead
  // of dying in the log.
  useEffect(() => {
    let unlistenConflicts: (() => void) | undefined;
    let unlistenError: (() => void) | undefined;
    onSyncConflicts((pages) => {
      push({
        kind: "warning",
        message: `Auto-sync merged with ${pages.length} conflict${pages.length === 1 ? "" : "s"}`,
        detail: `Resolve the markers, then sync again: ${pages.join(", ")}`,
      });
    }).then((u) => {
      unlistenConflicts = u;
    });
    onSyncError((message) => {
      push({ kind: "error", message: "Auto-sync failed", detail: message });
    }).then((u) => {
      unlistenError = u;
    });
    return () => {
      unlistenConflicts?.();
      unlistenError?.();
    };
  }, [push]);

  // Two paths into the same store:
  //
  //   1. `mount-state` events fire when the backend transitions between
  //      states. Great for live updates while the window stays open.
  //   2. `tray_status` is a one-shot pull on mount — protects against the
  //      race where the window reload (or a slow first paint) misses the
  //      single transition event the tray loop emits per state change.
  //      Without this, a window reload after onboarding shows
  //      "disconnected" until something next changes (which may be never).
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    commands
      .trayStatus()
      .then((status) => {
        if (!cancelled) setTray(status);
      })
      .catch(() => {
        // No-op — backend not ready yet, the event listener below will
        // fill the state in once it does emit.
      });

    onMountState((status) => {
      if (!cancelled) setTray(status);
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [setTray]);

  // Wiki-history prefetch. The History tab's `commands.wikiHistory(100)`
  // IPC walks the Git log + computes per-commit file counts, which can
  // take ~1–2 s on a vault with a few thousand commits. Fetching it
  // once when the shell mounts (i.e. as soon as the vault is up) means
  // the user's *first* visit to the History tab renders from the
  // already-warm cache — even if they never click into History at all,
  // the cost is one background IPC at shell-mount time.
  //
  // The store is also subscribed to `wiki-changed` below so a fresh
  // auto-commit refreshes the cached timeline before the next visit.
  const prefetchHistory = useWikiHistoryStore((s) => s.prefetch);
  const invalidateHistory = useWikiHistoryStore((s) => s.invalidate);

  useEffect(() => {
    prefetchHistory();
  }, [prefetchHistory]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void onWikiChanged(() => {
      invalidateHistory();
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      unlisten?.();
    };
  }, [invalidateHistory]);

  return (
    <div className="flex h-screen w-screen flex-col bg-neutral-950">
      <TopBar />
      <div className="flex min-h-0 flex-1">
        <Sidebar />
        <main className="flex min-w-0 flex-1 flex-col bg-neutral-950">
          <Outlet />
        </main>
      </div>
      <StatusBar />
      <DefaultShortcuts />
      <BackendToastsBridge />
    </div>
  );
}

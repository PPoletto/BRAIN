import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

export type UpdateAvailability = {
  available: boolean;
  version: string | null;
  current_version: string;
  notes: string | null;
  date: string | null;
  skipped: boolean;
};

type UpdateStore = {
  availability: UpdateAvailability | null;
  dismissed: boolean;
  /// True while an update check is in flight — used to drive a spinner on
  /// the manual "click the version in the status bar" trigger.
  checking: boolean;
  /// True while the downloader is running; backend triggers an app restart
  /// on success so the value flips back to false implicitly.
  installing: boolean;

  setDismissed: (v: boolean) => void;

  /// Pulls the latest update metadata from the backend. Returns the
  /// availability so the caller can show a toast — UpdatePrompt re-renders
  /// from the store regardless.
  check: () => Promise<UpdateAvailability | null>;

  /// Downloads + verifies + installs the update. The backend calls
  /// `app.restart()` on success, so this promise typically does not
  /// resolve.
  applyNow: () => Promise<void>;

  /// Persists the version string on the backend's skip-list so future
  /// checks ignore it until a newer version ships.
  skip: () => Promise<void>;
};

export const useUpdateStore = create<UpdateStore>((set, get) => ({
  availability: null,
  dismissed: false,
  checking: false,
  installing: false,

  setDismissed: (v) => set({ dismissed: v }),

  check: async () => {
    set({ checking: true });
    try {
      const a = await invoke<UpdateAvailability>("check_update");
      set({ availability: a, dismissed: false });
      return a;
    } catch (e: unknown) {
      // Spec S04: offline checks fail silently. We still surface the
      // failure to the caller so a manual "Check for updates" click can
      // toast it; the auto-check at startup ignores the rejection.
      console.debug("update check failed", e);
      throw e;
    } finally {
      set({ checking: false });
    }
  },

  applyNow: async () => {
    set({ installing: true });
    try {
      await invoke("apply_update");
      // The backend restarts the app — execution typically does not return
      // here, but if it does we leave `installing` in place so the UI
      // stays disabled until the restart actually happens.
    } catch (e: unknown) {
      set({ installing: false });
      throw e;
    }
  },

  skip: async () => {
    const a = get().availability;
    if (!a?.version) return;
    try {
      await invoke("skip_update", { version: a.version });
    } finally {
      set({ dismissed: true });
    }
  },
}));

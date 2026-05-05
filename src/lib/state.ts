import { create } from "zustand";
import type { TrayStatus } from "./commands";

type AppState = {
  tray: TrayStatus;
  setTray: (status: TrayStatus) => void;
};

export const useAppState = create<AppState>((set) => ({
  tray: {
    state: "disconnected",
    tooltip: "BRAIN disconnected",
    vault_path: null,
    active_operations: 0,
    message: null,
  },
  setTray: (status) => set({ tray: status }),
}));

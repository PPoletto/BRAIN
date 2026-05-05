import React from "react";
import ReactDOM from "react-dom/client";
import { RouterProvider } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { router } from "./App";
import { UpdatePrompt } from "./components/UpdatePrompt";
import { ToastProvider } from "./components/ui/Toast";
import "./styles/globals.css";

// The tray menu emits `navigate-to` events with the target pathname when
// the user picks "Open Brain Viewer", "Settings…", "Wiki history…" etc.
// `createBrowserRouter` does not react to `window.location.hash`, so we
// have to drive navigation programmatically.
listen<string>("navigate-to", (event) => {
  void router.navigate(event.payload);
}).catch((err) => console.warn("navigate-to listener failed to attach", err));

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ToastProvider>
      <RouterProvider router={router} />
      <UpdatePrompt />
    </ToastProvider>
  </React.StrictMode>,
);

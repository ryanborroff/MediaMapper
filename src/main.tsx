import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";

// The window starts hidden (tauri.conf.json) so it never flashes the wrong
// colour before the page paints. Scheduled before rendering, so the window
// still appears if rendering throws. Outside Tauri there is no window to show.
requestAnimationFrame(() => {
  requestAnimationFrame(() => {
    try {
      void getCurrentWindow().show().catch(() => {});
    } catch {
      // Not running inside Tauri.
    }
  });
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

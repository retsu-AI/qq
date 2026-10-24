import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { start } from "./lib/store";
import "./styles.css";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { configurePlatform, watchMobileViewport } from "./lib/platform";

configurePlatform(isTauri() ? await invoke("client_platform") : "desktop");
watchMobileViewport();
void start();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

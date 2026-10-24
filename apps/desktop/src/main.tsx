import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { start } from "./lib/store";
import "./styles.css";
import { invoke, isTauri } from "@tauri-apps/api/core";
import {
  configurePlatform,
  nativePlatform,
  watchMobileViewport,
} from "./lib/platform";

const root = document.getElementById("root")!;
const identity = await nativePlatform(isTauri(), () =>
  invoke("client_platform"),
);

if ("error" in identity) {
  // Say why instead of leaving a blank window.
  const message = document.createElement("p");
  message.setAttribute("role", "alert");
  message.style.padding = "24px";
  message.textContent = `QQ could not start: ${identity.error}`;
  root.replaceChildren(message);
} else {
  configurePlatform(identity.name);
  watchMobileViewport();
  void start();

  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

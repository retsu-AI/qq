import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Tauri expects a fixed dev port and serves the built `dist/` in release.
export default defineConfig(({ mode }) => {
  // `tauri android dev` / `tauri ios dev` set this to an address the device can
  // reach; desktop development leaves it unset and stays on localhost.
  const host = loadEnv(mode, ".", "TAURI_").TAURI_DEV_HOST || undefined;
  return {
    plugins: [react(), tailwindcss()],
    clearScreen: false,
    server: {
      port: 1420,
      strictPort: true,
      host,
      hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
      watch: { ignored: ["**/src-tauri/**"] },
    },
    build: {
      target: "es2022",
      sourcemap: false,
    },
  };
});

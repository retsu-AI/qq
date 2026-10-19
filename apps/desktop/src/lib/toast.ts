import { useSyncExternalStore } from "react";

export interface Toast {
  id: number;
  text: string;
  tone: "info" | "error";
}

// One short-lived message at a time, for results of commands that have no
// other visible effect (an editor opening, a commit landing).
let current: Toast | null = null;
let timer: number | undefined;
let next = 0;
const listeners = new Set<() => void>();

function changed() {
  for (const listener of listeners) listener();
}

export function toast(text: string, tone: Toast["tone"] = "info") {
  current = { id: ++next, text, tone };
  window.clearTimeout(timer);
  timer = window.setTimeout(dismissToast, tone === "error" ? 6000 : 3000);
  changed();
}

export function dismissToast() {
  current = null;
  changed();
}

export function useToast(): Toast | null {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
  );
}

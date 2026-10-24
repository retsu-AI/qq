import { useSyncExternalStore } from "react";

// Which sessions show their side-question panel. Asking opens it; closing
// hides it until the next question.
const open = new Set<string>();
let version = 0;
const listeners = new Set<() => void>();

function changed() {
  version++;
  for (const listener of listeners) listener();
}

export function showSidePanel(sessionId: string) {
  if (open.has(sessionId)) return;
  open.add(sessionId);
  changed();
}

export function hideSidePanel(sessionId: string) {
  if (open.delete(sessionId)) changed();
}

export function useSidePanel(sessionId: string): boolean {
  useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => version,
  );
  return open.has(sessionId);
}

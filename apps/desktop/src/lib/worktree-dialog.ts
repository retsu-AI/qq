import { useSyncExternalStore } from "react";

// The project a "New worktree session" dialog is open for, if any.
let current: string | null = null;
const listeners = new Set<() => void>();

export function showWorktreeDialog(projectId: string) {
  current = projectId;
  for (const listener of listeners) listener();
}

export function hideWorktreeDialog() {
  current = null;
  for (const listener of listeners) listener();
}

export function useWorktreeDialog(): string | null {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
  );
}

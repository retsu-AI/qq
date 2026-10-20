import { useSyncExternalStore } from "react";

// Which session's goal panel is open, and the goal dialog's draft.
export interface GoalDialogState {
  projectId: string;
  sessionId: string;
  /** Prefilled objective, e.g. from `/goal <objective>`. */
  objective?: string;
  /** Edit the current goal instead of setting a new one. */
  edit: boolean;
}

let panel: string | null = null;
let dialog: GoalDialogState | null = null;
let version = 0;
const listeners = new Set<() => void>();

function changed() {
  version++;
  for (const listener of listeners) listener();
}

export function openGoalPanel(sessionId: string) {
  panel = sessionId;
  changed();
}

export function closeGoalPanel() {
  panel = null;
  changed();
}

export function toggleGoalPanel(sessionId: string) {
  panel = panel === sessionId ? null : sessionId;
  changed();
}

export function openGoalDialog(state: GoalDialogState) {
  dialog = state;
  changed();
}

export function closeGoalDialog() {
  dialog = null;
  changed();
}

export function useGoalUi(): { panel: string | null; dialog: GoalDialogState | null } {
  useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => version,
  );
  return { panel, dialog };
}

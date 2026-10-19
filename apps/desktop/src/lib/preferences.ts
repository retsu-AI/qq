import { useSyncExternalStore } from "react";

export interface Preferences {
  theme: "system" | "light" | "dark";
  fontSize: number;
  notifications: boolean;
}

export const MIN_FONT_SIZE = 12;
export const MAX_FONT_SIZE = 22;
const KEY = "qq.preferences";

export function parsePreferences(json: string | null): Preferences {
  let value: Partial<Preferences> = {};
  try {
    value = JSON.parse(json ?? "{}") ?? {};
  } catch {
    /* Recover preferences only. */
  }
  return {
    theme: ["system", "light", "dark"].includes(value.theme ?? "")
      ? value.theme!
      : "system",
    fontSize:
      typeof value.fontSize === "number" &&
      Number.isInteger(value.fontSize) &&
      value.fontSize >= MIN_FONT_SIZE &&
      value.fontSize <= MAX_FONT_SIZE
        ? value.fontSize
        : 14,
    notifications: value.notifications === true,
  };
}

export function applyPreferences(value: Preferences) {
  document.documentElement.dataset.theme = value.theme;
  document.documentElement.style.setProperty(
    "--desktop-font-size",
    `${value.fontSize}px`,
  );
}

function read(): Preferences {
  try {
    return parsePreferences(localStorage.getItem(KEY));
  } catch {
    return parsePreferences(null);
  }
}

// One copy for the menu bar, settings and shortcuts. The native side only
// needs the notification flag, which callers push with the api.
let current = read();
const listeners = new Set<() => void>();

export function getPreferences(): Preferences {
  return current;
}

export function setPreferences(next: Preferences) {
  current = next;
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    /* Preferences only; the in-memory value still applies. */
  }
  applyPreferences(next);
  for (const listener of listeners) listener();
}

export function updatePreferences(change: Partial<Preferences>) {
  setPreferences({ ...current, ...change });
}

/** Text zoom in whole pixels, clamped to the supported range. */
export function zoom(step: -1 | 0 | 1) {
  const fontSize =
    step === 0
      ? 14
      : Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, current.fontSize + step));
  updatePreferences({ fontSize });
}

export function usePreferences(): Preferences {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
  );
}

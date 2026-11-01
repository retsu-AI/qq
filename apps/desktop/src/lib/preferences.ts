import { useSyncExternalStore } from "react";

export interface Preferences {
  theme: "system" | "light" | "dark";
  fontSize: number;
  notifications: boolean;
  /** The session's side panel: its details, its changes, or hidden. */
  sidePanel: SidePanel | null;
  /** Installed font family for the interface; null uses the default. */
  interfaceFont: string | null;
  /** Installed font family for code; null uses the default. */
  codeFont: string | null;
}

export type SidePanel = "details" | "changes";

/** Fonts that ship with Windows. The first entry is the default. */
export const INTERFACE_FONTS = [
  "Segoe UI Variable Text",
  "Segoe UI",
  "Arial",
  "Verdana",
  "Tahoma",
  "Georgia",
  "Calibri",
];
export const CODE_FONTS = [
  "Cascadia Code",
  "Cascadia Mono",
  "Consolas",
  "Lucida Console",
  "Courier New",
];
const DEFAULT_SANS = '"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif';
const DEFAULT_MONO =
  '"Cascadia Code", "Cascadia Mono", Consolas, ui-monospace, monospace';

/**
 * A font family name safe to place in CSS, or null. Names are letters,
 * digits, spaces, dots and hyphens, as installed font names are.
 */
export function fontName(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const name = value.trim().replace(/\s+/g, " ");
  return /^[\p{L}\p{N} .-]{1,64}$/u.test(name) ? name : null;
}

/** A CSS font stack: the chosen font, then the default when it is missing. */
export function fontStack(name: string | null, code: boolean): string {
  const fallback = code ? DEFAULT_MONO : DEFAULT_SANS;
  return name ? `"${name}", ${fallback}` : fallback;
}

export const MIN_FONT_SIZE = 12;
export const MAX_FONT_SIZE = 22;
const KEY = "qq.preferences";

export function parsePreferences(json: string | null): Preferences {
  let value: Partial<Preferences> & { changesPanel?: unknown } = {};
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
    sidePanel:
      value.sidePanel === "details" || value.sidePanel === "changes"
        ? value.sidePanel
        : // Saved before the details panel existed.
          value.changesPanel === true
          ? "changes"
          : null,
    interfaceFont: fontName(value.interfaceFont),
    codeFont: fontName(value.codeFont),
  };
}

export function applyPreferences(value: Preferences) {
  document.documentElement.dataset.theme = value.theme;
  document.documentElement.style.setProperty(
    "--desktop-font-size",
    `${value.fontSize}px`,
  );
  document.documentElement.style.setProperty(
    "--ui-font",
    fontStack(value.interfaceFont, false),
  );
  document.documentElement.style.setProperty(
    "--code-font",
    fontStack(value.codeFont, true),
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

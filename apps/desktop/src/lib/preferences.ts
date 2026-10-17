export interface Preferences {
  theme: "system" | "light" | "dark";
  fontSize: number;
  notifications: boolean;
}
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
      value.fontSize >= 12 &&
      value.fontSize <= 22
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

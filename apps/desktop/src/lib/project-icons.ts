// Per-project sidebar icons, chosen by the user and kept on this desktop.
import { useSyncExternalStore } from "react";

export const ICON_COLORS = [
  "blue",
  "violet",
  "pink",
  "red",
  "orange",
  "green",
  "teal",
  "gray",
] as const;
export type IconColor = (typeof ICON_COLORS)[number];

export const ICON_SYMBOLS = [
  "folder",
  "code",
  "terminal",
  "globe",
  "box",
  "flask",
  "rocket",
  "database",
  "book",
  "gamepad",
  "cpu",
  "palette",
  "smartphone",
  "server",
  "wrench",
  "leaf",
] as const;
export type IconSymbol = (typeof ICON_SYMBOLS)[number];

/** Emojis offered with one click; any other one can be typed. */
export const ICON_EMOJIS = [
  "🚀", "⚡", "🔥", "✨", "⭐", "🌟", "💡", "🎯", "🧠", "🤖",
  "💻", "🖥️", "📱", "⌨️", "🛠️", "🔧", "⚙️", "🧪", "🔬", "🧩",
  "📦", "🗂️", "📁", "📝", "📚", "📊", "📈", "🗄️", "🔒", "🔑",
  "🌐", "☁️", "🌍", "🏠", "🏗️", "🎮", "🕹️", "🎨", "🎵", "🎬",
  "🐍", "🦀", "🐳", "🐙", "🦊", "🐱", "🐶", "🦄", "🐝", "🌱",
  "🌲", "🍀", "🌊", "🌙", "☀️", "❄️", "🍕", "☕", "💎", "🏆",
];

export type ProjectIcon =
  | { kind: "initials"; color: IconColor }
  | { kind: "symbol"; color: IconColor; symbol: IconSymbol }
  | { kind: "emoji"; color: IconColor; emoji: string }
  | { kind: "image"; image: string };

/** Uploaded images are stored at this square size (2x a 32px badge). */
export const IMAGE_SIZE = 64;
/** A 64px PNG is a few KiB; anything far larger is not one we made. */
const MAX_IMAGE_CHARS = 200_000;
const MAX_ICONS = 256;
const KEY = "qq.projectIcons";

export const DEFAULT_ICON: ProjectIcon = { kind: "initials", color: "blue" };

/** A single emoji (one grapheme, not plain text), or null. */
export function emojiValue(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const text = value.trim();
  if (!text || text.length > 16) return null;
  const graphemes = [...new Intl.Segmenter().segment(text)];
  return graphemes.length === 1 &&
    /\p{Extended_Pictographic}|\p{Regional_Indicator}/u.test(text)
    ? text
    : null;
}

function color(value: unknown): IconColor {
  return ICON_COLORS.includes(value as IconColor)
    ? (value as IconColor)
    : "blue";
}

function icon(value: unknown): ProjectIcon | null {
  if (typeof value !== "object" || value === null) return null;
  const entry = value as Record<string, unknown>;
  switch (entry.kind) {
    case "initials":
      return { kind: "initials", color: color(entry.color) };
    case "symbol":
      return ICON_SYMBOLS.includes(entry.symbol as IconSymbol)
        ? {
            kind: "symbol",
            color: color(entry.color),
            symbol: entry.symbol as IconSymbol,
          }
        : null;
    case "emoji": {
      const emoji = emojiValue(entry.emoji);
      return emoji ? { kind: "emoji", color: color(entry.color), emoji } : null;
    }
    case "image":
      // Only raster data URLs this app produced; never a remote URL.
      return typeof entry.image === "string" &&
        entry.image.length <= MAX_IMAGE_CHARS &&
        /^data:image\/png;base64,[A-Za-z0-9+/]+=*$/.test(entry.image)
        ? { kind: "image", image: entry.image }
        : null;
    default:
      return null;
  }
}

/** Icons by project path; invalid entries are dropped. */
export function parseProjectIcons(json: string | null): Record<string, ProjectIcon> {
  const icons: Record<string, ProjectIcon> = Object.create(null);
  let value: unknown;
  try {
    value = JSON.parse(json ?? "{}");
  } catch {
    return icons;
  }
  if (typeof value !== "object" || value === null) return icons;
  for (const [path, entry] of Object.entries(value).slice(0, MAX_ICONS)) {
    const parsed = icon(entry);
    if (parsed) icons[path] = parsed;
  }
  return icons;
}

function read(): Record<string, ProjectIcon> {
  try {
    return parseProjectIcons(localStorage.getItem(KEY));
  } catch {
    return parseProjectIcons(null);
  }
}

let current = read();
const listeners = new Set<() => void>();

/** Saves or (with null) resets a project's icon. Throws when storage fails. */
export function setProjectIcon(path: string, value: ProjectIcon | null) {
  const next = { ...current };
  if (value) next[path] = value;
  else delete next[path];
  localStorage.setItem(KEY, JSON.stringify(next));
  current = next;
  for (const listener of listeners) listener();
}

export function useProjectIcons(): Record<string, ProjectIcon> {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
  );
}

/** Center-crops an image file to a small square PNG data URL. */
export async function imageIcon(file: File): Promise<string> {
  try {
    // A data URL, not a blob URL: the app's CSP allows only data: images.
    const source = await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
    const image = new Image();
    image.src = source;
    await image.decode();
    // SVGs without a size decode as 0x0; draw them at the target size.
    const width = image.naturalWidth || IMAGE_SIZE;
    const height = image.naturalHeight || IMAGE_SIZE;
    const side = Math.min(width, height);
    const canvas = document.createElement("canvas");
    canvas.width = IMAGE_SIZE;
    canvas.height = IMAGE_SIZE;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("Images can't be drawn here.");
    context.imageSmoothingQuality = "high";
    context.drawImage(
      image,
      (width - side) / 2,
      (height - side) / 2,
      side,
      side,
      0,
      0,
      IMAGE_SIZE,
      IMAGE_SIZE,
    );
    return canvas.toDataURL("image/png");
  } catch (error) {
    throw new Error(
      error instanceof Error && error.message.startsWith("Images")
        ? error.message
        : "That file couldn't be read as an image.",
    );
  }
}

import { useEffect, useRef, useState, type CSSProperties } from "react";
import clsx from "clsx";
import {
  Book,
  Box,
  Code,
  Cpu,
  Database,
  FlaskConical,
  Folder,
  Gamepad2,
  GitBranch,
  Globe,
  ImageUp,
  Leaf,
  Palette,
  Rocket,
  Server,
  Smartphone,
  SquareTerminal,
  Wrench,
  type LucideIcon,
} from "lucide-react";
import {
  DEFAULT_ICON,
  ICON_COLORS,
  ICON_EMOJIS,
  ICON_SYMBOLS,
  emojiValue,
  imageIcon,
  setProjectIcon,
  type IconColor,
  type IconSymbol,
  type ProjectIcon as Icon,
} from "../lib/project-icons";
import type { ProjectInfo } from "../lib/types";

/** Mid-tone hues readable on both the light and dark palettes. */
const HUES: Record<IconColor, string> = {
  blue: "#4f7ff0",
  violet: "#9466e8",
  pink: "#d9558f",
  red: "#e0584c",
  orange: "#dd8a2c",
  green: "#2ea86b",
  teal: "#1f9e9c",
  gray: "#7d848f",
};

const SYMBOLS: Record<IconSymbol, LucideIcon> = {
  folder: Folder,
  code: Code,
  terminal: SquareTerminal,
  globe: Globe,
  box: Box,
  flask: FlaskConical,
  rocket: Rocket,
  database: Database,
  book: Book,
  gamepad: Gamepad2,
  cpu: Cpu,
  palette: Palette,
  smartphone: Smartphone,
  server: Server,
  wrench: Wrench,
  leaf: Leaf,
};

function tint(color: IconColor): CSSProperties {
  return {
    color: HUES[color],
    backgroundColor: `color-mix(in srgb, ${HUES[color]} 16%, transparent)`,
  };
}

/** A project's badge: its chosen icon, or its initial. Worktrees show a branch. */
export function ProjectBadge({
  project,
  icon,
  size = "md",
}: {
  project: Pick<ProjectInfo, "name" | "worktree">;
  icon: Icon | undefined;
  size?: "sm" | "md";
}) {
  const box = size === "md" ? "h-8 w-8 rounded-lg" : "h-6 w-6 rounded-md";
  const glyph = size === "md" ? "h-4 w-4" : "h-3.5 w-3.5";
  const value = icon ?? DEFAULT_ICON;
  if (value.kind === "image")
    return (
      <img
        src={value.image}
        alt=""
        draggable={false}
        className={clsx(box, "shrink-0 object-cover")}
      />
    );
  const Symbol = value.kind === "symbol" ? SYMBOLS[value.symbol] : null;
  return (
    <span
      aria-hidden
      style={tint(value.color)}
      className={clsx(
        box,
        "flex shrink-0 items-center justify-center font-semibold uppercase",
        size === "md" ? "text-ui" : "text-caption",
      )}
    >
      {value.kind === "emoji" ? (
        <span className={size === "md" ? "text-body" : "text-small"}>
          {value.emoji}
        </span>
      ) : Symbol ? (
        <Symbol className={glyph} />
      ) : project.worktree && !icon ? (
        <GitBranch className={glyph} />
      ) : (
        project.name.slice(0, 1)
      )}
    </span>
  );
}

type Tab = "initials" | "symbol" | "emoji" | "image";

/** Chooses a project's sidebar icon. Kept on this desktop only. */
export function IconDialog({
  project,
  icon,
  close,
}: {
  project: ProjectInfo;
  icon: Icon | undefined;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const file = useRef<HTMLInputElement>(null);
  const start = icon ?? DEFAULT_ICON;
  // The tabs only offer choices; `selected` is the one icon chosen, shown in
  // the preview on every tab and saved by Save.
  const [selected, setSelected] = useState<Icon>(start);
  const [tab, setTab] = useState<Tab>(start.kind);
  // Kept while an image is selected, for the next symbol, emoji or initials.
  const [color, setColor] = useState<IconColor>(
    start.kind === "image" ? "blue" : start.color,
  );
  const [typed, setTyped] = useState("");
  const [image, setImage] = useState(start.kind === "image" ? start.image : "");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
    // showModal focuses the first tab; start on the selected one.
    dialog.current
      ?.querySelector<HTMLButtonElement>('[role="tab"][aria-selected="true"]')
      ?.focus();
  }, []);

  const pickColor = (value: IconColor) => {
    setColor(value);
    if (selected.kind !== "image") setSelected({ ...selected, color: value });
  };
  const chosenSymbol = selected.kind === "symbol" ? selected.symbol : null;
  const chosenEmoji = selected.kind === "emoji" ? selected.emoji : null;

  const save = (value: Icon | null) => {
    try {
      setProjectIcon(project.path, value);
      close();
    } catch {
      setError("The icon couldn't be saved on this desktop.");
    }
  };

  const tabs: Array<[Tab, string]> = [
    ["initials", "Initials"],
    ["symbol", "Symbol"],
    ["emoji", "Emoji"],
    ["image", "Image"],
  ];

  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="icon-title"
      className="m-auto w-[440px] max-w-[92vw] rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <form
        className="px-6 pb-5 pt-5"
        onSubmit={(event) => {
          event.preventDefault();
          save(selected);
        }}
      >
        <div className="flex items-center gap-3">
          <ProjectBadge project={project} icon={selected} />
          <div className="min-w-0">
            <h2 id="icon-title" className="text-title font-semibold">
              Project icon
            </h2>
            <p className="truncate text-small text-muted">{project.name}</p>
          </div>
        </div>

        <div
          role="tablist"
          aria-label="Icon type"
          className="mt-4 flex rounded-lg border border-line-strong bg-raised p-0.5"
        >
          {tabs.map(([value, label]) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={tab === value}
              onClick={() => setTab(value)}
              className={clsx(
                "h-7 flex-1 rounded-md text-ui",
                tab === value
                  ? "bg-active text-fg shadow-sm"
                  : "text-muted hover:text-fg",
              )}
            >
              {label}
            </button>
          ))}
        </div>

        <div className="mt-4 min-h-[124px] text-ui">
          {tab === "symbol" && (
            <div
              role="radiogroup"
              aria-label="Symbol"
              className="grid grid-cols-8 gap-1"
            >
              {ICON_SYMBOLS.map((value) => {
                const Symbol = SYMBOLS[value];
                return (
                  <button
                    key={value}
                    type="button"
                    role="radio"
                    aria-checked={chosenSymbol === value}
                    aria-label={value}
                    title={value}
                    onClick={() =>
                      setSelected({ kind: "symbol", color, symbol: value })
                    }
                    className={clsx(
                      "flex h-9 items-center justify-center rounded-md",
                      chosenSymbol === value
                        ? "bg-active text-fg ring-1 ring-accent"
                        : "text-muted hover:bg-hover hover:text-fg",
                    )}
                  >
                    <Symbol className="h-4 w-4" />
                  </button>
                );
              })}
            </div>
          )}
          {tab === "emoji" && (
            <div>
              <div
                role="radiogroup"
                aria-label="Emoji"
                className="grid max-h-[156px] grid-cols-10 gap-1 overflow-y-auto pr-1"
              >
                {ICON_EMOJIS.map((value) => (
                  <button
                    key={value}
                    type="button"
                    role="radio"
                    aria-checked={chosenEmoji === value}
                    aria-label={value}
                    onClick={() => {
                      setTyped("");
                      setSelected({ kind: "emoji", color, emoji: value });
                    }}
                    className={clsx(
                      "flex h-9 items-center justify-center rounded-md text-title",
                      chosenEmoji === value
                        ? "bg-active ring-1 ring-accent"
                        : "hover:bg-hover",
                    )}
                  >
                    {value}
                  </button>
                ))}
              </div>
              <label className="mt-3 flex items-center gap-2 text-muted">
                <span>Or any emoji (Win + .)</span>
                <input
                  value={typed}
                  onChange={(event) => {
                    setTyped(event.target.value);
                    const value = emojiValue(event.target.value);
                    if (value) setSelected({ kind: "emoji", color, emoji: value });
                  }}
                  aria-invalid={typed.trim() !== "" && !emojiValue(typed)}
                  maxLength={16}
                  className="h-8 w-16 rounded-lg border border-line-strong bg-raised px-2 text-center text-body text-fg outline-none focus:border-accent/60 focus-visible:outline-none"
                />
              </label>
            </div>
          )}
          {tab === "image" && (
            <div>
              <p className="text-muted">
                PNG, JPG, SVG or WebP. It's cropped to a square and a small
                copy is kept, so the original can move.
              </p>
              <input
                ref={file}
                type="file"
                accept="image/png,image/jpeg,image/svg+xml,image/webp"
                className="hidden"
                onChange={async (event) => {
                  const chosen = event.target.files?.[0];
                  event.target.value = "";
                  if (!chosen) return;
                  try {
                    const uploaded = await imageIcon(chosen);
                    setImage(uploaded);
                    setSelected({ kind: "image", image: uploaded });
                    setError(null);
                  } catch (failure) {
                    setError(String((failure as Error).message ?? failure));
                  }
                }}
              />
              <div className="mt-3 flex items-center gap-3">
                {image && (
                  <button
                    type="button"
                    role="radio"
                    aria-checked={selected.kind === "image"}
                    aria-label="Uploaded image"
                    onClick={() => setSelected({ kind: "image", image })}
                    className={clsx(
                      "rounded-lg p-1",
                      selected.kind === "image"
                        ? "bg-active ring-1 ring-accent"
                        : "hover:bg-hover",
                    )}
                  >
                    <img src={image} alt="" className="h-8 w-8 rounded-md object-cover" />
                  </button>
                )}
                <button
                  type="button"
                  onClick={() => file.current?.click()}
                  className="flex h-8 items-center gap-2 rounded-lg border border-line-strong px-3 hover:bg-hover"
                >
                  <ImageUp className="h-4 w-4" />
                  {image ? "Choose another image…" : "Choose image…"}
                </button>
              </div>
            </div>
          )}
          {tab === "initials" && (
            <div className="flex items-center gap-3">
              <button
                type="button"
                role="radio"
                aria-checked={selected.kind === "initials"}
                aria-label="Use initials"
                onClick={() => setSelected({ kind: "initials", color })}
                className={clsx(
                  "rounded-lg p-1",
                  selected.kind === "initials"
                    ? "bg-active ring-1 ring-accent"
                    : "hover:bg-hover",
                )}
              >
                <ProjectBadge
                  project={{ name: project.name, worktree: null }}
                  icon={{ kind: "initials", color }}
                />
              </button>
              <p className="text-muted">
                The first letter of the project's name, in the color below.
              </p>
            </div>
          )}
          {tab !== "image" && (
            <div
              role="radiogroup"
              aria-label="Color"
              className="mt-4 flex gap-2"
            >
              {ICON_COLORS.map((value) => (
                <button
                  key={value}
                  type="button"
                  role="radio"
                  aria-checked={color === value}
                  aria-label={value}
                  title={value}
                  onClick={() => pickColor(value)}
                  style={{ backgroundColor: HUES[value] }}
                  className={clsx(
                    "h-6 w-6 rounded-full",
                    color === value &&
                      "ring-2 ring-fg ring-offset-2 ring-offset-bg",
                  )}
                />
              ))}
            </div>
          )}
        </div>

        {error && (
          <p
            role="alert"
            className="mt-3 rounded-md bg-danger-soft px-3 py-2 text-ui text-danger"
          >
            {error}
          </p>
        )}

        <div className="mt-5 flex items-center gap-2">
          <button
            type="button"
            onClick={() => save(null)}
            disabled={!icon}
            className="h-8 rounded-lg px-3 text-ui text-muted hover:bg-hover hover:text-fg disabled:opacity-50"
          >
            Reset to initials
          </button>
          <div className="flex-1" />
          <button
            type="button"
            onClick={close}
            className="h-8 rounded-lg border border-line-strong px-3 text-ui hover:bg-hover"
          >
            Cancel
          </button>
          <button
            type="submit"
            className="h-8 rounded-lg bg-solid px-3 text-ui font-medium text-white hover:bg-solid-hover disabled:opacity-50"
          >
            Save
          </button>
        </div>
      </form>
    </dialog>
  );
}

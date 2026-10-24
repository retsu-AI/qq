import { useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ArrowLeft, ArrowRight, Copy, Minus, PanelLeft, Square, X } from "lucide-react";
import { Logo } from "./Logo";
import { platform } from "../lib/platform";

export type MenuItem =
  | {
      label: string;
      shortcut?: string;
      onSelect: () => void;
      disabled?: boolean;
      checked?: boolean;
      desktopOnly?: boolean;
    }
  | "separator";

export interface Menu {
  label: string;
  /** Alt plus this key opens the menu. */
  mnemonic: string;
  items: MenuItem[];
}

const appWindow = getCurrentWindow();

/**
 * The window's own title bar (decorations are off): navigation, the menu bar,
 * a drag region and caption buttons, drawn in the app theme.
 */
export function TitleBar({
  menus,
  sidebarOpen,
  toggleSidebar,
  back,
  forward,
}: {
  menus: Menu[];
  sidebarOpen: boolean;
  toggleSidebar: () => void;
  back: (() => void) | null;
  forward: (() => void) | null;
}) {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (platform.mobile) return;
    void appWindow.isMaximized().then(setMaximized);
    const stop = appWindow.onResized(() => {
      void appWindow.isMaximized().then(setMaximized);
    });
    return () => {
      void stop.then((unlisten) => unlisten());
    };
  }, []);

  if (platform.mobile) return (
    <header className="mobile-titlebar flex shrink-0 items-center border-b border-line bg-sidebar">
      <BarButton label={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
        onClick={toggleSidebar} pressed={sidebarOpen}>
        <PanelLeft className="h-5 w-5" />
      </BarButton>
      <Logo className="mx-2 h-5 w-5" />
      <div className="flex-1" />
      <MenuBar menus={menus.map((menu) => ({
        ...menu, items: menu.items.filter((item) => item === "separator" || !item.desktopOnly),
      }))} />
    </header>
  );

  return (
    <header
      data-tauri-drag-region
      className="flex h-9 shrink-0 select-none items-center border-b border-line bg-sidebar text-[12.5px]"
    >
      <div data-tauri-drag-region className="flex h-full items-center gap-0.5 pl-2.5">
        <Logo className="mr-1.5 h-[18px] w-[18px]" />
        <BarButton label="Back (Alt ←)" onClick={back ?? undefined} disabled={!back}>
          <ArrowLeft className="h-4 w-4" />
        </BarButton>
        <BarButton label="Forward (Alt →)" onClick={forward ?? undefined} disabled={!forward}>
          <ArrowRight className="h-4 w-4" />
        </BarButton>
        <BarButton
          label={`${sidebarOpen ? "Hide" : "Show"} sidebar (Ctrl B)`}
          onClick={toggleSidebar}
          pressed={sidebarOpen}
        >
          <PanelLeft className="h-4 w-4" />
        </BarButton>
        <MenuBar menus={menus} />
      </div>
      <div data-tauri-drag-region className="h-full flex-1" />
      <div className="flex h-full">
        <Caption label="Minimize" onClick={() => void appWindow.minimize()}>
          <Minus className="h-4 w-4" />
        </Caption>
        <Caption
          label={maximized ? "Restore" : "Maximize"}
          onClick={() => void appWindow.toggleMaximize()}
        >
          {maximized ? (
            <Copy className="h-3.5 w-3.5 -scale-x-100" />
          ) : (
            <Square className="h-3.5 w-3.5" />
          )}
        </Caption>
        <Caption label="Close" danger onClick={() => void appWindow.close()}>
          <X className="h-4 w-4" />
        </Caption>
      </div>
    </header>
  );
}

function BarButton({
  label,
  onClick,
  disabled,
  pressed,
  children,
}: {
  label: string;
  onClick?: () => void;
  disabled?: boolean;
  pressed?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={pressed}
      disabled={disabled}
      onClick={onClick}
      className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg disabled:pointer-events-none disabled:opacity-35"
    >
      {children}
    </button>
  );
}

function Caption({
  label,
  onClick,
  danger,
  children,
}: {
  label: string;
  onClick: () => void;
  danger?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      className={clsx(
        "flex h-full w-[46px] items-center justify-center text-muted",
        danger ? "hover:bg-[#c42b1c] hover:text-white" : "hover:bg-hover hover:text-fg",
      )}
    >
      {children}
    </button>
  );
}

function MenuBar({ menus }: { menus: Menu[] }) {
  const [open, setOpen] = useState<number | null>(null);
  const [active, setActive] = useState(-1);
  const bar = useRef<HTMLDivElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  // The field that had focus before the menu opened: Edit commands act on it.
  const previous = useRef<HTMLElement | null>(null);
  const show = (index: number) => {
    if (open === null)
      previous.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setOpen(index);
    setActive(-1);
  };
  const close = (restore: boolean) => {
    setOpen(null);
    if (restore) previous.current?.focus();
  };
  const items = open === null ? [] : menus[open].items;
  const enabled = (index: number) => {
    const item = items[index];
    return item !== undefined && item !== "separator" && !item.disabled;
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.altKey || event.ctrlKey || event.shiftKey || event.metaKey) return;
      if (document.querySelector("dialog[open]")) return;
      const index = menus.findIndex(
        (menu) => menu.mnemonic.toLowerCase() === event.key.toLowerCase(),
      );
      if (index < 0) return;
      event.preventDefault();
      show(index);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  useEffect(() => {
    if (open === null) return;
    panel.current?.focus();
    const onDown = (event: MouseEvent) => {
      if (!bar.current?.contains(event.target as Node)) setOpen(null);
    };
    const onBlur = () => setOpen(null);
    document.addEventListener("mousedown", onDown);
    window.addEventListener("blur", onBlur);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("blur", onBlur);
    };
  }, [open]);

  const run = (index: number) => {
    const item = items[index];
    if (!item || item === "separator" || item.disabled) return;
    close(true);
    item.onSelect();
  };
  const move = (delta: number) => {
    if (items.length === 0) return;
    let index = active;
    for (let step = 0; step < items.length; step++) {
      index = (index + delta + items.length) % items.length;
      if (enabled(index)) break;
    }
    setActive(index);
  };

  return (
    <div ref={bar} role="menubar" className="ml-1 flex h-full items-center">
      {menus.map((menu, index) => (
        <div key={menu.label} className="relative">
          <button
            type="button"
            role="menuitem"
            aria-haspopup="menu"
            aria-expanded={open === index}
            // Keep focus in the field so Edit commands still apply to it.
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => (open === index ? close(true) : show(index))}
            onMouseEnter={() => {
              if (open !== null && open !== index) show(index);
            }}
            className={clsx(
              "rounded-md px-2.5 py-1 text-muted hover:bg-hover hover:text-fg",
              open === index && "bg-hover text-fg",
            )}
          >
            {menu.label}
          </button>
          {open === index && (
            <div
              ref={panel}
              role="menu"
              tabIndex={-1}
              aria-label={menu.label}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown") move(1);
                else if (event.key === "ArrowUp") move(-1);
                else if (event.key === "ArrowRight") show((index + 1) % menus.length);
                else if (event.key === "ArrowLeft")
                  show((index - 1 + menus.length) % menus.length);
                else if (event.key === "Enter" || event.key === " ") run(active);
                else if (event.key === "Escape") close(true);
                else if (event.key === "Tab") close(false);
                else return;
                event.preventDefault();
                event.stopPropagation();
              }}
              className="animate-pop-down absolute left-0 top-full z-50 mt-1 min-w-[248px] rounded-xl border border-line-strong bg-raised p-1 text-[13px] text-fg shadow-popover outline-none"
            >
              {menu.items.map((item, itemIndex) =>
                item === "separator" ? (
                  <div key={`sep-${itemIndex}`} role="separator" className="my-1 border-t border-line" />
                ) : (
                  <div
                    key={item.label}
                    role={item.checked === undefined ? "menuitem" : "menuitemradio"}
                    aria-checked={item.checked}
                    aria-disabled={item.disabled}
                    onMouseEnter={() => setActive(itemIndex)}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => run(itemIndex)}
                    className={clsx(
                      "flex cursor-default items-center gap-3 rounded-lg px-2.5 py-1.5",
                      item.disabled ? "text-faint" : itemIndex === active && "bg-hover",
                    )}
                  >
                    <span className="w-3 text-accent">{item.checked ? "•" : ""}</span>
                    <span className="flex-1">{item.label}</span>
                    {item.shortcut && (
                      <span className="font-mono text-[11px] text-faint">{item.shortcut}</span>
                    )}
                  </div>
                ),
              )}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

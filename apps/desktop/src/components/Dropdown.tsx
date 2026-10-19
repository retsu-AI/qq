import {
  useEffect,
  useId,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import clsx from "clsx";
import { Check } from "lucide-react";

export interface DropdownOption<T extends string> {
  value: T;
  label: string;
  /** One short line under the label. */
  hint?: string;
  /** Consecutive options with the same group share a heading. */
  group?: string;
  /** Overrides `value` matching, for menus with one choice per group. */
  selected?: boolean;
}

export interface DropdownAction {
  label: string;
  onSelect: () => void;
  icon?: ReactNode;
  /** Destructive: drawn in the danger color. */
  danger?: boolean;
}

/** Popup height used to decide whether it fits below or above the trigger. */
const MAX_POPUP_HEIGHT = 340;

/**
 * A themed replacement for `<select>`: native select popups are drawn by
 * Windows and ignore the app theme. Keyboard: arrows, Home/End, Enter or
 * Space to choose, Escape to close, typing to jump by first letter.
 *
 * With no options it is a command menu (`role="menu"`). The popup is fixed to
 * the viewport so a scrolling ancestor cannot clip it; it flips to the other
 * side when there is no room, and closes when an ancestor scrolls.
 */
export function Dropdown<T extends string>({
  value,
  options,
  onChange,
  label,
  children,
  actions = [],
  side = "top",
  align = "start",
  disabled,
  className,
  rootClassName,
  bare,
  title,
}: {
  value: T | null;
  options: DropdownOption<T>[];
  onChange: (value: T) => void;
  /** Accessible name of the control. */
  label: string;
  /** Trigger content. */
  children: ReactNode;
  /** Entries after the options that run a command instead of selecting. */
  actions?: DropdownAction[];
  side?: "top" | "bottom";
  align?: "start" | "end";
  disabled?: boolean;
  className?: string;
  rootClassName?: string;
  /** Trigger takes only `className`, without the default chip styling. */
  bare?: boolean;
  title?: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [position, setPosition] = useState<CSSProperties>({});
  const root = useRef<HTMLDivElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const id = useId();
  const count = options.length + actions.length;
  const menu = options.length === 0;
  const selectedIndex = Math.max(
    0,
    options.findIndex((option) => option.value === value),
  );

  useEffect(() => {
    if (!open) return;
    list.current?.focus();
    const onDown = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    // Only a scrolling ancestor moves the trigger; a streaming transcript
    // scrolling beside the composer must not close its menus.
    const onScroll = (event: Event) => {
      const target = event.target as Node;
      if (trigger.current && target.contains?.(trigger.current))
        setOpen(false);
    };
    const onResize = () => setOpen(false);
    document.addEventListener("mousedown", onDown);
    document.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onResize);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onResize);
    };
  }, [open]);

  useEffect(() => {
    if (open)
      list.current
        ?.querySelector(`[data-index="${active}"]`)
        ?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const show = () => {
    if (disabled || count === 0) return;
    const rect = trigger.current?.getBoundingClientRect();
    if (rect) {
      const below = window.innerHeight - rect.bottom;
      const above = rect.top;
      const top =
        side === "top"
          ? above >= MAX_POPUP_HEIGHT || above >= below
          : below < MAX_POPUP_HEIGHT && above > below;
      setPosition({
        ...(top
          ? { bottom: window.innerHeight - rect.top + 6 }
          : { top: rect.bottom + 6 }),
        ...(align === "end"
          ? { right: window.innerWidth - rect.right }
          : { left: rect.left }),
        maxHeight: Math.max(120, Math.min(MAX_POPUP_HEIGHT, (top ? above : below) - 12)),
      });
    }
    setActive(menu ? 0 : selectedIndex);
    setOpen(true);
  };
  const close = (refocus = true) => {
    setOpen(false);
    if (refocus) trigger.current?.focus();
  };
  const choose = (index: number) => {
    close();
    if (index < options.length) {
      const option = options[index];
      if (!(option.selected ?? option.value === value)) onChange(option.value);
    } else {
      actions[index - options.length]?.onSelect();
    }
  };

  return (
    <div ref={root} className={clsx("relative", rootClassName)}>
      <button
        ref={trigger}
        type="button"
        title={title}
        disabled={disabled}
        aria-label={label}
        aria-haspopup={menu ? "menu" : "listbox"}
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        onClick={() => (open ? close() : show())}
        onKeyDown={(event) => {
          if (["ArrowDown", "ArrowUp", "Enter", " "].includes(event.key)) {
            event.preventDefault();
            show();
          }
        }}
        className={clsx(
          !bare &&
            "flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:bg-hover hover:text-fg disabled:opacity-50",
          !bare && open && "bg-hover text-fg",
          className,
        )}
      >
        {children}
      </button>
      {open && (
        <div
          ref={list}
          id={id}
          role={menu ? "menu" : "listbox"}
          tabIndex={-1}
          aria-label={label}
          aria-activedescendant={`${id}-${active}`}
          // Closes when focus moves elsewhere: another menu, the composer, or
          // a different window.
          onBlur={(event) => {
            if (!root.current?.contains(event.relatedTarget as Node | null))
              setOpen(false);
          }}
          onKeyDown={(event) => {
            // App shortcuts (Alt+V, Ctrl+K, …) pass through; opening another
            // menu moves focus away, which closes this one.
            if (event.altKey || event.ctrlKey || event.metaKey) return;
            const step = (delta: number) =>
              setActive((index) => (index + delta + count) % count);
            if (event.key === "ArrowDown") step(1);
            else if (event.key === "ArrowUp") step(-1);
            else if (event.key === "Home") setActive(0);
            else if (event.key === "End") setActive(count - 1);
            else if (event.key === "Enter" || event.key === " ") choose(active);
            else if (event.key === "Tab") close(false);
            else if (event.key === "Escape") close();
            else if (event.key.length === 1 && /\S/.test(event.key)) {
              const letter = event.key.toLowerCase();
              const labels = [
                ...options.map((option) => option.label),
                ...actions.map((action) => action.label),
              ];
              const next = labels.findIndex(
                (text, index) =>
                  index > active && text.toLowerCase().startsWith(letter),
              );
              const wrap = labels.findIndex((text) =>
                text.toLowerCase().startsWith(letter),
              );
              if (next >= 0 || wrap >= 0) setActive(next >= 0 ? next : wrap);
            } else return;
            // Keys handled here must not reach the composer (Escape stops a run).
            event.preventDefault();
            event.stopPropagation();
          }}
          style={position}
          className={clsx(
            "fixed z-50 overflow-y-auto rounded-xl border border-line-strong bg-raised p-1 text-[13px] text-fg shadow-[0_12px_32px_rgba(0,0,0,0.35)] outline-none",
            menu ? "min-w-[180px]" : "min-w-[220px]",
          )}
        >
          {options.map((option, index) => (
            <div key={option.value}>
              {option.group &&
                option.group !== options[index - 1]?.group && (
                  <div className="px-2.5 pb-1 pt-2 text-[10.5px] font-semibold uppercase tracking-wider text-faint first:pt-1">
                    {option.group}
                  </div>
                )}
              <div
                id={`${id}-${index}`}
                data-index={index}
                role="option"
                aria-selected={option.selected ?? option.value === value}
                onMouseEnter={() => setActive(index)}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => choose(index)}
                className={clsx(
                  "flex cursor-default items-start gap-2 rounded-lg px-2.5 py-1.5",
                  index === active && "bg-hover",
                )}
              >
                <Check
                  className={clsx(
                    "mt-[3px] h-3.5 w-3.5 shrink-0 text-accent",
                    !(option.selected ?? option.value === value) && "invisible",
                  )}
                />
                <span className="min-w-0">
                  <span className="block truncate">{option.label}</span>
                  {option.hint && (
                    <span className="block text-[11.5px] leading-snug text-muted">
                      {option.hint}
                    </span>
                  )}
                </span>
              </div>
            </div>
          ))}
          {actions.length > 0 && options.length > 0 && (
            <div role="separator" className="my-1 border-t border-line" />
          )}
          {actions.map((action, offset) => {
            const index = options.length + offset;
            return (
              <div
                key={action.label}
                id={`${id}-${index}`}
                data-index={index}
                role={menu ? "menuitem" : "option"}
                aria-selected={menu ? undefined : false}
                onMouseEnter={() => setActive(index)}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => choose(index)}
                className={clsx(
                  "flex cursor-default items-center gap-2 rounded-lg px-2.5 py-1.5",
                  !menu && !action.icon && "pl-8",
                  action.danger
                    ? "text-danger"
                    : clsx("text-muted", index === active && "text-fg"),
                  index === active &&
                    (action.danger ? "bg-danger-soft" : "bg-hover"),
                )}
              >
                {action.icon && (
                  <span className="flex h-3.5 w-3.5 shrink-0 items-center justify-center">
                    {action.icon}
                  </span>
                )}
                {action.label}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

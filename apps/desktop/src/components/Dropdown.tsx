import { useEffect, useId, useRef, useState, type ReactNode } from "react";
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
}

/**
 * A themed replacement for `<select>`: native select popups are drawn by
 * Windows and ignore the app theme. Keyboard: arrows, Home/End, Enter or
 * Space to choose, Escape to close, typing to jump by first letter.
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
  title?: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const id = useId();
  const count = options.length + actions.length;
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
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  useEffect(() => {
    if (open)
      list.current
        ?.querySelector(`[data-index="${active}"]`)
        ?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const show = () => {
    if (disabled || count === 0) return;
    setActive(selectedIndex);
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
    <div ref={root} className="relative">
      <button
        ref={trigger}
        type="button"
        title={title}
        disabled={disabled}
        aria-label={label}
        aria-haspopup="listbox"
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
          "flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:bg-hover hover:text-fg disabled:opacity-50",
          open && "bg-hover text-fg",
          className,
        )}
      >
        {children}
      </button>
      {open && (
        <div
          ref={list}
          id={id}
          role="listbox"
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
          className={clsx(
            "absolute z-50 max-h-[340px] min-w-[220px] overflow-y-auto rounded-xl border border-line-strong bg-raised p-1 text-[13px] text-fg shadow-[0_12px_32px_rgba(0,0,0,0.35)] outline-none",
            side === "top" ? "bottom-full mb-1.5" : "top-full mt-1.5",
            align === "end" ? "right-0" : "left-0",
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
                role="option"
                aria-selected={false}
                onMouseEnter={() => setActive(index)}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => choose(index)}
                className={clsx(
                  "cursor-default rounded-lg px-2.5 py-1.5 pl-8 text-muted",
                  index === active && "bg-hover text-fg",
                )}
              >
                {action.label}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

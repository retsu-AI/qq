import { useEffect, useRef, useState, type ReactNode } from "react";
import clsx from "clsx";
import { Minus, Monitor, Moon, Plus, Sun, X } from "lucide-react";
import { api } from "../lib/api";
import {
  MAX_FONT_SIZE,
  MIN_FONT_SIZE,
  updatePreferences,
  usePreferences,
  type Preferences,
} from "../lib/preferences";
import type { ProjectState } from "../lib/types";

const SHORTCUTS: Array<[string, string]> = [
  ["New session", "Ctrl N"],
  ["New worktree session", "Ctrl Shift N"],
  ["Open project", "Ctrl O"],
  ["Command palette", "Ctrl K"],
  ["Settings", "Ctrl ,"],
  ["Toggle sidebar", "Ctrl B"],
  ["Find a session", "Ctrl F"],
  ["Show or hide changes", "Ctrl Shift D"],
  ["Open project in editor", "Ctrl Shift E"],
  ["Ask a side question", "/btw in the composer"],
  ["Run a workspace command", "/ in the composer"],
  ["Back / forward", "Alt ← / Alt →"],
  ["Zoom in / out / reset", "Ctrl + / Ctrl - / Ctrl 0"],
  ["Next session that needs you", "Ctrl Shift A"],
  ["Mention a file", "@"],
  ["Send / new line", "Enter / Shift Enter"],
  ["Stop the run", "Esc in the composer"],
  ["Approve once / session / workspace / deny", "Y / S / W / N"],
  ["Open a menu", "Alt F / E / V / H"],
];

const THEMES: Array<{
  value: Preferences["theme"];
  label: string;
  icon: ReactNode;
}> = [
  { value: "system", label: "System", icon: <Monitor className="h-3.5 w-3.5" /> },
  { value: "light", label: "Light", icon: <Sun className="h-3.5 w-3.5" /> },
  { value: "dark", label: "Dark", icon: <Moon className="h-3.5 w-3.5" /> },
];

export function Settings({
  project,
  close,
}: {
  project: ProjectState | null;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const preferences = usePreferences();
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  const setNotifications = async (notifications: boolean) => {
    try {
      await api.setNotificationsEnabled(notifications);
      updatePreferences({ notifications });
      setError(null);
    } catch (error) {
      setError(String(error));
    }
  };
  const setFontSize = (fontSize: number) =>
    updatePreferences({
      fontSize: Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, fontSize)),
    });

  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="settings-title"
      className="m-auto max-h-[85vh] w-[620px] max-w-[92vw] overflow-hidden rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <div className="flex max-h-[85vh] flex-col">
        <header className="flex items-center gap-3 border-b border-line px-6 py-4">
          <h2 id="settings-title" className="flex-1 text-[16px] font-semibold">
            Settings
          </h2>
          <button
            autoFocus
            onClick={close}
            aria-label="Close settings"
            title="Close (Esc)"
            className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
          >
            <X className="h-4 w-4" />
          </button>
        </header>

        <div className="space-y-6 overflow-y-auto px-6 py-5">
          {error && (
            <p
              role="alert"
              className="rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
            >
              {error}
            </p>
          )}

          <Section title="Appearance">
            <Row label="Theme">
              <div
                role="radiogroup"
                aria-label="Theme"
                className="flex rounded-lg border border-line-strong bg-raised p-0.5"
              >
                {THEMES.map((theme) => (
                  <button
                    key={theme.value}
                    role="radio"
                    aria-checked={preferences.theme === theme.value}
                    onClick={() => updatePreferences({ theme: theme.value })}
                    className={clsx(
                      "flex h-7 items-center gap-1.5 rounded-md px-3 text-[12.5px]",
                      preferences.theme === theme.value
                        ? "bg-active text-fg shadow-sm"
                        : "text-muted hover:text-fg",
                    )}
                  >
                    {theme.icon}
                    {theme.label}
                  </button>
                ))}
              </div>
            </Row>
            <Row label="Text size" hint="Conversation and composer">
              <div className="flex items-center rounded-lg border border-line-strong bg-raised">
                <Stepper
                  label="Smaller text"
                  disabled={preferences.fontSize <= MIN_FONT_SIZE}
                  onClick={() => setFontSize(preferences.fontSize - 1)}
                >
                  <Minus className="h-3.5 w-3.5" />
                </Stepper>
                <span
                  aria-live="polite"
                  className="w-12 text-center font-mono text-[12.5px]"
                >
                  {preferences.fontSize}px
                </span>
                <Stepper
                  label="Larger text"
                  disabled={preferences.fontSize >= MAX_FONT_SIZE}
                  onClick={() => setFontSize(preferences.fontSize + 1)}
                >
                  <Plus className="h-3.5 w-3.5" />
                </Stepper>
              </div>
            </Row>
          </Section>

          <Section title="Notifications">
            <Row
              label="Notify me in Windows"
              hint="When QQ is in the background and a run finishes or needs approval. Needs the installed app; the text includes the session title."
            >
              <Switch
                label="Windows notifications"
                checked={preferences.notifications}
                onChange={(checked) => void setNotifications(checked)}
              />
            </Row>
          </Section>

          <Section title="Keyboard shortcuts">
            <dl className="divide-y divide-line overflow-hidden rounded-xl border border-line">
              {SHORTCUTS.map(([action, keys]) => (
                <div
                  key={action}
                  className="flex items-center justify-between gap-4 px-3.5 py-2 text-[13px]"
                >
                  <dt className="text-muted">{action}</dt>
                  <dd className="flex flex-wrap justify-end gap-1">
                    {keys.split(" / ").map((combo) => (
                      <kbd
                        key={combo}
                        className="rounded-md border border-line-strong bg-raised px-1.5 py-0.5 font-mono text-[11px] text-fg"
                      >
                        {combo}
                      </kbd>
                    ))}
                  </dd>
                </div>
              ))}
            </dl>
          </Section>

          <Section title="Diagnostics">
            <div className="space-y-1.5 rounded-xl border border-line px-3.5 py-3 text-[12.5px]">
              <Fact label="Project">{project?.path ?? "No project open"}</Fact>
              <Fact label="Connection">{project?.connection ?? "none"}</Fact>
              <Fact label="Engine">
                {project?.owns_server
                  ? "Started by this app"
                  : "Shared, or not running"}
              </Fact>
              {project?.error && (
                <p role="alert" className="pt-1 text-danger selectable">
                  {project.error}
                </p>
              )}
            </div>
            <h4 className="mb-2 mt-4 text-[12.5px] font-medium text-muted">
              MCP servers and external tools
            </h4>
            {project?.tool_hosts?.length ? (
              <ul className="space-y-1.5">
                {project.tool_hosts.map((host) => (
                  <li
                    key={host.name}
                    className="rounded-xl border border-line px-3.5 py-2.5 text-[12.5px]"
                  >
                    <div className="flex items-center gap-2">
                      <span
                        className={clsx(
                          "h-2 w-2 rounded-full",
                          host.ready ? "bg-ok" : "bg-danger",
                        )}
                      />
                      <span className="font-medium">{host.name}</span>
                      <span className="ml-auto text-muted">
                        {host.ready ? `${host.tool_count} tools` : "Unavailable"}
                      </span>
                    </div>
                    {host.message && (
                      <p className="mt-1 text-muted selectable">{host.message}</p>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-[12.5px] text-muted">
                None reported. Status appears once the project's capabilities
                load.
              </p>
            )}
            <p className="mt-3 text-[12px] text-faint">
              MCP servers and runtime defaults are configured in your QQ
              config.ron; trust a changed project configuration before it is
              used. Settings on this page apply to this desktop only.
            </p>
          </Section>
        </div>
      </div>
    </dialog>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <h3 className="mb-2.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
        {title}
      </h3>
      <div className="space-y-3">{children}</div>
    </section>
  );
}

function Row({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-6">
      <div className="min-w-0">
        <div className="text-[13.5px]">{label}</div>
        {hint && <div className="text-[12px] text-muted">{hint}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function Stepper({
  label,
  disabled,
  onClick,
  children,
}: {
  label: string;
  disabled: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className="flex h-8 w-8 items-center justify-center text-muted hover:text-fg disabled:opacity-35"
    >
      {children}
    </button>
  );
}

function Switch({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      role="switch"
      aria-label={label}
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className={clsx(
        "relative h-5 w-9 rounded-full transition-colors",
        checked ? "bg-solid" : "bg-line-strong",
      )}
    >
      <span
        className={clsx(
          "absolute left-0 top-0.5 h-4 w-4 rounded-full bg-white shadow transition-transform",
          checked ? "translate-x-[18px]" : "translate-x-0.5",
        )}
      />
    </button>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex gap-3">
      <span className="w-24 shrink-0 text-muted">{label}</span>
      <span className="min-w-0 break-all selectable">{children}</span>
    </div>
  );
}

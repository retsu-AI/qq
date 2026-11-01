import { useEffect, useRef, useState, type ReactNode } from "react";
import clsx from "clsx";
import {
  Activity,
  ChevronDown,
  Keyboard,
  KeyRound,
  Minus,
  Monitor,
  Moon,
  Plus,
  SlidersHorizontal,
  Sun,
  X,
} from "lucide-react";
import { api } from "../lib/api";
import {
  CODE_FONTS,
  INTERFACE_FONTS,
  MAX_FONT_SIZE,
  MIN_FONT_SIZE,
  fontName,
  fontStack,
  updatePreferences,
  usePreferences,
  type Preferences,
} from "../lib/preferences";
import { showSettings, type SettingsPage } from "../lib/store";
import type { ProjectState } from "../lib/types";
import { Dropdown, type DropdownOption } from "./Dropdown";
import { Providers } from "./Providers";

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

const PAGES: Array<{ value: SettingsPage; label: string; icon: ReactNode }> = [
  { value: "general", label: "General", icon: <SlidersHorizontal className="h-4 w-4" /> },
  { value: "providers", label: "Providers", icon: <KeyRound className="h-4 w-4" /> },
  { value: "shortcuts", label: "Shortcuts", icon: <Keyboard className="h-4 w-4" /> },
  { value: "diagnostics", label: "Diagnostics", icon: <Activity className="h-4 w-4" /> },
];

export function Settings({
  page,
  project,
}: {
  page: SettingsPage;
  project: ProjectState | null;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const closeButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
    // showModal focuses the first page link; start on Close instead.
    closeButton.current?.focus();
  }, []);
  const close = () => showSettings(null);
  const title = PAGES.find((entry) => entry.value === page)?.label ?? "Settings";

  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="settings-title"
      className="m-auto h-[min(640px,85vh)] w-[780px] max-w-[92vw] overflow-hidden rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) close();
      }}
    >
      <div className="flex h-full">
        <nav
          aria-label="Settings pages"
          className="flex w-48 shrink-0 flex-col gap-0.5 border-r border-line bg-sidebar p-2"
        >
          <div className="px-2.5 pb-2 pt-2 text-title font-semibold">Settings</div>
          {PAGES.map((entry) => (
            <button
              key={entry.value}
              aria-current={entry.value === page ? "page" : undefined}
              onClick={() => showSettings(entry.value)}
              className={clsx(
                "flex h-8 items-center gap-2.5 rounded-md px-2.5 text-left text-ui",
                entry.value === page
                  ? "bg-active text-fg"
                  : "text-muted hover:bg-hover hover:text-fg",
              )}
            >
              {entry.icon}
              {entry.label}
            </button>
          ))}
        </nav>
        <div className="flex min-w-0 flex-1 flex-col">
          <header className="flex items-center gap-3 px-6 pb-2 pt-4">
            <h2 id="settings-title" className="flex-1 text-title font-semibold">
              {title}
            </h2>
            <button
              ref={closeButton}
              onClick={close}
              aria-label="Close settings"
              title="Close (Esc)"
              className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
            >
              <X className="h-4 w-4" />
            </button>
          </header>
          <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-6 pt-2">
            {page === "general" && <General />}
            {page === "providers" && <Providers />}
            {page === "shortcuts" && <Shortcuts />}
            {page === "diagnostics" && <Diagnostics project={project} />}
          </div>
        </div>
      </div>
    </dialog>
  );
}

function General() {
  const preferences = usePreferences();
  const [error, setError] = useState<string | null>(null);
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
    <div className="space-y-7">
      {error && (
        <p
          role="alert"
          className="rounded-md bg-danger-soft px-3 py-2 text-ui text-danger"
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
                  "flex h-7 items-center gap-1.5 rounded-md px-3 text-ui",
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
            <span aria-live="polite" className="w-12 text-center font-mono text-ui">
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
        <FontRow
          label="Interface font"
          hint="Menus, sidebar and conversation"
          fonts={INTERFACE_FONTS}
          value={preferences.interfaceFont}
          code={false}
          onChange={(interfaceFont) => updatePreferences({ interfaceFont })}
        />
        <FontRow
          label="Code font"
          hint="Code blocks, diffs and tool output"
          fonts={CODE_FONTS}
          value={preferences.codeFont}
          code
          onChange={(codeFont) => updatePreferences({ codeFont })}
        />
        <div className="rounded-xl border border-line bg-raised/60 px-4 py-3">
          <p className="text-body">The agent reads the diff before it edits.</p>
          <p className="mt-1 font-mono text-ui text-muted">
            {'fn main() { println!("0O 1lI {}", 42); }'}
          </p>
        </div>
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
      <p className="text-small text-faint">
        Settings on this page apply to this desktop only.
      </p>
    </div>
  );
}

/** Option value for typing a font name; not a valid font name itself. */
const CUSTOM = "(custom)";

/** A font choice: Windows fonts, or any installed family by name. */
function FontRow({
  label,
  hint,
  fonts,
  value,
  code,
  onChange,
}: {
  label: string;
  hint: string;
  fonts: string[];
  value: string | null;
  code: boolean;
  onChange: (font: string | null) => void;
}) {
  const current = value ?? fonts[0];
  const custom = !fonts.includes(current);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const invalid = draft.trim() !== "" && fontName(draft) === null;
  const save = () => {
    if (invalid) return;
    const name = fontName(draft);
    onChange(name && name !== fonts[0] ? name : null);
    setEditing(false);
  };
  const options: Array<DropdownOption<string>> = [
    ...fonts.map((font, index) => ({
      value: font,
      label: index === 0 ? `${font} (default)` : font,
      fontFamily: fontStack(font, code),
    })),
    {
      value: CUSTOM,
      label: custom ? `Custom: ${current}` : "Custom…",
      hint: "Any font installed on this computer",
      fontFamily: custom ? fontStack(current, code) : undefined,
    },
  ];
  return (
    <div>
      <Row label={label} hint={hint}>
        <Dropdown
          label={label}
          value={custom ? CUSTOM : current}
          options={options}
          side="bottom"
          align="end"
          onChange={(choice) => {
            if (choice === CUSTOM) {
              setDraft(custom ? current : "");
              setEditing(true);
            } else {
              setEditing(false);
              onChange(choice === fonts[0] ? null : choice);
            }
          }}
          className="h-8 w-56 justify-between border border-line-strong bg-raised text-fg"
        >
          <span
            className="truncate"
            style={{ fontFamily: fontStack(current, code) }}
          >
            {current}
          </span>
          <ChevronDown className="h-3.5 w-3.5 shrink-0 text-muted" />
        </Dropdown>
      </Row>
      {editing && (
        <form
          className="mt-2 flex items-center justify-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            save();
          }}
        >
          <input
            autoFocus
            aria-label={`${label} name`}
            aria-invalid={invalid}
            placeholder={code ? "e.g. JetBrains Mono" : "e.g. Inter"}
            value={draft}
            maxLength={64}
            spellCheck={false}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                // Close the field, not the whole Settings dialog.
                event.preventDefault();
                event.stopPropagation();
                setEditing(false);
              }
            }}
            style={{ fontFamily: fontStack(fontName(draft), code) }}
            className={clsx(
              "h-8 w-56 rounded-lg border bg-bg px-2.5 text-ui outline-none focus-visible:outline-none",
              invalid ? "border-danger" : "border-line-strong focus:border-accent",
            )}
          />
          <button
            type="submit"
            disabled={invalid}
            className="h-8 rounded-lg bg-solid px-3 text-ui font-medium text-white hover:bg-solid-hover disabled:opacity-50"
          >
            Use font
          </button>
        </form>
      )}
      {editing && (
        <p className="mt-1 text-right text-small text-faint">
          {invalid
            ? "Use the font's name: letters, numbers, spaces, dots and hyphens."
            : "Leave it empty for the default. If the font isn't installed, the default is used."}
        </p>
      )}
    </div>
  );
}

function Shortcuts() {
  return (
    <dl className="divide-y divide-line overflow-hidden rounded-xl border border-line">
      {SHORTCUTS.map(([action, keys]) => (
        <div
          key={action}
          className="flex items-center justify-between gap-4 px-3.5 py-2 text-ui"
        >
          <dt className="text-muted">{action}</dt>
          <dd className="flex flex-wrap justify-end gap-1">
            {keys.split(" / ").map((combo) => (
              <kbd
                key={combo}
                className="rounded-md border border-line-strong bg-raised px-1.5 py-0.5 font-mono text-caption text-fg"
              >
                {combo}
              </kbd>
            ))}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Diagnostics({ project }: { project: ProjectState | null }) {
  return (
    <div className="space-y-7">
      <Section title="Engine">
        <div className="space-y-1.5 rounded-xl border border-line px-3.5 py-3 text-ui">
          <Fact label="Project">{project?.path ?? "No project open"}</Fact>
          <Fact label="Connection">{project?.connection ?? "none"}</Fact>
          <Fact label="Engine">
            {project?.owns_server ? "Started by this app" : "Shared, or not running"}
          </Fact>
          {project?.error && (
            <p role="alert" className="pt-1 text-danger selectable">
              {project.error}
            </p>
          )}
        </div>
      </Section>
      <Section title="MCP servers and external tools">
        {project?.tool_hosts?.length ? (
          <ul className="space-y-1.5">
            {project.tool_hosts.map((host) => (
              <li
                key={host.name}
                className="rounded-xl border border-line px-3.5 py-2.5 text-ui"
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
          <p className="text-ui text-muted">
            None reported. Status appears once the project's capabilities load.
          </p>
        )}
        <p className="text-small text-faint">
          MCP servers and runtime defaults are configured in your QQ
          config.ron; trust a changed project configuration before it is used.
        </p>
      </Section>
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <h3 className="mb-2.5 text-caption font-semibold uppercase tracking-wider text-faint">
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
        <div className="text-body">{label}</div>
        {hint && <div className="text-small text-muted">{hint}</div>}
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

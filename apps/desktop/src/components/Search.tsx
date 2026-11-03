import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import clsx from "clsx";
import {
  CircleAlert,
  CornerDownLeft,
  FolderOpen,
  Loader2,
  Search as SearchIcon,
  X,
} from "lucide-react";
import { api } from "../lib/api";
import { openInEditor } from "../lib/editor";
import { relativeTime } from "../lib/format";
import { useProjectIcons } from "../lib/project-icons";
import {
  SEARCH_TABS,
  highlight,
  rank,
  type Range,
  type SearchTab,
} from "../lib/search";
import { readSessionPreferences } from "../lib/session-preferences";
import {
  activate,
  openProject,
  pickProject,
  showSearch,
  showSettings,
} from "../lib/store";
import type { ProjectInfo, ProjectState, SessionRow } from "../lib/types";
import { showWorktreeDialog } from "../lib/worktree-dialog";
import { ProjectBadge } from "./ProjectIcon";

/** Rows per section on the All tab; the other tabs list every match. */
const ALL_LIMIT = { sessions: 6, projects: 4, commands: 4 };
const RECENT_SESSIONS = 8;

interface Command {
  name: string;
  hint?: string;
  run: () => unknown;
}

type Row =
  | { kind: "session"; project: ProjectInfo; row: SessionRow; title: string; ranges: Range[] }
  | { kind: "project"; project: ProjectInfo; open: boolean; ranges: Range[] }
  | { kind: "command"; command: Command; ranges: Range[] };

export function Search({
  tab,
  app,
  project,
}: {
  tab: SearchTab;
  app: {
    open: ProjectInfo[];
    recents: ProjectInfo[];
    projects: Record<string, ProjectState>;
  };
  project: ProjectState | null;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const icons = useProjectIcons();
  useEffect(() => {
    dialog.current?.showModal();
    input.current?.focus();
  }, []);
  const close = () => showSearch(null);

  // Local session names, read once when the window opens.
  const names = useMemo(
    () =>
      Object.fromEntries(
        app.open.map((p) => [p.id, readSessionPreferences(p.id).aliases]),
      ),
    [app.open],
  );
  const sessions = useMemo(
    () =>
      app.open.flatMap((info) =>
        (app.projects[info.id]?.sessions ?? []).map((row) => ({
          project: info,
          row,
          title: names[info.id]?.[row.id] ?? row.title,
        })),
      ),
    [app.open, app.projects, names],
  );
  const projects = useMemo(
    () => [
      ...app.open.map((info) => ({ project: info, open: true })),
      ...app.recents
        .filter((recent) => !app.open.some((p) => p.path === recent.path))
        .map((info) => ({ project: info, open: false })),
    ],
    [app.open, app.recents],
  );
  const commands = useCommands(project);

  const searching = query.trim() !== "";
  const sections: Array<{ label: string; rows: Row[] }> = [];
  const limit = (key: keyof typeof ALL_LIMIT) =>
    tab === "all" ? ALL_LIMIT[key] : Infinity;
  if (tab === "all" || tab === "sessions") {
    const found = rank(
      sessions,
      query,
      (s) => ({
        label: s.title,
        detail: `${s.row.tail} ${s.project.name}`,
        recency: s.row.updated_at_ms,
      }),
      searching ? limit("sessions") : tab === "all" ? RECENT_SESSIONS : Infinity,
    );
    sections.push({
      label: searching ? "Sessions" : "Recent sessions",
      rows: found.map(({ item, ranges }) => ({ kind: "session", ...item, ranges })),
    });
  }
  if ((tab === "all" && searching) || tab === "projects") {
    const found = rank(
      projects,
      query,
      (p) => ({ label: p.project.worktree?.branch ?? p.project.name, detail: p.project.path }),
      limit("projects"),
    );
    sections.push({
      label: "Projects",
      rows: found.map(({ item, ranges }) => ({ kind: "project", ...item, ranges })),
    });
  }
  if (tab === "all" || tab === "commands") {
    const pool =
      tab === "all" && !searching
        ? commands.filter((c) => QUICK_ACTIONS.includes(c.name))
        : commands;
    const found = rank(pool, query, (c) => ({ label: c.name, detail: c.hint }), limit("commands"));
    sections.push({
      label: tab === "all" && !searching ? "Actions" : "Commands",
      rows: found.map(({ item, ranges }) => ({ kind: "command", command: item, ranges })),
    });
  }
  const shown = sections.filter((section) => section.rows.length > 0);
  const rows = shown.flatMap((section) => section.rows);
  const current = Math.min(active, Math.max(0, rows.length - 1));

  useEffect(() => setActive(0), [query, tab]);
  useEffect(() => {
    list.current
      ?.querySelector(`[data-index="${current}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [current]);

  const choose = async (row: Row | undefined) => {
    if (!row || busy) return;
    setError(null);
    try {
      if (row.kind === "session") {
        activate(row.project.id);
        close();
        await api.focusSession(row.project.id, row.row.id);
      } else if (row.kind === "project") {
        if (row.open) {
          activate(row.project.id);
          close();
        } else {
          setBusy(true);
          const opened = await openProject(row.project.path);
          setBusy(false);
          if (opened) close();
          else setError(`Couldn't open ${row.project.path}.`);
        }
      } else {
        close();
        await row.command.run();
      }
    } catch (failure) {
      setBusy(false);
      setError(String(failure));
    }
  };

  const switchTab = (delta: 1 | -1) => {
    const index = SEARCH_TABS.findIndex(([value]) => value === tab);
    const next = SEARCH_TABS[(index + delta + SEARCH_TABS.length) % SEARCH_TABS.length];
    showSearch(next[0]);
  };

  let index = -1;
  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-label="Search"
      className="m-auto mt-[12vh] flex max-h-[min(600px,76vh)] w-[680px] max-w-[92vw] flex-col overflow-hidden rounded-2xl border border-line-strong bg-raised p-0 text-fg shadow-2xl backdrop:bg-black/50 [&:not([open])]:hidden"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) close();
      }}
    >
      <div className="flex items-center gap-2.5 px-5 pt-4">
        <SearchIcon className="h-4 w-4 shrink-0 text-faint" />
        <input
          ref={input}
          role="combobox"
          aria-label="Search sessions, projects and commands"
          aria-expanded
          aria-controls="search-results"
          aria-activedescendant={rows.length ? `search-row-${current}` : undefined}
          placeholder={PLACEHOLDERS[tab]}
          value={query}
          spellCheck={false}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            const el = event.currentTarget;
            if (event.key === "ArrowDown" || event.key === "ArrowUp") {
              event.preventDefault();
              if (rows.length)
                setActive((current + (event.key === "ArrowDown" ? 1 : -1) + rows.length) % rows.length);
            } else if (event.key === "Enter") {
              event.preventDefault();
              void choose(rows[current]);
            } else if (
              // Arrows move the caret inside the text, and switch tabs at its ends.
              (event.key === "ArrowLeft" && el.selectionStart === 0 && el.selectionEnd === 0) ||
              (event.key === "ArrowRight" && el.selectionStart === el.value.length)
            ) {
              event.preventDefault();
              switchTab(event.key === "ArrowLeft" ? -1 : 1);
            }
          }}
          className="min-w-0 flex-1 bg-transparent text-body outline-none placeholder:text-faint focus-visible:outline-none"
        />
        {busy && <Loader2 className="h-4 w-4 animate-spin text-muted" />}
        <button
          onClick={close}
          aria-label="Close search"
          title="Close (Esc)"
          className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
        >
          <X className="h-4 w-4" />
        </button>
      </div>

      <div role="tablist" aria-label="Search in" className="flex gap-1 border-b border-line px-4 pb-3 pt-3">
        {SEARCH_TABS.map(([value, label]) => (
          <button
            key={value}
            role="tab"
            tabIndex={-1}
            aria-selected={tab === value}
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => showSearch(value)}
            className={clsx(
              "h-7 rounded-md px-2.5 text-ui",
              tab === value ? "bg-active font-medium text-fg" : "text-muted hover:bg-hover hover:text-fg",
            )}
          >
            {label}
          </button>
        ))}
      </div>

      {error && (
        <p role="alert" className="mx-4 mt-3 rounded-md bg-danger-soft px-3 py-2 text-ui text-danger selectable">
          {error}
        </p>
      )}

      <div ref={list} id="search-results" role="listbox" aria-label="Results" className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
        {shown.map((section) => (
          <div key={section.label} role="group" aria-label={section.label} className="pb-1">
            <div className="px-3 pb-1 pt-2 text-small text-faint">{section.label}</div>
            {section.rows.map((row) => {
              index += 1;
              const i = index;
              return (
                <div
                  key={rowKey(row)}
                  id={`search-row-${i}`}
                  data-index={i}
                  role="option"
                  aria-selected={i === current}
                  onMouseMove={() => i !== current && setActive(i)}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => void choose(row)}
                  className={clsx(
                    "flex h-9 cursor-default items-center gap-3 rounded-lg px-3 text-ui",
                    i === current ? "bg-active" : "",
                  )}
                >
                  <RowContent row={row} icon={icons[rowProject(row)?.path ?? ""]} />
                  {i === current && <CornerDownLeft className="h-3.5 w-3.5 shrink-0 text-faint" />}
                </div>
              );
            })}
          </div>
        ))}
        {rows.length === 0 && (
          <p className="px-3 py-8 text-center text-ui text-faint">
            {searching
              ? `Nothing matches “${query.trim()}”.`
              : tab === "sessions"
                ? "No sessions in the open projects yet."
                : "Nothing here yet."}
            {tab === "sessions" && (
              <span className="mt-1 block text-small">
                Closed projects aren't searched; reopen one from Projects.
              </span>
            )}
          </p>
        )}
      </div>

      <footer className="flex items-center gap-4 border-t border-line px-4 py-2.5 text-small text-muted">
        <Hint keys={["Esc"]}>Close</Hint>
        <Hint keys={["↑", "↓"]}>Move</Hint>
        <Hint keys={["←", "→"]}>Change tab</Hint>
        <span className="flex-1" />
        <Hint keys={["Enter"]}>Open</Hint>
      </footer>
    </dialog>
  );
}

const PLACEHOLDERS: Record<SearchTab, string> = {
  all: "Search sessions, projects and commands",
  sessions: "Search sessions in open projects",
  projects: "Search open and recent projects",
  commands: "Search commands",
};

/** Commands offered on the All tab before anything is typed. */
const QUICK_ACTIONS = ["New session", "Open project…", "Settings"];

function useCommands(project: ProjectState | null): Command[] {
  const session = project?.focused ?? null;
  const idle = project?.transcript?.status === "idle";
  const id = project?.project_id;
  const commands: Array<Command | false> = [
    !!id && { name: "New session", hint: "Ctrl N", run: () => api.newSession(id) },
    { name: "Open project…", hint: "Ctrl O", run: pickProject },
    !!id && { name: "New worktree session…", hint: "Ctrl Shift N", run: () => showWorktreeDialog(id) },
    !!id && { name: "Open project in editor", hint: "Ctrl Shift E", run: () => openInEditor(id) },
    !!id && !!session && { name: "New child session", run: () => api.sessionAction(id, session, "child") },
    !!id && !!session && idle && {
      name: "Compact session context",
      hint: "The transcript stays; model context shrinks",
      run: () => api.sessionAction(id, session, "compact"),
    },
    !!id && !!session && idle && {
      name: "Roll back latest compaction",
      run: () => api.sessionAction(id, session, "rollback"),
    },
    !!id && { name: "Prune empty idle sessions", run: () => api.sessionAction(id, null, "prune") },
    { name: "Settings", hint: "Ctrl ,", run: () => showSettings("general") },
    { name: "Model providers", hint: "Settings", run: () => showSettings("providers") },
    { name: "Keyboard shortcuts", hint: "Settings", run: () => showSettings("shortcuts") },
    { name: "Diagnostics", hint: "Settings", run: () => showSettings("diagnostics") },
  ];
  return commands.filter((command): command is Command => Boolean(command));
}

function rowKey(row: Row): string {
  return row.kind === "session"
    ? `s:${row.project.id}:${row.row.id}`
    : row.kind === "project"
      ? `p:${row.project.path}`
      : `c:${row.command.name}`;
}

function rowProject(row: Row): ProjectInfo | null {
  return row.kind === "command" ? null : row.project;
}

function Highlighted({ text, ranges }: { text: string; ranges: Range[] }) {
  return (
    <>
      {highlight(text, ranges).map((run, i) =>
        run.hit ? (
          <mark key={i} className="rounded-sm bg-accent-soft text-accent">
            {run.text}
          </mark>
        ) : (
          <span key={i}>{run.text}</span>
        ),
      )}
    </>
  );
}

function RowContent({
  row,
  icon,
}: {
  row: Row;
  icon: Parameters<typeof ProjectBadge>[0]["icon"];
}) {
  if (row.kind === "command")
    return (
      <>
        <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-faint">›</span>
        <span className="min-w-0 flex-1 truncate">
          <Highlighted text={row.command.name} ranges={row.ranges} />
        </span>
        {row.command.hint && (
          <span className="shrink-0 truncate text-small text-faint">{row.command.hint}</span>
        )}
      </>
    );
  if (row.kind === "project")
    return (
      <>
        <ProjectBadge project={row.project} icon={icon} size="sm" />
        <span className="min-w-0 flex-1 truncate">
          <Highlighted text={row.project.worktree?.branch ?? row.project.name} ranges={row.ranges} />
          <span className="ml-2 text-small text-faint">{row.project.path}</span>
        </span>
        {!row.open && (
          <span className="flex shrink-0 items-center gap-1 text-small text-faint">
            <FolderOpen className="h-3.5 w-3.5" /> Reopen
          </span>
        )}
      </>
    );
  const status = sessionStatus(row.row);
  return (
    <>
      <ProjectBadge project={row.project} icon={icon} size="sm" />
      <span className="min-w-0 flex-1 truncate">
        <Highlighted text={row.title} ranges={row.ranges} />
        <span className="ml-2 text-small text-faint">{row.project.name}</span>
      </span>
      <span className="shrink-0 text-small text-faint">{status}</span>
    </>
  );
}

function sessionStatus(row: SessionRow): ReactNode {
  if (row.awaiting_approval > 0 || row.group === "needs_you")
    return (
      <span className="flex items-center gap-1 text-warn">
        <CircleAlert className="h-3.5 w-3.5" /> Needs you
      </span>
    );
  if (row.status !== "idle")
    return (
      <span className="flex items-center gap-1 text-accent">
        <Loader2 className="h-3.5 w-3.5 animate-spin" /> Working
      </span>
    );
  return relativeTime(row.updated_at_ms);
}

function Hint({ keys, children }: { keys: string[]; children: ReactNode }) {
  return (
    <span className="flex items-center gap-1.5">
      {children}
      {keys.map((key) => (
        <kbd key={key} className="rounded border border-line-strong bg-bg px-1.5 py-px font-mono text-caption text-muted">
          {key}
        </kbd>
      ))}
    </span>
  );
}

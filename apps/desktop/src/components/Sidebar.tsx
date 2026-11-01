import { useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  CircleAlert,
  FolderPlus,
  GitBranchPlus,
  Loader2,
  MoreHorizontal,
  Palette,
  Pencil,
  Plus,
  Search,
  Settings as SettingsIcon,
  SquarePen,
  Trash2,
  X,
} from "lucide-react";
import { api } from "../lib/api";
import {
  activate,
  closeProject,
  pickProject,
  removeWorktree,
  showSearch,
  showSettings,
} from "../lib/store";
import { relativeTime } from "../lib/format";
import type {
  Connection,
  ProjectInfo,
  ProjectState,
  SessionRow,
} from "../lib/types";
import {
  parseSessionPreferences,
  readSessionPreferences,
  sessionPreferencesKey,
  updateSessionPreferences,
  type SessionPreferences,
} from "../lib/session-preferences";
import { useProjectIcons } from "../lib/project-icons";
import { ConfirmDialog } from "./ConfirmDialog";
import { Dropdown } from "./Dropdown";
import { IconDialog, ProjectBadge } from "./ProjectIcon";
import { toast } from "../lib/toast";
import { showWorktreeDialog } from "../lib/worktree-dialog";

interface Props {
  app: {
    open: ProjectInfo[];
    projects: Record<string, ProjectState>;
    activeId: string | null;
  };
  project: ProjectState | null;
}

const EXPANDED_KEY = "qq.sidebar.expanded";
/** Recent sessions a card lists before "Show all". */
const RECENT_LIMIT = 8;

function readExpanded(): Record<string, boolean> {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(EXPANDED_KEY) ?? "{}");
    if (typeof value !== "object" || value === null) return {};
    return Object.fromEntries(
      Object.entries(value).filter(([, open]) => typeof open === "boolean"),
    );
  } catch {
    return {};
  }
}

export function Sidebar({ app, project }: Props) {
  const [expanded, setExpanded] = useState(readExpanded);
  // Local names and archive flags, per open project.
  const [metadata, setMetadata] = useState<Record<string, SessionPreferences>>({});
  const [error, setError] = useState<string | null>(null);
  const icons = useProjectIcons();
  const openIds = app.open.map((p) => p.id).join("\n");
  useEffect(() => {
    setMetadata(
      Object.fromEntries(app.open.map((p) => [p.id, readSessionPreferences(p.id)])),
    );
  }, [openIds]);

  const change = (
    projectId: string,
    sessionId: string,
    update: { title?: string; archived?: boolean },
  ) => {
    try {
      const next = updateSessionPreferences(
        metadata[projectId] ?? parseSessionPreferences(null),
        sessionId,
        update,
      );
      localStorage.setItem(sessionPreferencesKey(projectId), JSON.stringify(next));
      setMetadata((current) => ({ ...current, [projectId]: next }));
      setError(null);
    } catch (error) {
      setError(String(error));
    }
  };

  // The active project starts expanded and the others collapsed until the
  // user chooses; their choices are remembered.
  const isExpanded = (id: string) => expanded[id] ?? id === app.activeId;
  const setCardExpanded = (id: string, open: boolean) =>
    setExpanded((current) => {
      const next = { ...current, [id]: open };
      try {
        localStorage.setItem(EXPANDED_KEY, JSON.stringify(next));
      } catch {
        /* Preference only. */
      }
      return next;
    });

  // Worktrees list right after their repository.
  const sorted = useMemo(() => {
    const repoOf = (p: ProjectInfo) =>
      (p.worktree?.main_name ?? p.name).toLowerCase();
    return [...app.open].sort(
      (a, b) =>
        repoOf(a).localeCompare(repoOf(b)) ||
        Number(Boolean(a.worktree)) - Number(Boolean(b.worktree)) ||
        a.name.localeCompare(b.name),
    );
  }, [app.open]);

  const cards = sorted.map((info) => {
    const state = app.projects[info.id] ?? null;
    const meta = metadata[info.id] ?? parseSessionPreferences(null);
    const rows = (state?.sessions ?? []).map((row) => ({
      ...row,
      title: meta.aliases[row.id] ?? row.title,
    }));
    return { info, state, meta, rows };
  });

  return (
    <aside className="flex w-[280px] shrink-0 flex-col border-r border-line bg-sidebar">
      <div className="space-y-1 px-2 pb-2 pt-2">
        <button
          onClick={() => showSearch("all")}
          aria-label="Search sessions, projects and commands"
          className="flex h-8 w-full items-center gap-2 rounded-lg border border-line-strong bg-raised px-2.5 text-left text-ui text-faint hover:border-accent/60 hover:text-muted"
        >
          <Search className="h-3.5 w-3.5 shrink-0" />
          <span className="flex-1">Search</span>
          <kbd className="font-mono text-caption">Ctrl K</kbd>
        </button>
        {project && (
          <button
            onClick={() => void api.newSession(project.project_id)}
            className={clsx(
              "flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-left text-ui hover:bg-hover",
              project.focused === null && "bg-active",
            )}
          >
            <SquarePen className="h-4 w-4 text-muted" />
            <span className="flex-1">New session</span>
            <kbd className="font-mono text-caption text-faint">Ctrl N</kbd>
          </button>
        )}
        {error && (
          <p role="alert" className="px-1 text-small text-danger">
            {error}
          </p>
        )}
      </div>

      <div className="flex items-center gap-2 px-4 pb-1 pt-1 text-caption font-semibold uppercase tracking-wider text-faint">
        <span className="flex-1">Projects</span>
        <button
          onClick={() => void pickProject()}
          aria-label="Open project"
          title="Open project (Ctrl O)"
          className="flex h-5 w-5 items-center justify-center rounded text-faint hover:bg-hover hover:text-fg"
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
      </div>

      <nav
        aria-label="Projects and sessions"
        className="min-h-0 flex-1 space-y-1 overflow-y-auto px-2 pb-3"
      >
        {cards.map(({ info, state, meta, rows }) => (
          <ProjectCard
            key={info.id}
            info={info}
            state={state}
            active={info.id === app.activeId}
            icon={icons[info.path]}
            metadata={meta}
            rows={rows}
            expanded={isExpanded(info.id)}
            setExpanded={(open) => setCardExpanded(info.id, open)}
            onChange={(sessionId, update) => change(info.id, sessionId, update)}
          />
        ))}
        {app.open.length === 0 && (
          <button
            onClick={() => void pickProject()}
            className="flex w-full items-center gap-2.5 rounded-xl border border-dashed border-line-strong px-3 py-3 text-left text-ui text-muted hover:bg-hover hover:text-fg"
          >
            <FolderPlus className="h-4 w-4" />
            Open a project folder
          </button>
        )}
      </nav>

      <footer className="flex items-center gap-2 border-t border-line px-3 py-2 text-small text-muted">
        <ConnectionDot connection={project?.connection ?? null} />
        <span className="flex-1 truncate">
          {connectionLabel(project?.connection ?? null)}
          {project?.owns_server === false && project.connection === "live" && (
            <span className="text-faint"> · shared server</span>
          )}
        </span>
        <button
          onClick={() => showSettings()}
          title="Settings (Ctrl ,)"
          aria-label="Settings"
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg"
        >
          <SettingsIcon className="h-4 w-4" />
        </button>
      </footer>
    </aside>
  );
}

function ProjectCard({
  info,
  state,
  active,
  icon,
  metadata,
  rows,
  expanded,
  setExpanded,
  onChange,
}: {
  info: ProjectInfo;
  state: ProjectState | null;
  active: boolean;
  icon: Parameters<typeof ProjectBadge>[0]["icon"];
  metadata: SessionPreferences;
  rows: SessionRow[];
  expanded: boolean;
  setExpanded: (open: boolean) => void;
  onChange: (
    sessionId: string,
    update: { title?: string; archived?: boolean },
  ) => void;
}) {
  const [showArchived, setShowArchived] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [customizing, setCustomizing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const menu = useRef<HTMLSpanElement>(null);
  const sessions = state?.sessions ?? [];
  const needs = sessions.filter((s) => s.group === "needs_you").length;
  const working = sessions.some((s) => s.status !== "idle");
  const archivedCount = metadata.archived.length;

  const list = useMemo(() => {
    const archived = (row: SessionRow) => metadata.archived.includes(row.id);
    // Sessions that are running or need you stay visible even when archived.
    const shown = rows.filter((row) =>
      showArchived
        ? archived(row)
        : !archived(row) || row.status !== "idle" || row.group === "needs_you",
    );
    if (showArchived) return { rows: shown, hidden: 0 };
    // Needs-you sessions surface at any depth; otherwise roots only, and
    // children are reached from their parent's transcript.
    const needsYou = shown.filter((r) => r.group === "needs_you");
    const roots = shown.filter((r) => r.depth === 0 && r.group !== "needs_you");
    const running = roots.filter((r) => r.group === "working");
    const recent = roots
      .filter((r) => r.group === "idle" || r.group === "done")
      .sort((a, b) => b.updated_at_ms - a.updated_at_ms);
    const limit = showAll ? recent.length : RECENT_LIMIT;
    return {
      rows: [...needsYou, ...running, ...recent.slice(0, limit)],
      hidden: Math.max(0, recent.length - limit),
    };
  }, [rows, metadata.archived, showArchived, showAll]);

  const open = () => {
    if (!active) {
      activate(info.id);
      setExpanded(true);
    } else setExpanded(!expanded);
  };

  return (
    <section
      className={clsx(
        "rounded-xl border",
        active ? "border-line bg-raised/70" : "border-transparent",
      )}
    >
      <div
        className="group/card relative flex items-center"
        onContextMenu={(event) => {
          event.preventDefault();
          menu.current?.querySelector("button")?.click();
        }}
      >
        <button
          onClick={open}
          aria-expanded={expanded}
          title={info.path}
          className={clsx(
            "flex min-w-0 flex-1 items-center gap-2.5 rounded-xl py-2 pl-2 pr-9 text-left group-focus-within/card:pr-[84px] group-hover/card:pr-[84px]",
            !active && "hover:bg-hover",
          )}
        >
          <ProjectBadge project={info} icon={icon} />
          <span className="min-w-0 flex-1">
            <span className="block truncate text-ui font-medium">
              {info.worktree?.branch ?? info.name}
            </span>
            <span className="block truncate text-caption text-faint">
              {info.worktree
                ? `worktree of ${info.worktree.main_name}`
                : displayPath(info.path)}
            </span>
          </span>
        </button>
        <div className="absolute right-1.5 flex items-center gap-0.5">
          {needs > 0 && !expanded && (
            <span
              title={`${needs} session${needs === 1 ? "" : "s"} need you`}
              className="mr-1 rounded-full bg-warn-soft px-1.5 text-caption font-semibold text-warn group-hover/card:hidden"
            >
              {needs}
            </span>
          )}
          <button
            onClick={() => {
              if (!active) activate(info.id);
              setExpanded(true);
              void api.newSession(info.id);
            }}
            aria-label={`New session in ${info.name}`}
            title="New session"
            className="flex h-6 w-6 items-center justify-center rounded-md text-muted opacity-0 hover:bg-active hover:text-fg focus-visible:opacity-100 group-hover/card:opacity-100"
          >
            <Plus className="h-3.5 w-3.5" />
          </button>
          <span ref={menu}>
            <Dropdown
              label={`Actions for ${info.name}`}
              title="More actions"
              value={null}
              options={[]}
              onChange={() => undefined}
              side="bottom"
              align="end"
              bare
              className="flex h-6 w-6 items-center justify-center rounded-md text-muted opacity-0 hover:bg-active hover:text-fg focus-visible:opacity-100 group-hover/card:opacity-100 aria-expanded:bg-active aria-expanded:opacity-100"
              actions={[
                {
                  label: "Customize icon…",
                  icon: <Palette className="h-3.5 w-3.5" />,
                  onSelect: () => setCustomizing(true),
                },
                {
                  label: "New worktree session…",
                  icon: <GitBranchPlus className="h-3.5 w-3.5" />,
                  onSelect: () => showWorktreeDialog(info.id),
                },
                {
                  label: showArchived
                    ? "Hide archived sessions"
                    : `Show archived sessions (${archivedCount})`,
                  icon: <Archive className="h-3.5 w-3.5" />,
                  onSelect: () => {
                    setShowArchived((value) => !value);
                    setExpanded(true);
                  },
                },
                ...(info.worktree
                  ? [
                      {
                        label: "Remove this worktree…",
                        icon: <Trash2 className="h-3.5 w-3.5" />,
                        danger: true,
                        onSelect: () => setRemoving(true),
                      },
                    ]
                  : []),
                {
                  label: `Close ${info.name}`,
                  icon: <X className="h-3.5 w-3.5" />,
                  onSelect: () => void closeProject(info.id),
                },
              ]}
            >
              <MoreHorizontal className="h-4 w-4" />
            </Dropdown>
          </span>
          <button
            onClick={() => setExpanded(!expanded)}
            aria-label={expanded ? `Collapse ${info.name}` : `Expand ${info.name}`}
            className="flex h-6 w-6 items-center justify-center rounded-md text-faint hover:bg-active hover:text-fg"
          >
            <ChevronRight
              className={clsx("h-3.5 w-3.5 transition-transform", expanded && "rotate-90")}
            />
          </button>
        </div>
      </div>

      {expanded && (
        <div className="pb-1.5">
          {showArchived && (
            <div className="flex items-center gap-2 px-3 pb-1 text-caption text-faint">
              <span className="flex-1">Archived on this desktop</span>
              <button
                onClick={() => setShowArchived(false)}
                className="rounded px-1 hover:bg-hover hover:text-fg"
              >
                Done
              </button>
            </div>
          )}
          <ul className="space-y-px px-1">
            {list.rows.map((row) => (
              <SessionItem
                key={row.id}
                row={row}
                projectId={info.id}
                active={active && row.id === state?.focused}
                archived={metadata.archived.includes(row.id)}
                onChange={(update) => onChange(row.id, update)}
              />
            ))}
          </ul>
          {list.hidden > 0 && (
            <button
              onClick={() => setShowAll(true)}
              className="ml-1 mt-0.5 rounded-md px-2.5 py-1 text-small text-muted hover:bg-hover hover:text-fg"
            >
              Show {list.hidden} more
            </button>
          )}
          {showAll && !showArchived && list.hidden === 0 && sessions.length > RECENT_LIMIT && (
            <button
              onClick={() => setShowAll(false)}
              className="ml-1 mt-0.5 rounded-md px-2.5 py-1 text-small text-muted hover:bg-hover hover:text-fg"
            >
              Show fewer
            </button>
          )}
          {list.rows.length === 0 && (
            <p className="px-3 py-1 text-small text-faint">
              {showArchived
                ? "Nothing archived. Archive keeps a session out of this list on this desktop only."
                : state?.connection === "live"
                  ? "No sessions yet."
                  : connectionLabel(state?.connection ?? null)}
            </p>
          )}
        </div>
      )}
      {customizing && (
        <IconDialog project={info} icon={icon} close={() => setCustomizing(false)} />
      )}
      {removing && info.worktree && (
        <RemoveWorktreeDialog
          project={info}
          running={working}
          close={() => setRemoving(false)}
        />
      )}
    </section>
  );
}

/** Deletes a worktree's folder after confirmation; its branch is kept. */
function RemoveWorktreeDialog({
  project,
  running,
  close,
}: {
  project: ProjectInfo;
  running: boolean;
  close: () => void;
}) {
  const [force, setForce] = useState(false);
  const branch = project.worktree?.branch;
  return (
    <ConfirmDialog
      title="Remove this worktree?"
      confirmLabel={force ? "Discard changes and remove" : "Remove worktree"}
      onConfirm={async () => {
        if (running)
          throw new Error("A session here is still running. Stop it first.");
        await removeWorktree(project.id, force);
        toast(`Removed the ${branch ?? project.name} worktree`);
      }}
      close={close}
    >
      <p>
        Deletes <code className="font-mono text-fg">{project.path}</code> and
        closes it here.
        {branch ? (
          <>
            {" "}The branch <code className="font-mono text-fg">{branch}</code>{" "}
            and its commits stay in the repository.
          </>
        ) : null}
      </p>
      <label className="mt-3 flex items-center gap-2">
        <input
          type="checkbox"
          checked={force}
          onChange={(event) => setForce(event.target.checked)}
          className="accent-[var(--danger)]"
        />
        Also discard uncommitted changes (otherwise Git refuses if there are any)
      </label>
    </ConfirmDialog>
  );
}

/** The project's parent folder, shortened from the left. */
function displayPath(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const parent = parts.slice(0, -1);
  return parent.length > 2 ? `…/${parent.slice(-2).join("/")}` : parent.join("/");
}

function SessionItem({
  row,
  projectId,
  active,
  archived,
  onChange,
}: {
  row: SessionRow;
  projectId: string;
  active: boolean;
  archived: boolean;
  onChange: (update: { title?: string; archived?: boolean }) => void;
}) {
  const working = row.status !== "idle";
  const [renaming, setRenaming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const status =
    row.awaiting_approval > 0
      ? "Waiting for your approval"
      : working
        ? row.active_tool
          ? `Running ${row.active_tool}`
          : row.tail
        : "";
  return (
    <li className="group/row relative">
      {renaming ? (
        <RenameField
          initial={row.title}
          depth={row.depth}
          save={(title) => {
            onChange({ title });
            setRenaming(false);
          }}
          cancel={() => setRenaming(false)}
        />
      ) : (
        <button
          onClick={() => {
            activate(projectId);
            void api.focusSession(projectId, row.id);
          }}
          onDoubleClick={() => setRenaming(true)}
          aria-current={active ? "page" : undefined}
          className={clsx(
            "flex w-full items-start gap-2.5 rounded-md py-1.5 pr-9 text-left",
            active ? "bg-active" : "hover:bg-hover",
          )}
          style={{ paddingLeft: `${10 + Math.min(row.depth, 3) * 12}px` }}
        >
          <span className="mt-[5px] flex h-3 w-3 shrink-0 items-center justify-center">
            <StatusGlyph row={row} />
          </span>
          <span className="min-w-0 flex-1">
            <span
              className={clsx(
                "block truncate text-ui",
                row.unread > 0 || row.group === "needs_you"
                  ? "font-medium text-fg"
                  : "text-fg/90",
              )}
            >
              {row.title}
            </span>
            {status && (
              <span className="mt-0.5 block truncate text-small text-muted">
                {status}
              </span>
            )}
          </span>
        </button>
      )}
      {!renaming && (
        <div className="absolute right-1 top-1 flex items-center">
          {!working && (
            <span className="pointer-events-none absolute right-1.5 top-[5px] whitespace-nowrap text-caption text-faint group-focus-within/row:invisible group-hover/row:invisible">
              {relativeTime(row.updated_at_ms)}
            </span>
          )}
          <Dropdown
            label={`Actions for ${row.title}`}
            title="More actions"
            value={null}
            options={[]}
            onChange={() => undefined}
            side="bottom"
            align="end"
            bare
            className="flex h-6 w-6 items-center justify-center rounded-md text-muted opacity-0 hover:bg-active hover:text-fg focus-visible:opacity-100 group-hover/row:opacity-100 aria-expanded:bg-active aria-expanded:opacity-100"
            actions={[
              {
                label: "Rename locally",
                icon: <Pencil className="h-3.5 w-3.5" />,
                onSelect: () => setRenaming(true),
              },
              {
                label: archived ? "Restore" : "Archive locally",
                icon: archived ? (
                  <ArchiveRestore className="h-3.5 w-3.5" />
                ) : (
                  <Archive className="h-3.5 w-3.5" />
                ),
                onSelect: () => onChange({ archived: !archived }),
              },
              {
                label: "Delete…",
                icon: <Trash2 className="h-3.5 w-3.5" />,
                danger: true,
                onSelect: () => setDeleting(true),
              },
            ]}
          >
            <MoreHorizontal className="h-4 w-4" />
          </Dropdown>
        </div>
      )}
      {deleting && (
        <ConfirmDialog
          title="Delete this session?"
          confirmLabel="Delete"
          onConfirm={() => api.deleteSession(projectId, row.id)}
          close={() => setDeleting(false)}
        >
          <span className="font-medium text-fg">{row.title}</span> and its
          history are removed from QQ. This cannot be undone.
        </ConfirmDialog>
      )}
    </li>
  );
}

/** In-place title editor. Names are kept on this desktop only. */
function RenameField({
  initial,
  depth,
  save,
  cancel,
}: {
  initial: string;
  depth: number;
  save: (title: string) => void;
  cancel: () => void;
}) {
  const [title, setTitle] = useState(initial);
  const done = useRef(false);
  const finish = (commit: boolean) => {
    if (done.current) return;
    done.current = true;
    if (commit && title.trim() !== initial) save(title);
    else cancel();
  };
  return (
    <div
      className="flex items-center gap-2.5 rounded-md bg-active py-1 pr-1.5"
      style={{ paddingLeft: `${10 + Math.min(depth, 3) * 12}px` }}
    >
      <span className="h-3 w-3 shrink-0" />
      <input
        autoFocus
        onFocus={(event) => event.currentTarget.select()}
        aria-label="Session name on this desktop"
        title="Enter to save, Esc to cancel. Clear it to use the session's own title."
        maxLength={160}
        value={title}
        onChange={(event) => setTitle(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            finish(true);
          } else if (event.key === "Escape") {
            event.preventDefault();
            finish(false);
          }
        }}
        onBlur={() => finish(true)}
        className="min-w-0 flex-1 rounded border border-accent/60 bg-bg px-1.5 py-0.5 text-ui outline-none focus-visible:outline-none"
      />
    </div>
  );
}

function StatusGlyph({ row }: { row: SessionRow }) {
  if (row.awaiting_approval > 0)
    return <CircleAlert className="h-3 w-3 text-warn" />;
  if (row.status !== "idle")
    return <Loader2 className="h-3 w-3 animate-spin text-accent" />;
  if (row.failed && row.group === "needs_you")
    return <span className="h-2 w-2 rounded-full bg-danger" />;
  if (row.unread > 0 || row.group === "needs_you")
    return <span className="h-2 w-2 rounded-full bg-accent" />;
  return <span className="h-1.5 w-1.5 rounded-full bg-line-strong" />;
}

function ConnectionDot({ connection }: { connection: Connection | null }) {
  const color =
    connection === "live"
      ? "bg-ok"
      : connection === "failed" || connection === "offline"
        ? "bg-danger"
        : "bg-warn animate-qq-pulse";
  return (
    <span
      className={clsx(
        "h-2 w-2 shrink-0 rounded-full",
        connection ? color : "bg-line-strong",
      )}
    />
  );
}

function connectionLabel(connection: Connection | null): string {
  switch (connection) {
    case null:
      return "No project open";
    case "starting":
      return "Starting engine…";
    case "connecting":
      return "Connecting…";
    case "replaying":
      return "Catching up…";
    case "live":
      return "Connected";
    case "offline":
      return "Offline, reconnecting…";
    case "failed":
      return "Engine unavailable";
  }
}


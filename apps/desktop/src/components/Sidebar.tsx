import { useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  Archive,
  ArchiveRestore,
  ChevronsUpDown,
  CircleAlert,
  FolderOpen,
  FolderPlus,
  GitBranch,
  GitBranchPlus,
  KeyRound,
  Loader2,
  MoreHorizontal,
  Pencil,
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
  showProviders,
} from "../lib/store";
import { relativeTime } from "../lib/format";
import type {
  Connection,
  ProjectInfo,
  ProjectState,
  SessionRow,
} from "../lib/types";
import {
  matchesSession,
  parseSessionPreferences,
  updateSessionPreferences,
  type SessionPreferences,
} from "../lib/session-preferences";
import { ConfirmDialog } from "./ConfirmDialog";
import { Dropdown } from "./Dropdown";
import { toast } from "../lib/toast";
import { showWorktreeDialog } from "../lib/worktree-dialog";
import { platform } from "../lib/platform";

interface Props {
  app: {
    open: ProjectInfo[];
    projects: Record<string, ProjectState>;
    activeId: string | null;
  };
  project: ProjectState | null;
  openSettings: () => void;
  onNavigate?: () => void;
}

export function Sidebar({ app, project, openSettings, onNavigate }: Props) {
  const [query, setQuery] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const [metadata, setMetadata] = useState<SessionPreferences>(() =>
    parseSessionPreferences(null),
  );
  const [error, setError] = useState<string | null>(null);
  const storageKey = `qq.sessions:${project?.project_id ?? ""}`;
  useEffect(() => {
    try {
      setMetadata(parseSessionPreferences(localStorage.getItem(storageKey)));
    } catch {
      setMetadata(parseSessionPreferences(null));
    }
    setQuery("");
    setShowArchived(false);
    setError(null);
  }, [storageKey]);
  const change = (
    id: string,
    update: { title?: string; archived?: boolean },
  ) => {
    try {
      const next = updateSessionPreferences(metadata, id, update);
      localStorage.setItem(storageKey, JSON.stringify(next));
      setMetadata(next);
      setError(null);
    } catch (error) {
      setError(String(error));
    }
  };
  const groups = useMemo(() => {
    const rows = (project?.sessions ?? []).map((row) => ({
      ...row,
      title: metadata.aliases[row.id] ?? row.title,
    }));
    const visible = rows.filter(
      (row) =>
        matchesSession(query, row.title, row.tail) &&
        (showArchived
          ? metadata.archived.includes(row.id)
          : !metadata.archived.includes(row.id) ||
            row.status !== "idle" ||
            row.group === "needs_you"),
    );
    return query.trim() || showArchived
      ? [
          [showArchived ? "Archived locally" : "Search results", visible] as [
            string,
            SessionRow[],
          ],
        ]
      : groupSessions(visible);
  }, [project?.sessions, metadata, query, showArchived]);

  return (
    <aside className="flex w-[272px] shrink-0 flex-col border-r border-line bg-sidebar pt-2">

      <ProjectSwitcher
        open={app.open}
        projects={app.projects}
        activeId={app.activeId}
      />

      {project && (
        <div className="px-2 pb-2">
          <button
            onClick={() => { void api.newSession(project.project_id); onNavigate?.(); }}
            className={clsx(
              "flex w-full items-center gap-2.5 rounded-md px-2.5 py-1.5 text-left text-[13px] hover:bg-hover",
              project.focused === null && "bg-active",
            )}
          >
            <SquarePen className="h-4 w-4 text-muted" />
            <span className="flex-1">New session</span>
            <kbd className="font-mono text-[10.5px] text-faint">Ctrl N</kbd>
          </button>
        </div>
      )}

      {project && (
        <div className="px-2 pb-1">
          <div className="flex items-center gap-1">
            <label className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-lg border border-line bg-raised px-2.5 focus-within:border-line-strong">
              <Search className="h-3.5 w-3.5 shrink-0 text-faint" />
              <input
                aria-label="Search sessions"
                placeholder="Search sessions"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Escape" && query) {
                    event.preventDefault();
                    setQuery("");
                  }
                }}
                className="min-w-0 flex-1 bg-transparent text-[13px] outline-none placeholder:text-faint focus-visible:outline-none"
              />
              {query ? (
                <button
                  type="button"
                  aria-label="Clear search"
                  title="Clear (Esc)"
                  onClick={() => setQuery("")}
                  className="rounded p-0.5 text-faint hover:text-fg"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              ) : (
                <kbd className="font-mono text-[10.5px] text-faint">Ctrl F</kbd>
              )}
            </label>
            <button
              aria-pressed={showArchived}
              aria-label={
                showArchived
                  ? "Show current sessions"
                  : `Show archived sessions (${metadata.archived.length})`
              }
              title={
                showArchived
                  ? "Show current sessions"
                  : `Archived on this desktop (${metadata.archived.length})`
              }
              onClick={() => setShowArchived((value) => !value)}
              className={clsx(
                "flex h-8 w-8 shrink-0 items-center justify-center rounded-lg",
                showArchived
                  ? "bg-active text-fg"
                  : "text-faint hover:bg-hover hover:text-fg",
              )}
            >
              <Archive className="h-3.5 w-3.5" />
            </button>
          </div>
          {error && (
            <p role="alert" className="text-[12px] text-danger">
              {error}
            </p>
          )}
        </div>
      )}
      <nav
        aria-label="Sessions"
        className="min-h-0 flex-1 overflow-y-auto px-2 pb-3"
      >
        {project &&
          groups.map(([label, rows]) =>
            rows.length === 0 ? null : (
              <section key={label} className="mt-3 first:mt-1">
                <div className="mb-1 flex items-center gap-2 px-2.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
                  {label}
                  <span className="font-normal">{rows.length}</span>
                </div>
                <ul className="space-y-px">
                  {rows.map((row) => (
                    <SessionItem
                      key={row.id}
                      row={row}
                      projectId={project.project_id}
                      onNavigate={onNavigate}
                      active={row.id === project.focused}
                      archived={metadata.archived.includes(row.id)}
                      onChange={(update) => change(row.id, update)}
                    />
                  ))}
                </ul>
              </section>
            ),
          )}
        {project &&
          (query.trim() || showArchived) &&
          groups.every(([, rows]) => rows.length === 0) && (
            <p className="px-2.5 pt-3 text-[12.5px] text-faint">
              {query.trim()
                ? "No sessions match."
                : "Nothing archived. Archive keeps a session out of this list on this desktop only."}
            </p>
          )}
        {project &&
          project.sessions.length === 0 &&
          project.connection === "live" && (
            <p className="px-2.5 pt-2 text-[12.5px] text-faint">
              No sessions yet.
            </p>
          )}
      </nav>

      <footer className="flex items-center gap-2 border-t border-line px-3 py-2 text-[12px] text-muted">
        <ConnectionDot connection={project?.connection ?? null} />
        <span className="flex-1 truncate">
          {connectionLabel(project?.connection ?? null)}
          {project?.owns_server === false && project.connection === "live" && (
            <span className="text-faint"> · shared server</span>
          )}
        </span>
        {platform.features.hostConfiguration && <button
          onClick={() => showProviders(true)}
          title="Model providers"
          className="flex items-center gap-1.5 rounded-md px-1.5 py-1 text-muted hover:bg-hover hover:text-fg"
        >
          <KeyRound className="h-3.5 w-3.5" />
          Providers
        </button>}
        <button
          onClick={openSettings}
          title="Settings (Ctrl ,)"
          aria-label="Settings"
          className="flex h-6 w-6 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg"
        >
          <SettingsIcon className="h-3.5 w-3.5" />
        </button>
      </footer>
    </aside>
  );
}

function ProjectSwitcher({
  open,
  projects,
  activeId,
}: {
  open: ProjectInfo[];
  projects: Record<string, ProjectState>;
  activeId: string | null;
}) {
  const active = open.find((p) => p.id === activeId) ?? null;
  const needsYou = (id: string) =>
    projects[id]?.sessions.filter((s) => s.group === "needs_you").length ?? 0;
  const waiting = open
    .filter((p) => p.id !== activeId)
    .reduce((total, p) => total + needsYou(p.id), 0);
  const [removing, setRemoving] = useState(false);
  // Worktrees list right after their repository, under its name.
  const repoOf = (p: ProjectInfo) => (p.worktree?.main_name ?? p.name).toLowerCase();
  const sorted = [...open].sort(
    (a, b) =>
      repoOf(a).localeCompare(repoOf(b)) ||
      Number(Boolean(a.worktree)) - Number(Boolean(b.worktree)) ||
      a.name.localeCompare(b.name),
  );
  const repoCounts = new Map<string, number>();
  for (const p of open) repoCounts.set(repoOf(p), (repoCounts.get(repoOf(p)) ?? 0) + 1);
  const running = active
    ? (projects[active.id]?.sessions.some((s) => s.status !== "idle") ?? false)
    : false;

  return (
    <div className="px-2 pb-2">
      <Dropdown
        label="Switch project"
        title={active?.path}
        value={activeId}
        side="bottom"
        bare
        rootClassName="w-full"
        className="group/project flex w-full items-center gap-2.5 rounded-lg px-2 py-1.5 text-left hover:bg-hover aria-expanded:bg-hover"
        options={sorted.map((p) => {
          const needs = needsYou(p.id);
          return {
            value: p.id,
            label: p.worktree ? (p.worktree.branch ?? p.name) : p.name,
            hint:
              needs > 0
                ? `${needs} need${needs === 1 ? "s" : ""} you`
                : p.worktree
                  ? `Worktree · ${p.path}`
                  : p.path,
            group:
              (repoCounts.get(repoOf(p)) ?? 0) > 1
                ? (p.worktree?.main_name ?? p.name)
                : undefined,
          };
        })}
        onChange={activate}
        actions={[
          ...(platform.features.localProjects ? [{
            label: "Open project…",
            icon: <FolderPlus className="h-3.5 w-3.5" />,
            onSelect: () => void pickProject(),
          }] : []),
          ...(active && platform.features.worktrees
            ? [
                {
                  label: "New worktree session…",
                  icon: <GitBranchPlus className="h-3.5 w-3.5" />,
                  onSelect: () => showWorktreeDialog(active.id),
                },
              ]
            : []),
          ...(active?.worktree && platform.features.worktrees
            ? [
                {
                  label: "Remove this worktree…",
                  icon: <Trash2 className="h-3.5 w-3.5" />,
                  danger: true,
                  onSelect: () => setRemoving(true),
                },
              ]
            : []),
          ...(active
            ? [
                {
                  label: `Close ${active.name}`,
                  icon: <X className="h-3.5 w-3.5" />,
                  onSelect: () => void closeProject(active.id),
                },
              ]
            : []),
        ]}
      >
        <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-accent-soft text-[13px] font-semibold uppercase text-accent">
          {active?.worktree ? (
            <GitBranch className="h-4 w-4" />
          ) : (
            (active?.name.slice(0, 1) ?? <FolderOpen className="h-4 w-4" />)
          )}
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] font-medium">
            {active?.worktree?.branch ?? active?.name ?? "No project"}
          </span>
          {active && (
            <span className="block truncate text-[11px] text-faint">
              {active.worktree
                ? `worktree of ${active.worktree.main_name}`
                : displayPath(active.path)}
            </span>
          )}
        </span>
        {waiting > 0 && (
          <span title="Sessions in other projects need you">
            <Badge tone="warn">{waiting}</Badge>
          </span>
        )}
        <ChevronsUpDown className="h-3.5 w-3.5 shrink-0 text-faint group-hover/project:text-muted" />
      </Dropdown>
      {removing && active?.worktree && (
        <RemoveWorktreeDialog
          project={active}
          running={running}
          close={() => setRemoving(false)}
        />
      )}
    </div>
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
  onNavigate,
}: {
  row: SessionRow;
  projectId: string;
  active: boolean;
  archived: boolean;
  onChange: (update: { title?: string; archived?: boolean }) => void;
  onNavigate?: () => void;
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
          onClick={() => { void api.focusSession(projectId, row.id); onNavigate?.(); }}
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
                "block truncate text-[13px]",
                row.unread > 0 || row.group === "needs_you"
                  ? "font-medium text-fg"
                  : "text-fg/90",
              )}
            >
              {row.title}
            </span>
            {status && (
              <span className="mt-0.5 block truncate text-[11.5px] text-muted">
                {status}
              </span>
            )}
          </span>
        </button>
      )}
      {!renaming && (
        <div className="absolute right-1 top-1 flex items-center">
          {!working && (
            <span className="pointer-events-none absolute right-1.5 top-[5px] whitespace-nowrap text-[11px] text-faint group-focus-within/row:invisible group-hover/row:invisible">
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
        className="min-w-0 flex-1 rounded border border-accent/60 bg-bg px-1.5 py-0.5 text-[13px] outline-none focus-visible:outline-none"
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

function Badge({
  tone,
  children,
}: {
  tone: "warn";
  children: React.ReactNode;
}) {
  return (
    <span
      className={clsx(
        "rounded-full px-1.5 text-[10.5px] font-semibold",
        tone === "warn" && "bg-warn-soft text-warn",
      )}
    >
      {children}
    </span>
  );
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

/**
 * Needs-you sessions surface at any depth; otherwise roots only, children
 * are reached from their parent's transcript.
 */
function groupSessions(rows: SessionRow[]): Array<[string, SessionRow[]]> {
  const needs = rows.filter((r) => r.group === "needs_you");
  const roots = rows.filter((r) => r.depth === 0 && r.group !== "needs_you");
  const working = roots.filter((r) => r.group === "working");
  const recent = roots
    .filter((r) => r.group === "idle" || r.group === "done")
    .sort((a, b) => b.updated_at_ms - a.updated_at_ms);
  return [
    ["Needs you", needs],
    ["Working", working],
    ["Recent", recent],
  ];
}

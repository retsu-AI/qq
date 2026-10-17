import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  CircleAlert,
  FolderPlus,
  KeyRound,
  Loader2,
  SquarePen,
  Trash2,
  X,
} from "lucide-react";
import { api } from "../lib/api";
import { activate, closeProject, pickProject, showProviders } from "../lib/store";
import { relativeTime } from "../lib/format";
import type { Connection, ProjectInfo, ProjectState, SessionRow } from "../lib/types";
import { Logo } from "./Logo";
import { matchesSession, parseSessionPreferences, updateSessionPreferences, type SessionPreferences } from "../lib/session-preferences";

interface Props {
  app: { open: ProjectInfo[]; projects: Record<string, ProjectState>; activeId: string | null };
  project: ProjectState | null;
}

export function Sidebar({ app, project }: Props) {
  const [query, setQuery] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const [metadata, setMetadata] = useState<SessionPreferences>(() => parseSessionPreferences(null));
  const [error, setError] = useState<string | null>(null);
  const storageKey = `qq.sessions:${project?.project_id ?? ""}`;
  useEffect(() => {
    try { setMetadata(parseSessionPreferences(localStorage.getItem(storageKey))); }
    catch { setMetadata(parseSessionPreferences(null)); }
    setQuery(""); setShowArchived(false); setError(null);
  }, [storageKey]);
  const change = (id: string, update: { title?: string; archived?: boolean }) => {
    try {
      const next = updateSessionPreferences(metadata, id, update);
      localStorage.setItem(storageKey, JSON.stringify(next));
      setMetadata(next); setError(null);
    } catch (error) { setError(String(error)); }
  };
  const groups = useMemo(() => {
    const rows = (project?.sessions ?? []).map(row => ({ ...row, title: metadata.aliases[row.id] ?? row.title }));
    const visible = rows.filter(row => matchesSession(query, row.title, row.tail) &&
      (showArchived ? metadata.archived.includes(row.id) : !metadata.archived.includes(row.id) || row.status !== "idle" || row.group === "needs_you"));
    return query.trim() || showArchived ? [[showArchived ? "Archived locally" : "Search results", visible] as [string, SessionRow[]]] : groupSessions(visible);
  }, [project?.sessions, metadata, query, showArchived]);

  return (
    <aside className="flex w-[272px] shrink-0 flex-col border-r border-line bg-sidebar">
      <div className="flex h-12 items-center gap-2 px-4">
        <Logo className="h-5 w-5" />
        <span className="text-[13px] font-semibold tracking-tight">QQ</span>
      </div>

      <ProjectSwitcher open={app.open} projects={app.projects} activeId={app.activeId} />

      {project && (
        <div className="px-2 pb-2">
          <button
            onClick={() => void api.newSession(project.project_id)}
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

      {project && <div className="px-3 pb-2">
        <input aria-label="Search sessions" placeholder="Search sessions…" value={query} onChange={event => setQuery(event.target.value)} className="w-full rounded border border-line bg-raised px-2 py-1 text-[13px]" />
        <button aria-pressed={showArchived} onClick={() => setShowArchived(value => !value)} className="mt-1 text-[11px] text-muted">{showArchived ? "Show current sessions" : `Archived locally (${metadata.archived.length})`}</button>
        {error && <p role="alert" className="text-[12px] text-danger">{error}</p>}
      </div>}
      <nav aria-label="Sessions" className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
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
                      active={row.id === project.focused}
                      archived={metadata.archived.includes(row.id)}
                      onChange={update => change(row.id, update)}
                    />
                  ))}
                </ul>
              </section>
            ),
          )}
        {project && project.sessions.length === 0 && project.connection === "live" && (
          <p className="px-2.5 pt-2 text-[12.5px] text-faint">No sessions yet.</p>
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
        <button
          onClick={() => showProviders(true)}
          title="Model providers"
          className="flex items-center gap-1.5 rounded-md px-1.5 py-1 text-muted hover:bg-hover hover:text-fg"
        >
          <KeyRound className="h-3.5 w-3.5" />
          Providers
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
  const [expanded, setExpanded] = useState(false);
  const active = open.find((p) => p.id === activeId) ?? null;

  return (
    <div className="px-2 pb-2">
      <button
        onClick={() => setExpanded((v) => !v)}
        className="flex w-full items-center gap-2 rounded-md border border-line bg-raised px-2.5 py-1.5 text-left hover:bg-hover"
      >
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-medium">
            {active?.name ?? "No project"}
          </div>
          {active && <div className="truncate text-[11px] text-faint">{active.path}</div>}
        </div>
        <ChevronDown
          className={clsx("h-4 w-4 text-muted transition-transform", expanded && "rotate-180")}
        />
      </button>
      {expanded && (
        <div className="mt-1 rounded-md border border-line bg-raised py-1 shadow-lg">
          {open.map((p) => {
            const needs = projects[p.id]?.sessions.filter((s) => s.group === "needs_you").length ?? 0;
            return (
              <div key={p.id} className="group flex items-center">
                <button
                  onClick={() => {
                    activate(p.id);
                    setExpanded(false);
                  }}
                  className={clsx(
                    "flex min-w-0 flex-1 items-center gap-2 px-2.5 py-1.5 text-left text-[13px] hover:bg-hover",
                    p.id === activeId && "text-accent",
                  )}
                >
                  <span className="truncate">{p.name}</span>
                  {needs > 0 && <Badge tone="warn">{needs}</Badge>}
                </button>
                <button
                  title="Close project"
                  onClick={() => void closeProject(p.id)}
                  className="mr-1 rounded p-1 text-faint opacity-0 hover:bg-hover hover:text-fg group-hover:opacity-100"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              </div>
            );
          })}
          <button
            onClick={() => {
              setExpanded(false);
              void pickProject();
            }}
            className="flex w-full items-center gap-2 border-t border-line px-2.5 py-1.5 pt-2 text-left text-[13px] text-muted hover:bg-hover hover:text-fg"
          >
            <FolderPlus className="h-4 w-4" />
            Open project…
          </button>
        </div>
      )}
    </div>
  );
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
  const [title, setTitle] = useState(row.title);
  return (
    <li className="group relative">
      <button
        onClick={() => void api.focusSession(projectId, row.id)}
        className={clsx(
          "flex w-full items-start gap-2.5 rounded-md px-2.5 py-1.5 text-left",
          active ? "bg-active" : "hover:bg-hover",
        )}
        style={{ paddingLeft: `${10 + Math.min(row.depth, 3) * 12}px` }}
      >
        <span className="mt-[5px] flex h-3 w-3 shrink-0 items-center justify-center">
          <StatusGlyph row={row} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2">
            <span
              className={clsx(
                "truncate text-[13px]",
                row.unread > 0 || row.group === "needs_you" ? "font-medium text-fg" : "text-fg/90",
              )}
            >
              {row.title}
            </span>
            <span className="ml-auto shrink-0 text-[11px] text-faint group-hover:invisible">
              {working ? "" : relativeTime(row.updated_at_ms)}
            </span>
          </span>
          {(row.awaiting_approval > 0 || (working && (row.active_tool || row.tail))) && (
            <span className="mt-0.5 block truncate text-[11.5px] text-muted">
              {row.awaiting_approval > 0
                ? "Waiting for your approval"
                : row.active_tool
                  ? `Running ${row.active_tool}`
                  : row.tail}
            </span>
          )}
        </span>
      </button>
      {renaming && <form onSubmit={event => { event.preventDefault(); onChange({ title }); setRenaming(false); }} className="p-1">
        <input autoFocus aria-label="Local session name" maxLength={160} value={title} onChange={event => setTitle(event.target.value)} onKeyDown={event => { if (event.key === "Escape") setRenaming(false); }} className="w-full rounded border border-line bg-raised px-2 py-1" />
        <button type="submit" className="px-2 text-[11px]">Save local name</button><button type="button" onClick={() => setRenaming(false)} className="text-[11px]">Cancel</button>
      </form>}
      <div className="flex justify-end gap-2 px-2 text-[11px] text-muted opacity-0 group-hover:opacity-100 group-focus-within:opacity-100">
        <button onClick={() => { setTitle(row.title); setRenaming(true); }} title="Name on this desktop only">Rename locally</button>
        <button onClick={() => onChange({ archived: !archived })}>{archived ? "Restore" : "Archive locally"}</button>
      </div>
      <button
        title="Delete session"
        aria-label={`Delete ${row.title}`}
        onClick={(event) => {
          event.stopPropagation();
          if (confirm(`Delete "${row.title}"? This cannot be undone.`)) {
            void api.deleteSession(projectId, row.id);
          }
        }}
        className="absolute right-1.5 top-1.5 rounded p-1 text-faint opacity-0 hover:bg-active hover:text-danger group-hover:opacity-100 focus:opacity-100"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </button>
    </li>
  );
}

function StatusGlyph({ row }: { row: SessionRow }) {
  if (row.awaiting_approval > 0) return <CircleAlert className="h-3 w-3 text-warn" />;
  if (row.status !== "idle") return <Loader2 className="h-3 w-3 animate-spin text-accent" />;
  if (row.failed && row.group === "needs_you") return <span className="h-2 w-2 rounded-full bg-danger" />;
  if (row.unread > 0 || row.group === "needs_you")
    return <span className="h-2 w-2 rounded-full bg-accent" />;
  return <span className="h-1.5 w-1.5 rounded-full bg-line-strong" />;
}

function Badge({ tone, children }: { tone: "warn"; children: React.ReactNode }) {
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
  return <span className={clsx("h-2 w-2 shrink-0 rounded-full", connection ? color : "bg-line-strong")} />;
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

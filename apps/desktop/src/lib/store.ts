// App-wide client state: open projects (as pushed by the Rust core), which
// one is active, and the recent-project list. The core owns session state;
// this store only holds the latest projection of it.
import { useSyncExternalStore } from "react";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api } from "./api";
import type { ProjectInfo, ProjectState } from "./types";

interface AppSnapshot {
  projects: Record<string, ProjectState>;
  open: ProjectInfo[];
  recents: ProjectInfo[];
  activeId: string | null;
  error: string | null;
}

const ACTIVE_KEY = "qq.activeProject";
const OPEN_KEY = "qq.openProjects";

let snapshot: AppSnapshot = {
  projects: {},
  open: [],
  recents: [],
  activeId: null,
  error: null,
};
const listeners = new Set<() => void>();

function set(patch: Partial<AppSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  for (const listener of listeners) listener();
}

function persist() {
  try {
    localStorage.setItem(OPEN_KEY, JSON.stringify(snapshot.open.map((p) => p.path)));
    if (snapshot.activeId) localStorage.setItem(ACTIVE_KEY, snapshot.activeId);
  } catch {
    // Preferences only.
  }
}

export function useApp(): AppSnapshot {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => snapshot,
  );
}

export function activeProject(app: AppSnapshot): ProjectState | null {
  return app.activeId ? (app.projects[app.activeId] ?? null) : null;
}

export async function openProject(path: string) {
  try {
    const info = await api.openProject(path);
    const open = snapshot.open.some((p) => p.id === info.id)
      ? snapshot.open
      : [...snapshot.open, info];
    set({ open, activeId: info.id, error: null });
    persist();
    void refreshRecents();
  } catch (error) {
    set({ error: String(error) });
  }
}

export async function pickProject() {
  const path = await openDialog({ directory: true, multiple: false, title: "Open project folder" });
  if (typeof path === "string") await openProject(path);
}

export function activate(projectId: string) {
  set({ activeId: projectId });
  persist();
}

export async function closeProject(projectId: string) {
  await api.closeProject(projectId).catch(() => undefined);
  const open = snapshot.open.filter((p) => p.id !== projectId);
  const projects = { ...snapshot.projects };
  delete projects[projectId];
  const activeId =
    snapshot.activeId === projectId ? (open[0]?.id ?? null) : snapshot.activeId;
  set({ open, projects, activeId });
  persist();
}

export async function refreshRecents() {
  try {
    set({ recents: await api.recentProjects() });
  } catch {
    // The list is a convenience.
  }
}

export function dismissError() {
  set({ error: null });
}

let started = false;

/** Subscribe to core pushes and reopen what was open last time. */
export async function start() {
  if (started) return;
  started = true;
  await listen<ProjectState>("project-state", (event) => {
    const state = event.payload;
    if (!snapshot.open.some((p) => p.id === state.project_id)) return;
    set({ projects: { ...snapshot.projects, [state.project_id]: state } });
  });
  await refreshRecents();
  let paths: string[] = [];
  let active: string | null = null;
  try {
    paths = JSON.parse(localStorage.getItem(OPEN_KEY) ?? "[]");
    active = localStorage.getItem(ACTIVE_KEY);
  } catch {
    paths = [];
  }
  for (const path of paths) {
    await openProject(path);
  }
  if (active && snapshot.open.some((p) => p.id === active)) activate(active);
  // Folders passed on the command line open last so the first one is active.
  const launch = await api.launchPaths().catch(() => [] as string[]);
  for (const path of [...launch].reverse()) {
    await openProject(path);
  }
}

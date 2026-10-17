// App-wide client state: open projects (as pushed by the Rust core), which
// one is active, and the recent-project list. The core owns session state;
// this store only holds the latest projection of it.
import { useSyncExternalStore } from "react";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api } from "./api";
import type { PendingFile, ProjectInfo, ProjectState } from "./types";

interface AppSnapshot {
  projects: Record<string, ProjectState>;
  open: ProjectInfo[];
  recents: ProjectInfo[];
  activeId: string | null;
  error: string | null;
  providersOpen: boolean;
  /** Pending trust per project; absent until checked, empty when trusted. */
  trust: Record<string, PendingFile[]>;
}

const ACTIVE_KEY = "qq.activeProject";
const OPEN_KEY = "qq.openProjects";

let snapshot: AppSnapshot = {
  projects: {},
  open: [],
  recents: [],
  activeId: null,
  error: null,
  providersOpen: false,
  trust: {},
};
const listeners = new Set<() => void>();

function set(patch: Partial<AppSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  for (const listener of listeners) listener();
}

function persist() {
  try {
    localStorage.setItem(
      OPEN_KEY,
      JSON.stringify(snapshot.open.map((p) => p.path)),
    );
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
    // The actor can emit before open_project resolves; request a projection
    // after registration so the first state cannot be lost.
    await api.refresh(info.id);
    persist();
    void refreshRecents();
    void checkTrust(info.id);
  } catch (error) {
    set({ error: String(error) });
  }
}

export async function pickProject() {
  const path = await openDialog({
    directory: true,
    multiple: false,
    title: "Open project folder",
  });
  if (typeof path === "string") await openProject(path);
}

export function activate(projectId: string) {
  set({ activeId: projectId });
  persist();
  // The configuration may have changed on disk since it was last trusted.
  void checkTrust(projectId);
}

export async function checkTrust(projectId: string) {
  try {
    const pending = await api.projectTrust(projectId);
    set({ trust: { ...snapshot.trust, [projectId]: pending } });
  } catch (error) {
    set({ error: String(error) });
  }
}

/** Throws so the prompt can show why trust could not be recorded. */
export async function trustProject(projectId: string) {
  await api.trustProject(projectId);
  set({ trust: { ...snapshot.trust, [projectId]: [] } });
  await checkTrust(projectId);
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

export function showProviders(open: boolean) {
  set({ providersOpen: open });
}

export function dismissError() {
  set({ error: null });
}

let started = false;

/** Subscribe to core pushes and reopen what was open last time. */
export async function start() {
  if (started) return;
  started = true;
  await listen<string[]>("open-paths", (event) => {
    for (const path of event.payload) void openProject(path);
  });
  window.addEventListener("unhandledrejection", (event) => {
    event.preventDefault();
    set({ error: String(event.reason) });
  });
  await listen<ProjectState>("project-state", (event) => {
    const state = event.payload;
    if (!snapshot.open.some((p) => p.id === state.project_id)) return;
    set({ projects: { ...snapshot.projects, [state.project_id]: state } });
  });
  await refreshRecents();
  // A project file edited while the app was in the background needs trust
  // again; ask when the user comes back rather than failing silently.
  window.addEventListener("focus", () => {
    if (snapshot.activeId) void checkTrust(snapshot.activeId);
  });
  let paths: string[] = [];
  let active: string | null = null;
  try {
    const saved: unknown = JSON.parse(localStorage.getItem(OPEN_KEY) ?? "[]");
    paths = Array.isArray(saved)
      ? saved
          .filter((path): path is string => typeof path === "string")
          .slice(0, 16)
      : [];
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

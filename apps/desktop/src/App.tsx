import { useEffect } from "react";
import { Sidebar } from "./components/Sidebar";
import { SessionPane } from "./components/SessionPane";
import { Welcome } from "./components/Welcome";
import { Providers } from "./components/Providers";
import { activeProject, dismissError, pickProject, useApp } from "./lib/store";
import { api } from "./lib/api";

export function App() {
  const app = useApp();
  const project = activeProject(app);

  // Global shortcuts: Ctrl+N new session, Ctrl+O open project,
  // Ctrl+Shift+A jump to the next session that needs you.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.ctrlKey) return;
      const key = event.key.toLowerCase();
      if (key === "o" && !event.shiftKey) {
        event.preventDefault();
        void pickProject();
      } else if (key === "n" && !event.shiftKey && project) {
        event.preventDefault();
        void api.newSession(project.project_id);
      } else if (key === "a" && event.shiftKey && project) {
        event.preventDefault();
        const next = project.sessions.find(
          (s) => s.group === "needs_you" && s.id !== project.focused,
        );
        if (next) void api.focusSession(project.project_id, next.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [project]);

  return (
    <div className="flex h-full bg-bg text-fg">
      <Sidebar app={app} project={project} />
      <main className="relative flex min-w-0 flex-1 flex-col">
        {app.error && (
          <div className="absolute inset-x-0 top-0 z-20 flex items-center gap-3 border-b border-line bg-danger-soft px-4 py-2 text-[13px] text-danger">
            <span className="flex-1 truncate">{app.error}</span>
            <button className="text-muted hover:text-fg" onClick={dismissError}>
              Dismiss
            </button>
          </div>
        )}
        {project ? <SessionPane project={project} /> : <Welcome recents={app.recents} />}
      </main>
      {app.providersOpen && <Providers />}
    </div>
  );
}

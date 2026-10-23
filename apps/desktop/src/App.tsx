import { useEffect, useReducer, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Settings } from "./components/Settings";
import { CommandPalette } from "./components/CommandPalette";
import { TitleBar, type Menu } from "./components/TitleBar";
import { Logo } from "./components/Logo";
import {
  applyPreferences,
  getPreferences,
  updatePreferences,
  usePreferences,
  zoom,
} from "./lib/preferences";
import { Sidebar } from "./components/Sidebar";
import { ToastView } from "./components/ToastView";
import { WorktreeDialog } from "./components/WorktreeDialog";
import {
  hideWorktreeDialog,
  showWorktreeDialog,
  useWorktreeDialog,
} from "./lib/worktree-dialog";
import { openInEditor } from "./lib/editor";
import { SessionPane } from "./components/SessionPane";
import { Welcome } from "./components/Welcome";
import { Providers } from "./components/Providers";
import {
  activate,
  activeProject,
  closeProject,
  dismissError,
  pickProject,
  showProviders,
  useApp,
} from "./lib/store";
import { api } from "./lib/api";

const DOCS_URL = "https://retsu-ai.github.io/qq/";
const ISSUES_URL = "https://github.com/retsu-AI/qq/issues";
const SIDEBAR_KEY = "qq.sidebar";

interface Place {
  projectId: string;
  sessionId: string | null;
}

function readSidebar(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_KEY) !== "hidden";
  } catch {
    return true;
  }
}

/** Runs an editing command against the field that had focus. */
function edit(command: "undo" | "redo" | "cut" | "copy" | "selectAll") {
  document.execCommand(command);
}

async function paste() {
  try {
    const text = await navigator.clipboard.readText();
    document.execCommand("insertText", false, text);
  } catch {
    // Clipboard read can be refused; Ctrl V still works natively.
  }
}

export function App() {
  const app = useApp();
  const project = activeProject(app);
  const preferences = usePreferences();
  const worktreeFor = useWorktreeDialog();
  const worktreeProject = app.open.find((p) => p.id === worktreeFor) ?? null;
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [aboutOpen, setAboutOpen] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(readSidebar);

  useEffect(() => {
    applyPreferences(getPreferences());
    void api.setNotificationsEnabled(getPreferences().notifications);
  }, []);

  /** Shows the sidebar if hidden and focuses its search. */
  const findSession = () => {
    if (!sidebarOpen) toggleSidebar();
    requestAnimationFrame(() => {
      const search = document.querySelector<HTMLInputElement>(
        'input[aria-label="Search sessions"]',
      );
      search?.focus();
      search?.select();
    });
  };
  const toggleSidebar = () =>
    setSidebarOpen((open) => {
      try {
        localStorage.setItem(SIDEBAR_KEY, open ? "hidden" : "shown");
      } catch {
        /* Preference only. */
      }
      return !open;
    });

  // Back/forward through the places the user looked at, like a browser.
  const history = useRef<{ stack: Place[]; index: number }>({
    stack: [],
    index: -1,
  });
  const [, rerender] = useReducer((count: number) => count + 1, 0);
  useEffect(() => {
    if (!project) return;
    const place = { projectId: project.project_id, sessionId: project.focused };
    const state = history.current;
    const current = state.stack[state.index];
    if (
      current?.projectId === place.projectId &&
      current.sessionId === place.sessionId
    )
      return;
    state.stack = [...state.stack.slice(0, state.index + 1), place].slice(-50);
    state.index = state.stack.length - 1;
    rerender();
  }, [project?.project_id, project?.focused]);
  const go = (delta: -1 | 1) => {
    const state = history.current;
    const target = state.stack[state.index + delta];
    if (!target) return;
    state.index += delta;
    rerender();
    activate(target.projectId);
    void api.focusSession(target.projectId, target.sessionId).catch(() => {});
  };
  const canBack = history.current.index > 0;
  const canForward = history.current.index < history.current.stack.length - 1;

  const toggleChanges = () =>
    updatePreferences({ changesPanel: !getPreferences().changesPanel });

  const newSession = () => {
    if (project) void api.newSession(project.project_id);
  };

  // Global shortcuts. Ctrl N new session, Ctrl O open project, Ctrl , settings,
  // Ctrl K commands, Ctrl B sidebar, Ctrl F find session, Ctrl +/-/0 zoom,
  // Alt ←/→ history, Ctrl Shift A next session that needs you, Ctrl Shift D
  // changes panel, Ctrl Shift E open in editor, Ctrl Shift N worktree session.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (document.querySelector("dialog[open]")) return;
      if (
        event.altKey &&
        !event.ctrlKey &&
        (event.key === "ArrowLeft" || event.key === "ArrowRight")
      ) {
        event.preventDefault();
        go(event.key === "ArrowLeft" ? -1 : 1);
        return;
      }
      if (!event.ctrlKey) return;
      const key = event.key.toLowerCase();
      const handled = () => event.preventDefault();
      if (key === ",") {
        handled();
        setSettingsOpen(true);
      } else if (key === "k") {
        handled();
        setPaletteOpen(true);
      } else if (key === "b" && !event.shiftKey) {
        handled();
        toggleSidebar();
      } else if (key === "f" && !event.shiftKey && project) {
        handled();
        findSession();
      } else if (key === "=" || key === "+") {
        handled();
        zoom(1);
      } else if (key === "-") {
        handled();
        zoom(-1);
      } else if (key === "0") {
        handled();
        zoom(0);
      } else if (key === "o" && !event.shiftKey) {
        handled();
        void pickProject();
      } else if (key === "n" && !event.shiftKey && project) {
        handled();
        newSession();
      } else if (key === "n" && event.shiftKey && project) {
        handled();
        showWorktreeDialog(project.project_id);
      } else if (key === "e" && event.shiftKey && project) {
        handled();
        void openInEditor(project.project_id);
      } else if (key === "d" && event.shiftKey) {
        handled();
        toggleChanges();
      } else if (key === "a" && event.shiftKey && project) {
        handled();
        const next = project.sessions.find(
          (s) => s.group === "needs_you" && s.id !== project.focused,
        );
        if (next) void api.focusSession(project.project_id, next.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const theme = (value: "system" | "light" | "dark") => () =>
    updatePreferences({ theme: value });
  const menus: Menu[] = [
    {
      label: "File",
      mnemonic: "f",
      items: [
        {
          label: "New session",
          shortcut: "Ctrl N",
          onSelect: newSession,
          disabled: !project,
        },
        {
          label: "New worktree session…",
          shortcut: "Ctrl Shift N",
          onSelect: () => project && showWorktreeDialog(project.project_id),
          disabled: !project,
        },
        {
          label: "Open project…",
          shortcut: "Ctrl O",
          onSelect: () => void pickProject(),
        },
        {
          label: "Open in editor",
          shortcut: "Ctrl Shift E",
          onSelect: () => project && void openInEditor(project.project_id),
          disabled: !project,
        },
        {
          label: "Close project",
          onSelect: () => {
            if (project) void closeProject(project.project_id);
          },
          disabled: !project,
        },
        "separator",
        { label: "Model providers…", onSelect: () => showProviders(true) },
        {
          label: "Settings…",
          shortcut: "Ctrl ,",
          onSelect: () => setSettingsOpen(true),
        },
        "separator",
        {
          label: "Exit",
          shortcut: "Alt F4",
          onSelect: () => void getCurrentWindow().close(),
        },
      ],
    },
    {
      label: "Edit",
      mnemonic: "e",
      items: [
        { label: "Undo", shortcut: "Ctrl Z", onSelect: () => edit("undo") },
        { label: "Redo", shortcut: "Ctrl Y", onSelect: () => edit("redo") },
        "separator",
        { label: "Cut", shortcut: "Ctrl X", onSelect: () => edit("cut") },
        { label: "Copy", shortcut: "Ctrl C", onSelect: () => edit("copy") },
        { label: "Paste", shortcut: "Ctrl V", onSelect: () => void paste() },
        {
          label: "Select all",
          shortcut: "Ctrl A",
          onSelect: () => edit("selectAll"),
        },
        "separator",
        {
          label: "Find session",
          shortcut: "Ctrl F",
          onSelect: findSession,
          disabled: !project,
        },
        {
          label: "Command palette…",
          shortcut: "Ctrl K",
          onSelect: () => setPaletteOpen(true),
        },
      ],
    },
    {
      label: "View",
      mnemonic: "v",
      items: [
        {
          label: sidebarOpen ? "Hide sidebar" : "Show sidebar",
          shortcut: "Ctrl B",
          onSelect: toggleSidebar,
        },
        {
          label: preferences.changesPanel
            ? "Hide changes panel"
            : "Show changes panel",
          shortcut: "Ctrl Shift D",
          onSelect: toggleChanges,
        },
        {
          label: "Back",
          shortcut: "Alt ←",
          onSelect: () => go(-1),
          disabled: !canBack,
        },
        {
          label: "Forward",
          shortcut: "Alt →",
          onSelect: () => go(1),
          disabled: !canForward,
        },
        "separator",
        {
          label: "System theme",
          checked: preferences.theme === "system",
          onSelect: theme("system"),
        },
        {
          label: "Light theme",
          checked: preferences.theme === "light",
          onSelect: theme("light"),
        },
        {
          label: "Dark theme",
          checked: preferences.theme === "dark",
          onSelect: theme("dark"),
        },
        "separator",
        { label: "Zoom in", shortcut: "Ctrl +", onSelect: () => zoom(1) },
        { label: "Zoom out", shortcut: "Ctrl -", onSelect: () => zoom(-1) },
        { label: "Actual size", shortcut: "Ctrl 0", onSelect: () => zoom(0) },
      ],
    },
    {
      label: "Help",
      mnemonic: "h",
      items: [
        { label: "Documentation", onSelect: () => void openUrl(DOCS_URL) },
        {
          label: "Keyboard shortcuts",
          onSelect: () => setSettingsOpen(true),
        },
        { label: "Report an issue", onSelect: () => void openUrl(ISSUES_URL) },
        "separator",
        { label: "About QQ", onSelect: () => setAboutOpen(true) },
      ],
    },
  ];

  return (
    <div className="flex h-full flex-col bg-bg text-fg">
      <TitleBar
        menus={menus}
        sidebarOpen={sidebarOpen}
        toggleSidebar={toggleSidebar}
        back={canBack ? () => go(-1) : null}
        forward={canForward ? () => go(1) : null}
      />
      <div className="flex min-h-0 flex-1">
        {sidebarOpen && (
          <Sidebar
            app={app}
            project={project}
            openSettings={() => setSettingsOpen(true)}
          />
        )}
        <main className="relative flex min-w-0 flex-1 flex-col">
          {app.error && (
            <div className="flex shrink-0 items-center gap-3 border-b border-line bg-danger-soft px-4 py-2 text-[13px] text-danger">
              <span className="flex-1 truncate">{app.error}</span>
              <button
                className="text-muted hover:text-fg"
                onClick={dismissError}
              >
                Dismiss
              </button>
            </div>
          )}
          {project ? (
            <SessionPane
              project={project}
              pendingTrust={app.trust[project.project_id] ?? []}
            />
          ) : (
            <Welcome recents={app.recents} />
          )}
        </main>
      </div>
      {app.providersOpen && <Providers />}
      {settingsOpen && (
        <Settings project={project} close={() => setSettingsOpen(false)} />
      )}
      {paletteOpen && (
        <CommandPalette
          project={project}
          close={() => setPaletteOpen(false)}
          settings={() => setSettingsOpen(true)}
        />
      )}
      {aboutOpen && <About close={() => setAboutOpen(false)} />}
      {worktreeProject && (
        <WorktreeDialog project={worktreeProject} close={hideWorktreeDialog} />
      )}
      <ToastView />
    </div>
  );
}

function About({ close }: { close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
    void getVersion()
      .then(setVersion)
      .catch(() => setVersion(null));
  }, []);
  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="about-title"
      className="m-auto w-[360px] rounded-2xl border border-line-strong bg-bg p-6 text-center text-fg shadow-2xl backdrop:bg-black/50"
    >
      <Logo className="mx-auto h-12 w-12" />
      <h2 id="about-title" className="mt-3 text-[16px] font-semibold">
        QQ
      </h2>
      <p className="mt-0.5 text-[12.5px] text-muted">
        {version ? `Version ${version}` : "Desktop app"}
      </p>
      <p className="mt-3 text-[12.5px] text-muted">
        AI coding agents on your machine. Open source under the MIT license.
      </p>
      <button
        autoFocus
        onClick={close}
        className="mt-5 h-9 rounded-lg border border-line-strong px-5 text-[13px] hover:bg-hover"
      >
        Close
      </button>
    </dialog>
  );
}

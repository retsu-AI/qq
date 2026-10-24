import { FolderOpen, FolderGit2, KeyRound } from "lucide-react";
import { openProject, pickProject, showProviders } from "../lib/store";
import type { ProjectInfo } from "../lib/types";
import { Logo } from "./Logo";

export function Welcome({ recents }: { recents: ProjectInfo[] }) {
  return (
    <div className="flex flex-1 items-center justify-center p-10">
      <div className="w-full max-w-md">
        <Logo className="mb-6 h-10 w-10" />
        <h1 className="text-[22px] font-semibold tracking-tight">Welcome to QQ</h1>
        <p className="mt-1 text-muted">
          Open a project folder to start working with an agent. Sessions are shared with the
          <code className="mx-1 rounded bg-raised px-1 font-mono text-[12px]">qq</code>
          terminal app.
        </p>
        <button
          onClick={() => void pickProject()}
          className="mt-6 flex w-full items-center gap-3 rounded-lg border border-line-strong bg-raised px-4 py-3 text-left hover:bg-hover"
        >
          <FolderOpen className="h-4 w-4 text-accent" />
          <span className="flex-1 font-medium">Open project…</span>
          <kbd className="font-mono text-[11px] text-faint">Ctrl O</kbd>
        </button>
        <button
          onClick={() => showProviders(true)}
          className="mt-2 flex w-full items-center gap-3 rounded-lg border border-line px-4 py-3 text-left hover:bg-hover"
        >
          <KeyRound className="h-4 w-4 text-muted" />
          <span className="flex-1">Model providers</span>
        </button>
        {recents.length > 0 && (
          <div className="mt-8">
            <div className="mb-2 text-[11px] font-semibold uppercase tracking-wider text-faint">
              Recent
            </div>
            <ul className="space-y-0.5">
              {recents.map((recent) => (
                <li key={recent.id}>
                  <button
                    onClick={() => void openProject(recent.path)}
                    className="flex w-full items-center gap-3 rounded-md px-2 py-1.5 text-left hover:bg-hover"
                  >
                    <FolderGit2 className="h-4 w-4 shrink-0 text-muted" />
                    <span className="font-medium">{recent.name}</span>
                    <span className="truncate text-[12px] text-faint">{recent.path}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  );
}

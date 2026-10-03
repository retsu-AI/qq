import { useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { FileCog, FolderSearch, Loader2, ShieldAlert } from "lucide-react";
import { closeProject, trustProject } from "../lib/store";
import type { PendingFile, ProjectState } from "../lib/types";

/** `file` inside `root` as a project-relative path; otherwise unchanged. */
function relativeTo(root: string, file: string): string {
  const prefix = root.endsWith("\\") ? root : `${root}\\`;
  return file.toLowerCase().startsWith(prefix.toLowerCase()) ? file.slice(prefix.length) : file;
}

/**
 * Shown instead of the session view while a project's QQ configuration
 * waits for trust. Until then the server ignores the project, so there is
 * nothing else to do here.
 */
export function TrustPrompt({ project, pending }: { project: ProjectState; pending: PendingFile[] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trust = async () => {
    setBusy(true);
    setError(null);
    try {
      await trustProject(project.project_id);
    } catch (failure) {
      setError(String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <div className="mx-auto w-full max-w-[680px] px-6 py-[10vh]">
        <ShieldAlert className="mb-4 h-7 w-7 text-warn" />
        <h1 className="text-[20px] font-semibold tracking-tight">
          Trust the QQ settings in {project.name}?
        </h1>
        <p className="mt-2 text-muted">
          This project ships QQ configuration that can choose models, add providers and MCP
          servers, and pre-approve commands. QQ ignores it until you trust it. Review what it
          declares first, especially commands and servers it would start.
        </p>

        <div className="mt-6 space-y-3">
          {pending.map((file) => (
            <section key={file.path} className="overflow-hidden rounded-xl border border-line-strong bg-raised">
              <header className="flex items-center gap-2 border-b border-line px-4 py-2.5">
                <FileCog className="h-4 w-4 shrink-0 text-muted" />
                <span className="min-w-0 flex-1 truncate font-mono text-[12.5px] selectable" title={file.path}>
                  {relativeTo(project.path, file.path)}
                </span>
                <button
                  onClick={() => void revealItemInDir(file.path)}
                  className="flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-[12px] text-muted hover:bg-hover hover:text-fg"
                >
                  <FolderSearch className="h-3.5 w-3.5" /> Show in Explorer
                </button>
              </header>
              <ul className="space-y-1 px-4 py-3">
                {file.declarations.map((line, index) => (
                  <li key={index} className="flex gap-2 font-mono text-[12.5px] selectable">
                    <span className="text-faint">•</span>
                    <span className="min-w-0 break-words">{line}</span>
                  </li>
                ))}
                {file.declarations.length === 0 && (
                  <li className="text-[12.5px] text-muted">Sets {file.sections.join(", ")}</li>
                )}
              </ul>
            </section>
          ))}
        </div>

        {error && (
          <p className="mt-4 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable">{error}</p>
        )}

        <div className="mt-6 flex items-center gap-2">
          <button
            onClick={() => void trust()}
            disabled={busy}
            className="flex h-9 items-center gap-2 rounded-lg bg-solid px-4 text-[13px] font-medium text-white hover:bg-solid-hover disabled:opacity-60"
          >
            {busy && <Loader2 className="h-4 w-4 animate-spin" />}
            Trust and continue
          </button>
          <button
            onClick={() => void closeProject(project.project_id)}
            disabled={busy}
            className="h-9 rounded-lg border border-line-strong px-4 text-[13px] text-muted hover:bg-hover hover:text-fg"
          >
            Close project
          </button>
        </div>
        <p className="mt-3 text-[12px] text-faint">
          Trust is remembered until a file changes, then QQ asks again. This is the same as running
          <code className="mx-1 font-mono">qq trust</code>in the project folder.
        </p>
      </div>
    </div>
  );
}

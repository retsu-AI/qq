import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { pickProject, showProviders } from "../lib/store";
import type { ProjectState } from "../lib/types";

export function CommandPalette({
  project,
  close,
  settings,
}: {
  project: ProjectState | null;
  close: () => void;
  settings: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [query, setQuery] = useState("");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const session = project?.focused ?? null;
  const idle = project?.transcript?.status === "idle";
  const commands = [
    { name: "Open project", enabled: true, run: pickProject },
    { name: "Settings and diagnostics", enabled: true, run: settings },
    {
      name: "Connect model provider",
      enabled: true,
      run: () => showProviders(true),
    },
    {
      name: "New session",
      enabled: !!project,
      run: () => project && api.newSession(project.project_id),
    },
    {
      name: "New child session",
      enabled: !!session,
      run: () =>
        project && api.sessionAction(project.project_id, session, "child"),
    },
    {
      name: "Compact session context",
      enabled: !!session && idle,
      run: () =>
        project && api.sessionAction(project.project_id, session, "compact"),
    },
    {
      name: "Rollback latest compaction",
      enabled: !!session && idle,
      run: () =>
        project && api.sessionAction(project.project_id, session, "rollback"),
    },
    {
      name: "Prune empty idle sessions",
      enabled: !!project,
      run: () =>
        project && api.sessionAction(project.project_id, null, "prune"),
    },
  ].filter((command) =>
    command.name.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-label="Command palette"
      className="m-auto w-[520px] max-w-[90vw] rounded-xl border border-line-strong bg-bg p-4 text-fg backdrop:bg-black/50"
    >
      <div className="flex gap-2">
        <input
          autoFocus
          aria-label="Find a command"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          className="min-w-0 flex-1 rounded border border-line bg-raised p-2"
          placeholder="Find a command…"
        />
        <button onClick={close}>Close</button>
      </div>
      {error && (
        <p role="alert" className="text-danger">
          {error}
        </p>
      )}
      <ul className="mt-3">
        {commands.map((command) => (
          <li key={command.name}>
            <button
              disabled={!command.enabled}
              className="w-full rounded px-3 py-2 text-left hover:bg-hover disabled:opacity-40"
              onClick={async () => {
                try {
                  await command.run();
                  close();
                } catch (error) {
                  setError(String(error));
                }
              }}
            >
              {command.name}
            </button>
          </li>
        ))}
      </ul>
      <p className="mt-3 text-[12px] text-muted">
        Use Tab and Enter to choose a command. Escape closes. Compaction affects
        model context; the transcript stays available.
      </p>
    </dialog>
  );
}

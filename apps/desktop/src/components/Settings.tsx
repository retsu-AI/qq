import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import {
  applyPreferences,
  parsePreferences,
  type Preferences,
} from "../lib/preferences";
import type { ProjectState } from "../lib/types";

export function Settings({
  project,
  close,
}: {
  project: ProjectState | null;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [preferences, setPreferences] = useState(() =>
    parsePreferences(localStorage.getItem("qq.preferences")),
  );
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const update = async (next: Preferences) => {
    try {
      await api.setNotificationsEnabled(next.notifications);
      localStorage.setItem("qq.preferences", JSON.stringify(next));
      applyPreferences(next);
      setPreferences(next);
      setError(null);
    } catch (error) {
      setError(String(error));
    }
  };
  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="settings-title"
      className="m-auto max-h-[85vh] w-[620px] max-w-[90vw] overflow-auto rounded-xl border border-line-strong bg-bg p-6 text-fg shadow-xl backdrop:bg-black/50"
    >
      <div className="flex items-center justify-between">
        <h2 id="settings-title" className="text-lg font-semibold">
          Settings and diagnostics
        </h2>
        <button
          autoFocus
          onClick={close}
          className="rounded border border-line px-3 py-1"
        >
          Close
        </button>
      </div>
      {error && (
        <p role="alert" className="mt-3 text-danger">
          {error}
        </p>
      )}
      <div className="my-5 grid gap-4">
        <label>
          Theme{" "}
          <select
            aria-label="Theme"
            value={preferences.theme}
            onChange={(event) =>
              void update({
                ...preferences,
                theme: event.target.value as Preferences["theme"],
              })
            }
            className="ml-3 rounded bg-raised p-1"
          >
            <option value="system">System</option>
            <option value="light">Light</option>
            <option value="dark">Dark</option>
          </select>
        </label>
        <label>
          Transcript and composer text size{" "}
          <input
            aria-label="Transcript and composer text size"
            type="number"
            min={12}
            max={22}
            value={preferences.fontSize}
            onChange={(event) => {
              const fontSize = Number(event.target.value);
              if (
                Number.isInteger(fontSize) &&
                fontSize >= 12 &&
                fontSize <= 22
              )
                void update({ ...preferences, fontSize });
            }}
            className="ml-3 w-16 rounded bg-raised p-1"
          />
        </label>
        <label>
          <input
            type="checkbox"
            checked={preferences.notifications}
            onChange={(event) =>
              void update({
                ...preferences,
                notifications: event.target.checked,
              })
            }
          />{" "}
          Notify when QQ is unfocused and a run finishes or needs approval
        </label>
        <p className="text-[12px] text-muted">
          Windows notifications require an installed build. Notification text
          includes the session title.
        </p>
      </div>
      <h3 className="font-semibold">Keyboard shortcuts</h3>
      <dl className="my-3 grid grid-cols-2 gap-1 text-[13px]">
        <dt>New session</dt>
        <dd>Ctrl N</dd>
        <dt>Open project</dt>
        <dd>Ctrl O</dd>
        <dt>Next needs-you session</dt>
        <dd>Ctrl Shift A</dd>
        <dt>Command palette</dt>
        <dd>Ctrl K</dd>
        <dt>Settings</dt>
        <dd>Ctrl ,</dd>
        <dt>Send / newline</dt>
        <dd>Enter / Shift Enter</dd>
        <dt>Stop run</dt>
        <dd>Esc in composer</dd>
        <dt>Approval</dt>
        <dd>Y / S / W / N</dd>
      </dl>
      <h3 className="mt-5 font-semibold">Current project</h3>
      <p className="my-2 break-all text-[13px] selectable">
        {project?.path ?? "No project open"}
        <br />
        Connection: {project?.connection ?? "none"}
        <br />
        Engine: {project?.owns_server ? "supervised" : "shared or unavailable"}
      </p>
      {project?.error && (
        <p role="alert" className="text-danger">
          {project.error}
        </p>
      )}
      <h3 className="mt-5 font-semibold">MCP / external tools</h3>
      {project?.tool_hosts?.length ? (
        <ul className="my-2 space-y-2">
          {project.tool_hosts.map((host) => (
            <li key={host.name} className="rounded bg-raised p-2 text-[13px]">
              <strong>{host.name}</strong>:{" "}
              {host.ready ? "Ready" : "Unavailable"} · {host.tool_count} tools
              {host.message && <p>{host.message}</p>}
            </li>
          ))}
        </ul>
      ) : (
        <p className="my-2 text-[13px] text-muted">
          No external tool hosts reported. Status becomes available after the
          workspace capabilities load.
        </p>
      )}
      <p className="text-[12px] text-muted">
        Configure MCP and runtime defaults in your QQ config.ron. Trust changed
        project configuration before using it. Settings here apply to this
        desktop only.
      </p>
    </dialog>
  );
}

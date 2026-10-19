import { useEffect, useState } from "react";
import clsx from "clsx";
import { api } from "../lib/api";
import { createWorktreeSession } from "../lib/store";
import { toast } from "../lib/toast";
import { branchFromTask } from "../lib/worktree";
import type { ProjectInfo } from "../lib/types";
import { FormDialog, fieldClass } from "./FormDialog";

/**
 * Starts a session in a new Git worktree: its own folder and branch, so the
 * agent's edits stay apart from the main checkout and from other sessions.
 */
export function WorktreeDialog({
  project,
  close,
}: {
  project: ProjectInfo;
  close: () => void;
}) {
  const [task, setTask] = useState("");
  const [branch, setBranch] = useState("");
  const [edited, setEdited] = useState(false);
  const [base, setBase] = useState("");
  const [current, setCurrent] = useState<string | null>(null);
  const suggested = branchFromTask(task);
  const name = edited ? branch : suggested;
  const repo = project.worktree?.main_name ?? project.name;

  useEffect(() => {
    api
      .gitStatus(project.id, false)
      .then((status) => setCurrent(status.repo ? (status.branch ?? "HEAD") : null))
      .catch(() => setCurrent(null));
  }, [project.id]);

  return (
    <FormDialog
      title="New worktree session"
      submitLabel="Create worktree"
      close={close}
      submit={async () => {
        await createWorktreeSession(project.id, name, base.trim() || null, task);
        toast(`Created ${name} in ${repo}.worktrees`);
      }}
    >
      <p className="text-muted">
        Creates a separate checkout of <span className="text-fg">{repo}</span>{" "}
        on its own branch, in{" "}
        <code className="font-mono text-fg">{repo}.worktrees</code> beside the
        repository, and opens it as a project. Agents there cannot touch your
        main checkout or other sessions' files.
      </p>
      <label className="block">
        <span className="mb-1 block text-[12px] text-muted">Task (optional)</span>
        <textarea
          autoFocus
          rows={3}
          value={task}
          onChange={(event) => setTask(event.target.value)}
          placeholder="What should the agent do there? It waits in the new project's composer."
          className={clsx(fieldClass, "resize-y")}
        />
      </label>
      <div className="grid grid-cols-[1fr_auto] gap-3">
        <label className="block">
          <span className="mb-1 block text-[12px] text-muted">Branch</span>
          <input
            required
            value={name}
            onChange={(event) => {
              setEdited(true);
              setBranch(event.target.value);
            }}
            className={clsx(fieldClass, "font-mono")}
          />
        </label>
        <label className="block w-[170px]">
          <span className="mb-1 block text-[12px] text-muted">Starting from</span>
          <input
            value={base}
            onChange={(event) => setBase(event.target.value)}
            placeholder={current ?? "HEAD"}
            className={clsx(fieldClass, "font-mono")}
          />
        </label>
      </div>
      <p className="text-[12px] text-faint">
        An existing branch is checked out as it is. Uncommitted changes in the
        current checkout are not copied. The project's QQ configuration may ask
        for trust again in the new folder.
      </p>
    </FormDialog>
  );
}

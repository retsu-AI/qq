import { useState } from "react";
import clsx from "clsx";
import { ChevronRight, Code2, FileCode2, X } from "lucide-react";
import { openInEditor } from "../lib/editor";
import type { FileChange } from "../lib/changes";
import { CopyButton } from "./CodeBlock";
import { Diff } from "./Diff";
import { platform } from "../lib/platform";
import { Panel } from "./Panel";

/** The files this session's edits changed, each with its diffs. */
export function ChangesPanel({
  projectId,
  changes,
  close,
}: {
  projectId: string;
  changes: FileChange[];
  close: () => void;
}) {
  const [open, setOpen] = useState<Set<string>>(
    () => new Set(changes.length === 1 ? [changes[0].path] : []),
  );
  const added = changes.reduce((total, file) => total + file.added, 0);
  const removed = changes.reduce((total, file) => total + file.removed, 0);
  const allOpen = changes.length > 0 && changes.every((f) => open.has(f.path));
  const toggle = (path: string) =>
    setOpen((current) => {
      const next = new Set(current);
      if (!next.delete(path)) next.add(path);
      return next;
    });

  return (
    <Panel
      label="Changes"
      close={close}
      // Beside the conversation on wide windows, over it on narrow ones.
      className="animate-slide-in flex w-[min(460px,42vw)] min-w-[300px] shrink-0 flex-col border-l border-line bg-sidebar max-[1100px]:absolute max-[1100px]:inset-y-0 max-[1100px]:right-0 max-[1100px]:z-20 max-[1100px]:w-[min(460px,85%)] max-[1100px]:shadow-2xl"
    >
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line pl-4 pr-2">
        <h2 className="text-[13.5px] font-medium">Changes</h2>
        {changes.length > 0 && (
          <span className="text-[12px] text-faint">
            {changes.length} file{changes.length === 1 ? "" : "s"}{" "}
            <span className="font-mono text-ok">+{added}</span>{" "}
            <span className="font-mono text-danger">−{removed}</span>
          </span>
        )}
        {changes.length > 1 && (
          <button
            onClick={() =>
              setOpen(new Set(allOpen ? [] : changes.map((f) => f.path)))
            }
            className="ml-auto rounded-md px-2 py-1 text-[12px] text-muted hover:bg-hover hover:text-fg"
          >
            {allOpen ? "Collapse all" : "Expand all"}
          </button>
        )}
        <button
          onClick={close}
          aria-label="Close changes"
          title="Close (Ctrl Shift D)"
          className={clsx(
            "rounded-md p-1.5 text-muted hover:bg-hover hover:text-fg",
            changes.length <= 1 && "ml-auto",
          )}
        >
          <X className="h-4 w-4" />
        </button>
      </header>

      {changes.length === 0 ? (
        <div className="px-5 py-8 text-center text-[12.5px] text-muted">
          <FileCode2 className="mx-auto mb-2 h-5 w-5 text-faint" />
          No file edits in this session yet.
          <p className="mt-1 text-faint">
            Edits made with QQ's file tools appear here. Files changed by shell
            commands are not tracked.
          </p>
        </div>
      ) : (
        <ul className="min-h-0 flex-1 overflow-y-auto p-2">
          {changes.map((file) => {
            const expanded = open.has(file.path);
            const slash = Math.max(
              file.path.lastIndexOf("/"),
              file.path.lastIndexOf("\\"),
            );
            const name = file.path.slice(slash + 1);
            const folder = slash > 0 ? file.path.slice(0, slash) : "";
            return (
              <li key={file.path} className="group/file">
                <div className="flex items-center rounded-md hover:bg-hover">
                  <button
                    onClick={() => toggle(file.path)}
                    aria-expanded={expanded}
                    title={file.path}
                    className="flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-left text-[13px]"
                  >
                    <ChevronRight
                      className={clsx(
                        "h-3.5 w-3.5 shrink-0 text-faint transition-transform",
                        expanded && "rotate-90",
                      )}
                    />
                    <span className="shrink-0 font-medium">{name}</span>
                    <span className="min-w-0 truncate text-[12px] text-faint">
                      {folder}
                    </span>
                    <span className="ml-auto shrink-0 pl-2 font-mono text-[11.5px] text-ok">
                      +{file.added}
                    </span>
                    <span className="shrink-0 font-mono text-[11.5px] text-danger">
                      −{file.removed}
                    </span>
                  </button>
                  {platform.features.editor && <button
                    type="button"
                    onClick={() => void openInEditor(projectId, file.path)}
                    aria-label={`Open ${name} in editor`}
                    title="Open in editor"
                    className="flex h-6 items-center rounded-md px-1.5 text-muted opacity-0 hover:bg-active hover:text-fg focus-visible:opacity-100 group-hover/file:opacity-100"
                  >
                    <Code2 className="h-3.5 w-3.5" />
                  </button>}
                  <CopyButton
                    text={file.path}
                    label="Copy path"
                    className="mr-1 opacity-0 focus-visible:opacity-100 group-hover/file:opacity-100"
                  />
                </div>
                {expanded && (
                  <div className="mb-2 ml-2 mt-1 space-y-2">
                    {file.diffs.map((diff, index) => (
                      <Diff
                        key={index}
                        path={file.path}
                        diff={diff}
                        header={false}
                        maxHeight={null}
                      />
                    ))}
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

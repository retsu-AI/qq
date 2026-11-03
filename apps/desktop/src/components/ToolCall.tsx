import { useState } from "react";
import clsx from "clsx";
import {
  Ban,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleX,
  ExternalLink,
  Loader2,
} from "lucide-react";
import { api } from "../lib/api";
import { duration, isTerminal } from "../lib/format";
import { diffStats } from "../lib/changes";
import { isShell, summarizeTools, toolSubject, toolVerb } from "../lib/tools";
import type { Tool } from "../lib/types";
import { ApprovalCard } from "./ApprovalCard";
import { Diff } from "./Diff";
import { ToolOutput } from "./ToolOutput";

/**
 * A run of consecutive tool calls. Settled calls fold behind one summary
 * line ("Edited 2 files, ran 3 commands"), which opens to a card of one row
 * per call; each row opens to its command, diff or output. Running calls and
 * held approvals stay visible.
 */
export function ToolGroup({
  tools,
  projectId,
  sessionId,
}: {
  tools: Tool[];
  projectId: string;
  sessionId: string;
}) {
  const folded = tools.filter(
    (tool) => isTerminal(tool.state) && !tool.approval,
  );
  const foldable = folded.length > 1;
  const [expanded, setExpanded] = useState(false);
  const failed = folded.filter(
    (tool) => tool.is_error || tool.state === "failed",
  ).length;
  const elapsed = folded.reduce(
    (total, tool) => total + (tool.duration_ms ?? 0),
    0,
  );
  const edits = folded.reduce(
    (total, tool) => {
      if (!tool.diff) return total;
      const stats = diffStats(tool.diff.diff);
      return {
        added: total.added + stats.added,
        removed: total.removed + stats.removed,
      };
    },
    { added: 0, removed: 0 },
  );
  const rest = tools.filter((tool) => !foldable || !folded.includes(tool));

  return (
    <div className="space-y-1">
      {foldable && (
        <div>
          <button
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
            className="flex max-w-full items-center gap-2 rounded-md py-0.5 text-left text-ui text-muted hover:text-fg"
          >
            {failed > 0 ? (
              <CircleX className="h-3.5 w-3.5 shrink-0 text-danger" />
            ) : (
              <CircleCheck className="h-3.5 w-3.5 shrink-0 text-faint" />
            )}
            <span className="truncate">
              {summarizeTools(folded.map((tool) => tool.name))}
            </span>
            {(edits.added > 0 || edits.removed > 0) && (
              <LineCounts {...edits} />
            )}
            {failed > 0 && (
              <span className="shrink-0 text-small text-danger">
                · {failed} failed
              </span>
            )}
            {elapsed > 0 && (
              <span className="shrink-0 text-small text-faint">
                · {duration(elapsed)}
              </span>
            )}
            <ChevronRight
              className={clsx(
                "h-3.5 w-3.5 shrink-0 text-faint transition-transform",
                expanded && "rotate-90",
              )}
            />
          </button>
          {expanded && (
            <div className="mt-1.5 divide-y divide-line overflow-hidden rounded-xl border border-line">
              {folded.map((tool) => (
                <div key={tool.id} className="px-3 py-1.5">
                  <ToolRow tool={tool} projectId={projectId} />
                </div>
              ))}
            </div>
          )}
        </div>
      )}
      {rest.map((tool) =>
        tool.approval ? (
          <ApprovalCard
            key={tool.id}
            tool={tool}
            projectId={projectId}
            sessionId={sessionId}
          />
        ) : (
          <ToolRow key={tool.id} tool={tool} projectId={projectId} />
        ),
      )}
    </div>
  );
}

function LineCounts({ added, removed }: { added: number; removed: number }) {
  return (
    <span className="shrink-0 font-mono text-small">
      <span className="text-ok">+{added}</span>{" "}
      <span className="text-danger">−{removed}</span>
    </span>
  );
}

function ToolRow({ tool, projectId }: { tool: Tool; projectId: string }) {
  const [open, setOpen] = useState(false);
  const subject = toolSubject(tool.name, tool.arguments);
  const running = tool.state === "running" || tool.state === "requested";
  const output = tool.result ?? tool.live_output;
  const shell = isShell(tool.name);
  const expandable = Boolean(output || tool.diff || (shell && subject));
  const stats = tool.diff ? diffStats(tool.diff.diff) : null;

  return (
    <div className="text-ui">
      <div className="flex min-w-0 items-center gap-2">
        <button
          onClick={() => expandable && setOpen((v) => !v)}
          aria-expanded={expandable ? open : undefined}
          className={clsx(
            "flex min-w-0 flex-1 items-center gap-2 rounded-md py-0.5 text-left",
            expandable && "hover:text-fg",
          )}
        >
          <StateIcon tool={tool} />
          <span className="shrink-0 font-medium text-fg/90">{toolVerb(tool.name)}</span>
          <span className="min-w-0 truncate font-mono text-small text-muted">{subject}</span>
          {stats && <LineCounts {...stats} />}
          {tool.settled && <span className="shrink-0 text-small text-faint">· {tool.settled}</span>}
          {expandable && (
            <ChevronRight
              className={clsx("h-3.5 w-3.5 shrink-0 text-faint transition-transform", open && "rotate-90")}
            />
          )}
        </button>
        {tool.child_session && (
          <button
            onClick={() => void api.focusSession(projectId, tool.child_session!)}
            className="flex shrink-0 items-center gap-1 text-small text-accent hover:underline"
          >
            Open <ExternalLink className="h-3 w-3" />
          </button>
        )}
        <span className="shrink-0 text-small text-faint">{duration(tool.duration_ms)}</span>
      </div>

      {running && tool.live_output && !open && (
        <pre className="ml-6 mt-1 max-h-28 overflow-hidden whitespace-pre-wrap rounded-md bg-raised px-3 py-2 font-mono text-small leading-[1.5] text-muted [mask-image:linear-gradient(to_bottom,transparent,black_40%)]">
          {tailLines(tool.live_output, 6)}
        </pre>
      )}

      {open && (
        <div className="mb-1 ml-6 mt-1.5 space-y-2">
          {tool.diff && <Diff path={tool.diff.path} diff={tool.diff.diff} />}
          {shell ? (
            <ToolOutput text={output} error={tool.is_error} command={subject} />
          ) : (
            // An edit's own output only repeats its diff unless it failed.
            output &&
            (!tool.diff || tool.is_error) && (
              <ToolOutput text={output} error={tool.is_error} />
            )
          )}
        </div>
      )}
    </div>
  );
}

function StateIcon({ tool }: { tool: Tool }) {
  const cls = "h-3.5 w-3.5 shrink-0";
  switch (tool.state) {
    case "requested":
    case "running":
      return <Loader2 className={clsx(cls, "animate-spin text-accent")} />;
    case "awaiting_approval":
      return <CircleAlert className={clsx(cls, "text-warn")} />;
    case "completed":
      return tool.is_error ? (
        <CircleX className={clsx(cls, "text-danger")} />
      ) : (
        <CircleCheck className={clsx(cls, "text-faint")} />
      );
    case "failed":
      return <CircleX className={clsx(cls, "text-danger")} />;
    case "denied":
    case "interrupted":
      return <Ban className={clsx(cls, "text-faint")} />;
  }
}

function tailLines(text: string, count: number): string {
  const lines = text.replace(/\s+$/, "").split("\n");
  return lines.slice(-count).join("\n");
}

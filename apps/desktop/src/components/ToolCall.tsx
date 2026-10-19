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
import { summarizeTools, toolSubject, toolVerb } from "../lib/tools";
import type { Tool } from "../lib/types";
import { ApprovalCard } from "./ApprovalCard";
import { Diff } from "./Diff";

/**
 * A run of consecutive tool calls. Settled calls without a diff fold behind
 * one summary line ("Ran 3 commands, read 2 files"); running calls, held
 * approvals and edits with a diff stay visible.
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
    (tool) => isTerminal(tool.state) && !tool.diff && !tool.approval,
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
  const hidden = new Set(
    foldable && !expanded ? folded.map((tool) => tool.id) : [],
  );

  return (
    <div className="space-y-1">
      {foldable && (
        <button
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          className="group/summary flex max-w-full items-center gap-2 rounded-md py-0.5 text-left text-[13px] text-muted hover:text-fg"
        >
          {failed > 0 ? (
            <CircleX className="h-3.5 w-3.5 shrink-0 text-danger" />
          ) : (
            <CircleCheck className="h-3.5 w-3.5 shrink-0 text-faint" />
          )}
          <span className="truncate">
            {summarizeTools(folded.map((tool) => tool.name))}
          </span>
          {failed > 0 && (
            <span className="shrink-0 text-[11.5px] text-danger">
              · {failed} failed
            </span>
          )}
          {elapsed > 0 && (
            <span className="shrink-0 text-[11.5px] text-faint">
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
      )}
      {tools
        .filter((tool) => !hidden.has(tool.id))
        .map((tool) =>
          tool.approval ? (
            <ApprovalCard
              key={tool.id}
              tool={tool}
              projectId={projectId}
              sessionId={sessionId}
            />
          ) : (
            <div
              key={tool.id}
              className={clsx(foldable && folded.includes(tool) && "pl-5")}
            >
              <ToolRow tool={tool} projectId={projectId} />
            </div>
          ),
        )}
    </div>
  );
}

function ToolRow({ tool, projectId }: { tool: Tool; projectId: string }) {
  const [open, setOpen] = useState(false);
  const subject = toolSubject(tool.name, tool.arguments);
  const running = tool.state === "running" || tool.state === "requested";
  const output = tool.result ?? tool.live_output;
  const expandable = Boolean(output || tool.diff);

  return (
    <div className="text-[13px]">
      <div className="flex min-w-0 items-center gap-2">
        <button
          onClick={() => expandable && setOpen((v) => !v)}
          className={clsx(
            "flex min-w-0 flex-1 items-center gap-2 rounded-md py-0.5 text-left",
            expandable && "hover:text-fg",
          )}
        >
          <StateIcon tool={tool} />
          <span className="shrink-0 font-medium text-fg/90">{toolVerb(tool.name)}</span>
          <span className="min-w-0 truncate font-mono text-[12px] text-muted">{subject}</span>
          {tool.settled && <span className="shrink-0 text-[11.5px] text-faint">· {tool.settled}</span>}
          {expandable && (
            <ChevronRight
              className={clsx("h-3.5 w-3.5 shrink-0 text-faint transition-transform", open && "rotate-90")}
            />
          )}
        </button>
        {tool.child_session && (
          <button
            onClick={() => void api.focusSession(projectId, tool.child_session!)}
            className="flex shrink-0 items-center gap-1 text-[12px] text-accent hover:underline"
          >
            Open <ExternalLink className="h-3 w-3" />
          </button>
        )}
        <span className="shrink-0 text-[11.5px] text-faint">{duration(tool.duration_ms)}</span>
      </div>

      {running && tool.live_output && !open && (
        <pre className="ml-6 mt-1 max-h-28 overflow-hidden whitespace-pre-wrap rounded-md bg-raised px-3 py-2 font-mono text-[11.5px] leading-[1.5] text-muted [mask-image:linear-gradient(to_bottom,transparent,black_40%)]">
          {tailLines(tool.live_output, 6)}
        </pre>
      )}

      {tool.diff && !open && tool.state === "completed" && (
        <div className="ml-6 mt-1.5">
          <Diff path={tool.diff.path} diff={tool.diff.diff} maxHeight={220} />
        </div>
      )}

      {open && (
        <div className="ml-6 mt-1.5 space-y-2">
          {tool.diff && <Diff path={tool.diff.path} diff={tool.diff.diff} />}
          {output && (
            <pre
              className={clsx(
                "max-h-[360px] overflow-auto whitespace-pre-wrap rounded-md border border-line bg-raised px-3 py-2 font-mono text-[11.5px] leading-[1.5] selectable",
                tool.is_error ? "text-danger" : "text-muted",
              )}
            >
              {output}
            </pre>
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

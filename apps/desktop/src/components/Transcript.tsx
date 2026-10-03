import { memo, useState } from "react";
import clsx from "clsx";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Brain, ChevronRight, CircleCheck, CircleX, CornerDownRight, OctagonX } from "lucide-react";
import { cost, duration, outcomeDetail, outcomeLabel, tokens } from "../lib/format";
import type { Item, ProjectState, Transcript as TranscriptData } from "../lib/types";
import { ToolGroup } from "./ToolCall";

type ToolItem = Extract<Item, { kind: "tool" }>;
type Block = { kind: "item"; item: Item } | { kind: "tools"; id: string; tools: ToolItem[] };

/** Consecutive tool calls render as one group. */
function blocks(items: Item[]): Block[] {
  const out: Block[] = [];
  for (const item of items) {
    const last = out[out.length - 1];
    if (item.kind === "tool") {
      if (last?.kind === "tools") last.tools.push(item);
      else out.push({ kind: "tools", id: `tools-${item.id}`, tools: [item] });
    } else {
      out.push({ kind: "item", item });
    }
  }
  return out;
}

export function Transcript({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: TranscriptData;
}) {
  if (transcript.items.length === 0) {
    return <p className="pt-8 text-center text-muted">Send a message to start.</p>;
  }
  return (
    <div className="space-y-5">
      {blocks(transcript.items).map((block) =>
        block.kind === "tools" ? (
          <ToolGroup
            key={block.id}
            tools={block.tools}
            projectId={project.project_id}
            sessionId={transcript.session_id}
          />
        ) : (
          <ItemView key={block.item.id} item={block.item} />
        ),
      )}
    </div>
  );
}

function ItemView({ item }: { item: Item }) {
  switch (item.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div
            className={clsx(
              "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-md bg-active px-4 py-2.5 selectable",
              item.state === "queued" && "opacity-60",
            )}
          >
            {item.steering && (
              <div className="mb-1 flex items-center gap-1 text-[11px] text-muted">
                <CornerDownRight className="h-3 w-3" /> steering
              </div>
            )}
            {item.text}
            {item.state === "queued" && (
              <div className="mt-1 text-[11px] text-muted">Queued</div>
            )}
          </div>
        </div>
      );
    case "assistant":
      return (
        <div>
          <Markdown text={item.text} streaming={item.state === "streaming"} />
          {item.refusal && (
            <p className="mt-2 rounded-md bg-warn-soft px-3 py-2 text-[13px] text-warn">
              {item.refusal}
            </p>
          )}
          {item.truncated && (
            <p className="mt-1 text-[12px] text-faint">Output reached the model's length limit.</p>
          )}
        </div>
      );
    case "reasoning":
      return <Reasoning text={item.text} streaming={item.streaming} />;
    case "run_end":
      return <RunEnd item={item} />;
    case "tool":
      return null;
  }
}

const Markdown = memo(function Markdown({ text, streaming }: { text: string; streaming: boolean }) {
  if (!text && streaming) {
    return <span className="streaming-caret text-muted" />;
  }
  return (
    <div className={clsx("prose-qq", streaming && "is-streaming")}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children }) => (
            <a
              href={href}
              onClick={(event) => {
                event.preventDefault();
                if (href && /^https?:/.test(href)) void openUrl(href);
              }}
            >
              {children}
            </a>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});

function Reasoning({ text, streaming }: { text: string; streaming: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="text-[13px]">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 text-muted hover:text-fg"
      >
        <Brain className={clsx("h-3.5 w-3.5", streaming && "animate-qq-pulse text-accent")} />
        {streaming ? "Thinking…" : "Thought process"}
        <ChevronRight className={clsx("h-3.5 w-3.5 transition-transform", open && "rotate-90")} />
      </button>
      {(open || streaming) && (
        <div
          className={clsx(
            "mt-2 whitespace-pre-wrap border-l-2 border-line-strong pl-3 text-muted selectable",
            !open && "line-clamp-3",
          )}
        >
          {text}
        </div>
      )}
    </div>
  );
}

function RunEnd({ item }: { item: Extract<Item, { kind: "run_end" }> }) {
  const label = outcomeLabel(item.outcome);
  const detail = outcomeDetail(item.outcome);
  const ok = item.outcome?.type === "completed";
  const failed = item.outcome?.type === "failed" || item.outcome?.type === "budget_exhausted";
  const parts = [
    duration(item.duration_ms),
    item.tool_calls > 0 ? `${item.tool_calls} tool call${item.tool_calls === 1 ? "" : "s"}` : "",
    item.usage ? `${tokens(item.usage.input_tokens + item.usage.cache_read_input_tokens)} in · ${tokens(item.usage.output_tokens)} out` : "",
    cost(item.cost_usd_nanos),
  ].filter(Boolean);

  return (
    <div>
      <div className="flex items-center gap-2 text-[12px] text-faint">
        {ok ? (
          <CircleCheck className="h-3.5 w-3.5 text-ok" />
        ) : failed ? (
          <CircleX className="h-3.5 w-3.5 text-danger" />
        ) : (
          <OctagonX className="h-3.5 w-3.5 text-muted" />
        )}
        <span className={clsx(failed && "text-danger")}>{label}</span>
        {parts.length > 0 && <span>· {parts.join(" · ")}</span>}
      </div>
      {detail && (
        <p className="mt-1.5 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable">
          {detail}
        </p>
      )}
    </div>
  );
}

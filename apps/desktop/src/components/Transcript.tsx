import {
  isValidElement,
  memo,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { defaultRangeExtractor, useVirtualizer } from "@tanstack/react-virtual";
import clsx from "clsx";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  Brain,
  ChevronRight,
  CircleCheck,
  CircleX,
  CornerDownRight,
  OctagonX,
  Pencil,
  RotateCcw,
} from "lucide-react";
import { api } from "../lib/api";
import { drafts } from "../lib/drafts";
import { languageOf } from "../lib/highlight";
import {
  cost,
  duration,
  outcomeDetail,
  outcomeLabel,
  tokens,
} from "../lib/format";
import type {
  Item,
  ProjectState,
  Transcript as TranscriptData,
} from "../lib/types";
import { CodeBlock, CopyButton } from "./CodeBlock";
import { ToolGroup } from "./ToolCall";

type ToolItem = Extract<Item, { kind: "tool" }>;
type Block =
  | {
      kind: "item";
      item: Item;
      /** The latest user message before this item, for Retry. */
      prompt: string | null;
    }
  | { kind: "tools"; id: string; tools: ToolItem[] };

/** Consecutive tool calls render as one group. */
function blocks(items: Item[]): Block[] {
  const out: Block[] = [];
  let prompt: string | null = null;
  for (const item of items) {
    const last = out[out.length - 1];
    if (item.kind === "user") prompt = item.text;
    if (item.kind === "tool") {
      if (last?.kind === "tools" && last.tools.length < 32)
        last.tools.push(item);
      else out.push({ kind: "tools", id: `tools-${item.id}`, tools: [item] });
    } else {
      out.push({ kind: "item", item, prompt });
    }
  }
  return out;
}

/** What a message's hover actions need from the session. */
interface Actions {
  projectId: string;
  sessionId: string;
  /** Sending is possible now: live and no run in progress. */
  canSend: boolean;
}

export function Transcript({
  project,
  transcript,
  scrollElement,
}: {
  project: ProjectState;
  transcript: TranscriptData;
  scrollElement: HTMLDivElement | null;
}) {
  const rows = useMemo(() => blocks(transcript.items), [transcript.items]);
  const actions: Actions = {
    projectId: project.project_id,
    sessionId: transcript.session_id,
    canSend: project.connection === "live" && transcript.status === "idle",
  };
  const [focusedKey, setFocusedKey] = useState<string | number | null>(null);
  const keyFor = (index: number) => rows[index].kind === "tools" ? rows[index].id : rows[index].item.id;
  const focusedRow = focusedKey === null ? -1 : rows.findIndex((_, index) => keyFor(index) === focusedKey);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollElement,
    estimateSize: () => 180,
    overscan: 6,
    getItemKey: keyFor,
    rangeExtractor: (range) => {
      const visible = defaultRangeExtractor(range);
      return focusedRow >= 0 &&
        focusedRow < rows.length &&
        !visible.includes(focusedRow)
        ? [...visible, focusedRow].sort((a, b) => a - b)
        : visible;
    },
  });
  if (transcript.items.length === 0) {
    return (
      <p className="pt-8 text-center text-muted">Send a message to start.</p>
    );
  }
  return (
    <div
      role="list"
      aria-label="Transcript"
      className="relative w-full"
      style={{ height: virtualizer.getTotalSize() }}
    >
      {virtualizer.getVirtualItems().map((row) => {
        const block = rows[row.index];
        return (
          <div
            key={row.key}
            ref={virtualizer.measureElement}
            data-index={row.index}
            role="listitem"
            aria-setsize={rows.length}
            aria-posinset={row.index + 1}
            onFocusCapture={() => setFocusedKey(keyFor(row.index))}
            onBlurCapture={(event) => {
              if (!event.currentTarget.contains(event.relatedTarget))
                setFocusedKey(null);
            }}
            className="absolute left-0 top-0 w-full pb-5"
            style={{ transform: `translateY(${row.start}px)` }}
          >
            {block.kind === "tools" ? (
              <ToolGroup
                key={block.id}
                tools={block.tools}
                projectId={project.project_id}
                sessionId={transcript.session_id}
              />
            ) : (
              <ItemView
                key={block.item.id}
                item={block.item}
                prompt={block.prompt}
                actions={actions}
              />
            )}
          </div>
        );
      })}
    </div>
  );
}

function ItemView({
  item,
  prompt,
  actions,
}: {
  item: Item;
  prompt: string | null;
  actions: Actions;
}) {
  switch (item.kind) {
    case "user":
      return (
        <div className="group/message flex flex-col items-end">
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
          <MessageActions>
            <CopyButton text={item.text} label="Copy message" />
            <ActionButton
              label="Edit in composer"
              onClick={() => editInComposer(actions.sessionId, item.text)}
            >
              <Pencil className="h-3.5 w-3.5" />
            </ActionButton>
            <RetryButton text={item.text} actions={actions} />
          </MessageActions>
        </div>
      );
    case "assistant":
      return (
        <div className="group/message">
          <Markdown text={item.text} streaming={item.state === "streaming"} />
          {item.refusal && (
            <p className="mt-2 rounded-md bg-warn-soft px-3 py-2 text-[13px] text-warn">
              {item.refusal}
            </p>
          )}
          {item.truncated && (
            <p className="mt-1 text-[12px] text-faint">
              Output reached the model's length limit.
            </p>
          )}
          {item.state !== "streaming" && item.text && (
            <MessageActions>
              <CopyButton text={item.text} label="Copy response" />
              {prompt && <RetryButton text={prompt} actions={actions} />}
            </MessageActions>
          )}
        </div>
      );
    case "reasoning":
      return <Reasoning text={item.text} streaming={item.streaming} />;
    case "run_end":
      return <RunEnd item={item} prompt={prompt} actions={actions} />;
    case "tool":
      return null;
  }
}

/** Puts `text` in this session's composer draft and focuses it. */
function editInComposer(sessionId: string, text: string) {
  drafts.set(sessionId, text);
  requestAnimationFrame(() => {
    const composer = document.querySelector<HTMLTextAreaElement>(
      'textarea[aria-label="Message"]',
    );
    composer?.focus();
    composer?.setSelectionRange(text.length, text.length);
  });
}

/** Shown under a message on hover or keyboard focus. */
function MessageActions({ children }: { children: ReactNode }) {
  return (
    <div className="mt-1 flex h-6 items-center gap-0.5 opacity-0 transition-opacity focus-within:opacity-100 group-hover/message:opacity-100">
      {children}
    </div>
  );
}

function ActionButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      title={label}
      className="flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-muted hover:bg-hover hover:text-fg disabled:pointer-events-none disabled:opacity-40"
    >
      {children}
    </button>
  );
}

/** Sends `text` again as a new turn; QQ keeps history, so nothing is replaced. */
function RetryButton({
  text,
  actions,
  children,
}: {
  text: string;
  actions: Actions;
  children?: ReactNode;
}) {
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const retry = async () => {
    setSending(true);
    setError(null);
    try {
      await api.submitPrompt(actions.projectId, actions.sessionId, text);
    } catch (error) {
      setError(String(error));
    } finally {
      setSending(false);
    }
  };
  return (
    <>
      <ActionButton
        label={
          actions.canSend ? "Send this prompt again" : "Retry when the run ends"
        }
        disabled={!actions.canSend || sending}
        onClick={() => void retry()}
      >
        <RotateCcw className="h-3.5 w-3.5" />
        {children}
      </ActionButton>
      {error && (
        <span role="alert" className="text-[11.5px] text-danger">
          {error}
        </span>
      )}
    </>
  );
}

const Markdown = memo(function Markdown({
  text,
  streaming,
}: {
  text: string;
  streaming: boolean;
}) {
  if (!text && streaming) {
    return <span className="streaming-caret text-muted" />;
  }
  return (
    <div className={clsx("prose-qq", streaming && "is-streaming")}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          pre: ({ children }) => {
            const code = isValidElement<{
              className?: string;
              children?: ReactNode;
            }>(children)
              ? children.props
              : null;
            return (
              <CodeBlock
                code={String(code?.children ?? "").replace(/\n$/, "")}
                language={languageOf(code?.className)}
              />
            );
          },
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
        <Brain
          className={clsx(
            "h-3.5 w-3.5",
            streaming && "animate-qq-pulse text-accent",
          )}
        />
        {streaming ? "Thinking…" : "Thought process"}
        <ChevronRight
          className={clsx(
            "h-3.5 w-3.5 transition-transform",
            open && "rotate-90",
          )}
        />
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

function RunEnd({
  item,
  prompt,
  actions,
}: {
  item: Extract<Item, { kind: "run_end" }>;
  prompt: string | null;
  actions: Actions;
}) {
  const label = outcomeLabel(item.outcome);
  const detail = outcomeDetail(item.outcome);
  const ok = item.outcome?.type === "completed";
  const failed =
    item.outcome?.type === "failed" ||
    item.outcome?.type === "budget_exhausted";
  const parts = [
    duration(item.duration_ms),
    item.tool_calls > 0
      ? `${item.tool_calls} tool call${item.tool_calls === 1 ? "" : "s"}`
      : "",
    item.usage
      ? `${tokens(item.usage.input_tokens + item.usage.cache_read_input_tokens)} in · ${tokens(item.usage.output_tokens)} out`
      : "",
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
        {failed && prompt && (
          <RetryButton text={prompt} actions={actions}>
            Retry
          </RetryButton>
        )}
      </div>
      {detail && (
        <p className="mt-1.5 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable">
          {detail}
        </p>
      )}
    </div>
  );
}

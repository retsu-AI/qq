import { useEffect, useRef } from "react";
import clsx from "clsx";
import { Loader2, MessagesSquare, Plus, Square, X } from "lucide-react";
import { api } from "../lib/api";
import { drafts } from "../lib/drafts";
import { hideSidePanel, useSidePanel } from "../lib/side-panel";
import type { ProjectState, Transcript } from "../lib/types";
import { Markdown } from "./Transcript";

const STATE_LABELS: Record<string, string> = {
  failed: "Failed",
  cancelled: "Cancelled",
  timed_out: "Timed out after 120 s",
  interrupted: "Interrupted",
};

/**
 * The latest side thread: questions asked with `/btw` and their answers. They
 * never enter the main run's context.
 */
export function SideQuestions({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const open = useSidePanel(transcript.session_id);
  const list = useRef<HTMLDivElement>(null);
  const questions = transcript.side_questions;
  const latest = questions[questions.length - 1];
  const thread = latest
    ? questions.filter((side) => side.thread_id === latest.thread_id)
    : [];
  const running = latest?.state === "running";

  // Follow the answer as it streams.
  useEffect(() => {
    const el = list.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [latest?.answer, latest?.id, open]);

  if (!open) return null;
  const ask = (prefix: string) => {
    drafts.set(transcript.session_id, prefix);
    requestAnimationFrame(() => {
      const composer = document.querySelector<HTMLTextAreaElement>(
        'textarea[aria-label="Message"]',
      );
      composer?.focus();
      composer?.setSelectionRange(prefix.length, prefix.length);
    });
  };

  return (
    <section
      aria-label="Side questions"
      className="animate-pop-up mb-2 overflow-hidden rounded-2xl border border-line-strong bg-sidebar"
    >
      <header className="flex items-center gap-2 border-b border-line py-1.5 pl-3.5 pr-1.5 text-[12.5px]">
        <MessagesSquare className="h-3.5 w-3.5 text-accent" />
        <span className="font-medium">Side question</span>
        <span className="text-faint">· not added to the run</span>
        <span className="flex-1" />
        {running ? (
          <button
            onClick={() =>
              void api.cancelSideQuestion(project.project_id, transcript.session_id)
            }
            className="flex h-6 items-center gap-1 rounded-md px-2 text-muted hover:bg-hover hover:text-fg"
          >
            <Square className="h-3 w-3 fill-current" /> Cancel
          </button>
        ) : (
          <button
            onClick={() => ask("/btw-new ")}
            title="Ask without the earlier side questions as context"
            className="flex h-6 items-center gap-1 rounded-md px-2 text-muted hover:bg-hover hover:text-fg"
          >
            <Plus className="h-3.5 w-3.5" /> New thread
          </button>
        )}
        <button
          onClick={() => hideSidePanel(transcript.session_id)}
          aria-label="Close side questions"
          className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </header>
      <div ref={list} className="max-h-[38vh] space-y-3 overflow-y-auto px-3.5 py-3">
        {thread.length === 0 && (
          <p className="text-[12.5px] text-muted">
            Ask with <kbd className="font-mono">/btw your question</kbd>. The
            agent keeps working; answers use the session so far and read-only
            file tools.
          </p>
        )}
        {thread.map((side) => (
          <div key={side.id} className="space-y-1.5">
            <p className="whitespace-pre-wrap text-[13px] font-medium selectable">
              {side.question}
            </p>
            {side.answer ? (
              <div className="text-[13px]">
                <Markdown text={side.answer} streaming={side.state === "running"} />
              </div>
            ) : side.state === "running" ? (
              <p className="flex items-center gap-1.5 text-[12.5px] text-muted">
                <Loader2 className="h-3.5 w-3.5 animate-spin" /> Thinking…
              </p>
            ) : null}
            {STATE_LABELS[side.state] && (
              <p
                className={clsx(
                  "text-[12px]",
                  side.state === "failed" ? "text-danger" : "text-faint",
                )}
              >
                {STATE_LABELS[side.state]}
              </p>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

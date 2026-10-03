import { useEffect, useLayoutEffect, useRef, useState } from "react";
import clsx from "clsx";
import { ArrowUp, ChevronDown, Shield, Square } from "lucide-react";
import { api } from "../lib/api";
import { modelShort } from "../lib/format";
import type { ApprovalMode, ProjectState, Transcript } from "../lib/types";

/** Unsent text per session (or per project for a new session). */
const drafts = new Map<string, string>();

const MODES: Array<{ value: ApprovalMode; label: string; hint: string }> = [
  { value: "read_only", label: "Read only", hint: "No edits or commands" },
  { value: "ask", label: "Ask", hint: "Ask before every edit and command" },
  { value: "auto", label: "Auto", hint: "Run safe actions, ask for risky ones" },
  { value: "full", label: "Full access", hint: "Run everything except forbidden actions" },
];

export function Composer({
  project,
  transcript,
  autoFocus,
}: {
  project: ProjectState;
  transcript: Transcript | null;
  autoFocus?: boolean;
}) {
  const key = transcript?.session_id ?? `new:${project.project_id}`;
  const [text, setText] = useState(() => drafts.get(key) ?? "");
  const ref = useRef<HTMLTextAreaElement>(null);
  const running = transcript ? transcript.status !== "idle" : false;

  useEffect(() => {
    setText(drafts.get(key) ?? "");
    ref.current?.focus();
  }, [key]);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 280)}px`;
  }, [text]);

  const update = (value: string) => {
    setText(value);
    drafts.set(key, value);
  };

  const submit = () => {
    const value = text.trim();
    if (!value) return;
    void api.submitPrompt(project.project_id, transcript?.session_id ?? null, value);
    update("");
  };

  const stop = () => {
    if (transcript) void api.cancelRun(project.project_id, transcript.session_id);
  };

  return (
    <div className="rounded-xl border border-line-strong bg-raised shadow-[0_1px_0_rgba(0,0,0,0.2)] focus-within:border-accent/60">
      <textarea
        ref={ref}
        value={text}
        autoFocus={autoFocus}
        rows={1}
        placeholder={
          running
            ? "Queue a follow-up…"
            : transcript
              ? "Reply…"
              : "Describe a task, ask a question, or paste an error…"
        }
        onChange={(event) => update(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
            event.preventDefault();
            submit();
          } else if (event.key === "Escape" && running) {
            event.preventDefault();
            stop();
          }
        }}
        className="block max-h-[280px] w-full resize-none bg-transparent px-4 pb-1 pt-3 text-[14px] leading-6 outline-none placeholder:text-faint selectable"
      />
      <div className="flex items-center gap-1 px-2 pb-2">
        <ModelPicker project={project} transcript={transcript} />
        {transcript && <ModePicker project={project} transcript={transcript} />}
        <div className="flex-1" />
        {running && (
          <button
            onClick={stop}
            title="Stop (Esc)"
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line-strong px-2.5 text-[12.5px] text-muted hover:bg-hover hover:text-fg"
          >
            <Square className="h-3 w-3 fill-current" /> Stop
          </button>
        )}
        <button
          onClick={submit}
          disabled={!text.trim()}
          title={running ? "Queue follow-up (Enter)" : "Send (Enter)"}
          className={clsx(
            "flex h-8 w-8 items-center justify-center rounded-lg",
            text.trim()
              ? "bg-accent text-white hover:bg-accent-strong"
              : "bg-hover text-faint",
          )}
        >
          <ArrowUp className="h-4 w-4" />
        </button>
      </div>
    </div>
  );
}

function ModelPicker({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript | null;
}) {
  const current = transcript?.model ?? project.selected_model;
  if (project.models.length === 0) {
    return <span className="px-2 text-[12px] text-faint">{modelShort(current)}</span>;
  }
  return (
    <label className="relative flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:bg-hover hover:text-fg">
      <span className="max-w-[200px] truncate">{modelShort(current)}</span>
      <ChevronDown className="h-3.5 w-3.5" />
      <select
        value={current ?? ""}
        onChange={(event) => void api.selectModel(project.project_id, event.target.value)}
        className="absolute inset-0 cursor-pointer opacity-0"
        title="Model"
      >
        {current && !project.models.some((m) => m.id === current) && (
          <option value={current}>{current}</option>
        )}
        {project.models.map((model) => (
          <option key={model.id} value={model.id}>
            {model.name ?? model.id}
          </option>
        ))}
      </select>
    </label>
  );
}

function ModePicker({ project, transcript }: { project: ProjectState; transcript: Transcript }) {
  const mode = MODES.find((m) => m.value === transcript.approval_mode);
  return (
    <label
      className="relative flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:bg-hover hover:text-fg"
      title={mode?.hint}
    >
      <Shield className="h-3.5 w-3.5" />
      <span>{mode?.label ?? transcript.approval_mode}</span>
      <ChevronDown className="h-3.5 w-3.5" />
      <select
        value={transcript.approval_mode}
        onChange={(event) =>
          void api.setApprovalMode(
            project.project_id,
            transcript.session_id,
            event.target.value as ApprovalMode,
          )
        }
        className="absolute inset-0 cursor-pointer opacity-0"
      >
        {MODES.map((m) => (
          <option key={m.value} value={m.value}>
            {m.label} — {m.hint}
          </option>
        ))}
        {!mode && <option value={transcript.approval_mode}>{transcript.approval_mode}</option>}
      </select>
    </label>
  );
}

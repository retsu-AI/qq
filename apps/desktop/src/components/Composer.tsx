import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import clsx from "clsx";
import { ArrowUp, ChevronDown, Shield, Square } from "lucide-react";
import { api } from "../lib/api";
import { modelShort } from "../lib/format";
import { showProviders } from "../lib/store";
import { drafts } from "../lib/drafts";
import type { ApprovalMode, ProjectState, Transcript } from "../lib/types";

/** Model-picker entry that opens the provider dialog instead of selecting. */
const MANAGE = "__manage_providers__";

const MODES: Array<{ value: ApprovalMode; label: string; hint: string }> = [
  { value: "read_only", label: "Read only", hint: "No edits or commands" },
  { value: "ask", label: "Ask", hint: "Ask before every edit and command" },
  {
    value: "auto",
    label: "Auto",
    hint: "Run safe actions, ask for risky ones",
  },
  {
    value: "full",
    label: "Full access",
    hint: "Run everything except forbidden actions",
  },
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
  useSyncExternalStore(drafts.subscribe, drafts.revision);
  const text = drafts.get(key);
  const pending = drafts.isPending(key);
  const ref = useRef<HTMLTextAreaElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [mode, setMode] = useState("queue");
  const files = drafts.get(`files:${key}`);
  const running = transcript ? transcript.status !== "idle" : false;

  useEffect(() => {
    setError(null);
    ref.current?.focus();
  }, [key]);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 280)}px`;
  }, [text]);

  const update = (value: string) => {
    drafts.set(key, value);
  };

  const submit = async () => {
    const value = text.trim();
    if (!value || pending || project.connection !== "live") return;
    setError(null);
    try {
      await drafts.submit(key, (value) =>
        api.submitPrompt(
          project.project_id,
          transcript?.session_id ?? null,
          value,
          running ? mode : "queue",
          files
            .split("\n")
            .map((path) => path.trim())
            .filter(Boolean),
        ),
      );
      if (drafts.get(`files:${key}`) === files) drafts.set(`files:${key}`, "");
    } catch (error) {
      setError(String(error));
    }
  };

  const stop = () => {
    if (transcript)
      void api
        .cancelRun(project.project_id, transcript.session_id)
        .catch((error) => setError(String(error)));
  };

  return (
    <div className="rounded-xl border border-line-strong bg-raised shadow-[0_1px_0_rgba(0,0,0,0.2)] focus-within:border-accent/60">
      <textarea
        ref={ref}
        value={text}
        readOnly={pending}
        aria-busy={pending}
        aria-label="Message"
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
          if (
            event.key === "Enter" &&
            !event.shiftKey &&
            !event.nativeEvent.isComposing
          ) {
            event.preventDefault();
            void submit();
          } else if (event.key === "Escape" && running) {
            event.preventDefault();
            stop();
          }
        }}
        className="block max-h-[280px] w-full resize-none bg-transparent px-4 pb-1 pt-3 text-[14px] leading-6 outline-none placeholder:text-faint selectable"
      />
      {error && (
        <p role="alert" className="px-4 py-2 text-danger">
          {error}
        </p>
      )}
      <details className="px-4 py-1 text-[12px] text-muted">
        <summary className="cursor-pointer">
          Attach workspace files · @file mentions supported
        </summary>
        <textarea
          aria-label="Workspace files, one relative path per line"
          placeholder="src/main.rs"
          value={files}
          disabled={pending}
          onChange={(event) => drafts.set(`files:${key}`, event.target.value)}
          className="my-2 w-full rounded border border-line bg-bg p-2"
        />
        <p>
          Up to eight files. Use paths relative to this project; files are
          checked by QQ.
        </p>
      </details>
      {transcript && (
        <SessionPickers project={project} transcript={transcript} />
      )}
      <div className="flex flex-wrap items-center gap-1 px-2 pb-2">
        <ModelPicker project={project} transcript={transcript} />
        {transcript && <ModePicker project={project} transcript={transcript} />}
        {running && (
          <select
            aria-label="Send action"
            value={mode}
            onChange={(event) => setMode(event.target.value)}
            className="max-w-40 rounded bg-raised px-2 py-1 text-[12px]"
          >
            <option value="queue">Queue follow-up</option>
            {project.can_steer && (
              <option value="steer">Steer at next boundary</option>
            )}
            {project.can_interrupt && (
              <option value="interrupt">Interrupt and steer</option>
            )}
          </select>
        )}
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
          onClick={() => void submit()}
          disabled={!text.trim() || pending || project.connection !== "live"}
          aria-label={
            pending
              ? "Waiting for acknowledgement"
              : running
                ? mode === "queue"
                  ? "Queue follow-up"
                  : mode === "steer"
                    ? "Steer run"
                    : "Interrupt and steer"
                : "Send message"
          }
          title={
            running
              ? `${mode === "queue" ? "Queue follow-up" : mode === "steer" ? "Steer run" : "Interrupt and steer"} (Enter)`
              : "Send (Enter)"
          }
          className={clsx(
            "flex h-8 w-8 items-center justify-center rounded-lg",
            text.trim()
              ? "bg-solid text-white hover:bg-solid-hover"
              : "bg-hover text-faint",
          )}
        >
          <ArrowUp className="h-4 w-4" />
        </button>
      </div>
    </div>
  );
}

function SessionPickers({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const model = project.models.find(
    (model) => model.id === (transcript.model ?? project.selected_model),
  );
  const select = (type: "profile" | "effort" | "delegate", value: string) =>
    void api.setSessionOption(
      project.project_id,
      transcript.session_id,
      type,
      value || null,
    );
  return (
    <div
      className="flex flex-wrap gap-2 px-4 py-2 text-[12px] text-muted"
      title="Profile and effort changes apply to the next run. Delegate applies to the next held call."
    >
      <label>
        Profile{" "}
        <select
          aria-label="Session profile"
          value={transcript.profile}
          onChange={(event) => select("profile", event.target.value)}
          className="rounded bg-raised"
        >
          {!project.profiles.includes(transcript.profile) && (
            <option value={transcript.profile}>{transcript.profile}</option>
          )}
          {project.profiles.map((profile) => (
            <option key={profile}>{profile}</option>
          ))}
        </select>
      </label>
      <label>
        Effort{" "}
        <select
          aria-label="Reasoning effort for next run"
          value={transcript.reasoning_effort ?? ""}
          onChange={(event) => select("effort", event.target.value)}
          className="rounded bg-raised"
        >
          <option value="">Configured default</option>
          {transcript.reasoning_effort &&
            !model?.reasoning_efforts.includes(transcript.reasoning_effort) && (
              <option>{transcript.reasoning_effort}</option>
            )}
          {model?.reasoning_efforts.map((effort) => (
            <option key={effort}>{effort}</option>
          ))}
        </select>
      </label>
      <label>
        Approval delegate{" "}
        <select
          aria-label="Approval delegate"
          value={transcript.approval_delegate ?? ""}
          onChange={(event) => select("delegate", event.target.value)}
          className="rounded bg-raised"
        >
          <option value="">Configured default</option>
          <option value="by_mode">By approval mode</option>
          <option value="off">Human only</option>
          <option value="on">Reviewer first</option>
        </select>
      </label>
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
    return (
      <button
        onClick={() => showProviders(true)}
        className="flex h-8 items-center rounded-lg px-2 text-[12.5px] text-accent hover:bg-hover"
      >
        Connect a provider…
      </button>
    );
  }
  const providers = [...new Set(project.models.map((m) => m.provider))];
  return (
    <label className="relative flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:bg-hover hover:text-fg">
      <span className="max-w-[200px] truncate">{modelShort(current)}</span>
      <ChevronDown className="h-3.5 w-3.5" />
      <select
        value={current ?? ""}
        onChange={(event) => {
          if (event.target.value === MANAGE) showProviders(true);
          else void api.selectModel(project.project_id, event.target.value);
        }}
        className="absolute inset-0 cursor-pointer opacity-0"
        title="Model"
      >
        {current && !project.models.some((m) => m.id === current) && (
          <option value={current}>{current}</option>
        )}
        {providers.map((provider) => (
          <optgroup key={provider} label={provider}>
            {project.models
              .filter((model) => model.provider === provider)
              .map((model) => (
                <option key={model.id} value={model.id}>
                  {model.name ?? model.model}
                </option>
              ))}
          </optgroup>
        ))}
        <option value={MANAGE}>Manage providers…</option>
      </select>
    </label>
  );
}

function ModePicker({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
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
        aria-label="Approval mode"
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
        {!mode && (
          <option value={transcript.approval_mode}>
            {transcript.approval_mode}
          </option>
        )}
      </select>
    </label>
  );
}

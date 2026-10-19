import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import clsx from "clsx";
import {
  ArrowUp,
  AtSign,
  BookOpen,
  Brain,
  ChevronDown,
  FileText,
  Folder,
  Loader2,
  MessagesSquare,
  Shield,
  SlidersHorizontal,
  Square,
  TerminalSquare,
  X,
} from "lucide-react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api } from "../lib/api";
import { dropMentions, insertMentions } from "../lib/drop";
import {
  cost,
  modelLabel,
  modelShort,
  providerName,
  tokens,
} from "../lib/format";
import { showProviders } from "../lib/store";
import { drafts } from "../lib/drafts";
import {
  findMention,
  insertMention,
  specialSuggestions,
  type ActiveMention,
  type Suggestion,
} from "../lib/mentions";
import { showSidePanel } from "../lib/side-panel";
import {
  findSlash,
  insertSlash,
  parseSideCommand,
  slashSuggestions,
  type SlashSkill,
} from "../lib/slash";
import type { ApprovalMode, ProjectState, Transcript } from "../lib/types";

/** The token being completed: an `@` mention or a leading `/command`. */
type ActiveToken = ActiveMention & { slash?: boolean };
import { Dropdown, type DropdownOption } from "./Dropdown";

type SendMode = "queue" | "steer" | "interrupt";

const MODES: Array<DropdownOption<ApprovalMode>> = [
  { value: "read_only", label: "Read only", hint: "No edits or commands" },
  { value: "ask", label: "Ask", hint: "Ask before every edit and command" },
  { value: "auto", label: "Auto", hint: "Run safe actions, ask for risky ones" },
  {
    value: "full",
    label: "Full access",
    hint: "Run everything except forbidden actions",
  },
];

const SEND_LABELS: Record<SendMode, string> = {
  queue: "Queue follow-up",
  steer: "Steer run",
  interrupt: "Interrupt and steer",
};

/** Debounce for completion requests while typing a mention. */
const COMPLETION_DELAY_MS = 60;

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
  const [mode, setMode] = useState<SendMode>("queue");
  const running = transcript ? transcript.status !== "idle" : false;
  const sideRunning = transcript?.side_questions.some(
    (side) => side.state === "running",
  ) ?? false;
  const mentions = useMentionMenu(project.project_id, ref, {
    skills: project.skills,
    side: {
      available: Boolean(transcript) && project.can_side_question,
      running: sideRunning,
    },
  });
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    setError(null);
    setNote(null);
    mentions.close();
    ref.current?.focus();
  }, [key]);

  // Files dropped anywhere on the window are mentioned at the caret.
  const dragging = useFileDrop((paths) => {
    const el = ref.current;
    const { mentions: dropped, skipped } = dropMentions(project.path, paths);
    const current = drafts.get(key);
    const caret = el && document.activeElement === el ? el.selectionStart : current.length;
    const next = insertMentions(current, caret, dropped);
    drafts.set(key, next.text);
    const outside = skipped.filter((s) => s.reason === "outside").length;
    const spaced = skipped.filter((s) => s.reason === "spaces").length;
    setNote(
      [
        outside
          ? `${outside} file${outside === 1 ? " is" : "s are"} outside this project and can't be mentioned.`
          : "",
        spaced
          ? `${spaced} path${spaced === 1 ? " has" : "s have"} spaces, which mentions can't contain.`
          : "",
      ]
        .filter(Boolean)
        .join(" ") || null,
    );
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(next.caret, next.caret);
    });
  });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const fit = () => {
      el.style.height = "0px";
      el.style.height = `${Math.min(el.scrollHeight, 280)}px`;
    };
    fit();
    // Wrapping depends on width, which can settle after the first layout
    // (a side panel opening, the window resizing).
    let width = el.clientWidth;
    const observer = new ResizeObserver(() => {
      if (el.clientWidth === width) return;
      width = el.clientWidth;
      fit();
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [text]);

  const submit = async () => {
    const value = text.trim();
    if (!value || pending || project.connection !== "live") return;
    setError(null);
    mentions.close();
    const side = parseSideCommand(value);
    if (side) {
      if (!transcript || !project.can_side_question) {
        setError(
          transcript
            ? "This engine does not answer side questions."
            : "Side questions need a session: send a first message.",
        );
        return;
      }
      const sessionId = transcript.session_id;
      try {
        await drafts.submit(key, async () => {
          if (side.kind === "ask")
            await api.submitSideQuestion(
              project.project_id,
              sessionId,
              side.question,
              side.newThread,
            );
          else if (side.kind === "cancel")
            await api.cancelSideQuestion(project.project_id, sessionId);
        });
        if (side.kind !== "cancel") showSidePanel(sessionId);
      } catch (error) {
        setError(String(error));
      }
      return;
    }
    try {
      await drafts.submit(key, (value) =>
        api.submitPrompt(
          project.project_id,
          transcript?.session_id ?? null,
          value,
          running ? mode : "queue",
          [],
        ),
      );
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

  const choose = (suggestion: Suggestion) => {
    const el = ref.current;
    const active = mentions.active;
    if (!el || !active) return;
    const next = active.slash
      ? insertSlash(text, el.selectionStart, suggestion.value)
      : insertMention(text, el.selectionStart, active, suggestion.value);
    drafts.set(key, next.text);
    // Apply the caret after React writes the new value.
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(next.caret, next.caret);
      mentions.update(next.text, next.caret);
    });
  };

  const sendLabel = running ? SEND_LABELS[mode] : "Send message";

  return (
    <div className="relative">
      {mentions.open && (
        <MentionMenu
          below={(ref.current?.getBoundingClientRect().top ?? Infinity) < 360}
          suggestions={mentions.suggestions}
          loading={mentions.loading}
          highlight={mentions.highlight}
          query={mentions.active?.query ?? ""}
          onHover={mentions.setHighlight}
          onChoose={choose}
          slash={Boolean(mentions.active?.slash)}
        />
      )}
      <div
        className={clsx(
          "relative rounded-2xl border bg-raised shadow-[0_1px_0_rgba(0,0,0,0.2)] transition-colors focus-within:border-accent/60 focus-within:ring-1 focus-within:ring-accent/25",
          dragging ? "border-accent ring-2 ring-accent/30" : "border-line-strong",
        )}
      >
        {dragging && (
          <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center gap-2 rounded-2xl bg-raised/90 text-[13px] text-accent">
            <FileText className="h-4 w-4" /> Drop to mention files from this
            project
          </div>
        )}
        <textarea
          ref={ref}
          value={text}
          readOnly={pending}
          aria-busy={pending}
          aria-label="Message"
          aria-autocomplete="list"
          aria-expanded={mentions.open}
          aria-controls={mentions.open ? "mention-menu" : undefined}
          aria-activedescendant={
            mentions.open && mentions.suggestions.length
              ? `mention-${mentions.highlight}`
              : undefined
          }
          autoFocus={autoFocus}
          rows={1}
          placeholder={
            running
              ? "Queue a follow-up…"
              : transcript
                ? "Reply… (@ to mention a file)"
                : "Describe a task, ask a question, or paste an error. Type @ to mention a file."
          }
          onChange={(event) => {
            drafts.set(key, event.target.value);
            // An error describes the text that was sent; editing retires it.
            if (error) setError(null);
            mentions.update(event.target.value, event.target.selectionStart);
          }}
          onSelect={(event) =>
            mentions.update(
              event.currentTarget.value,
              event.currentTarget.selectionStart,
            )
          }
          onBlur={() => mentions.close()}
          onFocus={(event) =>
            mentions.update(
              event.currentTarget.value,
              event.currentTarget.selectionStart,
            )
          }
          onKeyDown={(event) => {
            if (mentions.open) {
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                mentions.move(event.key === "ArrowDown" ? 1 : -1);
                return;
              }
              // Enter and Tab choose a suggestion, and wait while results
              // load instead of sending. With no matches, Enter sends.
              if (
                (event.key === "Enter" || event.key === "Tab") &&
                !event.shiftKey &&
                !event.nativeEvent.isComposing &&
                (mentions.loading || mentions.suggestions.length > 0)
              ) {
                event.preventDefault();
                const suggestion = mentions.suggestions[mentions.highlight];
                if (suggestion) choose(suggestion);
                return;
              }
            }
            if (mentions.open && event.key === "Escape") {
              event.preventDefault();
              mentions.dismiss();
              return;
            }
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
          className="block max-h-[280px] w-full resize-none bg-transparent px-4 pb-1.5 pt-3.5 text-[14px] leading-6 outline-none focus-visible:outline-none placeholder:text-faint selectable"
        />
        {error && (
          <p
            role="alert"
            className="mx-3 mb-1 rounded-md bg-danger-soft px-3 py-1.5 text-[12.5px] text-danger selectable"
          >
            {error}
          </p>
        )}
        {note && (
          <p
            role="status"
            className="mx-3 mb-1 flex items-start gap-2 rounded-md bg-hover px-3 py-1.5 text-[12.5px] text-muted"
          >
            <span className="flex-1">{note}</span>
            <button
              type="button"
              aria-label="Dismiss"
              onClick={() => setNote(null)}
              className="text-faint hover:text-fg"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </p>
        )}
        <div className="flex items-center gap-0.5 px-2 pb-2">
          <button
            type="button"
            title="Mention a file (@)"
            aria-label="Mention a file"
            disabled={pending}
            onClick={() => {
              const el = ref.current;
              if (!el) return;
              const caret = el.selectionStart;
              const needsSpace = caret > 0 && !/\s/.test(text[caret - 1] ?? "");
              const next = `${text.slice(0, caret)}${needsSpace ? " " : ""}@${text.slice(caret)}`;
              const nextCaret = caret + (needsSpace ? 2 : 1);
              drafts.set(key, next);
              requestAnimationFrame(() => {
                el.focus();
                el.setSelectionRange(nextCaret, nextCaret);
                mentions.update(next, nextCaret);
              });
            }}
            className="flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-hover hover:text-fg disabled:opacity-50"
          >
            <AtSign className="h-4 w-4" />
          </button>
          {transcript && project.can_side_question && (
            <button
              type="button"
              title="Ask a side question without interrupting the run (/btw)"
              aria-label="Ask a side question"
              disabled={pending}
              onClick={() => {
                const el = ref.current;
                // Turns the current draft into a side question.
                const body = text.replace(/^\/(btw|ask|btw-new)\b\s*/, "");
                const next = `/btw ${body}`;
                drafts.set(key, next);
                showSidePanel(transcript.session_id);
                requestAnimationFrame(() => {
                  el?.focus();
                  el?.setSelectionRange(next.length, next.length);
                });
              }}
              className="flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-hover hover:text-fg disabled:opacity-50"
            >
              <MessagesSquare className="h-4 w-4" />
            </button>
          )}
          <ModelPicker project={project} transcript={transcript} />
          {transcript && <EffortPicker project={project} transcript={transcript} />}
          {transcript && <ModePicker project={project} transcript={transcript} />}
          {transcript && <SessionOptions project={project} transcript={transcript} />}
          <div className="flex-1" />
          {transcript && <ContextMeter transcript={transcript} />}
          {running && (
            <Dropdown<SendMode>
              label="When I send during a run"
              value={mode}
              onChange={setMode}
              align="end"
              options={[
                {
                  value: "queue",
                  label: "Queue follow-up",
                  hint: "Runs after the current run finishes",
                },
                ...(project.can_steer
                  ? [
                      {
                        value: "steer" as const,
                        label: "Steer",
                        hint: "Adds your message at the next step",
                      },
                    ]
                  : []),
                ...(project.can_interrupt
                  ? [
                      {
                        value: "interrupt" as const,
                        label: "Interrupt and steer",
                        hint: "Stops the current step, then continues with it",
                      },
                    ]
                  : []),
              ]}
            >
              <span>{mode === "queue" ? "Queue" : mode === "steer" ? "Steer" : "Interrupt"}</span>
              <ChevronDown className="h-3.5 w-3.5" />
            </Dropdown>
          )}
          {running && (
            <button
              onClick={stop}
              title="Stop (Esc)"
              className="ml-1 flex h-8 items-center gap-1.5 rounded-lg border border-line-strong px-2.5 text-[12.5px] text-muted hover:bg-hover hover:text-fg"
            >
              <Square className="h-3 w-3 fill-current" /> Stop
            </button>
          )}
          <button
            onClick={() => void submit()}
            disabled={!text.trim() || pending || project.connection !== "live"}
            aria-label={pending ? "Waiting for acknowledgement" : sendLabel}
            title={`${sendLabel} (Enter)`}
            className={clsx(
              "ml-1 flex h-8 w-8 items-center justify-center rounded-lg",
              text.trim() && !pending
                ? "bg-solid text-white hover:bg-solid-hover"
                : "bg-hover text-faint",
            )}
          >
            {pending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <ArrowUp className="h-4 w-4" />
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

/** True while files are dragged over the window; calls `onDrop` with paths. */
function useFileDrop(onDrop: (paths: string[]) => void): boolean {
  const [dragging, setDragging] = useState(false);
  const handler = useRef(onDrop);
  handler.current = onDrop;
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    let webview: ReturnType<typeof getCurrentWebview>;
    try {
      webview = getCurrentWebview();
    } catch {
      // Outside the desktop shell (the render fixture) there is no webview.
      return;
    }
    webview
      .onDragDropEvent((event) => {
        const payload = event.payload;
        if (payload.type === "enter") setDragging(payload.paths.length > 0);
        else if (payload.type === "leave") setDragging(false);
        else if (payload.type === "drop") {
          setDragging(false);
          if (payload.paths.length) handler.current(payload.paths);
        }
      })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  return dragging;
}

/** Context used as a ring, with tokens and session cost on hover. */
function ContextMeter({ transcript }: { transcript: Transcript }) {
  const limit = transcript.context_window;
  const used = transcript.context_tokens;
  const spent =
    transcript.cost_usd_nanos != null && transcript.cost_usd_nanos > 0
      ? cost(transcript.cost_usd_nanos)
      : "";
  if (!limit || used == null) return null;
  const fraction = Math.min(1, used / limit);
  const percent = Math.round(fraction * 100);
  const radius = 6.5;
  const circumference = 2 * Math.PI * radius;
  const tone =
    percent > 85 ? "text-danger" : percent > 65 ? "text-warn" : "text-accent";
  const label = `${percent}% of context used: ${tokens(used)} of ${tokens(limit)} tokens${spent ? `. Session cost ${spent}` : ""}`;
  return (
    <span
      role="img"
      aria-label={label}
      title={`${label}${percent > 65 ? ". Compact the session from Ctrl K to free space." : ""}`}
      className="mr-1 flex h-8 items-center gap-1.5 px-1 text-[11.5px] text-faint"
    >
      <svg viewBox="0 0 16 16" className="h-4 w-4 -rotate-90">
        <circle
          cx="8"
          cy="8"
          r={radius}
          fill="none"
          strokeWidth="2"
          className="stroke-line-strong"
        />
        <circle
          cx="8"
          cy="8"
          r={radius}
          fill="none"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray={`${fraction * circumference} ${circumference}`}
          className={clsx("stroke-current", tone)}
        />
      </svg>
      {percent}%{spent && <span>· {spent}</span>}
    </span>
  );
}

/**
 * Tracks the `@` token at the caret and fetches suggestions for it. Results
 * arriving for an older query are dropped.
 */
function useMentionMenu(
  projectId: string,
  textarea: React.RefObject<HTMLTextAreaElement | null>,
  slash: { skills: SlashSkill[]; side: { available: boolean; running: boolean } },
) {
  const [active, setActive] = useState<ActiveToken | null>(null);
  const [dismissed, setDismissed] = useState<number | null>(null);
  const [suggestions, setSuggestions] = useState<Suggestion[]>([]);
  const [loading, setLoading] = useState(false);
  const [highlight, setHighlight] = useState(0);
  const sequence = useRef(0);

  // `/` completes from the capabilities already in hand: no request.
  const slashKey = [
    slash.side.available,
    slash.side.running,
    ...slash.skills.map((skill) => skill.name),
  ].join("|");
  useEffect(() => {
    if (!active?.slash) return;
    sequence.current++;
    setSuggestions(slashSuggestions(active.query, slash.skills, slash.side));
    setHighlight(0);
    setLoading(false);
  }, [active?.slash, active?.query, slashKey]);

  useEffect(() => {
    if (!active || active.slash) return;
    const request = ++sequence.current;
    setLoading(true);
    const timer = window.setTimeout(() => {
      api
        .completeMention(projectId, active.query)
        .catch(() => [] as string[])
        .then((paths) => {
          if (request !== sequence.current) return;
          setSuggestions([
            ...specialSuggestions(active.query),
            ...paths.map<Suggestion>((path) => ({
              value: path,
              kind: path.endsWith("/") ? "dir" : "file",
            })),
          ]);
          setHighlight(0);
          setLoading(false);
        });
    }, COMPLETION_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [projectId, active?.start, active?.query, active?.slash]);

  const update = (text: string, caret: number) => {
    const command = findSlash(text, caret);
    const found: ActiveToken | null = command
      ? { ...command, slash: true }
      : findMention(text, caret);
    if (!found || (dismissed !== null && found.start === dismissed)) {
      if (!found) setDismissed(null);
      setActive(null);
      return;
    }
    setActive((previous) =>
      previous?.start === found.start &&
      previous.query === found.query &&
      Boolean(previous.slash) === Boolean(found.slash)
        ? previous
        : found,
    );
  };
  const close = () => {
    sequence.current++;
    setActive(null);
    setSuggestions([]);
    setLoading(false);
  };

  return {
    active,
    open:
      active !== null &&
      document.activeElement === textarea.current &&
      (loading || suggestions.length > 0 || active.query.length > 0),
    suggestions,
    loading,
    highlight,
    setHighlight,
    update,
    close,
    /** Escape: hide until the caret leaves this mention. */
    dismiss: () => {
      setDismissed(active?.start ?? null);
      close();
    },
    move: (delta: number) =>
      setHighlight((index) =>
        suggestions.length === 0
          ? 0
          : (index + delta + suggestions.length) % suggestions.length,
      ),
  };
}

function MentionMenu({
  suggestions,
  loading,
  highlight,
  query,
  onHover,
  onChoose,
  below,
  slash,
}: {
  /** Open under the composer: there is no room above it. */
  below: boolean;
  /** Completing a `/command` rather than an `@` mention. */
  slash: boolean;
  suggestions: Suggestion[];
  loading: boolean;
  highlight: number;
  query: string;
  onHover: (index: number) => void;
  onChoose: (suggestion: Suggestion) => void;
}) {
  const list = useRef<HTMLDivElement>(null);
  useEffect(() => {
    list.current
      ?.querySelector(`[data-index="${highlight}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [highlight]);
  return (
    <div
      ref={list}
      id="mention-menu"
      role="listbox"
      aria-label={slash ? "Command suggestions" : "Mention suggestions"}
      className={clsx(
        "absolute left-0 right-0 z-40 max-h-[300px] overflow-y-auto rounded-xl border border-line-strong bg-raised p-1 text-[13px] shadow-popover",
        below ? "animate-pop-down top-full mt-2" : "animate-pop-up bottom-full mb-2",
      )}
    >
      <div className="flex items-center justify-between px-2.5 pb-1 pt-1.5 text-[10.5px] font-semibold uppercase tracking-wider text-faint">
        <span>{slash ? "Commands" : "Mention"}</span>
        <span className="font-normal normal-case tracking-normal">
          ↑↓ choose · Enter insert · Esc close
        </span>
      </div>
      {suggestions.length === 0 && (
        <div className="flex items-center gap-2 px-2.5 py-2 text-muted">
          {loading ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" /> Searching files…
            </>
          ) : (
            <>
              No {slash ? "commands" : "files"} match “{query}”
            </>
          )}
        </div>
      )}
      {suggestions.map((suggestion, index) => {
        const cut = slash ? -1 : suggestion.value.replace(/\/$/, "").lastIndexOf("/");
        const directory = cut >= 0 ? suggestion.value.slice(0, cut + 1) : "";
        const name = slash
          ? `/${suggestion.value}`
          : cut >= 0
            ? suggestion.value.slice(cut + 1)
            : suggestion.value;
        return (
          <div
            key={`${suggestion.kind}:${suggestion.value}`}
            id={`mention-${index}`}
            data-index={index}
            role="option"
            aria-selected={index === highlight}
            onMouseEnter={() => onHover(index)}
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => onChoose(suggestion)}
            className={clsx(
              "flex cursor-default items-center gap-2.5 rounded-lg px-2.5 py-1.5",
              index === highlight && "bg-hover",
            )}
          >
            {suggestion.kind === "builtin" ? (
              <MessagesSquare className="h-3.5 w-3.5 shrink-0 text-accent" />
            ) : suggestion.kind === "command" ? (
              <TerminalSquare className="h-3.5 w-3.5 shrink-0 text-accent" />
            ) : suggestion.kind === "skill" ? (
              <BookOpen className="h-3.5 w-3.5 shrink-0 text-accent" />
            ) : suggestion.kind === "dir" ? (
              <Folder className="h-3.5 w-3.5 shrink-0 text-accent" />
            ) : suggestion.kind === "special" ? (
              <AtSign className="h-3.5 w-3.5 shrink-0 text-accent" />
            ) : (
              <FileText className="h-3.5 w-3.5 shrink-0 text-muted" />
            )}
            <span className="min-w-0 truncate font-mono text-[12.5px]">
              <span className="text-fg">{name}</span>
              {directory && <span className="ml-2 text-faint">{directory}</span>}
            </span>
            {suggestion.hint && (
              <span className="ml-auto min-w-0 shrink truncate pl-3 text-[12px] text-muted">
                {suggestion.hint}
              </span>
            )}
          </div>
        );
      })}
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
  const options: Array<DropdownOption<string>> = project.models.map((model) => ({
    value: model.id,
    label: model.name ?? model.model,
    group: providerName(model.provider),
  }));
  if (current && !project.models.some((model) => model.id === current))
    options.unshift({ value: current, label: modelShort(current), group: "Current" });
  return (
    <Dropdown
      label="Model"
      title="Model"
      value={current}
      options={options}
      onChange={(model) => void api.selectModel(project.project_id, model)}
      actions={[{ label: "Manage providers…", onSelect: () => showProviders(true) }]}
    >
      <span className="max-w-[180px] truncate">
        {modelLabel(current, project.models)}
      </span>
      <ChevronDown className="h-3.5 w-3.5" />
    </Dropdown>
  );
}

function EffortPicker({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const model = project.models.find(
    (model) => model.id === (transcript.model ?? project.selected_model),
  );
  const efforts = model?.reasoning_efforts ?? [];
  if (efforts.length === 0 && !transcript.reasoning_effort) return null;
  const options: Array<DropdownOption<string>> = [
    { value: "", label: "Default", hint: "The configured effort" },
    ...efforts.map((effort) => ({ value: effort, label: capitalize(effort) })),
  ];
  if (transcript.reasoning_effort && !efforts.includes(transcript.reasoning_effort))
    options.push({
      value: transcript.reasoning_effort,
      label: capitalize(transcript.reasoning_effort),
    });
  return (
    <Dropdown
      label="Reasoning effort for the next run"
      title="Reasoning effort (applies to the next run)"
      value={transcript.reasoning_effort ?? ""}
      options={options}
      onChange={(effort) =>
        void api.setSessionOption(
          project.project_id,
          transcript.session_id,
          "effort",
          effort || null,
        )
      }
    >
      <Brain className="h-3.5 w-3.5" />
      <span>{transcript.reasoning_effort ? capitalize(transcript.reasoning_effort) : "Effort"}</span>
      <ChevronDown className="h-3.5 w-3.5" />
    </Dropdown>
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
  const options = mode
    ? MODES
    : [...MODES, { value: transcript.approval_mode, label: transcript.approval_mode }];
  return (
    <Dropdown
      label="Approval mode"
      title={mode?.hint}
      value={transcript.approval_mode}
      options={options}
      onChange={(value) =>
        void api.setApprovalMode(project.project_id, transcript.session_id, value)
      }
    >
      <Shield className="h-3.5 w-3.5" />
      <span>{mode?.label ?? transcript.approval_mode}</span>
      <ChevronDown className="h-3.5 w-3.5" />
    </Dropdown>
  );
}

const DELEGATES: Array<{ value: string; label: string; hint: string }> = [
  { value: "", label: "Configured default", hint: "From your QQ configuration" },
  { value: "by_mode", label: "By approval mode", hint: "The mode decides who reviews" },
  { value: "off", label: "Me only", hint: "Every held call waits for you" },
  { value: "on", label: "Reviewer first", hint: "A reviewer answers before you" },
];

/** Profile and approval delegate: settings that are changed rarely. */
function SessionOptions({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const profiles = project.profiles.includes(transcript.profile)
    ? project.profiles
    : [transcript.profile, ...project.profiles];
  const delegate = transcript.approval_delegate ?? "";
  const options: Array<DropdownOption<string>> = [
    ...profiles.map((profile) => ({
      value: `profile:${profile}`,
      label: profile,
      group: "Profile (next run)",
      selected: profile === transcript.profile,
    })),
    ...DELEGATES.map((option) => ({
      value: `delegate:${option.value}`,
      label: option.label,
      hint: option.hint,
      group: "Who approves held calls",
      selected: option.value === delegate,
    })),
  ];
  return (
    <Dropdown
      label="Session options"
      title={`Profile: ${transcript.profile} · Approvals: ${
        DELEGATES.find((option) => option.value === delegate)?.label ?? delegate
      }`}
      value={null}
      options={options}
      onChange={(value) => {
        const [type, choice] = [value.slice(0, value.indexOf(":")), value.slice(value.indexOf(":") + 1)];
        void api.setSessionOption(
          project.project_id,
          transcript.session_id,
          type === "profile" ? "profile" : "delegate",
          choice || null,
        );
      }}
    >
      <SlidersHorizontal className="h-3.5 w-3.5" />
      {transcript.profile !== "default" && (
        <span className="max-w-[100px] truncate">{transcript.profile}</span>
      )}
    </Dropdown>
  );
}

function capitalize(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1).replace(/_/g, " ");
}

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import clsx from "clsx";
import { ArrowDown, KeyRound, Loader2 } from "lucide-react";
import { showProviders } from "../lib/store";
import { api } from "../lib/api";
import { activityLabel, cost, modelShort, tokens } from "../lib/format";
import type { PendingFile, ProjectState, Transcript as TranscriptData } from "../lib/types";
import { Composer } from "./Composer";
import { Transcript } from "./Transcript";
import { TrustPrompt } from "./TrustPrompt";

export function SessionPane({
  project,
  pendingTrust,
}: {
  project: ProjectState;
  pendingTrust: PendingFile[];
}) {
  const transcript = project.transcript;

  if (pendingTrust.length > 0) {
    return <TrustPrompt project={project} pending={pendingTrust} />;
  }

  if (project.connection === "failed") {
    return (
      <div className="flex flex-1 items-center justify-center p-10">
        <div className="max-w-md text-center">
          <h2 className="text-[16px] font-semibold">The QQ engine is unavailable</h2>
          <p className="mt-2 text-muted selectable">{project.error}</p>
          <button className="mt-3 rounded border border-line-strong px-3 py-2" onClick={() => void api.refresh(project.project_id)}>Retry connection</button>
          <p className="mt-4 text-[12.5px] text-faint">
            Install qq on PATH, build it with <code className="font-mono">cargo build</code>, or set
            <code className="mx-1 font-mono">QQ_DESKTOP_ENGINE</code> to its path, then reopen the project.
          </p>
        </div>
      </div>
    );
  }

  if (!project.focused || !transcript) {
    return <Landing project={project} />;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {project.connection !== "live" && <div role="status" className="border-b border-line bg-warn-soft px-5 py-2 text-warn">{project.connection === "replaying" ? "Catching up with saved history…" : "Disconnected. Sending is paused while QQ reconnects."}</div>}
      <Header project={project} transcript={transcript} />
      <TranscriptScroller project={project} transcript={transcript} />
      <div className="mx-auto w-full max-w-[860px] px-6 pb-5">
        <NoticeLine project={project} />
        <Composer project={project} transcript={transcript} />
      </div>
    </div>
  );
}

function Landing({ project }: { project: ProjectState }) {
  const connecting = project.connection !== "live" && project.sessions.length === 0;
  const signedOut = project.connection === "live" && project.models.length === 0;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="h-12 shrink-0" />
      <div className="flex flex-1 flex-col items-center justify-center px-6 pb-[12vh]">
        <div className="w-full max-w-[760px]">
          <h1 className="mb-1 text-center text-[24px] font-semibold tracking-tight">
            What should we work on?
          </h1>
          <p className="mb-6 text-center text-muted">
            in <span className="font-medium text-fg">{project.name}</span>
          </p>
          {connecting ? (
            <><NoticeLine project={project} /><div role="status" className="flex items-center justify-center gap-2 text-muted">
              <Loader2 className="h-4 w-4 animate-spin" /> {project.connection === "offline" ? "Offline, reconnecting to the engine…" : "Connecting to the engine…"}
            </div></>
          ) : signedOut ? (
            <ConnectProvider />
          ) : (
            <>
              <NoticeLine project={project} />
              <Composer project={project} transcript={null} autoFocus />
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function ConnectProvider() {
  return (
    <div className="mx-auto max-w-[460px] rounded-xl border border-line-strong bg-raised px-6 py-5 text-center">
      <KeyRound className="mx-auto mb-3 h-5 w-5 text-accent" />
      <p className="font-medium">Connect a model provider to start</p>
      <p className="mt-1 text-[12.5px] text-muted">
        Sign in with ChatGPT, or paste an Anthropic, OpenAI, Google, or xAI API key.
      </p>
      <button
        onClick={() => showProviders(true)}
        className="mt-4 h-9 rounded-lg bg-solid px-4 text-[13px] font-medium text-white hover:bg-solid-hover"
      >
        Connect a provider
      </button>
    </div>
  );
}

function Header({ project, transcript }: { project: ProjectState; transcript: TranscriptData }) {
  const running = transcript.status !== "idle";
  const limit = transcript.context_window;
  const used = transcript.context_tokens;
  const percent = limit && used ? Math.min(100, Math.round((used / limit) * 100)) : null;
  const runningTool = project.sessions.find((s) => s.id === transcript.session_id)?.active_tool;

  return (
    <header className="flex h-12 shrink-0 items-center gap-3 border-b border-line px-5">
      <h2 className="min-w-0 flex-1 truncate text-[14px] font-medium">{transcript.title}</h2>
      {running && (
        <span className="flex items-center gap-1.5 text-[12px] text-accent">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {activityLabel(transcript.activity, runningTool)}
        </span>
      )}
      {transcript.queued_prompts > 0 && (
        <span className="text-[12px] text-muted">{transcript.queued_prompts} queued</span>
      )}
      <span className="text-[12px] text-faint" title={transcript.model ?? undefined}>
        {modelShort(transcript.model)}
      </span>
      {percent !== null && (
        <span
          className="flex items-center gap-1.5 text-[12px] text-faint"
          title={`${tokens(used)} of ${tokens(limit)} context tokens`}
        >
          <span className="h-1.5 w-12 overflow-hidden rounded-full bg-raised">
            <span
              className={clsx(
                "block h-full rounded-full",
                percent > 85 ? "bg-danger" : percent > 65 ? "bg-warn" : "bg-accent",
              )}
              style={{ width: `${percent}%` }}
            />
          </span>
          {percent}%
        </span>
      )}
      {transcript.cost_usd_nanos != null && transcript.cost_usd_nanos > 0 && (
        <span className="text-[12px] text-faint">{cost(transcript.cost_usd_nanos)}</span>
      )}
    </header>
  );
}

/** Keeps the view pinned to the newest output unless the user scrolled up. */
function TranscriptScroller({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: TranscriptData;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const [showJump, setShowJump] = useState(false);

  // A different session always opens at its end.
  useEffect(() => {
    pinned.current = true;
  }, [transcript.session_id]);

  useLayoutEffect(() => {
    const el = ref.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  });

  const onScroll = () => {
    const el = ref.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    pinned.current = atBottom;
    setShowJump(!atBottom);
  };

  return (
    <div className="relative min-h-0 flex-1">
      <div ref={ref} onScroll={onScroll} className="h-full overflow-y-auto">
        <div className="mx-auto w-full max-w-[860px] px-6 pb-6 pt-6">
          {transcript.loading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="h-4 w-4 animate-spin" /> Loading session…
            </div>
          ) : (
            <Transcript project={project} transcript={transcript} />
          )}
        </div>
      </div>
      {showJump && (
        <button
          onClick={() => {
            pinned.current = true;
            ref.current?.scrollTo({ top: ref.current.scrollHeight, behavior: "smooth" });
          }}
          className="absolute bottom-3 left-1/2 flex -translate-x-1/2 items-center gap-1 rounded-full border border-line-strong bg-raised px-3 py-1 text-[12px] text-muted shadow-lg hover:text-fg"
        >
          <ArrowDown className="h-3.5 w-3.5" /> Latest
        </button>
      )}
    </div>
  );
}

function NoticeLine({ project }: { project: ProjectState }) {
  const notice = project.notice;
  if (!notice) return null;
  return (
    <div
      role={notice.level === "error" ? "alert" : "status"}
      className={clsx(
        "mb-2 rounded-md px-3 py-1.5 text-[12.5px] selectable",
        notice.level === "error" && "bg-danger-soft text-danger",
        notice.level === "warning" && "bg-warn-soft text-warn",
        notice.level === "info" && "bg-raised text-muted",
      )}
    >
      {notice.text}
    </div>
  );
}

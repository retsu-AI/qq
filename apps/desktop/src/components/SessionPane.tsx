import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import clsx from "clsx";
import {
  ArrowDown,
  Code2,
  FileDiff,
  KeyRound,
  Loader2,
  Target,
} from "lucide-react";
import { openInEditor } from "../lib/editor";
import { showProviders } from "../lib/store";
import { api } from "../lib/api";
import { collectChanges, type FileChange } from "../lib/changes";
import { updatePreferences, usePreferences } from "../lib/preferences";
import { activityLabel, modelLabel } from "../lib/format";
import type {
  GoalSnapshot,
  PendingFile,
  ProjectState,
  Transcript as TranscriptData,
} from "../lib/types";
import { ChangesPanel } from "./ChangesPanel";
import { Composer } from "./Composer";
import { GitStrip } from "./GitStrip";
import { ConfirmDialog } from "./ConfirmDialog";
import { GoalDialog } from "./GoalDialog";
import { GoalPanel, TONE_CLASSES } from "./GoalPanel";
import { SideQuestions } from "./SideQuestions";
import { budgetSummary, goalStatus } from "../lib/goals";
import {
  closeGoalDialog,
  closeGoalPanel,
  toggleGoalPanel,
  useGoalUi,
} from "../lib/goal-ui";
import { toast } from "../lib/toast";
import { Starters } from "./Starters";
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
          <h2 className="text-[16px] font-semibold">
            QQ could not load this project
          </h2>
          <p className="mt-2 text-muted selectable">{project.error}</p>
          <button
            className="mt-3 rounded border border-line-strong px-3 py-2"
            onClick={() => void api.refresh(project.project_id)}
          >
            Retry connection
          </button>
          <p className="mt-4 text-[12.5px] text-faint">
            Check the error above and connection details in Settings, then
            retry. If the engine was updated, restart its local server before
            reconnecting.
          </p>
        </div>
      </div>
    );
  }

  if (!project.focused || !transcript) {
    return <Landing project={project} />;
  }

  return <SessionView project={project} transcript={transcript} />;
}

function SessionView({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: TranscriptData;
}) {
  const { changesPanel } = usePreferences();
  const changes = useMemo(
    () => collectChanges(transcript.items),
    [transcript.items],
  );
  const goalUi = useGoalUi();
  const goalOpen = goalUi.panel === transcript.session_id;
  // One side panel at a time: opening Changes closes the goal panel.
  const toggleChanges = () => {
    if (goalOpen) closeGoalPanel();
    updatePreferences({ changesPanel: goalOpen ? true : !changesPanel });
  };
  const dialog =
    goalUi.dialog?.sessionId === transcript.session_id ? goalUi.dialog : null;
  return (
    <div className="relative flex min-h-0 flex-1">
      <div className="flex min-w-0 flex-1 flex-col">
        {project.connection !== "live" && (
          <div
            role="status"
            className="border-b border-line bg-warn-soft px-5 py-2 text-warn"
          >
            {project.connection === "replaying"
              ? "Catching up with saved history…"
              : "Disconnected. Sending is paused while QQ reconnects."}
          </div>
        )}
        <Header
          project={project}
          transcript={transcript}
          changes={changes}
          changesOpen={changesPanel && !goalOpen}
          toggleChanges={toggleChanges}
          goalOpen={goalOpen}
        />
        <TranscriptScroller project={project} transcript={transcript} />
        <div className="mx-auto w-full max-w-[860px] px-6 pb-5">
          <SideQuestions project={project} transcript={transcript} />
          <CheckInterruptionBanner project={project} />
          <NoticeLine project={project} />
          <Composer project={project} transcript={transcript} />
        </div>
      </div>
      {goalOpen && <GoalPanel project={project} transcript={transcript} />}
      {dialog && (
        <GoalDialog
          projectId={project.project_id}
          sessionId={transcript.session_id}
          current={transcript.goal}
          initialObjective={dialog.objective}
          edit={dialog.edit}
          close={closeGoalDialog}
        />
      )}
      {changesPanel && !goalOpen && (
        <ChangesPanel
          key={transcript.session_id}
          projectId={project.project_id}
          changes={changes}
          close={toggleChanges}
        />
      )}
    </div>
  );
}

function Landing({ project }: { project: ProjectState }) {
  const connecting =
    project.connection !== "live" && project.sessions.length === 0;
  const signedOut =
    project.connection === "live" && project.models.length === 0;
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
            <>
              <NoticeLine project={project} />
              <div
                role="status"
                className="flex items-center justify-center gap-2 text-muted"
              >
                <Loader2 className="h-4 w-4 animate-spin" />{" "}
                {project.connection === "offline"
                  ? "Offline, reconnecting to the engine…"
                  : "Connecting to the engine…"}
              </div>
            </>
          ) : signedOut ? (
            <ConnectProvider />
          ) : (
            <>
              <NoticeLine project={project} />
              <Composer project={project} transcript={null} autoFocus />
              <div className="mt-4">
                <Starters draftKey={`new:${project.project_id}`} />
              </div>
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
        Sign in with ChatGPT, or paste an Anthropic, OpenAI, Google, or xAI API
        key.
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

function Header({
  project,
  transcript,
  changes,
  changesOpen,
  toggleChanges,
  goalOpen,
}: {
  project: ProjectState;
  transcript: TranscriptData;
  changes: FileChange[];
  changesOpen: boolean;
  toggleChanges: () => void;
  goalOpen: boolean;
}) {
  const running = transcript.status !== "idle";
  const runningTool = project.sessions.find(
    (s) => s.id === transcript.session_id,
  )?.active_tool;

  return (
    <header className="flex h-12 shrink-0 items-center gap-3 border-b border-line px-5">
      <h2 className="min-w-0 flex-1 truncate text-[14px] font-medium">
        {transcript.title}
      </h2>
      {running && (
        <span className="flex items-center gap-1.5 text-[12px] text-accent">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {activityLabel(transcript.activity, runningTool)}
        </span>
      )}
      {transcript.queued_prompts > 0 && (
        <span className="text-[12px] text-muted">
          {transcript.queued_prompts} queued
        </span>
      )}
      <span
        className="text-[12px] text-faint"
        title={transcript.model ?? undefined}
      >
        {modelLabel(transcript.model, project.models)}
      </span>
      {(project.can_goal || transcript.goal) && (
        <GoalChip
          goal={transcript.goal}
          open={goalOpen}
          toggle={() => toggleGoalPanel(transcript.session_id)}
        />
      )}
      <GitStrip
        projectId={project.project_id}
        sessionTitle={transcript.title}
        refreshKey={`${transcript.session_id}:${transcript.status}:${transcript.items.length}`}
      />
      <button
        onClick={() => void openInEditor(project.project_id)}
        aria-label="Open in editor"
        title="Open the project in your editor (Ctrl Shift E)"
        className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg"
      >
        <Code2 className="h-3.5 w-3.5" />
      </button>
      <button
        onClick={toggleChanges}
        aria-pressed={changesOpen}
        title={`${changesOpen ? "Hide" : "Show"} changes (Ctrl Shift D)`}
        className={clsx(
          "-mr-2 flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px]",
          changesOpen
            ? "bg-active text-fg"
            : "text-muted hover:bg-hover hover:text-fg",
        )}
      >
        <FileDiff className="h-3.5 w-3.5" />
        Changes
        {changes.length > 0 && (
          <span className="rounded-full bg-accent-soft px-1.5 text-[11px] font-medium text-accent">
            {changes.length}
          </span>
        )}
      </button>
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
  const [scrollElement, setScrollElement] = useState<HTMLDivElement | null>(
    null,
  );
  const scrollRef = useCallback((element: HTMLDivElement | null) => {
    ref.current = element;
    setScrollElement(element);
  }, []);
  const pinned = useRef(true);
  const [showJump, setShowJump] = useState(false);

  // A different session always opens at its end.
  useEffect(() => {
    pinned.current = true;
  }, [transcript.session_id]);

  useEffect(() => {
    const el = ref.current;
    const content = el?.firstElementChild;
    if (!el || !content) return;
    const observer = new ResizeObserver(() => {
      if (pinned.current) el.scrollTop = el.scrollHeight;
    });
    observer.observe(content);
    return () => observer.disconnect();
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
      <div
        ref={scrollRef}
        role="region"
        aria-label="Conversation history"
        tabIndex={0}
        onKeyDown={(event) => {
          if (event.ctrlKey && event.key === "Home") {
            event.preventDefault();
            pinned.current = false;
            event.currentTarget.scrollTop = 0;
          } else if (event.ctrlKey && event.key === "End") {
            event.preventDefault();
            pinned.current = true;
            event.currentTarget.scrollTop = event.currentTarget.scrollHeight;
          } else if (["PageUp", "ArrowUp", "Home"].includes(event.key))
            pinned.current = false;
        }}
        onWheel={(event) => {
          if (event.deltaY < 0) pinned.current = false;
        }}
        onScroll={onScroll}
        className="h-full overflow-y-auto"
      >
        <div className="mx-auto w-full max-w-[860px] px-6 pb-6 pt-6">
          {transcript.loading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="h-4 w-4 animate-spin" /> Loading session…
            </div>
          ) : (
            <Transcript
              key={transcript.session_id}
              project={project}
              transcript={transcript}
              scrollElement={scrollElement}
            />
          )}
        </div>
      </div>
      {showJump && (
        <button
          onClick={() => {
            pinned.current = true;
            ref.current?.scrollTo({
              top: ref.current.scrollHeight,
              behavior: "smooth",
            });
          }}
          className="absolute bottom-3 left-1/2 flex -translate-x-1/2 items-center gap-1 rounded-full border border-line-strong bg-raised px-3 py-1 text-[12px] text-muted shadow-lg hover:text-fg"
        >
          <ArrowDown className="h-3.5 w-3.5" /> Latest
        </button>
      )}
    </div>
  );
}

/** Opens the goal panel; shows the goal's state and budget at a glance. */
function GoalChip({
  goal,
  open,
  toggle,
}: {
  goal: GoalSnapshot | null;
  open: boolean;
  toggle: () => void;
}) {
  const status = goal ? goalStatus(goal) : null;
  return (
    <button
      onClick={toggle}
      aria-pressed={open}
      title={goal ? `${goal.objective}\n${budgetSummary(goal)}` : "Set a goal (/goal)"}
      className={clsx(
        "flex h-7 max-w-[220px] items-center gap-1.5 rounded-md px-2 text-[12px]",
        open ? "bg-active text-fg" : "text-muted hover:bg-hover hover:text-fg",
      )}
    >
      <Target className="h-3.5 w-3.5 shrink-0" />
      {status ? (
        <span
          className={clsx(
            "shrink-0 rounded-full px-1.5 text-[10.5px] font-medium",
            TONE_CLASSES[status.tone],
          )}
        >
          {status.label}
        </span>
      ) : (
        "Goal"
      )}
    </button>
  );
}

/** An interrupted completion check holds the workspace until acknowledged. */
function CheckInterruptionBanner({ project }: { project: ProjectState }) {
  const [confirming, setConfirming] = useState(false);
  const hold = project.check_interruption;
  if (!hold) return null;
  return (
    <div
      role="alert"
      className="mb-2 flex items-start gap-3 rounded-md bg-warn-soft px-3 py-2 text-[12.5px] text-warn"
    >
      <span className="flex-1">
        A goal's completion check was interrupted and may have left changes
        behind. New work in this workspace waits until you review its effects
        and acknowledge it.
      </span>
      <button
        onClick={() => setConfirming(true)}
        className="shrink-0 rounded-md border border-warn/50 px-2 py-0.5 hover:bg-warn-soft"
      >
        Acknowledge…
      </button>
      {confirming && (
        <ConfirmDialog
          title="Acknowledge the interrupted check?"
          confirmLabel="I've resolved it"
          tone="primary"
          onConfirm={async () => {
            await api.acknowledgeCheckInterruption(project.project_id, hold.run_id);
            toast("Workspace released; resume the goal when ready");
          }}
          close={() => setConfirming(false)}
        >
          Confirm you have checked what the interrupted check changed (files,
          processes, anything external). This releases the workspace; it does
          not resume the goal.
        </ConfirmDialog>
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

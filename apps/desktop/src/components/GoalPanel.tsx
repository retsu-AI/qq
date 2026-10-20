import { useEffect, useState, type ReactNode } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  Circle,
  CircleDashed,
  CircleSlash,
  Pause,
  Pencil,
  Play,
  Target,
  Trash2,
  X,
} from "lucide-react";
import { api } from "../lib/api";
import { cost, tokens } from "../lib/format";
import {
  budgetSummary,
  duration,
  goalReason,
  goalStatus,
  needsCheckConsent,
  seen,
  type GoalAction,
  type GoalTone,
} from "../lib/goals";
import { closeGoalPanel, openGoalDialog } from "../lib/goal-ui";
import { toast } from "../lib/toast";
import type { GoalSnapshot, ProjectState, Transcript } from "../lib/types";
import { ConfirmDialog } from "./ConfirmDialog";

export const TONE_CLASSES: Record<GoalTone, string> = {
  active: "bg-accent-soft text-accent",
  attention: "bg-warn-soft text-warn",
  done: "bg-ok/15 text-ok",
  stopped: "bg-hover text-muted",
};

/** The session's goal: its state, progress and controls. */
export function GoalPanel({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const goal = transcript.goal;
  const [busy, setBusy] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  const act = async (action: GoalAction, done: string) => {
    if (!goal) return;
    setBusy(true);
    try {
      await api.goalControl(project.project_id, transcript.session_id, seen(goal), action);
      toast(done);
    } catch (error) {
      toast(String(error), "error");
    } finally {
      setBusy(false);
    }
  };
  const edit = (asNew: boolean) =>
    openGoalDialog({
      projectId: project.project_id,
      sessionId: transcript.session_id,
      edit: !asNew,
    });

  return (
    <aside
      aria-label="Goal"
      className="animate-slide-in flex w-[min(420px,40vw)] min-w-[300px] shrink-0 flex-col border-l border-line bg-sidebar max-[1100px]:absolute max-[1100px]:inset-y-0 max-[1100px]:right-0 max-[1100px]:z-20 max-[1100px]:w-[min(420px,85%)] max-[1100px]:shadow-2xl"
    >
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line pl-4 pr-2">
        <Target className="h-4 w-4 text-accent" />
        <h2 className="text-[13.5px] font-medium">Goal</h2>
        {goal && <StatusBadge goal={goal} />}
        <button
          onClick={closeGoalPanel}
          aria-label="Close goal"
          className="ml-auto rounded-md p-1.5 text-muted hover:bg-hover hover:text-fg"
        >
          <X className="h-4 w-4" />
        </button>
      </header>

      {!goal ? (
        <div className="px-5 py-8 text-center text-[13px] text-muted">
          <Target className="mx-auto mb-2 h-5 w-5 text-faint" />
          <p>No goal in this session.</p>
          <p className="mt-1 text-[12.5px] text-faint">
            A goal keeps QQ working across runs until it is achieved, checked by
            an optional command you approve, within a deadline and run budget.
          </p>
          {project.can_goal ? (
            <button
              onClick={() => edit(true)}
              className="mt-4 h-8 rounded-lg bg-solid px-3 text-[13px] font-medium text-white hover:bg-solid-hover"
            >
              Set a goal
            </button>
          ) : (
            <p className="mt-3 text-[12px] text-warn">
              This engine does not support goals.
            </p>
          )}
        </div>
      ) : (
        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 py-4 text-[13px]">
          <p className="whitespace-pre-wrap font-medium leading-relaxed selectable">
            {goal.objective}
          </p>

          {goalReason(goal.status) && (
            <p
              className={clsx(
                "rounded-lg px-3 py-2 text-[12.5px] selectable",
                goalStatus(goal).tone === "attention"
                  ? "bg-warn-soft text-warn"
                  : "bg-hover text-muted",
              )}
            >
              {goalReason(goal.status)}
            </p>
          )}

          {needsCheckConsent(goal) && goal.check && (
            <CheckConsent goal={goal} busy={busy} act={act} />
          )}

          <div className="flex flex-wrap gap-1.5">
            {goal.status.type === "active" ? (
              <Action icon={<Pause className="h-3.5 w-3.5" />} disabled={busy} onClick={() => void act({ type: "pause" }, "Goal paused")}>
                Pause
              </Action>
            ) : goal.status.type === "paused" ? (
              <Action icon={<Play className="h-3.5 w-3.5" />} disabled={busy} onClick={() => void act({ type: "resume" }, "Goal resumed")}>
                Resume
              </Action>
            ) : null}
            <Action icon={<Pencil className="h-3.5 w-3.5" />} disabled={busy} onClick={() => edit(goal.status.type === "achieved")}>
              {goal.status.type === "achieved" ? "New goal" : "Edit"}
            </Action>
            <Action icon={<Trash2 className="h-3.5 w-3.5" />} disabled={busy} onClick={() => setClearing(true)}>
              Clear
            </Action>
          </div>

          <Section title="Budget">
            <Fact label="Runs">
              {goal.counters.runs} of {goal.budget.max_goal_runs}
            </Fact>
            <Fact label="Deadline">
              {goal.budget.deadline_ms > now
                ? `${duration(goal.budget.deadline_ms - now)} left (${new Date(goal.budget.deadline_ms).toLocaleString()})`
                : "passed"}
            </Fact>
            <Fact label="Tokens">
              {goal.counters.usage
                ? tokens(
                    goal.counters.usage.input_tokens +
                      goal.counters.usage.cache_read_input_tokens +
                      goal.counters.usage.output_tokens,
                  )
                : "unknown"}
              {goal.budget.max_total_tokens ? ` of ${tokens(goal.budget.max_total_tokens)}` : ""}
            </Fact>
            <Fact label="Cost">
              {goal.counters.estimated_cost_usd_nanos == null
                ? "unknown"
                : cost(goal.counters.estimated_cost_usd_nanos) || "$0.00"}
              {goal.budget.max_cost_usd_nanos ? ` of ${cost(goal.budget.max_cost_usd_nanos)}` : ""}
            </Fact>
            {goal.wait && goal.wait.until_ms > now && (
              <Fact label="Waiting">
                {duration(goal.wait.until_ms - now)}: {goal.wait.reason}
              </Fact>
            )}
            {goal.status.type === "active" && !goal.pending_run_id && goal.next_run_at_ms > now && (
              <Fact label="Next run">in {duration(goal.next_run_at_ms - now)}</Fact>
            )}
          </Section>

          {goal.checklist.length > 0 && (
            <Section title="Checklist">
              <ul className="space-y-1">
                {goal.checklist.map((item) => (
                  <li key={item.id} className="flex items-start gap-2">
                    <ChecklistIcon state={item.state} />
                    <span
                      className={clsx(
                        "selectable",
                        item.state === "done" && "text-muted",
                        item.state === "dropped" && "text-faint line-through",
                      )}
                    >
                      {item.text}
                    </span>
                  </li>
                ))}
              </ul>
            </Section>
          )}

          {goal.notes && (
            <Section title="Notes">
              <p className="whitespace-pre-wrap text-muted selectable">{goal.notes}</p>
            </Section>
          )}

          <Section title="Completion check">
            {goal.check ? (
              <>
                <code className="block rounded-md bg-raised px-2 py-1 font-mono text-[12px] selectable">
                  {goal.check.command}
                </code>
                <p className="mt-1 text-[12px] text-faint">
                  {
                    {
                      pending: "Not approved yet.",
                      once: "Approved for one run.",
                      consumed: "The one-time approval was used; it needs consent again.",
                      for_goal: "Approved for every check of this goal.",
                    }[goal.check_authorization ?? "pending"]
                  }
                  {goal.counters.check_failures > 0 &&
                    ` Failed ${goal.counters.check_failures} time${goal.counters.check_failures === 1 ? "" : "s"}.`}
                </p>
                {goal.check_output && (
                  <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap rounded-md border border-line bg-raised px-2 py-1.5 font-mono text-[11.5px] text-muted selectable">
                    {goal.check_output}
                  </pre>
                )}
              </>
            ) : (
              <p className="text-[12.5px] text-faint">
                None. The goal is judged by an independent audit of the agent's
                claim instead.
              </p>
            )}
            {goal.audit_pending && (
              <p className="mt-1 text-[12px] text-accent">Auditing the achievement claim…</p>
            )}
          </Section>

          <p className="text-[11.5px] text-faint">
            {budgetSummary(goal, now)} · revision {goal.revision}
          </p>
        </div>
      )}

      {clearing && goal && (
        <ConfirmDialog
          title="Clear this goal?"
          confirmLabel="Clear goal"
          onConfirm={async () => {
            await api.goalControl(project.project_id, transcript.session_id, seen(goal), { type: "clear" });
            toast("Goal cleared");
          }}
          close={() => setClearing(false)}
        >
          QQ stops working toward “{goal.objective}”. The session and its history
          stay.
        </ConfirmDialog>
      )}
    </aside>
  );
}

/** Consent for exactly the displayed check command. Deny is the default. */
function CheckConsent({
  goal,
  busy,
  act,
}: {
  goal: GoalSnapshot;
  busy: boolean;
  act: (action: GoalAction, done: string) => Promise<void>;
}) {
  const command = goal.check!.command;
  return (
    <div className="rounded-lg border border-warn/40 bg-warn-soft/50 px-3 py-3">
      <p className="text-[12.5px] font-medium text-warn">Allow this completion check to run?</p>
      <code className="mt-2 block rounded-md bg-raised px-2 py-1 font-mono text-[12px] text-fg selectable">
        {command}
      </code>
      <p className="mt-1.5 text-[11.5px] text-muted">
        It runs in this workspace with your permissions. Changing the goal or
        command needs consent again.
      </p>
      <div className="mt-2.5 flex flex-wrap gap-1.5">
        <Action disabled={busy} onClick={() => void act({ type: "deny_check" }, "Check denied")}>
          Deny
        </Action>
        <Action
          disabled={busy}
          onClick={() => void act({ type: "approve_check", scope: "once", command }, "Check allowed once")}
        >
          Allow once
        </Action>
        <Action
          disabled={busy}
          onClick={() =>
            void act({ type: "approve_check", scope: "for_goal", command }, "Check allowed for this goal")
          }
        >
          Allow for this goal
        </Action>
      </div>
    </div>
  );
}

export function StatusBadge({ goal }: { goal: GoalSnapshot }) {
  const { label, tone } = goalStatus(goal);
  return (
    <span className={clsx("rounded-full px-2 py-0.5 text-[11px] font-medium", TONE_CLASSES[tone])}>
      {label}
    </span>
  );
}

function ChecklistIcon({ state }: { state: string }) {
  const cls = "mt-[3px] h-3.5 w-3.5 shrink-0";
  if (state === "done") return <CheckCircle2 className={clsx(cls, "text-ok")} />;
  if (state === "in_progress") return <CircleDashed className={clsx(cls, "text-accent")} />;
  if (state === "dropped") return <CircleSlash className={clsx(cls, "text-faint")} />;
  return <Circle className={clsx(cls, "text-faint")} />;
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <h3 className="mb-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
        {title}
      </h3>
      {children}
    </section>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex gap-3 py-0.5 text-[12.5px]">
      <span className="w-20 shrink-0 text-muted">{label}</span>
      <span className="min-w-0 selectable">{children}</span>
    </div>
  );
}

function Action({
  icon,
  disabled,
  onClick,
  children,
}: {
  icon?: ReactNode;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className="flex h-7 items-center gap-1.5 rounded-md border border-line-strong px-2.5 text-[12.5px] text-fg hover:bg-hover disabled:opacity-50"
    >
      {icon}
      {children}
    </button>
  );
}

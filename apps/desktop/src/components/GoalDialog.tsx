import { useEffect, useState } from "react";
import clsx from "clsx";
import { api } from "../lib/api";
import type { GoalDefaultsDto } from "../lib/generated";
import { presetLabel, seen, type BudgetPreset } from "../lib/goals";
import { openGoalPanel } from "../lib/goal-ui";
import { toast } from "../lib/toast";
import type { GoalSnapshot } from "../lib/types";
import { FormDialog, fieldClass } from "./FormDialog";

/**
 * Sets, edits or replaces a session goal. The form captures the goal
 * revision it opened with; a goal that changed meanwhile is refused by the
 * server and must be reviewed again. Setting a check does not consent to it.
 */
export function GoalDialog({
  projectId,
  sessionId,
  current,
  initialObjective,
  edit,
  close,
}: {
  projectId: string;
  sessionId: string;
  /** The session's goal when the dialog opened. */
  current: GoalSnapshot | null;
  initialObjective?: string;
  edit: boolean;
  close: () => void;
}) {
  const editing = edit && current !== null && current.status.type !== "achieved";
  const replacing = current !== null && !editing;
  const [opened] = useState(current);
  const [objective, setObjective] = useState(
    initialObjective ?? (editing ? current.objective : ""),
  );
  const [check, setCheck] = useState(editing ? (current.check?.command ?? "") : "");
  const [preset, setPreset] = useState<BudgetPreset>(editing ? "keep" : "defaults");
  const [confirmReplace, setConfirmReplace] = useState(false);
  const [defaults, setDefaults] = useState<GoalDefaultsDto | null>(null);
  const [defaultsError, setDefaultsError] = useState<string | null>(null);

  useEffect(() => {
    api
      .goalDefaults(projectId)
      .then(setDefaults)
      .catch((error) => setDefaultsError(String(error)));
  }, [projectId]);

  const presets: BudgetPreset[] = editing
    ? ["keep", "defaults", "focused", "extended"]
    : ["defaults", "focused", "extended"];

  return (
    <FormDialog
      title={editing ? "Edit goal" : replacing ? "Replace goal" : "Set a goal"}
      submitLabel={editing ? "Save goal" : replacing ? "Replace goal" : "Start goal"}
      close={close}
      submit={async () => {
        if (replacing && !confirmReplace)
          throw new Error("Confirm that this replaces the current goal.");
        await api.setGoal(projectId, sessionId, {
          objective,
          check: check.trim() || null,
          preset,
          expected: opened ? seen(opened) : null,
          replace: replacing,
        });
        toast(editing ? "Goal updated" : "Goal started");
        openGoalPanel(sessionId);
      }}
    >
      <p className="text-muted">
        QQ keeps working toward the goal across runs until it is achieved, a
        budget runs out, or it needs you. Editing a stopped goal does not resume
        it.
      </p>
      <label className="block">
        <span className="mb-1 block text-[12px] text-muted">Objective</span>
        <textarea
          autoFocus
          required
          rows={4}
          maxLength={2048}
          value={objective}
          onChange={(event) => setObjective(event.target.value)}
          placeholder="What does done look like? Be specific."
          className={clsx(fieldClass, "resize-y")}
        />
      </label>
      <label className="block">
        <span className="mb-1 block text-[12px] text-muted">
          Completion check (optional)
        </span>
        <input
          maxLength={1024}
          value={check}
          onChange={(event) => setCheck(event.target.value)}
          placeholder="A command that succeeds when the goal is met, e.g. cargo test"
          className={clsx(fieldClass, "font-mono")}
        />
        <span className="mt-1 block text-[11.5px] text-faint">
          It runs only after you consent to this exact command; ordinary
          approvals never cover it.
        </span>
      </label>
      <fieldset>
        <legend className="mb-1 text-[12px] text-muted">Budget</legend>
        <div className="space-y-1">
          {presets.map((value) => (
            <label
              key={value}
              className={clsx(
                "flex cursor-pointer items-center gap-2 rounded-lg border px-3 py-2",
                preset === value ? "border-accent/60 bg-accent-soft" : "border-line hover:bg-hover",
              )}
            >
              <input
                type="radio"
                name="goal-budget"
                checked={preset === value}
                onChange={() => setPreset(value)}
                className="accent-[var(--accent)]"
              />
              <span>{presetLabel(value, defaults)}</span>
            </label>
          ))}
        </div>
        {defaultsError && (
          <p className="mt-1 text-[12px] text-warn selectable">
            Could not read this workspace's goal settings: {defaultsError}
          </p>
        )}
      </fieldset>
      {replacing && current && (
        <label className="flex items-start gap-2 rounded-lg border border-warn/40 bg-warn-soft px-3 py-2 text-warn">
          <input
            type="checkbox"
            checked={confirmReplace}
            onChange={(event) => setConfirmReplace(event.target.checked)}
            className="mt-0.5 accent-[var(--warn)]"
          />
          <span>
            Replace the current goal: <span className="font-medium">“{current.objective}”</span>
          </span>
        </label>
      )}
    </FormDialog>
  );
}

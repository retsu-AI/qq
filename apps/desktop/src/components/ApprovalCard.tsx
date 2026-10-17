import { useEffect, useRef, useState } from "react";
import { approvalShortcut } from "../lib/approval-shortcuts";
import clsx from "clsx";
import {
  CircleAlert,
  Globe,
  MessageCircleQuestion,
  TerminalSquare,
} from "lucide-react";
import { api } from "../lib/api";
import { toolSubject, toolVerb } from "../lib/tools";
import type { ApprovalChoice, Tool } from "../lib/types";
import { Diff } from "./Diff";

/**
 * A held tool call. Keyboard (when focus is not in a text field):
 * Y approve once · S for session · W for workspace · N deny.
 */
export function ApprovalCard({
  tool,
  projectId,
  sessionId,
}: {
  tool: Tool;
  projectId: string;
  sessionId: string;
}) {
  const approval = tool.approval!;
  const [sent, setSent] = useState(false);
  const card = useRef<HTMLDivElement>(null);

  const respond = (decision: ApprovalChoice) => {
    if (sent) return;
    setSent(true);
    void api
      .respondApproval(projectId, sessionId, tool.id, decision)
      .catch(() => setSent(false));
  };

  useEffect(() => {
    if (approval.question) return;
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const selected =
        target?.closest("[data-approval-shortcuts]") ??
        document.querySelector("[data-approval-shortcuts]");
      const decision = approvalShortcut(event.key, {
        blocked:
          Boolean(document.querySelector("dialog[open]")) ||
          event.defaultPrevented ||
          event.repeat ||
          event.isComposing ||
          event.ctrlKey ||
          event.altKey ||
          event.metaKey ||
          Boolean(
            target?.closest("input, textarea, select, [contenteditable=true]"),
          ) ||
          selected !== card.current ||
          approval.shell?.verdict === "forbidden",
        grantable: approval.grantable,
      });
      if (decision) {
        event.preventDefault();
        respond(decision);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  if (approval.question) {
    return (
      <QuestionCard
        questions={approval.question.questions}
        disabled={sent}
        onAnswer={(answers) => respond({ type: "answer", answers })}
      />
    );
  }

  const shell = approval.shell;
  const forbidden = shell?.verdict === "forbidden";

  return (
    <div
      ref={card}
      data-approval-shortcuts
      className="overflow-hidden rounded-xl border border-warn/40 bg-warn-soft"
    >
      <div className="flex items-center gap-2 px-4 pt-3 text-[13px]">
        <CircleAlert className="h-4 w-4 text-warn" />
        <span className="font-medium">
          {shell
            ? "Run this command?"
            : approval.edit
              ? "Apply this edit?"
              : approval.fetch
                ? "Allow this request?"
                : `Allow ${toolVerb(tool.name).toLowerCase()}?`}
        </span>
      </div>

      <div className="space-y-2 px-4 py-3">
        {shell && (
          <div className="overflow-hidden rounded-lg border border-line bg-bg">
            <div className="flex items-start gap-2 px-3 py-2">
              <TerminalSquare className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted" />
              <code className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[12.5px] selectable">
                {shell.command}
              </code>
            </div>
            {(shell.cwd || (shell.reasons && shell.reasons.length > 0)) && (
              <div className="border-t border-line px-3 py-1.5 text-[11.5px] text-faint">
                {shell.cwd && <span>in {shell.cwd}</span>}
                {shell.reasons && shell.reasons.length > 0 && (
                  <span>
                    {shell.cwd ? " · " : ""}
                    {shell.reasons.join(", ")}
                  </span>
                )}
              </div>
            )}
          </div>
        )}
        {approval.edit && (
          <Diff path={approval.edit.path} diff={approval.edit.diff} />
        )}
        {approval.fetch && (
          <div className="flex items-center gap-2 rounded-lg border border-line bg-bg px-3 py-2 text-[12.5px]">
            <Globe className="h-3.5 w-3.5 text-muted" />
            <span className="font-mono text-muted">
              {approval.fetch.method ?? "GET"}
            </span>
            <span className="truncate font-mono selectable">
              {approval.fetch.url}
            </span>
          </div>
        )}
        {!shell && !approval.edit && !approval.fetch && (
          <div className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12.5px] selectable">
            {tool.name} {toolSubject(tool.name, tool.arguments)}
          </div>
        )}
        {approval.escalation && (
          <p className="text-[12px] text-muted">
            Reviewer: {approval.escalation}
          </p>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-2 border-t border-warn/20 px-4 py-2.5">
        <ActionButton
          primary
          disabled={sent || forbidden}
          onClick={() => respond({ type: "once" })}
          hint="Y"
        >
          Approve
        </ActionButton>
        {approval.grantable && !forbidden && (
          <>
            <ActionButton
              disabled={sent}
              onClick={() => respond({ type: "session" })}
              hint="S"
            >
              Always this session
            </ActionButton>
            <ActionButton
              disabled={sent}
              onClick={() => respond({ type: "workspace" })}
              hint="W"
            >
              Always in workspace
            </ActionButton>
          </>
        )}
        <div className="flex-1" />
        <ActionButton
          danger
          disabled={sent}
          onClick={() => respond({ type: "deny" })}
          hint="N"
        >
          Deny
        </ActionButton>
      </div>
      {approval.grantable && !forbidden && (
        <p className="px-4 pb-2.5 text-[11.5px] text-faint">
          "Always" allows {approval.grant_label}.
        </p>
      )}
    </div>
  );
}

function ActionButton({
  children,
  onClick,
  disabled,
  primary,
  danger,
  hint,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  primary?: boolean;
  danger?: boolean;
  hint?: string;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className={clsx(
        "flex h-8 items-center gap-2 rounded-lg px-3 text-[12.5px] font-medium disabled:opacity-50",
        primary && "bg-solid text-white hover:bg-solid-hover",
        danger && "border border-line-strong text-danger hover:bg-danger-soft",
        !primary && !danger && "border border-line-strong bg-bg hover:bg-hover",
      )}
    >
      {children}
      {hint && (
        <kbd
          className={clsx(
            "font-mono text-[10.5px]",
            primary ? "text-white/70" : "text-faint",
          )}
        >
          {hint}
        </kbd>
      )}
    </button>
  );
}

function QuestionCard({
  questions,
  disabled,
  onAnswer,
}: {
  questions: Array<{ prompt: string; options: string[]; free_text?: boolean }>;
  disabled: boolean;
  onAnswer: (answers: string[]) => void;
}) {
  const [answers, setAnswers] = useState<string[]>(() =>
    questions.map(() => ""),
  );
  const complete = answers.every((answer) => answer.trim());
  const setAnswer = (index: number, value: string) =>
    setAnswers((current) => current.map((a, i) => (i === index ? value : a)));

  return (
    <div className="overflow-hidden rounded-xl border border-accent/40 bg-accent-soft">
      <div className="flex items-center gap-2 px-4 pt-3 text-[13px] font-medium">
        <MessageCircleQuestion className="h-4 w-4 text-accent" />
        The agent has a question
      </div>
      <div className="space-y-4 px-4 py-3">
        {questions.map((question, index) => (
          <div key={index}>
            <p className="mb-2 selectable">{question.prompt}</p>
            <div className="flex flex-wrap gap-2">
              {question.options.map((option) => (
                <button
                  key={option}
                  disabled={disabled}
                  onClick={() => setAnswer(index, option)}
                  className={clsx(
                    "rounded-lg border px-3 py-1.5 text-[12.5px]",
                    answers[index] === option
                      ? "border-[var(--solid)] bg-solid text-white"
                      : "border-line-strong bg-bg hover:bg-hover",
                  )}
                >
                  {option}
                </button>
              ))}
            </div>
            {(question.free_text || question.options.length === 0) && (
              <input
                disabled={disabled}
                value={
                  question.options.includes(answers[index])
                    ? ""
                    : answers[index]
                }
                onChange={(event) => setAnswer(index, event.target.value)}
                placeholder="Type an answer…"
                className="mt-2 w-full rounded-lg border border-line-strong bg-bg px-3 py-1.5 text-[13px] outline-none focus:border-accent"
              />
            )}
          </div>
        ))}
      </div>
      <div className="flex gap-2 border-t border-accent/20 px-4 py-2.5">
        <ActionButton
          primary
          disabled={disabled || !complete}
          onClick={() => onAnswer(answers)}
        >
          Send answer
        </ActionButton>
        <ActionButton disabled={disabled} onClick={() => onAnswer([])}>
          Skip
        </ActionButton>
      </div>
    </div>
  );
}

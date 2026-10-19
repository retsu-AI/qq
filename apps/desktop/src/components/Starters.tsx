import type { ReactNode } from "react";
import { Bug, FileSearch, FlaskConical, GitCompare } from "lucide-react";
import { drafts } from "../lib/drafts";

const STARTERS: Array<{ icon: ReactNode; title: string; prompt: string }> = [
  {
    icon: <FileSearch className="h-4 w-4" />,
    title: "Explain this project",
    prompt:
      "Give me a tour of this project: what it does, how it is organized, and where to start reading.",
  },
  {
    icon: <GitCompare className="h-4 w-4" />,
    title: "Review my changes",
    prompt:
      "Review @diff for bugs, risky changes and missing tests. List findings by severity.",
  },
  {
    icon: <Bug className="h-4 w-4" />,
    title: "Fix a bug",
    prompt: "Find and fix the cause of this bug: ",
  },
  {
    icon: <FlaskConical className="h-4 w-4" />,
    title: "Add tests",
    prompt: "Add focused tests for @",
  },
];

/**
 * Prompts that start an empty session. Choosing one fills the composer for
 * editing; nothing is sent until the user sends it.
 */
export function Starters({ draftKey }: { draftKey: string }) {
  const choose = (prompt: string) => {
    drafts.set(draftKey, prompt);
    requestAnimationFrame(() => {
      const composer = document.querySelector<HTMLTextAreaElement>(
        'textarea[aria-label="Message"]',
      );
      if (!composer) return;
      // The caret is already at the end of the new value, so the composer's
      // focus handler sees a trailing `@` and opens the file picker.
      composer.focus();
      composer.setSelectionRange(prompt.length, prompt.length);
    });
  };
  return (
    <ul
      aria-label="Suggestions"
      className="grid grid-cols-1 gap-2 sm:grid-cols-2"
    >
      {STARTERS.map((starter) => (
        <li key={starter.title}>
          <button
            type="button"
            onClick={() => choose(starter.prompt)}
            className="animate-pop-up flex h-full w-full items-start gap-3 rounded-xl border border-line px-3.5 py-3 text-left hover:border-line-strong hover:bg-hover"
          >
            <span className="mt-0.5 text-accent">{starter.icon}</span>
            <span className="min-w-0">
              <span className="block text-[13px] font-medium">
                {starter.title}
              </span>
              <span className="mt-0.5 line-clamp-2 block text-[12px] text-muted">
                {starter.prompt}
              </span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

// `/` completion and side-question commands in the composer. A leading
// `/name` is sent as an ordinary prompt and resolved by the runtime to a
// workspace command or skill; `/btw`, `/ask`, `/btw-new` and `/btw-cancel`
// are handled by the desktop, as in the TUI.

import type { ActiveMention, Suggestion } from "./mentions";

export interface SlashSkill {
  name: string;
  kind: "command" | "skill";
  description: string;
}

/** The `/name` being typed at the start of the message, before any space. */
export function findSlash(text: string, caret: number): ActiveMention | null {
  const match = /^\/([\w.:-]*)$/.exec(text.slice(0, caret));
  return match ? { start: 0, query: match[1] } : null;
}

const MAX_SLASH_SUGGESTIONS = 50;

export function slashSuggestions(
  query: string,
  skills: SlashSkill[],
  side: { available: boolean; running: boolean },
  goals = false,
): Suggestion[] {
  const builtins: Suggestion[] = side.available
    ? [
        { value: "btw", kind: "builtin", hint: "Ask beside the run without interrupting it" },
        { value: "btw-new", kind: "builtin", hint: "Ask in a new side thread" },
        ...(side.running
          ? [
              {
                value: "btw-cancel",
                kind: "builtin" as const,
                hint: "Cancel the running side question",
              },
            ]
          : []),
      ]
    : [];
  const goalCommands: Suggestion[] = goals
    ? [
        { value: "goal", kind: "builtin", hint: "Show the goal, or /goal <objective> to set one" },
        { value: "goal edit", kind: "builtin", hint: "Edit the current goal" },
        { value: "goal pause", kind: "builtin", hint: "Pause the goal" },
        { value: "goal resume", kind: "builtin", hint: "Resume the goal" },
      ]
    : [];
  const entries: Suggestion[] = [
    ...builtins,
    ...goalCommands,
    ...skills.map<Suggestion>((skill) => ({
      value: skill.name.replace(/^\//, ""),
      kind: skill.kind,
      hint: skill.description,
    })),
  ];
  const lower = query.toLowerCase();
  const starts = entries.filter((entry) => entry.value.toLowerCase().startsWith(lower));
  const contains = entries.filter(
    (entry) =>
      !entry.value.toLowerCase().startsWith(lower) &&
      entry.value.toLowerCase().includes(lower),
  );
  return [...starts, ...contains].slice(0, MAX_SLASH_SUGGESTIONS);
}

/** Replaces the typed `/query` with `/value ` and puts the caret after it. */
export function insertSlash(
  text: string,
  caret: number,
  value: string,
): { text: string; caret: number } {
  const rest = text.slice(caret).replace(/^\S*/, "").replace(/^\s*/, "");
  const head = `/${value} `;
  return { text: head + rest, caret: head.length };
}

export type SideCommand =
  | { kind: "ask"; question: string; newThread: boolean }
  /** `/btw` alone: show the side questions. */
  | { kind: "show" }
  | { kind: "cancel" };

/** A message that is a side-question command, or null for a normal prompt. */
export function parseSideCommand(text: string): SideCommand | null {
  const match = /^\/(btw|ask|btw-new|btw-cancel)(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!match) return null;
  const question = (match[2] ?? "").trim();
  if (match[1] === "btw-cancel") return { kind: "cancel" };
  if (!question) return { kind: "show" };
  return { kind: "ask", question, newThread: match[1] === "btw-new" };
}

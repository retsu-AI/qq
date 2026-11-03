import { memo, useMemo, type ReactNode } from "react";
import clsx from "clsx";
import { highlight, languageForPath } from "../lib/highlight";
import { parseToolOutput, type OutputHeader, type Tone } from "../lib/tool-output";
import { CopyButton } from "./CodeBlock";

/** Lines highlighted per output; past this the rest renders plain. */
const MAX_HIGHLIGHTED_LINES = 400;

const TONES: Record<Tone, string> = {
  error: "text-danger",
  warn: "text-warn",
  ok: "text-ok",
};

/**
 * A tool's result as a terminal-style block: a header (the shell prompt and
 * command, or the tool and its subject) with its exit code, line range and
 * truncation, then the body with line numbers, file headings, search hits
 * and colored command output.
 */
export const ToolOutput = memo(function ToolOutput({
  text,
  error,
  command,
}: {
  /** The tool's result text; null when a command printed nothing. */
  text: string | null;
  error: boolean;
  /** Shell commands show their command line as a prompt. */
  command?: string;
}) {
  const { header, lines } = useMemo(() => parseToolOutput(text ?? ""), [text]);
  const language =
    header?.tool === "read" ? languageForPath(header.subject) : null;
  const query =
    header?.tool === "search" ? header.subject.replace(/^"|"$/g, "") : "";
  const digits = Math.max(
    2,
    ...lines.map((line) =>
      line.kind === "code" || line.kind === "match" ? line.number.length : 0,
    ),
  );
  const empty = lines.every((line) => line.kind === "text" && !line.text);
  let highlighted = 0;

  return (
    <div className="overflow-hidden rounded-lg border border-line bg-raised font-mono text-small leading-[1.55]">
      {command !== undefined ? (
        <div className="group/command relative flex items-start gap-2 px-3 py-2">
          <span className="select-none font-semibold text-ok">❯</span>
          <span className="min-w-0 flex-1 whitespace-pre-wrap break-all text-fg select-text">
            {command}
          </span>
          <Fields header={header} error={error} />
          <CopyButton
            text={command}
            label="Copy command"
            className="-my-0.5 opacity-0 focus-visible:opacity-100 group-hover/command:opacity-100"
          />
        </div>
      ) : (
        header && (
          <div className="flex items-center gap-2 px-3 py-1.5">
            <span className="shrink-0 text-accent">{header.tool}</span>
            <span className="min-w-0 flex-1 truncate text-muted" title={header.subject}>
              {header.subject}
            </span>
            <Fields header={header} error={error} />
          </div>
        )
      )}
      {!empty && (
        <div
          className={clsx(
            "max-h-[320px] overflow-auto py-1.5 select-text",
            (command !== undefined || header) && "border-t border-line",
          )}
        >
          {lines.map((line, index) => {
            switch (line.kind) {
              case "code":
              case "match": {
                const marked =
                  line.kind === "code" && highlighted++ < MAX_HIGHLIGHTED_LINES
                    ? highlight(line.text, language)
                    : null;
                return (
                  <div key={index} className="flex">
                    <span
                      className="box-content shrink-0 select-none px-3 text-right text-faint"
                      style={{ width: `${digits}ch` }}
                    >
                      {line.number}
                    </span>
                    {marked ? (
                      // highlight.js escapes the source; only its own spans are markup.
                      <span
                        className="hljs min-w-0 whitespace-pre-wrap break-words pr-3"
                        dangerouslySetInnerHTML={{ __html: marked }}
                      />
                    ) : (
                      <span className="min-w-0 whitespace-pre-wrap break-words pr-3 text-fg">
                        {line.kind === "match" ? marks(line.text, query) : line.text || " "}
                      </span>
                    )}
                  </div>
                );
              }
              case "path":
                return (
                  <div
                    key={index}
                    className="px-3 pt-1.5 font-medium text-accent first:pt-0"
                  >
                    {line.text}
                  </div>
                );
              case "gap":
                return (
                  <div
                    key={index}
                    className="mx-3 my-1 border-t border-dashed border-line-strong"
                  />
                );
              case "note":
                return (
                  <div key={index} className="px-3 italic text-faint">
                    {line.text}
                  </div>
                );
              case "text":
                return (
                  <div
                    key={index}
                    className={clsx(
                      "whitespace-pre-wrap break-words px-3",
                      line.tone
                        ? TONES[line.tone]
                        : error
                          ? "text-danger"
                          : "text-muted",
                    )}
                  >
                    {line.text || " "}
                  </div>
                );
            }
          })}
        </div>
      )}
    </div>
  );
});

/** The header fields worth a glance: exit code, range, counts, truncation. */
function Fields({
  header,
  error,
}: {
  header: OutputHeader | null;
  error: boolean;
}) {
  const chips: ReactNode[] = [];
  for (const [key, value] of header?.fields ?? []) {
    if (key === "exit") {
      const ok = value === "0" && !error;
      chips.push(
        <Chip key={key} className={ok ? "bg-ok/10 text-ok" : "bg-danger-soft text-danger"}>
          {ok ? "✓ exit 0" : `exit ${value}`}
        </Chip>,
      );
    } else if (key === "elapsed") {
      chips.push(<Chip key={key}>{`${value}s`}</Chip>);
    } else if (key === "lines") {
      chips.push(<Chip key={key}>{`L${value}`}</Chip>);
    } else if (["matches", "files", "entries"].includes(key)) {
      chips.push(<Chip key={key}>{`${value} ${key}`}</Chip>);
    } else if (key === "truncated" || key === "partial") {
      chips.push(
        <Chip key={key} className="bg-warn-soft text-warn">
          {key}
        </Chip>,
      );
    }
  }
  return chips.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chips}</span>
  ) : null;
}

function Chip({
  children,
  className = "bg-hover text-faint",
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <span className={clsx("whitespace-nowrap rounded px-1.5 text-caption", className)}>
      {children}
    </span>
  );
}

/** `text` with case-insensitive occurrences of `query` marked. */
function marks(text: string, query: string): ReactNode {
  if (!query) return text || " ";
  const lower = text.toLowerCase();
  const needle = query.toLowerCase();
  const parts: ReactNode[] = [];
  let at = 0;
  for (let found = lower.indexOf(needle); found >= 0; found = lower.indexOf(needle, at)) {
    if (found > at) parts.push(text.slice(at, found));
    parts.push(
      <mark key={found} className="rounded-sm bg-warn-soft text-warn">
        {text.slice(found, found + needle.length)}
      </mark>,
    );
    at = found + needle.length;
  }
  parts.push(text.slice(at));
  return parts;
}

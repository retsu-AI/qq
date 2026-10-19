import { memo, useMemo } from "react";
import clsx from "clsx";
import { FileDiff } from "lucide-react";
import { diffStats, parseDiff } from "../lib/changes";
import { highlight, languageForPath } from "../lib/highlight";

/** Lines highlighted per diff; past this the diff renders plain. */
const MAX_HIGHLIGHT_LINES = 800;

/** Unified diff with line numbers, per-line tinting and a +/− summary. */
export const Diff = memo(function Diff({
  path,
  diff,
  maxHeight = 360,
  header = true,
}: {
  path: string;
  diff: string;
  /** Null lets the diff grow with its container. */
  maxHeight?: number | null;
  /** Show the path and +/− header; the Changes panel draws its own. */
  header?: boolean;
}) {
  const lines = useMemo(() => parseDiff(diff), [diff]);
  const stats = useMemo(() => diffStats(diff), [diff]);
  const language = languageForPath(path);
  const html = useMemo(
    () =>
      lines.length > MAX_HIGHLIGHT_LINES
        ? null
        : lines.map((line) =>
            line.kind === "hunk" || line.kind === "note"
              ? null
              : highlight(line.text, language),
          ),
    [lines, language],
  );
  return (
    <div className="overflow-hidden rounded-lg border border-line">
      {header && (
        <div className="flex items-center gap-2 border-b border-line bg-raised px-3 py-1.5 text-[12px]">
          <FileDiff className="h-3.5 w-3.5 shrink-0 text-muted" />
          <span className="truncate font-mono" title={path}>
            {path}
          </span>
          <span className="ml-auto font-mono text-ok">+{stats.added}</span>
          <span className="font-mono text-danger">−{stats.removed}</span>
        </div>
      )}
      <div
        className="overflow-auto py-1 font-mono text-[12px] leading-[1.55] selectable"
        style={maxHeight === null ? undefined : { maxHeight }}
      >
        <div className="min-w-max">
          {lines.map((line, index) => {
            if (line.kind === "hunk" || line.kind === "note")
              return (
                <div
                  key={index}
                  className={clsx(
                    "whitespace-pre px-3 text-faint",
                    line.kind === "hunk" && index > 0 && "mt-1 border-t border-line pt-1",
                  )}
                >
                  {line.text}
                </div>
              );
            const marked = html?.[index];
            return (
              <div
                key={index}
                className={clsx(
                  "flex whitespace-pre",
                  line.kind === "add" && "bg-[var(--diff-add)]",
                  line.kind === "del" && "bg-[var(--diff-del)]",
                )}
              >
                <span className="w-10 shrink-0 select-none pr-2 text-right text-faint/70">
                  {line.old ?? ""}
                </span>
                <span className="w-10 shrink-0 select-none pr-2 text-right text-faint/70">
                  {line.new ?? ""}
                </span>
                <span
                  className={clsx(
                    "w-4 shrink-0 select-none text-center",
                    line.kind === "add" && "text-ok",
                    line.kind === "del" && "text-danger",
                  )}
                >
                  {line.kind === "add" ? "+" : line.kind === "del" ? "−" : ""}
                </span>
                {marked ? (
                  // highlight.js escapes the source; only its own spans are markup.
                  <span
                    className="hljs pr-3"
                    dangerouslySetInnerHTML={{ __html: marked }}
                  />
                ) : (
                  <span className="pr-3">{line.text || " "}</span>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
});

import clsx from "clsx";
import { FileDiff } from "lucide-react";

/** Unified diff with per-line tinting and a +/− summary. */
export function Diff({ path, diff, maxHeight = 360 }: { path: string; diff: string; maxHeight?: number }) {
  const lines = diff.replace(/\n$/, "").split("\n");
  let added = 0;
  let removed = 0;
  for (const line of lines) {
    if (line.startsWith("+") && !line.startsWith("+++")) added++;
    else if (line.startsWith("-") && !line.startsWith("---")) removed++;
  }
  return (
    <div className="overflow-hidden rounded-lg border border-line">
      <div className="flex items-center gap-2 border-b border-line bg-raised px-3 py-1.5 text-[12px]">
        <FileDiff className="h-3.5 w-3.5 text-muted" />
        <span className="truncate font-mono">{path}</span>
        <span className="ml-auto font-mono text-ok">+{added}</span>
        <span className="font-mono text-danger">−{removed}</span>
      </div>
      <pre
        className="overflow-auto py-1 font-mono text-[12px] leading-[1.55] selectable"
        style={{ maxHeight }}
      >
        {lines.map((line, index) => {
          if (line.startsWith("+++") || line.startsWith("---")) return null;
          const kind = line.startsWith("@@")
            ? "hunk"
            : line.startsWith("+")
              ? "add"
              : line.startsWith("-")
                ? "del"
                : "ctx";
          return (
            <div
              key={index}
              className={clsx(
                "px-3 whitespace-pre",
                kind === "add" && "bg-[var(--diff-add)]",
                kind === "del" && "bg-[var(--diff-del)]",
                kind === "hunk" && "text-faint",
              )}
            >
              {line || " "}
            </div>
          );
        })}
      </pre>
    </div>
  );
}

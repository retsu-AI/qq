import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import clsx from "clsx";
import { Check, Copy } from "lucide-react";
import { highlight } from "../lib/highlight";

/** A fenced block: language label, copy button and highlighted code. */
export function CodeBlock({
  code,
  language,
}: {
  code: string;
  language: string | null;
}) {
  const html = useMemo(() => highlight(code, language), [code, language]);
  return (
    <div className="code-block group/code">
      <div className="flex h-8 items-center gap-2 border-b border-line pl-3 pr-1.5 text-[11.5px] text-faint">
        <span className="font-mono">{language ?? "text"}</span>
        <CopyButton
          text={code}
          label="Copy code"
          className="ml-auto opacity-70 group-hover/code:opacity-100"
        >
          Copy
        </CopyButton>
      </div>
      <pre>
        {html === null ? (
          <code>{code}</code>
        ) : (
          // highlight.js escapes the source; only its own spans are markup.
          <code className="hljs" dangerouslySetInnerHTML={{ __html: html }} />
        )}
      </pre>
    </div>
  );
}

/** Copies `text`, then confirms with a check for a moment. */
export function CopyButton({
  text,
  label,
  className,
  children,
}: {
  text: string;
  label: string;
  className?: string;
  children?: ReactNode;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };
  return (
    <button
      type="button"
      onClick={() => void copy()}
      aria-label={copied ? "Copied" : label}
      title={copied ? "Copied" : label}
      className={clsx(
        "flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-muted hover:bg-hover hover:text-fg",
        className,
      )}
    >
      {copied ? (
        <Check className="h-3.5 w-3.5 text-ok" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
      {children && <span>{copied ? "Copied" : children}</span>}
    </button>
  );
}

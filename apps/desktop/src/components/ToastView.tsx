import clsx from "clsx";
import { X } from "lucide-react";
import { dismissToast, useToast } from "../lib/toast";

export function ToastView() {
  const toast = useToast();
  if (!toast) return null;
  return (
    // The wrapper centers; the animated box must not carry that transform.
    <div className="pointer-events-none fixed inset-x-0 bottom-6 z-50 flex justify-center px-4">
      <div
        key={toast.id}
        role={toast.tone === "error" ? "alert" : "status"}
        className={clsx(
          "animate-pop-up pointer-events-auto flex max-w-[560px] items-center gap-3 rounded-xl border bg-raised px-4 py-2.5 text-[13px] shadow-popover",
          toast.tone === "error"
            ? "border-danger/40 text-danger"
            : "border-line-strong text-fg",
        )}
      >
        <span className="selectable">{toast.text}</span>
        <button
          aria-label="Dismiss"
          onClick={dismissToast}
          className="text-faint hover:text-fg"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}

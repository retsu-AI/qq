import { useEffect, useRef, useState, type ReactNode } from "react";
import clsx from "clsx";

/** A themed replacement for `confirm()`, whose dialog Windows draws unstyled. */
export function ConfirmDialog({
  title,
  children,
  confirmLabel,
  onConfirm,
  close,
  tone = "danger",
}: {
  /** `danger` for destructive actions, `primary` for outward but safe ones. */
  tone?: "danger" | "primary";
  title: string;
  children: ReactNode;
  confirmLabel: string;
  /** Rejections are shown in the dialog, which stays open. */
  onConfirm: () => Promise<void>;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const confirm = async () => {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      close();
    } catch (error) {
      setError(String(error));
      setBusy(false);
    }
  };
  return (
    <dialog
      ref={dialog}
      onCancel={close}
      aria-labelledby="confirm-title"
      className="m-auto w-[400px] max-w-[92vw] rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <div className="px-6 pb-5 pt-5">
        <h2 id="confirm-title" className="text-[15px] font-semibold">
          {title}
        </h2>
        <div className="mt-2 text-[13px] text-muted">{children}</div>
        {error && (
          <p
            role="alert"
            className="mt-3 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable"
          >
            {error}
          </p>
        )}
        <div className="mt-5 flex justify-end gap-2">
          <button
            autoFocus
            onClick={close}
            className="h-8 rounded-lg border border-line-strong px-3 text-[13px] hover:bg-hover"
          >
            Cancel
          </button>
          <button
            onClick={() => void confirm()}
            disabled={busy}
            className={clsx(
              "h-8 rounded-lg px-3 text-[13px] font-medium text-white hover:opacity-90 disabled:opacity-60",
              tone === "danger" ? "bg-danger-solid" : "bg-solid",
            )}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </dialog>
  );
}

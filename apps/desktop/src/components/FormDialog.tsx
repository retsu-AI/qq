import { useEffect, useRef, useState, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";

/** A modal form whose submit may fail; the error shows and it stays open. */
export function FormDialog({
  title,
  children,
  submitLabel,
  submit,
  close,
}: {
  title: string;
  children: ReactNode;
  submitLabel: string;
  /** Rejections show in the dialog, which stays open. */
  submit: () => Promise<void>;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      onCancel={(event) => {
        if (busy) event.preventDefault();
        else close();
      }}
      aria-label={title}
      className="m-auto w-[520px] max-w-[92vw] rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <form
        className="px-6 pb-5 pt-5"
        onSubmit={async (event) => {
          event.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await submit();
            close();
          } catch (error) {
            setError(error instanceof Error ? error.message : String(error));
            setBusy(false);
          }
        }}
      >
        <h2 className="text-[15px] font-semibold">{title}</h2>
        <div className="mt-3 space-y-3 text-[13px]">{children}</div>
        {error && (
          <p
            role="alert"
            className="mt-3 max-h-40 overflow-y-auto whitespace-pre-wrap rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable"
          >
            {error}
          </p>
        )}
        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={close}
            disabled={busy}
            className="h-8 rounded-lg border border-line-strong px-3 text-[13px] hover:bg-hover disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy}
            className="flex h-8 items-center gap-2 rounded-lg bg-solid px-3 text-[13px] font-medium text-white hover:bg-solid-hover disabled:opacity-60"
          >
            {busy && <RefreshCw className="h-3.5 w-3.5 animate-spin" />}
            {submitLabel}
          </button>
        </div>
      </form>
    </dialog>
  );
}

export const fieldClass =
  "w-full rounded-lg border border-line-strong bg-raised px-3 py-2 text-[13px] outline-none focus:border-accent/60 focus-visible:outline-none";

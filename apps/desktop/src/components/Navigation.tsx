import { useEffect, useRef, type ComponentProps } from "react";
import { X } from "lucide-react";
import { platform } from "../lib/platform";
import { Sidebar } from "./Sidebar";

/** The same sidebar, placed in a focus-trapping drawer on phones. */
export function Navigation({ open, close, ...props }: ComponentProps<typeof Sidebar> & {
  open: boolean;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    if (!platform.mobile) return;
    if (open) dialog.current?.showModal();
    else dialog.current?.close();
  }, [open]);
  if (!platform.mobile) return open ? <Sidebar {...props} /> : null;
  return (
    <dialog ref={dialog} className="mobile-drawer" aria-label="Sessions and projects"
      onCancel={close} onClick={(event) => {
        if (event.target === event.currentTarget) close();
      }}>
      <div className="flex h-full flex-col bg-sidebar">
        <button autoFocus onClick={close} aria-label="Close sidebar"
          className="ml-auto flex h-11 w-11 items-center justify-center text-muted">
          <X className="h-5 w-5" />
        </button>
        <Sidebar {...props} onNavigate={close} />
      </div>
    </dialog>
  );
}

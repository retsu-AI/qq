import { useEffect, useRef, type ReactNode } from "react";
import { platform } from "../lib/platform";

/** Desktop side panel; mobile modal sheet with native focus containment. */
export function Panel({ label, close, className, children }: {
  label: string;
  close: () => void;
  className: string;
  children: ReactNode;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    if (platform.mobile) dialog.current?.showModal();
  }, []);
  return platform.mobile ? (
    <dialog ref={dialog} aria-label={label} onCancel={close}
      className={`mobile-panel ${className}`}>{children}</dialog>
  ) : <aside aria-label={label} className={className}>{children}</aside>;
}

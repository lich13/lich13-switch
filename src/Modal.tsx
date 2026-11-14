import {
  useEffect,
  useId,
  useRef,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { X } from "lucide-react";
/**
 * Opens the dialog, keeps Tab inside it and hands focus back to the opener on
 * close. `keepInitialFocus` leaves focus on a control the dialog focused itself.
 */
export function useDialogFocus(keepInitialFocus = false) {
  const ref = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const fallback = previous?.closest("details")?.querySelector("summary");
    const dialog = ref.current;
    dialog?.showModal();
    if (!keepInitialFocus || !dialog?.contains(document.activeElement))
      heading.current?.focus({ preventScroll: true });
    return () => {
      dialog?.close();
      if (fallback instanceof HTMLElement && fallback.isConnected)
        fallback.focus();
      else if (previous?.isConnected) previous.focus();
    };
  }, []);
  const onKeyDown = (e: KeyboardEvent<HTMLDialogElement>) => {
    if (e.key !== "Tab") return;
    const controls = Array.from(
      ref.current?.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex="0"]',
      ) ?? [],
    ).filter((element) => element.getClientRects().length > 0);
    const first = controls[0],
      last = controls.at(-1);
    if (!first || !last) {
      e.preventDefault();
      heading.current?.focus();
    } else if (
      e.shiftKey &&
      (document.activeElement === first ||
        document.activeElement === heading.current)
    ) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };
  return { ref, heading, onKeyDown };
}
export default function Modal({
  title,
  close,
  busy = false,
  compact = false,
  className = "",
  children,
}: {
  title: string;
  close: () => void;
  busy?: boolean;
  compact?: boolean;
  className?: string;
  children: ReactNode;
}) {
  const { ref, heading, onKeyDown } = useDialogFocus();
  const titleId = useId();
  useEffect(() => {
    if (!ref.current?.contains(document.activeElement))
      heading.current?.focus();
  }, [busy]);
  return (
    <dialog
      ref={ref}
      className={`modal gateway-modal shared-modal${compact ? " compact-modal" : ""}${className ? ` ${className}` : ""}`}
      aria-labelledby={titleId}
      aria-busy={busy}
      onKeyDown={onKeyDown}
      onCancel={(e) => {
        e.preventDefault();
        if (!busy) close();
      }}
    >
      <div className="modal-heading">
        <h2 ref={heading} id={titleId} tabIndex={-1}>
          {title}
        </h2>
        <button
          className="icon-button"
          aria-label="关闭"
          disabled={busy}
          onClick={close}
        >
          <X size={18} />
        </button>
      </div>
      <div className="modal-body">{children}</div>
    </dialog>
  );
}

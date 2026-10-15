import { useEffect, useRef } from "react";
import { X } from "lucide-react";

export function Drawer({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const focus = document.activeElement as HTMLElement | null;
    const dialog = ref.current;
    dialog?.showModal();
    return () => {
      dialog?.close();
      queueMicrotask(() => {
        if (!dialog?.isConnected && focus?.isConnected)
          focus.focus({ preventScroll: true });
      });
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className="usage-drawer"
      aria-label={title}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          const r = e.currentTarget.getBoundingClientRect();
          if (e.clientX < r.left) onClose();
        }
      }}
    >
      <header>
        <h2>{title}</h2>
        <button className="icon-button" aria-label="关闭详情" onClick={onClose}>
          <X size={18} />
        </button>
      </header>
      <div className="usage-drawer-body">{children}</div>
    </dialog>
  );
}

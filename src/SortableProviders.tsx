import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
  type CollisionDetection,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { restrictToVerticalAxis } from "@dnd-kit/modifiers";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical } from "lucide-react";
import type { EditRevision } from "./gateway-edit";
import { errorOf, type Provider } from "./types";

export type ProviderEdit =
  | { op: "reorder"; ids: string[] }
  | { op: "queueProvider"; id: string; queued: boolean }
  | { op: "concurrencyProvider"; id: string; maxConcurrency: number }
  | {
      op: "takeoverProvider";
      id: string;
      handoffAfterCompaction?: boolean;
      takeNewThreads?: boolean;
    }
  | { op: "rpmProvider"; id: string; maxRpm: number };
export type ProviderCommit = (
  edit: ProviderEdit,
  revision: EditRevision,
) => Promise<void>;
type Content = (
  provider: Provider,
  priority: number | null,
  handle: ReactNode,
  busy: boolean,
) => ReactNode;
type Drag = { id: string; ids: string[]; revision: string };

// CC Switch a1216b7e: handle-only pointer/keyboard sorting, adapted to revisioned edits.
export default function SortableProviders({
  providers,
  revision,
  disabled,
  visible = true,
  commit,
  report,
  rowClass,
  children,
}: {
  providers: Provider[];
  revision: string;
  disabled: boolean;
  visible?: boolean;
  commit: ProviderCommit;
  report: (message: string) => void;
  rowClass: (provider: Provider) => string;
  children: Content;
}) {
  const root = useRef<HTMLDivElement>(null);
  const drag = useRef<Drag | null>(null);
  const latest = useRef({ providers, revision, visible, commit, report });
  latest.current = { providers, revision, visible, commit, report };
  const [active, setActive] = useState<string | null>(null);
  const [generation, setGeneration] = useState(0);
  const [pending, setPending] = useState<string[] | null>(null);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 8 } }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );
  const cancel = (restoreFocus = false) => {
    const id = drag.current?.id;
    if (!id) return;
    drag.current = null;
    setActive(null);
    setGeneration((n) => n + 1);
    if (restoreFocus)
      requestAnimationFrame(() => {
        const row = [
          ...(root.current?.querySelectorAll<HTMLElement>(
            "[data-provider-id]",
          ) ?? []),
        ].find((row) => row.dataset.providerId === id);
        row
          ?.querySelector<HTMLButtonElement>(".provider-drag")
          ?.focus({ preventScroll: true });
      });
  };
  useEffect(() => {
    if (!visible) cancel();
    else if (drag.current && drag.current.revision !== revision) {
      cancel(true);
      latest.current.report("供应商设置已变化，请重新排序");
    }
  }, [revision, visible]);
  useEffect(() => {
    const blur = () => cancel();
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.isComposing && drag.current) {
        event.preventDefault();
        event.stopImmediatePropagation();
        cancel(true);
      }
    };
    window.addEventListener("blur", blur);
    document.addEventListener("keydown", escape, true);
    return () => {
      window.removeEventListener("blur", blur);
      document.removeEventListener("keydown", escape, true);
    };
  }, []);
  const collision: CollisionDetection = (args) => {
    // Ignore a release outside the visible list, including clipped scroll content.
    if (args.pointerCoordinates && root.current) {
      let { top, right, bottom, left } = root.current.getBoundingClientRect();
      for (
        let node = root.current.parentElement;
        node;
        node = node.parentElement
      ) {
        if (/(auto|scroll|hidden)/.test(getComputedStyle(node).overflowY)) {
          const box = node.getBoundingClientRect();
          top = Math.max(top, box.top);
          bottom = Math.min(bottom, box.bottom);
          left = Math.max(left, box.left);
          right = Math.min(right, box.right);
        }
      }
      const { x, y } = args.pointerCoordinates;
      if (x < left || x > right || y < top || y > bottom) return [];
    }
    return closestCenter(args);
  };
  const finish = async ({ over }: DragEndEvent) => {
    const start = drag.current;
    drag.current = null;
    setActive(null);
    if (!start || !over || start.id === over.id || !latest.current.visible)
      return;
    if (start.revision !== latest.current.revision) {
      latest.current.report("供应商设置已变化，请重新排序");
      return;
    }
    const from = start.ids.indexOf(start.id),
      to = start.ids.indexOf(String(over.id));
    if (from < 0 || to < 0) return;
    const ids = arrayMove(start.ids, from, to);
    setPending(ids);
    try {
      await latest.current.commit({ op: "reorder", ids }, start.revision);
    } catch (e) {
      latest.current.report(errorOf(e).message);
    } finally {
      setPending(null);
    }
  };
  const order = pending ?? drag.current?.ids;
  const rows = order
    ? [...providers].sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id))
    : providers;
  let priority = 0;
  return (
    <DndContext
      key={generation}
      sensors={sensors}
      modifiers={[restrictToVerticalAxis]}
      collisionDetection={collision}
      onDragStart={({ active }) => {
        drag.current = {
          id: String(active.id),
          ids: providers.map((p) => p.id),
          revision,
        };
        setActive(String(active.id));
      }}
      onDragEnd={(event) => void finish(event)}
      onDragCancel={() => {
        drag.current = null;
        setActive(null);
      }}
      accessibility={{
        screenReaderInstructions: {
          draggable: "按空格拾取，上下键移动，空格确认，Escape 取消。",
        },
        announcements: {
          onDragStart: () => "已拾取供应商",
          onDragOver: ({ over }) =>
            over
              ? `移动到第 ${rows.findIndex((p) => p.id === over.id) + 1} 位`
              : "已离开列表",
          onDragEnd: () => "拖动已结束",
          onDragCancel: () => "已取消排序",
        },
      }}
    >
      <SortableContext
        items={rows.map((p) => p.id)}
        strategy={verticalListSortingStrategy}
      >
        <div
          ref={root}
          className="sortable-providers"
          aria-label="供应商列表"
          aria-busy={Boolean(pending)}
        >
          {rows.map((provider) => (
            <SortableRow
              key={provider.id}
              provider={provider}
              priority={provider.queued ? ++priority : null}
              disabled={disabled || Boolean(pending)}
              busy={disabled || Boolean(active) || Boolean(pending)}
              className={rowClass(provider)}
            >
              {children}
            </SortableRow>
          ))}
        </div>
      </SortableContext>
    </DndContext>
  );
}

function SortableRow({
  provider,
  priority,
  disabled,
  busy,
  className,
  children,
}: {
  provider: Provider;
  priority: number | null;
  disabled: boolean;
  busy: boolean;
  className: string;
  children: Content;
}) {
  const {
    setNodeRef,
    setActivatorNodeRef,
    attributes,
    listeners,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: provider.id, disabled });
  return (
    <article
      ref={setNodeRef}
      data-provider-id={provider.id}
      className={`${className}${isDragging ? " is-dragging" : ""}`}
      style={{ transform: CSS.Transform.toString(transform), transition }}
    >
      {children(
        provider,
        priority,
        <button
          ref={setActivatorNodeRef}
          type="button"
          className="provider-drag"
          disabled={disabled}
          {...attributes}
          {...listeners}
          aria-label={`拖动 ${provider.name}`}
        >
          <GripVertical size={15} />
        </button>,
        busy,
      )}
    </article>
  );
}

import { useCallback, useEffect, useRef, useState, type MouseEvent, type ReactNode } from "react";
import { fmtBytes, fmtCount, thumbUrl } from "../api";

/**
 * Picking photos in a thumbnail grid, the library's way: a click opens the photo, Ctrl/⌘+click
 * adds or removes one, Shift+click picks a range, and pressing on one photo then moving over
 * others picks them all.
 */
export function usePick(order: number[], open: (id: number) => void) {
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const anchor = useRef<number | null>(null);
  const painting = useRef<null | boolean>(null); // adding (true) or removing (false) while dragging
  const moved = useRef(false);

  useEffect(() => {
    const up = () => (painting.current = null);
    window.addEventListener("mouseup", up);
    return () => window.removeEventListener("mouseup", up);
  }, []);
  // Photos that left the grid leave the selection too.
  useEffect(() => {
    setPicked((p) => {
      const keep = new Set(order);
      const next = new Set([...p].filter((id) => keep.has(id)));
      return next.size === p.size ? p : next;
    });
  }, [order]);

  const onDown = useCallback((e: MouseEvent, id: number) => {
    if (e.button !== 0 || e.shiftKey) return;
    moved.current = false;
    painting.current = !picked.has(id);
  }, [picked]);
  const onEnter = useCallback((id: number) => {
    if (painting.current == null) return;
    moved.current = true;
    const add = painting.current;
    setPicked((p) => {
      const n = new Set(p);
      if (add) n.add(id);
      else n.delete(id);
      return n;
    });
  }, []);
  const onLeaveFirst = useCallback((id: number) => {
    // Dragging out of the first photo picks it too.
    if (painting.current == null || moved.current) return;
    const add = painting.current;
    setPicked((p) => {
      const n = new Set(p);
      if (add) n.add(id);
      else n.delete(id);
      return n;
    });
  }, []);
  const onClick = useCallback((e: MouseEvent, id: number) => {
    if (moved.current) {
      moved.current = false;
      return;
    }
    if (e.shiftKey && anchor.current != null) {
      const a = order.indexOf(anchor.current);
      const b = order.indexOf(id);
      if (a >= 0 && b >= 0) {
        const [lo, hi] = a < b ? [a, b] : [b, a];
        setPicked((p) => new Set([...p, ...order.slice(lo, hi + 1)]));
        return;
      }
    }
    if (e.ctrlKey || e.metaKey || picked.size > 0) {
      anchor.current = id;
      setPicked((p) => {
        const n = new Set(p);
        if (n.has(id)) n.delete(id);
        else n.add(id);
        return n;
      });
      return;
    }
    anchor.current = id;
    open(id);
  }, [order, picked, open]);

  return {
    picked,
    setPicked,
    clear: () => setPicked(new Set()),
    pickAll: (ids: number[]) => setPicked((p) => new Set([...p, ...ids])),
    tileProps: (id: number) => ({
      onMouseDown: (e: MouseEvent) => onDown(e, id),
      onMouseEnter: () => onEnter(id),
      onMouseLeave: () => onLeaveFirst(id),
      onClick: (e: MouseEvent) => onClick(e, id),
    }),
  };
}

export function PhotoTile({ id, picked, marked, markLabel = "지움", children, ...rest }: {
  id: number; picked: boolean; marked?: boolean; markLabel?: string; children?: ReactNode;
} & ReturnType<ReturnType<typeof usePick>["tileProps"]>) {
  return (
    <button className={`keep-tile ${picked ? "picked" : ""} ${marked ? "marked" : ""}`} {...rest} draggable={false}>
      <img src={thumbUrl(id)} loading="lazy" alt="" draggable={false} />
      {picked && <span className="pick-check">✓</span>}
      {marked && <span className="keep-badge drop">{markLabel}</span>}
      {children}
    </button>
  );
}

/** "N장 선택 · X MB — [넣기] [빼기] [비교] [해제]" */
export function PickBar({ count, bytes, onAdd, onRemove, onCompare, onClear, prefer }: {
  count: number; bytes: number; onAdd?: () => void; onRemove?: () => void; onCompare?: () => void; onClear: () => void;
  /** Which action is the main one here. */
  prefer: "add" | "remove";
}) {
  // Always there (a hint while nothing is picked), so the photos do not jump when one is picked.
  if (count === 0) {
    return (
      <div className="pick-bar idle muted small">
        Ctrl+클릭으로 하나씩 · Shift+클릭으로 범위 · 누른 채 끌어서 여러 장 고르기 — 그냥 누르면 크게 봅니다
      </div>
    );
  }
  return (
    <div className="pick-bar">
      <span><b>{fmtCount(count)}장</b> 선택 · {fmtBytes(bytes)}</span>
      <span className="spacer" />
      {onAdd && <button className={`btn small ${prefer === "add" ? "danger-solid" : ""}`} onClick={onAdd}>지울 목록에 넣기</button>}
      {onRemove && <button className={`btn small ${prefer === "remove" ? "primary" : ""}`} onClick={onRemove}>지울 목록에서 빼기</button>}
      {onCompare && count >= 2 && count <= 4 && <button className="btn small" onClick={onCompare}>⇆ 비교</button>}
      <button className="btn small ghost" onClick={onClear}>선택 해제</button>
    </div>
  );
}

import { useEffect, useRef, useState } from "react";

const MAX = 8;
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * A picture that zooms like a photo app: mouse wheel zooms around the cursor, double-click
 * toggles fit ↔ 100% (actual pixels) at the clicked spot, drag pans while zoomed.
 * `resetKey` changes (next photo) bring it back to fit.
 */
export default function ZoomImage({ src, resetKey, onError }: { src: string; resetKey: unknown; onError?: () => void }) {
  const box = useRef<HTMLDivElement>(null);
  const img = useRef<HTMLImageElement>(null);
  const [t, setT] = useState({ s: 1, x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number } | null>(null);

  useEffect(() => setT({ s: 1, x: 0, y: 0 }), [resetKey]);

  /** New scale `s2` keeping the screen point (qx, qy) — relative to the box centre — fixed. */
  const zoomAt = (cur: typeof t, s2: number, qx: number, qy: number) => {
    if (s2 <= 1.001) return { s: 1, x: 0, y: 0 };
    const px = (qx - cur.x) / cur.s;
    const py = (qy - cur.y) / cur.s;
    return { s: s2, x: qx - s2 * px, y: qy - s2 * py };
  };

  const centreOffset = (clientX: number, clientY: number) => {
    const r = box.current!.getBoundingClientRect();
    return [clientX - (r.left + r.width / 2), clientY - (r.top + r.height / 2)] as const;
  };

  // Wheel must be a non-passive listener to stop the page from scrolling.
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const [qx, qy] = centreOffset(e.clientX, e.clientY);
      setT((cur) => zoomAt(cur, clamp(cur.s * Math.exp(-e.deltaY * 0.0015), 1, MAX), qx, qy));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  /** Scale at which one image pixel is one screen pixel. */
  const actualPixels = () => {
    const i = img.current;
    if (!i || !i.naturalWidth) return 2;
    const shown = i.getBoundingClientRect().width / t.s;
    return clamp(i.naturalWidth / Math.max(1, shown), 1.5, MAX);
  };

  return (
    <div
      ref={box}
      className={`zoom-box ${t.s > 1 ? "zoomed" : ""}`}
      onDoubleClick={(e) => {
        const [qx, qy] = centreOffset(e.clientX, e.clientY);
        setT((cur) => (cur.s > 1 ? { s: 1, x: 0, y: 0 } : zoomAt(cur, actualPixels(), qx, qy)));
      }}
      onPointerDown={(e) => {
        if (t.s <= 1 || e.button !== 0) return;
        drag.current = { x: e.clientX, y: e.clientY };
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        const dx = e.clientX - d.x;
        const dy = e.clientY - d.y;
        drag.current = { x: e.clientX, y: e.clientY };
        setT((cur) => ({ ...cur, x: cur.x + dx, y: cur.y + dy }));
      }}
      onPointerUp={() => (drag.current = null)}
    >
      <img
        ref={img}
        src={src}
        alt=""
        className="viewer-media"
        draggable={false}
        onError={onError}
        style={{ transform: `translate(${t.x}px, ${t.y}px) scale(${t.s})` }}
      />
      {t.s > 1 && <span className="zoom-badge">{Math.round(t.s * 100)}% · 더블클릭: 맞춤</span>}
    </div>
  );
}

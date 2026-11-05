import { useEffect, useMemo, useRef, useState } from "react";

/** One month (or year) of the library, in display order. */
export interface ScrubSegment {
  key: string;
  year: number | null;
  month: number | null;
  /** "2024년 6월", "2024년", "날짜 없음" */
  label: string;
  /** First list row of this group (its header). */
  startRow: number;
  /** Estimated height in the list, so the bar is proportional to scrolling. */
  px: number;
}

interface Props {
  segments: ScrubSegment[];
  /** First list row visible at the top of the grid. */
  topRow: number;
  onJump: (row: number) => void;
}

const MIN_LABEL_GAP = 16;
const MIN_DOT_GAP = 4;

/**
 * Google Photos-style date scrubber on the right of the grid: year labels and month dots placed
 * in proportion to how much of the list each period takes; hover shows the month, click or drag
 * jumps straight there.
 */
export default function TimelineScrubber({ segments, topRow, onJump }: Props) {
  const bar = useRef<HTMLDivElement>(null);
  const [h, setH] = useState(0);
  const [hover, setHover] = useState<{ y: number; i: number } | null>(null);
  const dragging = useRef(false);
  const lastJump = useRef(-1);
  const frame = useRef(0);

  useEffect(() => {
    const el = bar.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setH(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Cumulative top of each segment on the bar.
  const tops = useMemo(() => {
    const total = segments.reduce((a, s) => a + s.px, 0) || 1;
    let acc = 0;
    return segments.map((s) => {
      const t = (acc / total) * h;
      acc += s.px;
      return t;
    });
  }, [segments, h]);

  // Year labels where a year starts, skipping ones that would overlap; month dots likewise.
  const { labels, dots } = useMemo(() => {
    const labels: { y: number; text: string }[] = [];
    const dots: number[] = [];
    let lastLabel = -Infinity;
    let lastDot = -Infinity;
    let prevYear: number | null | undefined = undefined;
    segments.forEach((s, i) => {
      const y = tops[i];
      if (s.year !== prevYear) {
        prevYear = s.year;
        if (y - lastLabel >= MIN_LABEL_GAP && y <= h - 8) {
          labels.push({ y, text: s.year == null ? "날짜 없음" : String(s.year) });
          lastLabel = y;
          lastDot = y;
          return;
        }
      }
      if (s.month != null && y - lastDot >= MIN_DOT_GAP) {
        dots.push(y);
        lastDot = y;
      }
    });
    return { labels, dots };
  }, [segments, tops, h]);

  /** Segment under a y position on the bar (binary search over the tops). */
  const segAt = (y: number): number => {
    let lo = 0;
    let hi = tops.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (tops[mid] <= y) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  };

  const yOf = (e: React.PointerEvent) => {
    const r = bar.current!.getBoundingClientRect();
    return Math.min(Math.max(e.clientY - r.top, 0), r.height - 1);
  };

  const jump = (i: number) => {
    if (i === lastJump.current) return;
    lastJump.current = i;
    cancelAnimationFrame(frame.current);
    frame.current = requestAnimationFrame(() => onJump(segments[i].startRow));
  };

  // Where the visible top is, on the same scale as the labels (not the list's estimated height).
  const nowY = useMemo(() => {
    if (segments.length === 0) return 0;
    let i = 0;
    while (i + 1 < segments.length && segments[i + 1].startRow <= topRow) i++;
    const nextRow = i + 1 < segments.length ? segments[i + 1].startRow : segments[i].startRow + 1;
    const nextTop = i + 1 < tops.length ? tops[i + 1] : h;
    const frac = Math.min(1, Math.max(0, (topRow - segments[i].startRow) / Math.max(1, nextRow - segments[i].startRow)));
    return tops[i] + frac * (nextTop - tops[i]);
  }, [segments, tops, topRow, h]);

  if (segments.length === 0) return null;

  return (
    <div
      ref={bar}
      className={`scrubber ${hover ? "active" : ""}`}
      onPointerDown={(e) => {
        dragging.current = true;
        lastJump.current = -1;
        (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
        const y = yOf(e);
        const i = segAt(y);
        setHover({ y, i });
        jump(i);
      }}
      onPointerMove={(e) => {
        const y = yOf(e);
        const i = segAt(y);
        setHover({ y, i });
        if (dragging.current) jump(i);
      }}
      onPointerUp={(e) => {
        dragging.current = false;
        (e.target as HTMLElement).releasePointerCapture?.(e.pointerId);
      }}
      onPointerLeave={() => !dragging.current && setHover(null)}
      title=""
    >
      {dots.map((y) => (
        <span key={`d${y}`} className="scrub-dot" style={{ top: y }} />
      ))}
      {labels.map((l) => (
        <span key={`l${l.y}`} className="scrub-year" style={{ top: l.y }}>{l.text}</span>
      ))}
      <span className="scrub-now" style={{ top: nowY }} />
      {hover && (
        <>
          <span className="scrub-guide" style={{ top: hover.y }} />
          <span className="scrub-bubble" style={{ top: hover.y }}>{segments[hover.i].label}</span>
        </>
      )}
    </div>
  );
}

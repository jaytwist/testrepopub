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
  /** Rows in the whole list (the last segment ends here). */
  totalRows: number;
  /** Top of the viewport in rows, fractional (12.4 = 40% into row 12). */
  topRow: number;
  onJump: (row: number) => void;
  /** Wheel over the bar scrolls the list like the wheel over the photos. */
  onWheel: (deltaY: number) => void;
}

const MIN_LABEL_GAP = 16;
const MIN_DOT_GAP = 3;
/** A year this tall on the bar also gets quarter-month numbers (3, 6, 9, 12). */
const YEAR_SPAN_FOR_MONTHS = 120;
const MIN_MONTH_LABEL_GAP = 14;

/**
 * Google Photos-style date scrubber on the right of the grid: year labels and month marks placed
 * in proportion to how much of the list each period takes. Hover shows the month; click or drag
 * moves continuously (also inside a big month); the wheel works over it; while scrolling, the
 * current month is shown next to the position line.
 */
export default function TimelineScrubber({ segments, totalRows, topRow, onJump, onWheel }: Props) {
  const bar = useRef<HTMLDivElement>(null);
  const [h, setH] = useState(0);
  const [hover, setHover] = useState<{ y: number; i: number } | null>(null);
  const [scrolling, setScrolling] = useState(false);
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

  // Show the current month briefly whenever the list moves.
  const firstRender = useRef(true);
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    setScrolling(true);
    const t = setTimeout(() => setScrolling(false), 1000);
    return () => clearTimeout(t);
  }, [Math.round(topRow * 4)]); // eslint-disable-line react-hooks/exhaustive-deps

  // Cumulative top of each segment on the bar (+ the end).
  const tops = useMemo(() => {
    const total = segments.reduce((a, s) => a + s.px, 0) || 1;
    let acc = 0;
    const out = segments.map((s) => {
      const t = (acc / total) * h;
      acc += s.px;
      return t;
    });
    out.push(h);
    return out;
  }, [segments, h]);

  const rowEnd = (i: number) => (i + 1 < segments.length ? segments[i + 1].startRow : totalRows);

  // Year labels where a year starts; quarter-month numbers inside tall years; month dots.
  const { labels, months, dots } = useMemo(() => {
    const labels: { y: number; text: string }[] = [];
    const months: { y: number; text: string }[] = [];
    const dots: number[] = [];
    // Height of each year on the bar.
    const span = new Map<number | null, number>();
    segments.forEach((s, i) => span.set(s.year, (span.get(s.year) ?? 0) + (tops[i + 1] - tops[i])));
    let lastLabel = -Infinity;
    let lastText = -Infinity;
    let lastDot = -Infinity;
    let prevYear: number | null | undefined = undefined;
    segments.forEach((s, i) => {
      const y = tops[i];
      if (s.year !== prevYear) {
        prevYear = s.year;
        if (y - lastLabel >= MIN_LABEL_GAP && y <= h - 8) {
          labels.push({ y, text: s.year == null ? "날짜 없음" : String(s.year) });
          lastLabel = y;
          lastText = y;
          lastDot = y;
          return;
        }
      }
      if (s.month == null) return;
      if (s.month % 3 === 0 && (span.get(s.year) ?? 0) >= YEAR_SPAN_FOR_MONTHS && y - lastText >= MIN_MONTH_LABEL_GAP && y <= h - 8) {
        months.push({ y, text: `${s.month}월` });
        lastText = y;
        lastDot = y;
      } else if (y - lastDot >= MIN_DOT_GAP) {
        dots.push(y);
        lastDot = y;
      }
    });
    return { labels, months, dots };
  }, [segments, tops, h]);

  /** Segment under a y position on the bar (binary search over the tops). */
  const segAt = (y: number): number => {
    let lo = 0;
    let hi = segments.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (tops[mid] <= y) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  };

  /** List row at a y position: continuous, also inside a long month. */
  const rowAt = (y: number): number => {
    const i = segAt(y);
    const frac = (y - tops[i]) / Math.max(1, tops[i + 1] - tops[i]);
    const n = rowEnd(i) - segments[i].startRow;
    return segments[i].startRow + Math.min(n - 1, Math.max(0, Math.floor(frac * n)));
  };

  const yOf = (e: React.PointerEvent) => {
    const r = bar.current!.getBoundingClientRect();
    return Math.min(Math.max(e.clientY - r.top, 0), r.height - 1);
  };

  const jump = (row: number) => {
    if (row === lastJump.current) return;
    lastJump.current = row;
    cancelAnimationFrame(frame.current);
    frame.current = requestAnimationFrame(() => onJump(row));
  };

  // Where the visible top is, on the same scale as the labels (not the list's estimated height).
  const { nowY, nowSeg } = useMemo(() => {
    if (segments.length === 0) return { nowY: 0, nowSeg: 0 };
    let i = 0;
    while (i + 1 < segments.length && segments[i + 1].startRow <= topRow) i++;
    const frac = Math.min(1, Math.max(0, (topRow - segments[i].startRow) / Math.max(1, rowEnd(i) - segments[i].startRow)));
    return { nowY: tops[i] + frac * (tops[i + 1] - tops[i]), nowSeg: i };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [segments, tops, topRow, totalRows]);

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
        setHover({ y, i: segAt(y) });
        jump(rowAt(y));
      }}
      onPointerMove={(e) => {
        const y = yOf(e);
        setHover({ y, i: segAt(y) });
        if (dragging.current) jump(rowAt(y));
      }}
      onPointerUp={(e) => {
        dragging.current = false;
        (e.target as HTMLElement).releasePointerCapture?.(e.pointerId);
      }}
      onPointerLeave={() => !dragging.current && setHover(null)}
      onWheel={(e) => onWheel(e.deltaY)}
    >
      {dots.map((y) => (
        <span key={`d${y}`} className="scrub-dot" style={{ top: y }} />
      ))}
      {months.map((m) => (
        <span key={`m${m.y}`} className="scrub-month" style={{ top: m.y }}>{m.text}</span>
      ))}
      {labels.map((l) => (
        <span key={`l${l.y}`} className="scrub-year" style={{ top: l.y }}>{l.text}</span>
      ))}
      <span className="scrub-now" style={{ top: nowY }} />
      {hover ? (
        <>
          <span className="scrub-guide" style={{ top: hover.y }} />
          <span className="scrub-bubble" style={{ top: hover.y }}>{segments[hover.i].label}</span>
        </>
      ) : (
        scrolling && <span className="scrub-bubble now" style={{ top: nowY }}>{segments[nowSeg].label}</span>
      )}
    </div>
  );
}

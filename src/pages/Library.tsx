import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Virtuoso } from "react-virtuoso";
import { api, fmtCount, fmtDuration, thumbUrl, type FileFilter, type GridItem, type MonthCount } from "../api";
import { useJob } from "../App";
import FilterBar from "../components/FilterBar";
import Viewer from "../components/Viewer";

const PAGE = 2000;
const TILE = 168;
const GAP = 4;

type Row = { type: "header"; key: string; title: string; count: number } | { type: "tiles"; key: string; items: { item: GridItem; index: number }[] };

function monthKey(ts: number | null) {
  if (ts == null) return "none";
  const d = new Date(ts * 1000);
  return `${d.getUTCFullYear()}-${d.getUTCMonth() + 1}`;
}
function monthTitle(key: string) {
  if (key === "none") return "날짜 없음";
  const [y, m] = key.split("-");
  return `${y}년 ${m}월`;
}

export default function Library() {
  const { version } = useJob();
  const [filter, setFilter] = useState<FileFilter>({});
  const [timeline, setTimeline] = useState<MonthCount[]>([]);
  const [items, setItems] = useState<GridItem[]>([]);
  const [done, setDone] = useState(false);
  const [cols, setCols] = useState(6);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [viewIndex, setViewIndex] = useState<number | null>(null);
  const [bulk, setBulk] = useState<"label" | "date" | null>(null);
  const [bulkValue, setBulkValue] = useState("");
  const loading = useRef(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  const gen = useRef(0);

  const total = useMemo(() => timeline.reduce((a, m) => a + m.count, 0), [timeline]);
  const monthCounts = useMemo(() => new Map(timeline.map((m) => [`${m.year}-${m.month}`, m.count])), [timeline]);

  const reload = useCallback(() => {
    const g = ++gen.current;
    loading.current = true;
    setDone(false);
    Promise.all([api.timeline(filter), api.grid(filter, 0, PAGE)]).then(([tl, first]) => {
      if (g !== gen.current) return;
      setTimeline(tl);
      setItems(first);
      setDone(first.length < PAGE);
      loading.current = false;
    });
  }, [filter]);

  useEffect(() => {
    setSelected(new Set());
    reload();
  }, [reload, version]);

  const loadMore = useCallback(() => {
    if (loading.current || done) return;
    loading.current = true;
    const g = gen.current;
    api.grid(filter, items.length, PAGE).then((more) => {
      if (g !== gen.current) return;
      setItems((cur) => cur.concat(more));
      setDone(more.length < PAGE);
      loading.current = false;
    });
  }, [filter, items.length, done]);

  // Columns follow the window width.
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setCols(Math.max(2, Math.floor((el.clientWidth - 24) / (TILE + GAP)))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const rows: Row[] = useMemo(() => {
    const out: Row[] = [];
    let cur: string | null = null;
    let bucket: { item: GridItem; index: number }[] = [];
    const flush = () => {
      for (let i = 0; i < bucket.length; i += cols) {
        out.push({ type: "tiles", key: `${cur}-${i}`, items: bucket.slice(i, i + cols) });
      }
      bucket = [];
    };
    items.forEach((item, index) => {
      const k = monthKey(item.taken_at);
      if (k !== cur) {
        flush();
        cur = k;
        out.push({ type: "header", key: `h-${k}`, title: monthTitle(k), count: monthCounts.get(k) ?? 0 });
      }
      bucket.push({ item, index });
    });
    flush();
    return out;
  }, [items, cols, monthCounts]);

  const onTileClick = (e: React.MouseEvent, index: number, id: number) => {
    if (e.shiftKey && anchor != null) {
      const [a, b] = [Math.min(anchor, index), Math.max(anchor, index)];
      const next = new Set(selected);
      for (let i = a; i <= b; i++) next.add(items[i].id);
      setSelected(next);
    } else if (e.ctrlKey || e.metaKey || selected.size > 0) {
      const next = new Set(selected);
      next.has(id) ? next.delete(id) : next.add(id);
      setSelected(next);
      setAnchor(index);
    } else {
      setViewIndex(index);
    }
  };

  const toggle = (e: React.MouseEvent, index: number, id: number) => {
    e.stopPropagation();
    const next = new Set(selected);
    next.has(id) ? next.delete(id) : next.add(id);
    setSelected(next);
    setAnchor(index);
  };

  const selectAll = async () => setSelected(new Set(await api.queryIds(filter)));

  const applyBulk = async () => {
    const ids = Array.from(selected);
    if (bulk === "label" && bulkValue.trim()) await api.addLabel(ids, bulkValue.trim());
    if (bulk === "date" && bulkValue) await api.setDate(ids, bulkValue);
    setBulk(null);
    setBulkValue("");
    reload();
  };

  return (
    <div className="page library" ref={wrapRef}>
      <header className="page-head">
        <div>
          <h1>라이브러리</h1>
          <div className="muted">{fmtCount(total)}개 항목 · 클릭하면 크게 보기, Ctrl/Shift+클릭으로 여러 개 선택</div>
        </div>
      </header>
      <FilterBar filter={filter} onChange={setFilter} timeline={timeline} />

      {selected.size > 0 && (
        <div className="selbar">
          <b>{fmtCount(selected.size)}개 선택</b>
          <button className="btn small" onClick={() => { setBulk("label"); setBulkValue(""); }}>라벨 추가</button>
          <button className="btn small" onClick={() => { setBulk("date"); setBulkValue(""); }}>날짜 지정</button>
          <button className="btn small ghost" onClick={selectAll}>필터 결과 전체 선택</button>
          <button className="btn small ghost" onClick={() => setSelected(new Set())}>선택 해제</button>
          {bulk && (
            <span className="inline-form">
              {bulk === "label" ? (
                <input autoFocus placeholder="예: 가족여행, 졸업식" value={bulkValue} onChange={(e) => setBulkValue(e.target.value)} onKeyDown={(e) => e.key === "Enter" && applyBulk()} />
              ) : (
                <input autoFocus type="datetime-local" value={bulkValue} onChange={(e) => setBulkValue(e.target.value)} />
              )}
              <button className="btn small primary" onClick={applyBulk}>적용</button>
            </span>
          )}
        </div>
      )}

      {total === 0 && !loading.current ? (
        <div className="empty">
          <div className="empty-icon">⛁</div>
          <h2>아직 사진이 없습니다</h2>
          <p>왼쪽 <b>드라이브 · 폴더</b>에서 C 드라이브나 외장 드라이브의 폴더를 추가하고 스캔하세요.</p>
        </div>
      ) : (
        <Virtuoso
          className="grid-scroller"
          data={rows}
          endReached={loadMore}
          increaseViewportBy={800}
          computeItemKey={(_, r) => r.key}
          itemContent={(_, r) =>
            r.type === "header" ? (
              <div className="month-head">
                {r.title} <span className="muted">{fmtCount(r.count)}</span>
              </div>
            ) : (
              <div className="tile-row" style={{ gridTemplateColumns: `repeat(${cols}, ${TILE}px)`, gap: GAP }}>
                {r.items.map(({ item, index }) => (
                  <div
                    key={item.id}
                    className={`tile ${selected.has(item.id) ? "sel" : ""} ${item.online ? "" : "offline"}`}
                    style={{ width: TILE, height: TILE }}
                    onClick={(e) => onTileClick(e, index, item.id)}
                  >
                    <img loading="lazy" src={thumbUrl(item.id)} alt="" draggable={false} onError={(e) => ((e.target as HTMLImageElement).style.visibility = "hidden")} />
                    <button className="check" onClick={(e) => toggle(e, index, item.id)}>{selected.has(item.id) ? "✓" : ""}</button>
                    {item.kind === "video" && <span className="badge-video">▶ {fmtDuration(item.duration_ms)}</span>}
                    {item.kind === "raw" && <span className="badge-raw">RAW</span>}
                    {!item.online && <span className="badge-offline" title="드라이브 연결 안 됨">⏏</span>}
                  </div>
                ))}
              </div>
            )
          }
        />
      )}

      {viewIndex != null && items[viewIndex] && (
        <Viewer
          items={items}
          index={viewIndex}
          onIndex={(i) => {
            setViewIndex(i);
            if (i > items.length - 50) loadMore();
          }}
          onClose={() => setViewIndex(null)}
          onChanged={() => api.timeline(filter).then(setTimeline)}
        />
      )}
    </div>
  );
}

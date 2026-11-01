import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Virtuoso } from "react-virtuoso";
import { api, fmtBytes, fmtCount, fmtDate, fmtDuration, thumbUrl, type FileFilter, type FilteredStats, type GridItem, type MonthCount, type SortKey } from "../api";
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
function monthTitle(key: string, prefix = "") {
  if (key === "none") return "날짜 없음";
  const [y, m] = key.split("-");
  return `${prefix}${y}년 ${m}월`;
}
/** mtime is a real UTC timestamp: group by the PC's local month. */
function localMonthKey(ts: number) {
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}-${d.getMonth() + 1}`;
}

const MB = 1024 * 1024;
const SIZE_BUCKETS: [number, string][] = [[1024 * MB, "1GB 이상"], [100 * MB, "100MB – 1GB"], [10 * MB, "10 – 100MB"], [MB, "1 – 10MB"], [0, "1MB 미만"]];
const MP_BUCKETS: [number, string][] = [[40, "40MP 이상"], [12, "12 – 40MP"], [2, "2 – 12MP"], [0.000001, "2MP 미만"], [0, "해상도 정보 없음"]];
const DUR_BUCKETS: [number, string][] = [[600, "10분 이상"], [60, "1 – 10분"], [1, "1분 미만"], [0, "길이 없음 (사진)"]];

function bucket(table: [number, string][], v: number) {
  return (table.find(([min]) => v >= min) ?? table[table.length - 1])[1];
}

/** Group key + header title for an item under the current sort. */
function groupOf(item: GridItem, sort: SortKey): [string, string] {
  switch (sort) {
    case "mtime": {
      const k = localMonthKey(item.mtime);
      return [k, monthTitle(k, "수정: ")];
    }
    case "size": {
      const t = bucket(SIZE_BUCKETS, item.size);
      return [t, t];
    }
    case "resolution": {
      const t = bucket(MP_BUCKETS, ((item.width ?? 0) * (item.height ?? 0)) / 1e6);
      return [t, t];
    }
    case "duration": {
      const t = bucket(DUR_BUCKETS, (item.duration_ms ?? 0) / 1000);
      return [t, t];
    }
    case "name": {
      const c = (item.name[0] ?? "#").toUpperCase();
      const k = /[0-9]/.test(c) ? "0-9" : /[A-Z]/.test(c) ? c : /[가-힣]/.test(c) ? "한글" : "기타";
      return [k, k];
    }
    default: {
      const k = monthKey(item.taken_at);
      return [k, monthTitle(k)];
    }
  }
}

/** Small caption under each tile that shows the value being sorted by. */
function caption(item: GridItem, sort: SortKey): string | null {
  switch (sort) {
    case "size": return fmtBytes(item.size);
    case "resolution": return item.width ? `${item.width}×${item.height}` : null;
    case "duration": return item.duration_ms ? fmtDuration(item.duration_ms) : null;
    case "name": return item.name;
    case "mtime": return fmtDate(item.mtime);
    default: return null;
  }
}

export default function Library() {
  const { version } = useJob();
  const [filter, setFilter] = useState<FileFilter>({});
  const [timeline, setTimeline] = useState<MonthCount[]>([]);
  const [stats, setStats] = useState<FilteredStats | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
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

  // Group by the sort the *loaded* items were fetched with (not the one just selected), so a
  // transitional render never mixes orders.
  const [loadedSort, setLoadedSort] = useState<SortKey>("taken");
  const sort: SortKey = loadedSort;
  const total = stats?.count ?? 0;
  const monthCounts = useMemo(() => new Map(timeline.map((m) => [`${m.year}-${m.month}`, m.count])), [timeline]);

  const reload = useCallback(() => {
    const g = ++gen.current;
    loading.current = true;
    setDone(false);
    Promise.all([api.timeline(filter), api.grid(filter, 0, PAGE), api.filteredStats(filter)]).then(([tl, first, st]) => {
      if (g !== gen.current) return;
      setTimeline(tl);
      setStats(st);
      setLoadedSort(filter.sort ?? "taken");
      setItems(first);
      setDone(first.length < PAGE);
      setLoadError(null);
      loading.current = false;
    }).catch((e) => {
      if (g !== gen.current) return;
      setLoadError(String(e));
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
    let bucketItems: { item: GridItem; index: number }[] = [];
    // Bucket sizes are exact for capture-date sort (from the timeline); otherwise count what is loaded.
    const counts = new Map<string, number>();
    if (sort !== "taken") items.forEach((it) => { const k = groupOf(it, sort)[0]; counts.set(k, (counts.get(k) ?? 0) + 1); });
    const flush = () => {
      for (let i = 0; i < bucketItems.length; i += cols) {
        // Key by the first item's position: unique even if a group label repeats.
        out.push({ type: "tiles", key: `t-${bucketItems[i].index}`, items: bucketItems.slice(i, i + cols) });
      }
      bucketItems = [];
    };
    items.forEach((item, index) => {
      const [k, title] = groupOf(item, sort);
      if (k !== cur) {
        flush();
        cur = k;
        out.push({ type: "header", key: `h-${index}`, title, count: sort === "taken" ? monthCounts.get(k) ?? 0 : counts.get(k) ?? 0 });
      }
      bucketItems.push({ item, index });
    });
    flush();
    return out;
  }, [items, cols, monthCounts, sort]);

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
          <div className="muted">
            {stats ? (
              <>
                <b className="text">{fmtCount(stats.count)}개</b> · {fmtBytes(stats.bytes)}
                {stats.count > 0 && <> · 사진 {fmtCount(stats.photos)} · 동영상 {fmtCount(stats.videos)}{stats.raws ? ` · RAW ${fmtCount(stats.raws)}` : ""}</>}
                {stats.first_taken != null && stats.count > 1 && <> · {fmtDate(stats.first_taken)} ~ {fmtDate(stats.last_taken)}</>}
              </>
            ) : "불러오는 중…"}
          </div>
          <div className="muted small">클릭하면 크게 보기 · Ctrl/Shift+클릭으로 여러 개 선택</div>
        </div>
      </header>
      <FilterBar filter={filter} onChange={setFilter} />
      {loadError && <div className="alert bad">불러오기 실패: {loadError}</div>}

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

      {stats && total === 0 ? (
        <div className="empty">
          <div className="empty-icon">⛁</div>
          {Object.keys(filter).some((k) => k !== "sort" && k !== "desc" && (filter as any)[k] != null && (filter as any)[k] !== false) ? (
            <>
              <h2>조건에 맞는 항목이 없습니다</h2>
              <p>필터를 줄이거나 「모두 지우기」를 눌러 보세요.</p>
              <button className="btn" onClick={() => setFilter({ sort: filter.sort, desc: filter.desc })}>필터 모두 지우기</button>
            </>
          ) : (
            <>
              <h2>아직 사진이 없습니다</h2>
              <p>왼쪽 <b>드라이브 · 폴더</b>에서 「내 사진 폴더 빠르게 추가」를 누르거나 외장 드라이브를 추가하고 스캔하세요.</p>
            </>
          )}
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
                {r.title} {r.count > 0 && <span className="muted">{fmtCount(r.count)}{sort !== "taken" && !done ? "+" : ""}</span>}
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
                    {caption(item, sort) && <span className="tile-caption">{caption(item, sort)}</span>}
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
          onChanged={() => { api.timeline(filter).then(setTimeline); api.filteredStats(filter).then(setStats); }}
        />
      )}
    </div>
  );
}

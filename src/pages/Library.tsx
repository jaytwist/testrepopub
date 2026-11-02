import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Virtuoso, type ListRange } from "react-virtuoso";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  absPath, api, fmtBytes, fmtCount, fmtDate, fmtDuration, thumbUrl,
  type FileFilter, type FilteredStats, type GridItem, type GroupBy, type GroupRow, type SortKey,
} from "../api";
import { useJob } from "../App";
import FilterBar from "../components/FilterBar";
import Viewer from "../components/Viewer";

/** Items fetched per request inside one group. */
const PAGE = 300;
const TILE = 168;
const GAP = 4;

type Row = { type: "header"; key: string; g: GroupRow } | { type: "tiles"; key: string; g: GroupRow; row: number };

const GROUP_BYS: { key: GroupBy; name: string }[] = [
  { key: "month", name: "월별" },
  { key: "year", name: "연도별" },
  { key: "folder", name: "폴더별" },
  { key: "none", name: "묶지 않음" },
];

// --- small helpers ---------------------------------------------------------------------------

const store = {
  get(k: string): string | null {
    try { return localStorage.getItem(k); } catch { return null; }
  },
  set(k: string, v: string) {
    try { localStorage.setItem(k, v); } catch { /* private mode etc. */ }
  },
};

/** The filter restricted to one group (same rules the backend used to build the group). */
function groupFilter(base: FileFilter, g: GroupRow): FileFilter {
  if (g.key.startsWith("m:")) return { ...base, year: g.year, month: g.month };
  if (g.key.startsWith("y:")) return { ...base, year: g.year, month: null };
  if (g.key.startsWith("d:")) return { ...base, volume_id: g.volume_id, dir: g.dir };
  return base;
}

function groupTitle(g: GroupRow): { title: string; tip?: string } {
  if (g.key.startsWith("m:")) return { title: g.year ? `${g.year}년 ${g.month}월` : "날짜 없음" };
  if (g.key.startsWith("y:")) return { title: g.year ? `${g.year}년` : "날짜 없음" };
  if (g.key.startsWith("d:")) {
    const full = absPath({ mount: g.mount ?? "", rel_path: g.dir ?? "" });
    const sep = (g.mount ?? "").includes("\\") ? "\\" : "/";
    const rel = g.dir ? g.dir.split("/").join(sep) : "(최상위 폴더)";
    return { title: `${g.volume_label} › ${rel}`, tip: full };
  }
  return { title: "전체" };
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

// --- page ------------------------------------------------------------------------------------

export default function Library() {
  const { version, running, setRunning, lastDone } = useJob();
  const [filter, setFilter] = useState<FileFilter>({});
  const [groupBy, setGroupBy] = useState<GroupBy>(() => (store.get("mo.groupBy") as GroupBy) || "month");
  const [groups, setGroups] = useState<GroupRow[] | null>(null);
  const [stats, setStats] = useState<FilteredStats | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loadedSort, setLoadedSort] = useState<SortKey>("taken");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [cacheTick, setCacheTick] = useState(0);
  const [cols, setCols] = useState(6);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [viewId, setViewId] = useState<number | null>(null);
  const [bulk, setBulk] = useState<"label" | "date" | null>(null);
  const [bulkValue, setBulkValue] = useState("");
  const [busyGroup, setBusyGroup] = useState<string | null>(null);
  /** Result of the last delete, shown with an Undo button until dismissed. */
  const [deleted, setDeleted] = useState<{ batch_id: string; deleted: number; bytes: number; skipped_offline: number; failed: number } | null>(null);

  const wrapRef = useRef<HTMLDivElement>(null);
  const gen = useRef(0);
  /** group key -> items by position (sparse until loaded) */
  const cache = useRef(new Map<string, (GridItem | undefined)[]>());
  const inflight = useRef(new Set<string>());
  /** group key -> every id in the group (fetched when the group checkbox is used) */
  const groupIds = useRef(new Map<string, number[]>());
  const lastRange = useRef<ListRange>({ startIndex: 0, endIndex: 30 });

  // --- loading ---

  const reload = useCallback(() => {
    const g = ++gen.current;
    cache.current = new Map();
    inflight.current = new Set();
    groupIds.current = new Map();
    setCacheTick((t) => t + 1);
    Promise.all([api.groupSummary(filter, groupBy), api.filteredStats(filter)])
      .then(([gs, st]) => {
        if (g !== gen.current) return;
        setGroups(gs);
        setStats(st);
        setLoadedSort(filter.sort ?? "taken");
        setLoadError(null);
      })
      .catch((e) => g === gen.current && setLoadError(String(e)));
  }, [filter, groupBy]);

  useEffect(() => {
    setSelected(new Set());
    setAnchor(null);
    reload();
  }, [reload, version]);

  // Remember folds per grouping mode.
  useEffect(() => {
    store.set("mo.groupBy", groupBy);
    try {
      setCollapsed(new Set(JSON.parse(store.get(`mo.collapsed.${groupBy}`) ?? "[]")));
    } catch {
      setCollapsed(new Set());
    }
  }, [groupBy]);
  const saveCollapsed = (s: Set<string>) => {
    setCollapsed(s);
    store.set(`mo.collapsed.${groupBy}`, JSON.stringify(Array.from(s)));
  };

  const fetchPage = useCallback(
    (g: GroupRow, page: number) => {
      const k = `${g.key}#${page}`;
      const arr = cache.current.get(g.key);
      if (inflight.current.has(k) || (arr && arr[page * PAGE] !== undefined)) return;
      inflight.current.add(k);
      const myGen = gen.current;
      api
        .grid(groupFilter(filter, g), page * PAGE, PAGE)
        .then((items) => {
          if (myGen !== gen.current) return;
          const a = cache.current.get(g.key) ?? new Array<GridItem | undefined>(g.count);
          items.forEach((it, i) => (a[page * PAGE + i] = it));
          cache.current.set(g.key, a);
          setCacheTick((t) => t + 1);
        })
        .catch((e) => myGen === gen.current && setLoadError(String(e)))
        .finally(() => inflight.current.delete(k));
    },
    [filter],
  );

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
    for (const g of groups ?? []) {
      out.push({ type: "header", key: `h:${g.key}`, g });
      if (collapsed.has(g.key)) continue;
      const n = Math.ceil(g.count / cols);
      for (let r = 0; r < n; r++) out.push({ type: "tiles", key: `t:${g.key}:${r}`, g, row: r });
    }
    return out;
  }, [groups, collapsed, cols]);

  /** Load whatever pages the visible rows (plus a margin) need. */
  const loadRange = useCallback(
    (range: ListRange) => {
      lastRange.current = range;
      const from = Math.max(0, range.startIndex - 8);
      const to = Math.min(rows.length - 1, range.endIndex + 12);
      for (let i = from; i <= to; i++) {
        const r = rows[i];
        if (r?.type !== "tiles") continue;
        const first = r.row * cols;
        const last = Math.min(r.g.count - 1, first + cols - 1);
        fetchPage(r.g, Math.floor(first / PAGE));
        if (Math.floor(last / PAGE) !== Math.floor(first / PAGE)) fetchPage(r.g, Math.floor(last / PAGE));
      }
    },
    [rows, cols, fetchPage],
  );
  // Rows changed (filter, fold, resize) but the scroll range didn't: load for the same range again.
  useEffect(() => loadRange(lastRange.current), [loadRange]);

  // Loaded items in on-screen order: used by the viewer (←/→) and Shift+click ranges.
  const flat = useMemo(() => {
    const list: GridItem[] = [];
    for (const g of groups ?? []) {
      if (collapsed.has(g.key)) continue;
      const a = cache.current.get(g.key);
      if (a) for (const it of a) if (it) list.push(it);
    }
    return list;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [groups, collapsed, cacheTick]);
  const flatIndex = useMemo(() => new Map(flat.map((it, i) => [it.id, i])), [flat]);

  /** Fetch the next not-yet-loaded page in display order (viewer reaching the end). */
  const loadNext = () => {
    for (const g of groups ?? []) {
      if (collapsed.has(g.key)) continue;
      const a = cache.current.get(g.key);
      for (let p = 0; p * PAGE < g.count; p++) {
        if (!a || a[p * PAGE] === undefined) return fetchPage(g, p);
      }
    }
  };

  // --- selection ---

  const onTileClick = (e: React.MouseEvent, id: number) => {
    const index = flatIndex.get(id) ?? 0;
    if (e.shiftKey && anchor != null) {
      const [a, b] = [Math.min(anchor, index), Math.max(anchor, index)];
      const next = new Set(selected);
      for (let i = a; i <= b; i++) next.add(flat[i].id);
      setSelected(next);
    } else if (e.ctrlKey || e.metaKey || selected.size > 0) {
      const next = new Set(selected);
      next.has(id) ? next.delete(id) : next.add(id);
      setSelected(next);
      setAnchor(index);
    } else {
      setViewId(id);
    }
  };

  const toggleOne = (e: React.MouseEvent, id: number) => {
    e.stopPropagation();
    const next = new Set(selected);
    next.has(id) ? next.delete(id) : next.add(id);
    setSelected(next);
    setAnchor(flatIndex.get(id) ?? null);
  };

  /** How many of a group's files are selected (exact once its ids were fetched). */
  const selectedIn = (g: GroupRow): number => {
    const ids = groupIds.current.get(g.key);
    if (ids) return ids.reduce((n, id) => n + (selected.has(id) ? 1 : 0), 0);
    const a = cache.current.get(g.key);
    return a ? a.reduce((n, it) => n + (it && selected.has(it.id) ? 1 : 0), 0) : 0;
  };

  const toggleGroup = async (g: GroupRow) => {
    setBusyGroup(g.key);
    try {
      let ids = groupIds.current.get(g.key);
      if (!ids) {
        ids = await api.queryIds(groupFilter(filter, g));
        groupIds.current.set(g.key, ids);
      }
      const all = ids.every((id) => selected.has(id));
      const next = new Set(selected);
      ids.forEach((id) => (all ? next.delete(id) : next.add(id)));
      setSelected(next);
    } catch (e) {
      setLoadError(String(e));
    } finally {
      setBusyGroup(null);
    }
  };

  const toggleFold = (g: GroupRow) => {
    const next = new Set(collapsed);
    next.has(g.key) ? next.delete(g.key) : next.add(g.key);
    saveCollapsed(next);
  };
  const foldAll = () => saveCollapsed(new Set((groups ?? []).map((g) => g.key)));
  const unfoldAll = () => saveCollapsed(new Set());

  const selectAll = async () => setSelected(new Set(await api.queryIds(filter)));

  // --- actions on the selection ---

  const deleteSelected = useCallback(async () => {
    if (selected.size === 0 || running) return;
    const ids = Array.from(selected);
    let size = "";
    try {
      const st = await api.filteredStats({ ids });
      size = ` (${fmtBytes(st.bytes)})`;
    } catch { /* size is informational only */ }
    const ok = await confirm(
      `선택한 ${fmtCount(ids.length)}개${size}를 삭제할까요?\n\n` +
        "파일은 같은 드라이브의 격리 폴더로 옮겨집니다. 바로 아래 「되돌리기」나 「작업 기록」에서 복원할 수 있고, " +
        "「격리함 비우기」를 해야 완전히 지워집니다.",
      { title: "선택한 파일 삭제", kind: "warning", okLabel: "삭제", cancelLabel: "취소" },
    );
    if (!ok) return;
    try {
      await api.startDelete(ids);
      setRunning("delete");
    } catch (e) {
      setLoadError(String(e));
    }
  }, [selected, running, setRunning]);

  // Delete key = delete the selection (not while typing in a field or viewing a photo).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement).tagName;
      if (e.key !== "Delete" || viewId != null || tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
      e.preventDefault();
      deleteSelected();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [deleteSelected, viewId]);

  useEffect(() => {
    if (lastDone?.job === "delete" && lastDone.ok) setDeleted(lastDone.result);
    if (lastDone?.job === "undo" && lastDone.ok) setDeleted(null);
  }, [lastDone]);

  const undoDelete = async () => {
    if (!deleted) return;
    try {
      await api.startUndo(deleted.batch_id);
      setRunning("undo");
    } catch (e) {
      setLoadError(String(e));
    }
  };

  const applyBulk = async () => {
    const ids = Array.from(selected);
    if (bulk === "label" && bulkValue.trim()) await api.addLabel(ids, bulkValue.trim());
    if (bulk === "date" && bulkValue) await api.setDate(ids, bulkValue);
    setBulk(null);
    setBulkValue("");
    reload();
    setSelected(new Set());
  };

  // --- render ---

  const total = stats?.count ?? 0;
  const groupCount = groups?.length ?? 0;
  const foldedCount = (groups ?? []).filter((g) => collapsed.has(g.key)).length;
  const filtered = Object.keys(filter).some((k) => k !== "sort" && k !== "desc" && (filter as any)[k] != null && (filter as any)[k] !== false);
  const viewIndex = viewId != null ? flatIndex.get(viewId) : undefined;

  const header = (g: GroupRow) => {
    const n = selectedIn(g);
    const state = n === 0 ? "" : n >= g.count ? "on" : "some";
    const { title, tip } = groupTitle(g);
    const folded = collapsed.has(g.key);
    return (
      <div className={`group-head ${folded ? "folded" : ""}`}>
        <button
          className={`gcheck ${state}`}
          disabled={busyGroup === g.key}
          onClick={() => toggleGroup(g)}
          title={state === "on" ? "이 묶음 선택 해제" : "이 묶음 전체 선택"}
        >
          {state === "on" ? "✓" : state === "some" ? "–" : ""}
        </button>
        <button className="fold" onClick={() => toggleFold(g)} title={folded ? "펼치기" : "접기"}>
          {folded ? "▸" : "▾"}
        </button>
        <span className="group-title" onClick={() => toggleFold(g)} title={tip ?? title}>{title}</span>
        <span className="muted small">
          {fmtCount(g.count)}개 · {fmtBytes(g.bytes)}
          {n > 0 && <b className="accent-text"> · {fmtCount(n)}개 선택</b>}
        </span>
      </div>
    );
  };

  const tiles = (g: GroupRow, row: number) => {
    const a = cache.current.get(g.key);
    const start = row * cols;
    const end = Math.min(g.count, start + cols);
    const cells = [];
    for (let i = start; i < end; i++) {
      const item = a?.[i];
      if (!item) {
        cells.push(<div key={`s${i}`} className="tile skeleton" style={{ width: TILE, height: TILE }} />);
        continue;
      }
      const cap = caption(item, loadedSort);
      cells.push(
        <div
          key={item.id}
          className={`tile ${selected.has(item.id) ? "sel" : ""} ${item.online ? "" : "offline"}`}
          style={{ width: TILE, height: TILE }}
          onClick={(e) => onTileClick(e, item.id)}
        >
          <img loading="lazy" src={thumbUrl(item.id)} alt="" draggable={false} onError={(e) => ((e.target as HTMLImageElement).style.visibility = "hidden")} />
          <button className="check" onClick={(e) => toggleOne(e, item.id)}>{selected.has(item.id) ? "✓" : ""}</button>
          {item.kind === "video" && <span className="badge-video">▶ {fmtDuration(item.duration_ms)}</span>}
          {item.kind === "raw" && <span className="badge-raw">RAW</span>}
          {!item.online && <span className="badge-offline" title="드라이브 연결 안 됨">⏏</span>}
          {cap && <span className="tile-caption">{cap}</span>}
        </div>,
      );
    }
    return (
      <div className="tile-row" style={{ gridTemplateColumns: `repeat(${cols}, ${TILE}px)`, gap: GAP }}>
        {cells}
      </div>
    );
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
          <div className="muted small">클릭하면 크게 보기 · Ctrl/Shift+클릭으로 여러 개 선택 · 묶음 제목 왼쪽 동그라미로 묶음 전체 선택</div>
        </div>
      </header>
      <FilterBar filter={filter} onChange={setFilter} />
      {loadError && <div className="alert bad">불러오기 실패: {loadError}</div>}

      <div className="grid-toolbar">
        <label className="muted small">묶기</label>
        <div className="seg">
          {GROUP_BYS.map((b) => (
            <button key={b.key} className={groupBy === b.key ? "on" : ""} onClick={() => setGroupBy(b.key)}>{b.name}</button>
          ))}
        </div>
        {groupBy !== "none" && groupCount > 0 && (
          <>
            <span className="muted small">{fmtCount(groupCount)}개 묶음{foldedCount > 0 ? ` · ${fmtCount(foldedCount)}개 접힘` : ""}</span>
            <button className="btn small" onClick={foldAll} disabled={foldedCount === groupCount}>모두 접기</button>
            <button className="btn small" onClick={unfoldAll} disabled={foldedCount === 0}>모두 펼치기</button>
          </>
        )}
      </div>

      {deleted && deleted.deleted + deleted.skipped_offline + deleted.failed > 0 && (
        <div className={`alert ${deleted.failed ? "bad" : "ok"} undo-banner`}>
          <span>
            {fmtCount(deleted.deleted)}개({fmtBytes(deleted.bytes)})를 격리함으로 옮겼습니다.
            {deleted.skipped_offline > 0 && ` 연결되지 않은 드라이브의 ${fmtCount(deleted.skipped_offline)}개는 건너뛰었습니다.`}
            {deleted.failed > 0 && ` ${fmtCount(deleted.failed)}개는 옮기지 못했습니다.`}
          </span>
          {deleted.deleted > 0 && <button className="btn small" disabled={!!running} onClick={undoDelete}>되돌리기</button>}
          <button className="btn small ghost" onClick={() => setDeleted(null)}>닫기</button>
        </div>
      )}

      {selected.size > 0 && (
        <div className="selbar">
          <b>{fmtCount(selected.size)}개 선택</b>
          <button className="btn small" onClick={() => { setBulk("label"); setBulkValue(""); }}>라벨 추가</button>
          <button className="btn small" onClick={() => { setBulk("date"); setBulkValue(""); }}>날짜 지정</button>
          <button className="btn small danger" disabled={!!running} onClick={deleteSelected} title="Delete 키">🗑 삭제</button>
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
          {filtered ? (
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
          increaseViewportBy={800}
          computeItemKey={(_, r) => r.key}
          rangeChanged={loadRange}
          itemContent={(_, r) => (r.type === "header" ? header(r.g) : tiles(r.g, r.row))}
        />
      )}

      {viewIndex != null && (
        <Viewer
          items={flat}
          index={viewIndex}
          onIndex={(i) => {
            setViewId(flat[i].id);
            if (i > flat.length - 30) loadNext();
          }}
          onClose={() => setViewId(null)}
          onChanged={() => api.filteredStats(filter).then(setStats)}
        />
      )}
    </div>
  );
}

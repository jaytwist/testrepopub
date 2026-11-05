import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Virtuoso, type ListRange, type VirtuosoHandle } from "react-virtuoso";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  absPath, api, FAVORITE, fmtBytes, fmtCount, fmtDate, fmtDuration, thumbUrl,
  type FileFilter, type FilteredStats, type GridItem, type GroupBy, type GroupRow, type SortKey, type VolumeRow,
} from "../api";
import { useJob, useNav, type LibraryPreset } from "../App";
import FilterBar from "../components/FilterBar";
import Viewer from "../components/Viewer";
import TimelineScrubber, { type ScrubSegment } from "../components/TimelineScrubber";
import Compare from "../components/Compare";

/** Items fetched per request inside one group. */
const PAGE = 300;
/** Thumbnail size: adjustable (Ctrl+wheel / slider), remembered. */
const TILE_DEFAULT = 168;
const TILE_MIN = 96;
const TILE_MAX = 320;
const clampTile = (v: number) => Math.round(Math.min(TILE_MAX, Math.max(TILE_MIN, v)));
const GAP = 4;
/** Approximate list heights (CSS .group-head / .tile-row), for the date scrubber's proportions. */
const HEAD_PX = 48;
const HEAD_FOLDED_PX = 41;
const SCRUB_W = 56;

type Row = { type: "header"; key: string; g: GroupRow } | { type: "tiles"; key: string; g: GroupRow; row: number };

const GROUP_BYS: { key: GroupBy; name: string }[] = [
  { key: "month", name: "월별" },
  { key: "year", name: "연도별" },
  { key: "folder", name: "폴더별" },
  { key: "none", name: "묶지 않음" },
  // Only offered when a cleanup preset opens it.
  { key: "burst", name: "연사별" },
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
  if (g.key.startsWith("b:")) return { ...base, burst_id: g.burst_id };
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
  if (g.key.startsWith("b:")) return { title: `연사 · ${fmtDate(g.first_taken, true)}` };
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

interface LibraryProps {
  /** False while another screen is shown (the library stays mounted to keep its place). */
  active?: boolean;
  preset?: LibraryPreset | null;
  onClearPreset?: () => void;
  /** The preset's one-shot parts (pre-selection, review) were used: don't repeat them on return. */
  onConsumePreset?: () => void;
}

export default function Library({ active = true, preset, onClearPreset, onConsumePreset }: LibraryProps) {
  const nav = useNav();
  const { version, running, setRunning, lastDone } = useJob();
  // A preset (from cleanup suggestions, people, places) decides the first view, so the first load
  // already uses its filter and pre-selection.
  const [filter, setFilter] = useState<FileFilter>(() => preset?.filter ?? {});
  const [groupBy, setGroupBy] = useState<GroupBy>(() => preset?.groupBy ?? ((store.get("mo.groupBy") as GroupBy) || "month"));
  const [groups, setGroups] = useState<GroupRow[] | null>(null);
  const [stats, setStats] = useState<FilteredStats | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loadedSort, setLoadedSort] = useState<SortKey>("taken");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [cacheTick, setCacheTick] = useState(0);
  const [cols, setCols] = useState(6);
  const [tile, setTile] = useState(() => clampTile(Number(store.get("mo.tileSize")) || TILE_DEFAULT));
  const changeTile = useCallback((v: number) => {
    const t = clampTile(v);
    setTile(t);
    store.set("mo.tileSize", String(t));
  }, []);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [viewId, setViewId] = useState<number | null>(null);
  const [comparing, setComparing] = useState<number[] | null>(null);
  const [bulk, setBulk] = useState<"label" | "date" | "shift" | null>(null);
  const [bulkValue, setBulkValue] = useState("");
  const [busyGroup, setBusyGroup] = useState<string | null>(null);
  /** Show each burst as its best shot with a "+N" badge. */
  const [stack, setStack] = useState<boolean>(() => store.get("mo.stackBursts") === "1");
  /** One-by-one review (from cleanup suggestions): kind + files marked "keep" this session. */
  const [review, setReview] = useState<{ kind: string; title: string } | null>(() => (preset?.review ? { kind: preset.review, title: preset.title } : null));
  const [kept, setKept] = useState<Set<number>>(new Set());
  const reviewOpened = useRef(false);
  const [volumes, setVolumes] = useState<VolumeRow[]>([]);
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
  /** Ids to select once the preset's view has loaded. */
  /** Ids to select when the view for this exact filter object (and job version) loads. Idempotent,
   *  so React's double-run of effects in development doesn't lose it. */
  const pendingSelect = useRef<{ ids: number[]; filter: FileFilter; version: number } | null>(
    preset?.select ? { ids: preset.select, filter: preset.filter, version } : null,
  );
  const appliedPreset = useRef(preset?.key);

  useEffect(() => {
    if (preset?.select || preset?.review) onConsumePreset?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** The view a preset replaced (filter, grouping, scroll row), for 「← 이전 보기로」. */
  const prevView = useRef<{ filter: FileFilter; groupBy: GroupBy; row: number } | null>(null);
  const [hasPrev, setHasPrev] = useState(false);
  const pendingScroll = useRef<number | null>(null);

  // A new preset while the library is open replaces the current view (remembering it).
  useEffect(() => {
    if (!preset || preset.key === appliedPreset.current) return;
    appliedPreset.current = preset.key;
    prevView.current = { filter, groupBy, row: Math.floor(topRow) };
    setHasPrev(true);
    if (preset.select || preset.review) onConsumePreset?.();
    pendingSelect.current = preset.select ? { ids: preset.select, filter: preset.filter, version } : null;
    setFilter(preset.filter);
    if (preset.groupBy) setGroupBy(preset.groupBy);
    setReview(preset.review ? { kind: preset.review, title: preset.title } : null);
    setKept(new Set());
    reviewOpened.current = false;
  }, [preset?.key]);

  useEffect(() => {
    api.listVolumes().then(setVolumes).catch(() => {});
  }, [version]);

  // --- loading ---

  /** The filter actually sent to the backend (stacking applies outside the burst view). */
  /** "Show only the selection": the ids frozen when it was turned on. */
  const [onlySel, setOnlySel] = useState<number[] | null>(null);
  const qf: FileFilter = useMemo(
    () => ({ ...filter, stack_bursts: stack && groupBy !== "burst", ...(onlySel ? { ids: onlySel } : {}) }),
    [filter, stack, groupBy, onlySel],
  );

  const reload = useCallback(() => {
    const g = ++gen.current;
    cache.current = new Map();
    inflight.current = new Set();
    groupIds.current = new Map();
    setCacheTick((t) => t + 1);
    Promise.all([api.groupSummary(qf, groupBy), api.filteredStats(qf)])
      .then(([gs, st]) => {
        if (g !== gen.current) return;
        setGroups(gs);
        setStats(st);
        setLoadedSort(filter.sort ?? "taken");
        setLoadError(null);
      })
      .catch((e) => g === gen.current && setLoadError(String(e)));
  }, [qf, groupBy]);

  useEffect(() => {
    const p = pendingSelect.current;
    const keep = p != null && p.filter === filter && p.version === version;
    if (!keep) pendingSelect.current = null;
    setSelected(new Set(keep ? p.ids : []));
    setAnchor(null);
    reload();
  }, [reload, version]);

  // Remember folds per grouping mode.
  useEffect(() => {
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
        .grid(groupFilter(qf, g), page * PAGE, PAGE)
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
    [qf],
  );

  // Date scrubber on the right (chronological groupings only).
  const showScrub = (groupBy === "month" || groupBy === "year") && (groups?.length ?? 0) > 1;
  const scrubW = showScrub ? SCRUB_W : 0;

  // Columns follow the window width.
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setCols(Math.max(2, Math.floor((el.clientWidth - 24 - scrubW) / (tile + GAP)))));
    ro.observe(el);
    return () => ro.disconnect();
  }, [scrubW, tile]);

  const { rows, segments } = useMemo(() => {
    const out: Row[] = [];
    const segs: ScrubSegment[] = [];
    for (const g of groups ?? []) {
      const folded = collapsed.has(g.key);
      const n = folded ? 0 : Math.ceil(g.count / cols);
      segs.push({
        key: g.key,
        year: g.year,
        month: g.key.startsWith("m:") ? g.month : null,
        label: groupTitle(g).title,
        startRow: out.length,
        px: (folded ? HEAD_FOLDED_PX : HEAD_PX) + n * (tile + GAP),
      });
      out.push({ type: "header", key: `h:${g.key}`, g });
      for (let r = 0; r < n; r++) out.push({ type: "tiles", key: `t:${g.key}:${r}`, g, row: r });
    }
    return { rows: out, segments: segs };
  }, [groups, collapsed, cols, tile]);

  const virtuoso = useRef<VirtuosoHandle>(null);
  const [topRow, setTopRow] = useState(0);
  const scrollEl = useRef<HTMLElement | null>(null);
  /** First row whose bottom is below the top of the scroller (Virtuoso renders data-index). */
  const measureTop = useCallback(() => {
    const s = scrollEl.current;
    if (!s) return;
    const top = s.getBoundingClientRect().top;
    for (const it of Array.from(s.querySelectorAll<HTMLElement>("[data-index]"))) {
      const r = it.getBoundingClientRect();
      if (r.bottom > top + 1) {
        // Fractional: how far into this row the top of the view is (smooth position line).
        const into = r.height > 0 ? Math.min(1, Math.max(0, (top - r.top) / r.height)) : 0;
        setTopRow(Number(it.dataset.index) + into);
        return;
      }
    }
  }, []);
  const onScrollerRef = useCallback(
    (el: HTMLElement | Window | null) => {
      const prev = scrollEl.current;
      if (prev) prev.onscroll = null;
      scrollEl.current = el instanceof HTMLElement ? el : null;
      if (scrollEl.current) {
        let pending = false;
        scrollEl.current.onscroll = () => {
          if (pending) return;
          pending = true;
          requestAnimationFrame(() => {
            pending = false;
            measureTop();
          });
        };
      }
    },
    [measureTop],
  );
  // The list changed under the same scroll position (grouping, folding, loading).
  useEffect(() => {
    const t = setTimeout(measureTop, 50);
    return () => clearTimeout(t);
  }, [rows, measureTop]);
  /** Back to the view a preset replaced, at the same scroll position. */
  const goBack = () => {
    const p = prevView.current;
    if (!p) return;
    prevView.current = null;
    setHasPrev(false);
    onClearPreset?.();
    setReview(null);
    pendingScroll.current = p.row;
    setFilter(p.filter);
    setGroupBy(p.groupBy);
  };
  useEffect(() => {
    const row = pendingScroll.current;
    if (row == null || rows.length === 0) return;
    pendingScroll.current = null;
    setTimeout(() => virtuoso.current?.scrollToIndex({ index: Math.min(row, rows.length - 1), align: "start" }), 50);
  }, [rows]);

  const wheelBy = useCallback((dy: number) => scrollEl.current?.scrollBy({ top: dy }), []);
  const jumpTo = useCallback((row: number) => virtuoso.current?.scrollToIndex({ index: row, align: "start" }), []);

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

  // A review preset opens the viewer on the first candidate as soon as it has loaded.
  useEffect(() => {
    if (review && !reviewOpened.current && flat.length > 0) {
      reviewOpened.current = true;
      setViewId(flat[0].id);
    }
  }, [review, flat]);

  // --- drag a box to select (also started on a photo; a plain click still opens it) ---
  const gridRef = useRef<HTMLDivElement>(null);
  const suppressClick = useRef(false);
  const dragSel = useRef<{ x0: number; y0: number; x: number; y: number; base: Set<number>; hits: Set<number>; active: boolean } | null>(null);
  const [selRect, setSelRect] = useState<{ l: number; t: number; w: number; h: number } | null>(null);

  const evalDrag = useCallback(() => {
    const d = dragSel.current;
    const g = gridRef.current;
    if (!d || !g) return;
    const gr = g.getBoundingClientRect();
    const [l, r] = [Math.min(d.x0, d.x), Math.max(d.x0, d.x)];
    const [t, b] = [Math.min(d.y0, d.y), Math.max(d.y0, d.y)];
    setSelRect({ l: l - gr.left, t: t - gr.top, w: r - l, h: b - t });
    for (const el of Array.from(g.querySelectorAll<HTMLElement>(".tile[data-id]"))) {
      const er = el.getBoundingClientRect();
      if (er.right > l && er.left < r && er.bottom > t && er.top < b) d.hits.add(Number(el.dataset.id));
    }
    setSelected(new Set([...d.base, ...d.hits]));
  }, []);

  // While dragging near the top/bottom edge, keep scrolling (and keep collecting photos).
  const autoScroll = useCallback(() => {
    const d = dragSel.current;
    const s = scrollEl.current;
    if (!d?.active || !s) return;
    const sr = s.getBoundingClientRect();
    const edge = 48;
    const dy = d.y < sr.top + edge ? -(sr.top + edge - d.y) / 2 : d.y > sr.bottom - edge ? (d.y - (sr.bottom - edge)) / 2 : 0;
    if (dy !== 0) {
      s.scrollBy({ top: dy });
      evalDrag();
    }
    requestAnimationFrame(autoScroll);
  }, [evalDrag]);

  const onGridPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("button, input, a, .scrubber, .group-head")) return;
    const keep = e.ctrlKey || e.metaKey || e.shiftKey;
    dragSel.current = { x0: e.clientX, y0: e.clientY, x: e.clientX, y: e.clientY, base: keep ? new Set(selected) : new Set(), hits: new Set(), active: false };
  };
  const onGridPointerMove = (e: React.PointerEvent) => {
    const d = dragSel.current;
    if (!d) return;
    d.x = e.clientX;
    d.y = e.clientY;
    if (!d.active) {
      if (Math.hypot(d.x - d.x0, d.y - d.y0) < 8) return;
      d.active = true;
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      requestAnimationFrame(autoScroll);
    }
    evalDrag();
  };
  const onGridPointerUp = () => {
    const d = dragSel.current;
    dragSel.current = null;
    if (d?.active) {
      setSelRect(null);
      setAnchor(null);
      // The click that ends a drag must not open the photo under the pointer.
      suppressClick.current = true;
      setTimeout(() => (suppressClick.current = false), 60);
    }
  };

  // Ctrl + wheel over the grid: bigger / smaller thumbnails (instead of zooming the whole page).
  useEffect(() => {
    const g = gridRef.current;
    if (!g) return;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey) return;
      e.preventDefault();
      changeTile(tile * Math.exp(-e.deltaY * 0.0015));
    };
    g.addEventListener("wheel", onWheel, { passive: false });
    return () => g.removeEventListener("wheel", onWheel);
  });

  // Selection summary: how many, how big, which dates.
  const [selInfo, setSelInfo] = useState<FilteredStats | null>(null);
  useEffect(() => {
    if (selected.size === 0) {
      setSelInfo(null);
      return;
    }
    const t = setTimeout(() => api.filteredStats({ ids: Array.from(selected) }).then(setSelInfo).catch(() => {}), 250);
    return () => clearTimeout(t);
  }, [selected]);
  useEffect(() => {
    if (selected.size === 0 && onlySel) setOnlySel(null);
  }, [selected, onlySel]);
  const toggleOnlySel = () => {
    const ids = onlySel ? Array.from(selected) : Array.from(selected);
    // Keep the selection across the reload that changing the view causes.
    pendingSelect.current = { ids, filter, version };
    setOnlySel(onlySel ? null : ids);
  };

  // Backup check: needs content fingerprints for older entries (computed once, quickly).
  const [checkingCopies, setCheckingCopies] = useState(false);
  const setCopies = async (copies: FileFilter["copies"]) => {
    if (copies) {
      setCheckingCopies(true);
      try {
        await api.prepareCopies();
      } catch {
        /* the filter still works with what is known */
      }
      setCheckingCopies(false);
    }
    setFilter({ ...filter, copies: copies ?? null });
  };

  // --- selection ---

  const onTileClick = (e: React.MouseEvent, id: number) => {
    if (suppressClick.current) {
      suppressClick.current = false;
      return;
    }
    setCursor(id);
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
        ids = await api.queryIds(groupFilter(qf, g));
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

  const selectAll = async () => setSelected(new Set(await api.queryIds(qf)));

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

  // --- keyboard: a cursor that moves over the grid like in a file manager ---
  const [cursor, setCursor] = useState<number | null>(null);
  /** Where each loaded photo sits: list row + column, and the photos of each row. */
  const layout = useMemo(() => {
    const at = new Map<number, { row: number; col: number }>();
    const byRow = new Map<number, number[]>();
    for (const seg of segments) {
      const a = cache.current.get(seg.key);
      if (!a || collapsed.has(seg.key)) continue;
      a.forEach((it, pos) => {
        if (!it) return;
        const row = seg.startRow + 1 + Math.floor(pos / cols);
        at.set(it.id, { row, col: pos % cols });
        const r = byRow.get(row) ?? [];
        r[pos % cols] = it.id;
        byRow.set(row, r);
      });
    }
    return { at, byRow };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [segments, collapsed, cols, cacheTick]);

  const moveCursor = (dir: "left" | "right" | "up" | "down", extend: boolean) => {
    if (flat.length === 0) return;
    let next: number | undefined;
    const cur = cursor != null ? flatIndex.get(cursor) : undefined;
    if (cur == null) next = flat[0].id;
    else if (dir === "left") next = flat[Math.max(0, cur - 1)].id;
    else if (dir === "right") next = flat[Math.min(flat.length - 1, cur + 1)].id;
    else {
      const p = layout.at.get(cursor!);
      if (!p) return;
      const rowsWithItems = [...layout.byRow.keys()].sort((x, y) => x - y);
      const i = rowsWithItems.indexOf(p.row) + (dir === "down" ? 1 : -1);
      const ids = rowsWithItems[i] != null ? layout.byRow.get(rowsWithItems[i])!.filter((x) => x != null) : null;
      if (!ids || ids.length === 0) return;
      next = ids[Math.min(p.col, ids.length - 1)];
    }
    if (next == null) return;
    setCursor(next);
    const ni = flatIndex.get(next) ?? 0;
    if (extend) {
      const from = anchor ?? cur ?? ni;
      const [a, b] = [Math.min(from, ni), Math.max(from, ni)];
      const sel = new Set(selected);
      for (let k = a; k <= b; k++) sel.add(flat[k].id);
      setSelected(sel);
      if (anchor == null) setAnchor(from);
    } else {
      setAnchor(ni);
    }
    const row = layout.at.get(next)?.row;
    if (row != null) virtuoso.current?.scrollIntoView({ index: row });
    if (ni > flat.length - cols * 3) loadNext();
  };

  // Keys (not while typing, viewing a photo or comparing, nor while another screen is shown).
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement).tagName;
      if (viewId != null || comparing || tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
      const arrows: Record<string, "left" | "right" | "up" | "down"> = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
      if (arrows[e.key]) {
        e.preventDefault();
        moveCursor(arrows[e.key], e.shiftKey);
      } else if (e.key === " " && cursor != null) {
        e.preventDefault();
        const sel = new Set(selected);
        sel.has(cursor) ? sel.delete(cursor) : sel.add(cursor);
        setSelected(sel);
        setAnchor(flatIndex.get(cursor) ?? null);
      } else if (e.key === "Enter" && cursor != null) {
        e.preventDefault();
        setViewId(cursor);
      } else if ((e.ctrlKey || e.metaKey) && (e.key === "a" || e.key === "A")) {
        e.preventDefault();
        selectAll();
      } else if (e.key === "Escape") {
        if (selected.size > 0) setSelected(new Set());
        else setCursor(null);
      } else if (e.key === "f" || e.key === "F" || e.key === "ㄹ") {
        if (selected.size > 0) toggleFavSelected();
        else if (cursor != null) {
          const it = flat.find((x) => x.id === cursor);
          (it?.favorite ? api.removeLabel([cursor], FAVORITE) : api.addLabel([cursor], FAVORITE)).then(reload);
        }
      } else if (e.key === "Delete") {
        e.preventDefault();
        deleteSelected();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

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

  /** ♥ the selection, or un-♥ it when every selected (loaded) photo already is one. */
  const toggleFavSelected = async () => {
    const ids = Array.from(selected);
    const loaded = flat.filter((it) => selected.has(it.id));
    if (loaded.length > 0 && loaded.every((it) => it.favorite)) await api.removeLabel(ids, FAVORITE);
    else await api.addLabel(ids, FAVORITE);
    reload();
  };

  /** Move the capture time of the selection (camera clock / time zone wrong). */
  const shiftSelected = async (secs: number) => {
    if (!secs) return;
    await api.shiftDate(Array.from(selected), secs);
    setBulk(null);
    setBulkValue("");
    reload();
  };
  const shiftPreview = (secs: number) => {
    const first = flat.find((it) => selected.has(it.id) && it.taken_at != null);
    if (!first || first.taken_at == null || !secs) return null;
    return `${fmtDate(first.taken_at, true)} → ${fmtDate(first.taken_at + secs, true)}`;
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
        cells.push(<div key={`s${i}`} className="tile skeleton" style={{ width: tile, height: tile }} />);
        continue;
      }
      const cap = caption(item, loadedSort);
      cells.push(
        <div
          key={item.id}
          data-id={item.id}
          className={`tile ${selected.has(item.id) ? "sel" : ""} ${item.online ? "" : "offline"} ${cursor === item.id ? "cursor" : ""}`}
          style={{ width: tile, height: tile }}
          onClick={(e) => onTileClick(e, item.id)}
        >
          <img loading="lazy" src={thumbUrl(item.id)} alt="" draggable={false} onError={(e) => ((e.target as HTMLImageElement).style.visibility = "hidden")} />
          <button className="check" onClick={(e) => toggleOne(e, item.id)}>{selected.has(item.id) ? "✓" : ""}</button>
          {item.kind === "video" && <span className="badge-video">▶ {fmtDuration(item.duration_ms)}</span>}
          {item.kind === "raw" && <span className="badge-raw" style={item.favorite ? { right: 30 } : undefined}>RAW</span>}
          {item.favorite && <span className="badge-fav" title="즐겨찾기">♥</span>}
          {!item.online && <span className="badge-offline" title="드라이브 연결 안 됨">⏏</span>}
          {cap && <span className="tile-caption">{cap}</span>}
          {stack && groupBy !== "burst" && item.burst_rank === 0 && (item.burst_size ?? 0) > 1 && (
            <span className="badge-stack" title="연사 · 가장 잘 나온 사진">+{(item.burst_size ?? 1) - 1}</span>
          )}
          {groupBy === "burst" && item.burst_rank === 0 && <span className="badge-best">★ 베스트</span>}
          {kept.has(item.id) && <span className="badge-kept">남김</span>}
          {(filter.copies || filter.volume_id != null) && item.copies > 0 && (
            <span className="badge-copies" title="다른 드라이브에 똑같은 파일이 있습니다">사본 {item.copies}</span>
          )}
        </div>,
      );
    }
    return (
      <div className="tile-row" style={{ gridTemplateColumns: `repeat(${cols}, ${tile}px)`, gap: GAP }}>
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
          <div className="muted small">클릭하면 크게 보기 · 끌거나 Ctrl/Shift+클릭으로 여러 장 선택 · 키보드: 방향키 이동, Space 선택, Enter 보기, F 즐겨찾기, Ctrl+A 전체, Esc 해제</div>
        </div>
      </header>
      {preset && (
        <div className="alert ok preset-banner">
          <span><b>{preset.title}</b>{selected.size > 0 && ` · ${fmtCount(selected.size)}개 선택됨`}{kept.size > 0 && ` · ${fmtCount(kept.size)}개 남김`}</span>
          {review && flat.length > 0 && <button className="btn small primary" onClick={() => setViewId(flat[0].id)}>한 장씩 검토</button>}
          {hasPrev && (
            <button className="btn small" onClick={goBack} title="이 보기를 열기 전의 필터·묶기·위치로 돌아갑니다">← 이전 보기로</button>
          )}
          <button className="btn small ghost" onClick={() => { onClearPreset?.(); setFilter({}); prevView.current = null; setHasPrev(false); }}>닫고 전체 보기</button>
        </div>
      )}
      {volumes.filter((v) => v.file_count > 0).length > 1 && (
        <div className="drive-tabs" role="tablist" title="모든 드라이브를 합쳐 보거나 드라이브별로 따로 봅니다">
          <button className={filter.volume_id == null ? "on" : ""} onClick={() => setFilter({ ...filter, volume_id: null })}>
            모든 드라이브
          </button>
          {volumes.filter((v) => v.file_count > 0).map((v) => (
            <button
              key={v.id}
              className={`${filter.volume_id === v.id ? "on" : ""} ${v.online ? "" : "offline"}`}
              onClick={() => setFilter({ ...filter, volume_id: v.id })}
              title={`${v.mount}${v.online ? "" : " · 연결 안 됨 (썸네일만)"}`}
            >
              {v.removable ? "⏏" : "⛁"} {v.label} <span className="muted">{fmtCount(v.file_count)}</span>
            </button>
          ))}
          <span className="backup-check" title="다른 드라이브에 똑같은 파일(사본)이 있는지로 나눠 봅니다. 「이 드라이브에만」은 지우면 사라지는 사진입니다.">
            <span className="muted small">백업 확인</span>
            <span className="seg">
              <button className={!filter.copies ? "on" : ""} onClick={() => setCopies(null)}>전체</button>
              <button className={filter.copies === "only_here" ? "on" : ""} onClick={() => setCopies("only_here")}>
                {filter.volume_id != null ? "이 드라이브에만" : "한 곳에만"}
              </button>
              <button className={filter.copies === "elsewhere" ? "on" : ""} onClick={() => setCopies("elsewhere")}>다른 곳에도 있음</button>
            </span>
            {checkingCopies && <span className="muted small"><span className="spinner" /> 확인 중…</span>}
          </span>
        </div>
      )}
      <FilterBar filter={filter} onChange={setFilter} />
      {loadError && <div className="alert bad">불러오기 실패: {loadError}</div>}

      <div className="grid-toolbar">
        <label className="muted small">묶기</label>
        <div className="seg">
          {GROUP_BYS.filter((b) => b.key !== "burst" || groupBy === "burst").map((b) => (
            <button key={b.key} className={groupBy === b.key ? "on" : ""} onClick={() => { setGroupBy(b.key); store.set("mo.groupBy", b.key); }}>{b.name}</button>
          ))}
        </div>
        {groupBy !== "none" && groupCount > 0 && (
          <>
            <span className="muted small">{fmtCount(groupCount)}개 묶음{foldedCount > 0 ? ` · ${fmtCount(foldedCount)}개 접힘` : ""}</span>
            <button className="btn small" onClick={foldAll} disabled={foldedCount === groupCount}>모두 접기</button>
            <button className="btn small" onClick={unfoldAll} disabled={foldedCount === 0}>모두 펼치기</button>
          </>
        )}
        <label className="tile-size small" title="사진 크기 (Ctrl + 마우스 휠로도 조절)">
          <span className="muted">크기</span>
          <input type="range" min={TILE_MIN} max={TILE_MAX} step={8} value={tile} onChange={(e) => changeTile(Number(e.target.value))} />
        </label>
        {groupBy !== "burst" && (
          <label className="check-row small stack-toggle" title="연사로 찍은 사진을 가장 잘 나온 한 장으로 보여줍니다">
            <input type="checkbox" checked={stack} onChange={(e) => { setStack(e.target.checked); store.set("mo.stackBursts", e.target.checked ? "1" : "0"); }} />
            연사 겹쳐 보기
          </label>
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
          {selInfo && (
            <span className="muted small sel-info">
              {fmtBytes(selInfo.bytes)}
              {selInfo.first_taken != null && ` · ${fmtDate(selInfo.first_taken)}${selInfo.last_taken !== selInfo.first_taken ? ` ~ ${fmtDate(selInfo.last_taken)}` : ""}`}
            </span>
          )}
          <button className={`btn small ${onlySel ? "primary" : "ghost"}`} onClick={toggleOnlySel} title="고른 사진만 모아서 보기">
            {onlySel ? "전체 다시 보기" : "선택한 것만 보기"}
          </button>
          <button className="btn small" onClick={() => { setBulk("label"); setBulkValue(""); }}>라벨 추가</button>
          <button className="btn small" onClick={() => { setBulk("date"); setBulkValue(""); }}>날짜 지정</button>
          <button className="btn small" onClick={() => { setBulk("shift"); setBulkValue(""); }} title="카메라 시계·시간대가 틀린 사진을 한 번에 앞뒤로">시간 옮기기</button>
          <button className="btn small" onClick={toggleFavSelected} title="즐겨찾기 (F)">♥ 즐겨찾기</button>
          <button className="btn small" onClick={() => nav.openOrganize({ ids: Array.from(selected) })} title="고른 사진만 다른 폴더로 복사·이동">⇲ 한 곳으로 정리</button>
          <button
            className="btn small"
            disabled={selected.size < 2 || selected.size > 4}
            onClick={() => setComparing(Array.from(selected))}
            title={selected.size > 4 ? "한 번에 4장까지 비교할 수 있습니다" : "골라 둔 2–4장을 나란히 비교"}
          >
            ⇆ 비교{selected.size > 4 ? " (최대 4장)" : ""}
          </button>
          <button className="btn small danger" disabled={!!running} onClick={deleteSelected} title="Delete 키">🗑 삭제</button>
          <button className="btn small ghost" onClick={selectAll}>필터 결과 전체 선택</button>
          <button className="btn small ghost" onClick={() => setSelected(new Set())}>선택 해제</button>
          {bulk && (
            <span className="inline-form">
              {bulk === "label" ? (
                <input autoFocus placeholder="예: 가족여행, 졸업식" value={bulkValue} onChange={(e) => setBulkValue(e.target.value)} onKeyDown={(e) => e.key === "Enter" && applyBulk()} />
              ) : bulk === "date" ? (
                <input autoFocus type="datetime-local" value={bulkValue} onChange={(e) => setBulkValue(e.target.value)} />
              ) : (
                <>
                  {([["−9시간", -9 * 3600], ["−1시간", -3600], ["+1시간", 3600], ["+9시간", 9 * 3600], ["+1일", 86400]] as [string, number][]).map(([name, secs]) => (
                    <button key={name} className="chip ghost" onClick={() => setBulkValue(String(secs / 3600))}>{name}</button>
                  ))}
                  <input autoFocus type="number" step="0.5" className="hours-input" placeholder="시간 (예: -9)" value={bulkValue} onChange={(e) => setBulkValue(e.target.value)} />
                  <span className="muted small">시간</span>
                  {shiftPreview(Math.round(Number(bulkValue) * 3600)) && <span className="small">{shiftPreview(Math.round(Number(bulkValue) * 3600))}</span>}
                </>
              )}
              <button
                className="btn small primary"
                disabled={bulk === "shift" && !Number(bulkValue)}
                onClick={() => (bulk === "shift" ? shiftSelected(Math.round(Number(bulkValue) * 3600)) : applyBulk())}
              >
                적용
              </button>
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
        <div
          className={`grid-wrap ${selRect ? "selecting" : ""}`}
          ref={gridRef}
          onPointerDown={onGridPointerDown}
          onPointerMove={onGridPointerMove}
          onPointerUp={onGridPointerUp}
          onPointerCancel={onGridPointerUp}
        >
          {selRect && <div className="sel-rect" style={{ left: selRect.l, top: selRect.t, width: selRect.w, height: selRect.h }} />}
          <Virtuoso
            ref={virtuoso}
            scrollerRef={onScrollerRef}
            className="grid-scroller"
            style={{ height: "100%", marginRight: scrubW }}
            data={rows}
            increaseViewportBy={800}
            computeItemKey={(_, r) => r.key}
            rangeChanged={loadRange}
            itemContent={(_, r) => (r.type === "header" ? header(r.g) : tiles(r.g, r.row))}
          />
          {showScrub && <TimelineScrubber segments={segments} totalRows={rows.length} topRow={topRow} onJump={jumpTo} onWheel={wheelBy} />}
        </div>
      )}

      {comparing && <Compare ids={comparing} onClose={() => setComparing(null)} />}

      {viewIndex != null && (
        <Viewer
          items={flat}
          index={viewIndex}
          onIndex={(i) => {
            setViewId(flat[i].id);
            if (i > flat.length - 30) loadNext();
          }}
          onClose={() => setViewId(null)}
          onChanged={() => api.filteredStats(qf).then(setStats)}
          review={review ? {
            title: review.title,
            marked: selected,
            kept,
            onMark: (id) => {
              const next = new Set(selected);
              next.has(id) ? next.delete(id) : next.add(id);
              setSelected(next);
            },
            onKeep: (id) => {
              api.dismiss([id], review.kind).catch((e) => setLoadError(String(e)));
              setKept((k) => new Set(k).add(id));
              setSelected((s) => { const n = new Set(s); n.delete(id); return n; });
            },
            onFinish: () => {
              setViewId(null);
              if (selected.size > 0) deleteSelected();
            },
          } : undefined}
        />
      )}
    </div>
  );
}

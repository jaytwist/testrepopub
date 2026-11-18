import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api, fileUrl, fmtBytes, fmtCount, thumbUrl,
  type AlignFile, type AlignRow, type CopyFolderInfo, type FileDetail, type FirstDifference, type GridItem, type SwapGroup, type SwapReport,
} from "../api";
import { useJob } from "../App";
import Viewer from "./Viewer";

/** A folder of a set: where it is. */
export type WalkFolder = Pick<CopyFolderInfo, "volume_id" | "rel_dir" | "abs_path">;
const ref = (f: WalkFolder) => ({ volume_id: f.volume_id, rel_dir: f.rel_dir });

/** One set as the walk-through shows it: the folder that stays first, then its copies. */
export interface WalkSet {
  key: string;
  keep: WalkFolder;
  copies: WalkFolder[];
  /** Ticked for removal in the list behind (all of its copies). */
  checked: boolean;
  /** What ticking does, in words ("복사본 지우기" / "겹치는 것 지우기"); empty: nothing to tick. */
  action: string;
}

type Filter = "differs" | "only" | "unreadable" | "all";
const FILTERS: { id: Filter; label: string }[] = [
  { id: "differs", label: "다른 것" },
  { id: "only", label: "한쪽에만" },
  { id: "unreadable", label: "열 수 없음" },
  { id: "all", label: "전체" },
];

/**
 * A set photo by photo, its folders side by side: the original, copy 1, copy 2 … Opens on the
 * photos worth a look (same name, different content), with zoom shared by all panes and a blink
 * mode that shows the candidates in turn at the same spot. Per photo the user may keep a copy's
 * file instead of the original's (it is copied into the original folder in its place), or bring
 * in a photo only a copy has. Enter goes on to the next set.
 */
export default function FolderWalk({ sets, index, onIndex, onClose, onToggle, onReviewed }: {
  sets: WalkSet[];
  index: number;
  onIndex: (i: number) => void;
  onClose: () => void;
  onToggle: (key: string) => void;
  onReviewed: (key: string) => void;
}) {
  const { running, setRunning, lastDone } = useJob();
  const set = sets[index];
  // Which set is open, as text: the list behind is rebuilt whenever anything on the page
  // updates, and a new object for the same set must not start the walk over.
  const setId = set ? [set.key, set.keep.rel_dir, ...set.copies.map((c) => c.rel_dir)].join("|") : "";
  const setNow = useRef(set);
  setNow.current = set;
  const folders = useMemo(() => (setNow.current ? [setNow.current.keep, ...setNow.current.copies] : []), [setId]); // eslint-disable-line react-hooks/exhaustive-deps
  const [rows, setRows] = useState<AlignRow[] | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [cursor, setCursor] = useState(0);
  /** Row (index into `rows`) → the folder whose file is the one to keep. A copy (1…): its file
   *  goes into the original folder. The original (0): the copies' differing files are discarded. */
  const [choices, setChoices] = useState<Map<number, number>>(new Map());
  const [history, setHistory] = useState<number[]>([]);
  const [srcs, setSrcs] = useState<Record<number, string>>({});
  const [details, setDetails] = useState<Record<number, FileDetail>>({});
  const [broken, setBroken] = useState<Set<number>>(new Set());
  const [zoom, setZoom] = useState<{ x: number; y: number } | null>(null);
  const [blink, setBlink] = useState(false);
  const [tick, setTick] = useState(0);
  /** Pairs compared to the end ("original id:copy id"): where they first differ, or that they do not. */
  const [diffs, setDiffs] = useState<Record<string, FirstDifference>>({});
  /** Checking every photo of the set to the end: how far it is (null: not running). */
  const [sweep, setSweep] = useState<{ done: number; total: number; found: number } | null>(null);
  const sweeping = useRef(false);
  const [result, setResult] = useState<SwapReport | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [view, setView] = useState<GridItem[] | null>(null);
  const asked = useRef(new Set<number>());
  /** The rows as last loaded (to carry choices over to a reloaded list). */
  const rowsNow = useRef<AlignRow[] | null>(null);
  /** Tickets of the comparisons asked for by the photo in view: the newest one wins. */
  const ticket = useRef(Date.now());

  const load = useCallback((keepPlace: boolean) => {
    const set = setNow.current;
    if (!set) return;
    api.folderAlign(ref(set.keep), set.copies.map(ref)).then((r) => {
      // Choices are kept by row number: when the list comes back changed (a pair found to
      // differ becomes a row of its own), each choice follows its photo — by path — instead
      // of landing on whichever photo now has that number.
      const before = rowsNow.current;
      if (keepPlace && before) {
        const where = new Map(r.map((x, i) => [x.path, i]));
        const moved = (i: number) => (before[i] ? where.get(before[i].path) : undefined);
        setChoices((m) => new Map([...m].flatMap(([i, j]) => { const n = moved(i); return n == null ? [] : [[n, j] as [number, number]]; })));
        setHistory((h) => h.flatMap((i) => { const n = moved(i); return n == null ? [] : [n]; }));
      }
      rowsNow.current = r;
      setRows(r);
      if (keepPlace) return;
      // Open on what is worth a look; a set with nothing to look at opens on everything.
      const has = (k: Filter) => r.some((x) => (k === "unreadable" ? x.files.some((f) => f?.unreadable) : x.kind === k));
      setFilter(has("differs") ? "differs" : has("only") ? "only" : "all");
      setCursor(0);
      setChoices(new Map());
      setHistory([]);
    }).catch((e) => setErr(String(e)));
  }, [setId]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    setRows(null);
    setResult(null);
    setErr(null);
    setZoom(null);
    setBlink(false);
    load(false);
  }, [load]);
  useEffect(() => {
    if (lastDone?.job !== "cleanup:swap") return;
    if (lastDone.ok) setResult(lastDone.result as SwapReport);
    else if (lastDone.error !== "cancelled") setErr(lastDone.error ?? "사진을 넣지 못했습니다");
    setChoices(new Map());
    setHistory([]);
    rowsNow.current = null;
    load(true);
  }, [lastDone]); // eslint-disable-line react-hooks/exhaustive-deps

  const counts = useMemo(() => {
    const r = rows ?? [];
    return { all: r.length, differs: r.filter((x) => x.kind === "differs").length, only: r.filter((x) => x.kind === "only").length, unreadable: r.filter((x) => x.files.some((f) => f?.unreadable)).length };
  }, [rows]);
  /** The rows of the open filter, as indices into `rows`. */
  const shown = useMemo(
    () => (rows ?? []).map((_, i) => i).filter((i) => filter === "all" || (filter === "unreadable" ? rows![i].files.some((f) => f?.unreadable) : rows![i].kind === filter)),
    [rows, filter],
  );
  const at = Math.min(cursor, Math.max(0, shown.length - 1));
  const rowIndex = shown[at];
  const row = rowIndex != null ? rows![rowIndex] : null;
  /** What the app would pick where it can tell: the file that looks whole when the other is
   *  damaged, the one that opens when the other does not. The user's own choice goes first. */
  const suggested = useMemo(() => {
    const m = new Map<number, number>();
    (rows ?? []).forEach((r, i) => {
      const first = r.files[0];
      if (!first) return;
      r.files.forEach((f, j) => {
        if (j === 0 || !f || m.has(i)) return;
        const d = diffs[`${first.id}:${f.id}`];
        if (d?.at != null && d.whole_a != null && d.whole_b != null && d.whole_a !== d.whole_b) m.set(i, d.whole_a ? 0 : j);
        else if (!f.same && first.unreadable && !f.unreadable) m.set(i, j);
      });
    });
    return m;
  }, [rows, diffs]);
  /** Suggestions with the user's choices over them (-1: the user took a suggestion back). */
  const picked = useMemo(() => {
    const m = new Map([...suggested, ...choices]);
    for (const [i, j] of m) if (j < 0) m.delete(i);
    return m;
  }, [suggested, choices]);
  const chosen = rowIndex != null ? picked.get(rowIndex) : undefined;

  // Large pictures and details only of a photo the user stays on: while paging through, the
  // thumbnail stands in (it is there at once) and nothing is asked for that is passed by. The
  // next photo is fetched ahead once this one has been in view for a moment.
  useEffect(() => {
    const fetchRow = (i: number | undefined) => {
      for (const f of (i != null ? rows?.[i]?.files : null) ?? []) {
        if (!f || asked.current.has(f.id)) continue;
        asked.current.add(f.id);
        api.previewPath(f.id).then((p) => setSrcs((s) => ({ ...s, [f.id]: fileUrl(p) }))).catch(() => setSrcs((s) => ({ ...s, [f.id]: thumbUrl(f.id) })));
        api.fileDetail(f.id).then((d) => setDetails((s) => ({ ...s, [f.id]: d }))).catch(() => {});
      }
    };
    const now = setTimeout(() => fetchRow(rowIndex), 120);
    const ahead = setTimeout(() => fetchRow(shown[at + 1]), 600);
    return () => {
      clearTimeout(now);
      clearTimeout(ahead);
    };
  }, [rowIndex, at, shown, rows]);

  // The photo in view is compared to the end, copy by copy: the index calls two files the
  // same when their size and first 64 KiB agree, and a file damaged further in passes that.
  // (Large videos are left to the set-wide check: reading them takes a while.)
  const check = useCallback(async (a: AlignFile, b: AlignFile, ticket?: number) => {
    const key = `${a.id}:${b.id}`;
    const d = await api.folderFirstDifference(a.id, b.id, ticket);
    setDiffs((s) => ({ ...s, [key]: d }));
    return d;
  }, []);
  // Only for a photo the user stays on (a quarter of a second), one comparison at a time; on
  // moving to another photo the one under way is told to give up — paging fast through a set
  // must not leave dozens of files being read behind it.
  useEffect(() => {
    const first = row?.files[0];
    if (!first || sweeping.current) return;
    let alive = true;
    const timer = setTimeout(async () => {
      let found = false;
      for (const f of row!.files.slice(1)) {
        if (!alive) return;
        if (!f || diffs[`${first.id}:${f.id}`] || Math.max(first.size, f.size) > 64 << 20) continue;
        const d = await check(first, f, ++ticket.current).catch(() => null);
        if (d?.at != null && f.same) found = true;
      }
      // Found different although listed as the same: the index knows now — show it as such.
      if (found && alive) load(true);
    }, 250);
    return () => {
      alive = false;
      clearTimeout(timer);
      api.cancelFolderCheck(++ticket.current).catch(() => {});
    };
  }, [rowIndex, rows]); // eslint-disable-line react-hooks/exhaustive-deps
  const other = row?.files.find((f, n) => n > 0 && f != null && row.files[0] != null && diffs[`${row.files[0].id}:${f.id}`]?.at != null) ?? null;
  const pair = row?.files[0] && other ? `${row.files[0].id}:${other.id}` : null;

  /** Every photo of the set that is listed as the same, compared to the end. */
  const checkAll = async () => {
    if (!rows || sweeping.current) return;
    const todo = rows.flatMap((r) => (r.files[0] ? r.files.slice(1).filter((f): f is AlignFile => f != null && f.same && !diffs[`${r.files[0]!.id}:${f.id}`]).map((f) => [r.files[0]!, f] as const) : []));
    sweeping.current = true;
    let found = 0;
    setSweep({ done: 0, total: todo.length, found });
    for (let i = 0; i < todo.length && sweeping.current; i++) {
      const d = await check(todo[i][0], todo[i][1]).catch(() => null);
      if (d?.at != null) found += 1;
      setSweep({ done: i + 1, total: todo.length, found });
    }
    sweeping.current = false;
    if (found > 0) {
      load(true);
      setFilter("differs");
      setCursor(0);
    }
  };
  useEffect(() => () => { sweeping.current = false; }, [setId]);

  /** A copy's file can be picked unless it was compared with the original's and is the same to the end. */
  const identical = (r: AlignRow, j: number) => r.files[0] != null && r.files[j] != null && diffs[`${r.files[0]!.id}:${r.files[j]!.id}`]?.at === null;
  /** A copy's file that is known to differ from the original's (by the index, or compared just now). */
  const differing = (r: AlignRow, j: number) => r.files[0] != null && r.files[j] != null && (!r.files[j]!.same || diffs[`${r.files[0]!.id}:${r.files[j]!.id}`]?.at != null);
  /** What can be chosen: a copy's file unless it is the original's to the end; the original's
   *  where a copy's differs (choosing it lets that copy's file go). */
  const pickable = (r: AlignRow, j: number) => (j === 0 ? r.files.some((_, k) => k > 0 && differing(r, k)) : r.files[j] != null && !identical(r, j));
  const choose = (j: number) => {
    if (rowIndex == null || !row || !pickable(row, j)) return;
    // The same one again takes it back (a suggestion too).
    setChoices((m) => new Map(m).set(rowIndex, picked.get(rowIndex) === j ? -1 : j));
    setHistory((h) => [...h, rowIndex]);
  };
  const undo = () => {
    const last = history[history.length - 1];
    if (last == null) return;
    setHistory((h) => h.slice(0, -1));
    setChoices((m) => {
      const n = new Map(m);
      n.delete(last);
      return n;
    });
    const where = shown.indexOf(last);
    if (where >= 0) setCursor(where);
  };
  const bringAll = () => {
    setChoices((m) => {
      const n = new Map(m);
      (rows ?? []).forEach((r, i) => {
        const j = r.files.findIndex((f, k) => k > 0 && f != null);
        if (r.files[0] == null && j > 0) n.set(i, j);
      });
      return n;
    });
  };
  const copyOnly = (rows ?? []).filter((r) => r.files[0] == null).length;

  const apply = async () => {
    if (!set || !rows || picked.size === 0) return;
    const groups: SwapGroup[] = set.copies
      .map((c, n) => ({
        copy: ref(c),
        choices: [...picked.entries()].flatMap(([i, j]) => {
          const [k, f] = [rows[i].files[0], rows[i].files[n + 1]];
          if (j === n + 1 && f) return [{ keep_file: k?.id ?? null, copy_file: f.id }];
          if (j === 0 && k && f && differing(rows[i], n + 1)) return [{ keep_file: k.id, copy_file: f.id, discard: true }];
          return [];
        }),
      }))
      .filter((g) => g.choices.length > 0);
    setErr(null);
    setResult(null);
    try {
      await api.startFolderSwap(ref(set.keep), groups);
      setRunning("cleanup:swap");
    } catch (e) {
      setErr(String(e));
    }
  };

  const go = (to: number) => {
    if (to < 0 || to >= sets.length) return;
    if (set) onReviewed(set.key);
    onIndex(to);
  };
  const close = () => {
    if (set) onReviewed(set.key);
    onClose();
  };

  // The files of the photo in view, for blinking (those known to be the same to the end as
  // the original's are left out: there is nothing to see between them).
  const candidates = useMemo(() => {
    const out: number[] = [];
    row?.files.forEach((f, j) => {
      if (f && !(j > 0 && identical(row, j))) out.push(j);
    });
    return out;
  }, [row, diffs]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (!blink) return;
    const t = setInterval(() => setTick((n) => n + 1), 700);
    return () => clearInterval(t);
  }, [blink]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (view) return;
      const k = e.key;
      if (k === "Escape") return blink ? setBlink(false) : zoom ? setZoom(null) : close();
      if (k === "ArrowRight") setCursor(Math.min(shown.length - 1, at + 1));
      else if (k === "ArrowLeft") setCursor(Math.max(0, at - 1));
      else if (k >= "1" && k <= "4") choose(Number(k) - 1);
      else if (k === "b" || k === "B") setBlink((b) => !b);
      else if (k === "z" || k === "Z") undo();
      else if (k === "Enter") go(index + 1);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }); // every render: the handlers close over the current state

  const openViewer = async (id: number) => {
    const items = await api.grid({ ids: [id] }, 0, 1).catch(() => [] as GridItem[]);
    if (items.length) setView(items);
  };

  if (!set) return null;
  const d = pair ? diffs[pair] : null;
  const diffText = d && d.at != null
    ? d.size_a !== d.size_b
      ? `크기가 다릅니다 (${fmtBytes(d.size_a)} / ${fmtBytes(d.size_b)}) · 앞에서부터 ${Math.floor((d.at / Math.max(1, Math.min(d.size_a, d.size_b))) * 100)}% 지점까지는 같습니다`
      : `파일의 ${Math.floor((d.at / Math.max(1, d.size_a)) * 100)}% 지점부터 내용이 다릅니다 — 사진이라면 그 아래쪽이 깨졌는지 보세요`
    : null;

  const pane = (j: number, wide = false) => {
    const f = row?.files[j] ?? null;
    const info = f ? details[f.id] : null;
    const isChosen = chosen === j;
    const canPick = row != null && f != null && pickable(row, j);
    return (
      <div key={j} className={`fw-pane ${j === 0 ? "keep" : ""} ${isChosen ? "chosen" : ""} ${wide ? "wide" : ""}`}>
        <div className="fw-pane-head">
          <span className={`cf-tag ${j === 0 ? "keep" : "go"}`}>{j === 0 ? "원본" : `복사 ${j}`}</span>
          <span className="fw-folder" title={folders[j].abs_path}><bdi>{folders[j].abs_path}</bdi></span>
          <kbd>{j + 1}</kbd>
        </div>
        <div
          className={`cmp-img fw-img ${zoom ? "zoomed" : ""}`}
          onClick={(e) => {
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            setZoom(zoom ? null : { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height });
          }}
          onMouseMove={(e) => {
            if (!zoom) return;
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            setZoom({ x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height });
          }}
        >
          {f ? (
            <img
              src={f.video ? thumbUrl(f.id) : srcs[f.id] ?? thumbUrl(f.id)}
              alt=""
              draggable={false}
              style={zoom ? { transform: "scale(3)", transformOrigin: `${zoom.x * 100}% ${zoom.y * 100}%` } : undefined}
              onError={(e) => {
                setBroken((b) => new Set(b).add(f.id));
                const img = e.target as HTMLImageElement;
                if (!img.src.includes("thumb")) img.src = thumbUrl(f.id);
              }}
            />
          ) : (
            <span className="fw-none">이 폴더에는 없음</span>
          )}
          {f?.video && <span className="badge-video">▶ 영상</span>}
        </div>
        {f ? (
          <>
            <div className="fw-name">{f.name} <span className="muted">· {fmtBytes(f.size)}{info?.file.width ? ` · ${info.file.width}×${info.file.height}` : ""}{info?.taken ? ` · ${info.taken}` : ""}</span></div>
            <div className="fw-tags">
              {row!.files[0] && j > 0 && (
                identical(row!, j) ? <span className="fw-tag same">원본과 끝까지 같음 ✓</span>
                  : !f.same || diffs[`${row!.files[0]!.id}:${f.id}`]?.at != null ? <span className="fw-tag diff">원본과 내용 다름</span>
                    : <span className="fw-tag" title="크기와 앞부분이 같습니다. 끝까지 비교하면 확실해집니다.">원본과 같은 파일로 보임{Math.max(f.size, row!.files[0]!.size) > 64 << 20 ? " (큰 파일 — 묶음 검사에서 확인)" : " · 확인 중…"}</span>
              )}
              {row!.files[0] == null && <span className="fw-tag diff">원본 폴더에 없음</span>}
              {(() => {
                // Whole or damaged, where this file was compared with the original's.
                const first = row!.files[0];
                const d = first ? (j === 0 ? row!.files.slice(1).map((c) => (c ? diffs[`${first.id}:${c.id}`] : undefined)).find((x) => x?.at != null) : diffs[`${first.id}:${f.id}`]) : undefined;
                const ok = d?.at != null ? (j === 0 ? d.whole_a : d.whole_b) : null;
                return ok === false ? <span className="fw-tag bad" title="다른 쪽에는 없는 빈(0으로 채워진) 구간이 있거나, 중간에서 잘려 있습니다">빈 구간·잘림 있음 · 깨진 파일로 보임</span>
                  : ok === true ? <span className="fw-tag same" title="다른 쪽과 달리 빈 구간이나 잘림이 없습니다">온전해 보임 ✓</span>
                    : (f.unreadable || broken.has(f.id)) ? <span className="fw-tag bad">열 수 없음 · 깨짐 의심</span> : null;
              })()}
              <button className="btn small ghost" onClick={() => openViewer(f.id)}>{f.video ? "재생" : "자세히"}</button>
            </div>
            {canPick ? (
              <button className={`btn small fw-pick ${isChosen ? "primary" : ""}`} onClick={() => choose(j)}>
                {isChosen ? "● " : "○ "}
                {isChosen && rowIndex != null && !choices.has(rowIndex) && "(추천) "}
                {j === 0 ? "원본 것을 남김 — 복사본의 다른 파일은 격리함으로" : row!.files[0] ? "이쪽을 남김 — 원본 폴더의 것을 이것으로 바꿈" : "원본 폴더로 가져오기"}
              </button>
            ) : (
              <div className="muted small fw-pick">{j === 0 ? "" : "원본과 끝까지 같은 파일이라 고를 것이 없습니다"}</div>
            )}
          </>
        ) : (
          <div className="fw-name muted">—</div>
        )}
      </div>
    );
  };

  const strip = shown.slice(Math.max(0, at - 10), at + 11);
  const stripFrom = Math.max(0, at - 10);

  return (
    <div className="compare fw">
      <header className="cmp-head">
        <b>묶음 {index + 1} / {fmtCount(sets.length)}</b>
        <div className="fp-tabs">
          {FILTERS.map((f) => (
            <button key={f.id} className={`fp-tab ${filter === f.id ? "on" : ""} ${f.id === "differs" && counts.differs > 0 ? "alert-tab" : ""}`} disabled={counts[f.id] === 0} onClick={() => { setFilter(f.id); setCursor(0); }}>
              {f.label} {fmtCount(counts[f.id])}
            </button>
          ))}
        </div>
        <span className="spacer" />
        {set.action && (
          <label className="cf-check" title="이 창을 닫으면 목록에 체크되어 있습니다">
            <input type="checkbox" checked={set.checked} onChange={() => onToggle(set.key)} /> 이 묶음의 {set.action}
          </label>
        )}
        <button className="btn small" disabled={index === 0} onClick={() => go(index - 1)}>이전 묶음</button>
        <button className="btn small" disabled={index >= sets.length - 1} onClick={() => go(index + 1)}>다음 묶음 ⏎</button>
        <button className="btn small" onClick={close}>닫기</button>
      </header>
      {err && <div className="alert bad">{err}</div>}
      {result && (
        <div className={`alert ${result.failed ? "warn" : "ok"} small`}>
          {result.replaced > 0 && `원본 폴더의 사진 ${fmtCount(result.replaced)}장을 복사본 것으로 바꿨습니다(바뀐 옛 파일은 격리함에 있고 「작업 기록」에서 되돌릴 수 있습니다). `}
          {result.brought > 0 && `복사본에만 있던 ${fmtCount(result.brought)}장을 원본 폴더로 가져왔습니다. `}
          {result.discarded > 0 && `원본과 달랐던 복사본 ${fmtCount(result.discarded)}장을 격리함으로 옮겼습니다(「작업 기록」에서 되돌릴 수 있습니다). `}
          {result.failed > 0 && `실패 ${fmtCount(result.failed)}장: ${result.errors.slice(0, 2).join(" · ")}`}
        </div>
      )}

      {!rows ? (
        <div className="muted">두 폴더를 맞춰 보는 중…</div>
      ) : !row ? (
        <div className="card muted">
          {counts.all === 0 ? "이 폴더들에는 사진·영상이 없습니다." : "이 묶음에는 해당하는 사진이 없습니다."}
          {counts.differs === 0 && counts.only === 0 && counts.all > 0 && " 모든 폴더의 사진이 완전히 같습니다."}
        </div>
      ) : (
        <>
          <div className="fw-where">
            <b>{row.path}</b> <span className="muted">· {fmtCount(at + 1)} / {fmtCount(shown.length)}</span>

            {diffText && <span className="fw-diff">{diffText}</span>}
          </div>
          {blink && candidates.length > 1 ? (
            <div className="fw-panes one">{pane(candidates[tick % candidates.length], true)}</div>
          ) : (
            <div className="fw-panes" style={{ gridTemplateColumns: `repeat(${folders.length}, minmax(0, 1fr))` }}>{folders.map((_, j) => pane(j))}</div>
          )}
          <div className="fw-strip">
            <button className="btn small" disabled={at === 0} onClick={() => setCursor(at - 1)}>◀</button>
            {strip.map((i, n) => {
              const first = rows[i].files.find((f) => f != null)!;
              return (
                <button key={i} className={`fw-thumb ${stripFrom + n === at ? "cur" : ""}`} title={rows[i].path} onClick={() => setCursor(stripFrom + n)}>
                  <img src={thumbUrl(first.id)} alt="" loading="lazy" draggable={false} />
                  {rows[i].kind === "differs" && <span className="fw-dot" />}
                  {picked.has(i) && <span className="fw-ok">✓</span>}
                </button>
              );
            })}
            <button className="btn small" disabled={at >= shown.length - 1} onClick={() => setCursor(at + 1)}>▶</button>
          </div>
        </>
      )}

      <footer className="cmp-foot">
        <span className="muted small">
          ◀ ▶ 사진 이동 · <kbd>1</kbd>~<kbd>{folders.length}</kbd> 고르기 · <kbd>B</kbd> 번갈아 보기{blink ? " (켜짐)" : ""} · <kbd>Z</kbd> 되돌리기 · 사진을 누르면 모든 칸이 같은 곳을 확대 · <kbd>Enter</kbd> 다음 묶음
        </span>
        <span className="spacer" />
        {sweep ? (
          <span className="small">
            {sweep.done < sweep.total ? `끝까지 검사 중 ${fmtCount(sweep.done)} / ${fmtCount(sweep.total)}` : `검사 끝 · ${fmtCount(sweep.total)}쌍`}
            {sweep.found > 0 ? <b className="warn-text"> · 다른 것 {fmtCount(sweep.found)}장 발견</b> : sweep.done >= sweep.total ? " · 모두 끝까지 같음 ✓" : ""}
            {sweep.done < sweep.total && <button className="btn small ghost" onClick={() => { sweeping.current = false; }}>멈춤</button>}
          </span>
        ) : (
          <button className="btn small" disabled={!rows} title="같은 파일로 보이는 사진을 전부 끝까지 비교해, 뒤쪽이 깨진 것을 찾습니다" onClick={checkAll}>이 묶음 끝까지 검사</button>
        )}
        {copyOnly > 0 && <button className="btn small" onClick={bringAll}>복사본에만 있는 {fmtCount(copyOnly)}장 모두 고르기</button>}
        <button className={`btn small ${blink ? "primary" : ""}`} disabled={candidates.length < 2} onClick={() => setBlink((b) => !b)}>번갈아 보기</button>
        <button className="btn primary" disabled={picked.size === 0 || !!running} onClick={apply}>고른 {fmtCount(picked.size)}장 적용</button>
      </footer>
      {view && <Viewer items={view} index={0} onIndex={() => {}} onClose={() => setView(null)} />}
    </div>
  );
}

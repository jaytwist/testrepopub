import { useEffect, useMemo, useState } from "react";
import { FAVORITE,
  api, dateInputToTs, fmtBytes, fmtCount, fmtDate, labelName, tsToDateInput,
  type FileFilter, type FilterOptions, type LabelCount, type SortKey, type VolumeRow,
} from "../api";
import { useJob } from "../App";

interface Props {
  filter: FileFilter;
  onChange: (f: FileFilter) => void;
  /** Hide the sort control (e.g. on the Organize page where order doesn't matter). */
  showSort?: boolean;
}

const MB = 1024 * 1024;
const GB = 1024 * MB;
const DAY = 86400;

// --- Presets --------------------------------------------------------------------------------

type Range = [number | null, number | null];

const SIZE_PRESETS: [string, Range][] = [
  ["1MB 미만", [null, MB - 1]],
  ["1–10MB", [MB, 10 * MB]],
  ["10–100MB", [10 * MB, 100 * MB]],
  ["100MB–1GB", [100 * MB, GB]],
  ["1GB 이상", [GB, null]],
];
const MP_PRESETS: [string, Range][] = [
  ["작은 사진 (2MP 미만)", [null, 2]],
  ["보통 (2–12MP)", [2, 12]],
  ["고해상도 (12MP 이상)", [12, null]],
];
const DUR_PRESETS: [string, Range][] = [
  ["1분 미만", [null, 59]],
  ["1–10분", [60, 600]],
  ["10분 이상", [600, null]],
];

export const SORTS: { key: SortKey; name: string; desc: string; asc: string }[] = [
  { key: "taken", name: "촬영일", desc: "최신순", asc: "오래된순" },
  { key: "mtime", name: "수정일", desc: "최신순", asc: "오래된순" },
  { key: "size", name: "파일 크기", desc: "큰 것부터", asc: "작은 것부터" },
  { key: "resolution", name: "해상도", desc: "높은 것부터", asc: "낮은 것부터" },
  { key: "duration", name: "동영상 길이", desc: "긴 것부터", asc: "짧은 것부터" },
  { key: "name", name: "파일 이름", desc: "역순", asc: "가나다순" },
];

/** Wall-clock "today 00:00" as stored in the DB (local date, encoded as UTC). */
function todayTs() {
  const n = new Date();
  return Date.UTC(n.getFullYear(), n.getMonth(), n.getDate()) / 1000;
}
function yearRange(y: number): Range {
  return [Date.UTC(y, 0, 1) / 1000, Date.UTC(y, 11, 31, 23, 59, 59) / 1000];
}
function periodPresets(years: number[]): [string, string, Range][] {
  const y = new Date().getFullYear();
  return [
    ["all", "전체 기간", [null, null]],
    ["30d", "최근 30일", [todayTs() - 30 * DAY, null]],
    ["1y", "최근 1년", [todayTs() - 365 * DAY, null]],
    ["thisyear", `올해 (${y})`, [yearRange(y)[0], null]],
    ["lastyear", `작년 (${y - 1})`, yearRange(y - 1)],
    ...years.filter((yy) => yy < y - 1).map((yy) => [`y${yy}`, `${yy}년`, yearRange(yy)] as [string, string, Range]),
  ];
}

const sameRange = (a: Range, b: Range) => a[0] === b[0] && a[1] === b[1];

// --- Component ------------------------------------------------------------------------------

export default function FilterBar({ filter, onChange, showSort = true }: Props) {
  const { version } = useJob();
  const [opts, setOpts] = useState<FilterOptions | null>(null);
  const [labels, setLabels] = useState<LabelCount[]>([]);
  const [volumes, setVolumes] = useState<VolumeRow[]>([]);
  const [text, setText] = useState(filter.text ?? "");
  const [open, setOpen] = useState(false);
  const [customPeriod, setCustomPeriod] = useState(false);

  useEffect(() => {
    api.filterOptions().then(setOpts).catch(() => {});
    api.labelCounts().then(setLabels).catch(() => {});
    api.listVolumes().then(setVolumes).catch(() => {});
  }, [version]);

  // Debounced free-text search.
  useEffect(() => {
    const t = setTimeout(() => {
      if ((filter.text ?? "") !== text) onChange({ ...filter, text: text || null });
    }, 300);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text]);
  useEffect(() => setText(filter.text ?? ""), [filter.text]);

  const set = (patch: Partial<FileFilter>) => onChange({ ...filter, ...patch });
  const periods = useMemo(() => periodPresets(opts?.years ?? []), [opts]);
  const curPeriod: Range = [filter.date_from ?? null, filter.date_to ?? null];
  const periodKey = customPeriod ? "custom" : periods.find(([, , r]) => sameRange(r, curPeriod))?.[0] ?? "custom";
  const sort = filter.sort ?? "taken";
  const desc = filter.desc ?? true;
  const sortDef = SORTS.find((s) => s.key === sort)!;
  const userLabels = labels.filter((l) => l.user || l.label.startsWith("src:") || l.label === "screenshot" || l.label === "edited");

  // Active filters as removable chips.
  const chips: { text: string; clear: Partial<FileFilter> }[] = [];
  if (filter.date_from != null || filter.date_to != null) {
    const p = periods.find(([, , r]) => sameRange(r, curPeriod));
    chips.push({
      text: p ? p[1] : `${filter.date_from != null ? fmtDate(filter.date_from) : "처음"} ~ ${filter.date_to != null ? fmtDate(filter.date_to) : "지금"}`,
      clear: { date_from: null, date_to: null },
    });
  }
  if (filter.size_min != null || filter.size_max != null) {
    const p = SIZE_PRESETS.find(([, r]) => sameRange(r, [filter.size_min ?? null, filter.size_max ?? null]));
    chips.push({
      text: `크기 ${p ? p[0] : `${filter.size_min != null ? fmtBytes(filter.size_min) : "0"} ~ ${filter.size_max != null ? fmtBytes(filter.size_max) : "∞"}`}`,
      clear: { size_min: null, size_max: null },
    });
  }
  if (filter.mp_min != null || filter.mp_max != null) {
    const p = MP_PRESETS.find(([, r]) => sameRange(r, [filter.mp_min ?? null, filter.mp_max ?? null]));
    chips.push({ text: p ? p[0] : `${filter.mp_min ?? 0}–${filter.mp_max ?? "∞"}MP`, clear: { mp_min: null, mp_max: null } });
  }
  if (filter.dur_min != null || filter.dur_max != null) {
    const p = DUR_PRESETS.find(([, r]) => sameRange(r, [filter.dur_min ?? null, filter.dur_max ?? null]));
    chips.push({ text: `길이 ${p ? p[0] : "직접 지정"}`, clear: { dur_min: null, dur_max: null } });
  }
  if (filter.camera) chips.push({ text: `📷 ${filter.camera}`, clear: { camera: null } });
  if (filter.ext) chips.push({ text: filter.ext.toUpperCase(), clear: { ext: null } });
  if (filter.label) chips.push({ text: `라벨: ${labelName(filter.label)}`, clear: { label: null } });
  if (filter.volume_id != null) {
    const v = volumes.find((x) => x.id === filter.volume_id);
    chips.push({ text: `드라이브: ${v?.label ?? filter.volume_id}`, clear: { volume_id: null } });
  }
  if (filter.date_confidence) {
    const n: Record<string, string> = { high: "날짜 확실", medium: "파일명 날짜", low: "날짜 확인 필요" };
    chips.push({ text: n[filter.date_confidence] ?? filter.date_confidence, clear: { date_confidence: null } });
  }
  if (filter.has_gps != null) chips.push({ text: filter.has_gps ? "위치정보 있음" : "위치정보 없음", clear: { has_gps: null } });
  if (filter.online_only) chips.push({ text: "연결된 드라이브만", clear: { online_only: false } });
  const advancedCount = chips.length - (filter.date_from != null || filter.date_to != null ? 1 : 0);

  const clearAll = () => {
    setText("");
    setCustomPeriod(false);
    onChange({ kind: filter.kind, sort: filter.sort, desc: filter.desc });
  };

  const presetRow = (presets: [string, Range][], cur: Range, apply: (r: Range) => void) => (
    <div className="chips">
      <button className={`chip ghost ${cur[0] == null && cur[1] == null ? "on" : ""}`} onClick={() => apply([null, null])}>전체</button>
      {presets.map(([name, r]) => (
        <button key={name} className={`chip ghost ${sameRange(r, cur) ? "on" : ""}`} onClick={() => apply(sameRange(r, cur) ? [null, null] : r)}>{name}</button>
      ))}
    </div>
  );

  const kinds: [string | null, string][] = [[null, "전체"], ["photo", "사진"], ["video", "동영상"], ["raw", "RAW"], ["screenshot", "스크린샷"]];

  return (
    <div className="filterbar-wrap">
      <div className="filterbar">
        <div className="seg">
          {kinds.map(([k, name]) => (
            <button key={name} className={(filter.kind ?? null) === k ? "on" : ""} onClick={() => set({ kind: k })}>{name}</button>
          ))}
          <button
            className={`fav-tab ${filter.label === FAVORITE ? "on" : ""}`}
            title="즐겨찾기(♥)만 보기"
            onClick={() => set({ label: filter.label === FAVORITE ? null : FAVORITE })}
          >
            ♥
          </button>
        </div>
        <select
          value={periodKey}
          onChange={(e) => {
            const key = e.target.value;
            if (key === "custom") {
              setCustomPeriod(true);
              setOpen(true);
              return;
            }
            setCustomPeriod(false);
            const r = periods.find(([k]) => k === key)?.[2] ?? [null, null];
            set({ date_from: r[0], date_to: r[1], year: null, month: null });
          }}
        >
          {periods.map(([k, name]) => <option key={k} value={k}>{name}</option>)}
          <option value="custom">기간 직접 지정…</option>
        </select>
        <input className="search" placeholder="🔍 이름 · 폴더 · 카메라 · 라벨 검색" value={text} onChange={(e) => setText(e.target.value)} />
        {showSort && (
          <div className="sort">
            <select value={sort} onChange={(e) => set({ sort: e.target.value as SortKey, desc: e.target.value === "name" ? false : true })} title="정렬 기준">
              {SORTS.map((s) => <option key={s.key} value={s.key}>정렬: {s.name}</option>)}
            </select>
            <button className="btn small" onClick={() => set({ desc: !desc })} title="정렬 방향 바꾸기">
              {desc ? "↓" : "↑"} {desc ? sortDef.desc : sortDef.asc}
            </button>
          </div>
        )}
        <button className={`btn ${open ? "primary" : ""}`} onClick={() => setOpen(!open)}>
          상세 필터{advancedCount > 0 ? ` (${advancedCount})` : ""} {open ? "▴" : "▾"}
        </button>
      </div>

      {open && (
        <div className="filter-panel">
          <div className="fp-row">
            <label>기간</label>
            <div className="inline-form">
              <input type="date" value={tsToDateInput(filter.date_from)} onChange={(e) => { setCustomPeriod(true); set({ date_from: dateInputToTs(e.target.value) }); }} />
              <span className="muted">~</span>
              <input type="date" value={tsToDateInput(filter.date_to)} onChange={(e) => { setCustomPeriod(true); set({ date_to: dateInputToTs(e.target.value, true) }); }} />
            </div>
          </div>
          <div className="fp-row">
            <label>파일 크기</label>
            <div>
              {presetRow(SIZE_PRESETS, [filter.size_min ?? null, filter.size_max ?? null], ([a, b]) => set({ size_min: a, size_max: b }))}
              <div className="inline-form">
                <input type="number" min={0} placeholder="최소 MB" className="num" value={filter.size_min != null ? +(filter.size_min / MB).toFixed(2) : ""}
                  onChange={(e) => set({ size_min: e.target.value ? Math.round(Number(e.target.value) * MB) : null })} />
                <span className="muted">~</span>
                <input type="number" min={0} placeholder="최대 MB" className="num" value={filter.size_max != null ? +(filter.size_max / MB).toFixed(2) : ""}
                  onChange={(e) => set({ size_max: e.target.value ? Math.round(Number(e.target.value) * MB) : null })} />
                <span className="muted small">MB{opts ? ` · 가장 큰 파일 ${fmtBytes(opts.max_size)}` : ""}</span>
              </div>
            </div>
          </div>
          <div className="fp-row">
            <label>해상도</label>
            {presetRow(MP_PRESETS, [filter.mp_min ?? null, filter.mp_max ?? null], ([a, b]) => set({ mp_min: a, mp_max: b }))}
          </div>
          <div className="fp-row">
            <label>동영상 길이</label>
            {presetRow(DUR_PRESETS, [filter.dur_min ?? null, filter.dur_max ?? null], ([a, b]) => set({ dur_min: a, dur_max: b }))}
          </div>
          <div className="fp-grid">
            <div>
              <label>카메라 · 기기</label>
              <select value={filter.camera ?? ""} onChange={(e) => set({ camera: e.target.value || null })}>
                <option value="">전체</option>
                {opts?.cameras.map((c) => <option key={c.value} value={c.value}>{c.value} ({fmtCount(c.count)})</option>)}
              </select>
            </div>
            <div>
              <label>파일 형식</label>
              <select value={filter.ext ?? ""} onChange={(e) => set({ ext: e.target.value || null })}>
                <option value="">전체</option>
                {opts?.exts.map((x) => <option key={x.value} value={x.value}>{x.value.toUpperCase()} ({fmtCount(x.count)})</option>)}
              </select>
            </div>
            <div>
              <label>라벨</label>
              <select value={filter.label ?? ""} onChange={(e) => set({ label: e.target.value || null })}>
                <option value="">전체</option>
                {userLabels.map((l) => <option key={l.label} value={l.label}>{labelName(l.label)} ({fmtCount(l.count)})</option>)}
              </select>
            </div>
            <div>
              <label>드라이브</label>
              <select value={filter.volume_id ?? ""} onChange={(e) => set({ volume_id: e.target.value ? Number(e.target.value) : null })}>
                <option value="">전체</option>
                {volumes.map((v) => <option key={v.id} value={v.id}>{v.online ? "● " : "○ "}{v.label}</option>)}
              </select>
            </div>
            <div>
              <label>촬영 날짜 신뢰도</label>
              <select value={filter.date_confidence ?? ""} onChange={(e) => set({ date_confidence: e.target.value || null })}>
                <option value="">전체</option>
                <option value="high">확실 (사진 정보·직접 지정)</option>
                <option value="medium">파일 이름에서 추출</option>
                <option value="low">확인 필요 (추정)</option>
              </select>
            </div>
            <div>
              <label>위치정보 (GPS)</label>
              <select value={filter.has_gps == null ? "" : filter.has_gps ? "yes" : "no"} onChange={(e) => set({ has_gps: e.target.value === "" ? null : e.target.value === "yes" })}>
                <option value="">전체</option>
                <option value="yes">있음{opts ? ` (${fmtCount(opts.with_gps)})` : ""}</option>
                <option value="no">없음</option>
              </select>
            </div>
          </div>
          <label className="check-row">
            <input type="checkbox" checked={!!filter.online_only} onChange={(e) => set({ online_only: e.target.checked })} />
            지금 연결된 드라이브의 파일만
          </label>
        </div>
      )}

      {chips.length > 0 && (
        <div className="chips active-filters">
          {chips.map((c) => (
            <span key={c.text} className="chip user">
              {c.text}
              <button onClick={() => { if ("date_from" in c.clear) setCustomPeriod(false); set(c.clear); }} title="이 조건 지우기">×</button>
            </span>
          ))}
          <button className="btn small ghost" onClick={clearAll}>모두 지우기</button>
        </div>
      )}
    </div>
  );
}

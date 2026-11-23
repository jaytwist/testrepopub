import { useCallback, useEffect, useMemo, useState } from "react";
import { confirm, message, open } from "@tauri-apps/plugin-dialog";
import {
  api, fmtBytes, fmtCount, thumbUrl,
  type CopyFolder, type CopyPair, type DiskSpace, type ExecReport, type FolderDeleteReport, type FolderRule, type GridItem,
  type ImportSettings, type KeepItem, type LibraryOptions, type Merge, type PlanView, type TreeNode, type YearBucket,
} from "../api";
import Viewer from "./Viewer";
import Compare from "./Compare";
import Triage, { type ReviewResult } from "./Triage";
import { PhotoTile, PickBar, usePick } from "./PhotoPick";
import { useJob, useScope } from "../App";
import ScopePicker from "./ScopePicker";

const GB = 1024 * 1024 * 1024;
const DEFAULT_LIB: LibraryOptions = { big_video_bytes: 2 * GB, event_span_days: 62, remove_copies: true, remove_smaller_versions: true, keep_edits: "latest" };
const TAGS: Record<string, string> = {
  event: "이벤트", trip: "여행", loose: "낱장", received: "받은 사진", screenshot: "스크린샷", undated: "날짜 모름",
  raw: "RAW", live: "Live Photo", edited: "편집본",
};

/** The end of a path (last folders and the name), for long paths in narrow rows. */
function tail(p: string, n = 3): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > n ? `…/${parts.slice(-n).join("/")}` : p;
}

/** The year a library folder belongs to (same rule as the backend's `year_key`). */
function yearKey(group: string): string {
  const segs = group.split("/");
  let s = segs[0] ?? "";
  if (s === "받은 사진" || s === "큰 동영상") s = segs[1] ?? "";
  if (/^\d{4}$/.test(s)) return Number(s) < 2010 ? "before2010" : s;
  return "undated";
}

type ReviewEntry = { keep: number[]; drops: { id: number; size: number }[] };

/** A year's delete list beyond the automatic identical copies: similar photos not kept in a
 *  review, and photos put on it by hand. Kept while the app is open (until that year is pushed). */
interface DeleteList {
  reviewOf: (year: string) => Map<string, ReviewEntry>;
  saveReview: (year: string, r: ReviewResult) => void;
  unDrop: (year: string, ids: number[]) => void;
  /** copyIn: a photo only on another drive — not deleted there, just not brought in. */
  marked: Map<number, { size: number; year: string; copyIn?: boolean }>;
  mark: (items: { id: number; size: number; year: string; copyIn?: boolean }[]) => void;
  unmark: (ids: number[]) => void;
  /** id -> size of everything on a year's list (reviewed + by hand). */
  dropsFor: (year: string) => Map<number, number>;
  /** On any year's list. */
  listed: Set<number>;
}

/**
 * "라이브러리로 모으기", one year at a time: every photo and video into one library as
 * year / year-month / event (events named after the folders they came from). Each year shows
 * where things go, the copies that will be deleted (next to the original kept), and the photos
 * kept; once confirmed, that year is pushed: copies on the library drive are deleted (after a
 * byte-for-byte check) to make room, then files are moved, then files only on other drives are
 * copied in. Other drives are left as they are.
 */
export default function LibraryPlan() {
  const { running, setRunning, lastDone } = useJob();
  const [settings, setSettings] = useState<ImportSettings | null>(null);
  const [lib, setLib] = useState<LibraryOptions>(DEFAULT_LIB);
  // The work scope: only its photos are planned (into the library). A new scope needs a new plan.
  const { scope } = useScope();
  const scopeKey = scope ? `${scope.volume_id}:${scope.dir}` : "all";
  useEffect(() => { setPlan(null); }, [scopeKey]);
  const [plan, setPlan] = useState<PlanView | null>(null);
  const [tree, setTree] = useState<TreeNode[]>([]);
  const [years, setYears] = useState<YearBucket[]>([]);
  const [year, setYear] = useState<string | null>(null);
  const [byCopies, setByCopies] = useState(false);
  const [space, setSpace] = useState<DiskSpace | null>(null);
  const [rules, setRules] = useState<FolderRule[]>([]);
  const [renaming, setRenaming] = useState<{ node: TreeNode; name: string } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [result, setResult] = useState<{ label: string; rep: ExecReport } | null>(null);
  const [planning, setPlanning] = useState(false);
  const [quarantineOnly, setQuarantineOnly] = useState(false);
  const [lastRunYear, setLastRunYear] = useState<string | null>(null);
  // Folders picked in 「옮길 곳」, in the order picked (the first is where merges go).
  const [sel, setSel] = useState<string[]>([]);
  const [merges, setMerges] = useState<Merge[]>([]);
  const [excluded, setExcluded] = useState(0);
  const [previewing, setPreviewing] = useState(false);
  const [deleted, setDeleted] = useState<FolderDeleteReport | null>(null);
  // year -> review group -> decision; photos put on the delete list by hand.
  const [reviews, setReviews] = useState<Map<string, Map<string, ReviewEntry>>>(new Map());
  const [marked, setMarked] = useState<Map<number, { size: number; year: string; copyIn?: boolean }>>(new Map());
  const list: DeleteList = useMemo(() => {
    const dropsFor = (y: string) => {
      const m = new Map<number, number>();
      for (const e of reviews.get(y)?.values() ?? []) for (const d of e.drops) m.set(d.id, d.size);
      for (const [id, v] of marked) if (v.year === y) m.set(id, v.size);
      return m;
    };
    const listed = new Set<number>(marked.keys());
    for (const r of reviews.values()) for (const e of r.values()) for (const d of e.drops) listed.add(d.id);
    return {
      reviewOf: (y) => reviews.get(y) ?? new Map(),
      saveReview: (y, r) =>
        setReviews((prev) => {
          const next = new Map(prev);
          const cur = new Map(next.get(y) ?? []);
          for (const d of r.decisions) cur.set(d.key, { keep: d.keep, drops: r.drops.filter((x) => x.key === d.key).map(({ id, size }) => ({ id, size })) });
          next.set(y, cur);
          return next;
        }),
      unDrop: (y, ids) =>
        setReviews((prev) => {
          const out = new Set(ids);
          const next = new Map(prev);
          const cur = new Map(next.get(y) ?? []);
          for (const [k, e] of cur) {
            const back = e.drops.filter((d) => out.has(d.id)).map((d) => d.id);
            if (back.length) cur.set(k, { keep: [...e.keep, ...back], drops: e.drops.filter((d) => !out.has(d.id)) });
          }
          next.set(y, cur);
          return next;
        }),
      marked,
      mark: (items) => setMarked((prev) => { const n = new Map(prev); for (const i of items) n.set(i.id, { size: i.size, year: i.year, copyIn: i.copyIn }); return n; }),
      unmark: (ids) => setMarked((prev) => { const n = new Map(prev); for (const id of ids) n.delete(id); return n; }),
      dropsFor,
      listed,
    };
  }, [reviews, marked]);

  useEffect(() => {
    api.getImportSettings().then(setSettings).catch((e) => setErr(String(e)));
    api.getLibraryRules().then(setRules).catch(() => {});
    api.getLibraryMerges().then(setMerges).catch(() => {});
    api.libraryExcludedCount().then(setExcluded).catch(() => {});
  }, []);

  // "E:" on Windows; elsewhere the library folder itself.
  const drive = (() => {
    const d = settings?.target_dir ?? "";
    const m = /^([A-Za-z]:)/.exec(d);
    return m ? m[1].toUpperCase() : "라이브러리 드라이브";
  })();
  const preview = async (target = settings?.target_dir, keepResult = false) => {
    if (!target) return pickTarget();
    setErr(null);
    if (!keepResult) setResult(null);
    setPlanning(true);
    try {
      await api.startPlan({
        target_dir: target, template: "", rename: "keep", mode: "move", filter: {}, uncertain_folder: null,
        remove_empty_dirs: true, layout: "library", library: lib,
      });
      setRunning("organize:plan");
    } catch (e) {
      setErr(String(e));
      setPlanning(false);
    }
  };

  useEffect(() => {
    if (!lastDone) return;
    if (lastDone.job === "organize:plan" && planning) {
      setPlanning(false);
      if (lastDone.ok) {
        const p = lastDone.result as PlanView;
        setPlan(p);
        api.planTree(p.id).then(setTree).catch((e) => setErr(String(e)));
        api.libraryYears(p.id).then((ys) => {
          setYears(ys);
          // Stay on the chosen year unless it is finished; then the next one with work.
          setYear((cur) => {
            const still = ys.find((y) => y.key === cur);
            if (still && !still.done) return cur;
            return (ys.find((y) => !y.done) ?? ys[0])?.key ?? null;
          });
        }).catch((e) => setErr(String(e)));
        if (p.options.target_dir) api.diskSpace(p.options.target_dir).then(setSpace).catch(() => setSpace(null));
      } else setErr(lastDone.error ?? "미리보기를 만들지 못했습니다");
    }
    if (lastDone.job === "library:delete") {
      setPlan(null);
      setTree([]);
      if (lastDone.ok) {
        setDeleted(lastDone.result as FolderDeleteReport);
        setSel([]);
        api.libraryExcludedCount().then(setExcluded).catch(() => {});
        preview(undefined, true);
      } else setErr(lastDone.error ?? "삭제하지 못했습니다");
    }
    if (lastDone.job === "organize:run") {
      setPlan(null);
      setTree([]);
      if (lastDone.ok) {
        setResult({ label: lastRunYear ? (years.find((y) => y.key === lastRunYear)?.label ?? "") : "", rep: lastDone.result as ExecReport });
        if (lastRunYear) {
          // That year's list is done with.
          const y = lastRunYear;
          setReviews((prev) => { const n = new Map(prev); n.delete(y); return n; });
          setMarked((prev) => new Map([...prev].filter(([, v]) => v.year !== y)));
        }
        preview(undefined, true); // what is left
      } else setErr(lastDone.error ?? "실행하지 못했습니다");
    }
  }, [lastDone]); // eslint-disable-line react-hooks/exhaustive-deps

  const pickTarget = async () => {
    const d = await open({ directory: true, defaultPath: settings?.target_dir || undefined, title: "라이브러리 위치 (예: E:\\사진)" });
    if (typeof d !== "string" || !settings) return;
    const s = { ...settings, target_dir: d };
    setSettings(s);
    await api.setImportSettings(s).catch(() => {});
    setPlan(null);
  };

  const saveRule = async (node: TreeNode, kind: FolderRule["kind"], name: string | null = null) => {
    const next = rules.filter((r) => !(r.volume_id === node.vol && r.rel_dir === node.from_dir));
    next.push({ volume_id: node.vol, rel_dir: node.from_dir, kind, name });
    setRules(next);
    await api.setLibraryRules(next);
    setRenaming(null);
    preview();
  };

  /** Try one folder first. */
  const runFolder = async (group: string) => {
    if (!plan) return;
    const ok = await confirm(`「${group}」 폴더만 먼저 정리합니다. 이 폴더 원본의 ${drive} 사본도 함께 정리됩니다(격리, 되돌리기 가능).`,
      { title: "이 폴더만 먼저", kind: "warning", okLabel: "실행", cancelLabel: "취소" });
    if (!ok) return;
    try {
      setLastRunYear(null);
      await api.startExecute(plan.id, [group]);
      setRunning("organize:run");
    } catch (e) {
      setErr(String(e));
    }
  };

  /** Push one year: delete its copies (checked byte by byte), move, copy in. */
  const pushYear = async (y: YearBucket) => {
    if (!plan) return;
    const purge = !quarantineOnly;
    const drops = list.dropsFor(y.key);
    const keptOut = [...list.marked].filter(([, v]) => v.year === y.key && v.copyIn).length;
    const text = [
      `${y.label} 정리를 시작합니다.`,
      y.remove
        ? purge
          ? `· ${drive}의 복사본·사본 ${fmtCount(y.remove)}개 (${fmtBytes(y.remove_bytes)}) 삭제 — 원본과 내용을 끝까지 비교해 같은 것만`
          : `· ${drive}의 복사본·사본 ${fmtCount(y.remove)}개 격리함으로 (공간은 「격리함 비우기」 후에 생김)`
        : "",
      drops.size - keptOut ? `· 지울 목록의 사진 ${fmtCount(drops.size - keptOut)}장 영구 삭제 — 비슷한 사진·직접 고른 것 (♥·원본 지정은 남김)` : "",
      keptOut ? `· 다른 드라이브에만 있는 ${fmtCount(keptOut)}장은 지우지 않고 가져오지만 않음 (다음에도)` : "",
      y.moves ? `· ${drive} 안에서 옮김 ${fmtCount(y.moves)}개` : "",
      y.copies ? `· 다른 드라이브에서 가져오기 ${fmtCount(y.copies)}개 (${fmtBytes(y.copy_bytes)}) — 원래 자리는 그대로` : "",
      "",
      (purge && y.remove) || drops.size ? "삭제한 것은 되돌릴 수 없습니다. 옮긴 것은 「작업 기록」에서 되돌릴 수 있습니다." : "모든 작업은 「작업 기록」에서 되돌릴 수 있습니다.",
    ].filter((l, i, a) => l || (i > 0 && a[i - 1])).join("\n");
    if (!(await confirm(text, { title: `${y.label} Push`, kind: "warning", okLabel: "실행", cancelLabel: "취소" }))) return;
    try {
      setLastRunYear(y.key);
      const decisions = [...list.reviewOf(y.key)].map(([key, e]) => ({ key, keep: e.keep, drop: e.drops.map((d) => d.id) }));
      await api.startExecute(plan.id, undefined, y.key, purge, [...drops.keys()], decisions);
      setRunning("organize:run");
    } catch (e) {
      setErr(String(e));
    }
  };

  const toggleSel = (group: string) => setSel((s) => (s.includes(group) ? s.filter((g) => g !== group) : [...s, group]));
  const selNodes = sel.map((g) => tree.find((n) => n.group === g)).filter((n): n is TreeNode => !!n);
  const selCount = selNodes.reduce((a, n) => a + n.count, 0);
  const selBytes = selNodes.reduce((a, n) => a + n.bytes, 0);

  /** Later picks go into the first one. Remembered: planning again keeps the merge. */
  const mergeSel = async () => {
    if (sel.length < 2) return;
    const [into, ...from] = sel;
    const ok = await confirm(
      `${from.map((g) => `「${g}」`).join(", ")}을(를)\n「${into}」 안으로 합칩니다.\n\n` +
        "지금은 계획만 바뀝니다 — 실제로 옮기는 것은 그 해를 Push할 때입니다. 옮긴 뒤에는 「작업 기록」에서 되돌릴 수 있습니다.",
      { title: "폴더 합치기", kind: "info", okLabel: "합치기", cancelLabel: "취소" },
    );
    if (!ok) return;
    const next = [...merges.filter((m) => !from.includes(m.from)), ...from.map((f) => ({ from: f, into }))];
    setMerges(next);
    await api.setLibraryMerges(next);
    setSel([]);
    preview();
  };

  /** Delete the picked folders' photos on the library drive for good. */
  const deleteSel = async () => {
    if (!plan || sel.length === 0) return;
    const pv = await api.libraryDeletePreview(plan.id, sel).catch((e) => { setErr(String(e)); return null; });
    if (!pv) return;
    if (pv.count === 0 && pv.not_copied === 0) return;
    const lines = [
      pv.count > 0
        ? `폴더 ${sel.length}개의 사진·영상 ${fmtCount(pv.count)}개 (${fmtBytes(pv.bytes)})를 ${drive}에서 영구 삭제합니다. 사본도 함께 지웁니다.`
        : `폴더 ${sel.length}개: ${drive}에는 지울 사진이 없습니다.`,
      pv.not_copied ? `다른 드라이브에서 가져오려던 ${fmtCount(pv.not_copied)}개는 가져오지 않습니다(그 드라이브에는 그대로).` : "",
      "",
      "되돌릴 수 없습니다. 다른 드라이브(C: 등)에 있는 같은 사진은 그대로 두고, 다시 가져오지 않습니다.",
    ].filter((l, i, a) => l || (i > 0 && a[i - 1])).join("\n");
    let keepProtected = true;
    if (pv.protected.length > 0) {
      const leave = `보호된 ${fmtCount(pv.protected.length)}개는 남기고 삭제`;
      const answer = await message(`${lines}\n\n그중 ${fmtCount(pv.protected.length)}개는 ♥·원본 지정·항상 남길 폴더로 보호되어 있습니다.`, {
        title: "폴더 영구 삭제", kind: "warning", buttons: { yes: leave, no: "보호된 것까지 모두 삭제", cancel: "취소" },
      });
      if (answer === leave || answer === "Yes") keepProtected = true;
      else if (answer === "보호된 것까지 모두 삭제" || answer === "No") keepProtected = false;
      else return;
    } else if (!(await confirm(lines, { title: "폴더 영구 삭제", kind: "warning", okLabel: "영구 삭제", cancelLabel: "취소" }))) {
      return;
    }
    try {
      setDeleted(null);
      await api.startLibraryDelete(plan.id, sel, keepProtected);
      setRunning("library:delete");
    } catch (e) {
      setErr(String(e));
    }
  };

  const shownYears = useMemo(
    () => (byCopies ? [...years].sort((a, b) => Number(a.done) - Number(b.done) || b.remove_bytes - a.remove_bytes) : years),
    [years, byCopies],
  );
  const cur = years.find((y) => y.key === year) ?? null;
  const yearNodes = useMemo(() => (year ? tree.filter((n) => yearKey(n.group) === year) : []), [tree, year]);
  const s = plan?.summary;
  const todoYears = years.filter((y) => !y.done).length;

  return (
    <div className="library-plan">
      <ScopePicker note="이 안의 사진만 라이브러리로 옮깁니다. 라이브러리에 이미 있는 같은 사진은 그대로 두고, 범위 안의 사본만 정리합니다" />
      <div className="form-grid">
        <label>라이브러리 위치</label>
        <div className="inline-form">
          <span className="mono grow">{settings?.target_dir || "아직 고르지 않음"}</span>
          <button className="btn" onClick={pickTarget}>바꾸기</button>
        </div>
        <label>구조</label>
        <div className="muted small">
          <span className="mono">연도/연-월/이벤트</span> — 이벤트 이름은 원래 폴더 이름을 따릅니다 (앞의 날짜는 뗌).
          여러 해에 걸친 모음 폴더는 날짜별로 나눕니다. 낱장은 여행이면 여행 폴더, 아니면 연/월 바로 아래.
          RAW는 JPG 아래 <span className="mono">RAW</span> 폴더로, 받은 사진은 <span className="mono">받은 사진/</span>, 큰 동영상은 <span className="mono">큰 동영상/</span>으로.
        </div>
        <label>복사본·사본</label>
        <div className="col">
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_copies} onChange={(e) => { setLib({ ...lib, remove_copies: e.target.checked }); setPlan(null); }} />
            라이브러리 드라이브 안의 똑같은 파일(이름이 같으면 복사본, 다르면 사본)은 하나만 남기고 정리 (다른 드라이브는 그대로)
          </label>
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_smaller_versions} onChange={(e) => { setLib({ ...lib, remove_smaller_versions: e.target.checked }); setPlan(null); }} />
            다른 드라이브의 축소본·재압축본은 가져오지 않음 ({drive}의 것은 원본 옆에 두고 「비슷한 사진 검토」로)
          </label>
        </div>
        <label>편집본</label>
        <div className="col">
          <select value={lib.keep_edits} onChange={(e) => { setLib({ ...lib, keep_edits: e.target.value as LibraryOptions["keep_edits"] }); setPlan(null); }}>
            <option value="latest">원본 + 가장 최근 편집본만 남김 (권장)</option>
            <option value="all">원본 + 편집본 모두 남김</option>
            <option value="none">원본만 남김</option>
          </select>
          <span className="muted small">남기지 않을 편집본은 그 해의 「비슷한 사진 검토」에 '지울 것'으로 미리 골라 둡니다. 검토에서 바꿀 수 있고, 확인 전에는 지우지 않습니다. (가장 최근 = 파일 수정 시각 기준)</span>
        </div>
        <label>큰 동영상</label>
        <div className="inline-form">
          <input type="number" min={1} step={0.5} style={{ width: 80 }} value={lib.big_video_bytes / GB}
            onChange={(e) => { setLib({ ...lib, big_video_bytes: Math.max(0.1, Number(e.target.value)) * GB }); setPlan(null); }} />
          <span className="muted small">GB 이상은 「큰 동영상」 폴더에 따로</span>
        </div>
      </div>
      {err && <div className="alert bad">{err}</div>}
      {result && <RunResult label={result.label} rep={result.rep} drive={drive} />}
      {deleted && (
        <div className={`alert ${deleted.failed ? "bad" : "ok"}`}>
          폴더 영구 삭제:{" "}
          {[
            deleted.deleted ? `${fmtCount(deleted.deleted)}개 (${fmtBytes(deleted.bytes)}) 삭제` : "",
            deleted.kept_protected ? `보호된 ${fmtCount(deleted.kept_protected)}개 남김` : "",
            deleted.not_copied ? `다른 드라이브의 ${fmtCount(deleted.not_copied)}개는 가져오지 않음` : "",
          ].filter(Boolean).join(" · ") || "바뀐 것 없음"}
          {deleted.failed ? ` · 실패 ${fmtCount(deleted.failed)}개` : ""}
          {deleted.errors?.length > 0 && <details><summary>자세히</summary><pre>{deleted.errors.join("\n")}</pre></details>}
        </div>
      )}

      <div className="btn-row">
        <button className="btn primary" disabled={!!running} onClick={() => preview()}>
          {plan ? "다시 미리보기" : "미리보기 (아무것도 바뀌지 않음)"}
        </button>
        {planning && <span className="muted small">계획을 세우는 중…</span>}
      </div>

      {s && plan && (
        <div className="plan">
          <SpaceBar space={space} drive={drive} year={cur} purge={!quarantineOnly} />
          {s.unverified > 0 && (
            <div className="alert">
              사본인지 아직 확인하지 못한 파일 {fmtCount(s.unverified)}개는 이번에는 그대로 둡니다(지우지도 옮기지도 않음).
              예전 버전으로 색인된 파일이라 뒤에서 지문(파일 앞부분)을 읽는 중입니다 — 아래 분석 막대가 끝나면 다시 미리보기하세요.
            </div>
          )}
          <PlanTiming timing={s.timing} />
          {s.skip_offline > 0 && <div className="alert">연결되지 않은 드라이브의 파일 {fmtCount(s.skip_offline)}개는 이번 계획에서 빠졌습니다.</div>}

          {sel.length > 0 && (
            <div className="sel-bar">
              <span>
                <b>폴더 {sel.length}개</b> 선택 · {fmtCount(selCount)}개 · {fmtBytes(selBytes)}
                <span className="muted small"> · 기준: <span className="mono">{sel[0]}</span></span>
              </span>
              <span className="spacer" />
              <button className="btn small" onClick={() => setPreviewing(true)}>미리보기</button>
              <button className="btn small" disabled={sel.length < 2 || !!running} onClick={mergeSel}
                title="나중에 고른 폴더들을 처음 고른 폴더(기준) 안으로 합칩니다">
                합치기{sel.length >= 2 ? ` (${sel.length - 1}개 → 기준)` : ""}
              </button>
              <button className="btn small danger" disabled={!!running} onClick={deleteSel}>영구 삭제</button>
              <button className="btn small ghost" onClick={() => setSel([])}>선택 해제</button>
            </div>
          )}
          <div className="year-layout">
            <div className="year-list">
              <div className="year-list-head">
                <b>한 해씩 정리</b>
                <span className="muted small">{todoYears ? `남은 해 ${todoYears}` : "모두 끝남"}</span>
                <span className="spacer" />
                <div className="seg">
                  <button className={!byCopies ? "on" : ""} onClick={() => setByCopies(false)}>연도순</button>
                  <button className={byCopies ? "on" : ""} onClick={() => setByCopies(true)} title="지울 사본이 많은 해부터 — 공간을 빨리 확보">사본 많은 해부터</button>
                </div>
              </div>
              {shownYears.map((y) => (
                <button key={y.key} className={`year-row ${y.key === year ? "on" : ""} ${y.done && list.dropsFor(y.key).size === 0 ? "done" : ""}`} onClick={() => setYear(y.key)}>
                  <span className="year-name">{y.done && list.dropsFor(y.key).size === 0 ? "✓ " : ""}{y.label}</span>
                  <span className="muted small">
                    원본 {fmtCount(y.keep)} ({fmtBytes(y.keep_bytes)})
                    {y.remove + list.dropsFor(y.key).size ? (
                      <> · <span className="bad-text">지울 것 {fmtCount(y.remove + list.dropsFor(y.key).size)}</span></>
                    ) : null}
                    {y.moves ? ` · 옮김 ${fmtCount(y.moves)}` : ""}
                    {y.copies ? ` · 가져오기 ${fmtCount(y.copies)}` : ""}
                    {y.done && list.dropsFor(y.key).size === 0 ? " · 정리됨" : ""}
                  </span>
                </button>
              ))}
            </div>

            {cur && (
              <YearDetail
                key={`${plan.id}:${cur.key}`}
                planId={plan.id}
                y={cur}
                nodes={yearNodes}
                drive={drive}
                running={!!running}
                renaming={renaming}
                setRenaming={setRenaming}
                saveRule={saveRule}
                runFolder={runFolder}
                sel={sel}
                toggleSel={toggleSel}
                list={list}
                quarantineOnly={quarantineOnly}
                setQuarantineOnly={setQuarantineOnly}
                push={() => pushYear(cur)}
              />
            )}
          </div>

          {previewing && plan && <SelectionPreview planId={plan.id} groups={sel} list={list} onClose={() => setPreviewing(false)} />}

          {(rules.length > 0 || merges.length > 0 || excluded > 0) && (
            <div className="muted small">
              {rules.length > 0 && (
                <>
                  폴더 고침 {rules.length}개 기억 중 ·{" "}
                  <button className="link" onClick={async () => { setRules([]); await api.setLibraryRules([]); preview(); }}>지우기</button>
                </>
              )}
              {merges.length > 0 && (
                <>
                  {rules.length > 0 && " · "}합침 {merges.length}개 기억 중 ·{" "}
                  <button className="link" onClick={async () => { setMerges([]); await api.setLibraryMerges([]); preview(); }}>지우기</button>
                </>
              )}
              {excluded > 0 && (
                <>
                  {(rules.length > 0 || merges.length > 0) && " · "}영구 삭제한 사진 {fmtCount(excluded)}개는 다른 드라이브에서 다시 가져오지 않음 ·{" "}
                  <button className="link" title="지우면 다음 미리보기에서 다른 드라이브의 같은 사진을 다시 가져옵니다"
                    onClick={async () => { await api.clearLibraryExcluded(); setExcluded(0); preview(); }}>기억 지우기</button>
                </>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function RunResult({ label, rep, drive }: { label: string; rep: ExecReport; drive: string }) {
  const bad = rep.failed > 0 || rep.purge_kept > 0;
  return (
    <div className={`alert ${bad ? "bad" : "ok"}`}>
      <b>{label ? `${label} 끝남` : "끝남"}</b> — 옮김·가져오기 {fmtCount(rep.done)}개
      {rep.purged ? ` · ${drive} 복사본·사본 ${fmtCount(rep.purged)}개 삭제 (${fmtBytes(rep.purged_bytes)} 확보)` : ""}
      {rep.dropped ? ` · 지울 목록의 사진 ${fmtCount(rep.dropped)}장 삭제 (${fmtBytes(rep.dropped_bytes)})` : ""}
      {rep.drop_kept ? ` · 보호된 ${fmtCount(rep.drop_kept)}장은 남김(♥·원본 지정 등)` : ""}
      {rep.not_brought ? ` · 다른 드라이브의 ${fmtCount(rep.not_brought)}장은 가져오지 않음` : ""}
      {rep.removed - rep.purged - rep.purge_kept - rep.versions > 0 ? ` · 사본 ${fmtCount(rep.removed - rep.purged - rep.purge_kept - rep.versions)}개 격리함으로` : ""}
      {rep.versions ? ` · 축소본 ${fmtCount(rep.versions)}개 격리함으로 (작업 기록에서 확인 후 비우기)` : ""}
      {rep.remove_skipped ? ` · 보호된 사본 ${fmtCount(rep.remove_skipped)}개 남김(♥·원본 지정 등)` : ""}
      {rep.purge_kept ? ` · 내용이 달라 ${fmtCount(rep.purge_kept)}개는 지우지 않고 격리함에 둠` : ""}
      {rep.space_skipped ? ` · 공간이 모자라 ${fmtCount(rep.space_skipped)}개 (${fmtBytes(rep.space_skipped_bytes)})는 아직 가져오지 못함 — 다른 해를 정리한 뒤 다시 실행하세요` : ""}
      {rep.failed ? ` · 실패 ${fmtCount(rep.failed)}개` : ""}
      {rep.leftover_files ? ` · 사진·영상이 아닌 파일 ${fmtCount(rep.leftover_files)}개는 원래 폴더에 남음` : ""}
      {rep.cancelled ? " · 중간에 멈춤 (다시 실행하면 남은 것만)" : ""}
      <div className="muted small">옮긴 것은 「작업 기록」에서 되돌릴 수 있습니다.</div>
      {rep.errors?.length > 0 && <details><summary>자세히 ({rep.errors.length})</summary><pre>{rep.errors.join("\n")}</pre></details>}
    </div>
  );
}

function SpaceBar({ space, drive, year, purge }: { space: DiskSpace | null; drive: string; year: YearBucket | null; purge: boolean }) {
  if (!space || space.total <= 0) return null;
  const used = space.total - space.free;
  const gain = year && purge ? year.remove_bytes : 0;
  const cost = year ? year.copy_bytes : 0;
  const after = space.free + gain - cost;
  const short = year && cost > 0 && cost > space.free + gain - space.reserve;
  return (
    <div className="space-bar">
      <div className="space-line">
        <b>{drive} 남은 공간 {fmtBytes(space.free)}</b>
        <span className="muted small">전체 {fmtBytes(space.total)} · 항상 {fmtBytes(space.reserve)}는 비워 둠</span>
      </div>
      <div className="space-track"><div style={{ width: `${Math.min(100, (used / space.total) * 100)}%` }} /></div>
      {year && !year.done && (gain > 0 || cost > 0) && (
        <div className="small">
          {year.label}: {gain > 0 && <span className="ok-text">사본 삭제 +{fmtBytes(gain)}</span>}
          {gain > 0 && cost > 0 && " · "}
          {cost > 0 && <span>가져오기 −{fmtBytes(cost)}</span>} → 끝나면 약 <b>{fmtBytes(Math.max(0, after))}</b>
          {short && <span className="bad-text"> · 공간이 모자라 일부만 가져옵니다 (나머지는 다음에)</span>}
        </div>
      )}
    </div>
  );
}

type Tab = "where" | "list" | "similar" | "keep";

function YearDetail(props: {
  planId: string; y: YearBucket; nodes: TreeNode[]; drive: string; running: boolean;
  renaming: { node: TreeNode; name: string } | null; setRenaming: (r: { node: TreeNode; name: string } | null) => void;
  saveRule: (n: TreeNode, kind: FolderRule["kind"], name?: string | null) => void; runFolder: (group: string) => void;
  sel: string[]; toggleSel: (group: string) => void; list: DeleteList;
  quarantineOnly: boolean; setQuarantineOnly: (v: boolean) => void; push: () => void;
}) {
  const { planId, y, nodes, drive, running, renaming, setRenaming, saveRule, runFolder, sel, toggleSel, list } = props;
  const [tab, setTab] = useState<Tab>("where");
  // A year can have thousands of folders: drawn 300 at a time.
  const [shown, setShown] = useState(300);
  const drops = list.dropsFor(y.key);
  const work = y.moves + y.copies + y.remove + y.versions + drops.size;
  const done = y.done && drops.size === 0;
  const toDelete = y.remove + drops.size;
  return (
    <div className="year-detail">
      <div className="year-detail-head">
        <h3>{y.label}</h3>
        <span className="muted small">
          남는 원본 {fmtCount(y.keep)}개 ({fmtBytes(y.keep_bytes)})
          {y.in_place ? ` · 이미 제자리 ${fmtCount(y.in_place)}` : ""}
        </span>
      </div>
      <div className="seg">
        <button className={tab === "where" ? "on" : ""} onClick={() => setTab("where")}>옮길 곳 ({fmtCount(nodes.length)}개 폴더)</button>
        <button className={tab === "list" ? "on" : ""} onClick={() => setTab("list")}>지울 목록 ({fmtCount(toDelete)})</button>
        <button className={tab === "similar" ? "on" : ""} onClick={() => setTab("similar")}>비슷한 사진 검토</button>
        <button className={tab === "keep" ? "on" : ""} onClick={() => setTab("keep")}>살릴 원본 ({fmtCount(y.keep)})</button>
      </div>

      {tab === "where" && (
        <div className="lib-tree">
          {nodes.length === 0 && <div className="muted small">이 해에는 옮길 폴더가 없습니다.</div>}
          {nodes.slice(0, shown).map((n) => {
            const isEvent = n.tag === "event" && n.from_dir;
            return (
              <div key={n.group} className={`lib-node ${n.moves + n.copies === 0 ? "done" : ""} ${sel.includes(n.group) ? "picked" : ""}`}>
                <div className="lib-node-main">
                  <input type="checkbox" checked={sel.includes(n.group)} onChange={() => toggleSel(n.group)}
                    title="고르기 — 미리보기·합치기(처음 고른 폴더가 기준)·영구 삭제" />
                  {sel[0] === n.group ? <span className="pill ok">기준</span> : sel.includes(n.group) ? <span className="pill">{sel.indexOf(n.group) + 1}</span> : null}
                  {renaming?.node.group === n.group ? (
                    <span className="inline-form">
                      <input autoFocus value={renaming.name} onChange={(e) => setRenaming({ node: n, name: e.target.value })}
                        onKeyDown={(e) => { if (e.key === "Enter") saveRule(n, "event", renaming.name.trim() || null); if (e.key === "Escape") setRenaming(null); }} />
                      <button className="btn small primary" onClick={() => saveRule(n, "event", renaming.name.trim() || null)}>저장</button>
                    </span>
                  ) : (
                    <span className="mono">{n.group}</span>
                  )}
                  {n.tag && <span className="pill">{TAGS[n.tag] ?? n.tag}</span>}
                  <span className="muted small">
                    {fmtCount(n.count)}개
                    {n.moves ? ` · 옮김 ${fmtCount(n.moves)}` : ""}{n.copies ? ` · 가져오기 ${fmtCount(n.copies)}` : ""}{n.in_place ? ` · 제자리 ${fmtCount(n.in_place)}` : ""}
                  </span>
                  <span className="spacer" />
                  {isEvent && (
                    <>
                      <button className="btn small ghost" onClick={() => setRenaming({ node: n, name: n.event ?? "" })}>이름 바꾸기</button>
                      <button className="btn small ghost" onClick={() => saveRule(n, "loose")} title="이벤트로 묶지 않고 날짜별 낱장으로">낱장으로</button>
                      <button className="btn small ghost" onClick={() => saveRule(n, "skip")} title="이 폴더는 옮기지 않음">빼기</button>
                    </>
                  )}
                  {n.moves + n.copies > 0 && (
                    <button className="btn small" disabled={running} onClick={() => runFolder(n.group)}>이 폴더만 먼저</button>
                  )}
                </div>
                {n.sources.length > 0 && (
                  <div className="muted small mono lib-src" title={n.sources.map(([p, c]) => `${p} (${c})`).join("\n")}>
                    ← {n.sources[0][0]}{n.sources.length > 1 ? ` 외 ${n.sources.length - 1}곳` : ""}
                  </div>
                )}
              </div>
            );
          })}
          {nodes.length > shown && (
            <button className="btn small" onClick={() => setShown(shown + 300)}>폴더 더 보기 ({fmtCount(nodes.length - shown)}개 남음)</button>
          )}
        </div>
      )}
      {tab === "list" && <DeleteListTab planId={planId} year={y.key} drive={drive} copies={y.remove} list={list} />}
      {tab === "similar" && <SimilarTab planId={planId} year={y.key} label={y.label} list={list} />}
      {tab === "keep" && <KeepTab planId={planId} year={y.key} total={y.keep} list={list} />}

      <div className="year-push">
        {done ? (
          <span className="ok-text">✓ {y.label}은(는) 정리가 끝났습니다.</span>
        ) : (
          <>
            <button className="btn primary" disabled={running || work === 0} onClick={props.push}>
              {y.label} Push — 정리{toDelete ? `하고 지울 목록 ${fmtCount(toDelete)}개 지우기` : "하기"}
            </button>
            {y.remove > 0 && (
              <label className="check-row small">
                <input type="checkbox" checked={props.quarantineOnly} onChange={(e) => props.setQuarantineOnly(e.target.checked)} />
                복사본·사본은 지우지 말고 격리함에만 두기 (공간이 생기지 않음)
              </label>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function CopiesTab({ planId, year, drive }: { planId: string; year: string; drive: string }) {
  const [folders, setFolders] = useState<CopyFolder[] | null>(null);
  const [openDir, setOpenDir] = useState<string | null>(null);
  const [pairs, setPairs] = useState<CopyPair[]>([]);
  const [kept, setKept] = useState<Set<number>>(new Set());
  const [shown, setShown] = useState(200);
  const [cmp, setCmp] = useState<number[] | null>(null);
  useEffect(() => {
    api.yearCopies(planId, year).then(setFolders).catch(() => setFolders([]));
  }, [planId, year]);
  const openFolder = async (dir: string) => {
    if (openDir === dir) return setOpenDir(null);
    setOpenDir(dir);
    setPairs(await api.yearCopyPairs(planId, year, dir, 0, 60).catch(() => []));
  };
  const more = async (dir: string) => setPairs([...pairs, ...(await api.yearCopyPairs(planId, year, dir, pairs.length, 60).catch(() => []))]);
  const keep = async (id: number) => {
    await api.setOriginal(id).catch(() => {});
    setKept(new Set(kept).add(id));
  };
  if (!folders) return <div className="muted small">불러오는 중…</div>;
  if (folders.length === 0) return <div className="muted small">이 해에는 지울 사본이 없습니다.</div>;
  return (
    <div className="copy-folders">
      <div className="muted small">
        {drive} 안에서 원본과 내용이 똑같은 파일입니다 — 이름도 같으면 「복사본」, 이름이 다르면 「사본」. 원래 있던 폴더별로 묶었습니다.
        지우기 직전에 원본과 내용을 끝까지 비교하고, 하나라도 다르면 지우지 않습니다.
      </div>
      {folders.slice(0, shown).map((f) => (
        <div key={f.dir} className="lib-sec">
          <button className="lib-sec-head" onClick={() => openFolder(f.dir)}>
            <span>{openDir === f.dir ? "▾" : "▸"}</span>
            <span className="mono grow ellipsis" title={f.dir}><bdi>{f.dir}</bdi></span>
            <span className="muted small nowrap">
              {fmtCount(f.count)}개 · {fmtBytes(f.bytes)} (복사본 {fmtCount(f.same_name)} · 사본 {fmtCount(f.count - f.same_name)})
            </span>
          </button>
          <div className="small copy-kept-in">
            {f.dir_total > 0 && f.count >= f.dir_total
              ? <span className="bad-text">이 폴더의 사진은 전부 복사본·사본입니다 (정리 후 사진이 남지 않음)</span>
              : <span className="muted">이 폴더 사진 {fmtCount(f.dir_total)}개 중 {fmtCount(f.count)}개가 복사본·사본</span>}
            <span className="muted"> — 원본은 라이브러리의 </span>
            {f.kept_in.map(([g, c], i) => <span key={g}>{i > 0 && ", "}<span className="mono">{g}</span> <span className="muted">({fmtCount(c)})</span></span>)}
          </div>
          {openDir === f.dir && (
            <div className="copy-pairs">
              {pairs.map((p) => (
                <div key={p.file_id} className={`copy-pair ${kept.has(p.file_id) ? "kept" : ""}`}>
                  <figure className="clickable" title="원본과 나란히 크게 보기" onClick={() => setCmp([p.kept_id, p.file_id])}>
                    <img src={thumbUrl(p.kept_id)} loading="lazy" alt="" />
                    <figcaption className="ok-text">남길 원본</figcaption>
                  </figure>
                  <figure className="clickable" title="원본과 나란히 크게 보기" onClick={() => setCmp([p.kept_id, p.file_id])}>
                    <img src={thumbUrl(p.file_id)} loading="lazy" alt="" />
                    <figcaption className="bad-text">{kept.has(p.file_id) ? "남김" : sameName(p.src, p.kept_src) ? "복사본" : "사본"}</figcaption>
                  </figure>
                  <div className="copy-paths small">
                    <div className="mono" title={p.src}><span className="muted">사본</span> {tail(p.src)} <span className="muted">({fmtBytes(p.size)})</span></div>
                    <div className="mono" title={p.kept_src}><span className="muted">원본 (지금)</span> {tail(p.kept_src)}</div>
                    {p.kept_dst !== p.kept_src && <div className="mono" title={p.kept_dst}><span className="muted">원본 (정리 후)</span> {tail(p.kept_dst, 4)}</div>}
                  </div>
                  {!kept.has(p.file_id) && (
                    <button className="btn small ghost" onClick={() => keep(p.file_id)} title="원본으로 지정해 지우지 않음(다음에도)">목록에서 빼기</button>
                  )}
                </div>
              ))}
              {pairs.length < f.count && <button className="btn small" onClick={() => more(f.dir)}>더 보기 ({fmtCount(f.count - pairs.length)}개 남음)</button>}
            </div>
          )}
        </div>
      ))}
      {folders.length > shown && (
        <button className="btn small" onClick={() => setShown(shown + 200)}>폴더 더 보기 ({fmtCount(folders.length - shown)}개 남음)</button>
      )}
      {cmp && <Compare ids={cmp} onClose={() => setCmp(null)} />}
    </div>
  );
}

function KeepTab({ planId, year, total, list }: { planId: string; year: string; total: number; list: DeleteList }) {
  const [items, setItems] = useState<KeepItem[] | null>(null);
  useEffect(() => {
    api.yearKeep(planId, year, 0, 600).then(setItems).catch(() => setItems([]));
  }, [planId, year]);
  const more = async () => setItems([...(items ?? []), ...(await api.yearKeep(planId, year, items?.length ?? 0, 600).catch(() => []))]);
  if (!items) return <div className="muted small">불러오는 중…</div>;
  return (
    <div className="keep-tab">
      <PhotoGrid
        items={items.map((i) => ({ id: i.file_id, size: i.size, group: i.group, copies: i.copies, copyIn: i.action === "copy", info: i }))}
        list={list}
        prefer="add"
        onAdd={(ids) => list.mark(ids.map((id) => { const it = items.find((i) => i.file_id === id); return { id, size: it?.size ?? 0, year, copyIn: it?.action === "copy" }; }))}
        onRemove={(ids) => { list.unmark(ids); list.unDrop(year, ids); }}
      />
      {items.length < total && <button className="btn small" onClick={more}>더 보기 ({fmtCount(total - items.length)}개 남음)</button>}
    </div>
  );
}

/** The year's delete list in one place: identical copies (automatic), similar photos not kept in
 *  the review, and photos put on it by hand — each can be taken off again. */
function DeleteListTab({ planId, year, drive, copies, list }: { planId: string; year: string; drive: string; copies: number; list: DeleteList }) {
  const reviewed = [...list.reviewOf(year).values()].flatMap((e) => e.drops);
  const byHand = [...list.marked].filter(([, v]) => v.year === year).map(([id, v]) => ({ id, size: v.size, copyIn: v.copyIn }));
  return (
    <div className="delete-list">
      <h4>복사본·사본 — 자동 ({fmtCount(copies)})</h4>
      <CopiesTab planId={planId} year={year} drive={drive} />
      <h4>비슷한 사진에서 남기지 않은 것 ({fmtCount(reviewed.length)})</h4>
      {reviewed.length === 0 ? (
        <div className="muted small">「비슷한 사진 검토」에서 연사·축소본 묶음을 훑어보면 남기지 않은 사진이 여기에 들어옵니다.</div>
      ) : (
        <PhotoGrid items={reviewed} list={list} prefer="remove" onRemove={(ids) => list.unDrop(year, ids)} />
      )}
      <h4>직접 넣은 사진 ({fmtCount(byHand.length)})</h4>
      {byHand.length === 0 ? (
        <div className="muted small">「살릴 원본」이나 폴더 미리보기에서 사진을 골라 「지울 목록에 넣기」를 누르면 여기에 들어옵니다.</div>
      ) : (
        <PhotoGrid items={byHand} list={list} prefer="remove" onRemove={(ids) => list.unmark(ids)} />
      )}
      <div className="muted small">
        다른 드라이브(C: 등)에만 있는 사진은 지우지 않고 「안 가져옴」 — 라이브러리로 가져오지만 않습니다. 검토 결과와 직접 넣은 사진은 이 해를 Push하거나 앱을 끌 때까지 기억합니다.
      </div>
    </div>
  );
}

/** Bursts and similar photos of the year, reviewed group by group (keyboard); what is not kept
 *  goes on the year's delete list. */
function SimilarTab({ planId, year, label, list }: { planId: string; year: string; label: string; list: DeleteList }) {
  const [counts, setCounts] = useState<{ bursts: number; similar: number; unanalyzed: number } | null>(null);
  const [review, setReview] = useState<"burst" | "similar_image" | null>(null);
  const load = useCallback(() => {
    api.yearTriageCounts(planId, year).then(setCounts).catch(() => setCounts(null));
  }, [planId, year]);
  useEffect(load, [load]);
  const decided = list.reviewOf(year);
  const drops = [...decided.values()].reduce((a, e) => a + e.drops.length, 0);
  const initial = new Map([...decided].map(([k, e]) => [k, new Set(e.keep)]));
  const reviewedOf = (prefix: string) => [...decided.keys()].filter((k) => k.startsWith(prefix)).length;
  return (
    <div className="similar-tab">
      <div className="muted small">
        묶음마다 남길 한 장을 추천합니다(가장 선명하고 큰 것). 훑어본 묶음에서 남기지 않은 사진만 「지울 목록」에 들어가고, Push할 때 지워집니다.
        편집본과 다른 드라이브의 사진은 지우지 않습니다.
      </div>
      {counts == null ? (
        <div className="muted small">세는 중…</div>
      ) : (
        <div className="similar-rows">
          <div className="similar-row">
            <b>연사</b> <span className="muted">{fmtCount(counts.bursts)}묶음 · 훑어봄 {fmtCount(reviewedOf("burst:"))}</span>
            <span className="spacer" />
            <button className="btn small" disabled={counts.bursts === 0} onClick={() => setReview("burst")}>{reviewedOf("burst:") ? "이어서 검토" : "검토 시작"}</button>
          </div>
          <div className="similar-row">
            <b>비슷한 사진</b> <span className="muted">(축소본·재압축본·편집본) {fmtCount(counts.similar)}묶음 · 훑어봄 {fmtCount(reviewedOf("dup:"))}</span>
            <span className="spacer" />
            <button className="btn small" disabled={counts.similar === 0} onClick={() => setReview("similar_image")}>{reviewedOf("dup:") ? "이어서 검토" : "검토 시작"}</button>
          </div>
          {drops > 0 && <div className="small">지울 목록에 들어간 사진 <b>{fmtCount(drops)}</b>장 — 「지울 목록」 탭에서 확인·빼기</div>}
          {counts.unanalyzed > 0 && (
            <div className="muted small">아직 분석하지 않은 사진 {fmtCount(counts.unanalyzed)}장은 묶음에 없습니다(분석이 끝나면 나타남).</div>
          )}
        </div>
      )}
      {review && (
        <Triage
          kind={review}
          title={`${label} ${review === "burst" ? "연사" : "비슷한 사진"} 검토`}
          source={{ planId, year }}
          initial={initial}
          onFinish={(r) => list.saveReview(year, r)}
          onClose={() => { setReview(null); load(); }}
        />
      )}
    </div>
  );
}

/** Thumbnails with picking (Ctrl / Shift / drag), the viewer on a plain click, and compare. */
function PhotoGrid({ items, list, prefer, onAdd, onRemove }: {
  items: { id: number; size: number; group?: string; copies?: number; copyIn?: boolean; info?: KeepItem }[];
  list: DeleteList;
  prefer: "add" | "remove";
  onAdd?: (ids: number[]) => void;
  onRemove?: (ids: number[]) => void;
}) {
  const order = useMemo(() => items.map((i) => i.id), [items]);
  const sizes = useMemo(() => new Map(items.map((i) => [i.id, i.size])), [items]);
  const [view, setView] = useState<{ items: GridItem[]; index: number } | null>(null);
  const [cmp, setCmp] = useState<number[] | null>(null);
  const openAt = useCallback(async (id: number) => {
    const rows = await api.grid({ ids: order }, 0, order.length).catch(() => [] as GridItem[]);
    const byId = new Map(rows.map((r) => [r.id, r]));
    const all = order.map((i) => byId.get(i)).filter((x): x is GridItem => x != null);
    const index = all.findIndex((x) => x.id === id);
    if (index >= 0) setView({ items: all, index });
  }, [order]);
  const pick = usePick(order, openAt);
  const groups = useMemo(() => {
    const m = new Map<string, typeof items>();
    for (const it of items) (m.get(it.group ?? "") ?? m.set(it.group ?? "", []).get(it.group ?? "")!).push(it);
    // Look-alikes (same scene, other bytes) side by side, the biggest first.
    for (const [, its] of m) {
      const first = new Map<number, number>();
      its.forEach((it, i) => { const a = it.info?.alike; if (a != null && !first.has(a)) first.set(a, i); });
      its.sort((x, y) => {
        const kx = x.info?.alike != null ? first.get(x.info.alike)! : its.indexOf(x);
        const ky = y.info?.alike != null ? first.get(y.info.alike)! : its.indexOf(y);
        return kx - ky || ((y.info?.width ?? 0) * (y.info?.height ?? 0)) - ((x.info?.width ?? 0) * (x.info?.height ?? 0));
      });
    }
    return [...m.entries()];
  }, [items]);
  // How many of a look-alike group are in this preview (a group of one shows nothing).
  const alikeCount = useMemo(() => {
    const c = new Map<number, number>();
    for (const it of items) if (it.info?.alike != null) c.set(it.info.alike, (c.get(it.info.alike) ?? 0) + 1);
    return c;
  }, [items]);
  const [copiesOf, setCopiesOf] = useState<KeepItem | null>(null);
  const summary = useMemo(() => {
    const withCopies = items.filter((i) => (i.copies ?? 0) > 0);
    const gone = withCopies.reduce((a, i) => a + (i.copies ?? 0), 0);
    const alikeGroups = [...alikeCount.values()].filter((n) => n > 1).length;
    const alikeFiles = [...alikeCount.values()].filter((n) => n > 1).reduce((a, n) => a + n, 0);
    const damaged = items.filter((i) => i.info?.damaged).length;
    return { gone, withCopies: withCopies.length, alikeGroups, alikeFiles, damaged };
  }, [items, alikeCount]);
  const bytes = [...pick.picked].reduce((a, id) => a + (sizes.get(id) ?? 0), 0);
  const ids = () => [...pick.picked];
  return (
    <>
      <PickBar
        count={pick.picked.size}
        bytes={bytes}
        prefer={prefer}
        onAdd={onAdd && (() => { onAdd(ids()); pick.clear(); })}
        onRemove={onRemove && (() => { onRemove(ids()); pick.clear(); })}
        onCompare={() => setCmp(ids())}
        onClear={pick.clear}
      />
      {items.length > 0 && items[0].info && (
        <div className="small keep-summary">
          여기 보이는 <b>{fmtCount(items.length)}장은 모두 남는 사진</b>입니다.
          {summary.withCopies > 0 && <> 그중 {fmtCount(summary.withCopies)}장은 <b>원본</b>이고, 다른 폴더에 있는 똑같은 파일 {fmtCount(summary.gone)}개가 대신 지워집니다(「원본 −N」, 눌러서 위치 보기).</>}
          {summary.alikeGroups > 0 && <> <span className="warn-text">겉보기만 같은 사진 {fmtCount(summary.alikeGroups)}묶음({fmtCount(summary.alikeFiles)}장)</span>은 파일이 서로 달라 지금은 모두 남습니다 — 「비슷한 사진 검토」에서 하나만 남기세요.</>}
          {summary.damaged > 0 && <> <span className="bad-text">깨진 것 같은 파일 {fmtCount(summary.damaged)}장</span>(⚠).</>}
          {" "}사진에 마우스를 올리면 원래 위치·크기·해상도가 보입니다.
        </div>
      )}
      {groups.map(([g, its]) => (
        <div key={g} className="keep-group">
          {g && (
            <div className="small keep-group-head">
              <span className="mono">{g}</span> <span className="muted">· {fmtCount(its.length)}장</span>{" "}
              <button className="link" onClick={() => pick.pickAll(its.map((i) => i.id))}>모두 선택</button>
            </div>
          )}
          <div className="keep-grid">
            {its.map((it) => (
              <PhotoTile key={it.id} id={it.id} picked={pick.picked.has(it.id)} marked={list.listed.has(it.id)} markLabel={it.copyIn ? "안 가져옴" : "지움"}
                title={tileTitle(it)} className={it.info?.alike != null && (alikeCount.get(it.info.alike) ?? 0) > 1 ? `alike alike-${it.info.alike % 6}` : ""} {...pick.tileProps(it.id)}>
                {it.copies ? (
                  <span className="keep-badge orig" title={`이 사진이 원본으로 남고, 다른 폴더의 똑같은 파일 ${it.copies}개가 지워집니다 — 눌러서 위치 보기`}
                    onClick={(e) => { e.stopPropagation(); if (it.info) setCopiesOf(it.info); }} onMouseDown={(e) => e.stopPropagation()}>원본 −{it.copies}</span>
                ) : null}
                {it.info?.alike != null && (alikeCount.get(it.info.alike) ?? 0) > 1 && <span className="keep-badge alike-tag">비슷함{it.info.variant === "smaller" ? " · 작음" : it.info.variant === "recompressed" ? " · 재압축" : it.info.variant === "edited" ? " · 편집" : ""}</span>}
                {it.info?.damaged && <span className="keep-badge bad">⚠</span>}
                {it.copyIn ? <span className="keep-badge copyin">가져옴</span> : null}
              </PhotoTile>
            ))}
          </div>
        </div>
      ))}
      {view && <Viewer items={view.items} index={view.index} onIndex={(index) => setView((v) => (v ? { ...v, index } : v))} onClose={() => setView(null)} />}
      {cmp && <Compare ids={cmp} onClose={() => setCmp(null)} />}
      {copiesOf && (
        <div className="send-back" onClick={() => setCopiesOf(null)}>
          <div className="card copies-of" onClick={(e) => e.stopPropagation()}>
            <div className="row-between">
              <h3>원본과 지워질 똑같은 파일</h3>
              <button className="btn small ghost" onClick={() => setCopiesOf(null)}>닫기</button>
            </div>
            <div className="small"><span className="role-tag original">원본 · 남음</span> <span className="mono">{copiesOf.src}</span></div>
            <div className="small muted">→ 라이브러리 폴더 <span className="mono">{copiesOf.group}</span>로 옮겨집니다.</div>
            <div className="small" style={{ marginTop: 8 }}>바이트까지 똑같아서 지워지는 파일 {fmtCount(copiesOf.copies)}개{copiesOf.copies > (copiesOf.copies_at?.length ?? 0) ? ` (처음 ${copiesOf.copies_at?.length}개)` : ""}:</div>
            {(copiesOf.copies_at ?? []).map((p) => (
              <div key={p} className="small copies-of-row">
                <span className="role-tag copy">사본 · 지움</span> <span className="mono grow" title={p}><bdi>{p}</bdi></span>
                <button className="btn small ghost" onClick={() => api.openFolder(dirOf(p)).catch(() => {})}>폴더 열기</button>
              </div>
            ))}
            <div className="small muted" style={{ marginTop: 8 }}>지우기 전에 원본과 끝까지 비교해, 다르면 지우지 않고 남겨 둡니다.</div>
          </div>
        </div>
      )}
    </>
  );
}

const dirOf = (p: string) => p.slice(0, Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"))) || p;

/** What a preview photo is, on hover: where it is now, its size, and its copies. */
function tileTitle(it: { size: number; copies?: number; info?: KeepItem }): string | undefined {
  const k = it.info;
  if (!k) return undefined;
  const px = k.width && k.height ? ` · ${k.width}×${k.height}` : "";
  const lines = [`지금 위치: ${k.src ?? ""}`, `${fmtBytes(k.size)}${px}`];
  if (k.copies) lines.push(`원본으로 남음 — 똑같은 파일 ${k.copies}개가 지워짐:`, ...(k.copies_at ?? []).map((p) => `  ${p}`));
  if (k.alike != null) lines.push("겉보기가 같은 다른 파일이 있음(바이트는 다름) — 둘 다 남음, 「비슷한 사진 검토」에서 고르기");
  if (k.damaged) lines.push("깨진 것 같음");
  return lines.join("\n");
}

function sameName(a: string, b: string): boolean {
  const name = (p: string) => p.split(/[\\/]/).pop()?.toLowerCase() ?? "";
  return name(a) === name(b);
}

/** "계획 12초 — 사본·배치 4초 · …": where planning time goes (to tell what is slow on this PC). */
function PlanTiming({ timing }: { timing: [string, number][] }) {
  if (!timing?.length) return null;
  const sec = (ms: number) => (ms >= 10_000 ? `${Math.round(ms / 1000)}초` : `${(ms / 1000).toFixed(1)}초`);
  const total = timing.find(([n]) => n === "전체");
  const steps = timing.filter(([n]) => n !== "전체");
  if (total && total[1] < 1000) return <div className="muted small plan-timing">계획 1초 미만</div>;
  return (
    <div className="muted small plan-timing">
      계획 {total ? sec(total[1]) : ""} — {steps.map(([n, ms]) => `${n} ${sec(ms)}`).join(" · ")}
    </div>
  );
}

/** Photos of the picked folders, by folder; a click opens the photo big, Ctrl/Shift/drag picks
 *  photos to put on (or take off) their year's delete list. */
function SelectionPreview({ planId, groups, list, onClose }: { planId: string; groups: string[]; list: DeleteList; onClose: () => void }) {
  const [items, setItems] = useState<KeepItem[] | null>(null);
  useEffect(() => {
    api.groupItems(planId, groups, 0, 2000).then(setItems).catch(() => setItems([]));
  }, [planId, groups]);
  const ordered = useMemo(() => {
    const rank = new Map(groups.map((g, i) => [g, i]));
    return [...(items ?? [])].sort((a, b) => (rank.get(a.group) ?? 0) - (rank.get(b.group) ?? 0));
  }, [items, groups]);
  return (
    <div className="send-back" onClick={onClose}>
      <div className="card sel-preview" onClick={(e) => e.stopPropagation()}>
        <div className="row-between">
          <h3>선택한 폴더 미리보기 {groups.length > 1 && <span className="muted small">— 기준: <span className="mono">{groups[0]}</span></span>}</h3>
          <button className="btn small ghost" onClick={onClose}>닫기</button>
        </div>
        {!items && <div className="muted small">불러오는 중…</div>}
        {items && (
          <PhotoGrid
            items={ordered.map((i) => ({ id: i.file_id, size: i.size, group: i.group, copies: i.copies, copyIn: i.action === "copy", info: i }))}
            list={list}
            prefer="add"
            onAdd={(ids) => list.mark(ids.map((id) => { const it = ordered.find((i) => i.file_id === id); return { id, size: it?.size ?? 0, year: yearKey(it?.group ?? ""), copyIn: it?.action === "copy" }; }))}
            onRemove={(ids) => { list.unmark(ids); for (const y of new Set(ordered.filter((i) => ids.includes(i.file_id)).map((i) => yearKey(i.group)))) list.unDrop(y, ids); }}
          />
        )}
        {items && items.length >= 2000 && <div className="muted small">처음 2,000개만 보여 줍니다.</div>}
      </div>
    </div>
  );
}

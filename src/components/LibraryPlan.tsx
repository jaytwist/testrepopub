import { useEffect, useMemo, useState } from "react";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import {
  api, fmtBytes, fmtCount, thumbUrl,
  type CopyFolder, type CopyPair, type DiskSpace, type ExecReport, type FolderRule, type ImportSettings, type KeepItem,
  type LibraryOptions, type PlanView, type TreeNode, type YearBucket,
} from "../api";
import { useJob } from "../App";

const GB = 1024 * 1024 * 1024;
const DEFAULT_LIB: LibraryOptions = { big_video_bytes: 2 * GB, event_span_days: 62, remove_copies: true, remove_smaller_versions: true };
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

  useEffect(() => {
    api.getImportSettings().then(setSettings).catch((e) => setErr(String(e)));
    api.getLibraryRules().then(setRules).catch(() => {});
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
    if (lastDone.job === "organize:run") {
      setPlan(null);
      setTree([]);
      if (lastDone.ok) {
        setResult({ label: lastRunYear ? (years.find((y) => y.key === lastRunYear)?.label ?? "") : "", rep: lastDone.result as ExecReport });
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
    const text = [
      `${y.label} 정리를 시작합니다.`,
      y.moves ? `· ${drive} 안에서 옮김 ${fmtCount(y.moves)}개` : "",
      y.copies ? `· 다른 드라이브에서 복사 ${fmtCount(y.copies)}개 (${fmtBytes(y.copy_bytes)}) — 원래 자리는 그대로` : "",
      y.remove
        ? purge
          ? `· ${drive}의 똑같은 사본 ${fmtCount(y.remove)}개 (${fmtBytes(y.remove_bytes)}) 삭제 — 원본과 내용을 끝까지 비교해 같은 것만`
          : `· ${drive}의 똑같은 사본 ${fmtCount(y.remove)}개 격리함으로 (공간은 「격리함 비우기」 후에 생김)`
        : "",
      y.versions ? `· 축소본·재압축본 ${fmtCount(y.versions)}개는 격리함으로 (확인 후 비우기)` : "",
      "",
      purge && y.remove ? "삭제한 사본은 되돌릴 수 없습니다. 옮긴 것은 「작업 기록」에서 되돌릴 수 있습니다." : "모든 작업은 「작업 기록」에서 되돌릴 수 있습니다.",
    ].filter((l, i, a) => l || (i > 0 && a[i - 1])).join("\n");
    if (!(await confirm(text, { title: `${y.label} Push`, kind: "warning", okLabel: "실행", cancelLabel: "취소" }))) return;
    try {
      setLastRunYear(y.key);
      await api.startExecute(plan.id, undefined, y.key, purge);
      setRunning("organize:run");
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
        <label>사본</label>
        <div className="col">
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_copies} onChange={(e) => { setLib({ ...lib, remove_copies: e.target.checked }); setPlan(null); }} />
            라이브러리 드라이브 안의 똑같은 사본은 하나만 남기고 정리 (다른 드라이브는 그대로)
          </label>
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_smaller_versions} onChange={(e) => { setLib({ ...lib, remove_smaller_versions: e.target.checked }); setPlan(null); }} />
            원본이 있는 축소본·재압축본도 정리 (편집본은 원본 옆에 남김)
          </label>
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

      <div className="btn-row">
        <button className="btn primary" disabled={!!running} onClick={() => preview()}>
          {plan ? "다시 미리보기" : "미리보기 (아무것도 바뀌지 않음)"}
        </button>
        {planning && <span className="muted small">계획을 세우는 중…</span>}
      </div>

      {s && plan && (
        <div className="plan">
          <SpaceBar space={space} drive={drive} year={cur} purge={!quarantineOnly} />
          {s.leftover_files > 0 && (
            <div className="muted small">사진·동영상이 아닌 파일 {fmtCount(s.leftover_files)}개(문서 등)는 원래 폴더에 그대로 남습니다.</div>
          )}
          {s.skip_offline > 0 && <div className="alert">연결되지 않은 드라이브의 파일 {fmtCount(s.skip_offline)}개는 이번 계획에서 빠졌습니다.</div>}

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
                <button key={y.key} className={`year-row ${y.key === year ? "on" : ""} ${y.done ? "done" : ""}`} onClick={() => setYear(y.key)}>
                  <span className="year-name">{y.done ? "✓ " : ""}{y.label}</span>
                  <span className="muted small">
                    원본 {fmtCount(y.keep)} ({fmtBytes(y.keep_bytes)})
                    {y.remove ? <> · <span className="bad-text">사본 {fmtCount(y.remove)} ({fmtBytes(y.remove_bytes)})</span></> : null}
                    {y.moves ? ` · 옮김 ${fmtCount(y.moves)}` : ""}
                    {y.copies ? ` · 복사 ${fmtCount(y.copies)}` : ""}
                    {y.done ? " · 정리됨" : ""}
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
                quarantineOnly={quarantineOnly}
                setQuarantineOnly={setQuarantineOnly}
                push={() => pushYear(cur)}
              />
            )}
          </div>

          {rules.length > 0 && (
            <div className="muted small">
              폴더 고침 {rules.length}개 기억 중 ·{" "}
              <button className="link" onClick={async () => { setRules([]); await api.setLibraryRules([]); preview(); }}>모두 지우기</button>
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
      <b>{label ? `${label} 끝남` : "끝남"}</b> — 옮김·복사 {fmtCount(rep.done)}개
      {rep.purged ? ` · ${drive} 사본 ${fmtCount(rep.purged)}개 삭제 (${fmtBytes(rep.purged_bytes)} 확보)` : ""}
      {rep.removed - rep.purged - rep.purge_kept - rep.versions > 0 ? ` · 사본 ${fmtCount(rep.removed - rep.purged - rep.purge_kept - rep.versions)}개 격리함으로` : ""}
      {rep.versions ? ` · 축소본 ${fmtCount(rep.versions)}개 격리함으로 (작업 기록에서 확인 후 비우기)` : ""}
      {rep.remove_skipped ? ` · 보호된 사본 ${fmtCount(rep.remove_skipped)}개 남김(♥·원본 지정 등)` : ""}
      {rep.purge_kept ? ` · 내용이 달라 ${fmtCount(rep.purge_kept)}개는 지우지 않고 격리함에 둠` : ""}
      {rep.space_skipped ? ` · 공간이 모자라 ${fmtCount(rep.space_skipped)}개 (${fmtBytes(rep.space_skipped_bytes)})는 아직 복사 안 함 — 다른 해를 정리한 뒤 다시 실행하세요` : ""}
      {rep.failed ? ` · 실패 ${fmtCount(rep.failed)}개` : ""}
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
          {cost > 0 && <span>복사 −{fmtBytes(cost)}</span>} → 끝나면 약 <b>{fmtBytes(Math.max(0, after))}</b>
          {short && <span className="bad-text"> · 공간이 모자라 일부만 복사됩니다 (나머지는 다음에)</span>}
        </div>
      )}
    </div>
  );
}

type Tab = "where" | "copies" | "keep";

function YearDetail(props: {
  planId: string; y: YearBucket; nodes: TreeNode[]; drive: string; running: boolean;
  renaming: { node: TreeNode; name: string } | null; setRenaming: (r: { node: TreeNode; name: string } | null) => void;
  saveRule: (n: TreeNode, kind: FolderRule["kind"], name?: string | null) => void; runFolder: (group: string) => void;
  quarantineOnly: boolean; setQuarantineOnly: (v: boolean) => void; push: () => void;
}) {
  const { planId, y, nodes, drive, running, renaming, setRenaming, saveRule, runFolder } = props;
  const [tab, setTab] = useState<Tab>("where");
  const work = y.moves + y.copies + y.remove + y.versions;
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
        <button className={tab === "copies" ? "on" : ""} onClick={() => setTab("copies")}>지울 사본 ({fmtCount(y.remove + y.versions)})</button>
        <button className={tab === "keep" ? "on" : ""} onClick={() => setTab("keep")}>살릴 원본 ({fmtCount(y.keep)})</button>
      </div>

      {tab === "where" && (
        <div className="lib-tree">
          {nodes.length === 0 && <div className="muted small">이 해에는 옮길 폴더가 없습니다.</div>}
          {nodes.map((n) => {
            const isEvent = n.tag === "event" && n.from_dir;
            return (
              <div key={n.group} className={`lib-node ${n.moves + n.copies === 0 ? "done" : ""}`}>
                <div className="lib-node-main">
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
                    {n.moves ? ` · 옮김 ${fmtCount(n.moves)}` : ""}{n.copies ? ` · 복사 ${fmtCount(n.copies)}` : ""}{n.in_place ? ` · 제자리 ${fmtCount(n.in_place)}` : ""}
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
        </div>
      )}
      {tab === "copies" && <CopiesTab planId={planId} year={y.key} drive={drive} />}
      {tab === "keep" && <KeepTab planId={planId} year={y.key} total={y.keep} />}

      <div className="year-push">
        {y.done ? (
          <span className="ok-text">✓ {y.label}은(는) 정리가 끝났습니다.</span>
        ) : (
          <>
            <button className="btn primary" disabled={running || work === 0} onClick={props.push}>
              {y.label} Push — 정리{y.remove ? "하고 사본 지우기" : "하기"}
            </button>
            {y.remove > 0 && (
              <label className="check-row small">
                <input type="checkbox" checked={props.quarantineOnly} onChange={(e) => props.setQuarantineOnly(e.target.checked)} />
                사본은 지우지 말고 격리함에만 두기 (공간이 생기지 않음)
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
        {drive} 안에서 원본과 내용이 똑같은 사본입니다. 원래 있던 폴더별로 묶었습니다. 지우기 직전에 원본과 내용을 끝까지 비교하고, 하나라도 다르면 지우지 않습니다.
      </div>
      {folders.map((f) => (
        <div key={f.dir} className="lib-sec">
          <button className="lib-sec-head" onClick={() => openFolder(f.dir)}>
            <span>{openDir === f.dir ? "▾" : "▸"}</span>
            <span className="mono grow ellipsis" title={f.dir}><bdi>{f.dir}</bdi></span>
            <span className="muted small nowrap">
              {fmtCount(f.count)}개 · {fmtBytes(f.bytes)}{f.versions ? ` (축소본 ${fmtCount(f.versions)})` : ""}
            </span>
          </button>
          <div className="muted small copy-kept-in">
            원본은 → {f.kept_in.map(([g, c]) => `${g} (${fmtCount(c)})`).join(", ")}
          </div>
          {openDir === f.dir && (
            <div className="copy-pairs">
              {pairs.map((p) => (
                <div key={p.file_id} className={`copy-pair ${kept.has(p.file_id) ? "kept" : ""}`}>
                  <figure>
                    <img src={thumbUrl(p.kept_id)} loading="lazy" alt="" />
                    <figcaption className="ok-text">남길 원본</figcaption>
                  </figure>
                  <figure>
                    <img src={thumbUrl(p.file_id)} loading="lazy" alt="" />
                    <figcaption className="bad-text">{kept.has(p.file_id) ? "남김" : p.tag === "version" ? "축소본 (격리)" : "지울 사본"}</figcaption>
                  </figure>
                  <div className="copy-paths small">
                    <div className="mono" title={p.src}><span className="muted">사본</span> {tail(p.src)} <span className="muted">({fmtBytes(p.size)})</span></div>
                    <div className="mono" title={p.kept_src}><span className="muted">원본</span> {tail(p.kept_src)}</div>
                    {p.kept_dst !== p.kept_src && <div className="mono" title={p.kept_dst}><span className="muted">→ 갈 곳</span> {tail(p.kept_dst, 4)}</div>}
                  </div>
                  {!kept.has(p.file_id) && (
                    <button className="btn small ghost" onClick={() => keep(p.file_id)} title="이 사본도 원본으로 지정해 지우지 않음">이건 남기기</button>
                  )}
                </div>
              ))}
              {pairs.length < f.count && <button className="btn small" onClick={() => more(f.dir)}>더 보기 ({fmtCount(f.count - pairs.length)}개 남음)</button>}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

function KeepTab({ planId, year, total }: { planId: string; year: string; total: number }) {
  const [items, setItems] = useState<KeepItem[] | null>(null);
  useEffect(() => {
    api.yearKeep(planId, year, 0, 240).then(setItems).catch(() => setItems([]));
  }, [planId, year]);
  const more = async () => setItems([...(items ?? []), ...(await api.yearKeep(planId, year, items?.length ?? 0, 240).catch(() => []))]);
  const groups = useMemo(() => {
    const m = new Map<string, KeepItem[]>();
    for (const it of items ?? []) (m.get(it.group) ?? m.set(it.group, []).get(it.group)!).push(it);
    return [...m.entries()];
  }, [items]);
  if (!items) return <div className="muted small">불러오는 중…</div>;
  return (
    <div className="keep-tab">
      {groups.map(([g, its]) => (
        <div key={g} className="keep-group">
          <div className="mono small keep-group-head">{g}</div>
          <div className="keep-grid">
            {its.map((it) => (
              <div key={it.file_id} className="keep-tile" title={it.copies ? `사본 ${it.copies}개 정리` : ""}>
                <img src={thumbUrl(it.file_id)} loading="lazy" alt="" />
                {it.copies > 0 && <span className="keep-badge">사본 {it.copies}</span>}
                {it.action === "copy" && <span className="keep-badge copyin">복사</span>}
              </div>
            ))}
          </div>
        </div>
      ))}
      {items.length < total && <button className="btn small" onClick={more}>더 보기 ({fmtCount(total - items.length)}개 남음)</button>}
    </div>
  );
}

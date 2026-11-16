import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import {
  api, fmtBytes, fmtCount, fmtDate, withLast,
  type CopyFolderInfo, type CopyFolderPick, type CopyFolderReport, type CopyFolders as Found, type CopyFolderSet,
  type FolderProps, type NearFolderSet,
} from "../api";
import { useJob } from "../App";
import FolderPeek, { type Peek } from "./FolderPeek";
import FolderWalk, { type WalkSet } from "./FolderWalk";

const PAGE = 30;
const dirKey = (f: CopyFolderInfo) => `${f.volume_id}:${f.rel_dir}`;
const ref = (f: CopyFolderInfo) => ({ volume_id: f.volume_id, rel_dir: f.rel_dir });
/** A folder's own date is a real moment (not a photo's wall-clock time): shown in local time. */
const day = (ts: number | null) => (ts == null ? null : new Date(ts * 1000).toLocaleDateString("sv-SE"));

const WHY: Record<string, string> = {
  keep_rule: "「항상 남길 폴더」라서 원본으로 봄",
  filed: "라이브러리에 정리된 폴더라서 원본으로 봄",
  oldest: "가장 먼저 만든 폴더라서 원본으로 봄",
};

/** One exact set as shown: the folder that stays (the user may have switched it) and the rest. */
interface ExactRow { key: string; set: CopyFolderSet; all: CopyFolderInfo[]; keep: CopyFolderInfo; others: CopyFolderInfo[] }
/** One folder that is almost the same as its base, as shown (a pair may be turned around). */
interface NearRow {
  key: string; keep: CopyFolderInfo; copy: CopyFolderInfo; shared: number; sharedBytes: number; onlyKeep: number; onlyCopy: number; ratio: number;
  alsoListed: number; extraBytes: number; differs: number;
}
const inside = (f: CopyFolderInfo, of: CopyFolderInfo) => f.volume_id === of.volume_id && f.rel_dir.startsWith(`${of.rel_dir}/`);

/**
 * Whole-folder copies. 「100% 복사 폴더」: folders whose photos and videos are the same in number
 * and content — the original stays, the copies go. 「거의 같은 폴더」: at least 95 % the same — only
 * what both have goes from the copy. Nothing is ticked until the user ticks it; every folder
 * can be opened and compared first.
 */
export default function CopyFolders({ onTotals }: { onTotals?: (exact: number, near: number) => void }) {
  const { version, running, setRunning, lastDone } = useJob();
  const [found, setFound] = useState<Found | null>(null);
  const [err, setErr] = useState<string | null>(null);
  /** Which folder of an exact set stays, when not the suggested one (index into [keep, ...copies]). */
  const [stays, setStays] = useState<Record<string, number>>({});
  /** Near pairs turned around (the smaller folder stays). */
  const [turned, setTurned] = useState<Set<string>>(new Set());
  /** Folders ticked for removal: `x:<folder>` exact copies, `n:<base>|<folder>` almost-copies. */
  const [on, setOn] = useState<Set<string>>(new Set());
  /** Which of the two lists is open (the other one keeps its ticks). */
  const [tab, setTab] = useState<"exact" | "near">("exact");
  /** How alike "almost the same" must be. Below 95 % a folder can be ticked only after its
   *  comparison was opened: the lower the bar, the more pairs are no copies at all. */
  const [bar, setBar] = useState(0.95);
  const [seen, setSeen] = useState<Set<string>>(new Set());
  const [shown, setShown] = useState(PAGE);
  const [nearShown, setNearShown] = useState(PAGE);
  const [peek, setPeek] = useState<Peek | null>(null);
  /** The side-by-side walk-through: which list it walks and which set is open. */
  const [walk, setWalk] = useState<{ tab: "exact" | "near" | "review"; key: string } | null>(null);
  /** Sets already looked through (kept across restarts), and whether to hide them. */
  const [reviewed, setReviewed] = useState<Set<string>>(new Set());
  const [hideReviewed, setHideReviewed] = useState(false);
  useEffect(() => {
    api.foldersReviewed().then((r) => setReviewed(new Set(r))).catch(() => {});
  }, []);
  const markReviewed = useCallback((key: string) => {
    setReviewed((r) => (r.has(key) ? r : new Set(r).add(key)));
    api.setFolderReviewed(key, true).catch(() => {});
  }, []);
  const [props, setProps] = useState<Record<string, FolderProps>>({});
  const [confirming, setConfirming] = useState(false);
  const [keepInQuarantine, setKeepInQuarantine] = useState(false);
  /** Compare parts of each file with its twin instead of all of it: faster, and blind between the parts. */
  const [quick, setQuick] = useState(false);
  const [result, setResult] = useState<CopyFolderReport | null>(null);

  const load = useCallback(() => {
    // The list shown last time for this bar at once; the backend recomputes only when files changed.
    withLast(`copy-folders:${bar}`, () => api.copyFolders(bar), setFound).catch((e) => setErr(String(e)));
  }, [bar]);
  const changeBar = (b: number) => {
    setBar(b);
    setNearShown(PAGE);
    setOn((o) => new Set([...o].filter((k) => !k.startsWith("n:"))));
  };
  useEffect(load, [load, version]);
  useEffect(() => {
    if (lastDone?.job === "cleanup:swap") return load();
    if (lastDone?.job !== "cleanup:folders") return;
    setOn(new Set());
    setProps({});
    if (lastDone.ok) setResult(lastDone.result as CopyFolderReport);
    else if (lastDone.error !== "cancelled") setErr(lastDone.error ?? "복사 폴더를 정리하지 못했습니다");
  }, [lastDone]);

  const exact: ExactRow[] = useMemo(
    () =>
      (found?.sets ?? []).map((s) => {
        const all = [s.keep, ...s.copies];
        const key = dirKey(s.keep);
        const k = Math.min(stays[key] ?? 0, all.length - 1);
        return { key, set: s, all, keep: all[k], others: all.filter((_, i) => i !== k) };
      }),
    [found, stays],
  );
  const near: { key: string; set: NearFolderSet; rows: NearRow[] }[] = useMemo(
    () =>
      (found?.near ?? []).map((s) => ({
        key: dirKey(s.base),
        set: s,
        rows: s.others.map((o) => {
          const key = `${dirKey(s.base)}|${dirKey(o.info)}`;
          const flip = s.others.length === 1 && turned.has(key);
          return {
            key, ratio: o.ratio, shared: o.shared, sharedBytes: o.shared_bytes, alsoListed: o.also_listed, extraBytes: o.extra_bytes, differs: o.differs,
            keep: flip ? o.info : s.base, copy: flip ? s.base : o.info,
            onlyKeep: flip ? o.only_here : o.only_base, onlyCopy: flip ? o.only_base : o.only_here,
          };
        }),
      })),
    [found, turned],
  );

  const exactList = hideReviewed ? exact.filter((r) => !reviewed.has(r.key)) : exact;
  const nearList = hideReviewed ? near.filter((s) => !reviewed.has(s.key)) : near;
  /** The sets of the open list as the walk-through takes them. */
  const walkSets: WalkSet[] = walk?.tab === "review"
    ? (result?.to_review ?? []).map((p) => ({
        key: `${p.keep.volume_id}:${p.keep.rel_dir}`, action: "", checked: false,
        keep: { ...p.keep, abs_path: p.keep_path }, copies: [{ ...p.copy, abs_path: p.copy_path }],
      }))
    : walk?.tab === "near"
    ? nearList.map((s) => ({
        key: s.key, keep: s.rows[0].keep, copies: s.rows.map((r) => r.copy), action: "겹치는 것 지우기",
        checked: s.rows.filter((r) => !r.copy.keep_rule).length > 0 && s.rows.filter((r) => !r.copy.keep_rule).every((r) => on.has(`n:${r.key}`)),
      }))
    : exactList.map((r) => ({
        key: r.key, keep: r.keep, copies: r.others, action: "복사본 지우기",
        checked: r.others.filter((f) => !f.keep_rule).length > 0 && r.others.filter((f) => !f.keep_rule).every((f) => on.has(`x:${dirKey(f)}`)),
      }));
  const toggleSet = (key: string) => {
    if (walk?.tab === "near") {
      const rows = near.find((s) => s.key === key)?.rows.filter((r) => !r.copy.keep_rule) ?? [];
      // Looked at side by side: that is the comparison a tick below 95 % asks for.
      setSeen((x) => new Set([...x, ...rows.map((r) => r.key)]));
      toggle(rows.map((r) => `n:${r.key}`));
    } else {
      toggle(exact.find((r) => r.key === key)?.others.filter((f) => !f.keep_rule).map((f) => `x:${dirKey(f)}`) ?? []);
    }
  };
  const openWalk = (tab: "exact" | "near", key: string) => {
    setWalk({ tab, key });
  };

  const exactGoing = exact.flatMap((r) => r.others.filter((f) => !f.keep_rule && on.has(`x:${dirKey(f)}`)).map((f) => ({ r, f })));
  const nearGoing = near.flatMap((s) => s.rows.filter((r) => !r.copy.keep_rule && on.has(`n:${r.key}`)));
  const picks: CopyFolderPick[] = [
    ...exactGoing.map(({ r, f }) => ({ keep: ref(r.keep), copy: ref(f), partial: false })),
    ...nearGoing.map((r) => ({ keep: ref(r.keep), copy: ref(r.copy), partial: true })),
  ];
  const goingFiles = exactGoing.filter((x) => !nearGoing.some((r) => inside(x.f, r.copy))).reduce((a, x) => a + x.r.set.files, 0) + nearGoing.reduce((a, r) => a + r.shared, 0);
  // A 100 % copy inside an almost-copy that goes too is counted once (with the outer folder).
  const once = exactGoing.filter((x) => !nearGoing.some((r) => inside(x.f, r.copy)));
  const goingBytes = once.reduce((a, x) => a + x.r.set.bytes, 0) + nearGoing.reduce((a, r) => a + r.sharedBytes, 0);
  const exactBytes = exact.reduce((a, r) => a + r.others.filter((f) => !f.keep_rule).length * r.set.bytes, 0);
  const nearBytes = near.reduce((a, s) => a + s.rows.reduce((b, r) => b + r.sharedBytes, 0), 0);
  const nearExtra = near.reduce((a, s) => a + s.rows.reduce((b, r) => b + r.extraBytes, 0), 0);
  useEffect(() => onTotals?.(exactBytes, nearExtra), [exactBytes, nearExtra, onTotals]);

  const toggle = (keys: string[], value?: boolean) =>
    setOn((o) => {
      const n = new Set(o);
      const add = value ?? !keys.every((k) => n.has(k));
      for (const k of keys) add ? n.add(k) : n.delete(k);
      return n;
    });
  const exactKeys = exact.flatMap((r) => r.others.filter((f) => !f.keep_rule).map((f) => `x:${dirKey(f)}`));
  const allExactOn = exactKeys.length > 0 && exactKeys.every((k) => on.has(k));

  const loadProps = (folders: CopyFolderInfo[]) => {
    for (const f of folders) {
      api.folderProps(ref(f)).then((p) => setProps((m) => ({ ...m, [dirKey(f)]: p }))).catch(() => {});
    }
  };

  // Folders a recovery tool made: where files that only start the same are a real thing.
  const recovered = [...new Set([...exactGoing.flatMap((x) => [x.f.abs_path, x.r.keep.abs_path]), ...nearGoing.flatMap((r) => [r.copy.abs_path, r.keep.abs_path])].filter((p) => /recover|복구|rescue/i.test(p)))];
  const quickOn = quick && !keepInQuarantine;

  const run = async () => {
    setConfirming(false);
    setErr(null);
    setResult(null);
    try {
      await api.startRemoveCopyFolders(picks, !keepInQuarantine, quickOn);
      setRunning("cleanup:folders");
    } catch (e) {
      setErr(String(e));
    }
  };

  const open = (f: CopyFolderInfo) => api.openFolder(f.abs_path).catch((e) => setErr(String(e)));
  const folderProps = (f: CopyFolderInfo) => {
    const p = props[dirKey(f)];
    return (
      <>
        사진 {fmtCount(f.photos)} · 영상 {fmtCount(f.videos)} · <b>{fmtBytes(f.bytes)}</b>
        {day(f.created) && ` · 만든 날 ${day(f.created)}`}
        {day(f.modified) && ` · 고친 날 ${day(f.modified)}`}
        {f.taken_from != null && ` · 찍은 날 ${fmtDate(f.taken_from)}${fmtDate(f.taken_to) !== fmtDate(f.taken_from) ? ` ~ ${fmtDate(f.taken_to)}` : ""}`}
        {f.subfolders > 0 && ` · 하위 폴더 ${fmtCount(f.subfolders)}개`}
        {p && (p.other_files > 0 ? ` · 그 밖의 파일 ${fmtCount(p.other_files)}개 (${fmtBytes(p.other_bytes)}): ${p.examples.join(", ")}${p.other_files > p.examples.length ? " …" : ""}` : " · 그 밖의 파일 없음")}
      </>
    );
  };

  /** One folder of a set: its role (stays / goes), where it is, what is in it, and the ways to check. */
  const folder = (f: CopyFolderInfo, role: "keep" | "go" | "stay", tag: ReactNode, note: ReactNode, buttons: ReactNode) => (
    <div key={dirKey(f)} className={`cf-folder ${role}`}>
      <div className="cf-role">{tag}</div>
      <div className="cf-main">
        <div className="cf-path">{f.abs_path}</div>
        <div className="cf-props small">{folderProps(f)}</div>
        {note && <div className="cf-note small">{note}</div>}
      </div>
      <div className="cf-btns">{buttons}</div>
    </div>
  );

  return (
    <section className="cleanup-tier tier-first">
      <h2 className="tier-title">가장 먼저 — 100% 복사 폴더</h2>
      <p className="muted small tier-note">
        폴더째 복사해 둔 것입니다. 안의 사진·영상이 개수도 내용도 완전히 같은 폴더끼리 묶었습니다. <b>원본</b>(가장 먼저 만든 폴더)은 남기고 <b>복사본</b>을 지웁니다.
        지우기 전에 「안의 사진 보기」와 「나란히 비교」로 확인할 수 있고, 체크한 폴더만 지웁니다.{" "}
        {found?.drive ? `${found.drive} 드라이브만 봅니다. 다른 드라이브의 같은 폴더는 백업이므로 건드리지 않습니다.` : "한 드라이브 안에서만 비교하며, 다른 드라이브의 같은 폴더는 백업이므로 건드리지 않습니다."}
      </p>
      {err && <div className="alert bad">{err}</div>}
      {result && <Result r={result} onClose={() => setResult(null)} onReview={() => setWalk({ tab: "review", key: "" })} />}

      {found == null ? (
        <div className="card muted small">폴더를 비교하는 중…</div>
      ) : (
        <>
          <div className="fp-tabs cf-tabs">
            <button className={`fp-tab ${tab === "exact" ? "on" : ""}`} onClick={() => setTab("exact")}>
              100% 복사 폴더 · {fmtCount(exact.length)}묶음 · {fmtBytes(exactBytes)}
            </button>
            <button className={`fp-tab ${tab === "near" ? "on" : ""}`} onClick={() => setTab("near")}>
              거의 같은 폴더 ({Math.round(bar * 100)}% 이상) · {fmtCount(near.length)}묶음 · {fmtBytes(nearBytes)}
            </button>
          </div>
          {tab === "exact" && exact.length === 0 && (
            <div className="card muted small">
              내용이 완전히 같은 폴더는 없습니다. 「거의 같은 폴더」에서 기준을 낮춰 볼 수 있습니다.
              {found.unread > 0 && ` 아직 내용을 확인하지 못한 파일이 ${fmtCount(found.unread)}개 있어, 확인이 끝나면 더 나올 수 있습니다.`}
            </div>
          )}
          {tab === "near" && (
            <div className="cf-bar small">
              <span>얼마나 같아야 보여 줄까요?</span>
              {[0.95, 0.9, 0.8].map((b) => (
                <button key={b} className={`fp-tab ${bar === b ? "on" : ""}`} onClick={() => changeBar(b)}>{Math.round(b * 100)}% 이상</button>
              ))}
              <span className="muted">
                낮출수록 더 많이 나오지만, 복사본이 아니라 사진 몇 장을 같이 가진 다른 폴더도 섞입니다. 어느 기준에서든 지우는 것은 원본 폴더에 똑같이 있는 파일뿐입니다.
                {bar < 0.95 && " 95% 미만인 폴더는 「나란히 비교」를 연 뒤에 체크할 수 있습니다."}
              </span>
            </div>
          )}
          {tab === "near" && near.length === 0 && <div className="card muted small">{Math.round(bar * 100)}% 이상 같은 폴더는 없습니다.</div>}
          {tab === "exact" && exact.length > 0 && (
            <>
              <div className="cf-head">
                <div>
                  <b>{fmtCount(exact.length)}묶음</b> · 복사 폴더 {fmtCount(exact.reduce((a, r) => a + r.others.length, 0))}개 · 모두 지우면 <b>{fmtBytes(exactBytes)}</b> 확보
                  {found.unread > 0 && <span className="muted small"> · 아직 확인하지 못한 파일 {fmtCount(found.unread)}개 (끝나면 더 나올 수 있음)</span>}
                </div>
                <span className="cf-btns">
                  <label className="small"><input type="checkbox" checked={hideReviewed} onChange={(e) => setHideReviewed(e.target.checked)} /> 아직 안 본 묶음만</label>
                  <button className="btn small" onClick={() => toggle(exactKeys, !allExactOn)}>{allExactOn ? "선택 모두 해제" : "100% 묶음 모두 선택"}</button>
                </span>
              </div>
              <div className="cf-sets">
                {exactList.slice(0, shown).map((r, i) => {
                  const keys = r.others.filter((f) => !f.keep_rule).map((f) => `x:${dirKey(f)}`);
                  const n = keys.filter((k) => on.has(k)).length;
                  return (
                    <div key={r.key} className="card cf-set">
                      <div className="cf-set-head">
                        <span>
                          <b>묶음 {i + 1}</b> · 사진·영상 {fmtCount(r.set.files)}개 · {fmtBytes(r.set.bytes)} · 복사본 {fmtCount(r.others.length)}개
                          {n > 0 && <b className="cf-gain"> → {fmtBytes(n * r.set.bytes)} 확보</b>}
                          {r.keep.unreadable > 0 && <span className="cf-flag"> 열 수 없는 사진 {fmtCount(r.keep.unreadable)}장</span>}
                          {reviewed.has(r.key) && <span className="pill ok cf-seen">확인함</span>}
                        </span>
                        <span className="cf-btns">
                          {!props[dirKey(r.keep)] && <button className="btn small ghost" onClick={() => loadProps(r.all)}>그 밖의 파일 확인</button>}
                          {keys.length > 0 && (
                            <label className="cf-check">
                              <input type="checkbox" checked={n === keys.length} onChange={() => toggle(keys)} /> 이 묶음의 복사본 지우기
                            </label>
                          )}
                        </span>
                      </div>
                      {folder(
                        r.keep, "keep", <span className="cf-tag keep">원본 · 남김</span>,
                        r.keep === r.set.keep ? WHY[r.set.keep_why] : "직접 원본으로 고름",
                        <>
                          <button className="btn small" onClick={() => setPeek({ kind: "files", folder: r.keep, role: "원본" })}>안의 사진 보기</button>
                          <button className="btn small" onClick={() => open(r.keep)}>폴더 열기</button>
                        </>,
                      )}
                      {r.others.map((f) => {
                        const k = `x:${dirKey(f)}`;
                        const going = !f.keep_rule && on.has(k);
                        return folder(
                          f, going ? "go" : "stay",
                          <label className={`cf-tag ${going ? "go" : ""}`}>
                            <input type="checkbox" checked={going} disabled={f.keep_rule} onChange={() => toggle([k])} /> 복사본 · {going ? "지움" : "그대로 둠"}
                          </label>,
                          <>
                            {f.keep_rule ? "「항상 남길 폴더」라서 지우지 않습니다" : f.names === r.keep.names ? "원본과 파일 이름·위치까지 같음" : "원본과 내용은 같고 파일 이름·위치만 다름"}
                            {f.filed && " · 라이브러리에 정리된 폴더"}
                          </>,
                          <>
                            <button className="btn small" onClick={() => setPeek({ kind: "files", folder: f, role: "복사본" })}>안의 사진 보기</button>
                            <button className="btn small" onClick={() => openWalk("exact", r.key)}>나란히 비교</button>
                            <button className="btn small" title="이 폴더를 원본으로 남기고, 지금의 원본을 복사본으로 봅니다" onClick={() => { toggle(keys, false); setStays((m) => ({ ...m, [r.key]: r.all.indexOf(f) })); }}>
                              이쪽을 원본으로
                            </button>
                            <button className="btn small" onClick={() => open(f)}>폴더 열기</button>
                          </>,
                        );
                      })}
                    </div>
                  );
                })}
              </div>
              {exactList.length > shown && <button className="btn" onClick={() => setShown((n) => n + PAGE)}>더 보기 ({fmtCount(exactList.length - shown)}묶음 남음)</button>}
              {exactList.length === 0 && <div className="card muted small">모든 묶음을 확인했습니다. 「아직 안 본 묶음만」을 끄면 다시 보입니다.</div>}
            </>
          )}

          {tab === "near" && near.length > 0 && (
            <>
              <p className="muted small tier-note">
                두 폴더의 사진·영상이 대부분 같지만 서로 없는 것이 조금 있습니다. 복사본의 사진이 전부 원본에 들어 있는 폴더는 비율과 관계없이 여기에 나옵니다. 「나란히 비교」로 무엇이 다른지 본 뒤 체크하세요.
                체크하면 복사본에서 <b>원본에도 있는 것만</b> 지웁니다. 복사본에만 있는 사진은 그 폴더에 그대로 남고, 나중에 「한 곳으로 정리」가 가져갑니다.
              </p>
              <div className="cf-head">
                <div><b>{fmtCount(near.length)}묶음</b> · 폴더 {fmtCount(near.reduce((a, s) => a + s.rows.length, 0))}개 · 겹치는 것을 모두 지우면 <b>{fmtBytes(nearBytes)}</b> 확보
                  {nearBytes > nearExtra && <span className="muted small"> (이 중 {fmtBytes(nearBytes - nearExtra)}는 위 100% 목록에도 들어 있음)</span>}</div>
              </div>
              <div className="cf-sets">
                {nearList.slice(0, nearShown).map((s, i) => (
                  <div key={s.key} className="card cf-set">
                    <div className="cf-set-head">
                      <span>
                        <b>묶음 {i + 1}</b> · 폴더 {fmtCount(s.rows.length + 1)}개
                        {s.rows.some((r) => r.differs > 0) && <span className="cf-flag"> 이름은 같은데 내용이 다른 사진 {fmtCount(s.rows.reduce((a, r) => a + r.differs, 0))}장 — 나란히 보고 고르세요</span>}
                        {reviewed.has(s.key) && <span className="pill ok cf-seen">확인함</span>}
                      </span>
                      {!props[dirKey(s.set.base)] && <button className="btn small ghost" onClick={() => loadProps([s.set.base, ...s.set.others.map((o) => o.info)])}>그 밖의 파일 확인</button>}
                    </div>
                    {folder(
                      s.rows[0].keep, "keep", <span className="cf-tag keep">원본 · 남김</span>,
                      s.rows[0].keep === s.set.base ? "사진·영상이 더 많은(같으면 먼저 만든) 폴더라서 원본으로 봄" : "직접 원본으로 고름",
                      <>
                        <button className="btn small" onClick={() => setPeek({ kind: "files", folder: s.rows[0].keep, role: "원본" })}>안의 사진 보기</button>
                        <button className="btn small" onClick={() => open(s.rows[0].keep)}>폴더 열기</button>
                      </>,
                    )}
                    {s.rows.map((r) => {
                      const k = `n:${r.key}`;
                      const going = !r.copy.keep_rule && on.has(k);
                      // Below 95 % the comparison comes first — unless everything of the folder is in the original.
                      const locked = r.copy.keep_rule || (r.ratio < 0.95 && !seen.has(r.key) && r.onlyCopy + r.differs > 0);
                      return folder(
                        r.copy, going ? "go" : "stay",
                        <label className={`cf-tag ${going ? "go" : ""}`}>
                          <input type="checkbox" checked={going} disabled={locked} title={locked && !r.copy.keep_rule ? "먼저 「나란히 비교」로 무엇이 다른지 확인하세요" : undefined} onChange={() => toggle([k])} />{" "}
                          {going ? "겹치는 것 지움" : locked && !r.copy.keep_rule ? "비교 후 체크" : "그대로 둠"}
                        </label>,
                        <>
                          <b>{Math.floor(r.ratio * 100)}% 같음</b>
                          {r.onlyCopy === 0 && r.differs === 0 && <span className="cf-in"> 전부 원본에 있음</span>}
                          {r.differs > 0 && <b className="cf-flag"> · 이름은 같은데 내용이 다른 것 {fmtCount(r.differs)}장</b>} · 같은 것 {fmtCount(r.shared)}개 ({fmtBytes(r.sharedBytes)}) · 원본에만 {fmtCount(r.onlyKeep)}개 ·{" "}
                          <b>이 폴더에만 {fmtCount(r.onlyCopy)}개</b>
                          {r.copy.keep_rule ? " · 「항상 남길 폴더」라서 지우지 않습니다" : r.onlyCopy + r.differs > 0 ? " (지우지 않고 남김)" : " (이 폴더의 사진은 모두 원본에 있어, 지우면 폴더째 없어짐)"}
                          {r.alsoListed > 0 && <div>이 안의 하위 폴더 {fmtCount(r.alsoListed)}개는 위 100% 목록에도 있습니다. 여기서 체크하면 그 묶음들도 한 번에 정리됩니다.</div>}
                        </>,
                        <>
                          <button className="btn small" onClick={() => setPeek({ kind: "files", folder: r.copy, role: "복사본" })}>안의 사진 보기</button>
                          <button className="btn small primary" onClick={() => { setSeen((x) => new Set(x).add(r.key)); openWalk("near", s.key); }}>나란히 비교</button>
                          {s.rows.length === 1 && (
                            <button className="btn small" title="이 폴더를 원본으로 남기고, 다른 쪽에서 겹치는 것을 지웁니다" onClick={() => { toggle([k], false); setTurned((t) => { const n = new Set(t); if (!n.delete(r.key)) n.add(r.key); return n; }); }}>
                              이쪽을 원본으로
                            </button>
                          )}
                          <button className="btn small" onClick={() => open(r.copy)}>폴더 열기</button>
                        </>,
                      );
                    })}
                  </div>
                ))}
              </div>
              {nearList.length > nearShown && <button className="btn" onClick={() => setNearShown((n) => n + PAGE)}>더 보기 ({fmtCount(nearList.length - nearShown)}묶음 남음)</button>}
            </>
          )}

          {quickOn && recovered.length > 0 && (
            <div className="alert warn small">
              <b>빠른 확인을 권하지 않는 폴더가 선택되어 있습니다.</b> 복구 프로그램이 만든 폴더의 파일은 앞부분만 같고 뒤가 깨진 경우가 있어, 일부만 비교하면 놓칠 수 있습니다:
              <div className="cf-left">{recovered.slice(0, 4).map((p) => <div key={p}>{p}</div>)}{recovered.length > 4 && <div>외 {fmtCount(recovered.length - 4)}개</div>}</div>
            </div>
          )}
          {(exact.length > 0 || near.length > 0) && <div className="cf-foot">
            <div className="cf-opts">
            <label className="small">
              <input type="checkbox" checked={keepInQuarantine} onChange={(e) => setKeepInQuarantine(e.target.checked)} /> 지우지 말고 격리함에만 두기{" "}
              <span className="muted">(되돌릴 수 있지만 공간은 생기지 않음)</span>
            </label>
            <label className="small" title="큰 파일은 처음·끝과 32MB마다 1MB씩만 원본과 비교합니다. 4MB 이하 파일은 그대로 전부 비교합니다.">
              <input type="checkbox" checked={quickOn} disabled={keepInQuarantine} onChange={(e) => setQuick(e.target.checked)} /> 빠른 확인{" "}
              <span className="muted">(파일의 일부만 비교 — 더 빠르지만 비교하지 않은 곳이 다른 파일은 걸러내지 못함)</span>
            </label>
            </div>
            <span className="cf-btns">
              {picks.length === 0
                ? <span className="muted small">지울 복사본을 체크하세요</span>
                : <span className="muted small">100% 복사 폴더 {fmtCount(exactGoing.length)}개 · 거의 같은 폴더 {fmtCount(nearGoing.length)}개 선택</span>}
              <button className="btn primary" disabled={!!running || picks.length === 0} onClick={() => setConfirming(true)}>
                선택한 {fmtCount(picks.length)}개 폴더 {keepInQuarantine ? "격리" : "지우기"} ({fmtBytes(goingBytes)})
              </button>
            </span>
          </div>}
        </>
      )}

      {confirming && (
        <div className="send-back" onClick={() => setConfirming(false)}>
          <div className="card sel-preview cf-confirm" onClick={(e) => e.stopPropagation()}>
            <h3>{keepInQuarantine ? "복사 폴더를 격리함으로 옮기기" : "복사 폴더 지우기"}</h3>
            <div>
              폴더 <b>{fmtCount(picks.length)}개</b>에서 사진·영상 <b>{fmtCount(goingFiles)}개</b>(<b>{fmtBytes(goingBytes)}</b>)를 {keepInQuarantine ? "격리함으로 옮깁니다." : "지웁니다."}
            </div>
            <div className="small muted">
              {keepInQuarantine
                ? "「작업 기록」에서 되돌릴 수 있지만, 격리함을 비울 때까지 공간은 생기지 않습니다."
                : quickOn
                  ? "파일마다 원본 폴더의 짝과 일부(처음·끝·중간 여러 곳)를 비교해 같은 것만 지웁니다. 비교하지 않은 곳만 다른 파일은 같다고 보고 지웁니다. 지운 것은 되돌릴 수 없습니다."
                  : "파일마다 원본 폴더의 짝과 끝까지 비교해 같은 것만 지웁니다. 지운 것은 되돌릴 수 없습니다. 복사본에만 있는 파일은 지우지 않습니다."}
            </div>
            {!keepInQuarantine && (
              <div className="small muted">
                {quickOn
                  ? `${fmtBytes(goingBytes)} 중 일부만 읽습니다. 진행 중에 남은 시간이 표시됩니다.`
                  : `${fmtBytes(goingBytes)}를 원본과 비교하므로 그 두 배를 읽습니다. 디스크에 따라 수십 분에서 몇 시간이 걸리며, 진행 중에 속도와 남은 시간이 표시됩니다. 중간에 멈춰도 「작업 기록」의 「이어서 지우기」로 마저 지울 수 있습니다.`}
              </div>
            )}
            {quickOn && recovered.length > 0 && <div className="alert warn small"><b>복구 폴더가 포함되어 있습니다.</b> 이 폴더들은 「빠른 확인」을 끄고 지우시길 권합니다.</div>}
            <div className="cf-confirm-list small">
              {exactGoing.map(({ r, f }) => (
                <div key={dirKey(f)}>
                  <span className="cf-tag go">폴더째 지움</span> <span className="mono">{f.abs_path}</span>
                  <div className="muted">원본: <span className="mono">{r.keep.abs_path}</span> · {fmtCount(r.set.files)}개 · {fmtBytes(r.set.bytes)}</div>
                </div>
              ))}
              {nearGoing.map((r) => (
                <div key={r.key}>
                  <span className="cf-tag go">겹치는 것만 지움</span> <span className="mono">{r.copy.abs_path}</span>
                  <div className="muted">원본: <span className="mono">{r.keep.abs_path}</span> · {fmtCount(r.shared)}개 · {fmtBytes(r.sharedBytes)} · 이 폴더에 {fmtCount(r.onlyCopy)}개 남음</div>
                </div>
              ))}
            </div>
            <div className="cf-btns">
              <button className="btn" onClick={() => setConfirming(false)}>취소</button>
              <button className="btn primary danger-fill" onClick={run}>{keepInQuarantine ? "격리함으로 옮기기" : `${fmtCount(picks.length)}개 폴더 지우기`}</button>
            </div>
          </div>
        </div>
      )}
      {peek && <FolderPeek peek={peek} onClose={() => setPeek(null)} />}
      {walk && walkSets.length > 0 && (
        <FolderWalk
          sets={walkSets}
          index={Math.max(0, walkSets.findIndex((x) => x.key === walk.key))}
          onIndex={(index) => setWalk({ tab: walk.tab, key: walkSets[index].key })}
          onClose={() => setWalk(null)}
          onToggle={toggleSet}
          onReviewed={markReviewed}
        />
      )}
    </section>
  );
}

function Result({ r, onClose, onReview }: { r: CopyFolderReport; onClose: () => void; onReview: () => void }) {
  const clean = r.failed === 0 && r.different === 0 && !r.cancelled;
  return (
    <div className={`alert ${clean ? "ok" : "warn"} cf-result`}>
      <div>
        <b>
          폴더 {fmtCount(r.folders + r.partial)}개 정리 · 사진·영상 {fmtCount(r.files)}개 ·{" "}
          {r.purged ? `${fmtBytes(r.bytes)} 확보` : `${fmtBytes(r.bytes)} 격리함으로 옮김`}
        </b>
        {r.quick && " · 빠른 확인(파일의 일부만 비교)으로 지움"}
        {r.cancelled && " (중간에 멈춤 — 남은 파일은 격리함에 있습니다. 「작업 기록」에서 「이어서 지우기」를 누르면 남은 것을 마저 지우고, 「되돌리기」로 제자리에 돌려놓을 수 있습니다)"}
      </div>
      <ul className="small">
        {r.folders > 0 && <li>복사 폴더 {fmtCount(r.folders)}개는 폴더째 없어졌습니다.</li>}
        {r.unique_left > 0 && <li>복사본에만 있던 사진·영상 {fmtCount(r.unique_left)}개는 그 폴더에 그대로 남겼습니다.</li>}
        {r.labels_moved > 0 && <li>♥ 등 직접 붙인 표시 {fmtCount(r.labels_moved)}개를 남는 사진으로 옮겼습니다.</li>}
        {r.other_deleted > 0 && <li>사진이 아닌 파일 {fmtCount(r.other_deleted)}개도 지웠습니다 (원본 폴더의 같은 자리에 똑같이 있는 파일, Thumbs.db 같은 시스템 파일).</li>}
        {r.different > 0 && (
          <li>
            <b>끝까지 비교하니 원본과 내용이 달랐던 {fmtCount(r.different)}개는 지우지 않고 제자리에 두었습니다.</b> 둘 중 하나가 깨졌거나 덜 복구된 파일일 수 있습니다.
            {r.to_review.length > 0 && <> <button className="btn small primary" onClick={onReview}>나란히 보고 고르기</button></>}
            <div className="cf-left muted">{r.to_review.slice(0, 5).map((p) => <div key={p.copy_path}>{p.copy_path} ({fmtCount(p.files)}개)</div>)}</div>
          </li>
        )}
        {r.left > 0 && (
          <li>
            복사 폴더에만 있던 그 밖의 파일 {fmtCount(r.left)}개는 그대로 두었습니다:
            <div className="cf-left muted">{r.left_examples.slice(0, 5).map((p) => <div key={p}>{p}</div>)}</div>
          </li>
        )}
        {r.skipped > 0 && (
          <li>
            {fmtCount(r.skipped)}개 폴더는 건드리지 않았습니다:
            <div className="cf-left muted">{r.skipped_why.slice(0, 5).map((p) => <div key={p}>{p}</div>)}</div>
          </li>
        )}
        {r.failed > 0 && <li>실패 {fmtCount(r.failed)}개: {r.errors.slice(0, 3).join(" · ")}</li>}
      </ul>
      <button className="btn small" onClick={onClose}>닫기</button>
    </div>
  );
}

import { useCallback, useEffect, useMemo, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  api, fmtBytes, fmtCount, thumbUrl,
  type CopyFolderInfo, type CopyFolderPick, type CopyFolderReport, type CopyFolders as Found, type CopyFolderSet,
} from "../api";
import { useJob } from "../App";

const PAGE = 100;
const setKey = (s: CopyFolderSet) => `${s.keep.volume_id}:${s.keep.rel_dir}`;
const dirKey = (f: CopyFolderInfo) => `${f.volume_id}:${f.rel_dir}`;
/** A folder's own date is a real moment (not a photo's wall-clock time): shown in local time. */
const day = (ts: number | null) => (ts == null ? null : new Date(ts * 1000).toLocaleDateString("sv-SE"));

const WHY: Record<string, string> = {
  keep_rule: "「항상 남길 폴더」",
  filed: "라이브러리에 정리된 폴더",
  oldest: "가장 먼저 만든 폴더",
};

/**
 * 「100% 복사 폴더」: folders copied whole. Each row is one folder and its identical copies; the
 * oldest stays (the user can keep another), the copies are deleted after every file was compared
 * with its twin byte for byte.
 */
export default function CopyFolders({ onTotals }: { onTotals?: (bytes: number) => void }) {
  const { version, running, setRunning, lastDone } = useJob();
  const [found, setFound] = useState<Found | null>(null);
  const [err, setErr] = useState<string | null>(null);
  /** Which folder of a set stays, when not the suggested one (index into [keep, ...copies]). */
  const [stays, setStays] = useState<Record<string, number>>({});
  /** Copy folders the user unticked. */
  const [off, setOff] = useState<Set<string>>(new Set());
  const [shown, setShown] = useState(PAGE);
  const [photos, setPhotos] = useState<string | null>(null);
  const [keepInQuarantine, setKeepInQuarantine] = useState(false);
  const [result, setResult] = useState<CopyFolderReport | null>(null);

  const load = useCallback(() => {
    api.copyFolders().then(setFound).catch((e) => setErr(String(e)));
  }, []);
  useEffect(load, [load, version]);
  useEffect(() => {
    if (lastDone?.job !== "cleanup:folders") return;
    if (lastDone.ok) setResult(lastDone.result as CopyFolderReport);
    else if (lastDone.error !== "cancelled") setErr(lastDone.error ?? "복사 폴더를 정리하지 못했습니다");
  }, [lastDone]);

  /** Per set: the folder that stays and the ones that go (ticked, and not protected by a rule). */
  const rows = useMemo(
    () =>
      (found?.sets ?? []).map((s) => {
        const all = [s.keep, ...s.copies];
        const k = Math.min(stays[setKey(s)] ?? 0, all.length - 1);
        const others = all.filter((_, i) => i !== k);
        const going = others.filter((f) => !f.keep_rule && !off.has(dirKey(f)));
        return { set: s, key: setKey(s), all, keep: all[k], others, going };
      }),
    [found, stays, off],
  );
  const picks: CopyFolderPick[] = rows.flatMap((r) =>
    r.going.map((f) => ({ keep: { volume_id: r.keep.volume_id, rel_dir: r.keep.rel_dir }, copy: { volume_id: f.volume_id, rel_dir: f.rel_dir } })),
  );
  const goingFiles = rows.reduce((a, r) => a + r.going.length * r.set.files, 0);
  const goingBytes = rows.reduce((a, r) => a + r.going.length * r.set.bytes, 0);
  const possibleBytes = rows.reduce((a, r) => a + r.others.filter((f) => !f.keep_rule).length * r.set.bytes, 0);
  useEffect(() => onTotals?.(possibleBytes), [possibleBytes, onTotals]);

  const toggle = (f: CopyFolderInfo) =>
    setOff((o) => {
      const n = new Set(o);
      if (!n.delete(dirKey(f))) n.add(dirKey(f));
      return n;
    });
  const allOff = picks.length === 0;
  const toggleAll = () => setOff(allOff ? new Set() : new Set(rows.flatMap((r) => r.others.map(dirKey))));

  const run = async () => {
    const ok = await confirm(
      keepInQuarantine
        ? `폴더 ${fmtCount(picks.length)}개의 사진·영상 ${fmtCount(goingFiles)}개(${fmtBytes(goingBytes)})를 격리함으로 옮깁니다.\n\n` +
            "「작업 기록」에서 되돌릴 수 있지만, 격리함을 비울 때까지 공간은 생기지 않습니다."
        : `폴더 ${fmtCount(picks.length)}개의 사진·영상 ${fmtCount(goingFiles)}개(${fmtBytes(goingBytes)})를 지웁니다.\n\n` +
            "남는 폴더와 내용을 끝까지 비교해 같은 것만 지우며, 되돌릴 수 없습니다.",
      { title: "100% 복사 폴더 지우기", kind: "warning", okLabel: keepInQuarantine ? "옮기기" : "지우기", cancelLabel: "취소" },
    );
    if (!ok) return;
    setErr(null);
    setResult(null);
    try {
      await api.startRemoveCopyFolders(picks, !keepInQuarantine);
      setRunning("cleanup:folders");
    } catch (e) {
      setErr(String(e));
    }
  };

  const open = (f: CopyFolderInfo) => api.openFolder(f.abs_path).catch((e) => setErr(String(e)));
  const empty = found != null && found.sets.length === 0;

  return (
    <section className="cleanup-tier tier-first">
      <h2 className="tier-title">가장 먼저 — 100% 복사 폴더</h2>
      <p className="muted small tier-note">
        폴더째 복사해 둔 것입니다. 안의 사진·영상이 개수도 내용도 완전히 같은 폴더끼리 묶었습니다. 가장 먼저 만든 폴더를 남기고 복사 폴더를 지웁니다.{" "}
        {found?.drive
          ? `${found.drive} 드라이브만 봅니다. 다른 드라이브의 같은 폴더는 백업이므로 건드리지 않습니다.`
          : "한 드라이브 안에서만 비교합니다. 다른 드라이브의 같은 폴더는 백업이므로 건드리지 않습니다."}
      </p>
      {err && <div className="alert bad">{err}</div>}
      {result && <Result r={result} onClose={() => setResult(null)} />}

      {found == null ? (
        <div className="card muted small">폴더를 비교하는 중…</div>
      ) : empty ? (
        <div className="card muted small">
          폴더째 복사된 것이 없습니다.
          {found.unread > 0 && ` 아직 내용을 확인하지 못한 파일이 ${fmtCount(found.unread)}개 있어, 확인이 끝나면 더 나올 수 있습니다.`}
        </div>
      ) : (
        <div className="card cf-card">
          <div className="cf-head">
            <div>
              <b>{fmtCount(rows.length)}묶음</b> · 복사 폴더 {fmtCount(rows.reduce((a, r) => a + r.others.length, 0))}개 ·{" "}
              <b>{fmtBytes(possibleBytes)}</b>
              {found.unread > 0 && <span className="muted small"> · 아직 확인하지 못한 파일 {fmtCount(found.unread)}개 (끝나면 더 나올 수 있음)</span>}
            </div>
            <button className="btn small" onClick={toggleAll}>{allOff ? "모두 선택" : "모두 해제"}</button>
          </div>

          <div className="cf-list">
            {rows.slice(0, shown).map((r) => (
              <div key={r.key} className="cf-set">
                <div className="cf-row keep">
                  <span className="cf-tag keep">남김</span>
                  <span className="cf-path" title={r.keep.abs_path}>{r.keep.abs_path}</span>
                  <span className="cf-meta muted small">
                    {day(r.keep.created) && `만든 날 ${day(r.keep.created)} · `}
                    {fmtCount(r.set.files)}개 · {fmtBytes(r.set.bytes)}
                    {r.keep === r.set.keep && WHY[r.set.keep_why] && ` · ${WHY[r.set.keep_why]}`}
                  </span>
                  <span className="cf-btns">
                    <button className="btn small" onClick={() => setPhotos(photos === r.key ? null : r.key)}>{photos === r.key ? "사진 닫기" : "사진 보기"}</button>
                    <button className="btn small" onClick={() => open(r.keep)}>폴더 열기</button>
                  </span>
                </div>
                {r.others.map((f) => {
                  const on = !f.keep_rule && !off.has(dirKey(f));
                  return (
                    <div key={dirKey(f)} className={`cf-row ${on ? "go" : "stay"}`}>
                      <label className={`cf-tag ${on ? "go" : ""}`}>
                        <input type="checkbox" checked={on} disabled={f.keep_rule} onChange={() => toggle(f)} /> {on ? "지움" : "그대로"}
                      </label>
                      <span className="cf-path" title={f.abs_path}>{f.abs_path}</span>
                      <span className="cf-meta muted small">
                        {day(f.created) && `만든 날 ${day(f.created)} · `}
                        {f.keep_rule ? "「항상 남길 폴더」라서 지우지 않음" : f.names === r.keep.names ? "이름까지 같음" : "내용은 같고 파일 이름·위치만 다름"}
                        {f.filed && " · 라이브러리에 정리된 폴더"}
                      </span>
                      <span className="cf-btns">
                        <button className="btn small" title="이 폴더를 남기고 위의 폴더를 지웁니다" onClick={() => setStays((m) => ({ ...m, [r.key]: r.all.indexOf(f) }))}>
                          이쪽을 남기기
                        </button>
                        <button className="btn small" onClick={() => open(f)}>폴더 열기</button>
                      </span>
                    </div>
                  );
                })}
                {photos === r.key && (
                  <div className="cf-photos">
                    {r.set.sample.map((id) => <img key={id} src={thumbUrl(id)} alt="" loading="lazy" />)}
                    {r.set.files > r.set.sample.length && <span className="muted small">외 {fmtCount(r.set.files - r.set.sample.length)}개 — 모든 폴더에 똑같이 들어 있습니다</span>}
                  </div>
                )}
              </div>
            ))}
          </div>
          {rows.length > shown && (
            <button className="btn small" onClick={() => setShown((n) => n + PAGE)}>더 보기 ({fmtCount(rows.length - shown)}묶음 남음)</button>
          )}

          <div className="cf-foot">
            <label className="small">
              <input type="checkbox" checked={keepInQuarantine} onChange={(e) => setKeepInQuarantine(e.target.checked)} /> 지우지 말고 격리함에만 두기{" "}
              <span className="muted">(되돌릴 수 있지만 공간은 생기지 않음)</span>
            </label>
            <button className="btn primary" disabled={!!running || picks.length === 0} onClick={run}>
              선택한 {fmtCount(picks.length)}개 폴더 {keepInQuarantine ? "격리" : "지우기"} ({fmtBytes(goingBytes)})
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

function Result({ r, onClose }: { r: CopyFolderReport; onClose: () => void }) {
  const clean = r.failed === 0 && r.different === 0 && !r.cancelled;
  return (
    <div className={`alert ${clean ? "ok" : "warn"} cf-result`}>
      <div>
        <b>
          복사 폴더 {fmtCount(r.folders + r.partial)}개 정리 · 사진·영상 {fmtCount(r.files)}개 ·{" "}
          {r.purged ? `${fmtBytes(r.bytes)} 확보` : `${fmtBytes(r.bytes)} 격리함으로 옮김`}
        </b>
        {r.cancelled && " (중간에 멈춤 — 옮긴 파일은 격리함에 있으며 「작업 기록」에서 되돌리거나 비울 수 있습니다)"}
      </div>
      <ul className="small">
        {r.labels_moved > 0 && <li>♥ 등 직접 붙인 표시 {fmtCount(r.labels_moved)}개를 남는 사진으로 옮겼습니다.</li>}
        {r.other_deleted > 0 && <li>사진이 아닌 파일 {fmtCount(r.other_deleted)}개도 지웠습니다 (남는 폴더의 같은 자리에 똑같이 있는 파일, Thumbs.db 같은 시스템 파일).</li>}
        {r.different > 0 && (
          <li>끝까지 비교하니 내용이 달랐던 {fmtCount(r.different)}개는 지우지 않고 격리함에 두었습니다 — 「작업 기록」에서 되돌릴 수 있습니다.</li>
        )}
        {r.left > 0 && (
          <li>
            복사 폴더에만 있던 파일 {fmtCount(r.left)}개는 그대로 두었습니다{r.partial > 0 && ` (폴더 ${fmtCount(r.partial)}개가 남음)`}:
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

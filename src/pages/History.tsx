import { useEffect, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type BatchRow } from "../api";
import { useJob } from "../App";

const HOW = (quick: boolean) =>
  quick
    ? "빠른 확인: 원본이 있는 사본은 원본과 일부(앞·끝과 32MB마다 1MB)만 비교하고 지웁니다. 가운데만 깨진 파일은 알아채지 못합니다."
    : "원본이 있는 사본은 원본과 같은지 확인한 뒤 지웁니다(이미 확인된 것은 다시 읽지 않습니다). 진행 중 남은 시간이 표시되고, 멈췄다가 다시 누르면 이어집니다.";

const STATE_NAMES: Record<string, string> = { running: "진행 중", done: "완료", undone: "되돌림", purged: "격리함 비움" };

export default function History() {
  const { version, running, setRunning, lastDone } = useJob();
  const [rows, setRows] = useState<BatchRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  // "Quick check": compare parts of each copy with its original instead of all of it.
  const [quick, setQuick] = useState(false);

  useEffect(() => {
    api.batches().then(setRows);
  }, [version]);

  const undo = async (b: BatchRow) => {
    const ok = await confirm(`"${b.description}" 작업을 되돌립니다. 파일이 원래 위치로 돌아갑니다.`, { title: "되돌리기", kind: "info" });
    if (!ok) return;
    try {
      await api.startUndo(b.id);
      setRunning("undo");
    } catch (e) {
      setError(String(e));
    }
  };

  const purge = async (b: BatchRow) => {
    const ok = await confirm(
      `격리함의 파일 ${fmtCount(b.waiting)}개(${fmtBytes(b.bytes)})를 영구 삭제합니다.\n\n` +
        `${HOW(quick)} 이 작업은 되돌릴 수 없습니다.`,
      { title: "격리함 비우기", kind: "error" },
    );
    if (!ok) return;
    try {
      await api.startPurge(b.id, quick);
      setRunning("purge");
    } catch (e) {
      setError(String(e));
    }
  };

  // Everything that waits in the quarantine, whatever batch put it there.
  const waiting = rows.filter((b) => b.state === "done" && b.waiting > 0);
  const waitingFiles = waiting.reduce((n, b) => n + b.waiting, 0);
  const purgeAll = async () => {
    const ok = await confirm(
      `격리함에 있는 파일 전부(작업 ${fmtCount(waiting.length)}건, ${fmtCount(waitingFiles)}개)를 영구 삭제합니다.\n\n` +
        `${HOW(quick)} 이 작업은 되돌릴 수 없습니다.`,
      { title: "격리함 전부 비우기", kind: "error" },
    );
    if (!ok) return;
    try {
      await api.startPurgeAll(quick);
      setRunning("purge");
    } catch (e) {
      setError(String(e));
    }
  };

  const undoResult = lastDone && (lastDone.job === "undo" || lastDone.job === "purge") ? lastDone : null;

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>작업 기록 · 되돌리기</h1>
          <div className="muted">앱이 한 모든 이동·복사·격리는 기록되어 되돌릴 수 있습니다. 중복 정리나 삭제로 지운 파일은 같은 드라이브의 격리 폴더에 보관되며, 「격리함 비우기」를 해야만 실제로 삭제됩니다.
            (「한 곳으로 정리」에서 한 해씩 Push할 때 지운 사본은 원본과 내용을 비교한 뒤 바로 삭제되어 「격리함 비움」으로 표시됩니다.)</div>
        </div>
      </header>
      {error && <div className="alert bad">{error}</div>}
      {waitingFiles > 0 && (
        <div className="purge-all">
          <div>
            <b>격리함에 {fmtCount(waitingFiles)}개</b> <span className="muted">(작업 {fmtCount(waiting.length)}건)</span>
            <label className="small" title="원본과 일부만 비교합니다. 큰 영상이 많을 때 훨씬 빠르지만, 가운데만 깨진 파일은 알아채지 못합니다. 복구 폴더에서 온 사본에는 권하지 않습니다.">
              <input type="checkbox" checked={quick} onChange={(e) => setQuick(e.target.checked)} /> 빠른 확인 (일부만 비교)
            </label>
          </div>
          <button className="btn danger" disabled={!!running} onClick={purgeAll}>격리함 전부 비우기</button>
        </div>
      )}
      {undoResult && undoResult.ok && (
        <div className={`alert ${undoResult.result.failed ? "bad" : "ok"}`}>
          {undoResult.job === "undo" ? "되돌림" : "삭제"} {fmtCount(undoResult.result.restored)}개
          {undoResult.job === "purge" && undoResult.result.bytes > 0 && <> · {fmtBytes(undoResult.result.bytes)} 확보</>}
          {undoResult.result.failed > 0 && <> · 실패 {undoResult.result.failed}개<pre>{undoResult.result.errors.join("\n")}</pre></>}
        </div>
      )}
      <table className="table">
        <thead><tr><th>시각</th><th>작업</th><th>파일</th><th>상태</th><th /></tr></thead>
        <tbody>
          {rows.length === 0 && <tr><td colSpan={5} className="muted">아직 기록이 없습니다.</td></tr>}
          {rows.map((b) => (
            <tr key={b.id}>
              <td className="muted small">{new Date(b.created_at * 1000).toLocaleString("ko-KR")}</td>
              <td>{b.kind === "organize" ? "⇲ " : "🗑 "}{b.description}</td>
              <td>
                {fmtCount(b.op_count)}개 · {fmtBytes(b.bytes)}
                {b.state === "done" && b.waiting > 0 && b.waiting < b.op_count && <div className="small warn-text">격리함에 {fmtCount(b.waiting)}개 남음</div>}
              </td>
              <td><span className={`pill ${b.state === "done" ? "ok" : ""}`}>{STATE_NAMES[b.state] ?? b.state}</span></td>
              <td className="right">
                <button className="btn small" disabled={!!running || b.state !== "done"} onClick={() => undo(b)}>되돌리기</button>
                {/* Anything that left files in the quarantine can be emptied: deletions, and copies
                    whose run was stopped before they were compared and deleted. */}
                {(b.kind === "dedupe" || b.kind === "delete" || (b.kind === "copies" && b.waiting > 0)) && (
                  <button className="btn small danger" disabled={!!running || b.state !== "done" || b.waiting === 0} onClick={() => purge(b)}>
                    {b.kind === "copies" ? "이어서 지우기" : "격리함 비우기"}
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

import { useEffect, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type BatchRow } from "../api";
import { useJob } from "../App";

const STATE_NAMES: Record<string, string> = { running: "진행 중", done: "완료", undone: "되돌림", purged: "격리함 비움" };

export default function History() {
  const { version, running, setRunning, lastDone } = useJob();
  const [rows, setRows] = useState<BatchRow[]>([]);
  const [error, setError] = useState<string | null>(null);

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
      `격리함의 파일 ${fmtCount(b.op_count)}개(${fmtBytes(b.bytes)})를 영구 삭제합니다.\n\n이 작업은 되돌릴 수 없습니다.`,
      { title: "격리함 비우기", kind: "error" },
    );
    if (!ok) return;
    try {
      await api.startPurge(b.id);
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
          <div className="muted">앱이 한 모든 이동·복사·격리는 기록되어 되돌릴 수 있습니다. 중복 정리나 삭제로 지운 파일은 같은 드라이브의 격리 폴더에 보관되며, 「격리함 비우기」를 해야만 실제로 삭제됩니다.</div>
        </div>
      </header>
      {error && <div className="alert bad">{error}</div>}
      {undoResult && undoResult.ok && (
        <div className={`alert ${undoResult.result.failed ? "bad" : "ok"}`}>
          {undoResult.job === "undo" ? "되돌림" : "삭제"} {fmtCount(undoResult.result.restored)}개
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
              <td>{fmtCount(b.op_count)}개 · {fmtBytes(b.bytes)}</td>
              <td><span className={`pill ${b.state === "done" ? "ok" : ""}`}>{STATE_NAMES[b.state] ?? b.state}</span></td>
              <td className="right">
                <button className="btn small" disabled={!!running || b.state !== "done"} onClick={() => undo(b)}>되돌리기</button>
                {(b.kind === "dedupe" || b.kind === "delete") && (
                  <button className="btn small danger" disabled={!!running || b.state !== "done"} onClick={() => purge(b)}>격리함 비우기</button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

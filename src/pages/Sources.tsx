import { useEffect, useState } from "react";
import { open, confirm } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type SourceRow, type VolumeRow } from "../api";
import { useJob } from "../App";

function ago(ts: number) {
  if (!ts) return "스캔한 적 없음";
  const d = new Date(ts * 1000);
  return `${d.toLocaleDateString("ko-KR")} ${d.toLocaleTimeString("ko-KR", { hour: "2-digit", minute: "2-digit" })} 스캔`;
}

export default function Sources() {
  const { version, running, setRunning, lastDone } = useJob();
  const [volumes, setVolumes] = useState<VolumeRow[]>([]);
  const [sources, setSources] = useState<SourceRow[]>([]);
  const [error, setError] = useState<string | null>(null);

  const reload = (refresh = false) => {
    api.listVolumes(refresh).then(setVolumes);
    api.listSources().then(setSources);
  };
  useEffect(() => reload(true), [version]);

  const addFolder = async () => {
    setError(null);
    const picked = await open({ directory: true, multiple: true, title: "사진·동영상이 있는 폴더 선택 (드라이브 전체도 가능)" });
    if (!picked) return;
    const list = Array.isArray(picked) ? picked : [picked];
    try {
      for (const p of list) await api.addSource(p);
      reload();
      if (await confirm("폴더를 추가했습니다. 지금 스캔할까요?", { title: "스캔", kind: "info" })) scan();
    } catch (e) {
      setError(String(e));
    }
  };

  const scan = async (id?: number) => {
    setError(null);
    try {
      await api.startScan(id);
      setRunning("scan");
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (s: SourceRow) => {
    const ok = await confirm(`"${s.abs_path}" 를 목록에서 제거할까요?\n\n디스크의 파일은 삭제되지 않고, 앱의 색인만 지워집니다.`, { title: "폴더 제거", kind: "warning" });
    if (!ok) return;
    await api.removeSource(s.id);
    reload();
  };

  const scanResult = lastDone?.job === "scan" && lastDone.ok ? lastDone.result : null;

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>드라이브 · 폴더</h1>
          <div className="muted">C 드라이브와 외장 드라이브의 폴더를 추가하면 한 화면에서 모두 볼 수 있습니다. 외장 드라이브는 빼도 썸네일과 정보가 남습니다.</div>
        </div>
        <div className="btn-row">
          <button className="btn" onClick={() => reload(true)}>드라이브 새로고침</button>
          <button className="btn" disabled={!!running || sources.length === 0} onClick={() => scan()}>전체 다시 스캔</button>
          <button className="btn primary" onClick={addFolder}>+ 폴더 추가</button>
        </div>
      </header>
      {error && <div className="alert bad">{error}</div>}
      {scanResult && (
        <div className="alert ok">
          스캔 완료: 새 파일 {fmtCount(scanResult.scan.reduce((a: number, s: any) => a + s.added, 0))}개,
          변경 {fmtCount(scanResult.scan.reduce((a: number, s: any) => a + s.updated, 0))}개,
          사라짐 {fmtCount(scanResult.scan.reduce((a: number, s: any) => a + s.missing, 0))}개 · 썸네일 {fmtCount(scanResult.thumbs.generated)}개 생성
        </div>
      )}

      <h2 className="section-title">드라이브</h2>
      <div className="cards">
        {volumes.length === 0 && <div className="muted">아직 등록된 드라이브가 없습니다.</div>}
        {volumes.map((v) => (
          <div key={v.id} className={`card volume ${v.online ? "" : "offline"}`}>
            <div className="row-between">
              <b>{v.removable ? "⏏ " : "🖴 "}{v.label}</b>
              <span className={`pill ${v.online ? "ok" : ""}`}>{v.online ? `연결됨 · ${v.mount}` : "연결 안 됨"}</span>
            </div>
            <div className="muted">{fmtCount(v.file_count)}개 · {fmtBytes(v.total_bytes)}</div>
            {!v.online && <div className="muted small">마지막 연결: {new Date(v.last_seen * 1000).toLocaleDateString("ko-KR")}</div>}
          </div>
        ))}
      </div>

      <h2 className="section-title">스캔할 폴더</h2>
      <table className="table">
        <thead>
          <tr><th>폴더</th><th>드라이브</th><th>마지막 스캔</th><th /></tr>
        </thead>
        <tbody>
          {sources.length === 0 && (
            <tr><td colSpan={4} className="muted">「+ 폴더 추가」로 시작하세요. 예: C:\Users\이름\Pictures, E:\ (외장 드라이브 전체)</td></tr>
          )}
          {sources.map((s) => (
            <tr key={s.id} className={s.online ? "" : "offline"}>
              <td className="mono">{s.abs_path}</td>
              <td>{s.volume_label} {s.online ? "" : <span className="pill">연결 안 됨</span>}</td>
              <td className="muted">{ago(s.last_scan)}</td>
              <td className="right">
                <button className="btn small" disabled={!s.online || !!running} onClick={() => scan(s.id)}>스캔</button>
                <button className="btn small ghost" onClick={() => remove(s)}>제거</button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted small">
        Windows, Program Files, AppData, 휴지통 같은 시스템 폴더와 10KB 미만 아이콘 이미지는 자동으로 건너뜁니다.
      </p>
    </div>
  );
}

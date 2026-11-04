import { useEffect, useState } from "react";
import { api, fmtCount, fmtEta, onAnalysisStatus, type AnalysisOptions, type AppInfo, type LaneStatus } from "../api";
import ScanSettings from "../components/ScanSettings";

const OPEN_SOURCE: [string, string, string][] = [
  ["Tauri", "MIT / Apache-2.0", "앱 틀, 설치 파일, 자동 업데이트"],
  ["czkawka_core", "MIT", "비슷한 사진·동영상 찾기"],
  ["nom-exif", "MIT", "사진·동영상 정보(EXIF) 읽기"],
  ["image, jpeg-decoder, imagesize", "MIT / Apache-2.0", "이미지 디코딩·썸네일"],
  ["rusqlite (SQLite)", "MIT (SQLite: Public Domain)", "색인 데이터베이스"],
  ["ONNX Runtime · ort", "MIT / Apache-2.0", "PC 안에서 AI 모델 실행"],
  ["YuNet (OpenCV Zoo)", "MIT", "얼굴 찾기"],
  ["SFace (OpenCV Zoo)", "Apache-2.0", "얼굴 비교"],
  ["reverse_geocoder · GeoNames", "MIT / Apache-2.0 · CC-BY 4.0", "GPS를 지명으로 바꾸기 (지명 데이터: geonames.org)"],
  ["ffmpeg", "LGPL", "동영상 썸네일·변환"],
  ["React, react-virtuoso", "MIT", "화면"],
];

export default function Settings() {
  const [opts, setOpts] = useState<AnalysisOptions | null>(null);
  const [lane, setLane] = useState<LaneStatus | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  useEffect(() => {
    api.getAnalysisOptions().then(setOpts);
    api.analysisStatus().then(setLane);
    api.appInfo().then(setInfo);
    const un = onAnalysisStatus(() => api.analysisStatus().then(setLane));
    return () => {
      un.then((u) => u());
    };
  }, []);

  const save = async (patch: Partial<AnalysisOptions>) => {
    if (!opts) return;
    const next = { ...opts, ...patch };
    setOpts(next);
    await api.setAnalysisOptions(next);
  };

  const checkUpdate = async () => {
    setMsg("확인 중…");
    try {
      const u = await api.checkUpdate();
      setMsg(u.available ? `새 버전 v${u.version}이 있습니다. 왼쪽 아래 카드에서 업데이트하세요.` : `최신 버전입니다 (v${u.current})`);
    } catch (e) {
      setMsg(`업데이트 확인 실패: ${e}`);
    }
  };

  return (
    <div className="page settings-page">
      <header className="page-head">
        <div>
          <h1>설정</h1>
          <div className="muted">모든 처리는 이 PC 안에서만 이루어지며, 사진이나 얼굴 정보가 밖으로 나가지 않습니다.</div>
        </div>
      </header>

      <section className="card">
        <h2>사진 분석</h2>
        <p className="muted small">
          썸네일, 선명도(흐린 사진 찾기), 연사 묶기에 필요한 정보를 뒤에서 조금씩 만듭니다. 다른 작업을 시작하면 잠시 멈췄다가 끝나면 이어서 합니다.
        </p>
        {lane && (
          <div className="kv-inline">
            <span>분석 완료 <b>{fmtCount(lane.analyzed)}</b>개</span>
            <span>남음 <b>{fmtCount(lane.pending_online)}</b>개{lane.pending_online > 0 && lane.running && lane.eta_secs > 0 ? ` (${fmtEta(lane.eta_secs)})` : ""}</span>
            {lane.pending_offline > 0 && <span className="muted">연결 안 된 드라이브 {fmtCount(lane.pending_offline)}개 (연결하면 이어서)</span>}
          </div>
        )}
        {opts && (
          <>
            <label className="check-row"><input type="checkbox" checked={opts.auto} onChange={(e) => save({ auto: e.target.checked })} /> 스캔이 끝나면 자동으로 분석</label>
            <label className="check-row"><input type="checkbox" checked={opts.pause_on_battery} onChange={(e) => save({ pause_on_battery: e.target.checked })} /> 노트북이 배터리로 동작할 때는 멈추기</label>
          </>
        )}
        <div className="btn-row">
          {lane?.running && !lane.paused ? (
            <button className="btn" onClick={() => api.analysisPause(true)}>일시정지</button>
          ) : (
            <button className="btn primary" disabled={!lane || lane.pending_online === 0} onClick={() => api.analysisStart()}>
              {lane?.paused ? "다시 시작" : "지금 분석"}
            </button>
          )}
          {lane?.last_error && <span className="warn-text small">{lane.last_error}</span>}
        </div>
      </section>

      <section className="card">
        <h2>건너뛸 파일</h2>
        <ScanSettings />
      </section>

      <section className="card">
        <h2>앱 정보</h2>
        <div className="kv">
          <div><span>버전</span><b>v{info?.version}</b></div>
          <div><span>색인 데이터</span><b className="mono">{info?.data_dir}</b></div>
          <div><span>썸네일 캐시</span><b className="mono">{info?.cache_dir}</b></div>
          <div><span>ffmpeg</span><b>{info ? (info.ffmpeg ? "있음" : "없음 (동영상 기능 제한)") : ""}</b></div>
        </div>
        <div className="btn-row">
          <button className="btn" onClick={checkUpdate}>업데이트 확인</button>
          {msg && <span className="muted small">{msg}</span>}
        </div>
        <details>
          <summary>사용한 오픈소스</summary>
          <table className="table small">
            <tbody>
              {OPEN_SOURCE.map(([n, l, u]) => (
                <tr key={n}><td>{n}</td><td className="muted">{l}</td><td>{u}</td></tr>
              ))}
            </tbody>
          </table>
        </details>
      </section>
    </div>
  );
}

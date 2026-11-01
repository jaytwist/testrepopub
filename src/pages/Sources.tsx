import { useEffect, useState } from "react";
import { open, confirm } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type ScanOptions, type ScanStats, type SourceRow, type SuggestedFolder, type VolumeRow } from "../api";
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
  const [suggested, setSuggested] = useState<SuggestedFolder[]>([]);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [settingsOpen, setSettingsOpen] = useState(false);

  const reload = (refresh = false) => {
    api.listVolumes(refresh).then(setVolumes);
    api.listSources().then(setSources);
    api.suggestedFolders().then((s) => {
      setSuggested(s);
      setPicked(new Set(s.filter((f) => !f.already_added && f.kind === "user" && f.label !== "문서").map((f) => f.path)));
    });
  };

  const addPicked = async () => {
    setError(null);
    try {
      for (const p of picked) await api.addSource(p);
      reload();
      scan();
    } catch (e) {
      setError(String(e));
    }
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
  const scans: ScanStats[] = scanResult?.scan ?? [];
  const sum = (f: (s: ScanStats) => number) => scans.reduce((a, s) => a + f(s), 0);
  const skipParts: [string, number][] = [
    ["시스템·캐시 폴더", sum((s) => s.skipped.system_folders)],
    ["숨김 폴더", sum((s) => s.skipped.hidden_folders)],
    ["프로그램·게임 안의 이미지", sum((s) => s.skipped.app_files)],
    ["숨김·시스템 파일", sum((s) => s.skipped.hidden_files + s.skipped.system_files)],
    ["클라우드 전용(OneDrive) 파일", sum((s) => s.skipped.cloud_only)],
    ["아이콘 크기 이미지", sum((s) => s.skipped.too_small)],
  ];
  const remaining = suggested.filter((f) => !f.already_added);

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
          <div>
            스캔 완료: 새 파일 <b>{fmtCount(sum((s) => s.added))}</b>개, 변경 {fmtCount(sum((s) => s.updated))}개,
            목록에서 빠짐 {fmtCount(sum((s) => s.missing))}개 · 썸네일 {fmtCount(scanResult.thumbs.generated)}개 생성
          </div>
          {skipParts.some(([, n]) => n > 0) && (
            <div className="small muted">
              사용자 파일이 아니라서 건너뜀: {skipParts.filter(([, n]) => n > 0).map(([t, n]) => `${t} ${fmtCount(n)}`).join(" · ")}
            </div>
          )}
        </div>
      )}

      {remaining.length > 0 && (
        <div className="card quick-add">
          <div className="row-between">
            <div>
              <b>내 사진 폴더 빠르게 추가</b>
              <div className="muted small">C 드라이브 전체 대신 사용자 폴더만 고르면 더 빠르고 깔끔합니다. 외장 드라이브는 통째로 추가해도 시스템 폴더는 자동으로 건너뜁니다.</div>
            </div>
            <button className="btn primary" disabled={picked.size === 0 || !!running} onClick={addPicked}>선택한 {picked.size}개 추가하고 스캔</button>
          </div>
          <div className="quick-list">
            {remaining.map((f) => (
              <label key={f.path} className={`quick-item ${picked.has(f.path) ? "on" : ""}`}>
                <input
                  type="checkbox"
                  checked={picked.has(f.path)}
                  onChange={(e) => {
                    const n = new Set(picked);
                    e.target.checked ? n.add(f.path) : n.delete(f.path);
                    setPicked(n);
                  }}
                />
                <span>{f.kind === "drive" ? "⏏ " : "📁 "}<b>{f.label}</b></span>
                <span className="mono small muted">{f.path}</span>
              </label>
            ))}
          </div>
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
      <div className="settings-head" onClick={() => setSettingsOpen(!settingsOpen)}>
        <h2 className="section-title">스캔 설정 · 건너뛸 파일 {settingsOpen ? "▴" : "▾"}</h2>
        <span className="muted small">시스템 폴더, 프로그램·게임 폴더, 숨김 파일, 아이콘 크기 이미지는 사용자 파일이 아니므로 건너뜁니다.</span>
      </div>
      {settingsOpen && <ScanSettings />}
    </div>
  );
}

function ScanSettings() {
  const [o, setO] = useState<ScanOptions | null>(null);
  const [saved, setSaved] = useState(false);
  const [newDir, setNewDir] = useState("");
  useEffect(() => {
    api.getScanOptions().then(setO);
  }, []);
  if (!o) return null;
  const set = (p: Partial<ScanOptions>) => {
    setO({ ...o, ...p });
    setSaved(false);
  };
  const save = async () => {
    await api.setScanOptions(o);
    setSaved(true);
  };
  const addDir = () => {
    const d = newDir.trim();
    if (d && !o.exclude_dirs.some((x) => x.toLowerCase() === d.toLowerCase())) set({ exclude_dirs: [...o.exclude_dirs, d] });
    setNewDir("");
  };
  return (
    <div className="card scan-settings">
      <div className="fp-grid">
        <div>
          <label>이 크기보다 작은 이미지 건너뛰기</label>
          <div className="inline-form">
            <input type="number" min={0} className="num" value={Math.round(o.min_image_bytes / 1024)} onChange={(e) => set({ min_image_bytes: Math.max(0, Number(e.target.value)) * 1024 })} />
            <span className="muted">KB</span>
          </div>
        </div>
        <div>
          <label>긴 변이 이보다 작은 이미지 건너뛰기</label>
          <div className="inline-form">
            <input type="number" min={0} className="num" value={o.min_image_side} onChange={(e) => set({ min_image_side: Math.max(0, Number(e.target.value)) })} />
            <span className="muted">px (아이콘·스티커·웹 썸네일)</span>
          </div>
        </div>
      </div>
      <label className="check-row"><input type="checkbox" checked={o.skip_hidden} onChange={(e) => set({ skip_hidden: e.target.checked })} /> 숨김 폴더·파일 건너뛰기 (.thumbnails 등)</label>
      <label className="check-row"><input type="checkbox" checked={o.skip_app_folders} onChange={(e) => set({ skip_app_folders: e.target.checked })} /> 프로그램·게임이 설치된 폴더 안의 이미지 건너뛰기 (DLL이 여러 개 있는 폴더)</label>
      <label className="check-row"><input type="checkbox" checked={o.skip_cloud_only} onChange={(e) => set({ skip_cloud_only: e.target.checked })} /> OneDrive "온라인 전용" 파일 건너뛰기 (읽으면 다운로드가 시작됨)</label>
      <div>
        <label>건너뛸 폴더 이름 <span className="muted small">(어느 위치에 있든, 대소문자 무시)</span></label>
        <div className="chips excl">
          {o.exclude_dirs.map((d) => (
            <span key={d} className="chip">{d}<button onClick={() => set({ exclude_dirs: o.exclude_dirs.filter((x) => x !== d) })} title="목록에서 빼기">×</button></span>
          ))}
        </div>
        <div className="inline-form">
          <input placeholder="폴더 이름 추가 (예: 업무자료)" value={newDir} onChange={(e) => setNewDir(e.target.value)} onKeyDown={(e) => e.key === "Enter" && addDir()} />
          <button className="btn small" onClick={addDir}>추가</button>
        </div>
      </div>
      <div className="btn-row">
        <button className="btn primary" onClick={save}>저장</button>
        <button className="btn ghost" onClick={async () => { setO(await api.defaultScanOptions()); setSaved(false); }}>기본값으로</button>
        {saved && <span className="ok-text">저장됨 · 「전체 다시 스캔」 후 적용됩니다</span>}
      </div>
    </div>
  );
}

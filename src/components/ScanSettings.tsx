import { useEffect, useState } from "react";
import { api, type ScanOptions } from "../api";

export default function ScanSettings() {
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

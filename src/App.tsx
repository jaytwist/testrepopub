import { createContext, useContext, useEffect, useState } from "react";
import { api, onJobDone, onProgress, type AppInfo, type JobDone, type LibraryStats, type Progress, fmtBytes, fmtCount } from "./api";
import Library from "./pages/Library";
import Sources from "./pages/Sources";
import Duplicates from "./pages/Duplicates";
import Organize from "./pages/Organize";
import History from "./pages/History";

type Page = "library" | "sources" | "duplicates" | "organize" | "history";

interface JobCtx {
  running: string | null;
  progress: Progress | null;
  lastDone: JobDone | null;
  /** Increments after every finished job so pages can reload. */
  version: number;
  setRunning: (name: string | null) => void;
}

export const JobContext = createContext<JobCtx>({ running: null, progress: null, lastDone: null, version: 0, setRunning: () => {} });
export const useJob = () => useContext(JobContext);

const STAGE_NAMES: Record<string, string> = {
  walk: "파일 찾는 중",
  meta: "사진 정보 읽는 중",
  thumbs: "썸네일 만드는 중",
  prehash: "중복 후보 비교 중",
  hash: "파일 지문(해시) 계산 중",
  plan: "정리 계획 만드는 중",
  run: "파일 이동 중",
  quarantine: "격리함으로 옮기는 중",
  done: "마무리 중",
};

function stageName(p: Progress): string {
  if (STAGE_NAMES[p.stage]) return STAGE_NAMES[p.stage];
  if (p.job.startsWith("dedupe:similar")) return "유사 항목 분석 중";
  return p.stage;
}

const NAV: { id: Page; label: string; icon: string }[] = [
  { id: "library", label: "라이브러리", icon: "▦" },
  { id: "sources", label: "드라이브 · 폴더", icon: "⛁" },
  { id: "duplicates", label: "중복 정리", icon: "⧉" },
  { id: "organize", label: "한 곳으로 정리", icon: "⇲" },
  { id: "history", label: "작업 기록 · 되돌리기", icon: "↺" },
];

export default function App() {
  const [page, setPage] = useState<Page>("library");
  const [running, setRunning] = useState<string | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [lastDone, setLastDone] = useState<JobDone | null>(null);
  const [version, setVersion] = useState(0);
  const [stats, setStats] = useState<LibraryStats | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [toast, setToast] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    api.appInfo().then((i) => {
      setInfo(i);
      if (i.job) setRunning(i.job);
    });
    const p = onProgress((p) => {
      setProgress(p);
      setRunning((r) => r ?? p.job);
    });
    const d = onJobDone((d) => {
      setRunning(null);
      setProgress(null);
      setLastDone(d);
      setVersion((v) => v + 1);
      if (!d.ok && d.error !== "cancelled") setToast({ ok: false, text: d.error ?? "작업 실패" });
      else if (d.ok) setToast({ ok: true, text: "작업이 완료되었습니다" });
    });
    return () => {
      p.then((u) => u());
      d.then((u) => u());
    };
  }, []);

  useEffect(() => {
    api.libraryStats().then(setStats).catch(() => {});
  }, [version, page]);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 5000);
    return () => clearTimeout(t);
  }, [toast]);

  const pct = progress && progress.total > 0 ? Math.min(100, (progress.done / progress.total) * 100) : null;

  return (
    <JobContext.Provider value={{ running, progress, lastDone, version, setRunning }}>
      <div className="app">
        <aside className="sidebar">
          <div className="brand">
            <div className="logo">◐</div>
            <div>
              <div className="brand-name">Media Organizer</div>
              <div className="brand-sub">모든 드라이브의 사진·영상</div>
            </div>
          </div>
          <nav>
            {NAV.map((n) => (
              <button key={n.id} className={`nav-item ${page === n.id ? "active" : ""}`} onClick={() => setPage(n.id)}>
                <span className="nav-icon">{n.icon}</span>
                {n.label}
              </button>
            ))}
          </nav>
          {stats && (
            <div className="side-stats">
              <div><b>{fmtCount(stats.files)}</b> 개 · {fmtBytes(stats.bytes)}</div>
              <div>사진 {fmtCount(stats.photos)} · 동영상 {fmtCount(stats.videos)} · RAW {fmtCount(stats.raws)}</div>
              {stats.offline_files > 0 && <div className="muted">연결 안 된 드라이브 {fmtCount(stats.offline_files)}개</div>}
              {stats.low_confidence_dates > 0 && <div className="warn-text">날짜 확인 필요 {fmtCount(stats.low_confidence_dates)}개</div>}
              {stats.quarantined_files > 0 && <div className="muted">격리함 {fmtCount(stats.quarantined_files)}개 · {fmtBytes(stats.quarantined_bytes)}</div>}
            </div>
          )}
          {info && !info.ffmpeg && <div className="side-warning">ffmpeg가 없어 동영상 썸네일·유사 영상 찾기가 제한됩니다.</div>}
        </aside>

        <main className="content">
          {page === "library" && <Library />}
          {page === "sources" && <Sources />}
          {page === "duplicates" && <Duplicates />}
          {page === "organize" && <Organize />}
          {page === "history" && <History />}
        </main>

        {running && (
          <div className="jobbar">
            <div className="jobbar-text">
              <span className="spinner" />
              {progress ? stageName(progress) : "작업 준비 중"}
              {progress && progress.total > 0 && (
                <span className="muted"> · {fmtCount(progress.done)} / {fmtCount(progress.total)}</span>
              )}
              {progress && progress.total === 0 && progress.done > 0 && <span className="muted"> · {fmtCount(progress.done)}개 발견</span>}
              {progress?.message && <span className="jobbar-msg">{progress.message}</span>}
            </div>
            <div className="bar"><div className={`bar-fill ${pct == null ? "indeterminate" : ""}`} style={{ width: pct == null ? "30%" : `${pct}%` }} /></div>
            <button className="btn small ghost" onClick={() => api.cancelJob()}>취소</button>
          </div>
        )}
        {toast && <div className={`toast ${toast.ok ? "ok" : "bad"}`} onClick={() => setToast(null)}>{toast.text}</div>}
      </div>
    </JobContext.Provider>
  );
}

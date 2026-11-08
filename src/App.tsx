import { createContext, useContext, useEffect, useState } from "react";
import {
  api, onAnalysisStatus, onCardConnected, onDriveConnected, type Card, onJobDone, onProgress, onUpdateProgress, type AppInfo, type FileFilter, type GroupBy, type JobDone,
  type LaneStatus, type LibraryStats, type Progress, type UpdateInfo, type UpdateProgress, fmtBytes, fmtCount, fmtEta,
} from "./api";
import Library from "./pages/Library";
import Sources from "./pages/Sources";
import Duplicates from "./pages/Duplicates";
import Organize from "./pages/Organize";
import History from "./pages/History";
import Settings from "./pages/Settings";
import Cleanup from "./pages/Cleanup";
import Trips from "./pages/Trips";
import People from "./pages/People";
import ImportPanel from "./components/ImportPanel";

export type Page = "library" | "people" | "trips" | "cleanup" | "sources" | "duplicates" | "organize" | "history" | "settings";

/** Open the library with a ready-made view (used by cleanup suggestions, people, places). */
export interface LibraryPreset {
  key: number;
  title: string;
  filter: FileFilter;
  groupBy?: GroupBy;
  /** File ids to pre-select (e.g. "everything but the best shot"). */
  select?: number[];
  /** Open the one-by-one review (keep / delete) for this suggestion kind. */
  review?: string;
}

interface NavCtx {
  go: (p: Page) => void;
  openLibrary: (p: Omit<LibraryPreset, "key">) => void;
  /** Open 「한 곳으로 정리」 for these files only (e.g. the library selection), optionally with a
   *  folder structure preselected. */
  openOrganize: (filter: FileFilter, template?: string) => void;
}
export const NavContext = createContext<NavCtx>({ go: () => {}, openLibrary: () => {}, openOrganize: () => {} });
export const useNav = () => useContext(NavContext);

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

/** What a finished job did, in one line. Deletions say how much went and what was protected. */
function doneText(d: JobDone): string {
  const r = (d.result ?? {}) as Record<string, number>;
  if (d.job === "delete" || d.job === "dedupe:apply") {
    const n = r.deleted ?? r.quarantined ?? 0;
    const parts = [`${fmtCount(n)}개를 격리했습니다 (${fmtBytes(r.bytes ?? 0)})`];
    if (r.protected) parts.push(`보호된 ${fmtCount(r.protected)}개는 남겼습니다`);
    if (r.skipped_offline) parts.push(`연결 안 된 드라이브 ${fmtCount(r.skipped_offline)}개는 건너뜀`);
    return parts.join(" · ") + " — 「작업 기록」에서 되돌릴 수 있습니다";
  }
  if (d.job === "import") {
    const r2 = d.result as { copied: number; bytes: number; already: number; failed: number; target_dir: string };
    const parts = [`${fmtCount(r2.copied)}개를 가져왔습니다 (${fmtBytes(r2.bytes)}) → ${r2.target_dir}`];
    if (r2.already) parts.push(`이미 있던 ${fmtCount(r2.already)}개는 건너뜀`);
    if (r2.failed) parts.push(`실패 ${fmtCount(r2.failed)}개`);
    return parts.join(" · ") + " — 「작업 기록」에서 되돌릴 수 있습니다";
  }
  if (d.job === "import:preview") return "SD 카드를 확인했습니다";
  if (d.job === "scan" && Array.isArray(d.result?.scan)) {
    const s = (d.result.scan as Record<string, number>[]).reduce(
      (a, x) => ({
        seen: a.seen + (x.seen ?? 0), fresh: a.fresh + (x.added ?? 0) + (x.updated ?? 0), known: a.known + (x.moved ?? 0) + (x.reused ?? 0),
        walk: a.walk + (x.walk_ms ?? 0), read: a.read + (x.read_ms ?? 0),
      }),
      { seen: 0, fresh: 0, known: 0, walk: 0, read: 0 },
    );
    const parts = [`확인 ${fmtCount(s.seen)}개`, `새로 읽음 ${fmtCount(s.fresh)}개`];
    if (s.known) parts.push(`옮겨지거나 복사된 ${fmtCount(s.known)}개는 다시 읽지 않음`);
    parts.push(`목록 읽기 ${dur(s.walk)} · 새 파일 읽기 ${dur(s.read)}`);
    return parts.join(" · ");
  }
  return "작업이 완료되었습니다";
}

const dur = (ms: number) => {
  const s = Math.round(ms / 1000);
  return s < 60 ? `${s}초` : `${Math.floor(s / 60)}분 ${s % 60}초`;
};

const NAV: { section: string; items: { id: Page; label: string; icon: string }[] }[] = [
  {
    section: "보기",
    items: [
      { id: "library", label: "라이브러리", icon: "▦" },
      { id: "people", label: "인물", icon: "☺" },
      { id: "trips", label: "여행", icon: "✈" },
    ],
  },
  {
    section: "정리하기",
    items: [
      { id: "cleanup", label: "정리", icon: "✦" },
      { id: "organize", label: "한 곳으로 정리", icon: "⇲" },
    ],
  },
  {
    section: "관리",
    items: [
      { id: "sources", label: "드라이브 · 폴더", icon: "⛁" },
      { id: "history", label: "작업 기록 · 되돌리기", icon: "↺" },
      { id: "settings", label: "설정", icon: "⚙" },
    ],
  },
];

const LANE_REASON: Record<string, string> = { user: "일시정지됨", job: "다른 작업이 끝나면 이어서", battery: "배터리 사용 중이라 쉬는 중" };

export default function App() {
  const [page, setPage] = useState<Page>("library");
  const [running, setRunning] = useState<string | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [lastDone, setLastDone] = useState<JobDone | null>(null);
  const [version, setVersion] = useState(0);
  const [stats, setStats] = useState<LibraryStats | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [toast, setToast] = useState<{ ok: boolean; text: string } | null>(null);
  /** A drive with registered folders was just plugged in. */
  const [plugged, setPlugged] = useState<{ volume_id: number; label: string } | null>(null);
  useEffect(() => {
    const un = onDriveConnected(setPlugged);
    return () => {
      un.then((u) => u());
    };
  }, []);
  /** A camera card was plugged in (offer to import), and the import panel. */
  const [card, setCard] = useState<Card | null>(null);
  const [importing, setImporting] = useState<Card | null>(null);
  useEffect(() => {
    const un = onCardConnected(setCard);
    return () => {
      un.then((u) => u());
    };
  }, []);
  const checkPlugged = async () => {
    const p = plugged;
    setPlugged(null);
    try {
      // Only the drive that was plugged in, not every drive.
      if (p) await api.scanVolume(p.volume_id);
      else await api.startScan();
      setRunning("scan");
    } catch (e) {
      setToast({ ok: false, text: String(e) });
    }
  };
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [updating, setUpdating] = useState<UpdateProgress | null>(null);
  const [checking, setChecking] = useState(false);
  const [lane, setLane] = useState<LaneStatus | null>(null);
  const [preset, setPreset] = useState<LibraryPreset | null>(null);

  const [organizePreset, setOrganizePreset] = useState<{ key: number; filter: FileFilter; template?: string } | null>(null);
  const nav: NavCtx = {
    go: (p) => setPage(p),
    openLibrary: (p) => {
      setPreset({ ...p, key: Date.now() });
      setPage("library");
    },
    openOrganize: (filter, template) => {
      setOrganizePreset({ key: Date.now(), filter, template });
      setPage("organize");
    },
  };

  // Background analysis status (separate from user jobs).
  useEffect(() => {
    api.analysisStatus().then(setLane).catch(() => {});
    const un = onAnalysisStatus((s) => setLane((cur) => ({ ...(cur ?? s), ...s })));
    const t = setInterval(() => api.analysisStatus().then(setLane).catch(() => {}), 15000);
    return () => {
      clearInterval(t);
      un.then((u) => u());
    };
  }, []);

  // Quietly look for a new version shortly after start; never interrupts the user.
  useEffect(() => {
    const t = setTimeout(() => api.checkUpdate().then((u) => u.available && setUpdate(u)).catch(() => {}), 4000);
    const un = onUpdateProgress(setUpdating);
    return () => {
      clearTimeout(t);
      un.then((u) => u());
    };
  }, []);

  const checkNow = async () => {
    setChecking(true);
    try {
      const u = await api.checkUpdate();
      if (u.available) setUpdate(u);
      else setToast({ ok: true, text: `최신 버전입니다 (v${u.current})` });
    } catch (e) {
      setToast({ ok: false, text: `업데이트 확인 실패: ${e}` });
    } finally {
      setChecking(false);
    }
  };

  const installNow = async () => {
    setUpdating({ downloaded: 0, total: null });
    try {
      await api.installUpdate(); // the app restarts itself when done
    } catch (e) {
      setUpdating(null);
      setToast({ ok: false, text: String(e) });
    }
  };

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
      else if (d.ok) setToast({ ok: true, text: doneText(d) });
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

  /** A short job can finish before its caller marks it running (the done event arrives first):
   *  confirm with the backend so the bar never stays on a job that already ended. */
  const markRunning = (name: string | null) => {
    setRunning(name);
    if (name) {
      api.appInfo().then((i) => {
        if (!i.job) setRunning((r) => (r === name ? null : r));
      }).catch(() => {});
    }
  };

  return (
    <NavContext.Provider value={nav}>
    <JobContext.Provider value={{ running, progress, lastDone, version, setRunning: markRunning }}>
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
            {NAV.map((sec) => (
              <div key={sec.section} className="nav-section">
                <div className="nav-section-title">{sec.section}</div>
                {sec.items.map((n) => (
                  <button key={n.id} className={`nav-item ${page === n.id || (n.id === "cleanup" && page === "duplicates") ? "active" : ""}`} onClick={() => setPage(n.id)}>
                    <span className="nav-icon">{n.icon}</span>
                    {n.label}
                  </button>
                ))}
              </div>
            ))}
          </nav>
          {lane && (lane.running || lane.pending_online > 0) && (
            <div className="lane-card" title="썸네일·선명도 등 사진 정보를 뒤에서 만드는 중">
              <div className="row-between">
                <span className="small"><b>사진 분석</b> {lane.running && !lane.paused && <span className="spinner tiny" />}</span>
                {lane.running && !lane.paused ? (
                  <button className="btn small ghost" onClick={() => api.analysisPause(true)}>일시정지</button>
                ) : (
                  <button className="btn small ghost" onClick={() => api.analysisStart()}>{lane.paused ? "재개" : "시작"}</button>
                )}
              </div>
              {lane.running && lane.total > 0 && (
                <div className="bar"><div className="bar-fill" style={{ width: `${Math.min(100, (lane.done / lane.total) * 100)}%` }} /></div>
              )}
              <span className="muted small">
                {lane.paused && lane.reason ? LANE_REASON[lane.reason] : lane.running
                  ? `${fmtCount(lane.done)} / ${fmtCount(lane.total)}${lane.eta_secs ? ` · ${fmtEta(lane.eta_secs)} 남음` : ""}`
                  : `${fmtCount(lane.pending_online)}개 대기 중`}
              </span>
            </div>
          )}
          {update?.available && (
            <div className="update-card">
              <b>새 버전 v{update.version}</b>
              <span className="muted small">지금 v{update.current}</span>
              {update.notes && (
                <details>
                  <summary className="small">바뀐 점</summary>
                  <div className="small notes">{update.notes}</div>
                </details>
              )}
              {updating ? (
                <>
                  <div className="bar"><div className={`bar-fill ${updating.total ? "" : "indeterminate"}`} style={{ width: updating.total ? `${(updating.downloaded / updating.total) * 100}%` : "30%" }} /></div>
                  <span className="small muted">
                    {updating.total && updating.downloaded >= updating.total ? "설치 후 다시 시작합니다…" : `내려받는 중 ${fmtBytes(updating.downloaded)}${updating.total ? ` / ${fmtBytes(updating.total)}` : ""}`}
                  </span>
                </>
              ) : (
                <button className="btn small primary" disabled={!!running} title={running ? "진행 중인 작업이 끝나면 업데이트할 수 있습니다" : ""} onClick={installNow}>
                  업데이트 후 재시작
                </button>
              )}
              {running && !updating && <span className="small muted">작업이 끝나면 업데이트할 수 있습니다</span>}
            </div>
          )}
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
          <div className="side-version">
            <span>v{info?.version ?? ""}</span>
            <button className="btn small ghost" disabled={checking} onClick={checkNow}>{checking ? "확인 중…" : "업데이트 확인"}</button>
          </div>
        </aside>

        <main className="content">
          {/* The library stays mounted (hidden) so scroll position, filters and selection survive
              visits to other screens. */}
          <div style={{ display: page === "library" ? "contents" : "none" }}>
            <Library
              active={page === "library"}
              preset={preset}
              onClearPreset={() => setPreset(null)}
              onConsumePreset={() => setPreset((p) => p && { ...p, select: undefined, review: undefined })}
            />
          </div>
          {page === "cleanup" && <Cleanup />}
          {page === "trips" && <Trips />}
          {page === "people" && <People />}
          {page === "sources" && <Sources />}
          {page === "duplicates" && <Duplicates />}
          {page === "organize" && <Organize preset={organizePreset} />}
          {page === "history" && <History />}
          {page === "settings" && <Settings />}
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
        {card && !importing && (
          <div className="toast ok drive-toast card-toast">
            <span>⇩ SD 카드 <b>{card.label}</b>가 연결되었습니다</span>
            <button className="btn small primary" disabled={!!running} onClick={() => { setImporting(card); setCard(null); }}>새 사진 가져오기</button>
            <button className="btn small ghost" onClick={() => setCard(null)}>닫기</button>
          </div>
        )}
        {importing && <ImportPanel card={importing} onClose={() => setImporting(null)} />}
        {plugged && (
          <div className="toast ok drive-toast">
            <span>⏏ <b>{plugged.label}</b> 드라이브가 연결되었습니다</span>
            <button className="btn small primary" disabled={!!running} onClick={checkPlugged}>새 파일 확인</button>
            <button className="btn small ghost" onClick={() => setPlugged(null)}>닫기</button>
          </div>
        )}
      </div>
    </JobContext.Provider>
    </NavContext.Provider>
  );
}

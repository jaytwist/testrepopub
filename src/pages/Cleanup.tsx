import { useCallback, useEffect, useState } from "react";
import { api, fmtBytes, fmtCount, onAnalysisDone, type CleanupSummary, type FileFilter, type GroupBy } from "../api";
import { useJob, useNav } from "../App";

interface Card {
  key: string;
  icon: string;
  title: string;
  count: number;
  bytes: number;
  unit: string;
  why: string;
  /** Primary action: "review" = one by one, "grid" = all at once. */
  primary: "review" | "grid";
  filter?: FileFilter;
  groupBy?: GroupBy;
  review?: string;
  selectBurstExtras?: boolean;
  link?: "duplicates";
}

export default function Cleanup() {
  const { version } = useJob();
  const nav = useNav();
  const [s, setS] = useState<CleanupSummary | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(() => {
    api.cleanupSummary().then(setS).catch((e) => setErr(String(e)));
  }, []);
  useEffect(load, [load, version]);
  useEffect(() => {
    const un = onAnalysisDone(load);
    return () => {
      un.then((u) => u());
    };
  }, [load]);

  const open = async (c: Card, mode: "review" | "grid") => {
    if (c.link) return nav.go(c.link);
    setBusy(c.key);
    try {
      const select = c.selectBurstExtras ? await api.burstExtraIds() : undefined;
      nav.openLibrary({
        title: mode === "review" ? `${c.title} 검토` : c.title,
        filter: c.filter ?? {},
        groupBy: c.groupBy,
        select,
        review: mode === "review" ? c.review : undefined,
      });
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(null);
    }
  };

  const cards: Card[] = s
    ? [
        {
          key: "burst", icon: "▤", title: "연사", count: s.burst_extras.count, bytes: s.burst_extras.bytes, unit: "장 줄이기",
          why: s.burst_groups > 0
            ? `몇 초 사이에 비슷하게 찍은 ${fmtCount(s.burst_groups)}묶음. 가장 선명한 한 장(★)만 남기고 나머지를 미리 골라 둡니다.`
            : "몇 초 사이에 비슷하게 찍은 사진 묶음을 찾습니다.",
          primary: "review", filter: { in_burst: true }, groupBy: "burst", review: "burst_extra", selectBurstExtras: true,
        },
        {
          key: "blurry", icon: "◌", title: "흐린 사진", count: s.blurry.count, bytes: s.blurry.bytes, unit: "장",
          why: "같은 날 찍은 다른 사진보다 훨씬 흐린 사진. 배경만 흐린 인물 사진이나 하늘 사진은 제외합니다.",
          primary: "review", filter: { suggest: "blurry" }, groupBy: "month", review: "blurry",
        },
        {
          key: "screenshot", icon: "▭", title: "스크린샷", count: s.screenshots.count, bytes: s.screenshots.bytes, unit: "개",
          why: "화면을 캡처한 이미지. 필요한 것만 남기고 한꺼번에 정리하기 좋습니다.",
          primary: "grid", filter: { suggest: "screenshot" }, groupBy: "month", review: "screenshot",
        },
        {
          key: "video", icon: "▶", title: "큰 동영상", count: s.large_videos.count, bytes: s.large_videos.bytes, unit: "개",
          why: "500MB 이상인 동영상. 몇 개만 지워도 공간이 많이 생깁니다. 큰 것부터 보여드립니다.",
          primary: "grid", filter: { suggest: "large_video", sort: "size", desc: true }, groupBy: "none", review: "large_video",
        },
        {
          key: "dup", icon: "⧉", title: "똑같은 파일", count: s.exact_duplicates.count, bytes: s.exact_duplicates.bytes, unit: "개 사본",
          why: "내용이 완전히 같은 사본. 「중복 정리」에서 어느 것을 남길지 자동으로 골라 드립니다.",
          primary: "grid", link: "duplicates",
        },
      ]
    : [];

  const analyzing = s != null && s.pending > 0;
  const nothingAnalyzed = s != null && s.analyzed === 0;

  return (
    <div className="page cleanup-page">
      <header className="page-head">
        <div>
          <h1>정리 제안</h1>
          <div className="muted">지워도 될 만한 사진을 찾아 드립니다. 아무것도 자동으로 지우지 않으며, 지운 파일은 「작업 기록」에서 되돌릴 수 있습니다.</div>
        </div>
      </header>
      {err && <div className="alert bad">{err}</div>}

      {s && (
        <div className="cleanup-hero card">
          <div>
            <div className="muted small">정리하면 확보할 수 있는 공간</div>
            <div className="hero-num">최대 {fmtBytes(s.total_bytes)}</div>
          </div>
          {analyzing && (
            <div className="muted small hero-note">
              사진 분석 중 · {fmtCount(s.pending)}개 남음 — 분석이 끝날수록 흐린 사진과 연사가 더 찾아집니다.
              <button className="btn small ghost" onClick={() => nav.go("settings")}>분석 설정</button>
            </div>
          )}
        </div>
      )}

      {nothingAnalyzed && (
        <div className="alert">흐린 사진과 연사는 사진 분석이 끝나야 보입니다. 스캔이 끝나면 자동으로 분석합니다.</div>
      )}

      <div className="cleanup-cards">
        {[...cards].sort((x, y) => Number(x.count === 0) - Number(y.count === 0)).map((c) => {
          const empty = c.count === 0;
          return (
            <section key={c.key} className={`card cleanup-card ${empty ? "empty" : ""}`}>
              <div className="cc-head">
                <span className="cc-icon">{c.icon}</span>
                <h2>{c.title}</h2>
              </div>
              <div className="cc-num">
                {empty ? <span className="muted">없음</span> : <><b>{fmtCount(c.count)}</b>{c.unit} <span className="muted">· {fmtBytes(c.bytes)}</span></>}
              </div>
              <p className="muted small">{c.why}</p>
              {!empty && (
                <div className="btn-row">
                  {c.link ? (
                    <button className="btn primary" onClick={() => open(c, "grid")}>중복 정리로 가기</button>
                  ) : (
                    <>
                      <button className={`btn ${c.primary === "review" ? "primary" : ""}`} disabled={busy === c.key} onClick={() => open(c, "review")}>
                        한 장씩 검토
                      </button>
                      <button className={`btn ${c.primary === "grid" ? "primary" : ""}`} disabled={busy === c.key} onClick={() => open(c, "grid")}>
                        모아서 보기
                      </button>
                    </>
                  )}
                </div>
              )}
            </section>
          );
        })}
      </div>

      {s && (
        <p className="muted small">
          검토 화면 단축키: <kbd>←</kbd> <kbd>→</kbd> 이동 · <kbd>K</kbd> 남기기(다시 제안하지 않음) · <kbd>Delete</kbd> 삭제 목록에 담기 · <kbd>Esc</kbd> 끝내기(담은 파일을 한 번에 삭제)
        </p>
      )}
    </div>
  );
}

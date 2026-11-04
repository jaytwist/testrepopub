import { useEffect, useState } from "react";
import { api, fmtCount, fmtDate, thumbUrl, type TripRow, type TripsSummary } from "../api";
import { useJob, useNav } from "../App";

/** "2024.06.10 ~ 06.13 · 4일" */
function period(t: TripRow): string {
  const days = Math.max(1, Math.round((t.end_ts - t.start_ts) / 86400) + 1);
  const a = fmtDate(t.start_ts);
  const b = fmtDate(t.end_ts);
  return `${a}${b !== a ? ` ~ ${b}` : ""} · ${days}일`;
}

function yearOf(ts: number): number {
  return new Date(ts * 1000).getUTCFullYear();
}

export default function Trips() {
  const { version } = useJob();
  const nav = useNav();
  const [s, setS] = useState<TripsSummary | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    api.tripsSummary().then(setS).catch((e) => setErr(String(e)));
  }, [version]);

  const open = (t: TripRow) =>
    nav.openLibrary({ title: `${t.title} · ${fmtCount(t.count)}장`, filter: { trip_id: t.id, sort: "taken", desc: false }, groupBy: "none" });

  const byYear = new Map<number, TripRow[]>();
  for (const t of s?.trips ?? []) {
    const y = yearOf(t.start_ts);
    byYear.set(y, [...(byYear.get(y) ?? []), t]);
  }
  const pct = s && s.total > 0 ? Math.round((s.located / s.total) * 100) : 0;

  return (
    <div className="page trips-page">
      <header className="page-head">
        <div>
          <h1>여행</h1>
          <div className="muted">
            사진의 위치(GPS)와 날짜로 집을 떠난 여행을 자동으로 묶었습니다. 사진 검색창에 도시 이름(예: Honolulu)을 쳐도 찾을 수 있습니다.
          </div>
        </div>
      </header>
      {err && <div className="alert bad">{err}</div>}
      {s && (
        <div className="kv-inline trips-meta">
          <span>집으로 인식: <b>{s.home || "알 수 없음"}</b></span>
          <span>위치 정보가 있는 사진 <b>{pct}%</b> ({fmtCount(s.located)} / {fmtCount(s.total)})</span>
        </div>
      )}

      {s && s.trips.length === 0 && (
        <div className="empty">
          <div className="empty-icon">✈</div>
          <h2>아직 찾은 여행이 없습니다</h2>
          <p>
            집에서 80km 넘게 떨어진 곳에서 찍은 사진이 10장 이상 모이면 여행으로 묶습니다.
            {pct < 30 && " 위치 정보가 있는 사진이 적습니다 — 메신저로 받은 사진은 위치가 지워지고, 휴대폰 카메라의 위치 저장이 꺼져 있으면 기록되지 않습니다."}
          </p>
        </div>
      )}

      {[...byYear.entries()].map(([y, trips]) => (
        <section key={y} className="trip-year">
          <h2>{y}년 <span className="muted small">{trips.length}번</span></h2>
          <div className="trip-cards">
            {trips.map((t) => (
              <button key={t.id} className="trip-card" onClick={() => open(t)}>
                <div className="trip-cover">
                  {t.cover_file_id != null && <img src={thumbUrl(t.cover_file_id)} alt="" loading="lazy" />}
                </div>
                <div className="trip-body">
                  <b>{t.title}</b>
                  <span className="muted small">{period(t)} · {fmtCount(t.count)}장</span>
                  {t.place && <span className="muted small">{t.place}</span>}
                </div>
              </button>
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}

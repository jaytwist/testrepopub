import { useCallback, useEffect, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  api, fmtBytes, fmtCount, onAnalysisDone,
  type CleanupSummary, type DupSummary, type FileFilter, type GroupBy, type TriageKind,
} from "../api";
import { useJob, useNav } from "../App";
import Triage from "../components/Triage";

type Tier = "safe" | "quick" | "check";

interface Card {
  key: string;
  tier: Tier;
  icon: string;
  title: string;
  count: number;
  bytes: number;
  unit: string;
  why: string;
  /** The main button. */
  primary: Action;
  secondary?: Action[];
  /** Needs a search first (duplicates / similar photos not searched yet). */
  find?: { kind: string; label: string };
}

type Action =
  | { label: string; triage: { kind: TriageKind; variant?: "safe" | "edited" } }
  | { label: string; library: { filter: FileFilter; groupBy?: GroupBy; review?: string; selectAll?: boolean; selectBurstExtras?: boolean } }
  | { label: string; applyExact: true }
  | { label: string; go: "duplicates" };

const TIERS: { id: Tier; title: string; note: string }[] = [
  { id: "safe", title: "안전 — 한 번에 정리해도 됩니다", note: "똑같은 파일이 같은 드라이브에 또 있는 경우입니다. 원본과 다른 드라이브의 백업은 남습니다." },
  { id: "quick", title: "빠르게 보고 정리", note: "추천대로 미리 골라 두었습니다. 훑어보며 바꿀 것만 바꾸세요." },
  { id: "check", title: "확인이 필요한 것", note: "편집본이나 동영상처럼 내용이 다를 수 있는 것입니다. 하나씩 보고 결정하세요." },
];

/**
 * The cleanup hub: everything that could go, safest first, with the space it frees. Nothing is
 * deleted without a confirmation; protected files (originals, ♥, keep folders, the last copy, and
 * backups on other drives) stay; everything removed can be restored from 「작업 기록」.
 */
export default function Cleanup() {
  const { version, running, setRunning } = useJob();
  const nav = useNav();
  const [s, setS] = useState<CleanupSummary | null>(null);
  const [dups, setDups] = useState<DupSummary[] | null>(null);
  const [freed, setFreed] = useState(0);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [triage, setTriage] = useState<{ kind: TriageKind; variant?: "safe" | "edited"; title: string } | null>(null);

  const load = useCallback(() => {
    api.cleanupSummary().then(setS).catch((e) => setErr(String(e)));
    api.dupSummary().then(setDups).catch(() => setDups([]));
    api.batches()
      .then((bs) => setFreed(bs.filter((b) => (b.kind === "delete" || b.kind === "dedupe") && b.state !== "undone").reduce((a, b) => a + b.bytes, 0)))
      .catch(() => {});
  }, []);
  useEffect(load, [load, version]);
  useEffect(() => {
    const un = onAnalysisDone(load);
    return () => {
      un.then((u) => u());
    };
  }, [load]);

  const searched = (kind: string) => dups?.some((d) => d.kind === kind) ?? false;

  const run = async (c: Card, a: Action) => {
    setBusy(c.key);
    try {
      if ("triage" in a) setTriage({ ...a.triage, title: c.title });
      else if ("go" in a) nav.go(a.go);
      else if ("applyExact" in a) {
        const ok = await confirm(
          `같은 드라이브에 또 있는 사본 ${fmtCount(c.count)}개(${fmtBytes(c.bytes)})를 정리할까요?\n\n` +
            "각 드라이브에서 가장 좋은 한 장은 남고, 다른 드라이브의 백업과 보호된 파일(원본 지정·♥·항상 남길 폴더)도 남습니다. " +
            "정리한 파일은 「작업 기록」에서 되돌릴 수 있습니다.",
          { title: "똑같은 사본 정리", kind: "warning", okLabel: "정리", cancelLabel: "취소" },
        );
        if (!ok) return;
        await api.startDedupeApply("exact");
        setRunning("dedupe:apply");
      } else {
        const l = a.library;
        const select = l.selectBurstExtras ? await api.burstExtraIds() : l.selectAll ? await api.queryIds(l.filter) : undefined;
        nav.openLibrary({ title: c.title, filter: l.filter, groupBy: l.groupBy, select, review: l.review });
      }
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(null);
    }
  };

  const find = async (kind: string) => {
    try {
      await api.startDedupe(kind, kind === "similar_image" ? "high" : undefined);
      setRunning(`dedupe:${kind}`);
    } catch (e) {
      setErr(String(e));
    }
  };

  const cards: Card[] = s
    ? [
        {
          key: "exact", tier: "safe", icon: "⧉", title: "같은 드라이브의 똑같은 사본", count: s.exact_duplicates.count, bytes: s.exact_duplicates.bytes, unit: "개",
          why: "내용이 완전히 같은 파일이 한 드라이브에 여러 개 있습니다. 가장 좋은 한 장(원본)을 남기고 나머지를 정리합니다.",
          primary: { label: "한 번에 정리", applyExact: true },
          secondary: [{ label: "묶음별 빠른 검토", triage: { kind: "exact" } }, { label: "자세히", go: "duplicates" }],
          find: searched("exact") ? undefined : { kind: "exact", label: "똑같은 파일 찾기" },
        },
        {
          key: "burst", tier: "quick", icon: "▤", title: "연사", count: s.burst_extras.count, bytes: s.burst_extras.bytes, unit: "장 줄이기",
          why: s.burst_groups > 0
            ? `몇 초 사이에 비슷하게 찍은 ${fmtCount(s.burst_groups)}묶음. 가장 선명한 한 장(★)만 남기도록 골라 두었습니다.`
            : "몇 초 사이에 비슷하게 찍은 사진 묶음입니다.",
          primary: { label: "빠른 검토", triage: { kind: "burst" } },
          secondary: [{ label: "모아서 보기", library: { filter: { in_burst: true }, groupBy: "burst", selectBurstExtras: true } }],
        },
        {
          key: "screenshot", tier: "quick", icon: "▭", title: "스크린샷", count: s.screenshots.count, bytes: s.screenshots.bytes, unit: "개",
          why: "모두 골라 둔 상태로 엽니다. 남길 것만 선택을 풀고 삭제하세요.",
          primary: { label: "모아서 고르기", library: { filter: { suggest: "screenshot" }, groupBy: "month", selectAll: true } },
          secondary: [{ label: "한 장씩 검토", library: { filter: { suggest: "screenshot" }, groupBy: "month", review: "screenshot" } }],
        },
        {
          key: "blurry", tier: "quick", icon: "◌", title: "흐린 사진", count: s.blurry.count, bytes: s.blurry.bytes, unit: "장",
          why: "같은 날 찍은 다른 사진보다 훨씬 흐린 사진. 배경만 흐린 인물 사진이나 하늘 사진은 제외합니다.",
          primary: { label: "한 장씩 검토", library: { filter: { suggest: "blurry" }, groupBy: "month", review: "blurry" } },
          secondary: [{ label: "모아서 보기", library: { filter: { suggest: "blurry" }, groupBy: "month" } }],
        },
        {
          key: "variants_safe", tier: "quick", icon: "⇲", title: "축소본·메신저 전송본", count: s.variants_safe.count, bytes: s.variants_safe.bytes, unit: "개",
          why: "같은 사진을 작게 줄이거나 다시 저장한 것(카톡·메신저로 받은 사진 등). 원본이 남는 것만 골라 두었습니다.",
          primary: { label: "빠른 검토", triage: { kind: "similar_image", variant: "safe" } },
          find: searched("similar_image") ? undefined : { kind: "similar_image", label: "비슷한 사진 찾기" },
        },
        {
          key: "variants_edited", tier: "check", icon: "✎", title: "편집본·비슷한 사진", count: s.variants_edited.count, bytes: s.variants_edited.bytes, unit: "개",
          why: "자르거나 회전·보정한 다른 버전입니다. 모두 남겨 두었으니, 나란히 보고 필요 없는 것만 고르세요.",
          primary: { label: "나란히 비교하며 검토", triage: { kind: "similar_image", variant: "edited" } },
          find: searched("similar_image") ? undefined : { kind: "similar_image", label: "비슷한 사진 찾기" },
        },
        {
          key: "short_video", tier: "check", icon: "▷", title: "아주 짧은 동영상", count: s.short_videos.count, bytes: s.short_videos.bytes, unit: "개",
          why: "3초가 안 되는 동영상. 실수로 찍힌 경우가 많습니다.",
          primary: { label: "한 장씩 검토", library: { filter: { suggest: "short_video" }, groupBy: "month", review: "short_video" } },
          secondary: [{ label: "모아서 보기", library: { filter: { suggest: "short_video" }, groupBy: "month" } }],
        },
        {
          key: "large_video", tier: "check", icon: "▶", title: "큰 동영상", count: s.large_videos.count, bytes: s.large_videos.bytes, unit: "개",
          why: "500MB 이상인 동영상. 몇 개만 지워도 공간이 많이 생깁니다. 큰 것부터 보여 드립니다.",
          primary: { label: "모아서 보기", library: { filter: { suggest: "large_video", sort: "size", desc: true }, groupBy: "none" } },
          secondary: [{ label: "한 장씩 검토", library: { filter: { suggest: "large_video", sort: "size", desc: true }, groupBy: "none", review: "large_video" } }],
        },
      ]
    : [];

  const tierBytes = (t: Tier) => cards.filter((c) => c.tier === t).reduce((a, c) => a + c.bytes, 0);
  const analyzing = s != null && s.pending > 0;

  return (
    <div className="page cleanup-page">
      <header className="page-head">
        <div>
          <h1>정리</h1>
          <div className="muted">
            지워도 될 것을 안전한 순서로 모았습니다. 원본, ♥, 항상 남길 폴더, 다른 드라이브의 백업은 지우지 않으며,
            정리한 파일은 「작업 기록」에서 되돌릴 수 있습니다.
          </div>
        </div>
      </header>
      {err && <div className="alert bad">{err}</div>}

      {s && (
        <div className="cleanup-hero card">
          <div>
            <div className="muted small">정리하면 확보할 수 있는 공간</div>
            <div className="hero-num">{fmtBytes(s.total_bytes)}</div>
            <div className="hero-tiers small">
              <span className="t-safe">안전 {fmtBytes(tierBytes("safe"))}</span>
              <span className="t-quick">빠르게 볼 것 {fmtBytes(tierBytes("quick"))}</span>
              <span className="t-check">확인 필요 {fmtBytes(tierBytes("check"))}</span>
            </div>
          </div>
          <div className="hero-side">
            {freed > 0 && <div className="small">지금까지 정리한 공간 <b>{fmtBytes(freed)}</b></div>}
            {s.backups.count > 0 && (
              <div className="small muted">🛡 다른 드라이브에 백업된 사진 {fmtCount(s.backups.count)}개는 지울 대상에서 뺐습니다</div>
            )}
            {analyzing && (
              <div className="muted small hero-note">
                사진 분석 중 · {fmtCount(s.pending)}개 남음 — 끝날수록 흐린 사진과 연사가 더 찾아집니다.
              </div>
            )}
          </div>
        </div>
      )}

      {TIERS.map((t) => {
        const list = cards.filter((c) => c.tier === t.id);
        return (
          <section key={t.id} className={`cleanup-tier tier-${t.id}`}>
            <h2 className="tier-title">{t.title}</h2>
            <p className="muted small tier-note">{t.note}</p>
            <div className="cleanup-cards">
              {list.map((c) => {
                const empty = c.count === 0;
                return (
                  <section key={c.key} className={`card cleanup-card ${empty && !c.find ? "empty" : ""}`}>
                    <div className="cc-head">
                      <span className="cc-icon">{c.icon}</span>
                      <h3>{c.title}</h3>
                    </div>
                    <div className="cc-num">
                      {c.find && empty ? (
                        <span className="muted">아직 찾지 않음</span>
                      ) : empty ? (
                        <span className="muted">없음</span>
                      ) : (
                        <><b>{fmtCount(c.count)}</b>{c.unit} <span className="muted">· {fmtBytes(c.bytes)}</span></>
                      )}
                    </div>
                    <p className="muted small">{c.why}</p>
                    <div className="btn-row">
                      {c.find && (
                        <button className={`btn ${empty ? "primary" : "small"}`} disabled={!!running} onClick={() => find(c.find!.kind)}>
                          {empty ? c.find.label : "다시 찾기"}
                        </button>
                      )}
                      {!empty && (
                        <>
                          <button className="btn primary" disabled={busy === c.key || (!!running && "applyExact" in c.primary)} onClick={() => run(c, c.primary)}>
                            {c.primary.label}
                          </button>
                          {c.secondary?.map((a) => (
                            <button key={a.label} className="btn" disabled={busy === c.key} onClick={() => run(c, a)}>{a.label}</button>
                          ))}
                        </>
                      )}
                    </div>
                  </section>
                );
              })}
            </div>
          </section>
        );
      })}

      {s && (
        <p className="muted small">
          빠른 검토: <kbd>←</kbd><kbd>→</kbd> 남길 사진 고르기 · <kbd>Space</kbd> 다음 · <kbd>S</kbd> 모두 남기기 · <kbd>Z</kbd> 이전 ·
          한 장씩 검토: <kbd>K</kbd> 남기기 · <kbd>Delete</kbd> 지우기 · <kbd>Z</kbd> 되돌리기 · <kbd>Esc</kbd> 끝내고 한 번에 정리
        </p>
      )}

      {triage && <Triage kind={triage.kind} variant={triage.variant} title={triage.title} onClose={() => { setTriage(null); load(); }} />}
    </div>
  );
}
